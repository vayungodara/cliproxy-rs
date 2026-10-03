//! Port of Go's sdk/translator suite (registry_test.go, registry_summary_test.go,
//! registry_bytes_test.go) against Go-generated vectors in tests/fixtures/sdk_registry.json
//! (tests/reference/sdk.go).
//!
//! Covered elsewhere: summary intent through registered pairs, envelope dispatch, byte
//! outputs and apply_patch nil results are byte goldens in tests/golden.rs (the generator
//! calls sdk.TranslateRequest, TranslateRequestEnvelope, TranslateNonStream,
//! TranslateStream and TranslateTokenCount).
//! ponytail: plugin-hook tests (M6) are not ported; Rust has no plugin hooks.
use cpa_core::format::Format;
use cpa_translate::{RequestCtx, pair, token_count, translate_request, translate_token_count};
use serde_json::Value;

fn load() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sdk_registry.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The generator writes each byte as one rune.
fn latin1(v: &Value) -> Vec<u8> {
    v.as_str()
        .unwrap()
        .chars()
        .map(|c| u8::try_from(c as u32).unwrap())
        .collect()
}

fn format(v: &Value) -> Format {
    Format::parse(v.as_str().unwrap()).unwrap()
}

#[test]
fn registered_pairs_match_go() {
    let doc = load();
    let regs = doc["registrations"].as_array().unwrap();
    assert_eq!(regs.len(), Format::ALL.len() * Format::ALL.len());
    for r in regs {
        let (client, upstream) = (format(&r["client"]), format(&r["upstream"]));
        let go = r["request"].as_bool().unwrap();
        // Rust registers request, stream and non-stream together; so does Go.
        assert_eq!(go, r["stream"].as_bool().unwrap());
        assert_eq!(go, r["non_stream"].as_bool().unwrap());
        assert_eq!(
            pair(client, upstream).is_some(),
            go,
            "{client:?} -> {upstream:?} registration"
        );
        assert_eq!(
            token_count(client, upstream).is_some(),
            r["token_count"].as_bool().unwrap(),
            "{client:?} -> {upstream:?} TokenCount"
        );
    }
}

#[test]
fn fallbacks_match_go() {
    let doc = load();
    let vectors = doc["vectors"].as_array().unwrap();
    assert!(vectors.len() > 30, "generator lost vectors");
    let mut failures = vec![];
    for v in vectors {
        let (client, upstream) = (format(&v["client"]), format(&v["upstream"]));
        let (input, want) = (latin1(&v["input"]), latin1(&v["output"]));
        let got = match v["path"].as_str().unwrap() {
            "request" => {
                let ctx = RequestCtx {
                    model: v["model"].as_str().unwrap_or_default(),
                    stream: false,
                };
                translate_request(client, upstream, &ctx, &input).unwrap()
            }
            "token_count" => translate_token_count(client, upstream, v["count"].as_i64().unwrap(), &input),
            other => panic!("unknown path {other}"),
        };
        if got != want {
            failures.push(format!(
                "{} {client:?}->{upstream:?}: got {:?}, want {:?}",
                v["name"],
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(&want)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
