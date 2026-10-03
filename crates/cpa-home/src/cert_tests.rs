//! `-home-jwt` bootstrap against a fake enrollment endpoint and a throwaway CA, then
//! mTLS with the persisted files.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;

use crate::cert::{Paths, certificate_fingerprint, config_from_jwt_in};
use crate::client::Client;
use crate::fake::{self, FakeHome, Pki};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "cpa-home-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        Self(dir)
    }

    fn paths(&self) -> Paths {
        Paths::in_dir(self.0.join(".cli-proxy-api"))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn jwt(fingerprint: &str, port: u16) -> String {
    let payload = serde_json::json!({
        "certificate_id": " cert-1 ",
        "cluster_id": "cluster-a",
        "ca_fingerprint": fingerprint,
        "enrollment_secret": "s3cret",
        "ip": "127.0.0.1",
        "port": port,
        "iat": 1_700_000_000,
    });
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string());
    format!("header.{encoded}.signature")
}

/// `AB:CD:...`, the spelling operators paste; normalization must accept it.
fn colon_fingerprint(pem: &str) -> String {
    let hex = certificate_fingerprint(pem.as_bytes()).unwrap().to_uppercase();
    hex.as_bytes()
        .chunks(2)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect::<Vec<_>>()
        .join(":")
}

/// An enrollment endpoint that signs CSRs with `pki` and returns `ca_pem` as the CA.
async fn enrollment(pki: Arc<Pki>, ca_pem: String) -> FakeHome {
    FakeHome::start(move |args| {
        if args.len() != 5 || args[0] != "CERTIFICATE" || args[1] != "REQUEST" || args[3] != "s3cret" {
            return fake::raw("-ERR bad enrollment\r\n");
        }
        match pki.sign_csr(args[4].as_bytes()) {
            Ok((cn, certificate)) if cn == "cert-1" => {
                fake::bulk(serde_json::json!({"ok": true, "certificate": certificate, "ca": ca_pem}).to_string())
            }
            other => fake::raw(&format!("-ERR rejected {other:?}\r\n")),
        }
    })
    .await
}

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn enrollment_persists_pinned_files_once() {
    let pki = Arc::new(Pki::new("home-ca"));
    let ca_pem = pki.ca_pem();
    let home = enrollment(pki.clone(), ca_pem.clone()).await;
    let dir = TempDir::new("enroll");
    let paths = dir.paths();
    let token = jwt(&colon_fingerprint(&ca_pem), home.addr.port());

    let cfg = config_from_jwt_in(&token, &paths).await.unwrap();
    assert!(cfg.enabled && cfg.tls.enable && cfg.tls.use_target_server_name);
    assert_eq!(
        (cfg.node_id.as_str(), cfg.host.as_str(), cfg.port),
        ("cert-1", "127.0.0.1", home.addr.port())
    );
    assert_eq!(cfg.tls.ca_cert.as_deref(), Some(paths.ca_cert.as_path()));
    assert_eq!(mode(&paths.dir), 0o700);
    for file in [&paths.client_cert, &paths.client_key, &paths.ca_cert] {
        assert_eq!(mode(file), 0o600, "{}", file.display());
    }
    assert_eq!(std::fs::read_to_string(&paths.ca_cert).unwrap(), ca_pem);
    assert!(
        std::fs::read_to_string(&paths.client_key)
            .unwrap()
            .starts_with("-----BEGIN RSA PRIVATE KEY-----")
    );
    let enrolled = home.commands();
    assert_eq!(enrolled.len(), 1);
    // The raw certificate ID goes to Home; the CSR subject uses the trimmed one.
    assert_eq!(&enrolled[0][..4], ["CERTIFICATE", "REQUEST", " cert-1 ", "s3cret"]);
    assert!(enrolled[0][4].starts_with("-----BEGIN CERTIFICATE REQUEST-----"));

    // Existing files are reused and re-checked, never re-enrolled; permissions heal.
    std::fs::set_permissions(&paths.client_cert, std::fs::Permissions::from_mode(0o644)).unwrap();
    config_from_jwt_in(&token, &paths).await.unwrap();
    assert_eq!(home.commands().len(), 1);
    assert_eq!(mode(&paths.client_cert), 0o600);

    let other = Pki::new("other-ca");
    let wrong = jwt(&colon_fingerprint(&other.ca_pem()), home.addr.port());
    assert_eq!(
        config_from_jwt_in(&wrong, &paths).await.unwrap_err().to_string(),
        "home ca fingerprint mismatch"
    );
    std::fs::remove_file(&paths.ca_cert).unwrap();
    assert_eq!(
        config_from_jwt_in(&token, &paths).await.unwrap_err().to_string(),
        "home ca certificate file is missing"
    );
}

