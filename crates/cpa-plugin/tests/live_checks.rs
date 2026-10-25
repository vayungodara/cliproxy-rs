//! Go rechecks each plugin when it is about to be called, so a plugin fused or replaced
//! while an earlier plugin's call was running is skipped. Scripted in-process plugins
//! make that interleaving deterministic: the first plugin's call fuses the second.

use std::collections::BTreeMap;
use std::path::Path;

use bytes::Bytes;
use cpa_plugin::Host;
use cpa_plugin::testing::scripted::{Log, Script};

/// [`cpa_plugin::testing::scripted::host_with`] under this test binary's temp dir.
async fn host_with(name: &str, scripts: BTreeMap<&'static str, Script>) -> (Host, Log) {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("live-{name}-{}", std::process::id()));
    cpa_plugin::testing::scripted::host_with(&dir, scripts).await
}

fn calls(log: &Log) -> Vec<String> {
    std::mem::take(&mut *log.lock().unwrap())
}

#[tokio::test]
async fn chained_hooks_skip_a_plugin_fused_by_an_earlier_call() {
    let a = Script {
        caps: r#""request_normalizer":true,"request_interceptor":true,"response_stream_interceptor":true"#,
        results: [("request.normalize", r#"{"Body":"YQ=="}"#)].into(),
        fuse_on: Some(("request.normalize", "b")),
    };
    let b = Script {
        caps: a.caps,
        results: [("request.normalize", r#"{"Body":"Yg=="}"#)].into(),
        fuse_on: None,
    };
    let (host, log) = host_with("chain", [("a", a), ("b", b)].into()).await;
    let out = host.normalize_request("openai", "claude", "m", Bytes::from_static(b"{}"), false);
    assert_eq!(&out[..], b"a", "b's body would win if it were still called");
    assert_eq!(calls(&log), ["a request.normalize"]);
    let scope = Default::default();
    host.intercept_request_before_auth(Default::default(), "", &scope).await;
    host.intercept_stream_chunk(Default::default(), "", &scope).await;
    assert_eq!(
        calls(&log),
        ["a request.intercept_before", "a response.intercept_stream_chunk"]
    );
}

#[tokio::test]
async fn quota_lookup_skips_a_provider_fused_while_describing() {
    // a describes first (second pass) and fuses b; Go never asks b, so "x" is unknown.
    let a = Script {
        caps: r#""quota_provider":true"#,
        results: [("quota.describe", r#"{"supported_providers":["y"]}"#)].into(),
        fuse_on: Some(("quota.describe", "b")),
    };
    let b = Script {
        caps: r#""quota_provider":true"#,
        results: [("quota.describe", r#"{"supported_providers":["x"]}"#)].into(),
        fuse_on: None,
    };
    let (host, log) = host_with("quota", [("a", a), ("b", b)].into()).await;
    assert!(host.quota_provider_record("x").await.is_none());
    assert_eq!(calls(&log), ["a quota.describe"]);
}

#[tokio::test]
async fn fusing_a_plugin_drops_its_thinking_provider() {
    let a = Script {
        caps: r#""thinking_applier":true"#,
        results: [("thinking.identifier", r#"{"identifier":"T"}"#)].into(),
        fuse_on: None,
    };
    let (host, _) = host_with("thinking", [("a", a)].into()).await;
    assert_eq!(host.thinking_provider("t").as_deref(), Some("a"));
    host.fuse("a", "ThinkingApplier.ApplyThinking", "test");
    assert_eq!(host.thinking_provider("t"), None);
}

#[tokio::test]
async fn a_stale_executor_provider_replaced_by_a_builtin_one_is_kept() {
    for builtin_took_over in [false, true] {
        let a = Script {
            caps: r#""executor":true,"executor_model_scope":"both","executor_input_formats":["openai"],"executor_output_formats":["openai"]"#,
            results: [("executor.identifier", r#"{"identifier":"p"}"#)].into(),
            fuse_on: None,
        };
        let (host, _) = host_with("executor", [("a", a)].into()).await;
        let changes = host.register_executors(&|_| false, &|_| Vec::new()).await.unwrap();
        let registered: Vec<_> = changes
            .register
            .iter()
            .map(|a| (a.provider.as_str(), a.record.id.as_str()))
            .collect();
        assert_eq!(registered, [("p", "a")]);
        host.fuse("a", "Executor.Execute", "test");
        let native = move |p: &str| builtin_took_over && p == "p";
        let changes = host.register_executors(&native, &|_| Vec::new()).await.unwrap();
        let want: &[&str] = if builtin_took_over { &[] } else { &["p"] };
        assert_eq!(changes.unregister, want, "builtin_took_over={builtin_took_over}");
    }
}

#[tokio::test]
async fn a_builtin_provider_catalog_reserves_its_models_from_plugin_executors() {
    // Go TestRegisterExecutorsPreservesNativeCatalogPriority: first load, reloads
    // before and after the native credential publishes the model, a reload after it
    // withdraws it, then removal of the catalog.
    for (native_executor, extra_model) in [(true, false), (true, true), (false, false)] {
        let catalog = Script {
            caps: r#""model_registrar":true"#,
            results: [(
                "model.register",
                r#"{"Provider":"native-provider","Models":[{"ID":"shared-model"}]}"#,
            )]
            .into(),
            fuse_on: None,
        };
        let models = if extra_model {
            r#"{"Provider":"plugin-provider","Models":[{"ID":"shared-model"},{"ID":"plugin-only"}]}"#
        } else {
            r#"{"Provider":"plugin-provider","Models":[{"ID":"shared-model"}]}"#
        };
        let competitor = Script {
            caps: r#""model_registrar":true,"executor":true,"executor_model_scope":"both","executor_input_formats":["openai"],"executor_output_formats":["openai"]"#,
            results: [
                ("model.register", models),
                ("executor.identifier", r#"{"identifier":"plugin-provider"}"#),
            ]
            .into(),
            fuse_on: None,
        };
        let (host, _) = host_with("catalog-priority", [("catalog", catalog), ("comp", competitor)].into()).await;
        let native = move |p: &str| native_executor && p == "native-provider";
        let ids = |provider: &str| {
            host.models_for_provider(provider)
                .into_iter()
                .map(|m| m.id)
                .collect::<Vec<_>>()
        };
        let want: &[&str] = match (native_executor, extra_model) {
            (true, false) => &[],
            (true, true) => &["plugin-only"],
            (false, _) => &["shared-model"],
        };
        for (stage, published) in [
            ("first load", false),
            ("reload", false),
            ("published", true),
            ("withdrawn", false),
        ] {
            let case = format!("native_executor={native_executor} extra_model={extra_model} stage={stage}");
            host.register_models().await;
            // The native credential lists shared-model once it has published it.
            let providers = move |m: &str| {
                if published && native_executor && m == "shared-model" {
                    vec!["native-provider".to_owned()]
                } else {
                    Vec::new()
                }
            };
            let changes = host.register_executors(&native, &providers).await.unwrap();
            assert_eq!(ids("plugin-provider"), want, "{case}");
            assert_eq!(
                changes.register.is_empty(),
                want.is_empty(),
                "competing executor registered: {case}"
            );
            assert_eq!(ids("native-provider"), ["shared-model"], "{case}");
        }
        // Removing the catalog releases the reservation on the next pass.
        assert!(host.unload_plugin("catalog", None).await);
        host.register_models().await;
        let changes = host.register_executors(&native, &|_| Vec::new()).await.unwrap();
        let all: &[&str] = if extra_model {
            &["shared-model", "plugin-only"]
        } else {
            &["shared-model"]
        };
        assert_eq!(
            ids("plugin-provider"),
            all,
            "after removal: native_executor={native_executor}"
        );
        assert_eq!(changes.register.len(), 1);
    }
}

fn quota(identifier: &'static str, describe: &'static str) -> Script {
    Script {
        caps: r#""quota_provider":true"#,
        results: [("quota.identifier", identifier), ("quota.describe", describe)].into(),
        fuse_on: None,
    }
}

/// Every key maps to the plugin `quota_provider_record` selects for it.
async fn assert_selects(host: &Host, got: &std::collections::HashMap<String, String>) {
    for (provider, plugin_id) in got {
        assert!(host.has_quota_provider_for_plugin(plugin_id), "{provider}: {plugin_id}");
        let record = host.quota_provider_record(provider).await;
        assert_eq!(record.map(|r| r.id).as_deref(), Some(plugin_id.as_str()), "{provider}");
    }
}

/// Go `TestQuotaProviderPluginIDsMatchesQuotaProviderRecordPrecedence`: first-pass
/// matches (quota identifier, plugin ID, auth identifier) beat described providers,
/// and within a pass the earlier plugin wins.
#[tokio::test]
async fn quota_provider_plugin_ids_matches_quota_provider_record_precedence() {
    let mut bar_auth = quota(r#"{"identifier":"m-quota"}"#, "{}");
    bar_auth.caps = r#""quota_provider":true,"auth_provider":true"#;
    bar_auth.results.insert("auth.identifier", r#"{"identifier":" Bar "}"#);
    let (host, _) = host_with(
        "quota-ids",
        [
            (
                "a-multi",
                quota(
                    r#"{"identifier":"multi-quota"}"#,
                    r#"{"supported_providers":[" FOO ","bar"]}"#,
                ),
            ),
            ("b-foo", quota(r#"{"identifier":"foo"}"#, "{}")),
            ("m-bar-auth", bar_auth),
            ("z-foo", quota(r#"{"identifier":"foo"}"#, "{}")),
        ]
        .into(),
    )
    .await;
    let got = host.quota_provider_plugin_ids().await;
    let want: std::collections::HashMap<String, String> = [
        ("multi-quota", "a-multi"),
        ("a-multi", "a-multi"),
        ("foo", "b-foo"),
        ("b-foo", "b-foo"),
        ("bar", "m-bar-auth"),
        ("m-quota", "m-bar-auth"),
        ("m-bar-auth", "m-bar-auth"),
        ("z-foo", "z-foo"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    assert_eq!(got, want);
    assert_selects(&host, &got).await;
}

/// Go `TestQuotaProviderPluginIDsSkipsPluginsFusedDuringDescribe`: a plugin fused while
/// describing loses its keys to the next healthy candidate.
#[tokio::test]
async fn quota_provider_plugin_ids_skips_plugins_fused_during_describe() {
    for healthy in [r#"{"identifier":"foo"}"#, r#"{"identifier":"healthy-quota"}"#] {
        let mut broken = quota(r#"{"identifier":"foo"}"#, "{}");
        broken.fuse_on = Some(("quota.describe", "a-broken"));
        let (host, _) = host_with(
            "quota-fused",
            [
                ("a-broken", broken),
                ("b-healthy", quota(healthy, r#"{"supported_providers":["foo"]}"#)),
            ]
            .into(),
        )
        .await;
        let initial = host.quota_provider_record("foo").await;
        assert_eq!(initial.map(|r| r.id).as_deref(), Some("a-broken"), "{healthy}");
        let got = host.quota_provider_plugin_ids().await;
        assert!(host.is_fused("a-broken"), "{healthy}");
        assert_eq!(got.get("foo").map(String::as_str), Some("b-healthy"), "{healthy}");
        assert!(
            !got.contains_key("a-broken"),
            "{healthy}: the fused plugin's own key stays"
        );
        assert_selects(&host, &got).await;
    }
}

/// Provider keys use Go's `strings.ToLower` (simple, rune-wise), so U+0130 folds to a
/// plain "i": the earlier plugin keeps "high", where full Unicode lowercasing would file
/// it under "hi\u{307}gh" and give "high" to the later one.
#[tokio::test]
async fn quota_provider_plugin_ids_lowercase_like_go() {
    let (host, _) = host_with(
        "quota-go-lower",
        [
            ("a-upper", quota(r#"{"identifier":"H\u0130GH"}"#, "{}")),
            ("b-lower", quota(r#"{"identifier":"high"}"#, "{}")),
        ]
        .into(),
    )
    .await;
    let got = host.quota_provider_plugin_ids().await;
    let want: std::collections::HashMap<String, String> =
        [("high", "a-upper"), ("a-upper", "a-upper"), ("b-lower", "b-lower")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
    assert_eq!(got, want);
    assert_eq!(
        host.quota_provider_record("high").await.map(|r| r.id).as_deref(),
        Some("a-upper")
    );
}

/// Go `sdktranslator.SetPluginHooks(host)`: the host's request normalizer owns the
/// registry's translation; a host whose plugins offer no hook leaves it unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_as_translator_hooks_normalizes_requests() {
    let translate = || {
        cpa_translate::translate_request(
            cpa_core::format::Format::OpenAI,
            cpa_core::format::Format::OpenAI,
            &cpa_translate::RequestCtx {
                model: "m",
                stream: false,
                tool_search: false,
            },
            br#"{"model":"m","messages":[]}"#,
        )
        .unwrap()
    };
    let native = translate();
    let none = Script {
        caps: r#""usage_plugin":true"#,
        results: BTreeMap::new(),
        fuse_on: None,
    };
    let (quiet, quiet_log) = host_with("hooks-quiet", [("q", none)].into()).await;
    cpa_translate::hooks::set_plugin_hooks(Some(quiet.translator_hooks()));
    assert!(!cpa_translate::hooks::plugin_hooks_active(), "no plugin offers a hook");
    assert_eq!(translate(), native);
    assert!(calls(&quiet_log).is_empty());

    let a = Script {
        caps: r#""request_normalizer":true"#,
        results: [("request.normalize", r#"{"Body":"YQ=="}"#)].into(),
        fuse_on: None,
    };
    let (host, log) = host_with("hooks-normalize", [("a", a)].into()).await;
    cpa_translate::hooks::set_plugin_hooks(Some(host.translator_hooks()));
    assert!(cpa_translate::hooks::plugin_hooks_active());
    assert_eq!(translate(), b"a");
    assert_eq!(calls(&log), ["a request.normalize"]);
    // Unloading the last translator plugin turns the registry's flag off.
    assert!(host.unload_plugin("a", None).await);
    assert!(!cpa_translate::hooks::plugin_hooks_active());
    assert_eq!(translate(), native);
    assert!(calls(&log).is_empty());
    cpa_translate::hooks::set_plugin_hooks(None);
}

/// Go `RegisterPluginAuthParser(host)`: auth files go to the auth provider owning their
/// type while one is loaded; unloading the last one restores the built-in synthesizer
/// behind cpa-core's flag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_as_auth_parser_owns_its_plugins_auth_files() {
    use cpa_core::config::credentials;
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("live-auth-dir-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("acme.json"), br#"{"type":"acme","material":"m"}"#).unwrap();
    let cfg = cpa_core::config::Config {
        auth_dir: dir.clone(),
        ..Default::default()
    };
    let load = || {
        credentials::preparse_auth_dir(&cfg);
        let mut all = credentials::scan_auth_dir(&cfg).credentials;
        assert_eq!(all.len(), 1);
        all.remove(0)
    };
    let none = Script {
        caps: r#""usage_plugin":true"#,
        results: BTreeMap::new(),
        fuse_on: None,
    };
    let (quiet, quiet_log) = host_with("auth-parse-quiet", [("q", none)].into()).await;
    credentials::set_plugin_auth_parser(Some(quiet.auth_parser()));
    assert!(
        !credentials::plugin_auth_parser_active(),
        "no plugin offers an auth provider"
    );
    assert_eq!(load().id, "acme.json");
    assert!(calls(&quiet_log).is_empty());

    let a = Script {
        caps: r#""auth_provider":true"#,
        results: [
            ("auth.identifier", r#"{"identifier":"acme"}"#),
            (
                "auth.parse",
                r#"{"Handled":true,"Auths":[{"Provider":"acme","ID":"from-plugin","Metadata":{"token":"t"}}]}"#,
            ),
        ]
        .into(),
        fuse_on: None,
    };
    let (host, log) = host_with("auth-parse", [("a", a)].into()).await;
    credentials::set_plugin_auth_parser(Some(host.auth_parser()));
    assert!(credentials::plugin_auth_parser_active());
    let parsed = load();
    assert_eq!((parsed.id.as_str(), parsed.provider.as_str()), ("from-plugin", "acme"));
    assert_eq!(parsed.metadata["token"], "t");
    assert!(calls(&log).contains(&"a auth.parse".to_owned()));
    assert!(host.unload_plugin("a", None).await);
    assert!(!credentials::plugin_auth_parser_active());
    assert_eq!(load().id, "acme.json");
    assert!(calls(&log).is_empty());
    credentials::set_plugin_auth_parser(None);
    let _ = std::fs::remove_dir_all(&dir);
}
