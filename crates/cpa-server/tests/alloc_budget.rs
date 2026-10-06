//! Memory gate: the heap a large Claude request costs on its way through the proxy.
//!
//! A counting global allocator (this test binary only; release builds use the system
//! allocator untouched) records live bytes and allocation calls while one streamed
//! `/v1/messages` request of about 306 KB, then one of about 1.9 MB, goes from a
//! client through the router to a local fake Anthropic upstream and back. The request
//! body is built before counting starts and handed to the router directly. The fake
//! upstream runs on its own thread and runtime, which the allocator does not count:
//! hyper sizes its read buffers from how much the previous read returned, so the
//! upstream's allocations depend on scheduling, not on the proxy. Each size is sent
//! three times and gated on the smallest peak and call count. The ceilings are the
//! `alloc.*` lines of `bench/budgets.txt`; the measured values are printed so a change
//! can update them with a dated note.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::response::IntoResponse;
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use futures_util::StreamExt;
use tower::ServiceExt;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Set on the fake upstream's thread. `const`, so reading it never allocates.
    static UNCOUNTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn counted() -> bool {
    !UNCOUNTED.try_with(std::cell::Cell::get).unwrap_or(false)
}

// SAFETY: every call is forwarded to the system allocator unchanged; the counters are
// plain atomics and never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() && counted() {
            grow(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        if counted() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() && counted() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            grow(new_size);
        }
        p
    }
}

fn grow(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    if COUNTING.load(Ordering::Relaxed) {
        CALLS.fetch_add(1, Ordering::Relaxed);
        PEAK.fetch_max(live, Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The fake upstream drains the request without keeping it and streams a short reply.
async fn upstream(req: Request) -> axum::response::Response {
    let mut body = req.into_body().into_data_stream();
    while let Some(chunk) = body.next().await {
        drop(chunk.unwrap());
    }
    let sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n\
               event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
               event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n\
               event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
               event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n\
               event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    ([("content-type", "text/event-stream")], sse).into_response()
}

/// A coding-agent conversation of about `target` bytes, shaped like `bench/messages`:
/// an 18 KB system prompt, 24 tools, then assistant `tool_use` and user `tool_result`
/// turns.
fn conversation(target: usize) -> Bytes {
    use serde_json::json;
    let filler = |n: usize| "Explain the change in this diff and list any risks. ".repeat(n / 52 + 1);
    let tools: Vec<_> = (0..24)
        .map(|i| {
            json!({
                "name": format!("tool_{i}"),
                "description": filler(400),
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}, "text": {"type": "string"}}},
            })
        })
        .collect();
    let mut messages = vec![json!({"role": "user", "content": filler(2000)})];
    let mut size = 30_000;
    let mut turn = 0;
    while size < target {
        let id = format!("toolu_{turn:06}");
        messages.push(json!({"role": "assistant", "content": [
            {"type": "text", "text": filler(1000)},
            {"type": "tool_use", "id": id, "name": "tool_1", "input": {"path": "src/main.rs"}},
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": filler(20_000)},
        ]}));
        size += 22_000;
        turn += 1;
    }
    let body = json!({
        "model": "claude-opus-5-5",
        "max_tokens": 1024,
        "stream": true,
        "system": [{"type": "text", "text": filler(18_000)}],
        "tools": tools,
        "messages": messages,
    });
    Bytes::from(serde_json::to_vec(&body).unwrap())
}

/// The ceiling for `key` in bench/budgets.txt: `<key> <value> <tolerance %>`, read the way
/// bench/gate.sh reads it.
fn budget(key: &str) -> usize {
    include_str!("../../../bench/budgets.txt")
        .lines()
        .filter_map(|l| l.split('#').next())
        .find_map(|l| {
            let parts: Vec<&str> = l.split_whitespace().collect();
            (parts.first() == Some(&key)).then(|| {
                let value: f64 = parts[1].replace('_', "").parse().unwrap();
                let tolerance: f64 = parts.get(2).map_or(0.0, |t| t.parse().unwrap());
                (value * (1.0 + tolerance / 100.0)) as usize
            })
        })
        .unwrap_or_else(|| panic!("bench/budgets.txt has no {key}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_claude_requests_stay_within_their_heap_budget() {
    // The fake upstream: its own thread and single-threaded runtime, not counted.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        UNCOUNTED.with(|u| u.set(true));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, axum::Router::new().fallback(upstream))
                .await
                .unwrap()
        });
    });
    let dir: PathBuf = std::env::temp_dir().join(format!("cpa-alloc-budget-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("claude.json"),
        r#"{"type":"claude","access_token":"tok-A","email":"a@example.com"}"#,
    )
    .unwrap();
    let mut config = Config::parse("").unwrap();
    config.api_keys = vec!["client-key-1".into()];
    config.auth_dir = dir.clone();
    let creds = cpa_core::config::credentials::from_auth_dir(&config)
        .into_iter()
        .map(cpa_server::testing::local)
        .collect();
    let executors = Executors {
        claude: ClaudeExecutor::new(&upstream_url).unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(config, creds, executors));
    cpa_server::install_registry(&rt);
    let app = cpa_server::router(rt);

    let send = |body: Bytes| {
        let app = app.clone();
        async move {
            let request = axum::http::Request::post("/v1/messages")
                .header("x-api-key", "client-key-1")
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let response = app.oneshot(request).await.unwrap();
            let status = response.status();
            let text = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert_eq!(status, 200, "{text:?}");
            assert!(text.windows(12).any(|w| w == b"message_stop"), "{text:?}");
        }
    };

    let mut over = Vec::new();
    for (name, target) in [("306k", 306_000), ("1900k", 1_900_000)] {
        let body = conversation(target);
        // Warm-up: connection pool, registries and lazy statics.
        send(body.clone()).await;
        let (mut peak, mut calls) = (usize::MAX, usize::MAX);
        for _ in 0..3 {
            let base = LIVE.load(Ordering::Relaxed);
            PEAK.store(base, Ordering::Relaxed);
            CALLS.store(0, Ordering::Relaxed);
            COUNTING.store(true, Ordering::Relaxed);
            send(body.clone()).await;
            COUNTING.store(false, Ordering::Relaxed);
            peak = peak.min(PEAK.load(Ordering::Relaxed).saturating_sub(base));
            calls = calls.min(CALLS.load(Ordering::Relaxed));
        }
        let (peak_max, calls_max) = (
            budget(&format!("alloc.{name}.peak_bytes")),
            budget(&format!("alloc.{name}.allocations")),
        );
        println!(
            "{name}: body {} B, peak live {peak} B ({:.1}x body, ceiling {peak_max}), {calls} allocations (ceiling {calls_max})",
            body.len(),
            peak as f64 / body.len() as f64,
        );
        if peak > peak_max {
            over.push(format!("alloc.{name}.peak_bytes {peak} > {peak_max}"));
        }
        if calls > calls_max {
            over.push(format!("alloc.{name}.allocations {calls} > {calls_max}"));
        }
    }
    let _ = std::fs::remove_dir_all(dir);
    assert!(over.is_empty(), "over budget (bench/budgets.txt): {over:?}");
}
