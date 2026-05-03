use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use hudsucker::certificate_authority::CertificateAuthority;
use hudsucker::hyper::http::uri::Authority;
use hudsucker::openssl::{
    asn1::{Asn1Integer, Asn1Time},
    bn::BigNum,
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    rand,
    x509::{
        X509, X509Builder, X509NameBuilder,
        extension::{
            AuthorityKeyIdentifier, BasicConstraints, ExtendedKeyUsage, KeyUsage,
            SubjectAlternativeName, SubjectKeyIdentifier,
        },
    },
};
use hudsucker::rcgen::{
    BasicConstraints as RcBasicConstraints, CertificateParams, DnType, IsCa, KeyPair,
    KeyUsagePurpose,
};
use hudsucker::rustls::crypto::aws_lc_rs;
use hudsucker::rustls::{
    ServerConfig,
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
};
use tokio::sync::Mutex;

use crate::app::AppPaths;

const CERT_FILENAME: &str = "agent-mcp-b-ca-cert.pem";
const KEY_FILENAME: &str = "agent-mcp-b-ca-key.pem";
const TTL_SECS: i64 = 365 * 24 * 60 * 60;
const NOT_BEFORE_OFFSET: i64 = 60;

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

    pub fn load_or_create(&self) -> Result<LocalCertificateAuthority> {
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

        let ca_private_key = PKey::private_key_from_pem(key_pem.as_bytes())
            .context("failed parsing CA private key PEM")?;
        let ca_cert = X509::from_pem(cert_pem.as_bytes()).context("failed parsing CA cert PEM")?;

        Ok(LocalCertificateAuthority::new(
            ca_private_key,
            ca_cert,
            aws_lc_rs::default_provider(),
        ))
    }
}

