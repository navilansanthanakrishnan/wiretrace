use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::warn;

use super::types::{
    AutomationGeneration, GeneratedFile, LlmAnalysis, NormalizedEvent, WorkflowContextMap,
    WorkflowSession,
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-5.4";
const DEFAULT_CODEX_BIN: &str = "codex";
const DEFAULT_CODEX_TIMEOUT_SECS: u64 = 240;

#[derive(Clone)]
pub struct WorkflowLlmClient {
    http: Client,
    backend: WorkflowLlmBackend,
}

#[derive(Clone)]
enum WorkflowLlmBackend {
    None,
    ResponsesApi {
        base_url: String,
        api_key: String,
        model: String,
    },
    CodexCli {
        codex_bin: String,
        auth_file: PathBuf,
        model: String,
        timeout_secs: u64,
    },
}

impl WorkflowLlmClient {
    pub fn from_env() -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("failed to build workflow llm client")?;

        let backend = resolve_backend()?;
        Ok(Self { http, backend })
    }

    pub fn provider_name(&self) -> String {
        match &self.backend {
            WorkflowLlmBackend::None => "fallback".to_string(),
            WorkflowLlmBackend::ResponsesApi { model, .. } => {
                format!("responses_api:{model}")
            }
            WorkflowLlmBackend::CodexCli { model, .. } => {
                format!("codex_chatgpt:{model}")
            }
        }
    }

    pub async fn analyze_session(
        &self,
        session: &WorkflowSession,
        events: &[NormalizedEvent],
    ) -> Result<Option<LlmAnalysis>> {
        if matches!(self.backend, WorkflowLlmBackend::None) {
            return Ok(None);
        }

        let prompt = build_analysis_prompt(session, events);
        let text = self
            .generate_text(
                "You are an agentic workflow analyst. Build a context map from recorded software traffic. Return only valid JSON.",
                &prompt,
            )
            .await?;
        let value = parse_json_response(&text)?;

        Ok(Some(LlmAnalysis {
            context_map_markdown: value["context_map_markdown"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            automation_opportunities: string_array(&value["automation_opportunities"]),
            risks: string_array(&value["risks"]),
            recommended_next_steps: string_array(&value["recommended_next_steps"]),
        }))
    }

    pub async fn generate_automation(
        &self,
        session: &WorkflowSession,
        context_map: &WorkflowContextMap,
        prompt: &str,
    ) -> Result<AutomationGeneration> {
        if matches!(self.backend, WorkflowLlmBackend::None) {
            return fallback_automation(session, context_map, prompt);
        }

        let user_prompt = build_automation_prompt(session, context_map, prompt);
        let text = self
            .generate_text(
                "You are an agentic workflow engineer. Given a workflow context map, design and generate a concrete automation implementation. Return only valid JSON.",
                &user_prompt,
            )
            .await?;
        let value = parse_json_response(&text)?;

        Ok(AutomationGeneration {
            session_id: session.id.clone(),
            prompt: prompt.to_string(),
            summary: value["summary"].as_str().unwrap_or_default().to_string(),
            generated_files: value["generated_files"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|item| {
                    Some(GeneratedFile {
                        path: item.get("path")?.as_str()?.to_string(),
                        description: item.get("description")?.as_str()?.to_string(),
                        content: item.get("content")?.as_str()?.to_string(),
                    })
                })
                .collect(),
            follow_up_notes: string_array(&value["follow_up_notes"]),
        })
    }

    async fn generate_text(&self, instructions: &str, prompt: &str) -> Result<String> {
        match &self.backend {
            WorkflowLlmBackend::None => bail!("no workflow llm backend configured"),
            WorkflowLlmBackend::ResponsesApi {
                base_url,
                api_key,
                model,
            } => {
                let response = self
                    .responses_create(base_url, api_key, model, instructions, prompt)
                    .await?;
                extract_response_text(&response)
            }
            WorkflowLlmBackend::CodexCli {
                codex_bin,
                auth_file,
                model,
                timeout_secs,
            } => {
                self.codex_exec(codex_bin, auth_file, model, *timeout_secs, instructions, prompt)
                    .await
            }
        }
    }

    async fn responses_create(
        &self,
        base_url: &str,
        api_key: &str,
        model: &str,
        instructions: &str,
        prompt: &str,
    ) -> Result<Value> {
        let response = self
            .http
            .post(format!("{}/responses", base_url.trim_end_matches('/')))
            .bearer_auth(api_key)
            .json(&json!({
                "model": model,
                "input": [
                    {
                        "role": "system",
                        "content": [
                            {
                                "type": "input_text",
                                "text": instructions
                            }
                        ]
                    },
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "input_text",
                                "text": prompt
                            }
                        ]
                    }
                ]
            }))
            .send()
            .await
            .context("failed sending Responses API request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("OpenAI Responses API request failed: {status} {body}");
        }

        response
            .json::<Value>()
            .await
            .context("failed parsing Responses API response")
    }

    async fn codex_exec(
        &self,
        codex_bin: &str,
        auth_file: &Path,
        model: &str,
        timeout_secs: u64,
        instructions: &str,
        prompt: &str,
    ) -> Result<String> {
        let temp_home = tempdir().context("failed to create temporary CODEX_HOME")?;
        let temp_auth_file = temp_home.path().join("auth.json");
        let temp_config_file = temp_home.path().join("config.toml");
        let output_file = temp_home.path().join("last-message.txt");

        fs::copy(auth_file, &temp_auth_file)
            .await
            .with_context(|| format!("failed to copy Codex auth file from {}", auth_file.display()))?;
        fs::write(
            &temp_config_file,
            format!(
                "model = {model:?}\nforced_login_method = \"chatgpt\"\ncli_auth_credentials_store = \"file\"\nweb_search = \"disabled\"\n"
            ),
        )
        .await
        .context("failed to write temporary Codex config")?;

        let mut child = Command::new(codex_bin)
            .env("CODEX_HOME", temp_home.path())
            .arg("exec")
            .arg("--skip-git-repo-check")
            .arg("--ephemeral")
            .arg("--sandbox")
            .arg("read-only")
            .arg("--output-last-message")
            .arg(&output_file)
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to launch Codex backend binary `{codex_bin}`"))?;

        let combined_prompt = format!(
            "{instructions}\n\nReturn only the requested JSON payload.\n\n{prompt}"
        );
        let mut stdin = child
            .stdin
            .take()
            .context("failed to open stdin for Codex backend process")?;
        let stdin_task = tokio::spawn(async move {
            stdin.write_all(combined_prompt.as_bytes()).await?;
            stdin.shutdown().await
        });

        let output = timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
            .await
            .context("Codex backend timed out")?
            .context("failed waiting for Codex backend process")?;
        stdin_task
            .await
            .context("failed joining Codex backend stdin writer")?
            .context("failed writing prompt to Codex backend stdin")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!(
                "Codex backend request failed with status {}: {}",
                output.status,
                if stderr.is_empty() {
                    "no stderr output".to_string()
                } else {
                    stderr
                }
            );
        }

        let text = fs::read_to_string(&output_file)
            .await
            .context("failed to read Codex backend output")?;
        self.persist_refreshed_auth(auth_file, &temp_auth_file).await;
        Ok(text.trim().to_string())
    }

    async fn persist_refreshed_auth(&self, target: &Path, refreshed: &Path) {
        let original = fs::read(target).await;
        let updated = fs::read(refreshed).await;

        let (Ok(original), Ok(updated)) = (original, updated) else {
            return;
        };
        if original == updated {
            return;
        }

        if let Err(error) = fs::write(target, updated).await {
            warn!(
                "failed to persist refreshed Codex auth bundle back to {}: {}",
                target.display(),
                error
            );
        }
    }
}

