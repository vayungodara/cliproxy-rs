//! The API listener (Go internal/api/server.go `Start`): plain HTTP, or HTTPS when
//! `server.tls.enable` is set, with `server.tls.cert` and `server.tls.key` loaded once at
//! startup.
//!
//! On shutdown Go closes the server at once (`Server.Stop` calls `http.Server.Close`,
//! documented "without graceful draining"), so the caller simply drops the future.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::serve::ListenerExt;
use btls::ssl::{AlpnError, Ssl, SslAcceptor, SslFiletype, SslMethod, select_next_proto};
use cpa_core::config::Config;
use tokio::net::{TcpListener, TcpStream};

/// ALPN offer in wire form.
// ponytail: HTTP/1.1 only. Go also offers `h2` (`NextProtos: {"h2", "http/1.1"}`), but
// serving it needs axum's `http2` feature and the `h2` crate, which the build does not
// carry yet; clients that offer both fall back to HTTP/1.1.
const ALPN: &[u8] = b"\x08http/1.1";

/// Go `muxSniffDeadline`: a connection must finish its handshake within this.
const HANDSHAKE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// The configured TLS settings (`server.tls`, legacy `tls`).
fn tls_settings(cfg: &Config) -> (bool, String, String) {
    let tls = cfg.document.get("server").and_then(|s| s.get("tls"));
    let get = |k: &str| tls.and_then(|t| t.get(k));
    let text = |k: &str| {
        get(k)
            .and_then(serde_yaml_ng::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let enable = get("enable").and_then(serde_yaml_ng::Value::as_bool).unwrap_or(false);
    (enable, text("cert"), text("key"))
}

/// Go's TLS setup in `Server.Start`: `None` when `server.tls.enable` is off; an error,
/// worded as Go's, when the certificate or key path is empty or the pair does not load.
// ponytail: load failures carry BoringSSL's reason, not Go's `tls.LoadX509KeyPair`
// text; the "failed to start HTTPS server: " prefix and the startup abort match.
pub fn tls_acceptor(cfg: &Config) -> anyhow::Result<Option<Arc<SslAcceptor>>> {
    let (enable, cert, key) = tls_settings(cfg);
    if !enable {
        return Ok(None);
    }
    if cert.is_empty() || key.is_empty() {
        anyhow::bail!("failed to start HTTPS server: tls.cert or tls.key is empty");
    }
    let fail = |e: &dyn std::fmt::Display| anyhow::anyhow!("failed to start HTTPS server: {e}");
    let read = |path: &str| std::fs::read(path).map_err(|e| fail(&format!("open {path}: {e}")));
    // Read first so a missing file reports the path, as Go's os.ReadFile error does.
    read(&cert)?;
    read(&key)?;
    let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).map_err(|e| fail(&e))?;
    builder.set_certificate_chain_file(&cert).map_err(|e| fail(&e))?;
    builder
        .set_private_key_file(&key, SslFiletype::PEM)
        .map_err(|e| fail(&e))?;
    builder
        .check_private_key()
        .map_err(|_| fail(&"tls: private key does not match public key"))?;
    builder.set_alpn_select_callback(|_, client| select_next_proto(ALPN, client).ok_or(AlpnError::NOACK));
    Ok(Some(Arc::new(builder.build())))
}

/// TLS connections whose handshake finished, handed to axum in accept order. Each
/// handshake runs in its own task so a slow client never stalls accepting.
struct TlsListener {
    ready: tokio::sync::mpsc::Receiver<(tokio_btls::SslStream<TcpStream>, SocketAddr)>,
    local: SocketAddr,
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_btls::SslStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(conn) => conn,
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.local)
    }
}

fn tls_listener(tcp: TcpListener, acceptor: Arc<SslAcceptor>) -> std::io::Result<TlsListener> {
    let local = tcp.local_addr()?;
    let (tx, ready) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        loop {
            if tx.is_closed() {
                return;
            }
            let (stream, peer) = match tcp.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    // As axum's TCP listener: log and back off on accept errors.
                    tracing::debug!("accept error: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            };
            let (acceptor, tx) = (acceptor.clone(), tx.clone());
            tokio::spawn(async move {
                let Ok(ssl) = Ssl::new(acceptor.context()) else { return };
                let Ok(mut tls) = tokio_btls::SslStream::new(ssl, stream) else {
                    return;
                };
                match tokio::time::timeout(HANDSHAKE_DEADLINE, std::pin::Pin::new(&mut tls).accept()).await {
                    Ok(Ok(())) => {
                        let _ = tx.send((tls, peer)).await;
                    }
                    Ok(Err(e)) => tracing::debug!(%peer, "TLS handshake error: {e}"),
                    Err(_) => tracing::debug!(%peer, "TLS handshake timed out"),
                }
            });
        }
    });
    Ok(TlsListener { ready, local })
}

