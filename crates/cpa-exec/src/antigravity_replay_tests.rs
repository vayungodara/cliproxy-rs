//! Differential tests against `tests/fixtures/antigravity_replay_go.json`: every call
//! Go's own reasoning replay tests (and their randomized differentials) make to the
//! replay entry points, recorded with Go's results (tests/reference/antigravity_replay).
//! Expected values come only from Go.

use bytes::Bytes;
use cpa_core::exec::{Caller, Operation};
use serde_json::Value;

use super::*;

fn fixture() -> &'static Value {
    static PARSED: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    PARSED.get_or_init(|| serde_json::from_str(include_str!("../tests/fixtures/antigravity_replay_go.json")).unwrap())
}

fn records(kind: &str) -> &'static [Value] {
    fixture()[kind].as_array().unwrap()
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

fn strings(v: &Value) -> Vec<Vec<u8>> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|item| s(item).as_bytes().to_vec())
        .collect()
}

/// The ExecRequest a Go (req, opts) pair maps to: `body` is `req.Payload`,
/// `original_body` is `opts.OriginalRequest`.
fn request(r: &Value) -> ExecRequest {
    let mut headers = HeaderMap::new();
    for (name, values) in r["headers"].as_object().into_iter().flatten() {
        let name = http::HeaderName::from_bytes(name.as_bytes()).unwrap();
        for value in values.as_array().unwrap() {
            headers.append(name.clone(), s(value).parse().unwrap());
        }
    }
    let optional = |v: &Value| Some(s(v).to_owned()).filter(|v| !v.is_empty());
    ExecRequest {
        operation: Operation::Generate,
        source_format: Format::parse(s(&r["source"])).unwrap_or(Format::Gemini),
        response_format: Format::Gemini,
        stream_payloads: false,
        requested_model: s(&r["model"]).to_owned(),
        model: s(&r["model"]).to_owned(),
        original_body: Bytes::from(s(&r["original"]).to_owned()),
        body: Bytes::from(s(&r["body"]).to_owned()),
        stream: false,
        alt: None,
        session: None,
        execution_session: optional(&r["execution"]),
        derived_session: optional(&r["derived"]),
        resolved_model: None,
        compat_replay: None,
        usage: Default::default(),
        request_path: String::new(),
        headers,
        caller: Caller {
            principal: String::new(),
            source: "authorization",
            provider: String::new(),
        },
    }
}

/// Go falls back to a random session ID for a payload without a session ID or user text.
fn random_session(payload: &[u8]) -> bool {
    let contents = gj::get(payload, "request.contents");
    let explicit = ["sessionId", "session_id", "request.sessionId", "request.session_id"]
        .iter()
        .any(|path| !text(&gj::get(payload, path)).is_empty());
    !explicit
        && (!contents.is_array()
            || !contents
                .array()
                .iter()
                .any(|c| *c.get("role").bytes() == *b"user" && !c.get("parts.0.text").bytes().is_empty()))
}

#[test]
fn scopes_match_go() {
    let mut ran = 0;
    for r in records("scope") {
        let req = request(&r["request"]);
        let payload = s(&r["request"]["payload"]).as_bytes();
        let got = scope_from_request(&req, s(&r["request"]["model"]), payload).session;
        let want = s(&r["session"]);
        if got != want
            && want.starts_with("session:")
            && got.starts_with("session:")
            && random_session(payload)
            && random_session(&req.body)
        {
            continue;
        }
        assert_eq!(got, want, "{}: session key", s(&r["test"]));
        ran += 1;
    }
    assert!(ran >= 60, "fixture lost scope cases: {ran}");
}

#[test]
fn apply_matches_go() {
    let (mut ran, mut changed) = (0, 0);
    for r in records("apply") {
        let payload = s(&r["payload"]).as_bytes();
        let schemas: Schemas = r["schemas"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, schema)| {
                let value = GoValue::parse_f64(serde_json::to_string(schema).unwrap().as_bytes()).unwrap();
                (name.as_bytes().to_vec(), value)
            })
            .collect();
        let got = apply_items(payload, &strings(&r["items"]), &schemas);
        let want_changed = r["changed"].as_bool().unwrap();
        let test = s(&r["test"]);
        assert_eq!(got.is_some(), want_changed, "{test}: changed");
        let got = got.unwrap_or_else(|| payload.to_vec());
        assert_eq!(String::from_utf8_lossy(&got), s(&r["out"]), "{test}: payload");
        ran += 1;
        changed += usize::from(want_changed);
    }
    assert!(
        ran >= 400 && changed >= 100,
        "fixture lost apply cases: {ran} ({changed} changed)"
    );
}

