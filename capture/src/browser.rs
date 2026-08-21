//! Chrome DevTools Protocol capture.
//!
//! Launches a managed Chrome, watches the `Network` domain, and injects a page
//! script that reports clicks/submits through a CDP binding. Requests are then
//! attributed to the UI action that preceded them — which is what makes a capture
//! readable as "this button calls this endpoint" instead of a flat request log.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::event::{Exchange, Trigger, now};

const CHROME: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
/// A request starting within this window of a UI action is attributed to it.
const ATTRIBUTION_WINDOW: f64 = 5.0;

/// Reports UI actions to the binding installed by `Runtime.addBinding`.
const PAGE_SCRIPT: &str = r#"
(() => {
  if (window.__reqtrace) return;
  window.__reqtrace = true;
  const describe = (node) => {
    if (!(node instanceof Element)) return "unknown";
    const label = node.getAttribute("aria-label") || node.getAttribute("name") ||
      (node.innerText || node.textContent || "").trim().replace(/\s+/g, " ").slice(0, 48);
    const role = node.getAttribute("role");
    return node.tagName.toLowerCase() + (role ? `[role=${role}]` : "") + (label ? ` "${label}"` : "");
  };
  const emit = (kind) => (e) => __reqtraceEvent(JSON.stringify({ kind, label: describe(e.target) }));
  document.addEventListener("click", emit("click"), true);
  document.addEventListener("submit", emit("submit"), true);
  document.addEventListener("keydown", (e) => {
    if (e.key === "Enter") emit("keydown")(e);
  }, true);
})();
"#;

pub async fn run(open: &str, profile: &Path, chrome: Option<PathBuf>, port: u16, headless: bool) -> Result<()> {
    // Chrome starts blank and we navigate only once the Network domain is live,
    // so the very first request of the page load is captured too.
    let mut child = launch(chrome, profile, port, "about:blank", headless).await?;
    let ws = discover_target(port).await?;
    let (cdp, mut events) = Cdp::connect(&ws).await?;

    for method in ["Page.enable", "Runtime.enable", "Network.enable"] {
        cdp.call(method, json!({})).await?;
    }
    cdp.call("Runtime.addBinding", json!({"name": "__reqtraceEvent"})).await?;
    cdp.call("Page.addScriptToEvaluateOnNewDocument", json!({"source": PAGE_SCRIPT})).await?;
    cdp.call("Page.navigate", json!({"url": open})).await?;
    eprintln!("browser capture attached to {open}");

    let mut state = State::default();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = child.wait() => break,
            event = events.recv() => match event {
                Some(event) => state.handle(&cdp, event).await,
                None => break,
            },
        }
    }

    let _ = child.kill().await;
    Ok(())
}

/// In-flight requests, keyed by CDP request id, plus the last UI action seen.
#[derive(Default)]
struct State {
    inflight: HashMap<String, Exchange>,
    last_action: Option<(f64, Trigger)>,
}

impl State {
    async fn handle(&mut self, cdp: &Cdp, event: Value) {
        let params = &event["params"];
        let id = params["requestId"].as_str().unwrap_or_default().to_string();

        match event["method"].as_str().unwrap_or_default() {
            "Runtime.bindingCalled" => {
                if let Ok(action) = serde_json::from_str::<Value>(params["payload"].as_str().unwrap_or("")) {
                    self.last_action = Some((
                        now(),
                        Trigger {
                            kind: action["kind"].as_str().unwrap_or("click").to_string(),
                            label: action["label"].as_str().unwrap_or("unknown").to_string(),
                        },
                    ));
                }
            }
            "Network.requestWillBeSent" => {
                let request = &params["request"];
                self.inflight.insert(id, Exchange {
                    t: now(),
                    source: "browser",
                    method: request["method"].as_str().unwrap_or("GET").to_string(),
                    url: request["url"].as_str().unwrap_or_default().to_string(),
                    req_headers: headers(&request["headers"]),
                    req_body: request["postData"].as_str().map(str::to_string),
                    status: 0,
                    res_headers: BTreeMap::new(),
                    res_body: None,
                    ms: 0,
                    trigger: self.recent_action(),
                });
            }
            // Cookies and other browser-added headers only appear here, and we need
            // them verbatim so captured calls can be replayed later.
            "Network.requestWillBeSentExtraInfo" => {
                if let Some(pending) = self.inflight.get_mut(&id) {
                    pending.req_headers.extend(headers(&params["headers"]));
                }
            }
            "Network.responseReceived" => {
                if let Some(pending) = self.inflight.get_mut(&id) {
                    pending.status = params["response"]["status"].as_u64().unwrap_or(0) as u16;
                    pending.res_headers.extend(headers(&params["response"]["headers"]));
                }
            }
            "Network.loadingFinished" => {
                if let Some(mut pending) = self.inflight.remove(&id) {
                    pending.res_body = cdp.response_body(&id).await;
                    pending.ms = ((now() - pending.t) * 1000.0) as u64;
                    pending.emit();
                }
            }
            "Network.loadingFailed" => {
                self.inflight.remove(&id);
            }
            _ => {}
        }
    }

