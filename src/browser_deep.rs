use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{sleep, timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

use crate::app::AppPaths;
use crate::cli::{BrowserDeepCommand, OutputMode};
use crate::shutdown;

const DEFAULT_CHROME_PATHS: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
];
const FORCE_SHUTDOWN_WAIT: Duration = Duration::from_secs(2);
const TARGET_DISCOVERY_WAIT: Duration = Duration::from_secs(10);
const CDP_RESPONSE_WAIT: Duration = Duration::from_secs(5);
const INTERACTION_INJECT_SCRIPT: &str = r#"
(() => {
  if (window.__agentMcpBDeepInstalled) return;
  window.__agentMcpBDeepInstalled = true;

  function summarizeElement(node) {
    if (!node || !(node instanceof Element)) return "unknown";
    const tag = node.tagName ? node.tagName.toLowerCase() : "node";
    const id = node.id ? `#${node.id}` : "";
    const role = node.getAttribute("role");
    const name = node.getAttribute("name");
    const contentEditable = node.getAttribute("contenteditable");
    const aria = node.getAttribute("aria-label");
    const text = (node.innerText || node.textContent || "").trim().replace(/\s+/g, " ").slice(0, 48);
    const attrs = [
      role ? `[role=${role}]` : "",
      name ? `[name=${name}]` : "",
      contentEditable === "true" ? "[contenteditable=true]" : "",
      aria ? `[aria-label=${aria}]` : "",
    ].join("");
    const suffix = text ? ` "${text}"` : "";
    return `${tag}${id}${attrs}${suffix}`;
  }

  function emit(kind, event, extra) {
    if (typeof window.__agentMcpBInteraction !== "function") return;
    const payload = {
      kind,
      ts_ms: Date.now(),
      page_url: location.href,
      title: document.title,
      element: summarizeElement(event && event.target),
      extra: extra || null,
    };
    window.__agentMcpBInteraction(JSON.stringify(payload));
  }

  document.addEventListener("click", event => emit("click", event, null), true);
  document.addEventListener("submit", event => emit("submit", event, null), true);
  document.addEventListener("keydown", event => {
    if (event.key === "Enter" || event.key === " ") {
      emit("keydown", event, { key: event.key });
    }
  }, true);
})();
"#;

pub async fn run(_paths: &AppPaths, command: BrowserDeepCommand) -> Result<()> {
    let chrome_path = resolve_chrome_path(command.chrome_path.as_deref())?;
    let profile_dir = ManagedProfileDir::new(command.user_data_dir.clone())?;
    let mut child = launch_chrome(&chrome_path, &profile_dir, &command)
        .await
        .with_context(|| format!("failed to launch Chrome at {}", chrome_path.display()))?;

    let cdp_base = format!("http://127.0.0.1:{}", command.remote_debugging_port);
    let target = wait_for_page_target(&cdp_base, &command.open).await?;
    let mut session = CdpSession::connect(&target.web_socket_debugger_url).await?;
    session.initialize().await?;

    println!("launched deep browser from {}", chrome_path.display());
    println!("profile directory: {}", profile_dir.path().display());
    println!("open url: {}", command.open);
    println!("cdp target: {}", target.url);
    println!("close Chrome or press Ctrl+C to stop\n");

    let mut tracker = InteractionTracker::new(command.clone());

    let outcome = tokio::select! {
        status = child.wait() => BrowserDeepOutcome::ChromeExited(
            status.context("failed waiting for Chrome process")?
        ),
        signal = shutdown::wait_for_shutdown_signal() => BrowserDeepOutcome::Interrupted(signal?),
        result = drive_session(&mut session, &mut tracker) => BrowserDeepOutcome::Session(result),
    };

    match outcome {
        BrowserDeepOutcome::ChromeExited(status) => {
            session.shutdown().await;
            if !status.success() {
                bail!("Chrome exited with status {status}");
            }
        }
        BrowserDeepOutcome::Interrupted(signal_name) => {
            eprintln!("received {signal_name}, stopping deep browser session");
            terminate_child(&mut child).await?;
            session.shutdown().await;
        }
        BrowserDeepOutcome::Session(result) => {
            terminate_child(&mut child).await?;
            session.shutdown().await;
            result?;
        }
    }

    Ok(())
}

