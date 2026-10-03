//! Go tcp_proxy_test.go, and the goldens `zz_rsfix_live_tunnel_test.go` recorded from the
//! real Go code (tests/fixtures/codex_live_tunnel_go.json).

use std::sync::Mutex;
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;

use super::*;
use crate::realtime::dialer::{Conn, build, proxy_scheme};

fn golden() -> Value {
    serde_json::from_str(include_str!("../../tests/fixtures/codex_live_tunnel_go.json")).unwrap()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The Go-built frame for `remote:local` signed with `remote-password`.
fn valid_frame() -> Vec<u8> {
    let frames = golden()["frames"].as_array().unwrap().clone();
    let valid = frames.iter().find(|f| f["name"] == "valid").unwrap();
    unhex(valid["frame"].as_str().unwrap())
}

fn quiet() -> OnForwarding {
    Arc::new(|| {})
}

struct Never;

impl Dial for Never {
    fn dial(&self, _: SocketAddr) -> BoxFuture<'_, Result<Conn, String>> {
        Box::pin(async { Err("never".to_owned()) })
    }
}

/// Go's `recordingProxyDialer`: reports each dial and hands back the far end of a pipe,
/// or fails when `fail` is set.
struct Recording {
    dials: mpsc::UnboundedSender<(SocketAddr, Option<DuplexStream>)>,
    fail: bool,
}

impl Dial for Recording {
    fn dial(&self, target: SocketAddr) -> BoxFuture<'_, Result<Conn, String>> {
        Box::pin(async move {
            if self.fail {
                let _ = self.dials.send((target, None));
                return Err("proxy blocked".to_owned());
            }
            let (near, far) = tokio::io::duplex(4096);
            let _ = self.dials.send((target, Some(far)));
            Ok(Box::new(near) as Conn)
        })
    }
}

fn recording(
    fail: bool,
) -> (
    Arc<Recording>,
    mpsc::UnboundedReceiver<(SocketAddr, Option<DuplexStream>)>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Arc::new(Recording { dials: tx, fail }), rx)
}

/// Counts forwarding-started calls.
fn counter() -> (OnForwarding, Arc<Mutex<u32>>) {
    let count = Arc::new(Mutex::new(0));
    let seen = count.clone();
    (Arc::new(move || *seen.lock().unwrap() += 1), count)
}

const TARGET: &str = "20.42.0.20:443";

/// A tunnel for the Go tests' fixed target and credentials; aborted on drop.
struct Open(SocketAddr, AbortHandle);

impl Drop for Open {
    fn drop(&mut self) {
        self.1.abort();
    }
}

fn open_tunnel(dialer: Arc<dyn Dial>, on_forwarding: OnForwarding) -> Open {
    let (address, task) = open(
        TARGET.parse().unwrap(),
        dialer,
        "remote:local",
        "remote-password",
        on_forwarding,
    )
    .unwrap();
    Open(address, task)
}

