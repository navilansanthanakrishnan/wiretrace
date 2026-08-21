//! The only thing this binary produces: one JSON line per completed HTTP exchange.
//!
//! Everything downstream (inference, replay, MCP generation) reads this shape and
//! nothing else, so it is the whole contract between the Rust capture layer and
//! the Python pipeline.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Bodies larger than this are truncated. Enough for any real API payload.
pub const BODY_LIMIT: usize = 64 * 1024;

#[derive(Serialize)]
pub struct Exchange {
    /// Unix seconds, when the request left the client.
    pub t: f64,
    /// `proxy` or `browser` — how we saw it.
    pub source: &'static str,
    pub method: String,
    pub url: String,
    pub req_headers: BTreeMap<String, String>,
    pub req_body: Option<String>,
    pub status: u16,
    pub res_headers: BTreeMap<String, String>,
    pub res_body: Option<String>,
    /// Round-trip duration in milliseconds.
    pub ms: u64,
    /// The UI action this request is attributed to, when we can see one.
    pub trigger: Option<Trigger>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Trigger {
    /// `click`, `submit`, `keydown`, ...
    pub kind: String,
    /// Human-readable element description, e.g. `button "Send"`.
    pub label: String,
}

impl Exchange {
    pub fn emit(&self) {
        if let Ok(line) = serde_json::to_string(self) {
            println!("{line}");
        }
    }
}

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

/// Decode a body to text, or `None` if it is binary. Truncates at [`BODY_LIMIT`].
pub fn body_text(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    let head = &bytes[..bytes.len().min(BODY_LIMIT)];
    let text = String::from_utf8_lossy(head);
    // A high replacement-character ratio means this was never text.
    if text.chars().filter(|c| *c == '\u{FFFD}').count() * 20 > text.chars().count() {
        return None;
    }
    Some(text.into_owned())
}
