//! Go's reasoning replay end to end: each scenario in `tests/fixtures/codex_go.json`
//! (`replay`) ran its Claude turns through one Go `CodexExecutor` against a scripted
//! upstream. The same turns through one Rust executor must send the same upstream bodies.

use std::path::Path;

use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{Caller, ExecRequest, Operation, ResponseBody};
use cpa_core::format::Format;
use futures_util::StreamExt;
use http::HeaderMap;
use serde_json::Value;

use crate::codex::CodexExecutor;
use crate::codex_oauth::CodexOAuth;
use crate::codex_testkit::{GO, Mock, Reply};

fn request(scenario: &Value, turn: &Value) -> ExecRequest {
    let mut headers = HeaderMap::new();
    for (k, v) in scenario["headers"].as_object().into_iter().flatten() {
        headers.insert(
            http::HeaderName::try_from(k.as_str()).unwrap(),
            v.as_str().unwrap().parse().unwrap(),
        );
    }
    let body = Bytes::from(turn["payload"].as_str().unwrap().to_owned());
    ExecRequest {
        operation: Operation::Generate,
        source_format: Format::Claude,
        response_format: Format::Claude,
        requested_model: scenario["model"].as_str().unwrap().into(),
        model: scenario["model"].as_str().unwrap().into(),
        original_body: body.clone(),
        body,
        stream: turn["stream"].as_bool().unwrap(),
        alt: None,
        session: None,
        headers,
        execution_session: None,
        derived_session: None,
        request_path: String::new(),
        caller: Caller {
            principal: "client-key-FAKE".into(),
            source: "authorization",
        },
        resolved_model: None,
        usage: Default::default(),
    }
}

