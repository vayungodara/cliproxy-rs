//! Client tests against the fake Home. Expected values come from
//! `tests/fixtures/go_home_golden.json`, produced by running Go's internal/home code
//! (see `tests/fixtures/README.md`).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value as Json;
use tokio_util::sync::CancellationToken;

use crate::client::{Client, DispatchRequest, Recovery, ReleaseFrame, SetOptions, parse_cluster_nodes, set_args};
use crate::config::{CredentialConcurrency, HomeConfig};
use crate::error::Error;
use crate::fake::{self, FakeHome, Reply};

fn golden() -> Json {
    serde_json::from_str(include_str!("../tests/fixtures/go_home_golden.json")).unwrap()
}

fn golden_case<'a>(doc: &'a Json, section: &str, name: &str) -> &'a Json {
    doc[section]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("golden {section}/{name}"))
}

fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// Go `TestAuthDispatchRequestIncludesCount`, `…DefaultsCountToOne`,
/// `…IncludesCredentialPolicy`, `…IncludesExcludedAuthIDs`,
/// `…IncludesEmptyExcludedAuthIDs`, `…IncludesPinnedAuthID`,
/// `…DistinguishesLegacyAndRetryRoundProtocol`, `…IncludesParentSessionID` and
/// `…IncludesNodeKind`, as Go's exact request bytes.
#[test]
fn dispatch_request_json_matches_go_bytes() {
    let doc = golden();
    let excluded = Some(vec!["auth-a".to_owned(), "auth-b".to_owned()]);
    let cases: Vec<(&str, DispatchRequest)> = vec![
        (
            "basic",
            DispatchRequest {
                model: "gpt-5.4".into(),
                session_id: " s1 ".into(),
                count: 2,
                ..Default::default()
            },
        ),
        (
            "zero_count",
            DispatchRequest {
                model: "gpt-5.4".into(),
                ..Default::default()
            },
        ),
        (
            "headers",
            DispatchRequest {
                model: "m<x>&".into(),
                session_id: "s".into(),
                parent_session_id: " parent ".into(),
                headers: headers(&[
                    ("Authorization", " Bearer x "),
                    ("X-Multi", "a"),
                    ("X-Multi", " b"),
                    ("X-Node-Kind", "subagent"),
                    ("X-Empty", ""),
                ]),
                count: 1,
                credential_policy: " codex_alpha_search_v1 ".into(),
                pinned_auth_id: " pin ".into(),
                ..Default::default()
            },
        ),
        (
            "excluded",
            DispatchRequest {
                model: "gpt".into(),
                count: 3,
                excluded_auth_ids: excluded.clone(),
                ..Default::default()
            },
        ),
        (
            "excluded_empty",
            DispatchRequest {
                model: "gpt".into(),
                count: 3,
                excluded_auth_ids: Some(Vec::new()),
                ..Default::default()
            },
        ),
        (
            "retry_round",
            DispatchRequest {
                model: "gpt".into(),
                count: 1,
                retry_round: Some(2),
                excluded_auth_ids: excluded,
                pinned_auth_id: "pin".into(),
                ..Default::default()
            },
        ),
        (
            "retry_round_negative",
            DispatchRequest {
                model: "gpt".into(),
                count: 1,
                retry_round: Some(-5),
                ..Default::default()
            },
        ),
        // Go's "lower_node_kind" case uses a non-canonical http.Header key, which a
        // case-insensitive header map cannot express; it is not ported.
    ];
    for (name, request) in cases {
        let want = golden_case(&doc, "dispatch_requests", name)["json"].as_str().unwrap();
        assert_eq!(String::from_utf8(request.to_json()).unwrap(), want, "{name}");
    }
}

/// Go `TestBuildKVSetArgs`.
#[test]
fn kv_set_args_match_go() {
    let doc = golden();
    let ms = Duration::from_millis;
    let cases = [
        ("plain", "k", SetOptions::default()),
        (
            "ex_ceil",
            "k",
            SetOptions {
                ex: ms(1500),
                ..Default::default()
            },
        ),
        (
            "px_ceil",
            "k",
            SetOptions {
                px: Duration::from_micros(1500),
                ..Default::default()
            },
        ),
        (
            "nx_xx",
            "k",
            SetOptions {
                nx: true,
                xx: true,
                ..Default::default()
            },
        ),
        (
            "ex_px",
            "k",
            SetOptions {
                ex: ms(1000),
                px: ms(1000),
                ..Default::default()
            },
        ),
        ("empty_key", "  ", SetOptions::default()),
        (
            "ex_nx",
            " k ",
            SetOptions {
                ex: ms(2000),
                nx: true,
                ..Default::default()
            },
        ),
        (
            "px_xx",
            "k",
            SetOptions {
                px: ms(250),
                xx: true,
                ..Default::default()
            },
        ),
        // Go's "negative" case has no unsigned Duration equivalent.
    ];
    for (name, key, opts) in cases {
        let want = golden_case(&doc, "kv_set_args", name);
        match set_args(key, b"v", opts) {
            Ok(args) => {
                let got: Vec<String> = args.iter().map(|a| String::from_utf8_lossy(a).into_owned()).collect();
                let want: Vec<String> = serde_json::from_value(want["args"].clone()).unwrap();
                assert_eq!(got, want, "{name}");
            }
            Err(error) => assert_eq!(error.to_string(), want["error"].as_str().unwrap(), "{name}"),
        }
    }
}