    fn recent_action(&self) -> Option<Trigger> {
        self.last_action
            .as_ref()
            .filter(|(at, _)| now() - at < ATTRIBUTION_WINDOW)
            .map(|(_, trigger)| trigger.clone())
    }
}

fn headers(value: &Value) -> BTreeMap<String, String> {
    value
        .as_object()
        .map(|map| {
            map.iter()
                // Drop HTTP/2 pseudo-headers (`:method`, `:path`): they describe the
                // frame, not the request, and would be rejected on replay.
                .filter(|(k, _)| !k.starts_with(':'))
                .map(|(k, v)| (k.to_ascii_lowercase(), v.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

async fn launch(chrome: Option<PathBuf>, profile: &Path, port: u16, open: &str, headless: bool) -> Result<Child> {
    let binary = chrome.unwrap_or_else(|| PathBuf::from(CHROME));
    if !binary.exists() {
        bail!("chrome not found at {}", binary.display());
    }
    let mut command = Command::new(binary);
    if headless {
        command.args(["--headless=new", "--disable-gpu"]);
    }
    command
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .args(["--no-first-run", "--no-default-browser-check", "--remote-allow-origins=*"])
        .arg(open)
        .spawn()
        .context("launching chrome")
}

/// Polls Chrome's HTTP debugging endpoint until the page target is ready.
async fn discover_target(port: u16) -> Result<String> {
    let endpoint = format!("http://127.0.0.1:{port}/json");
    for _ in 0..50 {
        if let Ok(response) = reqwest::get(&endpoint).await
            && let Ok(targets) = response.json::<Vec<Value>>().await
                && let Some(url) = targets
                    .iter()
                    .find(|t| t["type"] == "page" && t["webSocketDebuggerUrl"].is_string())
                    .and_then(|t| t["webSocketDebuggerUrl"].as_str())
                {
                    return Ok(url.to_string());
                }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    bail!("no chrome page target appeared on port {port}")
}

/// A CDP connection: request/response calls plus a stream of unsolicited events.
#[derive(Clone)]
struct Cdp {
    outgoing: mpsc::UnboundedSender<Message>,
    waiting: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: Arc<AtomicU64>,
}

impl Cdp {
    async fn connect(ws_url: &str) -> Result<(Self, mpsc::UnboundedReceiver<Value>)> {
        let (stream, _) = connect_async(ws_url).await.context("connecting to CDP")?;
        let (mut sink, mut source) = stream.split();
        let (outgoing, mut to_send) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let waiting: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> = Arc::default();

        tokio::spawn(async move {
            while let Some(message) = to_send.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }
        });

        let inbox = Arc::clone(&waiting);
        tokio::spawn(async move {
            while let Some(Ok(Message::Text(text))) = source.next().await {
                let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                match value["id"].as_u64() {
                    Some(id) => {
                        if let Some(reply) = inbox.lock().await.remove(&id) {
                            let _ = reply.send(value);
                        }
                    }
                    None => {
                        let _ = event_tx.send(value);
                    }
                }
            }
        });

        Ok((Self { outgoing, waiting, next_id: Arc::new(AtomicU64::new(1)) }, event_rx))
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().await.insert(id, tx);
        self.outgoing
            .send(Message::Text(json!({"id": id, "method": method, "params": params}).to_string().into()))
            .map_err(|_| anyhow::anyhow!("CDP connection closed"))?;

        let reply = timeout(Duration::from_secs(5), rx)
            .await
            .with_context(|| format!("timed out calling {method}"))??;
        if let Some(error) = reply.get("error") {
            bail!("{method} failed: {error}");
        }
        Ok(reply["result"].clone())
    }

    async fn response_body(&self, request_id: &str) -> Option<String> {
        let result = self
            .call("Network.getResponseBody", json!({"requestId": request_id}))
            .await
            .ok()?;
        if result["base64Encoded"].as_bool().unwrap_or(false) {
            return None;
        }
        result["body"].as_str().map(str::to_string)
    }
}
