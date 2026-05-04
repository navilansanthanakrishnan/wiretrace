use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};
use url::Url;

use crate::workflow::store::WorkflowStore;
use crate::workflow::types::{
    ConnectionSummary, DomainSummary, LlmAnalysis, NormalizedEvent, OperationSummary,
    RecordingRequest, WorkflowContextMap, WorkflowMode, WorkflowSession, WorkflowStatus,
};

const CHILD_STOP_WAIT: Duration = Duration::from_secs(3);

pub struct ActiveRecorder {
    pub session: WorkflowSession,
    child: Child,
    event_count: Arc<AtomicUsize>,
    reader_task: tokio::task::JoinHandle<()>,
}

impl ActiveRecorder {
    pub async fn stop(mut self, store: &WorkflowStore) -> Result<WorkflowSession> {
        if self.child.id().is_some() {
            let _ = self.child.start_kill();
            let _ = timeout(CHILD_STOP_WAIT, self.child.wait()).await;
        }

        let _ = self.reader_task.await;
        let mut session = store.load_session(&self.session.id)?;
        session.event_count = self.event_count.load(Ordering::SeqCst);
        session.stopped_at_ms = Some(now_ms());
        session.status = WorkflowStatus::Recorded;
        store.save_session(&session)?;
        Ok(session)
    }
}

pub async fn begin_recording(
    store: &WorkflowStore,
    request: RecordingRequest,
) -> Result<ActiveRecorder> {
    let id = format!("wf-{}", now_ms());
    let name = request
        .name
        .clone()
        .unwrap_or_else(|| format!("workflow-{}", &id[3..]));
    let session = store.create_session(id, name, request.mode.clone(), now_ms())?;
    let current_exe = std::env::current_exe().context("failed to resolve current executable")?;
    let mut command = Command::new(current_exe);
    command.kill_on_drop(true);
    command.stdin(Stdio::null());
    command.stderr(Stdio::piped());
    command.stdout(Stdio::piped());

    match request.mode {
        WorkflowMode::Desktop => {
            let listen = format!("127.0.0.1:{}", pick_free_port()?);
            command.args([
                "attach",
                "--listen",
                &listen,
                "--service",
                &request.service,
                "--output",
                "json",
                "--allow-sensitive-output",
            ]);
        }
        WorkflowMode::BrowserDeep => {
            command.args([
                "browser-deep",
                "--open",
                &request.open,
                "--output",
                "json",
            ]);
            if let Some(user_data_dir) = request.user_data_dir.as_ref() {
                command.arg("--user-data-dir").arg(user_data_dir);
            }
        }
    }

    let mut child = command.spawn().context("failed to spawn recorder child process")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to capture recorder stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("failed to capture recorder stderr")?;

    let event_count = Arc::new(AtomicUsize::new(0));
    let session_for_stdout = session.clone();
    let store_for_stdout = store.clone();
    let event_count_for_stdout = Arc::clone(&event_count);
    let stderr_path = session.session_dir.join("stderr.log");
    let stderr_path_for_task = stderr_path.clone();

    let reader_task = tokio::spawn(async move {
        let stdout_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if serde_json::from_str::<Value>(&line).is_ok() {
                    let _ = store_for_stdout.append_raw_event(&session_for_stdout, &line);
                    event_count_for_stdout.fetch_add(1, Ordering::SeqCst);
                }
            }
        });

        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            let file = Arc::new(Mutex::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(stderr_path_for_task)
                    .ok(),
            ));
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(file) = file.lock().await.as_mut() {
                    use std::io::Write;
                    let _ = writeln!(file, "{line}");
                }
            }
        });

        let _ = stdout_task.await;
        let _ = stderr_task.await;
    });

    Ok(ActiveRecorder {
        session,
        child,
        event_count,
        reader_task,
    })
}