#[test]
fn jwt_claims_match_go() {
    let doc = golden();
    for case in doc["jwt"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let got = crate::cert::parse_claims(case["raw"].as_str().unwrap());
        match (&case["error"], got) {
            (Json::Null, Ok(claims)) => {
                assert_eq!(serde_json::to_value(&claims).unwrap(), case["claims"], "{name}");
            }
            (Json::String(want), Err(error)) => {
                // Decoder messages differ between Go and serde; validation ones must not.
                if want.starts_with("home jwt") {
                    assert_eq!(&error.to_string(), want, "{name}");
                }
            }
            (want, got) => panic!("{name}: want {want:?}, got {got:?}"),
        }
    }
    let normalized = &doc["fingerprint_normalize"];
    assert_eq!(crate::cert::normalize_fingerprint(" AB:CD ef 01 "), normalized[0]);
    assert_eq!(crate::cert::normalize_fingerprint(""), normalized[1]);
}

#[test]
fn cluster_nodes_sort_like_go() {
    let doc = golden();
    let nodes = parse_cluster_nodes(
        br#"{"ok":true,"nodes":[{"ip":"10.0.0.3","port":1,"client_count":5},{"ip":" ","port":2,"client_count":0},{"ip":"10.0.0.1","port":0},{"ip":" 10.0.0.2 ","port":3,"client_count":-4,"is_master":true},{"ip":"10.0.0.4","port":4,"client_count":5}]}"#,
    )
    .unwrap();
    let want: Vec<(String, i64, i64, bool)> = doc["cluster_nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            (
                n["ip"].as_str().unwrap().to_owned(),
                n["port"].as_i64().unwrap(),
                n["client_count"].as_i64().unwrap(),
                n["is_master"].as_bool().unwrap(),
            )
        })
        .collect();
    let got: Vec<(String, i64, i64, bool)> = nodes
        .into_iter()
        .map(|n| (n.ip, n.port, n.client_count, n.is_master))
        .collect();
    assert_eq!(got, want);
}

/// Go `TestRunConfigSubscriberLifetimeUsesLegacySubscribeWithoutLifecycleConfig` (the
/// arguments per recovery state, as Go's exact values).
#[test]
fn subscription_parameters_match_go_per_recovery_state() {
    let doc = golden();
    for case in doc["subscription_parameters"].as_array().unwrap() {
        let client = Client::with_options(
            HomeConfig {
                enabled: true,
                ..Default::default()
            },
            Duration::from_secs(3),
            "11111111-2222-3333-4444-555555555555".into(),
        );
        if case["legacy"].as_bool().unwrap() {
            client.enable_legacy_membership();
        }
        client.set_recovery(match case["state"].as_u64().unwrap() {
            0 => Recovery::Stable,
            1 => Recovery::TakeoverEligible,
            2 => Recovery::Switching,
            _ => Recovery::SwitchingTakeover,
        });
        let revision = case["revision"].as_i64().unwrap();
        if revision > 0 {
            client
                .set_lifecycle_config(CredentialConcurrency {
                    lifecycle_config_revision: revision,
                    cpa_heartbeat_timeout: 9_000_000_000,
                    ..Default::default()
                })
                .unwrap();
        }
        let (args, timeout) = client.subscription_parameters();
        let want: Vec<String> = serde_json::from_value(case["args"].clone()).unwrap();
        assert_eq!(args, want, "{case}");
        assert_eq!(
            timeout.as_millis() as i64,
            case["timeout_ms"].as_i64().unwrap(),
            "{case}"
        );
    }
}

