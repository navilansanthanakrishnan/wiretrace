use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use hudsucker::Proxy;
use hudsucker::rustls::crypto::aws_lc_rs;

use crate::app::AppPaths;
use crate::cli::ProxyCommand;

use super::authority::CertificateAuthorityPaths;
use super::capture::{CaptureConfig, CaptureHandler, Filters};

pub async fn run(paths: &AppPaths, command: ProxyCommand) -> Result<()> {
    run_with_shutdown(paths, command, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

pub async fn run_with_shutdown<F>(
    paths: &AppPaths,
    command: ProxyCommand,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    ensure_listen_available(command.listen)?;

    let ca_paths = CertificateAuthorityPaths::from_app_paths(paths);
    let certificate_authority = ca_paths.load_or_create()?;

    let config = Arc::new(CaptureConfig {
        output_mode: command.output,
        filters: Filters {
            host_contains: command.host_contains,
            url_contains: command.url_contains,
            methods: command.methods,
        },
        body_preview_bytes: command.body_preview_bytes,
        show_connect: command.show_connect,
    });

    println!("proxy listening on http://{}", command.listen);
    println!("ca certificate: {}", ca_paths.cert_path().display());
    println!("press Ctrl+C to stop\n");

    let proxy = Proxy::builder()
        .with_addr(command.listen)
        .with_ca(certificate_authority)
        .with_rustls_connector(aws_lc_rs::default_provider())
        .with_http_handler(CaptureHandler::new(config))
        .with_graceful_shutdown(shutdown)
        .build()
        .context("failed to construct proxy runtime")?;

    proxy.start().await.context("proxy exited with an error")
}

pub fn ensure_listen_available(listen: SocketAddr) -> Result<()> {
    match std::net::TcpListener::bind(listen) {
        Ok(listener) => {
            drop(listener);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            bail!(
                "listen address {listen} is already in use. stop the existing process on that port or choose a different --listen value"
            )
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed checking whether {listen} is available"))
        }
    }
}
