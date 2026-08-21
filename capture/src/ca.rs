//! Local certificate authority for TLS interception.
//!
//! One CA is generated per install and stored as PEM. Leaf certificates are
//! issued per host and cached. macOS trust installation lives in Python
//! (`reqtrace/system.py`) — it is just a `security` invocation.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use hudsucker::rcgen::date_time_ymd;
use hudsucker::certificate_authority::CertificateAuthority;
use hudsucker::hyper::http::uri::Authority;
use hudsucker::rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType, string::Ia5String,
};
use hudsucker::rustls::crypto::{CryptoProvider, aws_lc_rs};
use hudsucker::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use hudsucker::rustls::ServerConfig;
use tokio::sync::Mutex;

pub const COMMON_NAME: &str = "reqtrace local CA";

/// rcgen's defaults run from 1975 to 4096. A certificate valid for two millennia
/// is a red flag on sight and invites future policy to reject it, so both the CA
/// and its leaves get an ordinary lifetime.
const CA_YEARS: i32 = 5;
const LEAF_YEARS: i32 = 1;

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

pub struct Ca {
    pub cert_path: PathBuf,
    key_path: PathBuf,
}

impl Ca {
    /// Loads the CA from `dir`, generating it on first use.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let ca = Self {
            cert_path: dir.join("ca-cert.pem"),
            key_path: dir.join("ca-key.pem"),
        };
        if ca.cert_path.exists() && ca.key_path.exists() {
            return Ok(ca);
        }

        create_private_dir(dir).context("creating cert directory")?;
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::CrlSign,
        ];
        params.distinguished_name.push(DnType::CommonName, COMMON_NAME);
        (params.not_before, params.not_after) = validity(CA_YEARS);
        let cert = params.self_signed(&key)?;

        fs::write(&ca.cert_path, cert.pem())?;
        write_private(&ca.key_path, &key.serialize_pem())?;
        Ok(ca)
    }

    pub fn issuer(&self) -> Result<Issuer<'static, KeyPair>> {
        let key = KeyPair::from_pem(&fs::read_to_string(&self.key_path)?)?;
        let cert = fs::read_to_string(&self.cert_path)?;
        Issuer::from_ca_cert_pem(&cert, key).context("parsing CA certificate")
    }
}

/// Issues a leaf certificate per host, signed by the local CA.
///
/// Leaves carry an authority key identifier, basic constraints and an extended
/// key usage. Lenient clients do not need them; anything doing strict X.509
/// validation — notably Python 3.13+, which enables it by default — rejects the
/// certificate outright without them.
pub struct Issuing {
    issuer: Issuer<'static, KeyPair>,
    ca_der: CertificateDer<'static>,
    provider: Arc<CryptoProvider>,
    cache: Mutex<HashMap<String, Arc<ServerConfig>>>,
}

impl Issuing {
    pub fn new(ca: &Ca) -> Result<Self> {
        let pem = fs::read_to_string(&ca.cert_path)?;
        let der = pem_to_der(&pem).context("decoding CA certificate")?;
        Ok(Self {
            issuer: ca.issuer()?,
            ca_der: CertificateDer::from(der),
            provider: Arc::new(aws_lc_rs::default_provider()),
            cache: Mutex::new(HashMap::new()),
        })
    }

    fn issue(&self, host: &str) -> Result<Arc<ServerConfig>> {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::default();
        params.distinguished_name.push(DnType::CommonName, host);
        params.subject_alt_names = vec![SanType::DnsName(Ia5String::try_from(host)?)];
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        (params.not_before, params.not_after) = validity(LEAF_YEARS);

        let leaf = params.signed_by(&key, &self.issuer)?;
        let chain = vec![CertificateDer::from(leaf.der().to_vec()), self.ca_der.clone()];
        let private = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(key.serialize_der()));

        let mut config = ServerConfig::builder_with_provider(Arc::clone(&self.provider))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(chain, private)?;
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }
}

impl CertificateAuthority for Issuing {
    async fn gen_server_config(&self, authority: &Authority) -> Arc<ServerConfig> {
        let host = authority.host().to_string();
        let mut cache = self.cache.lock().await;
        if let Some(config) = cache.get(&host) {
            return Arc::clone(config);
        }
        let config = self
            .issue(&host)
            .unwrap_or_else(|error| panic!("issuing certificate for {host}: {error}"));
        cache.insert(host, Arc::clone(&config));
        config
    }
}

/// A validity window starting yesterday, to tolerate clock skew.
fn validity(years: i32) -> (time::OffsetDateTime, time::OffsetDateTime) {
    let today = time::OffsetDateTime::now_utc().date();
    let start = today.previous_day().unwrap_or(today);
    let end = start.replace_year(start.year() + years).unwrap_or(start);
    (
        date_time_ymd(start.year(), start.month() as u8, start.day()),
        date_time_ymd(end.year(), end.month() as u8, end.day()),
    )
}

/// The CA private key can mint a certificate for any host. It is owner-only.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::io::Write;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    base64_decode(&body)
}

/// Minimal base64 decode: the only encoded thing this binary ever reads is its
/// own CA certificate, so a dependency for it would not pay for itself.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for byte in input.bytes().filter(|b| !b.is_ascii_whitespace() && *b != b'=') {
        let value = ALPHABET.iter().position(|c| *c == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}