pub fn normalize_raw_events(lines: &[String]) -> Result<Vec<NormalizedEvent>> {
    let mut events = Vec::new();

    for line in lines {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => continue,
        };

        if value.get("type").and_then(Value::as_str) == Some("response") {
            let request = &value["request"];
            let response = &value["response"];
            let url = request["url"].as_str().unwrap_or_default().to_string();
            let parsed = Url::parse(&url).ok();
            events.push(NormalizedEvent {
                source: "desktop_proxy".to_string(),
                timestamp_ms: request["timestamp_ms"].as_u64().map(u128::from),
                interaction_id: request
                    .get("interaction")
                    .and_then(|value| value.get("id"))
                    .and_then(Value::as_u64),
                interaction_kind: request
                    .get("interaction")
                    .and_then(|value| value.get("trigger"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                interaction_element: request
                    .get("interaction")
                    .and_then(|value| value.get("app_name"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                method: request["method"].as_str().unwrap_or_default().to_string(),
                host: request["host"].as_str().unwrap_or_default().to_string(),
                path: parsed
                    .as_ref()
                    .map(|value| value.path().to_string())
                    .unwrap_or_default(),
                url,
                status: response["status"].as_u64().map(|value| value as u16),
                request_summary: request
                    .get("body")
                    .and_then(|value| value.get("preview"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                response_summary: response
                    .get("body")
                    .and_then(|value| value.get("preview"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                request_headers: header_entries_to_map(request.get("headers")),
                response_headers: header_entries_to_map(response.get("headers")),
            });
        } else if value.get("interaction_id").is_some() && value.get("url").is_some() {
            let url = value["url"].as_str().unwrap_or_default().to_string();
            let parsed = Url::parse(&url).ok();
            events.push(NormalizedEvent {
                source: "browser_deep".to_string(),
                timestamp_ms: None,
                interaction_id: value["interaction_id"].as_u64(),
                interaction_kind: value["interaction_kind"].as_str().map(ToOwned::to_owned),
                interaction_element: value["interaction_element"].as_str().map(ToOwned::to_owned),
                method: value["method"].as_str().unwrap_or_default().to_string(),
                host: parsed
                    .as_ref()
                    .and_then(|value| value.host_str().map(ToOwned::to_owned))
                    .unwrap_or_default(),
                path: parsed
                    .as_ref()
                    .map(|value| value.path().to_string())
                    .unwrap_or_default(),
                url,
                status: value["status"].as_u64().map(|status| status as u16),
                request_summary: value["request_summary"].as_str().map(ToOwned::to_owned),
                response_summary: value["response_summary"].as_str().map(ToOwned::to_owned),
                request_headers: BTreeMap::new(),
                response_headers: BTreeMap::new(),
            });
        }
    }

    Ok(events)
}

pub fn build_context_map(
    session: &WorkflowSession,
    events: &[NormalizedEvent],
    llm_analysis: Option<LlmAnalysis>,
) -> WorkflowContextMap {
    let mut domains: HashMap<String, DomainSummary> = HashMap::new();
    let mut operations: HashMap<String, OperationSummary> = HashMap::new();
    let mut auth_signals = BTreeMap::<String, usize>::new();
    let mut connections = Vec::<ConnectionSummary>::new();

    for event in events {
        let domain = domains.entry(event.host.clone()).or_insert(DomainSummary {
            host: event.host.clone(),
            request_count: 0,
            write_count: 0,
            read_count: 0,
        });
        domain.request_count += 1;
        if is_write_method(&event.method) {
            domain.write_count += 1;
        } else {
            domain.read_count += 1;
        }

        for (name, value) in event
            .request_headers
            .iter()
            .chain(event.response_headers.iter())
        {
            if name.eq_ignore_ascii_case("authorization")
                || name.eq_ignore_ascii_case("cookie")
                || name.eq_ignore_ascii_case("set-cookie")
            {
                *auth_signals.entry(format!("{name}={value}")).or_default() += 1;
            }
        }

        let signature = format!("{} {}", event.method, normalize_endpoint(&event.host, &event.path));
        let operation = operations
            .entry(signature.clone())
            .or_insert(OperationSummary {
                signature: signature.clone(),
                method: event.method.clone(),
                host: event.host.clone(),
                path: normalize_endpoint(&event.host, &event.path),
                request_count: 0,
                statuses: BTreeMap::new(),
                request_examples: Vec::new(),
                response_examples: Vec::new(),
                interaction_examples: Vec::new(),
            });
        operation.request_count += 1;
        if let Some(status) = event.status {
            *operation.statuses.entry(status.to_string()).or_default() += 1;
        }
        if let Some(request_summary) = event.request_summary.as_ref() {
            push_example(&mut operation.request_examples, request_summary);
        }
        if let Some(response_summary) = event.response_summary.as_ref() {
            push_example(&mut operation.response_examples, response_summary);
        }
        if let Some(kind) = event.interaction_kind.as_ref() {
            let example = format!(
                "{} {}",
                kind,
                event.interaction_element.clone().unwrap_or_default()
            );
            push_example(&mut operation.interaction_examples, &example);
            connections.push(ConnectionSummary {
                source: format!("interaction:{}:{}", event.interaction_id.unwrap_or_default(), kind),
                target: signature.clone(),
                label: event
                    .interaction_element
                    .clone()
                    .unwrap_or_else(|| "interaction".to_string()),
            });
        }
    }

    let mut domain_values = domains.into_values().collect::<Vec<_>>();
    domain_values.sort_by(|left, right| right.request_count.cmp(&left.request_count));

    let mut operation_values = operations.into_values().collect::<Vec<_>>();
    operation_values.sort_by(|left, right| right.request_count.cmp(&left.request_count));

    let writes = operation_values
        .iter()
        .filter(|operation| is_write_method(&operation.method))
        .cloned()
        .collect::<Vec<_>>();
    let reads = operation_values
        .iter()
        .filter(|operation| !is_write_method(&operation.method))
        .cloned()
        .collect::<Vec<_>>();

    WorkflowContextMap {
        session_id: session.id.clone(),
        summary: format!(
            "Captured {} events across {} domains and {} distinct operations.",
            events.len(),
            domain_values.len(),
            operation_values.len()
        ),
        domains: domain_values,
        operations: operation_values,
        writes,
        reads,
        auth_signals: auth_signals.into_keys().take(20).collect(),
        connection_map: connections,
        llm_analysis,
    }
}

fn header_entries_to_map(value: Option<&Value>) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let Some(array) = value.and_then(Value::as_array) else {
        return map;
    };
    for item in array {
        if let (Some(name), Some(value)) = (
            item.get("name").and_then(Value::as_str),
            item.get("value").and_then(Value::as_str),
        ) {
            map.insert(name.to_string(), value.to_string());
        }
    }
    map
}

fn normalize_endpoint(host: &str, path: &str) -> String {
    if path.is_empty() {
        return host.to_string();
    }

    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            if is_identifier_like_segment(segment) {
                ":id".to_string()
            } else {
                segment.to_string()
            }
        })
        .collect::<Vec<_>>();

    format!("{host}/{}", segments.join("/"))
}

fn is_identifier_like_segment(segment: &str) -> bool {
    is_long_numeric_segment(segment) || is_hex_segment(segment) || is_uuid_segment(segment)
}

fn is_long_numeric_segment(segment: &str) -> bool {
    segment.len() >= 2 && segment.chars().all(|character| character.is_ascii_digit())
}

fn is_hex_segment(segment: &str) -> bool {
    matches!(segment.len(), 16..=64) && segment.chars().all(|character| character.is_ascii_hexdigit())
}

fn is_uuid_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 8 | 13 | 18 | 23) {
            if *byte != b'-' {
                return false;
            }
        } else if !byte.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

fn push_example(target: &mut Vec<String>, value: &str) {
    if target.len() >= 3 {
        return;
    }
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return;
    }
    if target.iter().any(|existing| existing == trimmed) {
        return;
    }
    target.push(trimmed.to_string());
}

fn is_write_method(method: &str) -> bool {
    matches!(method, "POST" | "PUT" | "PATCH" | "DELETE")
}

fn pick_free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .context("failed to allocate a free local listen port")?;
    Ok(listener
        .local_addr()
        .context("failed to inspect local listen port")?
        .port())
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{build_context_map, normalize_raw_events};
    use crate::workflow::types::{WorkflowMode, WorkflowSession, WorkflowStatus};
    use std::path::PathBuf;

    fn sample_session() -> WorkflowSession {
        WorkflowSession {
            id: "wf-1".into(),
            name: "sample".into(),
            mode: WorkflowMode::Desktop,
            status: WorkflowStatus::Recorded,
            started_at_ms: 0,
            stopped_at_ms: Some(1),
            event_count: 0,
            session_dir: PathBuf::from("/tmp/wf-1"),
            raw_events_path: PathBuf::from("/tmp/wf-1/raw.jsonl"),
            normalized_events_path: PathBuf::from("/tmp/wf-1/norm.json"),
            context_map_path: PathBuf::from("/tmp/wf-1/map.json"),
            automation_dir: PathBuf::from("/tmp/wf-1/generated"),
            error: None,
        }
    }

    #[test]
    fn normalize_proxy_response_event() {
        let lines = vec![r#"{"type":"response","request":{"timestamp_ms":1,"method":"POST","url":"https://discord.com/api/v9/channels/123/messages","host":"discord.com","headers":[{"name":"authorization","value":"token"}],"body":{"preview":"{\"content\":\"hi\"}"}},"response":{"status":200,"headers":[{"name":"content-type","value":"application/json"}],"body":{"preview":"{\"id\":\"1\"}"}}}"#.into()];
        let events = normalize_raw_events(&lines).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].host, "discord.com");
        assert_eq!(events[0].method, "POST");
    }

    #[test]
    fn context_map_groups_operations() {
        let lines = vec![
            r#"{"type":"response","request":{"timestamp_ms":1,"method":"POST","url":"https://discord.com/api/v9/channels/123/messages","host":"discord.com","headers":[],"body":{"preview":"{\"content\":\"hi\"}"}},"response":{"status":200,"headers":[],"body":{"preview":"{\"id\":\"1\"}"}}}"#.into(),
            r#"{"type":"response","request":{"timestamp_ms":2,"method":"POST","url":"https://discord.com/api/v9/channels/456/messages","host":"discord.com","headers":[],"body":{"preview":"{\"content\":\"yo\"}"}},"response":{"status":200,"headers":[],"body":{"preview":"{\"id\":\"2\"}"}}}"#.into(),
        ];
        let events = normalize_raw_events(&lines).unwrap();
        let context = build_context_map(&sample_session(), &events, None);
        assert_eq!(context.operations.len(), 1);
        assert_eq!(context.domains[0].host, "discord.com");
    }
}