#[tokio::test]
async fn answers_are_rewritten_like_go() {
    for case in golden()["prepare"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let (answer, offer) = (case["answer"].as_str().unwrap(), case["offer"].as_str().unwrap());
        let result = prepare_answer(answer, offer, Arc::new(Never), quiet());
        if let Some(want) = case["error"].as_str() {
            let got = result.err().unwrap_or_else(|| panic!("{name}: expected {want}"));
            // ponytail: pion's SDP lexer words its syntax errors differently; the step matches.
            match want.split_once(" for TCP proxy: sdp: ") {
                Some((step, _)) => assert!(got.starts_with(&format!("{step} for TCP proxy: ")), "{name}: {got}"),
                None => assert_eq!(got, want, "{name}"),
            }
            continue;
        }
        let prepared = result.unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut masked = prepared.sdp.clone();
        for (i, tunnel) in prepared.tunnels.iter().enumerate() {
            let at = format!("{} {} ", tunnel.listener.ip(), tunnel.listener.port());
            masked = masked.replace(&at, &format!("LISTEN{i} "));
            assert!(tunnel.candidate.contains(&at), "{name}: {}", tunnel.candidate);
        }
        // ponytail: rtc-sdp drops whitespace at the end of an attribute value; pion keeps
        // it. The answer only feeds the relay's own peer, and credentials are trimmed.
        let lines = |sdp: &str| sdp.split("\r\n").map(str::trim_end).collect::<Vec<_>>().join("\r\n");
        assert_eq!(lines(&masked), lines(case["sdp"].as_str().unwrap()), "{name}");
        let targets: Vec<String> = prepared.tunnels.iter().map(|t| t.target.to_string()).collect();
        let listeners: Vec<String> = prepared.tunnels.iter().map(|t| t.listener.ip().to_string()).collect();
        assert_eq!(serde_json::json!(targets), case["targets"], "{name}");
        assert_eq!(serde_json::json!(listeners), case["listeners"], "{name}");
        let (ufrag, password) = credentials(&parse_sdp(answer).unwrap()).unwrap();
        let (local, _) = credentials(&parse_sdp(offer).unwrap()).unwrap();
        assert_eq!(
            format!("{ufrag}:{local}"),
            case["expected_user"].as_str().unwrap(),
            "{name}"
        );
        assert_eq!(password, case["password"].as_str().unwrap(), "{name}");
        // The candidate webrtc-rs is told to dial sits in the media it came from.
        for tunnel in &prepared.tunnels {
            let media = &parse_sdp(&prepared.sdp).unwrap().media_descriptions[usize::from(tunnel.mline)];
            assert_eq!(media.attribute("mid").flatten(), tunnel.mid.as_deref(), "{name}");
        }
    }
}

/// Go's `fragmentedReader`: at most `max` bytes per read.
struct Fragmented {
    data: Vec<u8>,
    max: usize,
}

