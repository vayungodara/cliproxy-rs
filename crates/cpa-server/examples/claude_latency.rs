//! Latency the proxy adds before the first byte on the Claude route (docs/BENCHMARKS.md,
//! "Claude latency"). Loopback only: run it inside `unshare -rn` (bench/latency.sh).
//!
//!   cargo run --release -p cpa-server --example claude_latency -- oauth
//!   cargo run --release -p cpa-server --example claude_latency -- apikey
//!
//! One process, one monotonic clock. The proxy (the production router, listener and
//! Claude executor) runs on its own Tokio runtime; a mock Anthropic upstream and a
//! keep-alive client run on a second one. `oauth` points the native Claude TLS client at
//! a local TLS mock through the executor's test hooks (`api.anthropic.com` resolves to
//! loopback, a throwaway CA is trusted), with a Claude Code OAuth credential, so
//! cloaking, cache_control, tool-name remapping and CCH signing all run. `apikey` uses a
//! Claude API key whose `base-url` is a plain HTTP mock.
//!
//! For each body size and concurrency it sends the same requests straight to the mock
//! (the baseline) and through the proxy, and prints one JSON line per run:
//! - `up_first`: client start to the mock's request handler (the request head arrived);
//! - `up_done`: client start to the whole request body read by the mock;
//! - `client_first`: client start to the first response body byte at the client;
//! - `stages` (concurrency 1 only): medians of the proxy's internal marks, from the
//!   `cpa_latency` trace events (handler entered, credential selected, executor entered,
//!   request translated, upstream request ready) and the mock's timestamps;
//! - `new_upstream_conns`: TCP (+TLS) connections the mock accepted during the
//!   measured requests, and `conn_setup_p50_ms`, the median server-side setup time
//!   after `accept()` returned: the TLS handshake on the OAuth path, next to nothing on
//!   the plain-HTTP API-key path. The TCP connect itself is not included;
//! - `process_vmhwm_kb`: this process's peak resident memory so far (proxy, mock and
//!   client together), so an upper bound on the proxy's own peak.
//!
//! Environment: N (requests per run, default 200), SIZES (bytes, default
//! 5000,50000,300000,2000000), CONC (default 1,8), WARMUP (default 20 requests).

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};

const KEY: &str = "sk-bench-client-key";
const MODEL: &str = "claude-sonnet-4-5-20250929";

// ---- proxy stage marks -------------------------------------------------------------

static MARKS: Mutex<Vec<(&'static str, Instant)>> = Mutex::new(Vec::new());
/// Marks are recorded only while a one-client run collects them; concurrent runs would
/// interleave requests in one vector.
static COLLECTING: AtomicBool = AtomicBool::new(false);
const STAGES: [&str; 5] = ["handler", "selected", "executor", "translated", "prepared"];
/// Every point of one request in order: the client start, the proxy's marks, the mock's
/// timestamps and the client's first byte. A stage is the span between neighbours.
const POINTS: [&str; 9] = [
    "start",
    "handler",
    "selected",
    "executor",
    "translated",
    "prepared",
    "upstream_head",
    "upstream_done",
    "client_first",
];
const STAGE_KEYS: [&str; 8] = [
    "start->handler",
    "handler->selected",
    "selected->executor",
    "executor->translated",
    "translated->prepared",
    "prepared->upstream_head",
    "upstream_head->upstream_done",
    "upstream_done->client_first",
];

struct Probe;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Probe {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Stage(Option<&'static str>);
        impl tracing::field::Visit for Stage {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "stage" {
                    self.0 = STAGES.iter().copied().find(|s| *s == value);
                }
            }
            fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
        }
        if !COLLECTING.load(Ordering::Relaxed) {
            return;
        }
        let now = Instant::now();
        let mut stage = Stage(None);
        event.record(&mut stage);
        if let Some(stage) = stage.0 {
            MARKS.lock().unwrap().push((stage, now));
        }
    }
}

