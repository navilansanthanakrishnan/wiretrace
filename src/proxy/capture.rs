use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full};
use hudsucker::hyper::{HeaderMap, Request, Response, StatusCode, Uri};
use hudsucker::{Body, HttpContext, HttpHandler, RequestOrResponse};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::cli::OutputMode;

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub output_mode: OutputMode,
    pub filters: Filters,
    pub body_preview_bytes: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub host_contains: Vec<String>,
    pub url_contains: Vec<String>,
    pub methods: Vec<String>,
}

impl Filters {
    pub fn matches(&self, method: &str, host: &str, url: &str) -> bool {
        let method_matches = self.methods.is_empty()
            || self
                .methods
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(method));

        let host_matches = self.host_contains.is_empty()
            || self
                .host_contains
                .iter()
                .any(|candidate| host.contains(candidate));

        let url_matches = self.url_contains.is_empty()
            || self
                .url_contains
                .iter()
                .any(|candidate| url.contains(candidate));

        method_matches && host_matches && url_matches
    }
}

#[derive(Debug, Clone)]
pub struct CaptureHandler {
    config: Arc<CaptureConfig>,
    printer: EventPrinter,
    pending_request: Option<PendingRequest>,
}

impl CaptureHandler {
    pub fn new(config: Arc<CaptureConfig>) -> Self {
        Self {
            printer: EventPrinter::new(config.output_mode),
            config,
            pending_request: None,
        }
    }
}

impl HttpHandler for CaptureHandler {
    async fn should_intercept(&mut self, _ctx: &HttpContext, req: &Request<Body>) -> bool {
        let host = host_from_headers(req.headers(), req.uri());
        let url = request_url(req.headers(), req.uri(), Some("https"));
        self.config
            .filters
            .matches(req.method().as_str(), &host, &url)
    }

    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        req: Request<Body>,
    ) -> RequestOrResponse {
        match capture_request(req, &self.config).await {
            Ok((req, Some(pending))) => {
                self.printer.print_request(&pending.request).await;
                self.pending_request = Some(pending);
                RequestOrResponse::Request(req)
            }
            Ok((req, None)) => RequestOrResponse::Request(req),
            Err(error) => {
                tracing::warn!(%error, "failed to capture request");
                RequestOrResponse::Response(internal_proxy_error(
                    "request capture failed before forwarding upstream",
                ))
            }
        }
    }

    async fn handle_response(&mut self, _ctx: &HttpContext, res: Response<Body>) -> Response<Body> {
        let Some(pending) = self.pending_request.take() else {
            return res;
        };

        match capture_response(res, &pending, &self.config).await {
            Ok((res, response)) => {
                self.printer
                    .print_response(&pending.request, &response)
                    .await;
                res
            }
            Err(error) => {
                tracing::warn!(%error, "failed to capture response");
                internal_proxy_error("response capture failed while streaming upstream response")
            }
        }
    }
}

async fn capture_request(
    req: Request<Body>,
    config: &CaptureConfig,
) -> Result<(Request<Body>, Option<PendingRequest>)> {
    let (parts, body) = req.into_parts();
    let host = host_from_headers(&parts.headers, &parts.uri);
    let url = request_url(&parts.headers, &parts.uri, None);

    if !config.filters.matches(parts.method.as_str(), &host, &url) {
        let request = Request::from_parts(parts, body);
        return Ok((request, None));
    }

    let collected = body
        .collect()
        .await
        .context("failed collecting request body")?;
    let bytes = collected.to_bytes();
    let preview = body_preview(&parts.headers, &bytes, config.body_preview_bytes);
    let headers = serialize_headers(&parts.headers);

    let captured = CapturedRequest {
        timestamp_ms: now_ms(),
        method: parts.method.to_string(),
        url,
        host,
        version: format!("{:?}", parts.version),
        headers,
        body: preview,
    };

    let request = Request::from_parts(parts, Body::from(Full::new(bytes.clone())));
    Ok((request, Some(PendingRequest { request: captured })))
}

async fn capture_response(
    res: Response<Body>,
    pending: &PendingRequest,
    config: &CaptureConfig,
) -> Result<(Response<Body>, CapturedResponse)> {
    let (parts, body) = res.into_parts();
    let collected = body
        .collect()
        .await
        .context("failed collecting response body")?;
    let bytes = collected.to_bytes();
    let preview = body_preview(&parts.headers, &bytes, config.body_preview_bytes);
    let headers = serialize_headers(&parts.headers);

    let captured = CapturedResponse {
        timestamp_ms: now_ms(),
        request_method: pending.request.method.clone(),
        request_url: pending.request.url.clone(),
        status: parts.status.as_u16(),
        reason: parts
            .status
            .canonical_reason()
            .unwrap_or("unknown")
            .to_string(),
        headers,
        body: preview,
    };

    let response = Response::from_parts(parts, Body::from(Full::new(bytes.clone())));
    Ok((response, captured))
}

#[derive(Debug, Clone)]
struct PendingRequest {
    request: CapturedRequest,
}

#[derive(Debug, Clone, Serialize)]
struct CapturedRequest {
    timestamp_ms: u128,
    method: String,
    url: String,
    host: String,
    version: String,
    headers: Vec<HeaderEntry>,
    body: BodyPreview,
}

#[derive(Debug, Clone, Serialize)]
struct CapturedResponse {
    timestamp_ms: u128,
    request_method: String,
    request_url: String,
    status: u16,
    reason: String,
    headers: Vec<HeaderEntry>,
    body: BodyPreview,
}