async fn drive_session(session: &mut CdpSession, tracker: &mut InteractionTracker) -> Result<()> {
    while let Some(event) = session.next_event().await {
        tracker.handle_event(session, event).await?;
    }

    bail!("CDP event stream ended unexpectedly")
}

async fn launch_chrome(
    chrome_path: &Path,
    profile_dir: &ManagedProfileDir,
    command: &BrowserDeepCommand,
) -> Result<Child> {
    let mut process = Command::new(chrome_path);
    process.kill_on_drop(true);
    process.stdin(Stdio::null());
    process.stdout(Stdio::null());
    process.stderr(Stdio::null());
    process.arg("--no-first-run");
    process.arg("--no-default-browser-check");
    process.arg("--new-window");
    process.arg("--no-proxy-server");
    process.arg("--disable-quic");
    process.arg(format!(
        "--remote-debugging-port={}",
        command.remote_debugging_port
    ));
    process.arg(format!("--user-data-dir={}", profile_dir.path().display()));
    process.arg(&command.open);
    process.spawn().context("failed to spawn Chrome process")
}

fn resolve_chrome_path(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        bail!("provided Chrome path does not exist: {}", path.display());
    }

    for candidate in DEFAULT_CHROME_PATHS {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }

    bail!("could not locate a Chrome executable in the default macOS install paths")
}

async fn terminate_child(child: &mut Child) -> Result<()> {
    if child.id().is_some() {
        let _ = child.start_kill();
        let _ = timeout(FORCE_SHUTDOWN_WAIT, child.wait()).await;
    }
    Ok(())
}

async fn wait_for_page_target(cdp_base: &str, open_url: &str) -> Result<CdpTarget> {
    let client = Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .context("failed to build HTTP client for DevTools target discovery")?;
    let target_url = Url::parse(open_url).ok();
    let started = tokio::time::Instant::now();

    loop {
        let response = client
            .get(format!("{cdp_base}/json/list"))
            .send()
            .await;

        if let Ok(response) = response {
            if response.status().is_success() {
                let targets = response
                    .json::<Vec<CdpTarget>>()
                    .await
                    .context("failed parsing DevTools targets")?;

                if let Some(target) = select_target(&targets, target_url.as_ref()) {
                    return Ok(target.clone());
                }
            }
        }

        if started.elapsed() >= TARGET_DISCOVERY_WAIT {
            bail!("timed out waiting for a page target on {cdp_base}");
        }

        sleep(Duration::from_millis(200)).await;
    }
}

fn select_target<'a>(targets: &'a [CdpTarget], preferred_url: Option<&Url>) -> Option<&'a CdpTarget> {
    let preferred_host = preferred_url.and_then(|url| url.host_str());

    targets
        .iter()
        .filter(|target| target.kind == "page" && !target.web_socket_debugger_url.is_empty())
        .find(|target| {
            preferred_host.is_some_and(|host| {
                Url::parse(&target.url)
                    .ok()
                    .and_then(|url| url.host_str().map(|value| value == host))
                    .unwrap_or(false)
            })
        })
        .or_else(|| {
            targets
                .iter()
                .filter(|target| target.kind == "page" && !target.web_socket_debugger_url.is_empty())
                .find(|target| target.url != "about:blank")
        })
}

#[derive(Debug)]
enum BrowserDeepOutcome {
    ChromeExited(ExitStatus),
    Interrupted(&'static str),
    Session(Result<()>),
}

#[derive(Debug, Deserialize, Clone)]
struct CdpTarget {
    #[serde(rename = "type")]
    kind: String,
    url: String,
    #[serde(default, rename = "webSocketDebuggerUrl")]
    web_socket_debugger_url: String,
}

struct CdpSession {
    sender: mpsc::UnboundedSender<Message>,
    events: mpsc::UnboundedReceiver<Value>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
    writer_task: tokio::task::JoinHandle<()>,
    reader_task: tokio::task::JoinHandle<()>,
}

impl CdpSession {
    async fn connect(ws_url: &str) -> Result<Self> {
        let (stream, _) = connect_async(ws_url)
            .await
            .with_context(|| format!("failed connecting to CDP target {ws_url}"))?;
        let (mut write, mut read) = stream.split();
        let (send_tx, mut send_rx) = mpsc::unbounded_channel::<Message>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Value>();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_reader = Arc::clone(&pending);

        let writer_task = tokio::spawn(async move {
            while let Some(message) = send_rx.recv().await {
                if write.send(message).await.is_err() {
                    break;
                }
            }
        });

        let reader_task = tokio::spawn(async move {
            while let Some(message) = read.next().await {
                let Ok(message) = message else {
                    break;
                };

                let text = match message {
                    Message::Text(text) => text.to_string(),
                    Message::Binary(binary) => String::from_utf8_lossy(&binary).into_owned(),
                    Message::Close(_) => break,
                    _ => continue,
                };

                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };

                if let Some(id) = value.get("id").and_then(|value| value.as_u64()) {
                    if let Some(sender) = pending_reader.lock().await.remove(&id) {
                        let _ = sender.send(value);
                    }
                } else {
                    let _ = event_tx.send(value);
                }
            }
        });

