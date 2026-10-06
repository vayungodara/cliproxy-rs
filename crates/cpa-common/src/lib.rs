//! Provider-neutral request helpers used by both translators and executors. Pure
//! transforms only: no networking, no clocks, no global mutable state beyond what Go keeps
//! (signature caches are passed in by the caller).
//!
//! Modules and their single owner while the port is in progress:
//! - `thinking`: internal/thinking (suffix parsing, the canonical ApplyThinking pipeline,
//!   validation, conversion, every provider applier under internal/thinking/provider).
//! - `signature`: internal/signature (thought-signature detection, validation and
//!   sanitizing for Claude, Gemini, GPT, Grok and Kimi).
//! - `payload`: executor/helps/payload_helpers.go and codex_tool_schema.go (config payload
//!   rules applied to the final provider body; owner: server).
//! - `headers`: util/header_helpers.go (custom `header:*` attributes and `$Header`
//!   references copied from the client request; owner: server).
//! - `json`: Go-exact tidwall/gjson, tidwall/sjson and encoding/json behaviour, used by
//!   every crate that edits JSON on the wire (owner: translators).
//! - `codex_client`: executor/helps/codex_multi_agent_v2.go (Codex-client request rewrites
//!   shared by every executor that serves Codex clients; owner: Codex).
//! - `codex_catalog`: the Codex client model catalog and store (client/codex/models,
//!   registry/codex_client_models.go; owner: Codex).
//! - `gemini_schema`: util/gemini_schema.go (JSON Schema cleaning for Gemini and Antigravity
//!   tool and response schemas; owner: translators).
//! - `session`: sdk/cliproxy/session (session identity, parent/child relationships and the
//!   derived identity executors key provider sessions on; owner: server).
//! - `gostr`: Go's `strings.ToLower`, `strings.EqualFold`, `strings.TrimSpace` and
//!   `strconv.Quote` on Go's own Unicode tables (owner: Google).
//!
//! The proxy-aware HTTP client (executor/helps/proxy_helpers.go) touches the network and
//! lives in cpa-exec instead.

pub mod codex_catalog;
pub mod codex_client;
pub mod gemini_schema;
pub mod gostr;
mod gostr_tables;
pub mod headers;
pub mod idle;
pub mod json;
#[cfg(test)]
mod json_go_vectors;
pub mod payload;
pub mod session;
pub mod signature;
pub mod thinking;

/// Calls recorded from Go's own test suites at 6fecc6e, one JSON object per line
/// (tests/reference/record/README.md).
#[cfg(test)]
pub(crate) fn go_calls() -> impl Iterator<Item = &'static str> {
    use std::io::Read;
    static CALLS: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let gz = include_bytes!("../tests/fixtures/go_calls.jsonl.gz");
        let mut text = String::new();
        flate2::read::GzDecoder::new(&gz[..]).read_to_string(&mut text).unwrap();
        text
    });
    CALLS.lines()
}

/// A recorded Go string: plain JSON text, or `{"b64": ...}` for bytes that are not UTF-8.
#[cfg(test)]
pub(crate) fn recorded_bytes(v: &serde_json::Value) -> Vec<u8> {
    use base64::Engine;
    match v {
        serde_json::Value::String(s) => s.as_bytes().to_vec(),
        serde_json::Value::Object(m) if m.contains_key("b64") => base64::engine::general_purpose::STANDARD
            .decode(m["b64"].as_str().unwrap())
            .unwrap(),
        other => panic!("not a recorded string: {other}"),
    }
}

/// Encodes bytes the way the recorder does, for comparison with recorded outputs.
#[cfg(test)]
pub(crate) fn recorded_value(b: &[u8]) -> serde_json::Value {
    use base64::Engine;
    match std::str::from_utf8(b) {
        Ok(s) => serde_json::Value::String(s.into()),
        Err(_) => serde_json::json!({"b64": base64::engine::general_purpose::STANDARD.encode(b)}),
    }
}

/// Test-only allocation counter: bytes allocated on the current thread, so parallel
/// tests do not disturb each other.
#[cfg(test)]
pub(crate) mod alloc_count {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static BYTES: Cell<usize> = const { Cell::new(0) };
    }

    struct Counting;

    // SAFETY: forwards to the system allocator unchanged; the counter is a const
    // thread-local that never allocates.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _ = BYTES.try_with(|b| b.set(b.get() + layout.size()));
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let _ = BYTES.try_with(|b| b.set(b.get() + new_size));
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static GLOBAL: Counting = Counting;

    pub(crate) fn bytes() -> usize {
        BYTES.with(Cell::get)
    }
}
