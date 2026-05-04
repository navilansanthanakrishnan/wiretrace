use anyhow::Result;

use crate::app::AppPaths;
use crate::cli::AttachCommand;
use crate::proxy;
use crate::system_proxy::{self, ProxySnapshot};

pub async fn run(paths: &AppPaths, command: AttachCommand) -> Result<()> {
    proxy::ensure_listen_available(command.proxy.listen)?;

    let snapshot = system_proxy::capture_snapshot(&command.service).await?;
    system_proxy::enable_local_proxy(&command.service, command.proxy.listen).await?;
    let mut restore_guard =
        ProxyRestoreGuard::new(command.service.clone(), snapshot, command.leave_enabled);

    println!("attached system proxy on service {}", command.service);
    println!("existing apps should use the proxy for new requests after reload or navigation");
    println!("press Ctrl+C to stop and restore the previous proxy settings\n");

    let result = proxy::run(paths, command.proxy).await;

    restore_guard.restore_async().await;

    result
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