        Ok(Self {
            sender: send_tx,
            events: event_rx,
            pending,
            next_id: AtomicU64::new(1),
            writer_task,
            reader_task,
        })
    }

    async fn initialize(&mut self) -> Result<()> {
        self.send_command("Page.enable", json!({})).await?;
        self.send_command("Runtime.enable", json!({})).await?;
        self.send_command("Debugger.enable", json!({})).await?;
        self.send_command("Network.enable", json!({})).await?;
        self.send_command(
            "Runtime.addBinding",
            json!({"name": "__agentMcpBInteraction"}),
        )
        .await?;
        self.send_command(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": INTERACTION_INJECT_SCRIPT }),
        )
        .await?;
        self.send_command(
            "Runtime.evaluate",
            json!({
                "expression": INTERACTION_INJECT_SCRIPT,
                "includeCommandLineAPI": false,
                "awaitPromise": false,
                "returnByValue": false,
            }),
        )
        .await?;
        Ok(())
    }

    async fn send_command(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let payload = json!({
            "id": id,
            "method": method,
            "params": params,
        });

        self.sender
            .send(Message::Text(payload.to_string().into()))
            .map_err(|_| anyhow::anyhow!("failed sending CDP command {method}"))?;

        let response = timeout(CDP_RESPONSE_WAIT, rx)
            .await
            .with_context(|| format!("timed out waiting for CDP response to {method}"))?
            .context("CDP command response channel closed")?;

        if response.get("error").is_some() {
            bail!("CDP command {method} failed: {}", response["error"]);
        }

        Ok(response["result"].clone())
    }

    async fn get_response_body_summary(&self, request_id: &str) -> String {
        let response = self
            .send_command("Network.getResponseBody", json!({ "requestId": request_id }))
            .await;

        match response {
            Ok(result) => {
                let body = result
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let base64_encoded = result
                    .get("base64Encoded")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                summarize_body(body, base64_encoded)
            }
            Err(_) => "{}".to_string(),
        }
    }

    async fn next_event(&mut self) -> Option<Value> {
        self.events.recv().await
    }

    async fn shutdown(self) {
        let _ = self.sender.send(Message::Close(None));
        self.writer_task.abort();
        self.reader_task.abort();
        let _ = self.writer_task.await;
        let _ = self.reader_task.await;
    }
}

#[derive(Debug, Clone)]
struct BrowserInteraction {
    id: u64,
    kind: String,
    element: String,
    page_url: String,
    title: String,
    timestamp_ms: u128,
}

#[derive(Debug, Deserialize)]
struct BindingPayload {
    kind: String,
    ts_ms: u128,
    page_url: String,
    title: String,
    element: String,
    #[allow(dead_code)]
    extra: Option<Value>,
}

#[derive(Debug, Clone)]
struct ObservedRequest {
    request_id: String,
    method: String,
    url: String,
    interaction_id: Option<u64>,
    request_summary: String,
    request_headers: BTreeMap<String, String>,
    response_headers: BTreeMap<String, String>,
    status: Option<u16>,
}

#[derive(Debug, Clone, Serialize)]
struct DeepFlowEvent<'a> {
    interaction_id: Option<u64>,
    interaction_kind: Option<&'a str>,
    interaction_element: Option<&'a str>,
    method: &'a str,
    url: &'a str,
    request_summary: &'a str,
    response_summary: &'a str,
    request_headers: &'a BTreeMap<String, String>,
    response_headers: &'a BTreeMap<String, String>,
    status: Option<u16>,
}

