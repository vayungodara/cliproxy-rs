//! Replays tests/fixtures/codex_client_translate_go.json, written by Go's
//! `TranslateRequestWithAPIKeyModelCompatibilityForExecutor` and
//! `OptimizeCodexMultiAgentV2RequestForAuth` at 6fecc6e
//! (tests/reference/codex_client/README.md).

use super::*;
use serde_json::Value;

fn headers(v: &Value) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, val) in v["headers"].as_object().into_iter().flatten() {
        out.insert(
            http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            val.as_str().unwrap().parse().unwrap(),
        );
    }
    out
}

fn flag(v: &Value, key: &str) -> bool {
    v[key].as_bool().unwrap_or(false)
}

fn settings(v: &Value) -> Settings {
    Settings {
        optimize_multi_agent_v2: flag(v, "optimize"),
        orphan_delegation: flag(v, "orphan"),
    }
}

#[test]
fn replays_go_translation_and_optimization() {
    let vectors: Vec<Value> =
        serde_json::from_str(include_str!("../tests/fixtures/codex_client_translate_go.json")).unwrap();
    let (mut translated, mut optimized) = (0, 0);
    for v in &vectors {
        let input = v["in"].as_str().unwrap().as_bytes();
        let want = v["out"].as_str().unwrap();
        let h = headers(v);
        match v["fn"].as_str().unwrap() {
            "translate" => {
                translated += 1;
                let from = Format::parse(v["from"].as_str().unwrap()).unwrap();
                let to = Format::parse(v["to"].as_str().unwrap()).unwrap();
                let client = Client {
                    headers: &h,
                    settings: settings(v),
                    target_executor: v["target"].as_str().unwrap_or_default(),
                    is_compat: flag(v, "compat"),
                };
                let ctx = RequestCtx {
                    model: "test-model",
                    stream: true,
                };
                let got = translate_request(from, to, &ctx, input, &client).unwrap();
                assert_eq!(
                    String::from_utf8(got).unwrap(),
                    want,
                    "{} -> {} compat={} target={:?} ua={}",
                    from.as_str(),
                    to.as_str(),
                    client.is_compat,
                    client.target_executor,
                    v["headers"]["User-Agent"]
                );
            }
            "optimize_auth" => {
                optimized += 1;
                let (got, renamed) = optimize_for_auth(&h, input, &settings(v), flag(v, "compat"), false);
                assert_eq!(String::from_utf8(got).unwrap(), want, "{v}");
                assert_eq!(renamed, flag(v, "bool"), "{v}");
            }
            other => panic!("unknown vector fn {other}"),
        }
    }
    assert_eq!((translated, optimized), (80, 8));
}

/// The Responses boundary and API keys read orphan delegation without its OAuth-only
/// (v8 `oauth.providers.codex`) value, as Go's `ForAPIKey` and handler config do.
#[test]
fn orphan_delegation_written_oauth_only_skips_api_keys() {
    let cfg = Config::parse(
        "client:\n  codex:\n    optimize-multi-agent-v2: true\noauth:\n  providers:\n    codex:\n      orphan-delegation-compatibility: true\n",
    )
    .unwrap();
    assert!(Settings::from_config(&cfg).orphan_delegation);
    let api_key = Settings::from_config(&cfg.for_api_key());
    assert!(!api_key.orphan_delegation);
    assert!(api_key.optimize_multi_agent_v2);
    let legacy = Config::parse("codex:\n  orphan-delegation-compatibility: true\n").unwrap();
    assert!(Settings::from_config(&legacy.for_api_key()).orphan_delegation);
}