/// Serves `app` on `listener` with the peer address as connect info, over TLS when
/// `tls` is set. Returns when the server fails; drop the future to stop.
pub async fn serve(listener: TcpListener, app: Router, tls: Option<Arc<SslAcceptor>>) -> std::io::Result<()> {
    let service = app.into_make_service_with_connect_info::<SocketAddr>();
    match tls {
        None => axum::serve(listener, service).await,
        Some(acceptor) => axum::serve(tls_listener(listener, acceptor)?.tap_io(|_| {}), service).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use btls::asn1::Asn1Time;
    use btls::bn::BigNum;
    use btls::ec::{EcGroup, EcKey};
    use btls::hash::MessageDigest;
    use btls::nid::Nid;
    use btls::pkey::PKey;
    use btls::ssl::{SslConnector, SslVerifyMode};
    use btls::x509::extension::SubjectAlternativeName;
    use btls::x509::{X509, X509NameBuilder};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn cfg(yaml: &str) -> Config {
        Config::parse(yaml).unwrap()
    }

    fn error(yaml: &str) -> String {
        match tls_acceptor(&cfg(yaml)) {
            Ok(_) => panic!("expected an error for {yaml}"),
            Err(e) => e.to_string(),
        }
    }

    /// A throwaway self-signed P-256 certificate for 127.0.0.1, written as PEM files.
    fn self_signed(dir: &std::path::Path) -> (String, String) {
        let key =
            PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap()).unwrap())
                .unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_nid(Nid::COMMONNAME, "127.0.0.1").unwrap();
        let name = name.build();
        let mut x509 = X509::builder().unwrap();
        x509.set_version(2).unwrap();
        x509.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
            .unwrap();
        x509.set_subject_name(&name).unwrap();
        x509.set_issuer_name(&name).unwrap();
        x509.set_pubkey(&key).unwrap();
        x509.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
        x509.set_not_after(&Asn1Time::days_from_now(1).unwrap()).unwrap();
        let san = SubjectAlternativeName::new()
            .ip("127.0.0.1")
            .build(&x509.x509v3_context(None, None))
            .unwrap();
        x509.append_extension(&san).unwrap();
        x509.sign(&key, MessageDigest::sha256()).unwrap();
        let (cert, key_path) = (dir.join("cert.pem"), dir.join("key.pem"));
        std::fs::write(&cert, x509.build().to_pem().unwrap()).unwrap();
        std::fs::write(&key_path, key.private_key_to_pem_pkcs8().unwrap()).unwrap();
        (cert.display().to_string(), key_path.display().to_string())
    }

    #[test]
    fn tls_settings_follow_go_start() {
        assert!(tls_acceptor(&cfg("{}\n")).unwrap().is_none());
        assert!(
            tls_acceptor(&cfg("server: {tls: {enable: false, cert: x, key: y}}\n"))
                .unwrap()
                .is_none()
        );
        let err = error("server: {tls: {enable: true, cert: ' ', key: k.pem}}\n");
        assert_eq!(err, "failed to start HTTPS server: tls.cert or tls.key is empty");
        // Legacy top-level `tls` maps to server.tls.
        let err = error("tls: {enable: true, cert: /nonexistent/c.pem, key: /nonexistent/k.pem}\n");
        assert!(
            err.starts_with("failed to start HTTPS server: open /nonexistent/c.pem"),
            "{err}"
        );
        let dir = std::env::temp_dir().join(format!("cpa-tls-mismatch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, _) = self_signed(&dir);
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let (_, key) = self_signed(&other);
        let err = error(&format!(
            "server: {{tls: {{enable: true, cert: {cert}, key: {key}}}}}\n"
        ));
        assert!(err.starts_with("failed to start HTTPS server: "), "{err}");
    }

    /// HTTPS end to end with a locally generated certificate: ALPN settles on HTTP/1.1
    /// even when h2 is offered, requests are served, and handlers see the peer address.
    #[tokio::test]
    async fn serves_https_with_alpn_and_connect_info() {
        let dir = std::env::temp_dir().join(format!("cpa-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key) = self_signed(&dir);
        let acceptor = tls_acceptor(&cfg(&format!(
            "server: {{tls: {{enable: true, cert: {cert}, key: {key}}}}}\n"
        )))
        .unwrap()
        .unwrap();
        let app = Router::new().route(
            "/peer",
            axum::routing::get(
                |axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>| async move {
                    peer.ip().to_string()
                },
            ),
        );
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        tokio::spawn(serve(tcp, app, Some(acceptor)));

        let handshake = |alpn: &'static [u8]| async move {
            let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
            connector.set_verify(SslVerifyMode::NONE);
            connector.set_alpn_protos(alpn).unwrap();
            let ssl = connector.build().configure().unwrap().into_ssl("127.0.0.1").unwrap();
            let tcp = TcpStream::connect(addr).await.unwrap();
            let mut tls = tokio_btls::SslStream::new(ssl, tcp).unwrap();
            std::pin::Pin::new(&mut tls).connect().await.unwrap();
            tls
        };
        let both = handshake(b"\x02h2\x08http/1.1").await;
        assert_eq!(both.ssl().selected_alpn_protocol(), Some(&b"http/1.1"[..]));

        let mut http1 = handshake(b"\x08http/1.1").await;
        assert_eq!(http1.ssl().selected_alpn_protocol(), Some(&b"http/1.1"[..]));
        http1
            .write_all(b"GET /peer HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut reply = String::new();
        http1.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
        assert!(reply.ends_with("127.0.0.1"), "{reply}");

        // Plain HTTP on the TLS port fails the handshake and is never served.
        let mut plain = TcpStream::connect(addr).await.unwrap();
        plain.write_all(b"GET /peer HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), plain.read_to_end(&mut buf)).await;
        assert!(!String::from_utf8_lossy(&buf).contains("200 OK"));
    }
}
