//! The API listener (Go internal/api/server.go `Start`): plain HTTP, or HTTPS when
//! `server.tls.enable` is set, with `server.tls.cert` and `server.tls.key` loaded once at
//! startup.
//!
//! On shutdown Go closes the server at once (`Server.Stop` calls `http.Server.Close`,
//! documented "without graceful draining"), so the caller simply drops the future.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use btls::ssl::{AlpnError, Ssl, SslAcceptor, SslFiletype, SslMethod, select_next_proto};
use cpa_core::config::Config;
use tokio::net::{TcpListener, TcpStream};

/// ALPN offer in wire form, in Go's order (`NextProtos: {"h2", "http/1.1"}`); the
/// server's preference wins, as in Go's `negotiateALPN`.
const ALPN: &[u8] = b"\x02h2\x08http/1.1";

/// Go `muxSniffDeadline`: a connection must finish its handshake and send its first
/// byte within this.
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
    // Go `negotiateALPN`: a client offering ALPN without a common protocol fails the
    // handshake with no_application_protocol; one offering none gets no protocol.
    builder.set_alpn_select_callback(|_, client| select_next_proto(ALPN, client).ok_or(AlpnError::ALERT_FATAL));
    Ok(Some(Arc::new(builder.build())))
}

/// Serves `app` on `listener` with the peer address as connect info, over TLS when
/// `tls` is set. Returns only if accepting fails for good; drop the future to stop.
/// RESP connections are closed, as Go does while management is disabled; see
/// [`serve_with_resp`].
pub async fn serve(listener: TcpListener, app: Router, tls: Option<Arc<SslAcceptor>>) -> std::io::Result<()> {
    serve_with_resp(listener, app, tls, None).await
}

