use anyhow::Result;

use crate::app::AppPaths;
use crate::chrome;
use crate::cli::{ChromeCommand, ProxyCommand};
use crate::proxy;

pub async fn run_proxy(paths: &AppPaths, command: ProxyCommand) -> Result<()> {
    proxy::run(paths, command).await
}

pub async fn run_chrome(_paths: &AppPaths, _command: ChromeCommand) -> Result<()> {
    chrome::run(_paths, _command).await
}