#[test]
fn prepare_matches_go() {
    let mut ran = 0;
    for r in records("prepare") {
        let test = s(&r["test"]);
        let model = s(&r["request"]["model"]);
        let session = s(&r["session"]);
        if !model.trim().is_empty() && !session.trim().is_empty() {
            cache::replay_delete_if_unchanged(model, session, &ReplaySnapshot::default());
            let ledger = strings(&r["ledger"]);
            if !ledger.is_empty() {
                assert!(cache::replay_put(model, session, &ledger), "{test}: seed ledger");
            }
        }
        let req = request(&r["request"]);
        let payload = s(&r["request"]["payload"]).as_bytes().to_vec();
        match prepare(&req, model, payload) {
            Ok((out, scope)) => {
                assert_eq!(s(&r["error"]), "", "{test}: Go failed");
                assert_eq!(String::from_utf8_lossy(&out), s(&r["out"]), "{test}: payload");
                assert_eq!(scope.session, session, "{test}: session");
            }
            Err(error) => assert_eq!(error, s(&r["error"]), "{test}: error"),
        }
        ran += 1;
    }
    assert!(ran >= 55, "fixture lost prepare cases: {ran}");
}

#[test]
fn accumulator_matches_go() {
    let scope = Scope {
        model: "gemini-3-flash".into(),
        session: "accumulator-test".into(),
        snapshot: ReplaySnapshot::default(),
    };
    let mut ran = 0;
    for r in records("accumulator") {
        let test = s(&r["test"]);
        let mut acc = Accumulator::new(&scope, s(&r["request"]).as_bytes()).unwrap();
        for payload in r["observed"].as_array().unwrap() {
            acc.observe(s(payload).as_bytes());
        }
        if !acc.overflow {
            acc.flush_all();
        }
        assert_eq!(acc.overflow, r["overflow"].as_bool().unwrap(), "{test}: overflow");
        assert_eq!(acc.terminal, r["terminal"].as_bool().unwrap(), "{test}: terminal");
        let items: Vec<String> = acc
            .items
            .iter()
            .map(|i| String::from_utf8_lossy(i).into_owned())
            .collect();
        let want: Vec<String> = strings(&r["items"])
            .into_iter()
            .map(|i| String::from_utf8(i).unwrap())
            .collect();
        assert_eq!(items, want, "{test}: items");
        ran += 1;
    }
    assert!(ran >= 25, "fixture lost accumulator cases: {ran}");
}

#[test]
fn invalid_signature_400_clears_the_entry_it_read() {
    // TestAntigravityReasoningReplayClearsOnInvalidSignature400, on the in-process store.
    let (model, session) = ("gemini-3-flash", "responses:clear-on-400");
    let item =
        br#"{"type":"thought_signature","thoughtSignature":"sig-aaaaaaaaaaaaaaaa","contentIndex":1,"partIndex":0}"#;
    let read = |snapshot: &mut ReplaySnapshot| {
        let (items, next) = cache::replay_get_with_snapshot(model, session);
        *snapshot = next;
        items
    };
    for (status, body, cleared) in [
        (429, &br#"{"error":"thought_signature"}"#[..], false),
        (400, &br#"{"error":"bad request"}"#[..], false),
        (400, &br#"{"error":{"message":"Invalid ThoughtSignature"}}"#[..], true),
    ] {
        assert!(cache::replay_put(model, session, &[item.to_vec()]));
        let mut snapshot = ReplaySnapshot::default();
        assert!(read(&mut snapshot).is_some());
        let scope = Scope {
            model: model.into(),
            session: session.into(),
            snapshot,
        };
        clear_on_invalid_signature(&scope, status, body);
        assert_eq!(
            read(&mut ReplaySnapshot::default()).is_none(),
            cleared,
            "{status} {body:?}"
        );
    }
}

/// A system prompt at encoding/json's depth limit hashes on a 2 MiB stack, with its
/// `cache_control` markers removed at every level.
#[test]
fn system_lane_handles_the_depth_limit() {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(|| {
            let deep = |level: &str| format!(r#"{{"system":{}1{}}}"#, level.repeat(4_999), "}]".repeat(4_999));
            let marked = system_lane(deep(r#"[{"cache_control":{"type":"ephemeral"},"t":"#).as_bytes());
            let plain = system_lane(deep(r#"[{"t":"#).as_bytes());
            assert_eq!(marked.len(), 32);
            assert_eq!(marked, plain);
            assert_ne!(plain, system_lane(deep(r#"[{"u":"#).as_bytes()));
        })
        .unwrap()
        .join()
        .unwrap();
}
