//! Replays tests/fixtures/codex_client_go.json, written by Go's own functions at 6fecc6e
//! (tests/reference/codex_client/README.md).

use super::*;
use serde_json::Value;

fn vectors() -> Vec<Value> {
    serde_json::from_str(include_str!("../tests/fixtures/codex_client_go.json")).unwrap()
}

fn headers(v: &Value) -> HeaderMap {
    let mut out = HeaderMap::new();
    if let Some(map) = v["headers"].as_object() {
        for (k, val) in map {
            out.insert(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                val.as_str().unwrap().parse().unwrap(),
            );
        }
    }
    out
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or_default()
}

fn flag(v: &Value, key: &str) -> bool {
    v[key].as_bool().unwrap_or(false)
}

#[test]
fn replays_every_go_vector() {
    let vectors = vectors();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for v in &vectors {
        let f = s(v, "fn");
        *counts.entry(f.to_owned()).or_default() += 1;
        let input = s(v, "in").as_bytes();
        let want = s(v, "out");
        let ctx = format!("{f}: {v}");
        match f {
            "orphan" => {
                let got = rewrite_orphan_delegation_input(&headers(v), input, flag(v, "flag"));
                assert_eq!(String::from_utf8(got).unwrap(), want, "{ctx}");
            }
            "agent_input" => {
                let settings = Settings {
                    optimize_multi_agent_v2: flag(v, "flag"),
                    orphan_delegation: false,
                };
                let got = rewrite_multi_agent_v2_input(&headers(v), input, &settings, flag(v, "compat"));
                assert_eq!(String::from_utf8(got).unwrap(), want, "{ctx}");
            }
            "spawn_models" => {
                let available: Vec<AvailableModel> = v["models"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| AvailableModel {
                        id: s(m, "id").to_owned(),
                        display_name: s(m, "display_name").to_owned(),
                        description: s(m, "description").to_owned(),
                    })
                    .collect();
                assert_eq!(spawn_agent_models(&available, &registry_facts), want, "{ctx}");
            }
            "replace_models" => {
                assert_eq!(replace_spawn_agent_models(s(v, "in"), s(v, "markdown")), want, "{ctx}");
            }
            "prepare_tools" => {
                let (got, eligible) =
                    prepare_tools(&headers(v), input, flag(v, "flag"), || s(v, "markdown").to_owned());
                assert_eq!(String::from_utf8(got).unwrap(), want, "{ctx}");
                assert_eq!(eligible, flag(v, "bool"), "{ctx}");
            }
            "optimize" => {
                let settings = Settings {
                    optimize_multi_agent_v2: flag(v, "flag"),
                    orphan_delegation: false,
                };
                let (got, renamed) =
                    optimize_request(&headers(v), input, &settings, false, || s(v, "markdown").to_owned());
                assert_eq!(String::from_utf8(got).unwrap(), want, "{ctx}");
                assert_eq!(renamed, flag(v, "bool"), "{ctx}");
            }
            "conflict" => assert_eq!(has_namespace_conflict(input), flag(v, "bool"), "{ctx}"),
            "restore" => {
                let got = restore_response(input, flag(v, "flag"));
                assert_eq!(String::from_utf8(got).unwrap(), want, "{ctx}");
            }
            "client_ua" => assert_eq!(is_codex_client_user_agent(s(v, "in")), flag(v, "bool"), "{ctx}"),
            other => panic!("unknown vector fn {other}"),
        }
    }
    let expected = [
        ("client_ua", 9),
        ("replace_models", 8),
        ("restore", 7),
        ("orphan", 6),
        ("agent_input", 6),
        ("prepare_tools", 5),
        ("optimize", 5),
        ("conflict", 5),
        ("spawn_models", 1),
    ];
    for (f, n) in expected {
        assert_eq!(counts.get(f).copied().unwrap_or(0), n, "vector count for {f}");
    }
}

/// Go `strings.EqualFold`: the long s folds to `s`, so a Unicode-folded header still
/// marks a collab_spawn subagent.
#[test]
fn subagent_header_folds_like_go() {
    let mut h = HeaderMap::new();
    h.insert("x-openai-subagent", "collab_\u{17f}pawn".parse().unwrap());
    let input = br#"{"input":[{"type":"function_call_output","call_id":"x","name":"create_thread","namespace":"codex_app","output":"o"}]}"#;
    let got = rewrite_orphan_delegation_input(&h, input, true);
    assert_ne!(got, input.to_vec());
    h.insert("x-openai-subagent", "collab_spawnx".parse().unwrap());
    assert_eq!(rewrite_orphan_delegation_input(&h, input, true), input.to_vec());
}

/// Tools already prepared at the Responses boundary only lose `message.encrypted`; the
/// description keeps whatever the boundary wrote and the model list is not rebuilt.
#[test]
fn prepared_tools_skip_the_model_list() {
    let mut h = HeaderMap::new();
    h.insert("user-agent", "codex_cli_rs/0.150.0".parse().unwrap());
    let input = br#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"properties":{"message":{"encrypted":true}}}}]}]}"#;
    let settings = Settings {
        optimize_multi_agent_v2: true,
        orphan_delegation: false,
    };
    let (got, renamed) = optimize_request(&h, input, &settings, true, || panic!("model list rebuilt"));
    assert!(renamed);
    assert_eq!(
        String::from_utf8(got).unwrap(),
        r#"{"tools":[{"type":"namespace","name":"collaboration-optimize","tools":[{"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"properties":{"message":{}}}}]}]}"#
    );
}

#[test]
fn settings_read_go_config_paths() {
    let cfg = Config::parse(
        "client:\n  codex:\n    optimize-multi-agent-v2: true\noauth:\n  providers:\n    codex:\n      orphan-delegation-compatibility: true\n",
    )
    .unwrap();
    assert_eq!(
        Settings::from_config(&cfg),
        Settings {
            optimize_multi_agent_v2: true,
            orphan_delegation: true
        }
    );
    assert_eq!(Settings::from_config(&Config::default()), Settings::default());
}

#[test]
fn normalizes_missing_or_null_instructions() {
    for (input, want) in [
        (r#"{"model":"m"}"#, r#"{"model":"m","instructions":""}"#),
        (r#"{"instructions":null}"#, r#"{"instructions":""}"#),
        (r#"{"instructions":"keep"}"#, r#"{"instructions":"keep"}"#),
    ] {
        let mut body = input.as_bytes().to_vec();
        normalize_codex_instructions(&mut body);
        assert_eq!(String::from_utf8(body).unwrap(), want);
    }
}
