//! `-home-jwt` bootstrap (Go internal/home/certificate.go): parse the enrollment JWT,
//! obtain an mTLS client certificate once over plain RESP, pin the Home CA by SHA-256
//! fingerprint and persist key, certificate and CA as 0600 files under
//! `~/.cli-proxy-api`. The JWT signature is not checked; the pinned CA fingerprint and
//! the enrollment secret carry the trust, exactly as in Go.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use btls::hash::MessageDigest;
use btls::pkey::{PKey, Private};
use btls::rsa::Rsa;
use btls::x509::{X509, X509NameBuilder, X509ReqBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{HomeConfig, HomeTlsConfig};
use crate::error::{Error, Result, redacted_decode_error};
use crate::resp::Value;
use crate::tls::Dialer;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Go `homeJWTClaims`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Claims {
    pub certificate_id: String,
    pub cluster_id: String,
    pub ca_fingerprint: String,
    pub enrollment_secret: String,
    pub ip: String,
    pub port: i64,
    #[serde(rename = "iat")]
    pub issued_at: i64,
}

/// Go `certificatePaths`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub dir: PathBuf,
    pub client_cert: PathBuf,
    pub client_key: PathBuf,
    pub ca_cert: PathBuf,
}

impl Paths {
    /// `~/.cli-proxy-api/{client-crt,client-key,home-ca-crt}.pem` (not `auth-dir`).
    pub fn default_location() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| Error::Other("$HOME is not defined".into()))?;
        Ok(Self::in_dir(PathBuf::from(home).join(".cli-proxy-api")))
    }

    pub fn in_dir(dir: PathBuf) -> Self {
        Self {
            client_cert: dir.join("client-crt.pem"),
            client_key: dir.join("client-key.pem"),
            ca_cert: dir.join("home-ca-crt.pem"),
            dir,
        }
    }
}

fn other(message: impl Into<String>) -> Error {
    Error::Other(message.into())
}

/// Go base64 `DecodeString`: non-zero trailing bits are accepted.
fn engine(padding: DecodePaddingMode) -> GeneralPurpose {
    GeneralPurpose::new(
        &base64::alphabet::URL_SAFE,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(padding),
    )
}

/// Go `parseHomeJWTClaims`.
pub fn parse_claims(raw: &str) -> Result<Claims> {
    let parts: Vec<&str> = raw.trim().split('.').collect();
    if parts.len() != 3 {
        return Err(other("home jwt is invalid"));
    }
    let payload = engine(DecodePaddingMode::RequireNone)
        .decode(parts[1])
        .or_else(|_| engine(DecodePaddingMode::RequireCanonical).decode(parts[1]))
        .map_err(|e| other(format!("illegal base64 data: {e}")))?;
    let claims: Claims = serde_json::from_slice(&payload).map_err(|e| redacted_decode_error("home jwt claims", &e))?;
    if claims.certificate_id.trim().is_empty() {
        return Err(other("home jwt certificate_id is required"));
    }
    if claims.cluster_id.trim().is_empty() {
        return Err(other("home jwt cluster_id is required"));
    }
    if normalize_fingerprint(&claims.ca_fingerprint).is_empty() {
        return Err(other("home jwt ca_fingerprint is required"));
    }
    if claims.enrollment_secret.trim().is_empty() {
        return Err(other("home jwt enrollment_secret is required"));
    }
    if claims.ip.trim().is_empty() || claims.port <= 0 {
        return Err(other("home jwt target address is invalid"));
    }
    // ponytail: Go accepts ports above 65535 and fails at dial; reject them here.
    if claims.port > i64::from(u16::MAX) {
        return Err(other("home jwt target address is invalid"));
    }
    Ok(claims)
}

/// Go `normalizeFingerprint`.
pub fn normalize_fingerprint(fingerprint: &str) -> String {
    fingerprint.trim().to_lowercase().replace([':', ' '], "")
}

/// Go `ConfigFromJWT` with the default certificate directory.
pub async fn config_from_jwt(raw: &str) -> Result<HomeConfig> {
    config_from_jwt_in(raw, &Paths::default_location()?).await
}

