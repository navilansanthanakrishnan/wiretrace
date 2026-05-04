use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

use crate::app::AppPaths;
use crate::cli::{
    WorkflowAskCommand, WorkflowBeginCommand, WorkflowServeCommand,
    WorkflowStatusCommand, WorkflowStopCommand,
};
use crate::workflow::llm::WorkflowLlmClient;
use crate::workflow::recorder::{ActiveRecorder, begin_recording, build_context_map, normalize_raw_events};
use crate::workflow::store::WorkflowStore;
use crate::workflow::types::{
    AutomationGeneration, RecordingRequest, ServerStatus, WorkflowContextMap, WorkflowMode,
    WorkflowSession, WorkflowStatus,
};

#[derive(Clone)]
pub struct WorkflowServerState {
    store: WorkflowStore,
    llm: WorkflowLlmClient,
    active: Arc<Mutex<Option<ActiveRecorder>>>,
}

pub async fn run_server(paths: &AppPaths, command: WorkflowServeCommand) -> Result<()> {
    let store = WorkflowStore::new(paths)?;
    let state = WorkflowServerState {
        store,
        llm: WorkflowLlmClient::from_env()?,
        active: Arc::new(Mutex::new(None)),
    };

    let router = Router::new()
        .route("/", get(index))
        .route("/api/status", get(get_status))
        .route("/api/recordings/begin", post(begin))
        .route("/api/recordings/stop", post(stop))
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/{session_id}", get(get_session))
        .route("/api/sessions/{session_id}/ask", post(ask))
        .with_state(state);

    println!("workflow server listening on http://{}", command.listen);
    axum::serve(
        tokio::net::TcpListener::bind(command.listen)
            .await
            .context("failed to bind workflow server listener")?,
        router,
    )
    .await
    .context("workflow server exited unexpectedly")
}

pub async fn run_client_begin(command: WorkflowBeginCommand) -> Result<()> {
    let request = RecordingRequest {
        mode: parse_mode(&command.mode)?,
        service: command.service,
        open: command.open,
        user_data_dir: command.user_data_dir,
        name: command.name,
        host_contains: command.host_contains,
        url_contains: command.url_contains,
        methods: command.methods,
    };
    let client = reqwest::Client::new();
    let response = client
        .post(api_url(command.server, "/api/recordings/begin"))
        .json(&request)
        .send()
        .await
        .context("failed to contact workflow server for begin")?;
    ensure_success(response).await
}

pub async fn run_client_stop(command: WorkflowStopCommand) -> Result<()> {
    let client = reqwest::Client::new();
    let response = client
        .post(api_url(command.server, "/api/recordings/stop"))
        .send()
        .await
        .context("failed to contact workflow server for stop")?;
    ensure_success(response).await
}

