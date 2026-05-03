use std::io::Read;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use brotli::Decompressor;
use flate2::read::{GzDecoder, ZlibDecoder};
use http_body_util::{BodyExt, Full};
use hudsucker::hyper::{HeaderMap, Request, Response, StatusCode, Uri};
use hudsucker::{Body, HttpContext, HttpHandler, RequestOrResponse};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::cli::OutputMode;

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub output_mode: OutputMode,
    pub filters: Filters,
    pub body_preview_bytes: usize,
    pub show_connect: bool,
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

    pub fn matches_connect(&self, host: &str) -> bool {
        self.host_contains.is_empty()
            || self
                .host_contains
                .iter()
                .any(|candidate| host.contains(candidate))
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
        if req.method().as_str().eq_ignore_ascii_case("CONNECT") {
            return self.config.filters.matches_connect(&host);
        }

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
                if should_print_request(&pending.request, &self.config) {
                    self.printer.print_request(&pending.request).await;
                }
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
                if should_print_response(&pending.request, &response, &self.config) {
                    self.printer
                        .print_response(&pending.request, &response)
                        .await;
                }
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

    let captured = CapturedRequest {
        timestamp_ms: now_ms(),
        method: parts.method.to_string(),
        url,
        host,
        version: format!("{:?}", parts.version),
        headers: serialize_headers(&parts.headers),
        focus_headers: summarize_headers(&parts.headers, HeaderFocus::Request),
        body: body_preview(&parts.headers, &bytes, config.body_preview_bytes),
        is_connect: parts.method.as_str().eq_ignore_ascii_case("CONNECT"),
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
        headers: serialize_headers(&parts.headers),
        focus_headers: summarize_headers(&parts.headers, HeaderFocus::Response),
        body: body_preview(&parts.headers, &bytes, config.body_preview_bytes),
    };

    let response = Response::from_parts(parts, Body::from(Full::new(bytes.clone())));
    Ok((response, captured))
}

fn should_print_request(request: &CapturedRequest, config: &CaptureConfig) -> bool {
    match config.output_mode {
        OutputMode::Focused => false,
        OutputMode::Pretty | OutputMode::Json => config.show_connect || !request.is_connect,
    }
}

fn should_print_response(
    request: &CapturedRequest,
    response: &CapturedResponse,
    config: &CaptureConfig,
) -> bool {
    if request.is_connect {
        return config.show_connect && config.output_mode != OutputMode::Focused;
    }

    match config.output_mode {
        OutputMode::Focused => is_api_like(request, response),
        OutputMode::Pretty | OutputMode::Json => true,
    }
}

fn is_api_like(request: &CapturedRequest, response: &CapturedResponse) -> bool {
    let url = request.url.to_ascii_lowercase();
    let request_has_json = request
        .headers
        .iter()
        .any(|header| header.name == "accept" && header.value.contains("json"))
        || request.headers.iter().any(|header| {
            header.name == "content-type"
                && (header.value.contains("json")
                    || header.value.contains("graphql")
                    || header.value.contains("x-www-form-urlencoded"))
        });

    let response_has_json = response.headers.iter().any(|header| {
        header.name == "content-type"
            && (header.value.contains("json")
                || header.value.contains("javascript")
                || header.value.contains("graphql"))
    });

    request.method != "GET"
        || url.contains("/api/")
        || url.contains("/graphql")
        || url.contains("/rpc")
        || url.contains("/query")
        || request.host.starts_with("api.")
        || request_has_json
        || response_has_json
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
    focus_headers: Vec<HeaderEntry>,
    body: BodyPreview,
    is_connect: bool,
}

