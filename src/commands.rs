use anyhow::Result;

use crate::app::AppPaths;
use crate::cli::{ChromeCommand, ProxyCommand};

pub async fn run_proxy(_paths: &AppPaths, _command: ProxyCommand) -> Result<()> {
    tracing::info!("proxy runtime will be implemented in the next commit");
    Ok(())
}

pub async fn run_chrome(_paths: &AppPaths, _command: ChromeCommand) -> Result<()> {
    tracing::info!("chrome launcher will be implemented in the next commit");
    Ok(())
}