struct InteractionTracker {
    command: BrowserDeepCommand,
    interactions: VecDeque<BrowserInteraction>,
    pending_requests: HashMap<String, ObservedRequest>,
    pending_request_header_extras: HashMap<String, BTreeMap<String, String>>,
    pending_response_header_extras: HashMap<String, BTreeMap<String, String>>,
    next_interaction_id: u64,
    printed_interactions: HashSet<u64>,
}

impl InteractionTracker {
    fn new(command: BrowserDeepCommand) -> Self {
        Self {
            command,
            interactions: VecDeque::new(),
            pending_requests: HashMap::new(),
            pending_request_header_extras: HashMap::new(),
            pending_response_header_extras: HashMap::new(),
            next_interaction_id: 0,
            printed_interactions: HashSet::new(),
        }
    }

    async fn handle_event(&mut self, session: &CdpSession, event: Value) -> Result<()> {
        let Some(method) = event.get("method").and_then(Value::as_str) else {
            return Ok(());
        };

        match method {
            "Runtime.bindingCalled" => self.handle_binding_called(event),
            "Network.requestWillBeSent" => self.handle_request_will_be_sent(event),
            "Network.requestWillBeSentExtraInfo" => self.handle_request_extra_info(event),
            "Network.responseReceived" => self.handle_response_received(event),
            "Network.responseReceivedExtraInfo" => self.handle_response_extra_info(event),
            "Network.loadingFinished" => self.handle_loading_finished(session, event).await,
            "Network.loadingFailed" => self.handle_loading_failed(event),
            _ => Ok(()),
        }
    }

    fn handle_binding_called(&mut self, event: Value) -> Result<()> {
        let params = &event["params"];
        if params["name"].as_str() != Some("__agentMcpBInteraction") {
            return Ok(());
        }

        let payload_text = params["payload"]
            .as_str()
            .context("missing CDP binding payload")?;
        let payload = serde_json::from_str::<BindingPayload>(payload_text)
            .context("failed parsing browser interaction payload")?;

        self.next_interaction_id += 1;
        let interaction = BrowserInteraction {
            id: self.next_interaction_id,
            kind: payload.kind,
            element: payload.element,
            page_url: payload.page_url,
            title: payload.title,
            timestamp_ms: payload.ts_ms,
        };

        self.prune_old(interaction.timestamp_ms);
        self.interactions.push_back(interaction);
        Ok(())
    }

    fn handle_request_will_be_sent(&mut self, event: Value) -> Result<()> {
        let params = &event["params"];
        let request = &params["request"];
        let request_id = params["requestId"]
            .as_str()
            .context("missing requestId")?
            .to_string();
        let method = request["method"]
            .as_str()
            .context("missing request method")?
            .to_string();
        let url = request["url"]
            .as_str()
            .context("missing request url")?
            .to_string();

        if !matches_filters(
            &self.command.host_contains,
            &self.command.url_contains,
            &self.command.methods,
            &method,
            &url,
        ) {
            return Ok(());
        }

        let request_time_ms = params["wallTime"]
            .as_f64()
            .map(|value| (value * 1000.0) as u128)
            .unwrap_or_else(now_ms);
        self.prune_old(request_time_ms);

        let interaction_id = self.match_interaction(&url, request_time_ms).map(|interaction| interaction.id);
        if interaction_id.is_none() && !self.command.record_all {
            return Ok(());
        }

        let request_summary = request
            .get("postData")
            .and_then(Value::as_str)
            .map(|text| summarize_body(text, false))
            .unwrap_or_else(|| "{}".to_string());
        let mut request_headers = parse_cdp_headers(
            request.get("headers"),
            self.command.allow_sensitive_output,
        );
        if let Some(extra) = self.pending_request_header_extras.remove(&request_id) {
            request_headers.extend(extra);
        }

        self.pending_requests.insert(
            request_id.clone(),
            ObservedRequest {
                request_id,
                method,
                url,
                interaction_id,
                request_summary,
                request_headers,
                response_headers: BTreeMap::new(),
                status: None,
            },
        );
        Ok(())
    }