fn resolve_backend() -> Result<WorkflowLlmBackend> {
    let preference =
        std::env::var("WORKFLOW_LLM_BACKEND").unwrap_or_else(|_| "auto".to_string());
    let model = std::env::var("WORKFLOW_LLM_MODEL")
        .or_else(|_| std::env::var("OPENAI_MODEL"))
        .unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let api_key = std::env::var("OPENAI_API_KEY").ok();
    let base_url =
        std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
    let codex_bin =
        std::env::var("WORKFLOW_CODEX_BIN").unwrap_or_else(|_| DEFAULT_CODEX_BIN.to_string());
    let codex_auth_file = std::env::var("WORKFLOW_CODEX_AUTH_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_codex_auth_file());
    let timeout_secs = std::env::var("WORKFLOW_CODEX_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_CODEX_TIMEOUT_SECS);

    let responses_backend = || -> Result<WorkflowLlmBackend> {
        let api_key = api_key
            .clone()
            .context("WORKFLOW_LLM_BACKEND=api requires OPENAI_API_KEY")?;
        Ok(WorkflowLlmBackend::ResponsesApi {
            base_url: base_url.clone(),
            api_key,
            model: model.clone(),
        })
    };

    let codex_backend = || -> Result<WorkflowLlmBackend> {
        if !codex_auth_file.exists() {
            bail!(
                "WORKFLOW_LLM_BACKEND=codex requires a file-backed Codex auth bundle at {}",
                codex_auth_file.display()
            );
        }
        Ok(WorkflowLlmBackend::CodexCli {
            codex_bin: codex_bin.clone(),
            auth_file: codex_auth_file.clone(),
            model: model.clone(),
            timeout_secs,
        })
    };

    match preference.as_str() {
        "auto" => {
            if codex_auth_file.exists() {
                codex_backend()
            } else if api_key.is_some() {
                responses_backend()
            } else {
                Ok(WorkflowLlmBackend::None)
            }
        }
        "codex" | "chatgpt" => codex_backend(),
        "api" | "responses" => responses_backend(),
        "none" => Ok(WorkflowLlmBackend::None),
        other => bail!("unsupported workflow llm backend: {other}"),
    }
}

fn default_codex_auth_file() -> PathBuf {
    std::env::var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".codex")
        })
        .join("auth.json")
}

