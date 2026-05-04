use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::app::AppPaths;

use super::types::{
    AutomationGeneration, NormalizedEvent, WorkflowContextMap, WorkflowMode, WorkflowSession,
    WorkflowStatus,
};

const SESSION_FILE: &str = "session.json";
const RAW_EVENTS_FILE: &str = "raw-events.jsonl";
const NORMALIZED_EVENTS_FILE: &str = "normalized-events.json";
const CONTEXT_MAP_FILE: &str = "context-map.json";
const AUTOMATION_FILE: &str = "automation.json";

#[derive(Clone)]
pub struct WorkflowStore {
    root: PathBuf,
}

impl WorkflowStore {
    pub fn new(paths: &AppPaths) -> Result<Self> {
        fs::create_dir_all(&paths.workflows_dir)
            .context("failed to create workflows root directory")?;
        Ok(Self {
            root: paths.workflows_dir.clone(),
        })
    }

    pub fn create_session(
        &self,
        id: String,
        name: String,
        mode: WorkflowMode,
        started_at_ms: u128,
    ) -> Result<WorkflowSession> {
        let session_dir = self.root.join(&id);
        let automation_dir = session_dir.join("generated");
        fs::create_dir_all(&automation_dir)
            .with_context(|| format!("failed to create workflow session directory {id}"))?;

        let session = WorkflowSession {
            id,
            name,
            mode,
            status: WorkflowStatus::Recording,
            started_at_ms,
            stopped_at_ms: None,
            event_count: 0,
            recorder_endpoint: None,
            raw_events_path: session_dir.join(RAW_EVENTS_FILE),
            normalized_events_path: session_dir.join(NORMALIZED_EVENTS_FILE),
            context_map_path: session_dir.join(CONTEXT_MAP_FILE),
            automation_dir,
            session_dir: session_dir.clone(),
            error: None,
        };

        self.save_session(&session)?;
        Ok(session)
    }

    pub fn save_session(&self, session: &WorkflowSession) -> Result<()> {
        let content = serde_json::to_vec_pretty(session).context("failed to encode session json")?;
        fs::write(session.session_dir.join(SESSION_FILE), content)
            .with_context(|| format!("failed to write session file for {}", session.id))
    }

    pub fn append_raw_event(&self, session: &WorkflowSession, line: &str) -> Result<()> {
        use std::io::Write;

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&session.raw_events_path)
            .with_context(|| format!("failed to open raw event log for {}", session.id))?;
        writeln!(file, "{line}")
            .with_context(|| format!("failed to append raw event for {}", session.id))
    }

    pub fn save_normalized_events(
        &self,
        session: &WorkflowSession,
        events: &[NormalizedEvent],
    ) -> Result<()> {
        let content =
            serde_json::to_vec_pretty(events).context("failed to encode normalized events json")?;
        fs::write(&session.normalized_events_path, content)
            .with_context(|| format!("failed to write normalized events for {}", session.id))
    }

    pub fn save_context_map(
        &self,
        session: &WorkflowSession,
        context_map: &WorkflowContextMap,
    ) -> Result<()> {
        let content =
            serde_json::to_vec_pretty(context_map).context("failed to encode context map json")?;
        fs::write(&session.context_map_path, content)
            .with_context(|| format!("failed to write context map for {}", session.id))
    }

    pub fn save_automation(
        &self,
        session: &WorkflowSession,
        automation: &AutomationGeneration,
    ) -> Result<()> {
        let content =
            serde_json::to_vec_pretty(automation).context("failed to encode automation json")?;
        fs::write(session.session_dir.join(AUTOMATION_FILE), content)
            .with_context(|| format!("failed to write automation file for {}", session.id))?;

        for generated in &automation.generated_files {
            let path = session.automation_dir.join(&generated.path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create automation directory {}", parent.display()))?;
            }
            fs::write(&path, &generated.content)
                .with_context(|| format!("failed to write generated automation file {}", path.display()))?;
        }

        Ok(())
    }

    pub fn list_sessions(&self) -> Result<Vec<WorkflowSession>> {
        let mut sessions = Vec::new();
        for entry in fs::read_dir(&self.root).context("failed to list workflow sessions")? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path().join(SESSION_FILE);
            if !path.exists() {
                continue;
            }
            let content = fs::read(&path)
                .with_context(|| format!("failed to read session file {}", path.display()))?;
            let session = serde_json::from_slice::<WorkflowSession>(&content)
                .with_context(|| format!("failed to parse session file {}", path.display()))?;
            sessions.push(session);
        }
        sessions.sort_by_key(|session| session.started_at_ms);
        sessions.reverse();
        Ok(sessions)
    }

    pub fn load_session(&self, session_id: &str) -> Result<WorkflowSession> {
        let path = self.root.join(session_id).join(SESSION_FILE);
        if !path.exists() {
            bail!("workflow session {session_id} not found");
        }
        let content = fs::read(&path)
            .with_context(|| format!("failed to read session file {}", path.display()))?;
        serde_json::from_slice(&content)
            .with_context(|| format!("failed to parse session file {}", path.display()))
    }

    pub fn load_raw_events(&self, session: &WorkflowSession) -> Result<Vec<String>> {
        if !session.raw_events_path.exists() {
            return Ok(Vec::new());
        }

        let content = fs::read_to_string(&session.raw_events_path)
            .with_context(|| format!("failed to read raw events for {}", session.id))?;
        Ok(content
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }

    pub fn load_context_map(&self, session: &WorkflowSession) -> Result<WorkflowContextMap> {
        let content = fs::read(&session.context_map_path)
            .with_context(|| format!("failed to read context map for {}", session.id))?;
        serde_json::from_slice(&content)
            .with_context(|| format!("failed to parse context map for {}", session.id))
    }
}
