//! Go's own Claude unit tests, replayed. tests/reference/claude/unit/record.py runs the
//! Go test files of CLIProxyAPI 6fecc6e with recording wrappers around the functions
//! they test; every recorded call (inputs and Go's outputs) is replayed here through
//! the Rust port. One test per Go file, named after its PARITY.md item; a failure lists
//! every diverging call with the Go test that made it.

use std::sync::OnceLock;

use serde_json::Value;

use super::{alias, stream};

fn fixture() -> &'static Value {
    static FIXTURE: OnceLock<Value> = OnceLock::new();
    FIXTURE.get_or_init(|| serde_json::from_str(include_str!("testdata/go_unit.json")).unwrap())
}

fn bytes(value: &Value) -> Option<Vec<u8>> {
    use base64::Engine;
    match value {
        Value::String(s) => Some(s.as_bytes().to_vec()),
        Value::Object(o) => o
            .get("b64")
            .and_then(Value::as_str)
            .map(|b| base64::engine::general_purpose::STANDARD.decode(b).unwrap()),
        _ => None,
    }
}

fn text(value: &Value) -> String {
    String::from_utf8(bytes(value).unwrap_or_default()).unwrap()
}

fn reverse(value: &Value) -> alias::Reverse {
    value
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
        .collect()
}

/// Replays one recorded call; `Err` describes how Rust differs from Go.
fn replay(record: &Value) -> Result<(), String> {
    let field = |name: &str| &record[name];
    match field("fn").as_str().unwrap() {
        "remap" => {
            let (out, map) = alias::remap(&text(field("body")), field("secret").as_str().unwrap());
            check("body", &out, &text(field("out")))?;
            check("reverse map", &map, &reverse(field("reverse")))
        }
        // Go's batched half alone: it refuses malformed JSON (the legacy half then runs,
        // recorded as "remap"); otherwise it equals the full remap.
        "remap_batched" => {
            let body = text(field("body"));
            if !field("ok").as_bool().unwrap() {
                return check("valid JSON", &cpa_common::json::valid(body.as_bytes()), &false);
            }
            let (out, map) = alias::remap(&body, field("secret").as_str().unwrap());
            check("body", &out, &text(field("out")))?;
            check("reverse map", &map, &reverse(field("reverse")))
        }
        "restore" => {
            let got = alias::restore_response(&text(field("body")), &reverse(field("reverse")));
            match field("error").as_str() {
                Some(error) => check("error", &got.err(), &Some(error.to_owned())),
                None => check("body", &got?, &text(field("out"))),
            }
        }
        "restore_line" => {
            let got = stream::restore_line(&bytes(field("line")).unwrap(), &reverse(field("reverse")));
            match field("error").as_str() {
                Some(error) => check("error", &got.err(), &Some(error.to_owned())),
                None => check("line", &String::from_utf8(got?).unwrap(), &text(field("out"))),
            }
        }
        "parse_alias" => {
            let name = field("name").as_str().unwrap();
            let want = field("ok").as_bool().unwrap().then(|| {
                (
                    field("server").as_str().unwrap(),
                    field("tool_id").as_str().unwrap(),
                    field("semantic").as_str().unwrap(),
                )
            });
            check("parts", &alias::alias_parts(name), &want)
        }
        other => Err(format!("no Rust replay for recorded function {other:?}")),
    }
}

fn check<T: PartialEq + std::fmt::Debug>(what: &str, got: &T, want: &T) -> Result<(), String> {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}:\n   rust: {got:?}\n     go: {want:?}"))
    }
}

/// Replays every call recorded from one Go test file; panics listing all divergences.
fn replay_file(file: &str) {
    let tests = fixture()["files"][file]
        .as_object()
        .unwrap_or_else(|| panic!("no records for {file}"));
    let mut failures = Vec::new();
    let mut count = 0;
    for (test, records) in tests {
        for (i, record) in records.as_array().unwrap().iter().enumerate() {
            count += 1;
            if let Err(why) = replay(record) {
                failures.push(format!("{test} call {i} ({}): {why}", record["fn"]));
            }
        }
    }
    assert!(count > 0, "{file}: nothing recorded");
    assert!(
        failures.is_empty(),
        "{file}: {} of {count} Go calls differ\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn go_assertions_held_while_recording() {
    assert_eq!(fixture()["go_failures"], serde_json::json!([]));
}

/// M1-0083: claude_executor_request_remap_test.go (MCP aliasing, mangled-alias
/// recovery, malformed-JSON fallback).
#[test]
fn m1_0083_request_remap() {
    replay_file("claude_executor_request_remap_test.go");
}

/// M1-0086: claude_executor_test.go (the calls recorded so far).
#[test]
fn m1_0086_executor() {
    replay_file("claude_executor_test.go");
}