impl AsyncRead for Fragmented {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let n = self.max.min(buf.remaining()).min(self.data.len());
        let chunk: Vec<u8> = self.data.drain(..n).collect();
        buf.put_slice(&chunk);
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn binding_frames_are_validated_like_go() {
    for case in golden()["frames"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let frame = unhex(case["frame"].as_str().unwrap());
        let mut reader = Fragmented {
            data: frame.clone(),
            max: 7,
        };
        let (user, password) = (case["user"].as_str().unwrap(), case["password"].as_str().unwrap());
        let got = read_validated_frame(&mut reader, user, password).await;
        match case["error"].as_str() {
            None => assert_eq!(
                hex(&got.unwrap_or_else(|e| panic!("{name}: {e}"))),
                case["returned"].as_str().unwrap(),
                "{name}"
            ),
            Some(want) => {
                let got = got.expect_err(name);
                // The failing step matches; the detail after it is pion's or rtc's wording.
                let step = |e: &str| e.split(':').next().unwrap().split(" type ").next().unwrap().to_owned();
                assert_eq!(step(&got), step(want), "{name}: {got} vs {want}");
            }
        }
    }
}

#[test]
fn proxy_targets_match_go() {
    for case in golden()["targets"].as_array().unwrap() {
        let ip: IpAddr = case["addr"].as_str().unwrap().parse().unwrap();
        assert_eq!(Value::Bool(is_public_target(ip)), case["public"], "{ip}");
        assert_eq!(
            Value::Bool(is_public_target(ip.to_canonical())),
            case["public_unmapped"],
            "{ip}"
        );
    }
}

/// `proxyutil.BuildDialer` on Go net/url's corner cases: which settings fail and how,
/// and the host, port and userinfo bytes a proxy dialer gets.
#[test]
fn proxy_settings_match_go() {
    for case in golden()["proxies"].as_array().unwrap() {
        let raw = case["in"].as_str().unwrap();
        assert_eq!(proxy_scheme(raw), case["scheme"].as_str().unwrap(), "{raw:?}");
        match (build(raw), case["mode"].as_i64().unwrap()) {
            (Ok(None), mode) => assert_eq!(mode, if raw.trim().is_empty() { 0 } else { 1 }, "{raw:?}"),
            (Err(e), 3) => assert_eq!(e, case["error"].as_str().unwrap(), "{raw:?}"),
            (Ok(Some(dialer)), 2) => {
                let (host, port, user) = dialer.parts();
                assert_eq!(host, case["hostname"].as_str().unwrap(), "{raw:?}");
                let default = match case["scheme"].as_str().unwrap() {
                    "http" => "80",
                    "https" => "443",
                    _ => "1080",
                };
                let want_port = case["port"].as_str().unwrap();
                assert_eq!(port, if want_port.is_empty() { default } else { want_port }, "{raw:?}");
                let want_user = case.get("username").map(|name| {
                    let password = case["has_password"]
                        .as_bool()
                        .unwrap()
                        .then(|| unhex(case["password"].as_str().unwrap()));
                    (unhex(name.as_str().unwrap()), password)
                });
                assert_eq!(user.cloned(), want_user, "{raw:?}");
            }
            (got, mode) => panic!("{raw:?}: Go mode {mode}, got {:?}", got.map(|d| d.is_some())),
        }
    }
}

/// One scripted SOCKS5 exchange (`runSocks` in the Go harness); returns what the client sent.
async fn socks_server(listener: TcpListener, case: Value) -> Vec<u8> {
    let Ok((mut conn, _)) = listener.accept().await else {
        return vec![];
    };
    let mut sent = Vec::new();
    async fn read(conn: &mut TcpStream, n: usize, sent: &mut Vec<u8>) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; n];
        conn.read_exact(&mut buf).await.ok()?;
        sent.extend_from_slice(&buf);
        Some(buf)
    }
    let method = case["method"].as_u64().unwrap() as u8;
    let reply = unhex(case["reply"].as_str().unwrap());
    let run = async {
        let head = read(&mut conn, 2, &mut sent).await?;
        read(&mut conn, usize::from(head[1]), &mut sent).await?;
        conn.write_all(&[5, method]).await.ok()?;
        if method == 0xff {
            return None;
        }
        if method == 2 {
            let v = read(&mut conn, 2, &mut sent).await?;
            read(&mut conn, usize::from(v[1]), &mut sent).await?;
            let pl = read(&mut conn, 1, &mut sent).await?;
            read(&mut conn, usize::from(pl[0]), &mut sent).await?;
            let ok = case["auth_ok"].as_bool().unwrap();
            conn.write_all(&[1, u8::from(!ok)]).await.ok()?;
            if !ok {
                return None;
            }
        }
        let request = read(&mut conn, 4, &mut sent).await?;
        let address = match request[3] {
            1 => 4,
            4 => 16,
            _ => usize::from(read(&mut conn, 1, &mut sent).await?[0]),
        };
        read(&mut conn, address + 2, &mut sent).await?;
        conn.write_all(&reply).await.ok()?;
        if reply[1] == 0 {
            conn.write_all(b"hello").await.ok()?;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Some(())
    };
    let _ = run.await;
    sent
}

