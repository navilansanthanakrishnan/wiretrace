use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hudsucker::certificate_authority::RcgenAuthority;
use hudsucker::rcgen::{
    BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose,
};
use hudsucker::rustls::crypto::aws_lc_rs;

use crate::app::AppPaths;

const CERT_FILENAME: &str = "agent-mcp-b-ca-cert.pem";
const KEY_FILENAME: &str = "agent-mcp-b-ca-key.pem";
const CERT_CACHE_SIZE: u64 = 1_024;

#[derive(Debug, Clone)]
pub struct CertificateAuthorityPaths {
    cert_path: PathBuf,
    key_path: PathBuf,
}

impl CertificateAuthorityPaths {
    pub fn from_app_paths(paths: &AppPaths) -> Self {
        Self {
            cert_path: paths.certs_dir.join(CERT_FILENAME),
            key_path: paths.certs_dir.join(KEY_FILENAME),
        }
    }

    pub fn cert_path(&self) -> &Path {
        &self.cert_path
    }

    pub fn ensure_materialized(&self) -> Result<()> {
        let _ = self.load_or_create()?;
        Ok(())
    }

    pub fn load_or_create(&self) -> Result<RcgenAuthority> {
        let (cert_pem, key_pem) = if self.cert_path.exists() && self.key_path.exists() {
            let cert =
                fs::read_to_string(&self.cert_path).context("failed reading CA certificate")?;
            let key =
                fs::read_to_string(&self.key_path).context("failed reading CA private key")?;
            (cert, key)
        } else {
            let (cert, key) = generate_ca_material()?;
            fs::write(&self.cert_path, &cert).context("failed writing CA certificate")?;
            fs::write(&self.key_path, &key).context("failed writing CA private key")?;
            (cert, key)
        };

        let key_pair = KeyPair::from_pem(&key_pem).context("failed parsing CA private key PEM")?;
        let issuer = Issuer::from_ca_cert_pem(&cert_pem, key_pair)
            .context("failed parsing generated CA certificate PEM")?;

        Ok(RcgenAuthority::new(
            issuer,
            CERT_CACHE_SIZE,
            aws_lc_rs::default_provider(),
        ))
    }
}

fn generate_ca_material() -> Result<(String, String)> {
    let mut params =
        CertificateParams::new(Vec::new()).context("failed creating CA certificate params")?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "agent-mcp-b Local CA");
    params
        .distinguished_name
        .push(DnType::OrganizationName, "agent-mcp-b");
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::CrlSign,
    ];

    let key_pair = KeyPair::generate().context("failed generating CA key pair")?;
    let certificate = params
        .self_signed(&key_pair)
        .context("failed self-signing CA certificate")?;

    Ok((certificate.pem(), key_pair.serialize_pem()))
}
