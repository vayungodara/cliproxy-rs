//! The chatgpt.com ClientHello against Go's: `tests/fixtures/codex_chrome_hello.json`
//! was captured from Go's own `newUtlsRoundTripper` (uTLS v1.8.2 `HelloChrome_Auto`) by
//! `tests/reference/codex/zz_rsfix_chrome_hello_test.go`. Both sides randomize GREASE,
//! extension order and the ECH GREASE payload, so the comparison uses the same
//! normalization as the generator.

use super::*;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use tokio::io::AsyncReadExt;

fn grease(v: u16) -> bool {
    v & 0x0f0f == 0x0a0a && v >> 8 == v & 0xff
}

fn u16_name(v: u16) -> String {
    if grease(v) { "GREASE".into() } else { format!("{v:04x}") }
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

fn be(data: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([data[at], data[at + 1]])
}

#[derive(Debug, PartialEq)]
struct Hello {
    ciphers: Vec<String>,
    /// In wire order.
    extensions: Vec<String>,
    contents: BTreeMap<String, String>,
    key_shares: Vec<String>,
    ech_payload: usize,
}

/// `rsfixParseHello` in the Go generator.
fn parse(record: &[u8]) -> Hello {
    let mut p = 4 + 2 + 32;
    p += 1 + record[p] as usize;
    let n = be(record, p) as usize;
    p += 2;
    let ciphers = (0..n).step_by(2).map(|i| u16_name(be(record, p + i))).collect();
    p += n;
    p += 1 + record[p] as usize;
    let end = p + 2 + be(record, p) as usize;
    p += 2;
    let (mut extensions, mut contents, mut key_shares, mut ech_payload) = (Vec::new(), BTreeMap::new(), Vec::new(), 0);
    while p < end {
        let kind = be(record, p);
        let size = be(record, p + 2) as usize;
        let data = &record[p + 4..p + 4 + size];
        p += 4 + size;
        let name = u16_name(kind);
        extensions.push(name.clone());
        match kind {
            k if grease(k) => {}
            0x0033 => {
                let mut q = 2;
                while q < data.len() {
                    let len = be(data, q + 2) as usize;
                    key_shares.push(format!("{}:{len}", u16_name(be(data, q))));
                    q += 4 + len;
                }
            }
            0xfe0d => {
                contents.insert(name, hex(&data[..5]));
                let enc = be(data, 6) as usize;
                ech_payload = be(data, 8 + enc) as usize;
            }
            0x000a | 0x002b => {
                let start = if kind == 0x002b { 1 } else { 2 };
                let mut norm = hex(&data[..start]);
                for q in (start..data.len().saturating_sub(1)).step_by(2) {
                    norm.push(',');
                    norm.push_str(&u16_name(be(data, q)));
                }
                contents.insert(name, norm);
            }
            _ => {
                contents.insert(name, hex(data));
            }
        }
    }
    Hello {
        ciphers,
        extensions,
        contents,
        key_shares,
        ech_payload,
    }
}

/// The production Chrome client's first record, sent to a local listener that closes.
async fn capture() -> Vec<u8> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let capture = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut header = [0; 5];
        socket.read_exact(&mut header).await.unwrap();
        assert_eq!(header[0], 22, "TLS handshake record");
        let mut record = vec![0; u16::from_be_bytes([header[3], header[4]]) as usize];
        socket.read_exact(&mut record).await.unwrap();
        record
    });
    let transport = Transport::new(Hooks {
        trust: None,
        resolve: vec![("chatgpt.com".into(), address)],
    });
    let url = format!("https://chatgpt.com:{}/backend-api/codex/responses", address.port());
    // The default (keep-alive off), as Go dials it.
    let client = transport.for_url(&url, &Proxy::Inherit, false);
    assert!(client.post(&url).send().await.is_err(), "the listener never answers");
    capture.await.unwrap()
}

