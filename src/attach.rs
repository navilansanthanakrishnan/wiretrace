use anyhow::Result;

use crate::app::AppPaths;
use crate::cli::AttachCommand;
use crate::proxy;
use crate::system_proxy;

pub async fn run(paths: &AppPaths, command: AttachCommand) -> Result<()> {
    proxy::ensure_listen_available(command.proxy.listen)?;

    let snapshot = system_proxy::capture_snapshot(&command.service).await?;
    system_proxy::enable_local_proxy(&command.service, command.proxy.listen).await?;

    println!("attached system proxy on service {}", command.service);
    println!("existing apps should use the proxy for new requests after reload or navigation");
    println!("press Ctrl+C to stop and restore the previous proxy settings\n");

    let result = proxy::run(paths, command.proxy).await;

    if command.leave_enabled {
        println!("leaving macOS system proxy enabled on {}", command.service);
    } else if let Err(error) = system_proxy::restore_snapshot(&command.service, &snapshot).await {
        eprintln!("failed restoring previous proxy settings: {error}");
    } else {
        println!("restored previous proxy settings on {}", command.service);
    }

    result
}