/// `runConnect`: the request headers, then the scripted response.
async fn connect_server(listener: TcpListener, response: String) -> Vec<u8> {
    let Ok((mut conn, _)) = listener.accept().await else {
        return vec![];
    };
    let mut sent = Vec::new();
    let mut byte = [0u8; 1];
    while !sent.ends_with(b"\r\n\r\n") && conn.read_exact(&mut byte).await.is_ok() {
        sent.push(byte[0]);
    }
    let _ = conn.write_all(response.as_bytes()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    sent
}

#[tokio::test]
async fn proxy_dialers_speak_like_go() {
    for case in golden()["dialers"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let server = if case["kind"] == "socks" {
            tokio::spawn(socks_server(listener, case.clone()))
        } else {
            tokio::spawn(connect_server(listener, case["response"].as_str().unwrap().to_owned()))
        };
        let port = host.rsplit(':').next().unwrap().to_owned();
        let url = case["url"]
            .as_str()
            .unwrap()
            .replacen("HOST", &host, 1)
            .replacen("PORT", &port, 1);
        let target: SocketAddr = case["target"].as_str().unwrap().parse().unwrap();
        let dialed = build(&url).unwrap().unwrap().dial(target).await;
        match case["error"].as_str() {
            Some(want) => assert_eq!(dialed.err().unwrap().replace(&host, "HOST"), want, "{name}"),
            None => {
                let mut conn = dialed.unwrap_or_else(|e| panic!("{name}: {e}"));
                let mut first = vec![0u8; 64];
                let n = tokio::time::timeout(Duration::from_millis(300), conn.read(&mut first))
                    .await
                    .map_or(0, |r| r.unwrap_or(0));
                assert_eq!(&first[..n], case["first_read"].as_str().unwrap().as_bytes(), "{name}");
            }
        }
        let sent = server.await.unwrap();
        let sent = if case["kind"] == "socks" {
            hex(&sent)
        } else {
            String::from_utf8(sent).unwrap()
        };
        assert_eq!(sent, case["sent"].as_str().unwrap(), "{name}");
    }
}

/// An `https` proxy: TLS to the proxy (verified, ALPN http/1.1), then `CONNECT` inside.
#[tokio::test]
async fn https_proxy_wraps_connect_in_tls() {
    use btls::asn1::Asn1Time;
    use btls::bn::BigNum;
    use btls::ec::{EcGroup, EcKey};
    use btls::hash::MessageDigest;
    use btls::nid::Nid;
    use btls::pkey::PKey;
    use btls::ssl::{AlpnError, SslAcceptor, SslMethod};
    use btls::x509::extension::SubjectAlternativeName;
    use btls::x509::{X509, X509NameBuilder};

    let key =
        PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap()).unwrap()).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_nid(Nid::COMMONNAME, "proxy.test").unwrap();
    let name = name.build();
    let mut cert = X509::builder().unwrap();
    cert.set_version(2).unwrap();
    cert.set_serial_number(&BigNum::from_u32(7).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    cert.set_subject_name(&name).unwrap();
    cert.set_issuer_name(&name).unwrap();
    cert.set_pubkey(&key).unwrap();
    cert.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
    cert.set_not_after(&Asn1Time::days_from_now(1).unwrap()).unwrap();
    let san = SubjectAlternativeName::new()
        .ip("127.0.0.1")
        .build(&cert.x509v3_context(None, None))
        .unwrap();
    cert.append_extension(&san).unwrap();
    cert.sign(&key, MessageDigest::sha256()).unwrap();
    let cert = cert.build();

    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    acceptor.set_private_key(&key).unwrap();
    acceptor.set_certificate(&cert).unwrap();
    let (alpn_tx, mut alpn_rx) = mpsc::unbounded_channel();
    acceptor.set_alpn_select_callback(move |_, offered| {
        let _ = alpn_tx.send(offered.to_vec());
        btls::ssl::select_next_proto(b"\x08http/1.1", offered).ok_or(AlpnError::NOACK)
    });
    let acceptor = acceptor.build();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        // The first client does not trust the certificate and aborts its handshake.
        let mut tls = loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let ssl = btls::ssl::Ssl::new(acceptor.context()).unwrap();
            let mut tls = tokio_btls::SslStream::new(ssl, tcp).unwrap();
            if std::pin::Pin::new(&mut tls).accept().await.is_ok() {
                break tls;
            }
        };
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            tls.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
        }
        tls.write_all(b"HTTP/1.1 200 OK\r\n\r\ninside").await.unwrap();
        tls.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        String::from_utf8(request).unwrap()
    });

    let untrusted = build(&format!("https://u:p@{host}"))
        .unwrap()
        .unwrap()
        .dial(TARGET.parse().unwrap())
        .await;
    assert!(
        untrusted
            .err()
            .unwrap()
            .starts_with("HTTPS proxy TLS handshake failed: "),
        "a proxy certificate outside the trusted roots fails"
    );
    let mut conn = build(&format!("https://u:p@{host}"))
        .unwrap()
        .unwrap()
        .trusting(cert)
        .dial(TARGET.parse().unwrap())
        .await
        .unwrap();
    let mut first = [0u8; 6];
    conn.read_exact(&mut first).await.unwrap();
    assert_eq!(&first, b"inside");
    assert_eq!(
        server.await.unwrap(),
        "CONNECT 20.42.0.20:443 HTTP/1.1\r\nHost: 20.42.0.20:443\r\nUser-Agent: Go-http-client/1.1\r\nProxy-Authorization: Basic dTpw\r\n\r\n"
    );
    let mut offered = None;
    while let Ok(alpn) = alpn_rx.try_recv() {
        offered = Some(alpn);
    }
    assert_eq!(offered.as_deref(), Some(&b"\x08http/1.1"[..]));
}