#[tokio::test]
async fn replay_scenarios_match_go() {
    let scenarios = GO["replay"].as_array().unwrap();
    assert_eq!(scenarios.len(), 2);
    for scenario in scenarios {
        let name = scenario["name"].as_str().unwrap();
        let mock = Mock::start().await;
        let turns = scenario["turns"].as_array().unwrap();
        mock.script(
            "/responses",
            turns
                .iter()
                .map(|t| {
                    let status = t["upstream_status"].as_u64().unwrap() as u16;
                    let kind = if status == 200 {
                        "text/event-stream"
                    } else {
                        "application/json"
                    };
                    Reply {
                        status,
                        headers: vec![("content-type".into(), kind.into())],
                        body: t["upstream_body"].as_str().unwrap().into(),
                    }
                })
                .collect(),
        );
        let executor = CodexExecutor::with_client(wreq::Client::new(), CodexOAuth::new(wreq::Client::new()));
        let mut credential = Credential::from_file(
            Path::new("/fake"),
            Path::new("/fake/codex-replay.json"),
            serde_json::json!({"type": "codex", "access_token": "at-FAKE", "account_id": "acct-FAKE-1"})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        credential.attributes.insert("base_url".into(), mock.url.clone());
        let cfg = Config::default();
        for (i, turn) in turns.iter().enumerate() {
            let result = executor.execute(&credential, request(scenario, turn), &cfg).await;
            let error = match result {
                Err(error) => Some(error),
                Ok(response) => match response.body {
                    ResponseBody::Stream(mut stream) => {
                        let mut failed = None;
                        while let Some(item) = stream.next().await {
                            if let Err(error) = item {
                                failed = Some(error);
                            }
                        }
                        failed
                    }
                    ResponseBody::Buffered(_) => None,
                },
            };
            let captured = mock.take();
            assert_eq!(captured.len(), 1, "{name} turn {i}: one upstream request");
            assert_eq!(
                String::from_utf8_lossy(&captured[0].body),
                turn["upstream"].as_str().unwrap(),
                "{name} turn {i}: upstream body"
            );
            match (&error, turn["error"].as_object()) {
                (Some(error), Some(go)) => {
                    assert_eq!(error.status as u64, go["status"].as_u64().unwrap(), "{name} turn {i}")
                }
                (None, None) => {}
                (rust, go) => panic!("{name} turn {i}: rust error {rust:?}, go error {go:?}"),
            }
        }
    }
}

/// Go's limits: an entry keeps at most 256 turns, dropping the oldest.
#[test]
fn entries_keep_the_newest_turns() {
    let cache = super::Cache::default();
    let call = |i: usize| format!(r#"{{"type":"function_call","call_id":"c{i}","name":"f","arguments":"{{}}"}}"#);
    for i in 0..300 {
        let marker = format!(r#"{{"type":"cpa_codex_replay_turn","id":"t{i}"}}"#);
        assert!(cache.append("m", "s", &[marker.into_bytes(), call(i).into_bytes()]));
    }
    // A repeated turn id is not stored twice.
    let again = br#"{"type":"cpa_codex_replay_turn","id":"t299"}"#.to_vec();
    cache.append("m", "s", &[again, call(299).into_bytes()]);
    let items = cache.get("m", "s").unwrap();
    assert_eq!(items.len(), 512);
    assert_eq!(items[0], br#"{"type":"cpa_codex_replay_turn","id":"t44"}"#);
    assert!(cache.get("m", "other").is_none());
}

/// Go's Home KV backend for Codex replay, recorded by the reference's
/// internal/cache/zz_rustgolden_test.go: base64 item arrays appended by CAS against the
/// value read, known turns not appended twice, TTL renewal on read and deletion.
#[tokio::test]
async fn home_kv_replay_matches_go() {
    use std::sync::{Arc, Mutex};
    let golden: serde_json::Value =
        serde_json::from_str(include_str!("claude/testdata/go_codex_replay_home.json")).unwrap();
    let steps = golden["steps"].as_array().unwrap();
    let values = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let home = cpa_home::fake::FakeHome::start(crate::claude::kv_test::kv_home(values)).await;
    let client = home.client();
    let turn = |id: &str, call: &str| -> Vec<Vec<u8>> {
        vec![
            format!(r#"{{"type":"cpa_codex_replay_turn","id":" {id} ","call_ids":["{call}"],"extra":1}}"#).into_bytes(),
            format!(r#"{{"type":"function_call","id":"fc","call_id":"{call}","name":"f","arguments":"{{\"a\":\"<b>\"}}","status":"completed"}}"#).into_bytes(),
            br#"{"type":"message","role":"assistant"}"#.to_vec(),
        ]
    };
    let mut seen = 0;
    let mut calls = || {
        let all: Vec<serde_json::Value> = home
            .commands()
            .iter()
            .filter_map(|c| crate::claude::kv_test::as_go_call(c))
            .collect();
        let new = all[seen..].to_vec();
        seen = all.len();
        serde_json::Value::from(new)
    };
    let texts = |items: Option<Vec<Vec<u8>>>| -> serde_json::Value {
        items
            .unwrap_or_default()
            .into_iter()
            .map(|i| String::from_utf8(i).unwrap())
            .collect()
    };
    assert!(super::home_append(&client, " gpt-5 ", " sess-1 ", &turn("m1", "c1")).await);
    assert_eq!(calls(), steps[0]["calls"], "append_first");
    let items = super::home_get(&client, "gpt-5", "sess-1").await.unwrap();
    assert_eq!(texts(items), steps[1]["result"]["items"], "get_first");
    assert_eq!(calls(), steps[1]["calls"], "get_first");
    assert!(super::home_append(&client, "gpt-5", "sess-1", &turn("m2", "c2")).await);
    assert_eq!(calls(), steps[2]["calls"], "append_second");
    assert!(super::home_append(&client, "gpt-5", "sess-1", &turn("m1", "c1")).await);
    assert_eq!(calls(), steps[3]["calls"], "append_known");
    let items = super::home_get(&client, "gpt-5", "sess-1").await.unwrap();
    assert_eq!(texts(items), steps[4]["result"]["items"], "get_both");
    assert_eq!(calls(), steps[4]["calls"], "get_both");
    assert!(!super::home_append(&client, "gpt-5", "sess-1", &[br#"{"type":"message"}"#.to_vec()]).await);
    assert_eq!(calls(), steps[5]["calls"], "append_nothing");
    super::home_delete(&client, "gpt-5", "sess-1").await.unwrap();
    assert_eq!(calls(), steps[6]["calls"], "delete");
    assert!(super::home_get(&client, "gpt-5", "sess-1").await.unwrap().is_none());
    assert_eq!(calls(), steps[7]["calls"], "get_missing");
}