#[tokio::test]
async fn enrollment_refuses_an_unpinned_ca() {
    let pki = Arc::new(Pki::new("home-ca"));
    let impostor = Pki::new("impostor-ca");
    let home = enrollment(pki.clone(), impostor.ca_pem()).await;
    let dir = TempDir::new("pin");
    let paths = dir.paths();
    let token = jwt(&colon_fingerprint(&pki.ca_pem()), home.addr.port());
    assert_eq!(
        config_from_jwt_in(&token, &paths).await.unwrap_err().to_string(),
        "home ca fingerprint mismatch"
    );
    assert!(!paths.client_cert.exists() && !paths.ca_cert.exists());
    // The key survives for the next attempt, as in Go.
    assert!(paths.client_key.exists());
}

#[tokio::test]
async fn enrollment_failures_report_home_errors() {
    for (reply, want) in [
        ("-ERR enrollment secret rejected\r\n", "ERR enrollment secret rejected"),
        ("$-1\r\n", "home certificate request returned nil"),
        (
            "+OK\r\n",
            "home certificate request returned unsupported resp prefix '+'",
        ),
    ] {
        let home = FakeHome::start(move |_| fake::raw(reply)).await;
        let dir = TempDir::new("errors");
        let token = jwt("ab", home.addr.port());
        assert_eq!(
            config_from_jwt_in(&token, &dir.paths()).await.unwrap_err().to_string(),
            want
        );
    }
    for (body, want) in [
        (r#"{"ok":false}"#, "home certificate request failed"),
        (
            r#"{"ok":true,"certificate":"x","ca":" "}"#,
            "home certificate response is incomplete",
        ),
    ] {
        let home = FakeHome::start(move |_| fake::bulk(body)).await;
        let dir = TempDir::new("errors");
        let token = jwt("ab", home.addr.port());
        assert_eq!(
            config_from_jwt_in(&token, &dir.paths()).await.unwrap_err().to_string(),
            want
        );
    }
}

#[tokio::test]
async fn an_existing_pkcs8_key_is_reused_and_non_rsa_keys_are_refused() {
    let pki = Arc::new(Pki::new("home-ca"));
    let ca_pem = pki.ca_pem();
    let home = enrollment(pki.clone(), ca_pem.clone()).await;
    let dir = TempDir::new("pkcs8");
    let paths = dir.paths();
    std::fs::create_dir_all(&paths.dir).unwrap();
    let rsa = btls::rsa::Rsa::generate(2048).unwrap();
    let key = btls::pkey::PKey::from_rsa(rsa).unwrap();
    std::fs::write(&paths.client_key, key.private_key_to_pem_pkcs8().unwrap()).unwrap();
    config_from_jwt_in(&jwt(&colon_fingerprint(&ca_pem), home.addr.port()), &paths)
        .await
        .unwrap();
    let cert = btls::x509::X509::from_pem(&std::fs::read(&paths.client_cert).unwrap()).unwrap();
    assert!(cert.public_key().unwrap().public_eq(&key));
    assert_eq!(mode(&paths.client_key), 0o600);

    let ec_dir = TempDir::new("ec");
    let ec_paths = ec_dir.paths();
    std::fs::create_dir_all(&ec_paths.dir).unwrap();
    let group = btls::ec::EcGroup::from_curve_name(btls::nid::Nid::X9_62_PRIME256V1).unwrap();
    let ec = btls::pkey::PKey::from_ec_key(btls::ec::EcKey::generate(&group).unwrap()).unwrap();
    std::fs::write(&ec_paths.client_key, ec.private_key_to_pem_pkcs8().unwrap()).unwrap();
    let error = config_from_jwt_in(&jwt("ab", home.addr.port()), &ec_paths)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "client key is not rsa");
}