    fn handle_request_extra_info(&mut self, event: Value) -> Result<()> {
        let params = &event["params"];
        let request_id = params["requestId"]
            .as_str()
            .context("missing request extra-info request id")?
            .to_string();
        let headers = parse_cdp_headers(
            params.get("headers"),
            self.command.allow_sensitive_output,
        );
        if let Some(observed) = self.pending_requests.get_mut(&request_id) {
            observed.request_headers.extend(headers);
        } else {
            self.pending_request_header_extras.insert(request_id, headers);
        }
        Ok(())
    }

    fn handle_response_received(&mut self, event: Value) -> Result<()> {
        let params = &event["params"];
        let request_id = params["requestId"]
            .as_str()
            .context("missing response request id")?;
        let Some(observed) = self.pending_requests.get_mut(request_id) else {
            return Ok(());
        };

        observed.status = params["response"]["status"]
            .as_u64()
            .map(|value| value as u16);
        observed.response_headers.extend(parse_cdp_headers(
            params["response"].get("headers"),
            self.command.allow_sensitive_output,
        ));
        if observed.request_headers.is_empty() {
            observed.request_headers.extend(parse_cdp_headers(
                params["response"].get("requestHeaders"),
                self.command.allow_sensitive_output,
            ));
        }
        if let Some(extra) = self.pending_response_header_extras.remove(request_id) {
            observed.response_headers.extend(extra);
        }
        Ok(())
    }

    fn handle_response_extra_info(&mut self, event: Value) -> Result<()> {
        let params = &event["params"];
        let request_id = params["requestId"]
            .as_str()
            .context("missing response extra-info request id")?
            .to_string();
        let headers = parse_cdp_headers(
            params.get("headers"),
            self.command.allow_sensitive_output,
        );
        if let Some(observed) = self.pending_requests.get_mut(&request_id) {
            observed.response_headers.extend(headers);
        } else {
            self.pending_response_header_extras.insert(request_id, headers);
        }
        Ok(())
    }

    async fn handle_loading_finished(&mut self, session: &CdpSession, event: Value) -> Result<()> {
        let request_id = event["params"]["requestId"]
            .as_str()
            .context("missing finished request id")?;
        let Some(observed) = self.pending_requests.remove(request_id) else {
            return Ok(());
        };

        let response_summary = session.get_response_body_summary(&observed.request_id).await;
        self.print_flow(&observed, observed.status, &response_summary);
        Ok(())
    }

    fn handle_loading_failed(&mut self, event: Value) -> Result<()> {
        let params = &event["params"];
        let request_id = params["requestId"]
            .as_str()
            .context("missing failed request id")?;
        let Some(observed) = self.pending_requests.remove(request_id) else {
            return Ok(());
        };
        let error_text = params["errorText"].as_str().unwrap_or("failed");
        let blocked_reason = params["blockedReason"]
            .as_str()
            .map(|value| format!(" blocked={value}"))
            .unwrap_or_default();
        self.print_flow(
            &observed,
            None,
            &format!("{{failed error=\"{error_text}\"{blocked_reason}}}"),
        );
        Ok(())
    }