#[tokio::test]
async fn tunnel_authenticates_before_the_fixed_target_dial() {
    let (dialer, mut dials) = recording(false);
    let (on_forwarding, started) = counter();
    let tunnel = open_tunnel(dialer, on_forwarding);
    let mut client = TcpStream::connect(tunnel.0).await.unwrap();
    let frame = valid_frame();
    client.write_all(&frame).await.unwrap();
    let (target, far) = tokio::time::timeout(Duration::from_secs(1), dials.recv())
        .await
        .expect("dial after STUN authentication")
        .unwrap();
    assert_eq!(target.to_string(), TARGET);
    let mut far = far.unwrap();
    let mut forwarded = vec![0u8; frame.len()];
    far.read_exact(&mut forwarded).await.unwrap();
    assert_eq!(forwarded, frame);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        *started.lock().unwrap(),
        1,
        "forwarding started once the frame went through"
    );
    far.write_all(b"reply").await.unwrap();
    let mut reply = [0u8; 5];
    client.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"reply");
    client.write_all(b"more").await.unwrap();
    let mut more = [0u8; 4];
    far.read_exact(&mut more).await.unwrap();
    assert_eq!(&more, b"more");
    // The listener closed when the first connection claimed the tunnel.
    assert!(TcpStream::connect(tunnel.0).await.is_err());
    // Closing the tunnel closes the spliced client.
    drop(tunnel);
    let mut rest = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut rest)).await;
    assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_))), "client closed with the tunnel");
}

/// Go `blockingContextDialer`: the dial never finishes; `canceled` fires when it is dropped.
struct Blocking {
    started: mpsc::UnboundedSender<()>,
    canceled: mpsc::UnboundedSender<()>,
}

struct OnDrop(mpsc::UnboundedSender<()>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

impl Dial for Blocking {
    fn dial(&self, _: SocketAddr) -> BoxFuture<'_, Result<Conn, String>> {
        let _ = self.started.send(());
        let guard = OnDrop(self.canceled.clone());
        Box::pin(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
            Err(String::new())
        })
    }
}

#[tokio::test]
async fn closing_the_tunnel_cancels_the_proxy_dial() {
    let (started_tx, mut started) = mpsc::unbounded_channel();
    let (canceled_tx, mut canceled) = mpsc::unbounded_channel();
    let (on_forwarding, forwarded) = counter();
    let tunnel = open_tunnel(
        Arc::new(Blocking {
            started: started_tx,
            canceled: canceled_tx,
        }),
        on_forwarding,
    );
    let mut client = TcpStream::connect(tunnel.0).await.unwrap();
    client.write_all(&valid_frame()).await.unwrap();
    let started = tokio::time::timeout(Duration::from_secs(1), started.recv()).await;
    assert_eq!(started, Ok(Some(())), "dial started");
    drop(tunnel);
    let canceled = tokio::time::timeout(Duration::from_secs(1), canceled.recv()).await;
    assert_eq!(canceled, Ok(Some(())), "close cancelled the dial");
    assert_eq!(*forwarded.lock().unwrap(), 0);
}

#[tokio::test]
async fn proxy_failure_does_not_fall_back() {
    let (dialer, mut dials) = recording(true);
    let (on_forwarding, forwarded) = counter();
    let tunnel = open_tunnel(dialer, on_forwarding);
    let mut client = TcpStream::connect(tunnel.0).await.unwrap();
    client.write_all(&valid_frame()).await.unwrap();
    let (target, conn) = tokio::time::timeout(Duration::from_secs(1), dials.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((target.to_string().as_str(), conn.is_none()), (TARGET, true));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        TcpStream::connect(tunnel.0).await.is_err(),
        "candidate listener closed after the proxy failure"
    );
    let mut rest = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut rest)).await;
    assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_))), "client closed");
    assert_eq!(*forwarded.lock().unwrap(), 0);
}