/// [`serve`] with Go's protocol routing (protocol_multiplexer.go `routeMuxConnection`):
/// a TLS connection that negotiated `h2` goes to the HTTP/2 server and one that
/// negotiated `http/1.1` to the HTTP/1.1 server. Any other connection, plain or TLS
/// without ALPN, is sniffed: a RESP type prefix goes to the Redis protocol
/// (`management`'s usage queue, closed without it), anything else to HTTP/1.1, which
/// never speaks h2c. A connection must finish its handshake and send its first byte
/// within Go's 10 s sniff deadline.
pub async fn serve_with_resp(
    listener: TcpListener,
    app: Router,
    tls: Option<Arc<SslAcceptor>>,
    management: Option<Arc<crate::management::Management>>,
) -> std::io::Result<()> {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(conn) => conn,
            // As axum's serve loop: connection errors are the peer's problem; anything
            // else (EMFILE) backs off.
            Err(e) if is_connection_error(&e) => continue,
            Err(e) => {
                tracing::error!("accept error: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        go_socket_defaults(&stream);
        let (app, management, tls) = (app.clone(), management.clone(), tls.clone());
        let deadline = tokio::time::Instant::now() + HANDSHAKE_DEADLINE;
        tokio::spawn(async move {
            let Some(acceptor) = tls else {
                let mut first = [0u8; 1];
                match tokio::time::timeout_at(deadline, stream.peek(&mut first)).await {
                    Ok(Ok(n)) if n > 0 => {}
                    _ => return,
                }
                if crate::resp::is_resp_prefix(first[0]) {
                    if let Some(management) = management {
                        crate::resp::serve(stream, peer, management).await;
                    }
                    return;
                }
                return serve_connection(stream, peer, app, false).await;
            };
            let Some(mut tls) = handshake(&acceptor, stream, peer, deadline).await else {
                return;
            };
            match tls.ssl().selected_alpn_protocol() {
                Some(b"h2") => return serve_connection(tls, peer, app, true).await,
                Some(b"http/1.1") => return serve_connection(tls, peer, app, false).await,
                _ => {}
            }
            let first = match tokio::time::timeout_at(deadline, tokio::io::AsyncReadExt::read_u8(&mut tls)).await {
                Ok(Ok(byte)) => byte,
                _ => return,
            };
            let io = Prefixed {
                first: Some(first),
                inner: tls,
            };
            if crate::resp::is_resp_prefix(first) {
                if let Some(management) = management {
                    crate::resp::serve(io, peer, management).await;
                }
                return;
            }
            serve_connection(io, peer, app, false).await;
        });
    }
}

/// A stream whose first byte was already read for sniffing (Go `bufferedConn`).
struct Prefixed<I> {
    first: Option<u8>,
    inner: I,
}

impl<I: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Prefixed<I> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() > 0
            && let Some(byte) = this.first.take()
        {
            buf.put_slice(&[byte]);
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<I: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Prefixed<I> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

fn is_connection_error(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::{ConnectionAborted, ConnectionRefused, ConnectionReset};
    matches!(e.kind(), ConnectionRefused | ConnectionAborted | ConnectionReset)
}

/// Go's accepted TCP connections: `TCP_NODELAY` and keep-alive probes after 15 s idle,
/// every 15 s, 9 times (net `newTCPConn`, `defaultTCPKeepAlive*`).
fn go_socket_defaults(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
    let interval = std::time::Duration::from_secs(15);
    let keepalive = socket2::TcpKeepalive::new().with_time(interval).with_interval(interval);
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
    let keepalive = keepalive.with_retries(9);
    let _ = socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive);
}

/// The TLS handshake within Go's sniff deadline; failures close the connection.
async fn handshake(
    acceptor: &SslAcceptor,
    stream: TcpStream,
    peer: SocketAddr,
    deadline: tokio::time::Instant,
) -> Option<tokio_btls::SslStream<TcpStream>> {
    let ssl = Ssl::new(acceptor.context()).ok()?;
    let mut tls = tokio_btls::SslStream::new(ssl, stream).ok()?;
    match tokio::time::timeout_at(deadline, std::pin::Pin::new(&mut tls).accept()).await {
        Ok(Ok(())) => Some(tls),
        Ok(Err(e)) => {
            tracing::debug!(%peer, "TLS handshake error: {e}");
            None
        }
        Err(_) => {
            tracing::debug!(%peer, "TLS handshake timed out");
            None
        }
    }
}

/// Serves one connection as HTTP/2 or HTTP/1.1 (with upgrades, for WebSockets). The
/// HTTP/2 server keeps Go's defaults where they are visible to clients: 250 concurrent
/// streams and no extended CONNECT (x/net http2 `defaultMaxStreams`,
/// `disableExtendedConnectProtocol`).
// ponytail: other HTTP/2 settings (window sizes, frame and header-list limits) are
// hyper's defaults, not Go's.
async fn serve_connection<I>(io: I, peer: SocketAddr, app: Router, h2: bool)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
        req.extensions_mut().insert(axum::extract::ConnectInfo(peer));
        let mut app = app.clone();
        tower_service::Service::call(&mut app, req.map(axum::body::Body::new))
    });
    let io = hyper_util::rt::TokioIo::new(io);
    let result = if h2 {
        hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
            .max_concurrent_streams(250)
            .serve_connection(io, service)
            .await
    } else {
        hyper::server::conn::http1::Builder::new()
            .serve_connection(io, service)
            .with_upgrades()
            .await
    };
    if let Err(e) = result {
        tracing::trace!(%peer, "connection error: {e}");
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

    /// The route every listener test serves: the protocol version the server saw and the
    /// peer address from connect info.
    fn app() -> Router {
        Router::new().route(
            "/peer",
            axum::routing::get(
                |axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>,
                 req: axum::extract::Request| async move { format!("{:?} {}", req.version(), peer.ip()) },
            ),
        )
    }

    /// Sends the HTTP/2 client preface and returns what came back before the connection
    /// closed or went quiet.
    async fn h2_preface<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(mut io: S) -> Vec<u8> {
        io.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n").await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), io.read_to_end(&mut buf)).await;
        buf
    }

    /// An HTTP/2 SETTINGS frame header (type 4) starts the server's h2 preface.
    fn is_h2_settings(reply: &[u8]) -> bool {
        reply.len() >= 9 && reply[3] == 4
    }

    /// HTTPS end to end with a locally generated certificate, against Go's
    /// `NextProtos: {"h2", "http/1.1"}` and `negotiateALPN`: the server's preference
    /// wins, `h2` is served as HTTP/2, `http/1.1` and no ALPN as HTTP/1.1, a client
    /// without a common protocol fails the handshake, and handlers see the peer address.
    #[tokio::test]
    async fn serves_https_h2_and_http1_by_alpn() {
        let dir = std::env::temp_dir().join(format!("cpa-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key) = self_signed(&dir);
        let acceptor = tls_acceptor(&cfg(&format!(
            "server: {{tls: {{enable: true, cert: {cert}, key: {key}}}}}\n"
        )))
        .unwrap()
        .unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        tokio::spawn(serve(tcp, app(), Some(acceptor)));

        let handshake = |alpn: Option<&'static [u8]>| async move {
            let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
            connector.set_verify(SslVerifyMode::NONE);
            if let Some(alpn) = alpn {
                connector.set_alpn_protos(alpn).unwrap();
            }
            let ssl = connector.build().configure().unwrap().into_ssl("127.0.0.1").unwrap();
            let tcp = TcpStream::connect(addr).await.unwrap();
            let mut tls = tokio_btls::SslStream::new(ssl, tcp).unwrap();
            std::pin::Pin::new(&mut tls).connect().await.map(|()| tls)
        };
        let selected = |tls: &tokio_btls::SslStream<TcpStream>| tls.ssl().selected_alpn_protocol().map(<[u8]>::to_vec);
        // Server preference, whatever the client's order.
        let both = handshake(Some(b"\x08http/1.1\x02h2")).await.unwrap();
        assert_eq!(selected(&both), Some(b"h2".to_vec()));
        // No common protocol: Go sends no_application_protocol and fails the handshake.
        assert!(handshake(Some(b"\x06spdy/3")).await.is_err());

        // A real HTTP/2 request.
        let client = wreq::Client::builder()
            .tls_cert_verification(false)
            .http2_only()
            .build()
            .unwrap();
        let res = client.get(format!("https://{addr}/peer")).send().await.unwrap();
        assert_eq!(res.version(), wreq::Version::HTTP_2);
        assert_eq!(res.text().await.unwrap(), "HTTP/2.0 127.0.0.1");

        // http/1.1, and no ALPN at all, are served as HTTP/1.1.
        for alpn in [Some(&b"\x08http/1.1"[..]), None] {
            let mut http1 = handshake(alpn).await.unwrap();
            let want = alpn.map(|_| b"http/1.1".to_vec());
            assert_eq!(selected(&http1), want);
            http1
                .write_all(b"GET /peer HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut reply = String::new();
            http1.read_to_string(&mut reply).await.unwrap();
            assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
            assert!(reply.ends_with("HTTP/1.1 127.0.0.1"), "{reply}");
        }
        // Without ALPN, Go hands the connection to the HTTP/1.1 server: no HTTP/2.
        let reply = h2_preface(handshake(None).await.unwrap()).await;
        assert!(!is_h2_settings(&reply), "{reply:?}");

        // Plain HTTP on the TLS port fails the handshake and is never served.
        let mut plain = TcpStream::connect(addr).await.unwrap();
        plain.write_all(b"GET /peer HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), plain.read_to_end(&mut buf)).await;
        assert!(!String::from_utf8_lossy(&buf).contains("200 OK"));
    }

    /// Plain HTTP is HTTP/1.1 only, as Go's server without `UnencryptedHTTP2`: no h2c by
    /// prior knowledge.
    #[tokio::test]
    async fn plain_http_is_http1_only() {
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        tokio::spawn(serve(tcp, app(), None));
        let res = wreq::Client::new()
            .get(format!("http://{addr}/peer"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.version(), wreq::Version::HTTP_11);
        assert_eq!(res.text().await.unwrap(), "HTTP/1.1 127.0.0.1");
        let reply = h2_preface(TcpStream::connect(addr).await.unwrap()).await;
        assert!(!is_h2_settings(&reply), "{reply:?}");
    }

    /// Go net `newTCPConn` on accepted connections: no delay, keep-alive 15 s / 15 s / 9.
    #[tokio::test]
    async fn accepted_sockets_get_go_defaults() {
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let _client = TcpStream::connect(tcp.local_addr().unwrap()).await.unwrap();
        let (accepted, _) = tcp.accept().await.unwrap();
        go_socket_defaults(&accepted);
        assert!(accepted.nodelay().unwrap());
        let socket = socket2::SockRef::from(&accepted);
        assert!(socket.keepalive().unwrap());
        // Windows cannot read keep-alive timings back.
        #[cfg(target_os = "linux")]
        {
            assert_eq!(socket.tcp_keepalive_time().unwrap(), std::time::Duration::from_secs(15));
            assert_eq!(
                socket.tcp_keepalive_interval().unwrap(),
                std::time::Duration::from_secs(15)
            );
            assert_eq!(socket.tcp_keepalive_retries().unwrap(), 9);
        }
    }

    /// WebSocket upgrades (the Responses and realtime sockets) work through the listener
    /// over plain HTTP and over TLS with http/1.1.
    #[tokio::test]
    async fn websocket_upgrades_over_http1() {
        let echo = Router::new().route(
            "/ws",
            axum::routing::get(|ws: axum::extract::ws::WebSocketUpgrade| async move {
                ws.on_upgrade(|mut socket| async move {
                    while let Some(Ok(message)) = socket.recv().await {
                        if socket.send(message).await.is_err() {
                            break;
                        }
                    }
                })
            }),
        );
        let dir = std::env::temp_dir().join(format!("cpa-tls-ws-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key) = self_signed(&dir);
        let acceptor = tls_acceptor(&cfg(&format!(
            "server: {{tls: {{enable: true, cert: {cert}, key: {key}}}}}\n"
        )))
        .unwrap()
        .unwrap();
        for (scheme, tls) in [("ws", None), ("wss", Some(acceptor))] {
            let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = tcp.local_addr().unwrap();
            tokio::spawn(serve(tcp, echo.clone(), tls));
            let client = wreq::Client::builder()
                .tls_cert_verification(false)
                .http1_only()
                .build()
                .unwrap();
            let mut socket = client
                .websocket(format!("{scheme}://{addr}/ws"))
                .send()
                .await
                .unwrap()
                .into_websocket()
                .await
                .unwrap();
            socket.send(wreq::ws::message::Message::text("ping")).await.unwrap();
            let reply = socket.recv().await.unwrap().unwrap().into_text().unwrap();
            assert_eq!(reply.as_str(), "ping", "{scheme}");
        }
    }
}