    fn match_interaction(&self, request_url: &str, request_time_ms: u128) -> Option<&BrowserInteraction> {
        let request_host = Url::parse(request_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .unwrap_or_default();

        self.interactions
            .iter()
            .rev()
            .find(|interaction| {
                let delta = request_time_ms.saturating_sub(interaction.timestamp_ms);
                delta <= self.command.interaction_window_ms as u128
                    && hosts_related(
                        &request_host,
                        &Url::parse(&interaction.page_url)
                            .ok()
                            .and_then(|url| url.host_str().map(str::to_string))
                            .unwrap_or_default(),
                    )
            })
    }

    fn prune_old(&mut self, now_ms: u128) {
        while let Some(front) = self.interactions.front() {
            let expired = now_ms.saturating_sub(front.timestamp_ms)
                > self.command.interaction_window_ms as u128;
            if expired {
                self.interactions.pop_front();
            } else {
                break;
            }
        }
    }

    fn print_flow(&mut self, request: &ObservedRequest, status: Option<u16>, response_summary: &str) {
        let interaction = request.interaction_id.and_then(|interaction_id| self
            .interactions
            .iter()
            .find(|interaction| interaction.id == interaction_id));

        if let Some(interaction) = interaction
            && self.printed_interactions.insert(interaction.id)
        {
            println!(
                "\n[interaction #{}] {} {} @ {}",
                interaction.id,
                interaction.kind,
                interaction.element,
                compact_page_label(&interaction.page_url, &interaction.title)
            );
        }

        match self.command.output {
            OutputMode::Simple | OutputMode::Focused => {
                let label = compact_operation_label(&request.url);
                let status_text = status.map(|value| format!("[{value}]")).unwrap_or_else(|| "[failed]".to_string());
                println!(
                    "  {} ({}) {} {}",
                    request.method, label, response_summary, status_text
                );
            }
            OutputMode::Pretty => {
                println!("  {} {}", request.method, request.url);
                println!("    request: {}", request.request_summary);
                println!("    response: {}", response_summary);
                if let Some(status) = status {
                    println!("    status: {status}");
                } else {
                    println!("    status: failed");
                }
            }
            OutputMode::Json => {
                let event = DeepFlowEvent {
                    interaction_id: interaction.map(|value| value.id),
                    interaction_kind: interaction.map(|value| value.kind.as_str()),
                    interaction_element: interaction.map(|value| value.element.as_str()),
                    method: &request.method,
                    url: &request.url,
                    request_summary: &request.request_summary,
                    response_summary,
                    request_headers: &request.request_headers,
                    response_headers: &request.response_headers,
                    status,
                };
                println!("{}", serde_json::to_string(&event).unwrap_or_default());
            }
        }
    }
}

fn matches_filters(
    host_contains: &[String],
    url_contains: &[String],
    methods: &[String],
    method: &str,
    url: &str,
) -> bool {
    let host = Url::parse(url)
        .ok()
        .and_then(|value| value.host_str().map(str::to_string))
        .unwrap_or_default();

    let method_matches = methods.is_empty()
        || methods
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(method));
    let host_matches = host_contains.is_empty()
        || host_contains.iter().any(|candidate| host.contains(candidate));
    let url_matches = url_contains.is_empty()
        || url_contains.iter().any(|candidate| url.contains(candidate));

    method_matches && host_matches && url_matches
}

fn compact_page_label(page_url: &str, title: &str) -> String {
    let url_label = compact_operation_label(page_url);
    if title.is_empty() {
        url_label
    } else {
        format!("{url_label} \"{}\"", title.replace('\n', " "))
    }
}

fn parse_cdp_headers(
    value: Option<&Value>,
    allow_sensitive_output: bool,
) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    let Some(object) = value.and_then(Value::as_object) else {
        return headers;
    };

    for (name, raw_value) in object {
        let rendered = match raw_value {
            Value::String(text) => text.clone(),
            Value::Array(values) => values
                .iter()
                .filter_map(|item| item.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            other => other.to_string(),
        };
        headers.insert(
            name.to_string(),
            sanitize_header_value(name, &rendered, allow_sensitive_output),
        );
    }

    headers
}

fn sanitize_header_value(name: &str, value: &str, allow_sensitive_output: bool) -> String {
    if allow_sensitive_output {
        return value.to_string();
    }

    if matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
    ) {
        "<redacted>".to_string()
    } else {
        value.to_string()
    }
}

fn compact_operation_label(url: &str) -> String {
    let Ok(parsed) = Url::parse(url) else {
        return url.to_string();
    };
    let host = parsed.host_str().unwrap_or("unknown-host");
    let path = parsed.path().trim_end_matches('/');

    if let Some(proc_name) = path.strip_prefix("/trpc/") {
        return format!("{host}/trpc/{proc_name}");
    }

    let normalized = normalize_path(path);
    if normalized.is_empty() {
        host.to_string()
    } else {
        format!("{host}{normalized}")
    }
}

fn normalize_path(path: &str) -> String {
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(normalize_path_segment)
        .collect::<Vec<_>>();

    if segments.is_empty() {
        String::new()
    } else {
        format!("/{}", segments.join("/"))
    }
}

fn normalize_path_segment(segment: &str) -> String {
    if is_identifier_like_segment(segment) {
        ":id".to_string()
    } else {
        segment.to_string()
    }
}

fn is_identifier_like_segment(segment: &str) -> bool {
    is_long_numeric_segment(segment)
        || is_uuid_like_segment(segment)
        || is_hex_identifier_segment(segment)
}

fn is_long_numeric_segment(segment: &str) -> bool {
    segment.len() >= 6 && segment.chars().all(|char| char.is_ascii_digit())
}

