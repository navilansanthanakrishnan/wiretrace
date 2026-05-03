use anyhow::Result;

use crate::app::AppPaths;
use crate::attach;
use crate::chrome;
use crate::cli::{AttachCommand, CaCommand, ChromeCommand, ProxyCommand};
use crate::local_ca;
use crate::proxy;

pub async fn run_proxy(paths: &AppPaths, command: ProxyCommand) -> Result<()> {
    proxy::run(paths, command).await
}

pub async fn run_chrome(_paths: &AppPaths, _command: ChromeCommand) -> Result<()> {
    chrome::run(_paths, _command).await
}

pub async fn run_attach(paths: &AppPaths, command: AttachCommand) -> Result<()> {
    attach::run(paths, command).await
}

pub async fn run_ca(paths: &AppPaths, command: CaCommand) -> Result<()> {
    local_ca::run(paths, command).await
}
