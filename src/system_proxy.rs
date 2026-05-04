use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process::Command as ProcessCommand;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::Command as AsyncCommand;

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
    let mut snapshot = ProxySnapshot {
        web: read_proxy(service, ProxyKind::Web).await?,
        secure_web: read_proxy(service, ProxyKind::SecureWeb).await?,
    };
    sanitize_stale_loopback_snapshot(&mut snapshot);
    Ok(snapshot)
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

pub fn restore_snapshot_blocking(service: &str, snapshot: &ProxySnapshot) -> Result<()> {
    restore_proxy_blocking(service, ProxyKind::Web, &snapshot.web)?;
    restore_proxy_blocking(service, ProxyKind::SecureWeb, &snapshot.secure_web)
}

async fn restore_proxy(service: &str, kind: ProxyKind, settings: &ProxySettings) -> Result<()> {
    if settings.enabled {
        set_proxy(service, kind, &settings.server, &settings.port.to_string()).await?;
        set_proxy_state(service, kind, true).await
    } else {
        set_proxy_state(service, kind, false).await
    }
}

fn restore_proxy_blocking(service: &str, kind: ProxyKind, settings: &ProxySettings) -> Result<()> {
    if settings.enabled {
        set_proxy_blocking(service, kind, &settings.server, &settings.port.to_string())?;
        set_proxy_state_blocking(service, kind, true)
    } else {
        set_proxy_state_blocking(service, kind, false)
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

fn set_proxy_blocking(service: &str, kind: ProxyKind, host: &str, port: &str) -> Result<()> {
    let args = [kind.set_command(), service, host, port, "off"];
    run_networksetup_blocking(&args).map(|_| ())
}

fn set_proxy_state_blocking(service: &str, kind: ProxyKind, enabled: bool) -> Result<()> {
    let state = if enabled { "on" } else { "off" };
    let args = [kind.state_command(), service, state];
    run_networksetup_blocking(&args).map(|_| ())
}

async fn run_networksetup(args: &[&str]) -> Result<String> {
    let output = AsyncCommand::new("networksetup")
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to execute networksetup {}", args.join(" ")))?;

    validate_networksetup_output(args, output.status.success(), &output.stdout, &output.stderr)
}

fn run_networksetup_blocking(args: &[&str]) -> Result<String> {
    let output = ProcessCommand::new("networksetup")
        .args(args)
        .output()
        .with_context(|| format!("failed to execute networksetup {}", args.join(" ")))?;

    validate_networksetup_output(args, output.status.success(), &output.stdout, &output.stderr)
}

fn validate_networksetup_output(
    args: &[&str],
    success: bool,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<String> {
    if !success {
        bail!(
            "networksetup {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(stderr).trim()
        );
    }

    Ok(String::from_utf8_lossy(stdout).into_owned())
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

fn sanitize_stale_loopback_snapshot(snapshot: &mut ProxySnapshot) {
    sanitize_stale_loopback_setting(&mut snapshot.web);
    sanitize_stale_loopback_setting(&mut snapshot.secure_web);
}

fn sanitize_stale_loopback_setting(settings: &mut ProxySettings) {
    if !settings.enabled || !is_loopback_host(&settings.server) || settings.port == 0 {
        return;
    }

    if loopback_listener_alive(&settings.server, settings.port) {
        return;
    }

    settings.enabled = false;
    settings.server.clear();
    settings.port = 0;
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn loopback_listener_alive(host: &str, port: u16) -> bool {
    let connect_host = if host == "localhost" { "127.0.0.1" } else { host };
    let Ok(mut addrs) = (connect_host, port).to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.next() else {
        return false;
    };

    TcpStream::connect_timeout(&addr, Duration::from_millis(150)).is_ok()
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

#[cfg(test)]
mod tests {
    use super::{
        ProxySettings, ProxySnapshot, is_loopback_host, sanitize_stale_loopback_snapshot,
    };

    #[test]
    fn stale_loopback_snapshot_is_downgraded_to_disabled() {
        let mut snapshot = ProxySnapshot {
            web: ProxySettings {
                enabled: true,
                server: "127.0.0.1".to_string(),
                port: 65000,
            },
            secure_web: ProxySettings {
                enabled: true,
                server: "localhost".to_string(),
                port: 65001,
            },
        };

        sanitize_stale_loopback_snapshot(&mut snapshot);

        assert!(!snapshot.web.enabled);
        assert_eq!(snapshot.web.server, "");
        assert_eq!(snapshot.web.port, 0);
        assert!(!snapshot.secure_web.enabled);
        assert_eq!(snapshot.secure_web.server, "");
        assert_eq!(snapshot.secure_web.port, 0);
    }

    #[test]
    fn non_loopback_snapshot_is_preserved() {
        let mut snapshot = ProxySnapshot {
            web: ProxySettings {
                enabled: true,
                server: "10.0.0.2".to_string(),
                port: 8080,
            },
            secure_web: ProxySettings {
                enabled: false,
                server: String::new(),
                port: 0,
            },
        };

        sanitize_stale_loopback_snapshot(&mut snapshot);

        assert!(snapshot.web.enabled);
        assert_eq!(snapshot.web.server, "10.0.0.2");
        assert_eq!(snapshot.web.port, 8080);
    }

    #[test]
    fn loopback_host_matches_supported_values() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("10.0.0.2"));
    }
}