#[tokio::test]
async fn chatgpt_clienthello_matches_go_chrome_profile() {
    let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/codex_chrome_hello.json")).unwrap();
    let go = &fixture["hellos"][0];
    let strings = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_owned())
            .collect()
    };
    let go_contents: BTreeMap<String, String> = go["contents"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
        .collect();
    let go_payloads: BTreeSet<u64> = fixture["hellos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["ech_payload_len"].as_u64().unwrap())
        .collect();
    let mut orders = BTreeSet::new();
    for _ in 0..12 {
        let hello = parse(&capture().await);
        assert_eq!(hello.ciphers, strings(&go["ciphers"]), "cipher suites");
        let mut sorted = hello.extensions.clone();
        sorted.sort();
        assert_eq!(sorted, strings(&go["extensions"]), "extension set");
        assert_eq!(hello.contents, go_contents, "extension contents");
        assert_eq!(hello.key_shares, strings(&go["key_shares"]), "key shares");
        assert!(
            [144, 176, 208, 240].contains(&hello.ech_payload),
            "ECH GREASE payload {} (Go saw {go_payloads:?})",
            hello.ech_payload
        );
        orders.insert(hello.extensions);
    }
    assert!(
        orders.len() > 1,
        "extensions are permuted per connection, as Chrome does"
    );
}

/// `TestFallbackRoundTripperRoutesProtectedHosts`, Codex rows.
#[test]
fn only_https_chatgpt_uses_the_chrome_profile() {
    for (url, chrome) in [
        ("https://chatgpt.com/backend-api/codex/responses", true),
        ("https://ChatGPT.com:8443/backend-api/codex/responses", true),
        ("https://caller@chatgpt.com/backend-api/codex", true),
        ("http://chatgpt.com/backend-api/codex/responses", false),
        ("https://chatgpt.com.example/backend-api", false),
        ("https://api.openai.com/v1/responses", false),
        ("http://127.0.0.1:8080/responses", false),
    ] {
        assert_eq!(is_chatgpt(url), chrome, "{url}");
    }
}

/// How the local chatgpt.com treats each connection.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Answer every request and keep the connection open.
    KeepAlive,
    /// Close the connection shortly after its first answer, leaving the client a stale
    /// pooled one.
    CloseAfterFirst,
    /// HTTP/2 only: answer a connection's first stream at once, then hold every later
    /// stream until another stream arrives on the same connection (at most [`PAIR_WAIT`])
    /// and answer both, counting the pair. Only a client that has two requests in flight
    /// on one connection forms a pair; one that sends them one at a time never does.
    Pair,
}