/// Runs one operation against a fake answering like Go's golden harness.
async fn run_op(name: &str) -> (Vec<Vec<String>>, Result<Json, String>) {
    let reply = |args: &[String]| -> Reply {
        let first = args[0].to_lowercase();
        match first.as_str() {
            "ping" => fake::raw("+PONG\r\n"),
            _ => fake::raw("+OK\r\n"),
        }
    };
    let responses: BTreeMap<&str, &str> = [
        ("get_config", "$8\r\nport: 1\n\r\n"),
        ("get_config_missing", "$-1\r\n"),
        ("get_config_empty", "$0\r\n\r\n"),
        ("kv_get", "$1\r\nv\r\n"),
        ("kv_get_miss", "$-1\r\n"),
        ("kv_set", "+OK\r\n"),
        ("kv_set_unmet", "$-1\r\n"),
        ("kv_setnx", "+OK\r\n"),
        ("kv_setnx_nottl", "+OK\r\n"),
        ("kv_cas", ":1\r\n"),
        ("kv_cas_nottl", ":0\r\n"),
        ("kv_cas_unsupported", "-ERR unknown command 'CAS'\r\n"),
        ("kv_del", ":2\r\n"),
        ("kv_expire", ":1\r\n"),
        ("kv_expire_subsecond", ":1\r\n"),
        ("kv_incrby", ":7\r\n"),
        ("kv_mget", "*3\r\n$1\r\nx\r\n$-1\r\n$0\r\n\r\n"),
        ("kv_mset", "+OK\r\n"),
        ("lpush_usage", ":1\r\n"),
        ("lpush_usage_empty", ":1\r\n"),
        ("lpush_inflight", ":1\r\n"),
        ("rpush_request_log", ":1\r\n"),
        ("rpush_app_log", ":1\r\n"),
        ("rpush_plugin_status", ":1\r\n"),
        ("get_refresh_auth", "$10\r\n{\"id\":\"a\"}\r\n"),
        ("get_refresh_auth_missing", "$-1\r\n"),
        ("get_models", "$11\r\n{\"data\":[]}\r\n"),
        ("rpop_auth", "$19\r\n{\"auth\":{\"id\":\"a\"}}\r\n"),
        ("rpop_auth_not_found", "$-1\r\n"),
        ("rpop_auth_server_error", "-ERR dispatch denied\r\n"),
        ("concurrency_release", ":1\r\n"),
    ]
    .into_iter()
    .collect();
    let tasks = r#"[{"id":3,"operation":"install","plugin_id":"p","created_at":"2026-01-02T03:04:05Z","updated_at":"2026-01-02T03:04:05Z"}]"#;
    let response = match name {
        "get_plugin_tasks" => format!("${}\r\n{tasks}\r\n", tasks.len()),
        _ => responses[name].to_owned(),
    };
    let home = FakeHome::start(move |args| {
        if args[0].eq_ignore_ascii_case("ping") {
            return reply(args);
        }
        fake::raw(&response)
    })
    .await;
    let client = home.client();
    let text = |b: Vec<u8>| Json::String(String::from_utf8(b).unwrap());
    let result: Result<Json, Error> = match name {
        n if n.starts_with("get_config") => client.get_config().await.map(text),
        "kv_get" | "kv_get_miss" => client.kv_get("cpa:k").await.map(|v| {
            let found = v.is_some();
            serde_json::json!([String::from_utf8(v.unwrap_or_default()).unwrap(), found])
        }),
        "kv_set" => client
            .kv_set(
                "k",
                b"v\x00",
                SetOptions {
                    ex: Duration::from_millis(1500),
                    nx: true,
                    ..Default::default()
                },
            )
            .await
            .map(Json::Bool),
        "kv_set_unmet" => client
            .kv_set(
                "k",
                b"v",
                SetOptions {
                    xx: true,
                    ..Default::default()
                },
            )
            .await
            .map(Json::Bool),
        "kv_setnx" => client
            .kv_set_nx("k", b"v", Duration::from_secs(2))
            .await
            .map(Json::Bool),
        "kv_setnx_nottl" => client.kv_set_nx("k", b"v", Duration::ZERO).await.map(Json::Bool),
        "kv_cas" => client
            .kv_compare_and_swap("k", Some(b"old"), b"new", Duration::from_micros(1500))
            .await
            .map(Json::Bool),
        "kv_cas_nottl" => client
            .kv_compare_and_swap("k", None, b"new", Duration::ZERO)
            .await
            .map(Json::Bool),
        "kv_cas_unsupported" => {
            let first = client.kv_compare_and_swap("k", None, b"new", Duration::ZERO).await;
            let second = client.kv_compare_and_swap("k", None, b"new", Duration::ZERO).await;
            match (first, second) {
                (Err(first), Err(second)) => Err(Error::Other(format!("{first}|{second}"))),
                other => panic!("{other:?}"),
            }
        }
        "kv_del" => client.kv_del(&["a", "b"]).await.map(Json::from),
        "kv_expire" => client.kv_expire("k", Duration::from_millis(2500)).await.map(Json::Bool),
        "kv_expire_subsecond" => client.kv_expire("k", Duration::from_millis(300)).await.map(Json::Bool),
        "kv_incrby" => client.kv_incr_by("k", -3).await.map(Json::from),
        "kv_mget" => client.kv_mget(&["a", "b", "c"]).await.map(|items| {
            let values: Vec<String> = items
                .iter()
                .map(|v| String::from_utf8(v.clone().unwrap_or_default()).unwrap())
                .collect();
            let found: Vec<bool> = items.iter().map(Option::is_some).collect();
            serde_json::json!([values, found])
        }),
        "kv_mset" => {
            let pairs = BTreeMap::from([("b".to_owned(), b"2".to_vec()), ("a".to_owned(), b"1".to_vec())]);
            client.kv_mset(&pairs).await.map(|()| Json::Null)
        }
        "lpush_usage" => client.lpush_usage(br#"{"u":1}"#).await.map(|()| Json::Null),
        "lpush_usage_empty" => client.lpush_usage(b"").await.map(|()| Json::Null),
        "lpush_inflight" => client
            .lpush_in_flight_snapshot(br#"{"kind":"part"}"#)
            .await
            .map(|()| Json::Null),
        "rpush_request_log" => client.rpush_request_log(b"r").await.map(|()| Json::Null),
        "rpush_app_log" => client.rpush_app_log(b"a").await.map(|()| Json::Null),
        "rpush_plugin_status" => client.rpush_plugin_status(b"p").await.map(|()| Json::Null),
        "get_plugin_tasks" => client.get_plugin_tasks().await.map(|tasks| {
            Json::Array(
                tasks
                    .iter()
                    .map(|t| {
                        serde_json::json!({"id": t.id, "operation": t.operation, "plugin_id": t.plugin_id,
                            "created_at": t.created_at, "updated_at": t.updated_at})
                    })
                    .collect(),
            )
        }),
        "get_refresh_auth" => client.get_refresh_auth(" idx ", " sha ").await.map(text),
        "get_refresh_auth_missing" => client.get_refresh_auth("idx", "").await.map(text),
        "get_models" => {
            let headers = crate::client::lower_map([("User-Agent", "ua")]);
            let query = crate::client::lower_map([("key", "k")]);
            client.get_models(&headers, &query).await.map(text)
        }
        "rpop_auth" => client
            .rpop_auth(&DispatchRequest {
                model: "gpt".into(),
                session_id: "s".into(),
                parent_session_id: "p".into(),
                headers: headers(&[("X-Node-Kind", "k")]),
                count: 2,
                credential_policy: "pol".into(),
                retry_round: Some(2),
                excluded_auth_ids: Some(vec!["x".into()]),
                ..Default::default()
            })
            .await
            .map(text),
        n if n.starts_with("rpop_auth") => client
            .rpop_auth(&DispatchRequest {
                model: "gpt".into(),
                count: 1,
                ..Default::default()
            })
            .await
            .map(text),
        "concurrency_release" => client
            .push_concurrency_release(&ReleaseFrame {
                credential_id: "cred-1".into(),
                model: "gpt".into(),
                release_seq: 3,
            })
            .await
            .map(|()| Json::Null),
        other => panic!("unported golden op {other}"),
    };
    (home.commands(), result.map_err(|e| e.to_string()))
}

/// Go `TestKVGetConvertsRedisNilToMiss`, `TestKVMGetConvertsNilItemsToMiss`,
/// `TestKVSetConditionUnmetReturnsFalse`, `TestKVCompareAndSwapSendsCASCommand`,
/// `TestKVCompareAndSwapOmitsPXWithoutTTL`, `TestKVCompareAndSwapLatchesUnsupportedHome`,
/// `TestKVMSetUsesStableKeyOrder`, `TestRPushPluginStatusUsesPluginStatusKey`,
/// `TestGetPluginTasksUsesPluginTasksKey`,
/// `TestModelsRequestSerializationCarriesCredentials` and the in-flight snapshot's
/// dedicated key (`TestClientLPushInFlightSnapshotUsesDedicatedKeyWithoutChangingHeartbeat`):
/// every client operation sends Go's commands and decodes Go's replies.
#[tokio::test]
async fn every_command_matches_go_on_the_wire() {
    let doc = golden();
    let skipped = ["kv_ttl", "kv_ttl_missing", "kv_ttl_persistent"];
    for case in doc["commands"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        if skipped.contains(&name) {
            continue;
        }
        let (commands, result) = run_op(name).await;
        let want: Vec<Vec<String>> = match &case["commands"] {
            Json::Null => Vec::new(),
            v => serde_json::from_value(v.clone()).unwrap(),
        };
        assert_eq!(commands, want, "{name}");
        match (&case["error"], result) {
            (Json::Null, Ok(got)) => {
                let want = &case["result"];
                if !(want.is_null() && got.is_null()) {
                    assert_eq!(&got, want, "{name}");
                }
            }
            (Json::String(want), Err(got)) if name == "kv_cas_unsupported" => {
                assert_eq!(got, format!("{want}|{want}"), "{name}");
            }
            (Json::String(want), Err(got)) => assert_eq!(&got, want, "{name}"),
            (want, got) => panic!("{name}: want error {want:?}, got {got:?}"),
        }
    }
}

/// Go `TestRPopAuthLeavesCompleteServerErrorDeterministic`.
#[tokio::test]
async fn rpop_server_error_is_deterministic_and_keeps_the_lifetime() {
    let home = FakeHome::start(|args| match args[0].as_str() {
        "ping" => fake::raw("+PONG\r\n"),
        _ => fake::raw("-ERR dispatch denied\r\n"),
    })
    .await;
    let client = home.client();
    client.set_heartbeat(true);
    let error = client.rpop_auth(&request()).await.unwrap_err();
    assert!(!error.is_ambiguous(), "{error:?}");
    assert!(!client.fenced() && client.heartbeat_ok());
}

fn request() -> DispatchRequest {
    DispatchRequest {
        model: "gpt-5.4".into(),
        count: 1,
        ..Default::default()
    }
}

/// Go `TestRPopAuthMarksRequestReadThenCloseAmbiguous`.
#[tokio::test]
async fn rpop_read_then_close_is_ambiguous_and_fences() {
    let home = FakeHome::start(|args| match args[0].as_str() {
        "ping" => fake::raw("+PONG\r\n"),
        _ => Reply::Close,
    })
    .await;
    let client = home.client();
    client.set_heartbeat(true);
    let error = client.rpop_auth(&request()).await.unwrap_err();
    assert!(error.is_ambiguous(), "{error:?}");
    assert!(client.fenced() && client.ambiguous_dispatch() && !client.heartbeat_ok());
    assert_eq!(client.rpop_auth(&request()).await.unwrap_err(), Error::DispatchFenced);
    assert_eq!(client.kv_get("k").await.unwrap_err(), Error::DispatchFenced);
    assert_eq!(home.count("rpop"), 1);
}

/// Go `TestRPopAuthLeavesHELLOSetupInterruptionDeterministic` and
/// `TestRPopAuthLeavesPreSendFailureDeterministic`.
#[tokio::test]
async fn failures_before_rpop_is_sent_are_deterministic() {
    // Setup interruption: the probe connection closes before RPOP.
    let home = FakeHome::start(|_| Reply::Close).await;
    let client = home.client();
    let error = client.rpop_auth(&request()).await.unwrap_err();
    assert!(!error.is_ambiguous() && !client.fenced(), "{error:?}");
    assert_eq!(home.count("rpop"), 0);
    // Local validation.
    let empty = DispatchRequest {
        model: "  ".into(),
        ..request()
    };
    assert_eq!(
        client.rpop_auth(&empty).await.unwrap_err().to_string(),
        "home: requested model is empty"
    );
    // Nothing listening: a dial failure.
    let unreachable = Client::with_options(
        HomeConfig {
            enabled: true,
            host: "127.0.0.1".into(),
            port: 1,
            disable_cluster_discovery: true,
            ..Default::default()
        },
        Duration::from_millis(200),
        "x".into(),
    );
    let error = unreachable.rpop_auth(&request()).await.unwrap_err();
    assert!(!error.is_ambiguous() && !unreachable.fenced(), "{error:?}");
}

/// Go `TestAbortAmbiguousDispatchClosesBlockedRPopWithoutWaitingForResponse`.
#[tokio::test]
async fn abort_releases_a_blocked_rpop_without_waiting_for_home() {
    let home = FakeHome::start(|args| match args[0].as_str() {
        "ping" => fake::raw("+PONG\r\n"),
        _ => Reply::Hang,
    })
    .await;
    // A long timeout so only the abort can end the wait.
    let client = Client::with_options(home.config(), Duration::from_secs(30), "x".into());
    client.set_heartbeat(true);
    let pending = tokio::spawn({
        let client = client.clone();
        async move { client.rpop_auth(&request()).await }
    });
    while home.count("rpop") == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    client.abort_ambiguous_dispatch();
    let error = tokio::time::timeout(Duration::from_secs(1), pending)
        .await
        .expect("blocked RPOP ended")
        .unwrap()
        .unwrap_err();
    assert!(error.is_ambiguous(), "{error:?}");
    assert!(!client.heartbeat_ok());
}

#[tokio::test]
async fn dropping_an_issued_rpop_fences_the_lifetime() {
    let home = FakeHome::start(|args| match args[0].as_str() {
        "ping" => fake::raw("+PONG\r\n"),
        _ => Reply::Hang,
    })
    .await;
    let client = Client::with_options(home.config(), Duration::from_secs(30), "x".into());
    let dispatch = tokio::spawn({
        let client = client.clone();
        async move { client.rpop_auth(&request()).await }
    });
    while home.count("rpop") == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    dispatch.abort();
    let _ = dispatch.await;
    assert!(client.fenced() && client.ambiguous_dispatch());
}

/// Go `TestAbortAmbiguousDispatchFencesConcurrentRPop`.
#[tokio::test]
async fn concurrent_rpops_after_abort_are_all_fenced_without_dialing() {
    let home = FakeHome::start(|_| fake::raw("+PONG\r\n")).await;
    let client = home.client();
    client.abort_ambiguous_dispatch();
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let client = client.clone();
        tasks.push(tokio::spawn(async move { client.rpop_auth(&request()).await }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap_err(), Error::DispatchFenced);
    }
    assert!(home.commands().is_empty());
}

#[tokio::test]
async fn release_waits_for_membership_to_be_stable() {
    // Go TestConcurrencyReleaseDoesNotOpenBeforeMembershipReady.
    let home = FakeHome::start(|_| fake::raw(":1\r\n")).await;
    for state in [
        Recovery::TakeoverEligible,
        Recovery::Switching,
        Recovery::SwitchingTakeover,
    ] {
        let client = home.client();
        client.set_recovery(state);
        let frame = ReleaseFrame {
            credential_id: "cred-a".into(),
            model: "model-a".into(),
            release_seq: 1,
        };
        assert_eq!(
            client.push_concurrency_release(&frame).await.unwrap_err(),
            Error::NotConnected
        );
    }
    assert!(home.commands().is_empty());
    let invalid = ReleaseFrame {
        credential_id: String::new(),
        model: "m".into(),
        release_seq: 1,
    };
    assert_eq!(
        home.client()
            .push_concurrency_release(&invalid)
            .await
            .unwrap_err()
            .to_string(),
        "invalid concurrency release frame"
    );
}

#[tokio::test]
async fn get_config_switches_to_the_least_loaded_cluster_node() {
    let target = FakeHome::start(|args| match args[0].as_str() {
        "CLUSTER" => fake::bulk(r#"{"ok":true,"nodes":[]}"#),
        _ => fake::bulk("port: 9\n"),
    })
    .await;
    let target_port = target.addr.port();
    let seed = FakeHome::start(move |args| match args[0].as_str() {
        "CLUSTER" => fake::bulk(format!(
            r#"{{"ok":true,"nodes":[{{"ip":"127.0.0.2","port":9,"client_count":4}},{{"ip":"127.0.0.1","port":{target_port},"client_count":1}}]}}"#
        )),
        _ => fake::bulk("port: 1\n"),
    })
    .await;
    let mut cfg = seed.config();
    cfg.disable_cluster_discovery = false;
    let client = Client::with_options(cfg, Duration::from_millis(300), "x".into());
    assert_eq!(client.get_config().await.unwrap(), b"port: 9\n");
    assert_eq!(client.addr().unwrap(), format!("127.0.0.1:{target_port}"));
    assert_eq!(client.recovery(), Recovery::Switching);
    assert_eq!(seed.commands(), vec![vec!["CLUSTER".to_owned(), "NODES".to_owned()]]);
    assert_eq!(client.cluster_nodes().len(), 2);
}

/// Go `TestGetConfigContinuesAfterClusterDiscoveryResponseError` and
/// `TestGetConfigSkipsSecondDialAfterClusterTransportFailure`.
#[tokio::test]
async fn cluster_discovery_errors_split_by_transport() {
    // A Home error reply or an unusable payload still fetches the config.
    for reply in ["-ERR cluster command unsupported\r\n", ":1\r\n"] {
        let reply = reply.to_owned();
        let home = FakeHome::start(move |args| match args[0].as_str() {
            "CLUSTER" => fake::raw(&reply),
            _ => fake::bulk("host: 127.0.0.1\n"),
        })
        .await;
        let mut cfg = home.config();
        cfg.disable_cluster_discovery = false;
        let client = Client::with_options(cfg, Duration::from_millis(300), "x".into());
        assert_eq!(client.get_config().await.unwrap(), b"host: 127.0.0.1\n");
        assert_eq!((home.count("CLUSTER"), home.count("get")), (1, 1));
    }
    // A transport failure stops before GET.
    let unreachable = Client::with_options(
        HomeConfig {
            enabled: true,
            host: "127.0.0.1".into(),
            port: 1,
            ..Default::default()
        },
        Duration::from_millis(200),
        "x".into(),
    );
    let error = unreachable.get_config().await.unwrap_err().to_string();
    assert!(
        error.starts_with("home cluster discovery transport failed: "),
        "{error}"
    );
}

fn failover_client(disabled: bool) -> Client {
    let client = Client::with_options(
        HomeConfig {
            enabled: true,
            host: "seed.example.com".into(),
            port: 8327,
            disable_cluster_discovery: disabled,
            ..Default::default()
        },
        Duration::from_millis(200),
        "x".into(),
    );
    client
        .update_cluster_nodes(
            br#"{"nodes":[{"ip":"failed.example.com","port":8327,"client_count":1},{"ip":"healthy.example.com","port":8327,"client_count":2}]}"#,
        )
        .unwrap();
    client
}

#[test]
fn reconnect_failures_fail_over_after_three_and_survive_new_lifetimes() {
    // Go TestNewLifetimePreservesClusterFailoverState.
    let client = failover_client(false);
    client.enable_legacy_membership();
    client.mark_reconnect_failure("connect");
    client.mark_reconnect_failure("connect");
    assert_eq!(client.addr().unwrap(), "seed.example.com:8327");
    client.close();
    let next = client.new_lifetime();
    assert_eq!(next.membership_instance_id(), client.membership_instance_id());
    assert!(next.legacy_membership() && !next.fenced());
    assert_eq!(next.reconnect_failures(), 2);
    next.mark_reconnect_failure("connect");
    assert_eq!(next.addr().unwrap(), "failed.example.com:8327");
    assert_eq!(next.reconnect_failures(), 0);
    // A fresh client has its own identity.
    assert_ne!(
        Client::new(HomeConfig::default()).membership_instance_id(),
        client.membership_instance_id()
    );
}

#[test]
fn disabled_discovery_never_switches_targets() {
    // Go TestFailoverAfterReconnectFailureDisabledDoesNotSwitchToClusterNode.
    let client = failover_client(true);
    for _ in 0..5 {
        client.mark_reconnect_failure("connect");
        client.mark_subscription_timeout();
    }
    assert_eq!(client.addr().unwrap(), "seed.example.com:8327");
    assert!(client.cluster_nodes().is_empty());
}

/// Go `TestRedisOptionsHomeTLSDisabled`, `TestRedisOptionsHomeTLSEnabledUsesSeedHostAsServerName`
/// and `TestRedisOptionsHomeTLSEnabledUsesExplicitServerName`: no TLS without `enable`;
/// with it, the seed host is verified even after failing over to a cluster node's
/// address, unless an explicit server name is set.
#[test]
fn tls_server_names_follow_the_seed_host_or_the_explicit_name() {
    let client = |host: &str, tls: crate::config::HomeTlsConfig| {
        Client::with_options(
            HomeConfig {
                enabled: true,
                host: host.into(),
                port: 444,
                tls,
                ..Default::default()
            },
            Duration::from_millis(200),
            "x".into(),
        )
    };
    let enabled = crate::config::HomeTlsConfig {
        enable: true,
        ..Default::default()
    };
    let plain = client("127.0.0.1", Default::default());
    assert_eq!(plain.dial_server_name("127.0.0.1").unwrap(), None);
    let seeded = client("home.example.com", enabled.clone());
    assert_eq!(
        seeded.dial_server_name("127.0.0.1").unwrap().as_deref(),
        Some("home.example.com")
    );
    let explicit = client(
        "127.0.0.1",
        crate::config::HomeTlsConfig {
            server_name: "home.example.com".into(),
            insecure_skip_verify: true,
            ..enabled
        },
    );
    assert_eq!(
        explicit.dial_server_name("127.0.0.1").unwrap().as_deref(),
        Some("home.example.com")
    );
}

#[test]
fn heartbeat_timeout_fails_over_at_once_and_skips_the_current_target() {
    let client = failover_client(false);
    client.mark_subscription_timeout();
    assert_eq!(client.addr().unwrap(), "failed.example.com:8327");
    client.mark_subscription_timeout();
    assert_eq!(client.addr().unwrap(), "healthy.example.com:8327");
    client.mark_subscription_timeout();
    assert_eq!(client.addr().unwrap(), "failed.example.com:8327");
}

#[test]
fn ambiguous_dispatch_suppresses_takeover_for_the_next_lifetime() {
    // Go TestAmbiguousDispatchSuppressesTakeoverForNextLifetime.
    let client = failover_client(false);
    client.set_recovery(Recovery::SwitchingTakeover);
    client.abort_ambiguous_dispatch();
    assert!(client.ambiguous_dispatch());
    client.suppress_takeover();
    assert_eq!(client.new_lifetime().recovery(), Recovery::Switching);
}

#[test]
fn lifecycle_config_is_validated_before_use() {
    let client = failover_client(false);
    let error = client
        .set_lifecycle_config(CredentialConcurrency {
            busy_retry_min: 1_500_000,
            ..Default::default()
        })
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "validate credential concurrency lifecycle config: credential concurrency busy retry durations must be whole milliseconds"
    );
    // Go TestClientSetLifecycleConfigAcceptsHomeAuthoritativeHeartbeat.
    client
        .set_lifecycle_config(CredentialConcurrency {
            cpa_heartbeat_timeout: 20_000_000_000,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(client.limiter_config().heartbeat_timeout(), Duration::from_secs(20));
}

/// A fake Home serving one subscriber lifetime.
async fn subscriber_home(ack: &'static str, ping_ok: Arc<AtomicUsize>) -> FakeHome {
    FakeHome::start(move |args| match args[0].to_lowercase().as_str() {
        "get" => fake::bulk("port: 1\n"),
        "subscribe" => fake::raw(ack),
        "ping" => {
            if ping_ok.load(Ordering::SeqCst) == 0 {
                Reply::Close
            } else {
                fake::raw("+PONG\r\n")
            }
        }
        _ => fake::raw("-ERR unexpected\r\n"),
    })
    .await
}

const ACK: &str = "*3\r\n$9\r\nsubscribe\r\n$6\r\nconfig\r\n:1\r\n";

fn short_heartbeat(client: &Client, revision: i64) {
    client
        .set_lifecycle_config(CredentialConcurrency {
            lifecycle_config_revision: revision,
            cpa_heartbeat_timeout: 300_000_000,
            ..Default::default()
        })
        .unwrap();
}

/// Go `TestRunConfigSubscriberLifetimeReturnsAfterHeartbeatLoss` and
/// `TestRunConfigSubscriberLifetimeRebuildsFreshCommandPoolBeforeReady`.
#[tokio::test]
async fn subscriber_lifetime_applies_updates_until_the_heartbeat_is_lost() {
    let home = subscriber_home(ACK, Arc::new(AtomicUsize::new(1))).await;
    let client = home.client();
    client.set_managed_lifetime(true);
    short_heartbeat(&client, 0);
    let configs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let ready = Arc::new(AtomicUsize::new(0));
    let shutdown = CancellationToken::new();
    let run = tokio::spawn({
        let (client, configs, ready, shutdown) = (client.clone(), configs.clone(), ready.clone(), shutdown.clone());
        async move {
            client
                .run_config_subscriber_lifetime(
                    &shutdown,
                    |raw| {
                        configs.lock().unwrap().push(String::from_utf8_lossy(raw).into_owned());
                        Ok(())
                    },
                    || {
                        ready.fetch_add(1, Ordering::SeqCst);
                    },
                )
                .await
        }
    });
    while ready.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(client.heartbeat_ok());
    home.push(fake::message("config", b"  port: 2\n  "));
    home.push(fake::message("config", b"   "));
    home.push(fake::pong());
    tokio::time::sleep(Duration::from_millis(100)).await;
    home.push(fake::pong());
    let error = run.await.unwrap().unwrap_err();
    assert!(error.is_timeout(), "{error:?}");
    assert!(!client.heartbeat_ok());
    // Managed lifetimes are closed by the service, not the subscriber.
    assert!(!client.fenced());
    assert_eq!(
        *configs.lock().unwrap(),
        vec!["port: 1\n".to_owned(), "port: 2".to_owned()]
    );
    let subscribe: Vec<Vec<String>> = home.commands().into_iter().filter(|c| c[0] == "subscribe").collect();
    assert_eq!(subscribe, vec![vec!["subscribe".to_owned(), "config".to_owned()]]);
    // After the ACK the command pool was rebuilt and probed.
    assert!(home.count("ping") >= 1);
}

/// Go `TestConfigSubscriberUsesAppliedLifecycleRevisionAndRebuildsCommands` and
/// `TestRunConfigSubscriberLifetimePreservesTakeoverWhenFreshCommandProbeFails`.
#[tokio::test]
async fn membership_args_and_takeover_eligibility_follow_go() {
    // Probe fails after a protocol-one ACK: takeover eligibility must survive.
    let ping_ok = Arc::new(AtomicUsize::new(0));
    let home = subscriber_home(ACK, ping_ok).await;
    let client = home.client();
    short_heartbeat(&client, 7);
    let error = client
        .run_config_subscriber_lifetime(&CancellationToken::new(), |_| Ok(()), || panic!("not ready"))
        .await
        .unwrap_err();
    assert!(!error.is_timeout(), "{error:?}");
    assert_eq!(client.recovery(), Recovery::TakeoverEligible);
    // Unmanaged lifetimes close themselves.
    assert!(client.fenced());
    let subscribe = home.commands().into_iter().find(|c| c[0] == "subscribe").unwrap();
    assert_eq!(
        subscribe,
        vec!["subscribe", "config", "7", "11111111-2222-3333-4444-555555555555"]
    );
    // The next lifetime asks to take over its previous membership.
    let next = client.new_lifetime();
    short_heartbeat(&next, 7);
    assert_eq!(
        next.subscription_parameters().0,
        vec!["config", "7", "takeover", "11111111-2222-3333-4444-555555555555"]
    );
}

/// Go `TestRunConfigSubscriberLifetimeRejectsInvalidSubscriptionACK`.
#[tokio::test]
async fn invalid_acks_and_legacy_rejections_end_the_lifetime() {
    let wrong = subscriber_home(
        "*3\r\n$9\r\nsubscribe\r\n$7\r\ncluster\r\n:1\r\n",
        Arc::new(AtomicUsize::new(1)),
    )
    .await;
    let client = wrong.client();
    let error = client
        .run_config_subscriber_lifetime(&CancellationToken::new(), |_| Ok(()), || {})
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "invalid Home subscription ACK");

    let legacy = subscriber_home(
        "-ERR wrong number of arguments for 'subscribe' command\r\n",
        Arc::new(AtomicUsize::new(1)),
    )
    .await;
    let client = legacy.client();
    short_heartbeat(&client, 3);
    let error = client
        .run_config_subscriber_lifetime(&CancellationToken::new(), |_| Ok(()), || {})
        .await
        .unwrap_err();
    assert!(error.is_legacy_membership_protocol(), "{error:?}");
}

#[tokio::test]
async fn initial_config_rejection_and_shutdown_end_the_lifetime() {
    let home = subscriber_home(ACK, Arc::new(AtomicUsize::new(1))).await;
    let client = home.client();
    let error = client
        .run_config_subscriber_lifetime(&CancellationToken::new(), |_| Err("bad config".into()), || {})
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "bad config");
    assert_eq!(home.count("subscribe"), 0);

    let client = home.client();
    client.set_managed_lifetime(true);
    let shutdown = CancellationToken::new();
    let run = tokio::spawn({
        let (client, shutdown) = (client.clone(), shutdown.clone());
        async move {
            client
                .run_config_subscriber_lifetime(&shutdown, |_| Ok(()), || {})
                .await
        }
    });
    while !client.heartbeat_ok() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    shutdown.cancel();
    assert_eq!(run.await.unwrap().unwrap_err(), Error::Cancelled);
    assert!(!client.heartbeat_ok());
}

#[tokio::test]
async fn cluster_channel_updates_replace_the_node_list() {
    let home = subscriber_home(ACK, Arc::new(AtomicUsize::new(1))).await;
    let mut cfg = home.config();
    cfg.disable_cluster_discovery = false;
    // Discovery on: the initial CLUSTER NODES gets an error reply and is skipped.
    let client = Client::with_options(cfg, Duration::from_millis(300), "x".into());
    client.set_managed_lifetime(true);
    let shutdown = CancellationToken::new();
    let run = tokio::spawn({
        let (client, shutdown) = (client.clone(), shutdown.clone());
        async move {
            client
                .run_config_subscriber_lifetime(&shutdown, |_| Ok(()), || {})
                .await
        }
    });
    while !client.heartbeat_ok() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    home.push(fake::message(
        "cluster",
        br#"{"ok":true,"nodes":[{"ip":"10.1.1.1","port":8327,"client_count":0}]}"#,
    ));
    while client.cluster_nodes().is_empty() {
        home.push(fake::pong());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(client.cluster_nodes()[0].ip, "10.1.1.1");
    shutdown.cancel();
    let _ = run.await;
}

/// Go `TestIssuedRPopAuthErrorClassification` (protocol errors after the request).
#[tokio::test]
async fn corrupt_rpop_replies_are_ambiguous() {
    // go-redis rejects negative lengths other than -1 as protocol errors, not Nil.
    for reply in ["$-2\r\n", "*-2\r\n", "%1\r\n"] {
        let home = FakeHome::start(move |args| match args[0].as_str() {
            "ping" => fake::raw("+PONG\r\n"),
            _ => fake::raw(reply),
        })
        .await;
        let client = home.client();
        let error = client.rpop_auth(&request()).await.unwrap_err();
        assert!(error.is_ambiguous(), "{reply:?}: {error:?}");
        assert!(client.fenced(), "{reply:?}");
    }
}

/// Go `TestMembershipTakeoverUnavailableError`: Home's takeover refusal and an old
/// Home's SUBSCRIBE arity error are told apart, and unrelated errors are neither.
#[test]
fn membership_errors_classify_like_go() {
    let server = |message: &str| Error::Server(message.into());
    assert!(server("ERR membership_takeover_unavailable").is_membership_takeover_unavailable());
    let legacy = server("ERR wrong number of arguments for 'subscribe' command");
    assert!(!legacy.is_membership_takeover_unavailable() && legacy.is_legacy_membership_protocol());
    for unrelated in [
        server("ERR connection refused"),
        server("ERR duplicate certificate"),
        Error::Timeout,
    ] {
        assert!(!unrelated.is_membership_takeover_unavailable() && !unrelated.is_legacy_membership_protocol());
    }
}

/// Go `TestClientLPushInFlightSnapshotErrorKeepsHeartbeat`.
#[tokio::test]
async fn a_failed_snapshot_push_keeps_the_heartbeat() {
    let home = FakeHome::start(|args| match args[0].to_lowercase().as_str() {
        "lpush" => fake::raw("-ERR unavailable\r\n"),
        _ => fake::raw("+PONG\r\n"),
    })
    .await;
    let client = home.client();
    client.set_heartbeat(true);
    assert!(client.lpush_in_flight_snapshot(br#"{"revision":1}"#).await.is_err());
    assert!(client.heartbeat_ok());
}

/// Go `TestClientClosePermanentlyFencesDispatch`: a closed client dispatches nothing and
/// never opens a pool again.
#[tokio::test]
async fn a_closed_client_stays_fenced() {
    let home = FakeHome::start(|_| fake::raw("+PONG\r\n")).await;
    let client = home.client();
    client.close();
    assert_eq!(client.rpop_auth(&request()).await.unwrap_err(), Error::DispatchFenced);
    assert_eq!(client.kv_get("k").await.unwrap_err(), Error::DispatchFenced);
    assert!(client.ensure_pools().is_err());
    assert!(home.commands().is_empty(), "nothing was dialed");
}

/// Go `TestDefaultProductionClientTimeouts`: operations and the subscription heartbeat
/// both default to three seconds.
#[test]
fn production_timeouts_match_go() {
    let client = Client::new(HomeConfig {
        enabled: true,
        host: "127.0.0.1".into(),
        port: 6379,
        ..Default::default()
    });
    assert_eq!(client.op_timeout(), Duration::from_secs(3));
    assert_eq!(client.subscription_parameters().1, Duration::from_secs(3));
}

/// Go `TestQueryToLowerMap` and `TestModelsRequestOmitsEmptyCredentials`: names are
/// lower-cased and repeated values joined; an empty map is omitted from the request.
#[tokio::test]
async fn models_requests_lower_case_and_omit_empty_maps() {
    let query = crate::client::lower_map([("Key", "v1"), ("Key", "v2"), ("Token", "abc")]);
    assert_eq!(query["key"], "v1, v2");
    assert_eq!(query["token"], "abc");
    assert!(crate::client::lower_map([]).is_empty());
    let home = FakeHome::start(|_| fake::bulk("[]")).await;
    let client = home.client();
    client.get_models(&BTreeMap::new(), &BTreeMap::new()).await.unwrap();
    let sent = home
        .commands()
        .into_iter()
        .find(|c| c[0].eq_ignore_ascii_case("get"))
        .unwrap();
    assert_eq!(sent[1], r#"{"type":"models"}"#);
}
