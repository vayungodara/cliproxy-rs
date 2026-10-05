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
    let client = transport.for_url(&url, &Proxy::Inherit);
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

/// Go dials a dedicated uTLS connection for every chatgpt.com request. The Chrome client
/// here keeps its connections instead (docs/DIFFERENCES-FROM-GO.md): HTTP/2 multiplexes
/// requests, concurrent ones included, and HTTP/1.1 keeps a connection alive.
#[tokio::test]
async fn chatgpt_client_reuses_connections() {
    use std::sync::atomic::Ordering::SeqCst;
    for alpn in [&b"\x02h2\x08http/1.1"[..], b"\x08http/1.1"] {
        let (ca, addr, accepted) = crate::test_tls::counting_upstream("chatgpt.com", alpn).await;
        let transport = Transport::new(Hooks {
            trust: Some(wreq::tls::trust::CertStore::from_pem_stack(ca).unwrap()),
            resolve: vec![("chatgpt.com".into(), addr)],
        });
        let url = format!("https://chatgpt.com:{}/backend-api/codex/responses", addr.port());
        let get = || async {
            let client = transport.for_url(&url, &Proxy::Inherit);
            let response = client.get(&url).send().await.unwrap();
            assert_eq!(response.bytes().await.unwrap(), "ok");
        };
        for _ in 0..3 {
            get().await;
        }
        assert_eq!(accepted.load(SeqCst), 1, "sequential requests, ALPN {alpn:?}");
        if alpn.starts_with(b"\x02h2") {
            futures_util::future::join_all((0..4).map(|_| get())).await;
            assert_eq!(accepted.load(SeqCst), 1, "concurrent HTTP/2 requests");
        }
    }
}