/// Go `ConfigFromJWT`: a TLS-enabled Home config whose certificate files exist.
pub async fn config_from_jwt_in(raw: &str, paths: &Paths) -> Result<HomeConfig> {
    let claims = parse_claims(raw)?;
    ensure_files(&claims, paths).await?;
    Ok(HomeConfig {
        enabled: true,
        node_id: claims.certificate_id.trim().to_owned(),
        host: claims.ip.trim().to_owned(),
        port: claims.port as u16,
        disable_cluster_discovery: false,
        tls: HomeTlsConfig {
            enable: true,
            ca_cert: Some(paths.ca_cert.clone()),
            client_cert: Some(paths.client_cert.clone()),
            client_key: Some(paths.client_key.clone()),
            use_target_server_name: true,
            ..HomeTlsConfig::default()
        },
    })
}

fn is_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| !m.is_dir())
}

/// Go `ensureHomeCertificateFiles`.
async fn ensure_files(claims: &Claims, paths: &Paths) -> Result<()> {
    if is_file(&paths.client_cert) && is_file(&paths.client_key) {
        if !is_file(&paths.ca_cert) {
            return Err(other("home ca certificate file is missing"));
        }
        let ca = std::fs::read(&paths.ca_cert).map_err(|e| other(e.to_string()))?;
        verify_ca(&ca, &claims.ca_fingerprint)?;
        for path in [&paths.client_cert, &paths.client_key, &paths.ca_cert] {
            chmod_0600(path)?;
        }
        return Ok(());
    }
    crate::private_fs::create_dir_all(&paths.dir).map_err(|e| other(e.to_string()))?;
    let key = load_or_create_key(&paths.client_key)?;
    let csr = create_csr(&claims.certificate_id, &key)?;
    let response = request_certificate(claims, &csr).await?;
    if response.certificate.trim().is_empty() || response.ca.trim().is_empty() {
        return Err(other("home certificate response is incomplete"));
    }
    verify_ca(response.ca.as_bytes(), &claims.ca_fingerprint)?;
    write_0600(&paths.client_cert, response.certificate.as_bytes())?;
    write_0600(&paths.ca_cert, response.ca.as_bytes())?;
    Ok(())
}

/// Go `verifyCACertificatePEM`.
pub fn verify_ca(pem: &[u8], expected: &str) -> Result<()> {
    let actual = certificate_fingerprint(pem)?;
    let expected = normalize_fingerprint(expected);
    if expected.is_empty() {
        return Err(other("home ca fingerprint is required"));
    }
    if actual != expected {
        return Err(other("home ca fingerprint mismatch"));
    }
    Ok(())
}

/// Go `pem.Decode`: the first block in the input, whatever its type.
fn first_pem_block(input: &[u8]) -> Option<(String, Vec<u8>)> {
    let text = String::from_utf8_lossy(input);
    let start = text.find("-----BEGIN ")?;
    let rest = &text[start + 11..];
    let label_end = rest.find("-----")?;
    let label = rest[..label_end].to_owned();
    let body_start = rest[label_end + 5..].trim_start_matches(['\r', '\n']);
    let end_marker = format!("-----END {label}-----");
    let body = &body_start[..body_start.find(&end_marker)?];
    // Optional RFC 1421 headers end at the first blank line.
    let body = match body
        .find("\n\n")
        .filter(|_| body.lines().next().is_some_and(|l| l.contains(':')))
    {
        Some(i) => &body[i + 2..],
        None => body,
    };
    let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    let der = base64::engine::general_purpose::STANDARD.decode(compact).ok()?;
    Some((label, der))
}

/// Go `certificateFingerprintPEM`: SHA-256 of the first certificate's DER, hex.
pub fn certificate_fingerprint(pem: &[u8]) -> Result<String> {
    let Some((_, der)) = first_pem_block(pem).filter(|(label, _)| label == "CERTIFICATE") else {
        return Err(other("home ca certificate pem is invalid"));
    };
    let cert = X509::from_der(&der).map_err(|e| other(format!("x509: {e}")))?;
    // Go `x509.ParseCertificate` rejects bytes after the certificate.
    if cert.to_der().map_err(|e| other(format!("x509: {e}")))? != der {
        return Err(other("x509: trailing data"));
    }
    Ok(Sha256::digest(&der).iter().map(|b| format!("{b:02x}")).collect())
}

fn chmod_0600(path: &Path) -> Result<()> {
    crate::private_fs::restrict(path).map_err(|e| other(e.to_string()))
}

