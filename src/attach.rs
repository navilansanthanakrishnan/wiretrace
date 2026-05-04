use anyhow::Result;
use tokio::sync::oneshot;
use tokio::time::{Duration, timeout};

use crate::app::AppPaths;
use crate::cli::AttachCommand;
use crate::proxy;
use crate::shutdown;
use crate::system_proxy::{self, ProxySnapshot};

const FORCE_SHUTDOWN_WAIT: Duration = Duration::from_secs(2);

pub async fn run(paths: &AppPaths, command: AttachCommand) -> Result<()> {
    proxy::ensure_listen_available(command.proxy.listen)?;

    let snapshot = system_proxy::capture_snapshot(&command.service).await?;
    system_proxy::enable_local_proxy(&command.service, command.proxy.listen).await?;
    let mut restore_guard =
        ProxyRestoreGuard::new(command.service.clone(), snapshot, command.leave_enabled);
    let proxy_paths = paths.clone();
    let proxy_command = command.proxy.clone();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let mut shutdown_tx = Some(shutdown_tx);

    println!("attached system proxy on service {}", command.service);
    println!("existing apps should use the proxy for new requests after reload or navigation");
    println!("press Ctrl+C to stop and restore the previous proxy settings\n");

    let mut proxy_task = tokio::spawn(async move {
        proxy::run_with_shutdown(&proxy_paths, proxy_command, async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    let outcome = tokio::select! {
        result = &mut proxy_task => AttachOutcome::ProxyFinished(
            result.expect("proxy task join failure")
        ),
        signal = shutdown::wait_for_shutdown_signal() => AttachOutcome::ShutdownSignal(
            signal?
        ),
    };

    match outcome {
        AttachOutcome::ProxyFinished(result) => {
            restore_guard.restore_async().await;
            result
        }
        AttachOutcome::ShutdownSignal(signal_name) => {
            eprintln!("received {signal_name}, restoring proxy settings");
            restore_guard.restore_async().await;

            if let Some(tx) = shutdown_tx.take() {
                let _ = tx.send(());
            }

            match timeout(FORCE_SHUTDOWN_WAIT, &mut proxy_task).await {
                Ok(join_result) => {
                    join_result.expect("proxy task join failure during shutdown")?;
                }
                Err(_) => {
                    eprintln!(
                        "proxy shutdown exceeded {}s; aborting proxy task",
                        FORCE_SHUTDOWN_WAIT.as_secs()
                    );
                    proxy_task.abort();
                    let _ = proxy_task.await;
                }
            }

            Ok(())
        }
    }
}

enum AttachOutcome {
    ProxyFinished(Result<()>),
    ShutdownSignal(&'static str),
}

struct ProxyRestoreGuard {
    service: String,
    snapshot: Option<ProxySnapshot>,
    leave_enabled: bool,
}

impl ProxyRestoreGuard {
    fn new(service: String, snapshot: ProxySnapshot, leave_enabled: bool) -> Self {
        Self {
            service,
            snapshot: Some(snapshot),
            leave_enabled,
        }
    }

    async fn restore_async(&mut self) {
        if self.leave_enabled {
            println!("leaving macOS system proxy enabled on {}", self.service);
            self.snapshot = None;
            return;
        }

        let Some(snapshot) = self.snapshot.take() else {
            return;
        };

        if let Err(error) = system_proxy::restore_snapshot(&self.service, &snapshot).await {
            eprintln!("failed restoring previous proxy settings: {error}");
            self.snapshot = Some(snapshot);
        } else {
            println!("restored previous proxy settings on {}", self.service);
        }
    }
}

impl Drop for ProxyRestoreGuard {
    fn drop(&mut self) {
        if self.leave_enabled {
            return;
        }

        let Some(snapshot) = self.snapshot.take() else {
            return;
        };

        if let Err(error) = system_proxy::restore_snapshot_blocking(&self.service, &snapshot) {
            eprintln!(
                "failed restoring previous proxy settings during cleanup: {error}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProxyRestoreGuard;
    use crate::system_proxy::{ProxySettings, ProxySnapshot};

    fn sample_snapshot() -> ProxySnapshot {
        ProxySnapshot {
            web: ProxySettings {
                enabled: false,
                server: String::new(),
                port: 0,
            },
            secure_web: ProxySettings {
                enabled: false,
                server: String::new(),
                port: 0,
            },
        }
    }

    #[test]
    fn drop_guard_does_not_panic_when_leave_enabled() {
        let guard = ProxyRestoreGuard::new("Wi-Fi".to_string(), sample_snapshot(), true);
        drop(guard);
    }
}