// ---- mock upstream -----------------------------------------------------------------

#[derive(Clone, Copy)]
struct Seen {
    head: Instant,
    done: Instant,
}

#[derive(Default)]
struct Mock {
    conns: AtomicUsize,
    setups: Mutex<Vec<Duration>>,
    seen: Mutex<HashMap<u64, Seen>>,
}

const SSE: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_bench\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"model\":\"claude-sonnet-4-5-20250929\",\"content\":[],\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"cache_creation_input_tokens\":0,",
    "\"cache_read_input_tokens\":0,\"output_tokens\":1}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":2}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

fn marker(body: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(body).ok()?;
    let at = text.find("lat-req-")? + 8;
    let digits: String = text[at..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

async fn serve_mock(listener: tokio::net::TcpListener, tls: Option<Arc<btls::ssl::SslAcceptor>>, mock: Arc<Mock>) {
    loop {
        let (tcp, _) = listener.accept().await.unwrap();
        // Setup time starts here, after the TCP connect completed (not measured).
        let accepted = Instant::now();
        tcp.set_nodelay(true).unwrap();
        let (tls, mock) = (tls.clone(), mock.clone());
        tokio::spawn(async move {
            mock.conns.fetch_add(1, Ordering::Relaxed);
            match tls {
                Some(acceptor) => {
                    let ssl = btls::ssl::Ssl::new(acceptor.context()).unwrap();
                    let mut stream = tokio_btls::SslStream::new(ssl, tcp).unwrap();
                    if std::pin::Pin::new(&mut stream).accept().await.is_err() {
                        return;
                    }
                    mock.setups.lock().unwrap().push(accepted.elapsed());
                    serve_conn(stream, mock).await;
                }
                None => {
                    mock.setups.lock().unwrap().push(accepted.elapsed());
                    serve_conn(tcp, mock).await;
                }
            }
        });
    }
}

async fn serve_conn<I>(io: I, mock: Arc<Mock>)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let mock = mock.clone();
        async move {
            let head = Instant::now();
            let body = axum::body::to_bytes(axum::body::Body::new(req.into_body()), usize::MAX)
                .await
                .unwrap_or_default();
            let done = Instant::now();
            if let Some(id) = marker(&body) {
                mock.seen.lock().unwrap().insert(id, Seen { head, done });
            }
            let response = hyper::Response::builder()
                .header("content-type", "text/event-stream")
                .header("request-id", "req_bench")
                .body(axum::body::Body::from(SSE))
                .unwrap();
            Ok::<_, Infallible>(response)
        }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(hyper_util::rt::TokioIo::new(io), service)
        .await;
}

/// A throwaway CA and a leaf for `host`: (CA PEM, acceptor offering HTTP/1.1, as the
/// native Claude client asks for nothing else).
fn tls_acceptor(host: &str) -> (Vec<u8>, btls::ssl::SslAcceptor) {
    use btls::asn1::Asn1Time;
    use btls::bn::BigNum;
    use btls::ec::{EcGroup, EcKey};
    use btls::hash::MessageDigest;
    use btls::nid::Nid;
    use btls::pkey::PKey;
    use btls::ssl::{SslAcceptor, SslMethod};
    use btls::x509::extension::{BasicConstraints, SubjectAlternativeName};
    use btls::x509::{X509, X509NameBuilder};
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
    let key = || PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap();
    let name = |cn: &str| {
        let mut n = X509NameBuilder::new().unwrap();
        n.append_entry_by_text("CN", cn).unwrap();
        n.build()
    };
    let (ca_key, leaf_key) = (key(), key());
    let ca_name = name("cliproxy-rs latency bench CA");
    let mut ca = X509::builder().unwrap();
    ca.set_version(2).unwrap();
    ca.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    ca.set_subject_name(&ca_name).unwrap();
    ca.set_issuer_name(&ca_name).unwrap();
    ca.set_pubkey(&ca_key).unwrap();
    ca.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
    ca.set_not_after(&Asn1Time::days_from_now(2).unwrap()).unwrap();
    ca.append_extension(&BasicConstraints::new().critical().ca().build().unwrap())
        .unwrap();
    ca.sign(&ca_key, MessageDigest::sha256()).unwrap();
    let ca = ca.build();
    let mut leaf = X509::builder().unwrap();
    leaf.set_version(2).unwrap();
    leaf.set_serial_number(&BigNum::from_u32(2).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    leaf.set_subject_name(&name(host)).unwrap();
    leaf.set_issuer_name(&ca_name).unwrap();
    leaf.set_pubkey(&leaf_key).unwrap();
    leaf.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
    leaf.set_not_after(&Asn1Time::days_from_now(2).unwrap()).unwrap();
    let san = SubjectAlternativeName::new()
        .dns(host)
        .build(&leaf.x509v3_context(Some(&ca), None))
        .unwrap();
    leaf.append_extension(&san).unwrap();
    leaf.sign(&ca_key, MessageDigest::sha256()).unwrap();
    let leaf = leaf.build();
    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    acceptor.set_private_key(&leaf_key).unwrap();
    acceptor.set_certificate(&leaf).unwrap();
    acceptor.set_alpn_select_callback(|_, client| {
        btls::ssl::select_next_proto(b"\x08http/1.1", client).ok_or(btls::ssl::AlpnError::NOACK)
    });
    (ca.to_pem().unwrap(), acceptor.build())
}

// ---- request bodies ----------------------------------------------------------------

const WORDS: &str = "the function returns early when the buffer is empty so callers must check the length \
    before reading and the parser keeps a cursor into the source while the tokenizer emits spans \
    for identifiers numbers strings and punctuation which the resolver later binds to declarations ";

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn text(&mut self, n: usize) -> String {
        let mut out = String::with_capacity(n + WORDS.len());
        while out.len() < n {
            let i = (self.next() % (WORDS.len() as u64 - 40)) as usize;
            out.push_str(&WORDS[i..]);
            out.push_str(&format!(" {} ", self.next() % 1_000_000_007));
        }
        out.truncate(n);
        out
    }
}

/// A coding-agent conversation of `size` bytes (within a few bytes), shaped like
/// bench/messages: a system prompt with a cache breakpoint, tools, then assistant turns
/// (signed thinking, text, tool_use) and user tool_result turns, and a final user text
/// padded to the size. `{ID}` marks the request.
fn conversation(size: usize) -> String {
    let mut r = Rng(0x9e37_79b9_7f4a_7c15 ^ size as u64);
    let system_len = (size / 5).clamp(600, 18_000);
    let system = json!([
        {"type": "text", "text": format!("You are a coding agent working in the user's repository. {}", r.text(system_len / 3))},
        {"type": "text", "text": r.text(system_len * 2 / 3), "cache_control": {"type": "ephemeral"}},
    ]);
    let tool_count = (size / 8_000).clamp(1, 24);
    let tools: Vec<Value> = (0..tool_count)
        .map(|i| {
            json!({
                "name": format!("tool_{i:02}"),
                "description": r.text(900),
                "input_schema": {"type": "object", "properties": {
                    "path": {"type": "string", "description": r.text(120)},
                    "content": {"type": "string", "description": r.text(120)},
                    "limit": {"type": "integer"},
                }, "required": ["path"]},
            })
        })
        .collect();
    let render = |messages: &[Value], tail: &str| {
        let mut messages = messages.to_vec();
        messages.push(json!({"role": "user", "content": [{"type": "text", "text": format!("{tail} lat-req-{{ID}}")}]}));
        serde_json::to_string(&json!({
            "model": MODEL,
            "max_tokens": 32000,
            "stream": true,
            "thinking": {"type": "enabled", "budget_tokens": 16000},
            "metadata": {"user_id": "user_bench_account__session_0123456789abcdef"},
            "system": system,
            "tools": tools,
            "messages": messages,
        }))
        .unwrap()
    };
    let step = (size / 12).clamp(1_500, 25_000);
    let mut messages = vec![json!({"role": "user", "content": [{"type": "text", "text": r.text(300)}]})];
    let mut n = 0;
    while render(&messages, "").len() + step <= size {
        let id = format!("toolu_bench_{n}");
        let thinking = (step / 16).clamp(100, 1_500);
        let text = (step / 60).clamp(40, 400);
        messages.push(json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": r.text(thinking), "signature": "EqQBCkgIBRABGAIiQL".repeat(20)},
            {"type": "text", "text": r.text(text)},
            {"type": "tool_use", "id": id, "name": format!("tool_{:02}", n % tool_count), "input": {"path": "src/lib.rs"}},
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": r.text(step - thinking - text - 800)},
        ]}));
        n += 1;
    }
    let pad = size.saturating_sub(render(&messages, "").len());
    render(&messages, &r.text(pad))
}

