use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowMode {
    Desktop,
    BrowserDeep,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    Recording,
    Recorded,
    Analyzing,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowSession {
    pub id: String,
    pub name: String,
    pub mode: WorkflowMode,
    pub status: WorkflowStatus,
    pub started_at_ms: u128,
    pub stopped_at_ms: Option<u128>,
    pub event_count: usize,
    pub recorder_endpoint: Option<String>,
    pub session_dir: PathBuf,
    pub raw_events_path: PathBuf,
    pub normalized_events_path: PathBuf,
    pub context_map_path: PathBuf,
    pub automation_dir: PathBuf,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingRequest {
    pub mode: WorkflowMode,
    pub service: String,
    pub open: String,
    pub user_data_dir: Option<PathBuf>,
    pub name: Option<String>,
    pub host_contains: Vec<String>,
    pub url_contains: Vec<String>,
    pub methods: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedEvent {
    pub source: String,
    pub timestamp_ms: Option<u128>,
    pub interaction_id: Option<u64>,
    pub interaction_kind: Option<String>,
    pub interaction_element: Option<String>,
    pub method: String,
    pub url: String,
    pub host: String,
    pub path: String,
    pub status: Option<u16>,
    pub request_summary: Option<String>,
    pub response_summary: Option<String>,
    pub request_headers: BTreeMap<String, String>,
    pub response_headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowContextMap {
    pub session_id: String,
    pub summary: String,
    pub domains: Vec<DomainSummary>,
    pub operations: Vec<OperationSummary>,
    pub writes: Vec<OperationSummary>,
    pub reads: Vec<OperationSummary>,
    pub auth_signals: Vec<String>,
    pub connection_map: Vec<ConnectionSummary>,
    pub llm_analysis: Option<LlmAnalysis>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainSummary {
    pub host: String,
    pub request_count: usize,
    pub write_count: usize,
    pub read_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationSummary {
    pub signature: String,
    pub method: String,
    pub host: String,
    pub path: String,
    pub request_count: usize,
    pub statuses: BTreeMap<String, usize>,
    pub request_examples: Vec<String>,
    pub response_examples: Vec<String>,
    pub interaction_examples: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionSummary {
    pub source: String,
    pub target: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmAnalysis {
    pub context_map_markdown: String,
    pub automation_opportunities: Vec<String>,
    pub risks: Vec<String>,
    pub recommended_next_steps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutomationGeneration {
    pub session_id: String,
    pub prompt: String,
    pub summary: String,
    pub generated_files: Vec<GeneratedFile>,
    pub follow_up_notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedFile {
    pub path: String,
    pub description: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatus {
    pub active_session: Option<WorkflowSession>,
    pub recent_sessions: Vec<WorkflowSession>,
}