/// Go `closedUpstreamDialer`: the proxied connection is already closed.
struct Closed;

impl Dial for Closed {
    fn dial(&self, _: SocketAddr) -> BoxFuture<'_, Result<Conn, String>> {
        Box::pin(async {
            let (near, far) = tokio::io::duplex(64);
            drop(far);
            Ok(Box::new(near) as Conn)
        })
    }
}

#[tokio::test]
async fn write_failure_does_not_log_forwarding_start() {
    let (on_forwarding, forwarded) = counter();
    let tunnel = open_tunnel(Arc::new(Closed), on_forwarding);
    let mut client = TcpStream::connect(tunnel.0).await.unwrap();
    client.write_all(&valid_frame()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(*forwarded.lock().unwrap(), 0);
}

#[tokio::test]
async fn unauthenticated_connections_never_dial() {
    let frames = golden()["frames"].as_array().unwrap().clone();
    let (dialer, mut dials) = recording(false);
    let (on_forwarding, forwarded) = counter();
    let tunnel = open_tunnel(dialer, on_forwarding);
    // Frames that are invalid in themselves (the wrong_* cases differ only in what the
    // validator expects).
    for name in [
        "bad_fingerprint",
        "missing_fingerprint",
        "username_after_integrity",
        "trailing",
    ] {
        let frame = frames.iter().find(|f| f["name"] == name).unwrap();
        let mut client = TcpStream::connect(tunnel.0).await.unwrap();
        client
            .write_all(&unhex(frame["frame"].as_str().unwrap()))
            .await
            .unwrap();
        let mut rest = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut rest)).await;
        assert!(
            matches!(read, Ok(Ok(0)) | Ok(Err(_))),
            "{name}: rejected connections are closed"
        );
    }
    // A frame for another session (Go: `attacker:local`).
    let mut client = TcpStream::connect(tunnel.0).await.unwrap();
    let mut frame = valid_frame();
    let at = frame.windows(12).position(|w| w == b"remote:local").unwrap();
    frame[at..at + 6].copy_from_slice(b"attack");
    client.write_all(&frame).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), dials.recv())
            .await
            .is_err(),
        "no dial for an unauthenticated connection"
    );
    assert_eq!(*forwarded.lock().unwrap(), 0);
}

#[tokio::test]
async fn at_most_four_connections_wait_for_authentication() {
    let (dialer, mut dials) = recording(false);
    let tunnel = open_tunnel(dialer, quiet());
    let mut idle = Vec::new();
    for _ in 0..4 {
        idle.push(TcpStream::connect(tunnel.0).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut excess = TcpStream::connect(tunnel.0).await.unwrap();
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(1), excess.read(&mut buf)).await;
    assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_))), "the fifth is closed at once");
    // A waiting connection that gives up frees its slot for the real one.
    drop(idle.pop());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut client = TcpStream::connect(tunnel.0).await.unwrap();
    client.write_all(&valid_frame()).await.unwrap();
    let (target, _) = tokio::time::timeout(Duration::from_secs(1), dials.recv())
        .await
        .expect("the freed slot accepted the session's connection")
        .unwrap();
    assert_eq!(target.to_string(), TARGET);
}

#[tokio::test]
async fn tunnel_targets_and_credentials_are_checked() {
    for (target, user, password, want) in [
        (
            "10.0.0.1:443",
            "remote:local",
            "pw",
            "Codex live TCP proxy target is not allowed",
        ),
        (
            "20.42.0.20:8443",
            "remote:local",
            "pw",
            "Codex live TCP proxy target is not allowed",
        ),
        (
            "20.42.0.20:443",
            " ",
            "pw",
            "Codex live TCP proxy tunnel configuration is incomplete",
        ),
        (
            "20.42.0.20:443",
            "remote:local",
            "",
            "Codex live TCP proxy tunnel configuration is incomplete",
        ),
    ] {
        let opened = open(target.parse().unwrap(), Arc::new(Never), user, password, quiet());
        assert_eq!(opened.err().as_deref(), Some(want), "{target} {user:?} {password:?}");
    }
}