// ---- load --------------------------------------------------------------------------

struct Sample {
    id: u64,
    start: Instant,
    client_first: Duration,
}

struct Target {
    client: wreq::Client,
    url: String,
}

async fn one(target: &Target, template: &str, id: u64) -> Sample {
    let body = Bytes::from(template.replace("{ID}", &id.to_string()));
    let start = Instant::now();
    let response = target
        .client
        .post(&target.url)
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "interleaved-thinking-2025-05-14")
        .header("x-api-key", KEY)
        .body(body)
        .send()
        .await
        .expect("request");
    let status = response.status();
    let mut stream = response.bytes_stream();
    let first = stream.next().await;
    let client_first = start.elapsed();
    let mut data = first.map(|c| c.unwrap().to_vec()).unwrap_or_default();
    while let Some(chunk) = stream.next().await {
        data.extend_from_slice(&chunk.unwrap());
    }
    let text = String::from_utf8_lossy(&data);
    assert!(
        status == 200 && text.contains("message_stop"),
        "bad response {status}: {}",
        &text[..text.len().min(400)]
    );
    Sample {
        id,
        start,
        client_first,
    }
}

fn pct(values: &mut [f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let v = values[((values.len() - 1) as f64 * p).round() as usize];
    (v * 1000.0).round() / 1000.0
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

struct Run {
    samples: Vec<Sample>,
    marks: Vec<Vec<(&'static str, Instant)>>,
}

async fn run(target: Arc<Target>, template: Arc<String>, n: usize, conc: usize, next_id: Arc<AtomicUsize>) -> Run {
    let collect_marks = conc == 1;
    COLLECTING.store(collect_marks, Ordering::Relaxed);
    let mut tasks = Vec::new();
    let per = n.div_ceil(conc);
    for _ in 0..conc {
        let (target, template, next_id) = (target.clone(), template.clone(), next_id.clone());
        tasks.push(tokio::spawn(async move {
            let mut out = Vec::new();
            for _ in 0..per {
                let id = next_id.fetch_add(1, Ordering::Relaxed) as u64;
                if collect_marks {
                    MARKS.lock().unwrap().clear();
                }
                let sample = one(&target, &template, id).await;
                let marks = if collect_marks {
                    std::mem::take(&mut *MARKS.lock().unwrap())
                } else {
                    Vec::new()
                };
                out.push((sample, marks));
            }
            out
        }));
    }
    let mut samples = Vec::new();
    let mut marks = Vec::new();
    for task in tasks {
        for (sample, mark) in task.await.unwrap() {
            samples.push(sample);
            marks.push(mark);
        }
    }
    Run { samples, marks }
}

fn summarize(run: &Run, mock: &Mock) -> Value {
    let seen = mock.seen.lock().unwrap();
    let (mut up_first, mut up_done, mut client_first) = (Vec::new(), Vec::new(), Vec::new());
    let mut stages: [Vec<f64>; STAGE_KEYS.len()] = Default::default();
    for (i, s) in run.samples.iter().enumerate() {
        let m = seen.get(&s.id).copied().expect("the mock saw the request");
        up_first.push(ms(m.head - s.start));
        up_done.push(ms(m.done - s.start));
        client_first.push(ms(s.client_first));
        let marks = &run.marks[i];
        if marks.is_empty() {
            continue;
        }
        let at = |name: &str| marks.iter().find(|(n, _)| *n == name).map(|(_, t)| *t);
        let mut points = vec![Some(s.start)];
        points.extend(STAGES.iter().map(|n| at(n)));
        points.extend([Some(m.head), Some(m.done), Some(s.start + s.client_first)]);
        debug_assert_eq!(points.len(), POINTS.len());
        for (i, pair) in points.windows(2).enumerate() {
            if let (Some(a), Some(b)) = (pair[0], pair[1]) {
                stages[i].push(ms(b.saturating_duration_since(a)));
            }
        }
    }
    let mut stage_json = serde_json::Map::new();
    for (key, values) in STAGE_KEYS.iter().zip(&mut stages) {
        if !values.is_empty() {
            stage_json.insert(
                (*key).to_owned(),
                json!({"p50": pct(values, 0.5), "p99": pct(values, 0.99)}),
            );
        }
    }
    json!({
        "up_first_ms": {"p50": pct(&mut up_first, 0.5), "p99": pct(&mut up_first, 0.99)},
        "up_done_ms": {"p50": pct(&mut up_done, 0.5), "p99": pct(&mut up_done, 0.99)},
        "client_first_ms": {"p50": pct(&mut client_first, 0.5), "p99": pct(&mut client_first, 0.99)},
        "stages_ms": stage_json,
    })
}

/// `VmHWM` of this process from /proc, in KB (0 where /proc is missing).
fn vmhwm_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmHWM:"))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        })
        .unwrap_or(0)
}

fn env_list(name: &str, default: &str) -> Vec<usize> {
    std::env::var(name)
        .unwrap_or_else(|_| default.into())
        .split(',')
        .map(|s| s.trim().parse().unwrap())
        .collect()
}

fn main() {
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let mode = std::env::args().nth(1).unwrap_or_else(|| "oauth".into());
    assert!(mode == "oauth" || mode == "apikey", "mode: oauth or apikey");
    let n: usize = std::env::var("N").ok().map_or(200, |v| v.parse().unwrap());
    let warmup: usize = std::env::var("WARMUP").ok().map_or(20, |v| v.parse().unwrap());
    let sizes = env_list("SIZES", "5000,50000,300000,2000000");
    let concs = env_list("CONC", "1,8");
    tracing_subscriber::registry()
        .with(
            Probe.with_filter(
                tracing_subscriber::filter::Targets::new().with_target("cpa_latency", tracing::Level::TRACE),
            ),
        )
        .init();

    let workers = |n| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(n)
            .enable_all()
            .build()
            .unwrap()
    };
    let proxy_rt = workers(4);
    let load_rt = workers(4);

    // Mock upstream.
    let mock = Arc::new(Mock::default());
    let (ca, acceptor) = tls_acceptor("api.anthropic.com");
    let tls = (mode == "oauth").then(|| Arc::new(acceptor));
    let mock_addr: SocketAddr = load_rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_mock(listener, tls.clone(), mock.clone()));
        addr
    });

    // Proxy.
    let dir = std::env::temp_dir().join(format!("cpa-latency-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut yaml = format!(
        "auth-dir: {:?}\napi-keys:\n  - {KEY}\nrequest-retry: 0\n",
        dir.to_string_lossy()
    );
    if mode == "oauth" {
        std::fs::write(
            dir.join("claude-bench.json"),
            json!({
                "type": "claude",
                "email": "bench@example.com",
                "access_token": "sk-ant-oat01-bench-not-a-real-token",
                "expired": "2099-01-01T00:00:00Z",
                "claude_device_ids": ["0123456789abcdef".repeat(4)],
                "account_uuid": "6f1d2c3b-4a59-4e6f-8a7b-9c0d1e2f3a4b",
            })
            .to_string(),
        )
        .unwrap();
    } else {
        yaml.push_str(&format!(
            "claude-api-key:\n  - api-key: sk-ant-api03-FAKE-bench-key\n    base-url: http://{mock_addr}\n    models:\n      - name: {MODEL}\n        alias: {MODEL}\n"
        ));
    }
    let config = cpa_core::config::Config::parse(&yaml).unwrap();
    let credentials: Vec<_> = cpa_core::config::credentials::load(&config)
        .into_iter()
        .map(cpa_server::testing::local)
        .collect();
    assert_eq!(credentials.len(), 1, "one Claude credential");
    let hooks = cpa_exec::claude::Hooks {
        trust: Some(wreq::tls::trust::CertStore::from_pem_stack(ca.clone()).unwrap()),
        resolve: vec![("api.anthropic.com".into(), mock_addr)],
    };
    let executors = cpa_exec::Executors {
        claude: cpa_exec::claude::ClaudeExecutor::with_hooks(hooks, cpa_exec::claude::DEFAULT_BASE_URL),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let proxy_addr: SocketAddr = proxy_rt.block_on(async {
        let rt = Arc::new(cpa_server::testing::runtime(config, credentials, executors));
        rt.set_local_model(true);
        cpa_server::install_registry(&rt);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(cpa_server::listener::serve(listener, cpa_server::router(rt), None));
        addr
    });

    let direct_client = wreq::Client::builder()
        .no_proxy()
        .tls_cert_store(wreq::tls::trust::CertStore::from_pem_stack(ca).unwrap())
        .resolve("api.anthropic.com", mock_addr)
        .build()
        .unwrap();
    let direct = Arc::new(Target {
        client: direct_client,
        url: if mode == "oauth" {
            "https://api.anthropic.com/v1/messages".into()
        } else {
            format!("http://{mock_addr}/v1/messages")
        },
    });
    let proxied = Arc::new(Target {
        client: wreq::Client::builder().no_proxy().build().unwrap(),
        url: format!("http://{proxy_addr}/v1/messages"),
    });

    let next_id = Arc::new(AtomicUsize::new(1));
    load_rt.block_on(async {
        for &size in &sizes {
            let template = Arc::new(conversation(size));
            for &conc in &concs {
                let mut line = serde_json::Map::new();
                line.insert("mode".into(), json!(mode));
                line.insert("size".into(), json!(template.len()));
                line.insert("conc".into(), json!(conc));
                line.insert("n".into(), json!(n));
                for (label, target) in [("direct", &direct), ("proxy", &proxied)] {
                    run(
                        target.clone(),
                        template.clone(),
                        warmup.max(conc),
                        conc,
                        next_id.clone(),
                    )
                    .await;
                    let conns_before = mock.conns.load(Ordering::Relaxed);
                    let setups_before = mock.setups.lock().unwrap().len();
                    let result = run(target.clone(), template.clone(), n, conc, next_id.clone()).await;
                    let mut summary = summarize(&result, &mock);
                    let mut setups: Vec<f64> = mock.setups.lock().unwrap()[setups_before..]
                        .iter()
                        .map(|d| ms(*d))
                        .collect();
                    summary["new_upstream_conns"] = json!(mock.conns.load(Ordering::Relaxed) - conns_before);
                    summary["conn_setup_p50_ms"] = json!(pct(&mut setups, 0.5));
                    line.insert(label.into(), summary);
                }
                // The whole process (proxy, mock and client) so far: an upper bound on
                // the proxy's own peak.
                line.insert("process_vmhwm_kb".into(), json!(vmhwm_kb()));
                println!("{}", Value::Object(line));
            }
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
}
