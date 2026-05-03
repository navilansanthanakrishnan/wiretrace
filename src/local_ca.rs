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
        CaAction::Status => print_status(&authority_paths),
        CaAction::Trust => trust_ca(&authority_paths).await,
    }
}

fn print_status(authority_paths: &CertificateAuthorityPaths) -> Result<()> {
    let cert_path = authority_paths.cert_path();
    println!("ca_certificate={}", cert_path.display());
    println!("exists={}", cert_path.exists());
    println!("login_keychain={}", login_keychain_path().display());
    Ok(())
}

async fn trust_ca(authority_paths: &CertificateAuthorityPaths) -> Result<()> {
    let cert_path = authority_paths.cert_path();
    let keychain = login_keychain_path();

    let output = Command::new("security")
        .args([
            "add-trusted-cert",
            "-d",
            "-r",
            "trustRoot",
            "-p",
            "ssl",
            "-k",
        ])
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
    println!("restart Chrome and reload the target app or site.");
    Ok(())
}

fn login_keychain_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join("Library/Keychains/login.keychain-db")
}