fn build_analysis_prompt(session: &WorkflowSession, events: &[NormalizedEvent]) -> String {
    let compact_events = events
        .iter()
        .take(200)
        .map(|event| {
            format!(
                "{} {} {} -> {:?} interaction={:?} req={} resp={}",
                event.method,
                event.host,
                event.path,
                event.status,
                event.interaction_kind,
                event.request_summary.as_deref().unwrap_or("{}"),
                event.response_summary.as_deref().unwrap_or("{}"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "Analyze this recorded workflow session and return only valid JSON with keys context_map_markdown (string), automation_opportunities (string[]), risks (string[]), recommended_next_steps (string[]).\n\nSession: {}\nMode: {:?}\nEvents: {}\n\nCompact event list:\n{}",
        session.name,
        session.mode,
        events.len(),
        compact_events
    )
}

fn build_automation_prompt(
    session: &WorkflowSession,
    context_map: &WorkflowContextMap,
    prompt: &str,
) -> String {
    let operations = context_map
        .operations
        .iter()
        .take(40)
        .map(|operation| {
            format!(
                "{} count={} statuses={:?} req={:?} resp={:?}",
                operation.signature,
                operation.request_count,
                operation.statuses,
                operation.request_examples,
                operation.response_examples,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "Using this workflow context map, generate an automation implementation for the request below. Return only valid JSON with keys summary (string), generated_files (array of {{path,description,content}}), follow_up_notes (string[]).\n\nSession: {}\nPrompt: {}\nContext summary: {}\nLLM context markdown:\n{}\nOperations:\n{}",
        session.name,
        prompt,
        context_map.summary,
        context_map
            .llm_analysis
            .as_ref()
            .map(|analysis| analysis.context_map_markdown.as_str())
            .unwrap_or(""),
        operations
    )
}

fn extract_response_text(response: &Value) -> Result<String> {
    let output = response["output"].as_array().context("missing output array")?;
    let mut pieces = Vec::new();
    for item in output {
        if item.get("type").and_then(Value::as_str) == Some("message") {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        pieces.push(text.to_string());
                    }
                }
            }
        }
    }

    if pieces.is_empty() {
        bail!("Responses API returned no text output");
    }

    Ok(pieces.join("\n"))
}

fn parse_json_response(text: &str) -> Result<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Ok(value);
    }

    let start = text.find('{').context("response did not contain json object")?;
    let end = text
        .rfind('}')
        .context("response did not contain json object end")?;
    serde_json::from_str(&text[start..=end])
        .context("failed parsing JSON payload from response text")
}

fn string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|item| item.as_str().map(ToOwned::to_owned))
        .collect()
}

fn fallback_automation(
    session: &WorkflowSession,
    context_map: &WorkflowContextMap,
    prompt: &str,
) -> Result<AutomationGeneration> {
    let summary = format!(
        "Fallback automation plan for {} based on {} recorded operations.",
        session.name,
        context_map.operations.len()
    );
    let generated_files = vec![GeneratedFile {
        path: "automation-plan.md".to_string(),
        description: "Deterministic fallback automation plan when no LLM backend is configured."
            .to_string(),
        content: format!(
            "# Automation Request\n\n{}\n\n# Available Operations\n\n{}\n",
            prompt,
            context_map
                .operations
                .iter()
                .map(|operation| format!(
                    "- `{}` x{} {:?}",
                    operation.signature, operation.request_count, operation.statuses
                ))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }];

    Ok(AutomationGeneration {
        session_id: session.id.clone(),
        prompt: prompt.to_string(),
        summary,
        generated_files,
        follow_up_notes: vec![
            "Set OPENAI_API_KEY or use a file-backed Codex auth bundle to enable model-generated automation output."
                .to_string(),
        ],
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{extract_response_text, parse_json_response};

    #[test]
    fn extracts_text_from_response_output() {
        let response = json!({
            "output": [
                {
                    "type": "message",
                    "content": [
                        { "text": "{\"ok\":true}" }
                    ]
                }
            ]
        });

        let text = extract_response_text(&response).expect("text");
        assert_eq!(text, "{\"ok\":true}");
    }

    #[test]
    fn parses_json_with_leading_text() {
        let value = parse_json_response("note\n{\"summary\":\"ok\"}\n").expect("json");
        assert_eq!(value["summary"], "ok");
    }
}
