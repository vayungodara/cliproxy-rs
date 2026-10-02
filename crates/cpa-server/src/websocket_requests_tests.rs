//! Pure-function parity with Go: every vector in `tests/ws_fixtures/ws_vectors.json` is
//! the output of CLIProxyAPI's own handler helpers (`zz_rsfix_ws_test.go`,
//! `TestRSFixWSVectors`). Expected values never come from this crate.

use super::*;
use crate::websocket_tools::{Retained, prepare_fallback_turn};
use serde_json::Value;
use std::sync::LazyLock;

static VECTORS: LazyLock<Vec<Value>> = LazyLock::new(|| {
    serde_json::from_str::<Value>(include_str!("../tests/ws_fixtures/ws_vectors.json"))
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
});

fn vectors(fun: &str) -> Vec<&'static Value> {
    let out: Vec<&Value> = VECTORS.iter().filter(|v| v["fn"] == fun).collect();
    assert!(!out.is_empty(), "no {fun} vectors");
    out
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or_default()
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| x.as_str().unwrap().to_owned())
        .collect()
}

fn expected_error(v: &Value) -> Option<WsError> {
    v["err_status"].as_u64().map(|status| WsError {
        status: status as u16,
        message: s(v, "err").to_owned(),
    })
}

#[test]
fn normalize_matches_go() {
    for v in vectors("normalize") {
        let got = normalize(
            s(v, "raw"),
            s(v, "last"),
            s(v, "output"),
            s(v, "last_id"),
            &strings(v, "pending"),
            false,
            v["bypass"].as_bool().unwrap_or(false),
        );
        match expected_error(v) {
            Some(error) => assert_eq!(got, Err(error), "raw {}", s(v, "raw")),
            None => assert_eq!(
                got,
                Ok((s(v, "out").to_owned(), s(v, "updated").to_owned())),
                "raw {} last {}",
                s(v, "raw"),
                s(v, "last")
            ),
        }
    }
}

#[test]
fn passthrough_matches_go() {
    for v in vectors("passthrough") {
        let got = normalize_passthrough(s(v, "raw"), s(v, "model"));
        match expected_error(v) {
            Some(error) => assert_eq!(got, Err(error), "raw {}", s(v, "raw")),
            None => assert_eq!(got, Ok(s(v, "out").to_owned()), "raw {}", s(v, "raw")),
        }
    }
}

#[test]
fn prewarm_followup_matches_go() {
    for v in vectors("prewarm_followup") {
        let got = prewarm_followup(s(v, "raw"), s(v, "last"));
        match expected_error(v) {
            Some(error) => assert_eq!(got, Err(error), "raw {}", s(v, "raw")),
            None => assert_eq!(
                got,
                Ok((s(v, "out").to_owned(), s(v, "updated").to_owned())),
                "raw {}",
                s(v, "raw")
            ),
        }
    }
}

#[test]
fn local_prewarm_detection_matches_go() {
    for v in vectors("prewarm_local") {
        assert_eq!(
            is_local_prewarm(s(v, "raw")),
            v["bool"].as_bool().unwrap_or(false),
            "{}",
            s(v, "raw")
        );
    }
}

#[test]
fn prewarm_payloads_match_go() {
    let v = vectors("prewarm_payloads")[0];
    let go = strings(v, "list");
    let id = gjson::get(&go[0], "response.id").str().to_owned();
    let created_at = gjson::get(&go[0], "response.created_at").i64();
    assert!(id.starts_with("resp_prewarm_"));
    assert_eq!(prewarm_payloads(s(v, "raw"), &id, created_at).to_vec(), go);
}

#[test]
fn chunk_payloads_match_go() {
    for v in vectors("chunk") {
        assert_eq!(
            payloads_from_chunk(s(v, "raw").as_bytes()),
            strings(v, "list"),
            "{:?}",
            s(v, "raw")
        );
    }
}

#[test]
fn error_payloads_match_go() {
    for v in vectors("error_payload") {
        let status = v["status"].as_u64().unwrap_or(0) as u16;
        assert_eq!(
            error_payload(status, s(v, "raw")),
            s(v, "out"),
            "{status} {:?}",
            s(v, "raw")
        );
    }
}

#[test]
fn exposure_matches_go() {
    for v in vectors("expose") {
        let status = v["status"].as_u64().unwrap() as u16;
        assert_eq!(
            crate::classify::is_request_fault(status, s(v, "raw")),
            v["bool"].as_bool().unwrap_or(false),
            "{status} {}",
            s(v, "raw")
        );
    }
}

#[test]
fn completion_restore_and_pending_calls_match_go() {
    for v in vectors("completion") {
        let mut turn = Turn::default();
        let mut got = Vec::new();
        for event in strings(v, "events") {
            turn.collect(&event);
            let kind = gjson::get(&event, "type").str().to_owned();
            let event = if kind == "response.completed" || kind == "response.done" {
                let restored = turn.restore_completion(event);
                got.push(restored.clone());
                got.push(turn.completed_output(&restored));
                restored
            } else {
                event
            };
            turn.track_pending(&event);
        }
        assert_eq!(got, strings(v, "list"), "events {:?}", v["events"]);
        assert_eq!(turn.pending().join(","), s(v, "out"));
    }
}

/// The Go sequence runs against shared caches: `bypass` marks a committed turn and
/// `events` are upstream events recorded into it first.
#[test]
fn tool_call_repair_sequence_matches_go() {
    let all = vectors("repair");
    let (keyed, unkeyed): (Vec<_>, Vec<_>) = all.into_iter().partition(|v| !s(v, "key").is_empty());
    let key = s(keyed[0], "key");
    let retained = Retained::new(key.to_owned());
    for v in keyed {
        let (out, turn) = prepare_fallback_turn(key, s(v, "raw").to_owned());
        assert_eq!(out, s(v, "out"), "raw {}", s(v, "raw"));
        let mut turn = turn.expect("a session key records the turn");
        for event in strings(v, "events") {
            turn.record_response(&event);
        }
        if v["bypass"].as_bool().unwrap_or(false) {
            turn.commit();
        }
    }
    drop(retained);
    for v in unkeyed {
        assert_eq!(prepare_fallback_turn("", s(v, "raw").to_owned()).0, s(v, "out"));
    }
}

#[test]
fn close_reason_truncation_matches_go() {
    for v in vectors("truncate") {
        let max = v["max"].as_u64().unwrap_or(0) as usize;
        assert_eq!(truncate_reason(s(v, "raw"), max), s(v, "out"));
    }
}