#[tokio::test]
async fn bootstrapped_files_authenticate_over_mtls_and_pin_the_server() {
    let pki = Arc::new(Pki::new("home-ca"));
    let ca_pem = pki.ca_pem();
    let enroll = enrollment(pki.clone(), ca_pem.clone()).await;
    let dir = TempDir::new("mtls");
    let cfg = config_from_jwt_in(&jwt(&colon_fingerprint(&ca_pem), enroll.addr.port()), &dir.paths())
        .await
        .unwrap();

    let home = FakeHome::start_tls(pki.acceptor(), |args| match args[0].as_str() {
        "get" => fake::bulk("port: 8317\n"),
        _ => fake::raw("+PONG\r\n"),
    })
    .await;
    let mut tls_cfg = cfg.clone();
    tls_cfg.port = home.addr.port();
    tls_cfg.disable_cluster_discovery = true;
    let client = Client::with_options(tls_cfg.clone(), Duration::from_secs(2), "x".into());
    assert_eq!(client.get_config().await.unwrap(), b"port: 8317\n");

    // A server whose certificate does not chain to the pinned CA is refused.
    let impostor = Pki::new("impostor-ca");
    let fake_home = FakeHome::start_tls(impostor.acceptor(), |_| fake::bulk("stolen")).await;
    let mut wrong = tls_cfg.clone();
    wrong.port = fake_home.addr.port();
    let error = Client::with_options(wrong, Duration::from_secs(2), "x".into())
        .get_config()
        .await
        .unwrap_err();
    assert!(error.to_string().contains("handshake"), "{error}");
    assert!(fake_home.commands().is_empty());

    // Without the client certificate Home refuses the connection.
    let mut anonymous = tls_cfg.clone();
    anonymous.tls.client_cert = None;
    anonymous.tls.client_key = None;
    let result = Client::with_options(anonymous, Duration::from_secs(2), "x".into())
        .get_config()
        .await;
    assert!(result.is_err());
    // Only the authenticated GET reached the real fake.
    assert_eq!(home.count("get"), 1);
}

#[tokio::test]
async fn dns_names_match_sans_only_never_the_subject_cn() {
    let pki = Arc::new(Pki::new("home-ca"));
    let ca_pem = pki.ca_pem();
    let enroll = enrollment(pki.clone(), ca_pem.clone()).await;
    let dir = TempDir::new("cn");
    let cfg = config_from_jwt_in(&jwt(&colon_fingerprint(&ca_pem), enroll.addr.port()), &dir.paths())
        .await
        .unwrap();
    let get = |home: &FakeHome| {
        let mut cfg = cfg.clone();
        cfg.port = home.addr.port();
        cfg.disable_cluster_discovery = true;
        cfg.tls.server_name = "localhost".into();
        Client::with_options(cfg, Duration::from_secs(2), "x".into())
    };
    let handler = |_: &[String]| fake::bulk("ok");
    // Go's VerifyHostname ignores the CN: a CN-only certificate must not verify.
    let cn_only = FakeHome::start_tls(pki.named_acceptor("localhost", None), handler).await;
    let error = get(&cn_only).get_config().await.unwrap_err();
    assert!(error.to_string().contains("handshake"), "{error}");
    let with_san = FakeHome::start_tls(pki.named_acceptor("other", Some("localhost")), handler).await;
    assert_eq!(get(&with_san).get_config().await.unwrap(), b"ok");
}

#[test]
fn tls_config_errors_match_go() {
    let mut cfg = crate::config::HomeTlsConfig {
        enable: true,
        client_cert: Some("/nonexistent/cert.pem".into()),
        ..Default::default()
    };
    let error = crate::tls::Dialer::new(&cfg, "127.0.0.1", 1, "127.0.0.1", Duration::from_secs(1))
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        "home tls: client certificate and key must be set together"
    );
    cfg.client_cert = None;
    cfg.ca_cert = Some("/nonexistent/ca.pem".into());
    let error = crate::tls::Dialer::new(&cfg, "127.0.0.1", 1, "127.0.0.1", Duration::from_secs(1))
        .err()
        .unwrap();
    assert!(error.to_string().starts_with("home tls: read ca-cert: "), "{error}");
    let dir = TempDir::new("tlscfg");
    std::fs::create_dir_all(&dir.0).unwrap();
    let empty = dir.0.join("empty.pem");
    std::fs::write(&empty, "not a certificate").unwrap();
    cfg.ca_cert = Some(empty);
    let error = crate::tls::Dialer::new(&cfg, "127.0.0.1", 1, "127.0.0.1", Duration::from_secs(1))
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "home tls: ca-cert contains no PEM certificates");
}

#[test]
fn fingerprint_requires_a_leading_certificate_block() {
    let pki = Pki::new("ca");
    let pem = pki.ca_pem();
    let hex = certificate_fingerprint(pem.as_bytes()).unwrap();
    assert_eq!(hex.len(), 64);
    // Leading text before the block is skipped, like pem.Decode.
    assert_eq!(certificate_fingerprint(format!("junk\n{pem}").as_bytes()).unwrap(), hex);
    let key_first = format!("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n{pem}");
    assert_eq!(
        certificate_fingerprint(key_first.as_bytes()).unwrap_err().to_string(),
        "home ca certificate pem is invalid"
    );
}
