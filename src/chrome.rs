use std::path::{Path, PathBuf};
use std::process::ExitStatus;
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
    let chrome_target = resolve_chrome_target(command.chrome_path.as_deref())?;
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

    let mut child = launch_chrome(&chrome_target, &profile_dir, &command)
        .await
        .with_context(|| {
            format!(
                "failed to launch Chrome at {}",
                chrome_target.display().display()
            )
        })?;

    println!("launched Chrome from {}", chrome_target.display().display());
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
    chrome_target: &ChromeTarget,
    profile_dir: &ManagedProfileDir,
    command: &ChromeCommand,
) -> Result<Child> {
    let mut process = match chrome_target {
        ChromeTarget::MacApp { app_bundle, .. } => {
            let mut command = Command::new("open");
            command.arg("-na");
            command.arg(app_bundle);
            command.arg("--args");
            command
        }
        ChromeTarget::Executable(path) => Command::new(path),
    };

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

fn resolve_chrome_target(override_path: Option<&Path>) -> Result<ChromeTarget> {
    if let Some(path) = override_path {
        if path.is_file() {
            return Ok(chrome_target_from_path(path.to_path_buf()));
        }

        if path.is_dir() && path.extension().and_then(|value| value.to_str()) == Some("app") {
            return Ok(ChromeTarget::MacApp {
                display: path.to_path_buf(),
                app_bundle: path.to_path_buf(),
            });
        }

        bail!("provided Chrome path does not exist: {}", path.display());
    }

    for candidate in DEFAULT_CHROME_PATHS {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(chrome_target_from_path(path));
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

#[derive(Debug, Clone)]
enum ChromeTarget {
    MacApp {
        display: PathBuf,
        app_bundle: PathBuf,
    },
    Executable(PathBuf),
}

impl ChromeTarget {
    fn display(&self) -> &Path {
        match self {
            Self::MacApp { display, .. } => display.as_path(),
            Self::Executable(path) => path.as_path(),
        }
    }
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

fn chrome_target_from_path(path: PathBuf) -> ChromeTarget {
    if let Some(app_bundle) = mac_app_bundle_from_executable(&path) {
        ChromeTarget::MacApp {
            display: path,
            app_bundle,
        }
    } else {
        ChromeTarget::Executable(path)
    }
}

fn mac_app_bundle_from_executable(path: &Path) -> Option<PathBuf> {
    let contents = path
        .components()
        .map(|component| component.as_os_str().to_owned())
        .collect::<Vec<_>>();

    let contents_index = contents
        .iter()
        .position(|component| component == "Contents")?;
    if contents_index == 0 {
        return None;
    }

    Some(contents[..contents_index].iter().collect())
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
