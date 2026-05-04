use std::net::SocketAddr;
use std::path::PathBuf;
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
use tower_http::services::ServeDir;

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
        .nest_service("/assets", ServeDir::new(ui_dist_dir().join("assets")))
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
        llm_provider: state.llm.provider_name(),
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
    let normalized_events = state
        .store
        .load_normalized_events(&session)
        .map_err(AppError::internal)?;
    let automation = state
        .store
        .load_automation(&session)
        .map_err(AppError::internal)?;
    Ok(Json(SessionDetail {
        session,
        context_map,
        normalized_events,
        automation,
    }))
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

async fn index() -> Result<Html<String>, AppError> {
    let path = ui_dist_dir().join("index.html");
    if path.exists() {
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(AppError::internal)?;
        Ok(Html(content))
    } else {
        Ok(Html(FALLBACK_INDEX_HTML.to_string()))
    }
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
    normalized_events: Vec<crate::workflow::types::NormalizedEvent>,
    automation: Option<AutomationGeneration>,
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

fn ui_dist_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui/dist")
}

const FALLBACK_INDEX_HTML: &str = r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Workflow Studio</title>
  <style>
    :root { color-scheme: dark; --bg:#0a0c0e; --fg:#d7dadc; --muted:#8a9099; --line:#23272e; }
    body { margin:0; background:var(--bg); color:var(--fg); font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace; }
    main { max-width: 880px; margin: 0 auto; padding: 48px 24px; }
    pre { border:1px solid var(--line); padding:16px; overflow:auto; }
  </style>
</head>
<body>
  <main>
    <h1>Workflow Studio UI is not built</h1>
    <pre>Run `npm install` and `npm run ui:build`, then restart `cargo run -- workflow serve`.</pre>
  </main>
</body>
</html>"#;
