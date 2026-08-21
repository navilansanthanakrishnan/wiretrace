//! HTTPS-intercepting proxy.
//!
//! Terminates TLS with a leaf certificate signed by our local CA, forwards the
//! request upstream over a second TLS session, and emits the plaintext exchange.
//! This is the path used for native desktop apps, which reach it via the macOS
//! system proxy settings.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use http_body_util::BodyExt;
use hudsucker::hyper::{HeaderMap, Request, Response, Uri};
use hudsucker::rustls::crypto::aws_lc_rs;
use hudsucker::{Body, HttpContext, HttpHandler, Proxy, RequestOrResponse, decode_response};

use crate::ca::{Ca, Issuing};
use crate::event::{Exchange, body_text, now};

pub async fn run(listen: SocketAddr, cert_dir: &Path, hosts: Vec<String>) -> Result<()> {
    let ca = Ca::load_or_create(cert_dir)?;
    eprintln!("proxy listening on {listen} (ca: {})", ca.cert_path.display());

    let proxy = Proxy::builder()
        .with_addr(listen)
        .with_ca(Issuing::new(&ca)?)
        .with_rustls_connector(aws_lc_rs::default_provider())
        .with_http_handler(Handler {
            hosts: Arc::new(hosts),
            pending: None,
        })
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .build()
        .context("building proxy")?;

    proxy.start().await.context("proxy failed")
}

#[derive(Clone)]
struct Handler {
    /// Host substrings to intercept. Empty means everything.
    hosts: Arc<Vec<String>>,
    pending: Option<(f64, String, String, HeaderMap, Option<String>)>,
}

impl Handler {
    fn wanted(&self, host: &str) -> bool {
        self.hosts.is_empty() || self.hosts.iter().any(|h| host.contains(h.as_str()))
    }
}

impl HttpHandler for Handler {
    /// Returning false makes hudsucker blind-tunnel the connection, so traffic we
    /// were not asked to watch is never decrypted.
    async fn should_intercept(&mut self, _ctx: &HttpContext, req: &Request<Body>) -> bool {
        self.wanted(&host_of(req.headers(), req.uri()))
    }

    async fn handle_request(&mut self, _ctx: &HttpContext, req: Request<Body>) -> RequestOrResponse {
        let (parts, body) = req.into_parts();
        let host = host_of(&parts.headers, &parts.uri);
        if parts.method == hudsucker::hyper::Method::CONNECT || !self.wanted(&host) {
            return RequestOrResponse::Request(Request::from_parts(parts, body));
        }

        let bytes = match body.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(error) => {
                eprintln!("request body read failed: {error}");
                return RequestOrResponse::Request(Request::from_parts(parts, Body::empty()));
            }
        };

        self.pending = Some((
            now(),
            parts.method.to_string(),
            url_of(&host, &parts.uri),
            parts.headers.clone(),
            body_text(&bytes),
        ));
        RequestOrResponse::Request(Request::from_parts(parts, Body::from(bytes)))
    }

    async fn handle_response(&mut self, _ctx: &HttpContext, res: Response<Body>) -> Response<Body> {
        let Some((t, method, url, req_headers, req_body)) = self.pending.take() else {
            return res;
        };
        let res = decode_response(res).unwrap_or_else(|error| {
            eprintln!("response decode failed: {error}");
            Response::new(Body::empty())
        });

        let (parts, body) = res.into_parts();
        let bytes = match body.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(error) => {
                eprintln!("response body read failed: {error}");
                return Response::from_parts(parts, Body::empty());
            }
        };

        Exchange {
            t,
            source: "proxy",
            method,
            url,
            req_headers: headers(&req_headers),
            req_body,
            status: parts.status.as_u16(),
            res_headers: headers(&parts.headers),
            res_body: body_text(&bytes),
            ms: ((now() - t) * 1000.0) as u64,
            trigger: None,
        }
        .emit();

        Response::from_parts(parts, Body::from(bytes))
    }
}

fn headers(map: &HeaderMap) -> std::collections::BTreeMap<String, String> {
    map.iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// The authority as the client meant it, minus a port that adds nothing.
///
/// Clients disagree: curl sends an origin-form URI, others send an absolute one
/// carrying `:443`. Dropping only the scheme's default port keeps both spellings
/// of the same request on one endpoint — and one set of credentials — while a
/// service on a real non-default port stays addressable.
fn host_of(headers: &HeaderMap, uri: &Uri) -> String {
    let authority = uri
        .authority()
        .map(|authority| authority.as_str().to_string())
        .or_else(|| {
            headers
                .get("host")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        })
        .unwrap_or_default();

    let default_port = match uri.scheme_str() {
        Some("http") => ":80",
        _ => ":443",
    };
    authority
        .strip_suffix(default_port)
        .unwrap_or(&authority)
        .to_string()
}

/// The absolute URL, always rebuilt from the normalized authority.
fn url_of(host: &str, uri: &Uri) -> String {
    let scheme = uri.scheme_str().unwrap_or("https");
    let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    format!("{scheme}://{host}{path}")
}