const PAIR_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// A local chatgpt.com that counts the TCP connections it accepts and the ones the client
/// closed. Every request gets `200 ok` after `delay`, over HTTP/2 when ALPN picks it and
/// HTTP/1.1 with keep-alive otherwise, as `mode` says.
struct Upstream {
    addr: std::net::SocketAddr,
    ca: Vec<u8>,
    accepted: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    closed_by_client: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// HTTP/2 streams answered together with another on one connection ([`Mode::Pair`]).
    pairs: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Upstream {
    async fn start(alpn: &'static [u8], delay: std::time::Duration, mode: Mode) -> Self {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let (ca, acceptor) = crate::test_tls::acceptor(&["chatgpt.com".to_owned()], alpn);
        let acceptor = Arc::new(acceptor);
        let (accepted, closed) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let pairs = Arc::new(AtomicUsize::new(0));
        let (count, gone, paired) = (accepted.clone(), closed.clone(), pairs.clone());
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                count.fetch_add(1, SeqCst);
                let (acceptor, gone, paired) = (acceptor.clone(), gone.clone(), paired.clone());
                tokio::spawn(async move {
                    let ssl = btls::ssl::Ssl::new(acceptor.context()).unwrap();
                    let mut tls = tokio_btls::SslStream::new(ssl, tcp).unwrap();
                    if std::pin::Pin::new(&mut tls).accept().await.is_err() {
                        return;
                    }
                    let by_client = if tls.ssl().selected_alpn_protocol() == Some(b"h2") {
                        serve_h2(tls, delay, mode, &paired).await
                    } else {
                        serve_h1(tls, delay, mode == Mode::CloseAfterFirst).await
                    };
                    if by_client {
                        gone.fetch_add(1, SeqCst);
                    }
                });
            }
        });
        Self {
            addr,
            ca,
            accepted,
            closed_by_client: closed,
            pairs,
        }
    }

    fn transport(&self) -> Transport {
        Transport::new(Hooks {
            trust: Some(wreq::tls::trust::CertStore::from_pem_stack(self.ca.clone()).unwrap()),
            resolve: vec![("chatgpt.com".into(), self.addr)],
        })
    }

    fn url(&self) -> String {
        format!("https://chatgpt.com:{}/backend-api/codex/responses", self.addr.port())
    }

    fn accepted(&self) -> usize {
        self.accepted.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn pairs(&self) -> usize {
        self.pairs.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn closed_by_client(&self) -> usize {
        self.closed_by_client.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Returns whether the client closed the connection (false when the server did).
async fn serve_h2<S>(io: S, delay: std::time::Duration, mode: Mode, pairs: &std::sync::atomic::AtomicUsize) -> bool
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    type Respond = http2::server::SendResponse<bytes::Bytes>;
    let answer = |mut respond: Respond| {
        let response = http::Response::builder().status(200).body(()).unwrap();
        let mut body = respond.send_response(response, false).unwrap();
        body.send_data(bytes::Bytes::from_static(b"ok"), true).unwrap();
    };
    let Ok(mut conn) = http2::server::handshake(io).await else {
        return true;
    };
    let mut first = true;
    while let Some(Ok((_, respond))) = conn.accept().await {
        tokio::time::sleep(delay).await;
        if mode == Mode::Pair && !first {
            // Hold this answer until a second stream shares the connection. Accepting keeps
            // driving the connection, so the client's frames still arrive meanwhile.
            match tokio::time::timeout(PAIR_WAIT, conn.accept()).await {
                Ok(Some(Ok((_, other)))) => {
                    pairs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    answer(respond);
                    answer(other);
                }
                Ok(_) => {
                    answer(respond);
                    return true;
                }
                Err(_) => answer(respond),
            }
            continue;
        }
        first = false;
        answer(respond);
        if mode == Mode::CloseAfterFirst {
            // Let the answer go out, then drop the connection without GOAWAY.
            let _ = tokio::time::timeout(std::time::Duration::from_millis(50), conn.accept()).await;
            return false;
        }
    }
    true
}

/// Returns whether the client closed the connection (false when the server did).
async fn serve_h1<S>(mut io: S, delay: std::time::Duration, close_after_first: bool) -> bool
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    let mut buf = Vec::new();
    loop {
        let head_end = loop {
            if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            let mut chunk = [0u8; 4096];
            match io.read(&mut chunk).await {
                Ok(0) | Err(_) => return true,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        while buf.len() < head_end + length {
            let mut chunk = [0u8; 4096];
            match io.read(&mut chunk).await {
                Ok(0) | Err(_) => return true,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        buf.drain(..head_end + length);
        tokio::time::sleep(delay).await;
        let reply = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok";
        if io.write_all(reply).await.is_err() {
            return true;
        }
        if close_after_first {
            // A keep-alive answer, then the server goes away while the client pools it.
            let _ = io.flush().await;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            return false;
        }
    }
}

/// One request through the chatgpt.com client with `chatgpt-keep-alive` on.
async fn get_ok(transport: &Transport, url: &str, proxy: &Proxy) {
    get_ok_with(transport, url, proxy, true).await;
}

async fn get_ok_with(transport: &Transport, url: &str, proxy: &Proxy, keep_alive: bool) {
    let response = transport.for_url(url, proxy, keep_alive).get(url).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap(), "ok");
}

const H2: &[u8] = b"\x02h2\x08http/1.1";
const H1: &[u8] = b"\x08http/1.1";

/// The default, `chatgpt-keep-alive` off, is Go's behaviour: every request dials its own
/// connection and the client closes it after the response, so none is ever kept idle
/// (and no idle timer can run), over HTTP/2 and HTTP/1.1.
#[tokio::test]
async fn chatgpt_client_keeps_no_idle_connection_by_default() {
    for alpn in [H2, H1] {
        let up = Upstream::start(alpn, std::time::Duration::ZERO, Mode::KeepAlive).await;
        let (transport, url) = (up.transport(), up.url());
        for _ in 0..3 {
            get_ok_with(&transport, &url, &Proxy::Inherit, false).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(up.accepted(), 3, "a connection per request, ALPN {alpn:?}");
        assert_eq!(up.closed_by_client(), 3, "none left idle, ALPN {alpn:?}");
    }
}

/// With the switch on, sequential requests share one connection per proxy; switching it
/// off takes the per-request client from the next request and retires every pooled
/// client, so the connections they kept close and no pool (or pool timer) is left.
#[tokio::test]
async fn chatgpt_keep_alive_switch_applies_per_request() {
    use std::sync::atomic::Ordering::SeqCst;
    let up = Upstream::start(H2, std::time::Duration::ZERO, Mode::KeepAlive).await;
    let (transport, url) = (up.transport(), up.url());
    let ((a, a_tunnels), (b, b_tunnels)) = (counting_proxy(up.addr).await, counting_proxy(up.addr).await);
    let pooled = || {
        let cache = transport.chrome.lock().unwrap();
        cache.iter().filter(|((_, keep_alive), _)| *keep_alive).count()
    };
    for proxy in [&a, &b, &a, &b] {
        get_ok_with(&transport, &url, proxy, true).await;
    }
    assert_eq!((a_tunnels.load(SeqCst), b_tunnels.load(SeqCst)), (1, 1));
    assert_eq!(
        (up.accepted(), pooled()),
        (2, 2),
        "one kept connection and client per proxy"
    );
    get_ok_with(&transport, &url, &Proxy::Direct, false).await;
    get_ok_with(&transport, &url, &Proxy::Direct, false).await;
    assert_eq!(up.accepted(), 4, "off: a new connection per request");
    assert_eq!(pooled(), 0, "pooled clients retired");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        up.closed_by_client(),
        4,
        "the two kept connections closed with their pools"
    );
}

/// Go dials a dedicated uTLS connection for every chatgpt.com request. With
/// `chatgpt-keep-alive` on, the Chrome client keeps its connections instead
/// (docs/DIFFERENCES-FROM-GO.md): sequential requests share one connection over HTTP/2
/// and HTTP/1.1.
#[tokio::test]
async fn chatgpt_client_reuses_connections() {
    for alpn in [H2, H1] {
        let up = Upstream::start(alpn, std::time::Duration::ZERO, Mode::KeepAlive).await;
        let (transport, url) = (up.transport(), up.url());
        for _ in 0..3 {
            get_ok(&transport, &url, &Proxy::Inherit).await;
        }
        assert_eq!(up.accepted(), 1, "sequential requests, ALPN {alpn:?}");
    }
}

/// Concurrent HTTP/2 requests share one connection at the same time: the mock holds the
/// second stream's answer until a third stream arrives on that connection, which only
/// happens when the client multiplexes instead of sending one request at a time.
#[tokio::test]
async fn chatgpt_client_multiplexes_concurrent_http2_requests() {
    let up = Upstream::start(H2, std::time::Duration::ZERO, Mode::Pair).await;
    let (transport, url) = (up.transport(), up.url());
    // The connection's first stream is answered at once, so the pool holds a live one.
    get_ok(&transport, &url, &Proxy::Inherit).await;
    futures_util::future::join_all((0..2).map(|_| get_ok(&transport, &url, &Proxy::Inherit))).await;
    assert_eq!(up.accepted(), 1, "one connection");
    assert_eq!(up.pairs(), 1, "both requests in flight on it together");
}

/// At most two connections ([`CHROME_IDLE_PER_HOST`]) stay idle: after four concurrent HTTP/1.1
/// requests (four connections) the client closes the two extra ones, and the next burst
/// reuses the two it kept.
#[tokio::test]
async fn chatgpt_client_keeps_at_most_two_idle_connections() {
    let up = Upstream::start(H1, std::time::Duration::from_millis(100), Mode::KeepAlive).await;
    let (transport, url) = (up.transport(), up.url());
    let burst = || futures_util::future::join_all((0..4).map(|_| get_ok(&transport, &url, &Proxy::Inherit)));
    burst().await;
    assert_eq!(up.accepted(), 4);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(up.closed_by_client(), 2, "idle connections beyond the bound");
    burst().await;
    assert_eq!(up.accepted(), 6, "the two kept ones are reused");
}

/// A pooled connection the server closed while it sat idle is not used again: the next
/// request opens a new connection and succeeds, over HTTP/2 and HTTP/1.1.
#[tokio::test]
async fn chatgpt_client_replaces_a_connection_the_server_closed() {
    for alpn in [H2, H1] {
        let up = Upstream::start(alpn, std::time::Duration::ZERO, Mode::CloseAfterFirst).await;
        let (transport, url) = (up.transport(), up.url());
        get_ok(&transport, &url, &Proxy::Inherit).await;
        // The server closes its side 50 ms after answering.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        get_ok(&transport, &url, &Proxy::Inherit).await;
        assert_eq!(up.accepted(), 2, "ALPN {alpn:?}");
    }
}

/// An HTTP CONNECT proxy that tunnels every request to `target` and counts its tunnels.
async fn counting_proxy(target: std::net::SocketAddr) -> (Proxy, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tunnels = std::sync::Arc::new(AtomicUsize::new(0));
    let count = tunnels.clone();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let count = count.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if client.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                if !head.starts_with(b"CONNECT ") {
                    return;
                }
                count.fetch_add(1, SeqCst);
                let mut upstream = tokio::net::TcpStream::connect(target).await.unwrap();
                client
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await
                    .unwrap();
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    (Proxy::Url(format!("http://{addr}")), tunnels)
}

/// Pools never cross proxies: each effective proxy has its own client, so a request
/// through proxy B never rides a connection tunnelled through proxy A (or a direct one),
/// and each proxy's own connection is reused for its later requests.
#[tokio::test]
async fn chatgpt_pools_are_isolated_per_proxy() {
    use std::sync::atomic::Ordering::SeqCst;
    let up = Upstream::start(H2, std::time::Duration::ZERO, Mode::KeepAlive).await;
    let (transport, url) = (up.transport(), up.url());
    let ((a, a_tunnels), (b, b_tunnels)) = (counting_proxy(up.addr).await, counting_proxy(up.addr).await);
    get_ok(&transport, &url, &a).await;
    get_ok(&transport, &url, &a).await;
    assert_eq!(
        (a_tunnels.load(SeqCst), up.accepted()),
        (1, 1),
        "proxy A reuses its tunnel"
    );
    get_ok(&transport, &url, &b).await;
    assert_eq!(
        (a_tunnels.load(SeqCst), b_tunnels.load(SeqCst), up.accepted()),
        (1, 1, 2),
        "proxy B dials its own"
    );
    get_ok(&transport, &url, &Proxy::Direct).await;
    assert_eq!(up.accepted(), 3, "a direct request uses neither tunnel");
    get_ok(&transport, &url, &a).await;
    get_ok(&transport, &url, &b).await;
    assert_eq!(
        (a_tunnels.load(SeqCst), b_tunnels.load(SeqCst), up.accepted()),
        (1, 1, 3),
        "each proxy kept its own connection"
    );
}