pub async fn run_client_status(command: WorkflowStatusCommand) -> Result<()> {
    let status = reqwest::get(api_url(command.server, "/api/status"))
        .await
        .context("failed to contact workflow server for status")?
        .json::<ServerStatus>()
        .await
        .context("failed to parse workflow status response")?;
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

pub async fn run_client_ask(command: WorkflowAskCommand) -> Result<()> {
    let client = reqwest::Client::new();
    let request = AskRequest {
        prompt: command.prompt,
    };
    let session_path = match command.session_id {
        Some(id) => format!("/api/sessions/{id}/ask"),
        None => "/api/sessions/latest/ask".to_string(),
    };
    let response = client
        .post(api_url(command.server, &session_path))
        .json(&request)
        .send()
        .await
        .context("failed to contact workflow server for ask")?;
    ensure_success(response).await
}

async fn begin(
    State(state): State<WorkflowServerState>,
    Json(request): Json<RecordingRequest>,
) -> Result<Json<WorkflowSession>, AppError> {
    let mut active = state.active.lock().await;
    if active.is_some() {
        return Err(AppError::bad_request("a recording session is already active"));
    }
    let recorder = begin_recording(&state.store, request)
        .await
        .map_err(AppError::internal)?;
    let session = recorder.session.clone();
    *active = Some(recorder);
    Ok(Json(session))
}

async fn stop(State(state): State<WorkflowServerState>) -> Result<Json<WorkflowSession>, AppError> {
    let recorder = {
        let mut active = state.active.lock().await;
        active
            .take()
            .ok_or_else(|| AppError::bad_request("no active recording session"))?
    };

    let mut session = recorder.stop(&state.store).await.map_err(AppError::internal)?;
    session.status = WorkflowStatus::Analyzing;
    state.store.save_session(&session).map_err(AppError::internal)?;

    let raw_events = state.store.load_raw_events(&session).map_err(AppError::internal)?;
    let normalized = normalize_raw_events(&raw_events).map_err(AppError::internal)?;
    state
        .store
        .save_normalized_events(&session, &normalized)
        .map_err(AppError::internal)?;

    let llm_analysis = state
        .llm
        .analyze_session(&session, &normalized)
        .await
        .map_err(AppError::internal)?;
    let context_map = build_context_map(&session, &normalized, llm_analysis);
    state
        .store
        .save_context_map(&session, &context_map)
        .map_err(AppError::internal)?;

    session.status = WorkflowStatus::Ready;
    state.store.save_session(&session).map_err(AppError::internal)?;
    Ok(Json(session))
}

async fn get_status(
    State(state): State<WorkflowServerState>,
) -> Result<Json<ServerStatus>, AppError> {
    let active_session = state
        .active
        .lock()
        .await
        .as_ref()
        .map(|recorder| recorder.session.clone());
    let recent_sessions = state.store.list_sessions().map_err(AppError::internal)?;
    Ok(Json(ServerStatus {
        active_session,
        recent_sessions,
    }))
}

async fn list_sessions(
    State(state): State<WorkflowServerState>,
) -> Result<Json<Vec<WorkflowSession>>, AppError> {
    Ok(Json(state.store.list_sessions().map_err(AppError::internal)?))
}

async fn get_session(
    Path(session_id): Path<String>,
    State(state): State<WorkflowServerState>,
) -> Result<Json<SessionDetail>, AppError> {
    let session = resolve_session(&state, &session_id).await?;
    let context_map = if session.context_map_path.exists() {
        Some(state.store.load_context_map(&session).map_err(AppError::internal)?)
    } else {
        None
    };
    Ok(Json(SessionDetail { session, context_map }))
}

async fn ask(
    Path(session_id): Path<String>,
    State(state): State<WorkflowServerState>,
    Json(request): Json<AskRequest>,
) -> Result<Json<AutomationGeneration>, AppError> {
    let session = resolve_session(&state, &session_id).await?;
    let context_map = state
        .store
        .load_context_map(&session)
        .map_err(AppError::internal)?;
    let automation = state
        .llm
        .generate_automation(&session, &context_map, &request.prompt)
        .await
        .map_err(AppError::internal)?;
    state
        .store
        .save_automation(&session, &automation)
        .map_err(AppError::internal)?;
    Ok(Json(automation))
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn resolve_session(
    state: &WorkflowServerState,
    session_id: &str,
) -> Result<WorkflowSession, AppError> {
    if session_id == "latest" {
        let session = state
            .store
            .list_sessions()
            .map_err(AppError::internal)?
            .into_iter()
            .find(|session| matches!(session.status, WorkflowStatus::Ready | WorkflowStatus::Recorded))
            .ok_or_else(|| AppError::bad_request("no completed workflow sessions available"))?;
        Ok(session)
    } else {
        state
            .store
            .load_session(session_id)
            .map_err(AppError::internal)
    }
}

fn parse_mode(mode: &str) -> Result<WorkflowMode> {
    match mode {
        "desktop" => Ok(WorkflowMode::Desktop),
        "browser_deep" | "browser-deep" => Ok(WorkflowMode::BrowserDeep),
        other => bail!("unsupported workflow mode: {other}"),
    }
}

fn api_url(server: SocketAddr, path: &str) -> String {
    format!("http://{}{}", server, path)
}

async fn ensure_success(response: reqwest::Response) -> Result<()> {
    if response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        println!("{body}");
        Ok(())
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("workflow server request failed: {status} {body}")
    }
}

#[derive(Debug, Serialize)]
struct SessionDetail {
    session: WorkflowSession,
    context_map: Option<WorkflowContextMap>,
}

#[derive(Debug, Deserialize, Serialize)]
struct AskRequest {
    prompt: String,
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

const INDEX_HTML: &str = r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Workflow Studio</title>
  <style>
    :root { color-scheme: dark; --bg:#0d1117; --panel:#161b22; --line:#30363d; --text:#e6edf3; --muted:#8b949e; --accent:#2f81f7; }
    body { margin:0; font-family: ui-sans-serif, system-ui, sans-serif; background: radial-gradient(circle at top, #132238, var(--bg) 45%); color:var(--text); }
    .wrap { max-width: 1200px; margin: 0 auto; padding: 32px 20px 80px; }
    .hero { display:flex; justify-content:space-between; gap:24px; align-items:end; margin-bottom:24px; }
    .hero h1 { margin:0; font-size:38px; }
    .hero p { margin:8px 0 0; color:var(--muted); max-width:680px; }
    .grid { display:grid; grid-template-columns: 340px 1fr; gap:20px; }
    .panel { background: rgba(22,27,34,0.9); border:1px solid var(--line); border-radius:18px; padding:18px; backdrop-filter: blur(10px); }
    .panel h2 { margin:0 0 14px; font-size:18px; }
    .stack { display:grid; gap:12px; }
    label { display:grid; gap:6px; color:var(--muted); font-size:13px; }
    input, select, textarea, button { font: inherit; border-radius:12px; border:1px solid var(--line); background:#0d1117; color:var(--text); }
    input, select, textarea { padding:12px; }
    textarea { min-height:140px; resize:vertical; }
    button { padding:12px 14px; cursor:pointer; background:linear-gradient(180deg, #2f81f7, #1f6feb); border:none; }
    button.secondary { background:#21262d; border:1px solid var(--line); }
    .status { font-size:14px; color:var(--muted); }
    .sessions { display:grid; gap:10px; max-height:300px; overflow:auto; }
    .session { border:1px solid var(--line); border-radius:14px; padding:12px; cursor:pointer; }
    .session.active { outline:2px solid var(--accent); }
    pre { white-space: pre-wrap; overflow-wrap:anywhere; background:#0b0f14; padding:14px; border-radius:14px; border:1px solid #222; }
    .split { display:grid; grid-template-columns: 1fr 1fr; gap:16px; }
  </style>
</head>
<body>
  <div class="wrap">
    <div class="hero">
      <div>
        <h1>Workflow Studio</h1>
        <p>Record proxy and browser workflows, analyze the captured graph, and synthesize concrete automation artifacts from the resulting context map.</p>
      </div>
      <div class="status" id="status">Loading…</div>
    </div>
    <div class="grid">
      <div class="panel stack">
        <h2>Recorder</h2>
        <label>Mode
          <select id="mode">
            <option value="desktop">desktop</option>
            <option value="browser_deep">browser_deep</option>
          </select>
        </label>
        <label>Name
          <input id="name" placeholder="discord send-message flow" />
        </label>
        <label>Service
          <input id="service" value="Wi-Fi" />
        </label>
        <label>Open URL
          <input id="open" value="https://example.com" />
        </label>
        <label>User Data Dir
          <input id="profile" placeholder="/tmp/workflow-browser-profile" />
        </label>
        <label>Host Filters
          <input id="hostFilters" placeholder="discord.com,api.example.com" />
        </label>
        <label>URL Filters
          <input id="urlFilters" placeholder="/api/,/trpc/" />
        </label>
        <label>Method Filters
          <input id="methodFilters" placeholder="GET,POST" />
        </label>
        <div class="stack">
          <button id="begin">Begin Recording</button>
          <button id="stop" class="secondary">Stop + Analyze</button>
        </div>
        <h2>Sessions</h2>
        <div id="sessions" class="sessions"></div>
      </div>
      <div class="panel stack">
        <h2>Context Map</h2>
        <pre id="context">Select a session after recording.</pre>
        <div class="split">
          <div class="panel stack">
            <h2>Ask for Automation</h2>
            <textarea id="askPrompt" placeholder="Can you build me an automation that logs into X, opens Y, and submits Z?"></textarea>
            <button id="ask">Generate Automation</button>
          </div>
          <div class="panel stack">
            <h2>Automation Output</h2>
            <pre id="automation">No automation generated yet.</pre>
          </div>
        </div>
      </div>
    </div>
  </div>
  <script>
    let currentSessionId = null;
    async function fetchJson(url, options) {
      const res = await fetch(url, options);
      const text = await res.text();
      const data = text ? JSON.parse(text) : null;
      if (!res.ok) throw new Error((data && data.error) || text || res.statusText);
      return data;
    }
    async function refreshStatus() {
      const data = await fetchJson('/api/status');
      document.getElementById('status').textContent = data.active_session
        ? `Active: ${data.active_session.name} (${data.active_session.mode})`
        : 'Idle';
      const sessions = document.getElementById('sessions');
      sessions.innerHTML = '';
      for (const session of data.recent_sessions) {
        const el = document.createElement('div');
        el.className = 'session' + (session.id === currentSessionId ? ' active' : '');
        el.innerHTML = `<strong>${session.name}</strong><div>${session.mode} · ${session.status} · ${session.event_count} events</div><div>${session.id}</div>`;
        el.onclick = () => loadSession(session.id);
        sessions.appendChild(el);
      }
    }
    async function loadSession(sessionId) {
      currentSessionId = sessionId;
      const data = await fetchJson(`/api/sessions/${sessionId}`);
      document.getElementById('context').textContent = JSON.stringify(data, null, 2);
      await refreshStatus();
    }
    document.getElementById('begin').onclick = async () => {
      await fetchJson('/api/recordings/begin', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          mode: document.getElementById('mode').value,
          name: document.getElementById('name').value || null,
          service: document.getElementById('service').value,
          open: document.getElementById('open').value,
          user_data_dir: document.getElementById('profile').value || null,
          host_contains: document.getElementById('hostFilters').value
            ? document.getElementById('hostFilters').value.split(',').map(value => value.trim()).filter(Boolean)
            : [],
          url_contains: document.getElementById('urlFilters').value
            ? document.getElementById('urlFilters').value.split(',').map(value => value.trim()).filter(Boolean)
            : [],
          methods: document.getElementById('methodFilters').value
            ? document.getElementById('methodFilters').value.split(',').map(value => value.trim()).filter(Boolean)
            : []
        })
      });
      await refreshStatus();
    };
    document.getElementById('stop').onclick = async () => {
      const session = await fetchJson('/api/recordings/stop', { method: 'POST' });
      currentSessionId = session.id;
      await loadSession(session.id);
    };
    document.getElementById('ask').onclick = async () => {
      const sessionId = currentSessionId || 'latest';
      const data = await fetchJson(`/api/sessions/${sessionId}/ask`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ prompt: document.getElementById('askPrompt').value })
      });
      document.getElementById('automation').textContent = JSON.stringify(data, null, 2);
    };
    refreshStatus();
  </script>
</body>
</html>"#;