fn generate_ca_material() -> Result<(String, String)> {
    let mut params =
        CertificateParams::new(Vec::new()).context("failed creating CA certificate params")?;
    params.is_ca = IsCa::Ca(RcBasicConstraints::Unconstrained);
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

#[derive(Debug)]
pub struct LocalCertificateAuthority {
    ca_private_key: PKey<Private>,
    ca_cert: X509,
    ca_chain_der: CertificateDer<'static>,
    provider: Arc<CryptoProvider>,
    cache: Mutex<HashMap<String, Arc<ServerConfig>>>,
}

impl LocalCertificateAuthority {
    fn new(ca_private_key: PKey<Private>, ca_cert: X509, provider: CryptoProvider) -> Self {
        let ca_chain_der =
            CertificateDer::from(ca_cert.to_der().expect("CA certificate must encode to DER"));

        Self {
            ca_private_key,
            ca_cert,
            ca_chain_der,
            provider: Arc::new(provider),
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn gen_server_config_for_authority(&self, authority: &Authority) -> Result<Arc<ServerConfig>> {
        let host = authority.host();

        let leaf_key = PKey::ec_gen("prime256v1").context("failed generating leaf EC key")?;
        let leaf_cert = self
            .gen_leaf_cert(host, &leaf_key)
            .with_context(|| format!("failed generating leaf certificate for {host}"))?;

        let certs = vec![
            CertificateDer::from(
                leaf_cert
                    .to_der()
                    .context("failed encoding leaf certificate to DER")?,
            ),
            self.ca_chain_der.clone(),
        ];

        let private_key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(
            leaf_key
                .private_key_to_pkcs8()
                .context("failed encoding leaf private key to PKCS8")?,
        ));

        let mut server_cfg = ServerConfig::builder_with_provider(Arc::clone(&self.provider))
            .with_safe_default_protocol_versions()
            .context("failed to set protocol versions")?
            .with_no_client_auth()
            .with_single_cert(certs, private_key)
            .context("failed building rustls server config with leaf certificate")?;

        server_cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Arc::new(server_cfg))
    }

    fn gen_leaf_cert(&self, host: &str, leaf_key: &PKey<Private>) -> Result<X509> {
        let mut name_builder = X509NameBuilder::new().context("failed creating X509NameBuilder")?;
        name_builder
            .append_entry_by_nid(Nid::COMMONNAME, host)
            .context("failed setting leaf common name")?;
        let name = name_builder.build();

        let mut builder = X509Builder::new().context("failed creating leaf X509Builder")?;
        builder
            .set_version(2)
            .context("failed setting leaf cert version")?;
        builder
            .set_subject_name(&name)
            .context("failed setting leaf subject")?;
        builder
            .set_issuer_name(self.ca_cert.subject_name())
            .context("failed setting leaf issuer")?;
        builder
            .set_pubkey(leaf_key)
            .context("failed setting leaf public key")?;

        let not_before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time must be after epoch")
            .as_secs() as i64
            - NOT_BEFORE_OFFSET;

        builder
            .set_not_before(Asn1Time::from_unix(not_before)?.as_ref())
            .context("failed setting leaf not_before")?;
        builder
            .set_not_after(Asn1Time::from_unix(not_before + TTL_SECS)?.as_ref())
            .context("failed setting leaf not_after")?;

        let mut serial_number = [0_u8; 16];
        rand::rand_bytes(&mut serial_number).context("failed generating leaf serial")?;
        let serial = BigNum::from_slice(&serial_number).context("failed creating BigNum serial")?;
        let serial = Asn1Integer::from_bn(&serial).context("failed creating ASN1 serial")?;
        builder
            .set_serial_number(&serial)
            .context("failed setting leaf serial")?;

        builder
            .append_extension(
                BasicConstraints::new()
                    .critical()
                    .build()
                    .context("failed building basic constraints")?,
            )
            .context("failed appending basic constraints")?;
        builder
            .append_extension(
                KeyUsage::new()
                    .digital_signature()
                    .key_encipherment()
                    .build()
                    .context("failed building key usage")?,
            )
            .context("failed appending key usage")?;
        builder
            .append_extension(
                ExtendedKeyUsage::new()
                    .server_auth()
                    .build()
                    .context("failed building extended key usage")?,
            )
            .context("failed appending extended key usage")?;

        let subject_alt_name = {
            let context = builder.x509v3_context(Some(&self.ca_cert), None);
            SubjectAlternativeName::new()
                .dns(host)
                .build(&context)
                .context("failed building subject alternative name")?
        };
        let subject_key_identifier = {
            let context = builder.x509v3_context(Some(&self.ca_cert), None);
            SubjectKeyIdentifier::new()
                .build(&context)
                .context("failed building subject key identifier")?
        };
        let authority_key_identifier = {
            let context = builder.x509v3_context(Some(&self.ca_cert), None);
            AuthorityKeyIdentifier::new()
                .keyid(true)
                .issuer(true)
                .build(&context)
                .context("failed building authority key identifier")?
        };
        builder
            .append_extension(subject_alt_name)
            .context("failed appending subject alternative name")?;
        builder
            .append_extension(subject_key_identifier)
            .context("failed appending subject key identifier")?;
        builder
            .append_extension(authority_key_identifier)
            .context("failed appending authority key identifier")?;

        builder
            .sign(&self.ca_private_key, MessageDigest::sha256())
            .context("failed signing leaf certificate")?;

        Ok(builder.build())
    }
}

impl CertificateAuthority for LocalCertificateAuthority {
    async fn gen_server_config(&self, authority: &Authority) -> Arc<ServerConfig> {
        let host = authority.host().to_string();

        {
            let cache = self.cache.lock().await;
            if let Some(config) = cache.get(&host) {
                return Arc::clone(config);
            }
        }

        let config = self
            .gen_server_config_for_authority(authority)
            .unwrap_or_else(|error| panic!("failed generating server config for {host}: {error}"));

        let mut cache = self.cache.lock().await;
        cache.insert(host, Arc::clone(&config));
        config
    }
}
