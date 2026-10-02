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
//!   rules applied to the final provider body).
//! - `headers`: util/header_helpers.go (custom `header:*` attributes and `$Header`
//!   references copied from the client request).
//! - `json`: Go-exact tidwall/gjson, tidwall/sjson and encoding/json behaviour, used by
//!   every crate that edits JSON on the wire (owner: translators).
//! - `codex_client`: executor/helps/codex_multi_agent_v2.go (Codex-client request rewrites
//!   shared by every executor that serves Codex clients).
//!
//! - `gojson`: tidwall/gjson and sjson v1.2.5 semantics for byte-faithful raw JSON edits,
//!   used by `thinking` and `signature` and available to every executor and translator.
//!
//! The proxy-aware HTTP client (executor/helps/proxy_helpers.go) touches the network and
//! lives in cpa-exec instead.

pub mod gojson;
pub mod gostr;
mod gostr_tables;
pub mod json;
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
