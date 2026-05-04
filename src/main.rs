mod app;
mod attach;
mod chrome;
mod cli;
mod commands;
mod interaction;
mod local_ca;
mod logging;
mod proxy;
mod shutdown;
mod system_proxy;

use anyhow::Result;
use clap::Parser;

use crate::app::AppPaths;
use crate::cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    logging::init(&cli.log_filter)?;

    let paths = AppPaths::resolve()?;
    paths.ensure()?;

    match cli.command {
        Command::Proxy(command) => commands::run_proxy(&paths, command).await?,
        Command::Chrome(command) => commands::run_chrome(&paths, command).await?,
        Command::Attach(command) => commands::run_attach(&paths, command).await?,
        Command::Ca(command) => commands::run_ca(&paths, command).await?,
        Command::Paths => {
            println!("root={}", paths.root.display());
            println!("certs={}", paths.certs_dir.display());
            println!("logs={}", paths.logs_dir.display());
        }
    }

    Ok(())
}