#[derive(Debug, Clone, Serialize)]
struct CapturedResponse {
    timestamp_ms: u128,
    request_method: String,
    request_url: String,
    status: u16,
    reason: String,
    headers: Vec<HeaderEntry>,
    focus_headers: Vec<HeaderEntry>,
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
    decoded_bytes: usize,
    truncated: bool,
    encoding: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum BodyKind {
    Json,
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
            OutputMode::Focused => {}
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
            OutputMode::Focused => print_focused_flow(request, response),
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

fn print_focused_flow(request: &CapturedRequest, response: &CapturedResponse) {
    println!("\n[flow] {} {}", request.method, request.url);
    println!("  status: {} {}", response.status, response.reason);

    if !request.focus_headers.is_empty() {
        println!("  request-headers:");
        for header in &request.focus_headers {
            println!("    {}: {}", header.name, header.value);
        }
    }

    if !response.focus_headers.is_empty() {
        println!("  response-headers:");
        for header in &response.focus_headers {
            println!("    {}: {}", header.name, header.value);
        }
    }

    if request.body.kind != BodyKind::Empty {
        print_body("request-body", &request.body);
    }

    if response.body.kind != BodyKind::Empty {
        print_body("response-body", &response.body);
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
            value: redact_header(name.as_str(), &String::from_utf8_lossy(value.as_bytes())),
        })
        .collect()
}

fn summarize_headers(headers: &HeaderMap, focus: HeaderFocus) -> Vec<HeaderEntry> {
    let names = match focus {
        HeaderFocus::Request => [
            "content-type",
            "accept",
            "origin",
            "referer",
            "x-requested-with",
            "x-discord-locale",
            "authorization",
        ]
        .as_slice(),
        HeaderFocus::Response => ["content-type", "content-encoding", "cache-control"].as_slice(),
    };

    names
        .iter()
        .filter_map(|name| {
            headers.get(*name).map(|value| HeaderEntry {
                name: (*name).to_string(),
                value: redact_header(name, &String::from_utf8_lossy(value.as_bytes())),
            })
        })
        .collect()
}

fn redact_header(name: &str, value: &str) -> String {
    if is_sensitive_name(name) {
        "<redacted>".into()
    } else {
        value.to_string()
    }
}

fn is_sensitive_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "set-cookie"
            | "x-super-properties"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "api_key"
            | "apikey"
            | "secret"
    )
}

fn body_preview(headers: &HeaderMap, bytes: &[u8], limit: usize) -> BodyPreview {
    if bytes.is_empty() {
        return BodyPreview {
            kind: BodyKind::Empty,
            preview: String::new(),
            original_bytes: 0,
            decoded_bytes: 0,
            truncated: false,
            encoding: None,
        };
    }

    let encoding = headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase());

    let decoded = decode_body(bytes, encoding.as_deref()).unwrap_or_else(|| bytes.to_vec());
    let truncated = decoded.len() > limit;
    let slice = &decoded[..decoded.len().min(limit)];
    let content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if let Some(pretty_json) = render_json_preview(slice) {
        return BodyPreview {
            kind: BodyKind::Json,
            preview: pretty_json,
            original_bytes: bytes.len(),
            decoded_bytes: decoded.len(),
            truncated,
            encoding,
        };
    }

    if is_textual_content_type(&content_type) || std::str::from_utf8(slice).is_ok() {
        return BodyPreview {
            kind: BodyKind::Text,
            preview: String::from_utf8_lossy(slice).into_owned(),
            original_bytes: bytes.len(),
            decoded_bytes: decoded.len(),
            truncated,
            encoding,
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
        decoded_bytes: decoded.len(),
        truncated,
        encoding,
    }
}

fn render_json_preview(slice: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(slice).ok()?;
    let mut value = serde_json::from_str::<Value>(text).ok()?;
    redact_json_value(&mut value);
    serde_json::to_string_pretty(&value).ok()
}

fn redact_json_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, nested) in map.iter_mut() {
                if is_sensitive_name(key) {
                    *nested = Value::String("<redacted>".into());
                } else {
                    redact_json_value(nested);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_json_value(item);
            }
        }
        _ => {}
    }
}