fn is_uuid_like_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    if bytes.len() != 36 {
        return false;
    }

    for (index, byte) in bytes.iter().enumerate() {
        let is_hyphen = matches!(index, 8 | 13 | 18 | 23);
        if is_hyphen {
            if *byte != b'-' {
                return false;
            }
        } else if !byte.is_ascii_hexdigit() {
            return false;
        }
    }

    true
}

fn is_hex_identifier_segment(segment: &str) -> bool {
    matches!(segment.len(), 16..=64) && segment.chars().all(|char| char.is_ascii_hexdigit())
}

fn summarize_body(text: &str, base64_encoded: bool) -> String {
    if base64_encoded {
        return "{binary}".to_string();
    }

    if text.trim().is_empty() {
        return "{}".to_string();
    }

    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return summarize_json_value(&value);
    }

    if text.len() > 64 {
        "{text}".to_string()
    } else {
        "{text}".to_string()
    }
}

fn summarize_json_value(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            keys.truncate(3);
            format!("{{{}}}", keys.join(","))
        }
        Value::Array(items) => {
            if let Some(Value::Object(map)) = items.first() {
                let mut keys = map.keys().cloned().collect::<Vec<_>>();
                keys.sort();
                keys.truncate(3);
                format!("{{{}}}", keys.join(","))
            } else {
                format!("[{}]", items.len())
            }
        }
        Value::Null => "{null}".to_string(),
        Value::Bool(_) => "{bool}".to_string(),
        Value::Number(_) => "{number}".to_string(),
        Value::String(_) => "{string}".to_string(),
    }
}

fn hosts_related(request_host: &str, page_host: &str) -> bool {
    request_host == page_host
        || request_host.ends_with(page_host)
        || page_host.ends_with(request_host)
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

enum ManagedProfileDir {
    Persistent(PathBuf),
    Temporary(TempDir),
}

impl ManagedProfileDir {
    fn new(path: Option<PathBuf>) -> Result<Self> {
        match path {
            Some(path) => {
                std::fs::create_dir_all(&path).with_context(|| {
                    format!(
                        "failed to create Chrome profile directory {}",
                        path.display()
                    )
                })?;
                Ok(Self::Persistent(path))
            }
            None => {
                let temp_dir = tempfile::Builder::new()
                    .prefix("agent-mcp-b-browser-deep-profile-")
                    .tempdir()
                    .context("failed to create temporary Chrome profile directory")?;
                Ok(Self::Temporary(temp_dir))
            }
        }
    }

    fn path(&self) -> &Path {
        match self {
            Self::Persistent(path) => path.as_path(),
            Self::Temporary(temp_dir) => temp_dir.path(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        compact_operation_label, hosts_related, matches_filters, normalize_path_segment,
        summarize_json_value,
    };
    use serde_json::json;

    #[test]
    fn compact_operation_label_normalizes_ids() {
        assert_eq!(
            compact_operation_label("https://discord.com/api/v9/channels/1486087030302703719/messages"),
            "discord.com/api/v9/channels/:id/messages"
        );
    }

    #[test]
    fn compact_operation_label_prefers_trpc_procedure() {
        assert_eq!(
            compact_operation_label("https://api.diceblox.com/trpc/config.get?batch=1"),
            "api.diceblox.com/trpc/config.get"
        );
    }

    #[test]
    fn matches_filters_checks_method_host_and_url() {
        assert!(matches_filters(
            &[String::from("discord.com")],
            &[String::from("/messages")],
            &[String::from("POST")],
            "POST",
            "https://discord.com/api/v9/channels/1/messages"
        ));
    }

    #[test]
    fn summarize_json_value_collapses_top_level_keys() {
        assert_eq!(
            summarize_json_value(&json!({"b":1,"a":2,"c":3,"d":4})),
            "{a,b,c}"
        );
    }

    #[test]
    fn hosts_related_matches_subdomains() {
        assert!(hosts_related("api.diceblox.com", "diceblox.com"));
        assert!(hosts_related("diceblox.com", "diceblox.com"));
        assert!(!hosts_related("discord.com", "diceblox.com"));
    }

    #[test]
    fn normalize_path_segment_rewrites_long_identifiers() {
        assert_eq!(normalize_path_segment("1486087030302703719"), ":id");
        assert_eq!(normalize_path_segment("messages"), "messages");
    }
}
