use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::app::AppPaths;
use crate::cli::{CaAction, CaCommand};
use crate::proxy::authority::CertificateAuthorityPaths;

pub async fn run(paths: &AppPaths, command: CaCommand) -> Result<()> {
    let authority_paths = CertificateAuthorityPaths::from_app_paths(paths);
    authority_paths.ensure_materialized()?;

    match command.action {
        CaAction::Status => print_status(&authority_paths).await,
        CaAction::Trust => trust_ca(&authority_paths).await,
    }
}

async fn print_status(authority_paths: &CertificateAuthorityPaths) -> Result<()> {
    let cert_path = authority_paths.cert_path();
    println!("ca_certificate={}", cert_path.display());
    println!("exists={}", cert_path.exists());
    println!("login_keychain={}", login_keychain_path().display());
    println!(
        "user_trust_settings_enabled={}",
        user_trust_settings_enabled().await?
    );
    println!(
        "user_trust_contains_ca={}",
        trust_domain_contains_ca(TrustDomain::User).await?
    );
    println!(
        "admin_trust_contains_ca={}",
        trust_domain_contains_ca(TrustDomain::Admin).await?
    );
    Ok(())
}

async fn trust_ca(authority_paths: &CertificateAuthorityPaths) -> Result<()> {
    let cert_path = authority_paths.cert_path();
    let keychain = login_keychain_path();

    let output = Command::new("security")
        .args(["add-trusted-cert", "-r", "trustRoot", "-p", "ssl", "-k"])
        .arg(&keychain)
        .arg(cert_path)
        .output()
        .await
        .context("failed to execute security add-trusted-cert")?;

    if !output.status.success() {
        bail!(
            "failed to trust CA certificate: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    println!("trusted_ca={}", cert_path.display());
    println!("keychain={}", keychain.display());
    println!(
        "user_trust_contains_ca={}",
        trust_domain_contains_ca(TrustDomain::User).await?
    );
    println!("fully quit and reopen Chrome, then reload the target app or site.");
    Ok(())
}

fn login_keychain_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join("Library/Keychains/login.keychain-db")
}

async fn user_trust_settings_enabled() -> Result<bool> {
    let output = Command::new("security")
        .arg("user-trust-settings-enable")
        .output()
        .await
        .context("failed to inspect user trust settings state")?;

    if !output.status.success() {
        bail!(
            "failed to inspect user trust settings state: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).contains("Enabled"))
}

async fn trust_domain_contains_ca(domain: TrustDomain) -> Result<bool> {
    let mut command = Command::new("security");
    command.arg("dump-trust-settings");

    if matches!(domain, TrustDomain::Admin) {
        command.arg("-d");
    }

    let output = command
        .output()
        .await
        .with_context(|| format!("failed to inspect {} trust settings", domain.name()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No Trust Settings were found") {
            return Ok(false);
        }
        bail!(
            "failed to inspect {} trust settings: {}",
            domain.name(),
            stderr.trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).contains("agent-mcp-b Local CA"))
}

#[derive(Clone, Copy)]
enum TrustDomain {
    User,
    Admin,
}

impl TrustDomain {
    fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Admin => "admin",
        }
    }
}