fn decode_body(bytes: &[u8], encoding: Option<&str>) -> Option<Vec<u8>> {
    match encoding {
        Some("gzip") => {
            let mut decoder = GzDecoder::new(bytes);
            let mut output = Vec::new();
            decoder.read_to_end(&mut output).ok()?;
            Some(output)
        }
        Some("br") => {
            let mut decoder = Decompressor::new(bytes, 4096);
            let mut output = Vec::new();
            decoder.read_to_end(&mut output).ok()?;
            Some(output)
        }
        Some("deflate") => {
            let mut decoder = ZlibDecoder::new(bytes);
            let mut output = Vec::new();
            decoder.read_to_end(&mut output).ok()?;
            Some(output)
        }
        _ => Some(bytes.to_vec()),
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
            || value.contains("application/problem+json")
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
        BodyKind::Json | BodyKind::Text | BodyKind::Binary => {
            println!(
                "  {label}: kind={:?} bytes={} decoded={} truncated={}{}",
                body.kind,
                body.original_bytes,
                body.decoded_bytes,
                body.truncated,
                body.encoding
                    .as_ref()
                    .map(|value| format!(" encoding={value}"))
                    .unwrap_or_default()
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

#[derive(Debug, Clone, Copy)]
enum HeaderFocus {
    Request,
    Response,
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::{Compression, write::GzEncoder};
    use hudsucker::hyper::{HeaderMap, http::HeaderValue};

    use super::{
        BodyKind, BodyPreview, CapturedRequest, CapturedResponse, Filters, HeaderEntry,
        body_preview, is_api_like, serialize_headers,
    };

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

    #[test]
    fn body_preview_formats_json() {
        let headers = HeaderMap::new();
        let preview = body_preview(&headers, br#"{"ok":true,"items":[1,2]}"#, 512);
        assert_eq!(preview.kind, BodyKind::Json);
        assert!(preview.preview.contains("\"ok\": true"));
    }

    #[test]
    fn serialize_headers_redacts_sensitive_values() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer super-secret"));
        headers.insert("cookie", HeaderValue::from_static("token=super-secret"));
        headers.insert("accept", HeaderValue::from_static("application/json"));

        let serialized = serialize_headers(&headers);

        assert!(serialized.iter().any(|entry| {
            entry.name == "authorization" && entry.value == "<redacted>"
        }));
        assert!(serialized
            .iter()
            .any(|entry| entry.name == "cookie" && entry.value == "<redacted>"));
        assert!(serialized.iter().any(|entry| {
            entry.name == "accept" && entry.value == "application/json"
        }));
    }

    #[test]
    fn body_preview_decodes_gzip_json() {
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", HeaderValue::from_static("gzip"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(br#"{"ok":true,"items":[1,2]}"#).unwrap();
        let bytes = encoder.finish().unwrap();

        let preview = body_preview(&headers, &bytes, 512);
        assert_eq!(preview.kind, BodyKind::Json);
        assert_eq!(preview.encoding.as_deref(), Some("gzip"));
        assert!(preview.preview.contains("\"items\": ["));
    }

    #[test]
    fn body_preview_redacts_sensitive_json_fields() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));

        let preview = body_preview(
            &headers,
            br#"{"headers":{"Authorization":"Bearer super-secret-token"},"access_token":"abc123"}"#,
            512,
        );

        assert_eq!(preview.kind, BodyKind::Json);
        assert!(preview.preview.contains("\"Authorization\": \"<redacted>\""));
        assert!(preview.preview.contains("\"access_token\": \"<redacted>\""));
        assert!(!preview.preview.contains("super-secret-token"));
        assert!(!preview.preview.contains("abc123"));
    }

    #[test]
    fn api_like_heuristic_ignores_basic_html_get() {
        let request = CapturedRequest {
            timestamp_ms: 0,
            method: "GET".into(),
            url: "https://example.com/".into(),
            host: "example.com".into(),
            version: "HTTP/1.1".into(),
            headers: vec![HeaderEntry {
                name: "accept".into(),
                value: "text/html".into(),
            }],
            focus_headers: Vec::new(),
            body: empty_body_preview(),
            is_connect: false,
        };

        let response = CapturedResponse {
            timestamp_ms: 0,
            request_method: request.method.clone(),
            request_url: request.url.clone(),
            status: 200,
            reason: "OK".into(),
            headers: vec![HeaderEntry {
                name: "content-type".into(),
                value: "text/html; charset=utf-8".into(),
            }],
            focus_headers: Vec::new(),
            body: BodyPreview {
                kind: BodyKind::Text,
                preview: "<html></html>".into(),
                original_bytes: 13,
                decoded_bytes: 13,
                truncated: false,
                encoding: None,
            },
        };

        assert!(!is_api_like(&request, &response));
    }

    fn empty_body_preview() -> BodyPreview {
        BodyPreview {
            kind: BodyKind::Empty,
            preview: String::new(),
            original_bytes: 0,
            decoded_bytes: 0,
            truncated: false,
            encoding: None,
        }
    }
}