/// Go `writeFile0600`: create or truncate, then force 0600.
fn write_0600(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = crate::private_fs::create_truncate(path).map_err(|e| other(e.to_string()))?;
    file.write_all(bytes).map_err(|e| other(e.to_string()))?;
    chmod_0600(path)
}

/// Go `loadOrCreateClientKey`: PKCS#1 or PKCS#8 RSA, else a new 2048-bit key in PKCS#1.
fn load_or_create_key(path: &Path) -> Result<PKey<Private>> {
    if is_file(path) {
        let pem = std::fs::read(path).map_err(|e| other(e.to_string()))?;
        let key = parse_rsa_key(&pem)?;
        chmod_0600(path)?;
        return Ok(key);
    }
    let rsa = Rsa::generate(2048).map_err(|e| other(e.to_string()))?;
    let pem = rsa.private_key_to_pem().map_err(|e| other(e.to_string()))?;
    write_0600(path, &pem)?;
    PKey::from_rsa(rsa).map_err(|e| other(e.to_string()))
}

/// Go `parseRSAPrivateKeyPEM`.
fn parse_rsa_key(pem: &[u8]) -> Result<PKey<Private>> {
    let Some((label, der)) = first_pem_block(pem) else {
        return Err(other("client key pem is invalid"));
    };
    match label.as_str() {
        "RSA PRIVATE KEY" => Rsa::private_key_from_der(&der)
            .and_then(PKey::from_rsa)
            .map_err(|e| other(e.to_string())),
        "PRIVATE KEY" => {
            let key = PKey::private_key_from_der(&der).map_err(|e| other(e.to_string()))?;
            if key.rsa().is_err() {
                return Err(other("client key is not rsa"));
            }
            Ok(key)
        }
        other_type => Err(other(format!("client key pem type {other_type:?} is unsupported"))),
    }
}

/// Go `createClientCSR`: subject CN is the certificate ID, SHA-256 with RSA.
pub fn create_csr(certificate_id: &str, key: &PKey<Private>) -> Result<Vec<u8>> {
    let id = certificate_id.trim();
    if id.is_empty() {
        return Err(other("certificate id is required"));
    }
    let build = || -> std::result::Result<Vec<u8>, btls::error::ErrorStack> {
        let mut name = X509NameBuilder::new()?;
        name.append_entry_by_text("CN", id)?;
        let name = name.build();
        let mut req = X509ReqBuilder::new()?;
        req.set_subject_name(&name)?;
        req.set_pubkey(key)?;
        req.sign(key, MessageDigest::sha256())?;
        req.build().to_pem()
    };
    build().map_err(|e| other(e.to_string()))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CertificateResponse {
    ok: bool,
    certificate: String,
    ca: String,
}

/// Go `requestClientCertificate`: `CERTIFICATE REQUEST <id> <secret> <csr>` on a plain
/// connection; Home answers one bulk JSON or an error.
async fn request_certificate(claims: &Claims, csr: &[u8]) -> Result<CertificateResponse> {
    let dialer = Dialer::plain(claims.ip.trim(), claims.port as u16, REQUEST_TIMEOUT);
    let exchange = async {
        let mut conn = dialer.dial().await?;
        conn.send(&[
            b"CERTIFICATE".as_slice(),
            b"REQUEST",
            claims.certificate_id.as_bytes(),
            claims.enrollment_secret.as_bytes(),
            csr,
        ])
        .await?;
        let reply = conn.recv().await?;
        conn.shutdown().await;
        Ok::<_, Error>(reply)
    };
    let reply = tokio::time::timeout(REQUEST_TIMEOUT, exchange)
        .await
        .unwrap_or(Err(Error::Timeout))?;
    let payload = match reply {
        Value::Bulk(payload) => payload,
        Value::Nil => return Err(other("home certificate request returned nil")),
        Value::Error(message) => return Err(other(message.trim().to_owned())),
        Value::Simple(_) => return Err(other("home certificate request returned unsupported resp prefix '+'")),
        Value::Int(_) => return Err(other("home certificate request returned unsupported resp prefix ':'")),
        Value::Array(_) => return Err(other("home certificate request returned unsupported resp prefix '*'")),
    };
    let response: CertificateResponse =
        serde_json::from_slice(&payload).map_err(|e| redacted_decode_error("home certificate response", &e))?;
    if !response.ok {
        return Err(other("home certificate request failed"));
    }
    Ok(response)
}
