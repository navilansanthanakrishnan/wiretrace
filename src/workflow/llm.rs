use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde_json::{Value, json};

use super::types::{
    AutomationGeneration, GeneratedFile, LlmAnalysis, NormalizedEvent, WorkflowContextMap,
    WorkflowSession,
};

#[derive(Clone)]
pub struct WorkflowLlmClient {
    http: Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
}

impl WorkflowLlmClient {
    pub fn from_env() -> Result<Self> {
        let base_url =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
        let api_key = std::env::var("OPENAI_API_KEY").ok();
        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-5".into());
        Ok(Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .context("failed to build workflow llm client")?,
            base_url,
            api_key,
            model,
        })
    }

    pub async fn analyze_session(
        &self,
        session: &WorkflowSession,
        events: &[NormalizedEvent],
    ) -> Result<Option<LlmAnalysis>> {
        let Some(_) = self.api_key else {
            return Ok(None);
        };

        let prompt = build_analysis_prompt(session, events);
        let response = self
            .responses_create(
                "You are an agentic workflow analyst. Build a context map from recorded software traffic. Return only valid JSON.",
                &prompt,
            )
            .await?;
        let text = extract_response_text(&response)?;
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
        let Some(_) = self.api_key else {
            return fallback_automation(session, context_map, prompt);
        };

        let user_prompt = build_automation_prompt(session, context_map, prompt);
        let response = self
            .responses_create(
                "You are an agentic workflow engineer. Given a workflow context map, design and generate a concrete automation implementation. Return only valid JSON.",
                &user_prompt,
            )
            .await?;
        let text = extract_response_text(&response)?;
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

    async fn responses_create(&self, instructions: &str, prompt: &str) -> Result<Value> {
        let api_key = self
            .api_key
            .as_ref()
            .context("OPENAI_API_KEY is not set")?;

        let response = self
            .http
            .post(format!("{}/responses", self.base_url.trim_end_matches('/')))
            .bearer_auth(api_key)
            .json(&json!({
                "model": self.model,
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
    let end = text.rfind('}').context("response did not contain json object end")?;
    serde_json::from_str(&text[start..=end]).context("failed parsing JSON payload from response text")
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
    let file = GeneratedFile {
        path: "automation-plan.md".into(),
        description: "Fallback automation plan generated without an OpenAI API key".into(),
        content: format!(
            "# Automation Request\n\n{}\n\n# Workflow Summary\n\n{}\n\n# Candidate Write Operations\n\n{}\n",
            prompt,
            context_map.summary,
            context_map
                .writes
                .iter()
                .map(|operation| format!("- {}", operation.signature))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    };

    Ok(AutomationGeneration {
        session_id: session.id.clone(),
        prompt: prompt.to_string(),
        summary: "Generated fallback automation planning artifact because OPENAI_API_KEY is not configured.".into(),
        generated_files: vec![file],
        follow_up_notes: vec![
            "Set OPENAI_API_KEY to enable model-backed automation synthesis.".into(),
            "Review automation-plan.md and refine the target write operations.".into(),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::{extract_response_text, parse_json_response};
    use serde_json::json;

    #[test]
    fn extracts_text_from_response_output() {
        let response = json!({
            "output": [
                {
                    "type": "message",
                    "content": [
                        { "type": "output_text", "text": "{\"ok\":true}" }
                    ]
                }
            ]
        });
        assert_eq!(extract_response_text(&response).unwrap(), "{\"ok\":true}");
    }

    #[test]
    fn parses_json_with_leading_text() {
        let value = parse_json_response("Here is JSON\n{\"ok\":true}\n").unwrap();
        assert_eq!(value["ok"], true);
    }
}
