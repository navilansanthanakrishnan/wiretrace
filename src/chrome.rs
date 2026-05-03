use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::time::sleep;

use crate::app::AppPaths;
use crate::cli::ChromeCommand;
use crate::proxy;

const DEFAULT_CHROME_PATHS: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
];

pub async fn run(paths: &AppPaths, command: ChromeCommand) -> Result<()> {
    proxy::ensure_listen_available(command.proxy.listen)?;

    let chrome_path = resolve_chrome_path(command.chrome_path.as_deref())?;
    let profile_dir = ManagedProfileDir::new(command.user_data_dir.clone())?;
    let proxy_paths = paths.clone();

    if !command.insecure_ignore_cert_errors {
        println!(
            "managed Chrome launch expects the local CA to be trusted for HTTPS interception.\npass --insecure-ignore-cert-errors for a first-run session if needed.\n"
        );
    }

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let proxy_command = command.proxy.clone();
    let mut proxy_task = tokio::spawn(async move {
        proxy::run_with_shutdown(&proxy_paths, proxy_command, async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    wait_for_proxy(command.proxy.listen).await?;

    let mut child = launch_chrome(&chrome_path, &profile_dir, &command)
        .await
        .with_context(|| format!("failed to launch Chrome at {}", chrome_path.display()))?;

    println!("launched Chrome from {}", chrome_path.display());
    println!("profile directory: {}", profile_dir.path().display());
    println!("proxy address: http://{}", command.proxy.listen);
    println!("close Chrome or press Ctrl+C to stop\n");

    let outcome = tokio::select! {
        proxy_result = &mut proxy_task => SessionOutcome::Proxy(
            proxy_result.context("proxy task join failure")?
        ),
        status = child.wait() => SessionOutcome::ChromeExited(
            status.context("failed waiting for Chrome process")?
        ),
        _ = tokio::signal::ctrl_c() => SessionOutcome::Interrupted,
    };

    match outcome {
        SessionOutcome::Proxy(result) => {
            terminate_child(&mut child).await?;
            result?;
        }
        SessionOutcome::ChromeExited(status) => {
            let _ = shutdown_tx.send(());
            proxy_task
                .await
                .context("proxy task join failure after Chrome exit")??;

            if !status.success() {
                bail!("Chrome exited with status {status}");
            }
        }
        SessionOutcome::Interrupted => {
            terminate_child(&mut child).await?;
            let _ = shutdown_tx.send(());
            proxy_task
                .await
                .context("proxy task join failure after interrupt")??;
        }
    }

    Ok(())
}

async fn launch_chrome(
    chrome_path: &Path,
    profile_dir: &ManagedProfileDir,
    command: &ChromeCommand,
) -> Result<Child> {
    let mut process = Command::new(chrome_path);
    process.kill_on_drop(true);
    process.stdin(Stdio::null());
    process.stdout(Stdio::null());
    process.stderr(Stdio::null());

    process.arg(format!("--proxy-server=http://{}", command.proxy.listen));
    process.arg("--proxy-bypass-list=<-loopback>");
    process.arg("--disable-quic");
    process.arg("--no-first-run");
    process.arg("--no-default-browser-check");
    process.arg("--new-window");
    process.arg(format!("--user-data-dir={}", profile_dir.path().display()));

    if command.insecure_ignore_cert_errors {
        process.arg("--ignore-certificate-errors");
    }

    process.arg(&command.open);
    process.spawn().context("failed to spawn Chrome process")
}

fn resolve_chrome_path(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        bail!("provided Chrome path does not exist: {}", path.display());
    }

    for candidate in DEFAULT_CHROME_PATHS {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }

    bail!("could not locate a Chrome executable in the default macOS install paths")
}

async fn terminate_child(child: &mut Child) -> Result<()> {
    if child.id().is_some() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }

    Ok(())
}

enum SessionOutcome {
    Proxy(Result<()>),
    ChromeExited(ExitStatus),
    Interrupted,
}

enum ManagedProfileDir {
    Persistent(PathBuf),
    Temporary(TempDir),
}

impl ManagedProfileDir {
    fn new(path: Option<PathBuf>) -> Result<Self> {
        match path {
            Some(path) => {
                std::fs::create_dir_all(&path).with_context(|| {
                    format!(
                        "failed to create Chrome profile directory {}",
                        path.display()
                    )
                })?;
                Ok(Self::Persistent(path))
            }
            None => {
                let temp_dir = tempfile::Builder::new()
                    .prefix("agent-mcp-b-chrome-profile-")
                    .tempdir()
                    .context("failed to create temporary Chrome profile directory")?;
                Ok(Self::Temporary(temp_dir))
            }
        }
    }

    fn path(&self) -> &Path {
        match self {
            Self::Persistent(path) => path.as_path(),
            Self::Temporary(temp_dir) => temp_dir.path(),
        }
    }
}

async fn wait_for_proxy(listen: std::net::SocketAddr) -> Result<()> {
    for _ in 0..20 {
        if TcpStream::connect(listen).await.is_ok() {
            return Ok(());
        }

        sleep(Duration::from_millis(100)).await;
    }

    bail!("proxy did not become ready on {listen}")
}
