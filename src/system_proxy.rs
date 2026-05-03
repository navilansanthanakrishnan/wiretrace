use std::net::SocketAddr;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct ProxySettings {
    pub enabled: bool,
    pub server: String,
    pub port: u16,
}

#[derive(Debug, Clone)]
pub struct ProxySnapshot {
    pub web: ProxySettings,
    pub secure_web: ProxySettings,
}

pub async fn capture_snapshot(service: &str) -> Result<ProxySnapshot> {
    Ok(ProxySnapshot {
        web: read_proxy(service, ProxyKind::Web).await?,
        secure_web: read_proxy(service, ProxyKind::SecureWeb).await?,
    })
}

pub async fn enable_local_proxy(service: &str, listen: SocketAddr) -> Result<()> {
    let host = listen.ip().to_string();
    let port = listen.port().to_string();

    set_proxy(service, ProxyKind::Web, &host, &port).await?;
    set_proxy_state(service, ProxyKind::Web, true).await?;
    set_proxy(service, ProxyKind::SecureWeb, &host, &port).await?;
    set_proxy_state(service, ProxyKind::SecureWeb, true).await
}

pub async fn restore_snapshot(service: &str, snapshot: &ProxySnapshot) -> Result<()> {
    restore_proxy(service, ProxyKind::Web, &snapshot.web).await?;
    restore_proxy(service, ProxyKind::SecureWeb, &snapshot.secure_web).await
}

async fn restore_proxy(service: &str, kind: ProxyKind, settings: &ProxySettings) -> Result<()> {
    if settings.enabled {
        set_proxy(service, kind, &settings.server, &settings.port.to_string()).await?;
        set_proxy_state(service, kind, true).await
    } else {
        set_proxy_state(service, kind, false).await
    }
}

async fn read_proxy(service: &str, kind: ProxyKind) -> Result<ProxySettings> {
    let args = [kind.get_command(), service];
    let output = run_networksetup(&args).await?;
    parse_proxy_output(&output)
}

async fn set_proxy(service: &str, kind: ProxyKind, host: &str, port: &str) -> Result<()> {
    let args = [kind.set_command(), service, host, port, "off"];
    run_networksetup(&args).await.map(|_| ())
}

async fn set_proxy_state(service: &str, kind: ProxyKind, enabled: bool) -> Result<()> {
    let state = if enabled { "on" } else { "off" };
    let args = [kind.state_command(), service, state];
    run_networksetup(&args).await.map(|_| ())
}

async fn run_networksetup(args: &[&str]) -> Result<String> {
    let output = Command::new("networksetup")
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to execute networksetup {}", args.join(" ")))?;

    if !output.status.success() {
        bail!(
            "networksetup {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_proxy_output(output: &str) -> Result<ProxySettings> {
    let mut enabled = None;
    let mut server = None;
    let mut port = None;

    for line in output.lines() {
        if let Some(value) = line.strip_prefix("Enabled: ") {
            enabled = Some(matches!(value.trim(), "Yes" | "1"));
        } else if let Some(value) = line.strip_prefix("Server: ") {
            server = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("Port: ") {
            port = Some(value.trim().parse::<u16>().unwrap_or_default());
        }
    }

    Ok(ProxySettings {
        enabled: enabled.context("missing proxy enabled field from networksetup output")?,
        server: server.unwrap_or_default(),
        port: port.unwrap_or_default(),
    })
}

#[derive(Debug, Clone, Copy)]
enum ProxyKind {
    Web,
    SecureWeb,
}

impl ProxyKind {
    fn get_command(self) -> &'static str {
        match self {
            Self::Web => "-getwebproxy",
            Self::SecureWeb => "-getsecurewebproxy",
        }
    }

    fn set_command(self) -> &'static str {
        match self {
            Self::Web => "-setwebproxy",
            Self::SecureWeb => "-setsecurewebproxy",
        }
    }

    fn state_command(self) -> &'static str {
        match self {
            Self::Web => "-setwebproxystate",
            Self::SecureWeb => "-setsecurewebproxystate",
        }
    }
}