#[derive(Debug, Clone, Serialize)]
struct HeaderEntry {
    name: String,
    value: String,
}

#[derive(Debug, Clone, Serialize)]
struct BodyPreview {
    kind: BodyKind,
    preview: String,
    original_bytes: usize,
    truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum BodyKind {
    Text,
    Binary,
    Empty,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CaptureEvent<'a> {
    Request {
        request: &'a CapturedRequest,
    },
    Response {
        request: &'a CapturedRequest,
        response: &'a CapturedResponse,
    },
}

#[derive(Debug, Clone)]
struct EventPrinter {
    output_mode: OutputMode,
    gate: Arc<Mutex<()>>,
}

impl EventPrinter {
    fn new(output_mode: OutputMode) -> Self {
        Self {
            output_mode,
            gate: Arc::new(Mutex::new(())),
        }
    }

    async fn print_request(&self, request: &CapturedRequest) {
        let _guard = self.gate.lock().await;
        match self.output_mode {
            OutputMode::Pretty => {
                println!(
                    "\n[request] {} {}\n  host: {}\n  version: {}",
                    request.method, request.url, request.host, request.version
                );
                print_headers(&request.headers);
                print_body("request-body", &request.body);
            }
            OutputMode::Json => {
                let event = CaptureEvent::Request { request };
                println!("{}", serde_json::to_string(&event).unwrap_or_default());
            }
        }
    }

    async fn print_response(&self, request: &CapturedRequest, response: &CapturedResponse) {
        let _guard = self.gate.lock().await;
        match self.output_mode {
            OutputMode::Pretty => {
                println!(
                    "[response] {} {} -> {} {}\n",
                    request.method, request.url, response.status, response.reason
                );
                print_headers(&response.headers);
                print_body("response-body", &response.body);
            }
            OutputMode::Json => {
                let event = CaptureEvent::Response { request, response };
                println!("{}", serde_json::to_string(&event).unwrap_or_default());
            }
        }
    }
}

fn host_from_headers(headers: &HeaderMap, uri: &Uri) -> String {
    if let Some(authority) = uri.authority() {
        return authority.host().to_string();
    }

    headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown-host".to_string())
}

fn request_url(headers: &HeaderMap, uri: &Uri, default_scheme: Option<&str>) -> String {
    if uri.scheme().is_some() && uri.authority().is_some() {
        return uri.to_string();
    }

    let scheme = uri.scheme_str().or(default_scheme).unwrap_or("http");
    let host = host_from_headers(headers, uri);
    let path = uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");

    format!("{scheme}://{host}{path}")
}

fn serialize_headers(headers: &HeaderMap) -> Vec<HeaderEntry> {
    headers
        .iter()
        .map(|(name, value)| HeaderEntry {
            name: name.as_str().to_string(),
            value: String::from_utf8_lossy(value.as_bytes()).into_owned(),
        })
        .collect()
}

fn body_preview(headers: &HeaderMap, bytes: &[u8], limit: usize) -> BodyPreview {
    if bytes.is_empty() {
        return BodyPreview {
            kind: BodyKind::Empty,
            preview: String::new(),
            original_bytes: 0,
            truncated: false,
        };
    }

    let truncated = bytes.len() > limit;
    let slice = &bytes[..bytes.len().min(limit)];
    let content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if is_textual_content_type(&content_type) || std::str::from_utf8(slice).is_ok() {
        return BodyPreview {
            kind: BodyKind::Text,
            preview: String::from_utf8_lossy(slice).into_owned(),
            original_bytes: bytes.len(),
            truncated,
        };
    }

    let preview = slice
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");

    BodyPreview {
        kind: BodyKind::Binary,
        preview,
        original_bytes: bytes.len(),
        truncated,
    }
}

fn is_textual_content_type(content_type: &str) -> bool {
    matches!(
        content_type,
        value if value.starts_with("text/")
            || value.contains("application/json")
            || value.contains("application/javascript")
            || value.contains("application/xml")
            || value.contains("application/x-www-form-urlencoded")
            || value.contains("application/graphql")
    )
}

fn print_headers(headers: &[HeaderEntry]) {
    if headers.is_empty() {
        println!("  headers: <none>");
        return;
    }

    println!("  headers:");
    for header in headers {
        println!("    {}: {}", header.name, header.value);
    }
}

fn print_body(label: &str, body: &BodyPreview) {
    match body.kind {
        BodyKind::Empty => println!("  {label}: <empty>"),
        BodyKind::Text | BodyKind::Binary => {
            println!(
                "  {label}: kind={:?} bytes={} truncated={}",
                body.kind, body.original_bytes, body.truncated
            );
            if body.preview.is_empty() {
                println!("    <empty>");
            } else {
                for line in body.preview.lines() {
                    println!("    {line}");
                }
            }
        }
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn internal_proxy_error(message: &'static str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .body(Body::from(message))
        .expect("static 502 response is valid")
}

#[cfg(test)]
mod tests {
    use super::Filters;

    #[test]
    fn filters_match_when_all_constraints_pass() {
        let filters = Filters {
            host_contains: vec!["discord.com".into()],
            url_contains: vec!["/api/".into()],
            methods: vec!["GET".into()],
        };

        assert!(filters.matches(
            "GET",
            "discord.com",
            "https://discord.com/api/v9/channels/1/messages",
        ));
    }

    #[test]
    fn filters_reject_when_method_does_not_match() {
        let filters = Filters {
            methods: vec!["POST".into()],
            ..Filters::default()
        };

        assert!(!filters.matches("GET", "discord.com", "https://discord.com"));
    }
}
