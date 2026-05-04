use anyhow::Result;

#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

pub async fn wait_for_shutdown_signal() -> Result<&'static str> {
    #[cfg(unix)]
    {
        let mut terminate =
            signal(SignalKind::terminate()).map_err(|error| anyhow::anyhow!(error))?;
        let mut hangup = signal(SignalKind::hangup()).map_err(|error| anyhow::anyhow!(error))?;

        tokio::select! {
            _ = tokio::signal::ctrl_c() => Ok("SIGINT"),
            _ = terminate.recv() => Ok("SIGTERM"),
            _ = hangup.recv() => Ok("SIGHUP"),
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .map_err(|error| anyhow::anyhow!(error))?;
        Ok("SIGINT")
    }
}
