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
                    tool_search: false,
                };
                let got = translate_request(from, to, &ctx, input, &client).unwrap();
                assert_eq!(
                    String::from_utf8(got.body).unwrap(),
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

/// Orphan delegation is a shared `upstream.codex` setting (Go 52d5507d): API keys
/// read it from every spelling, the historical `oauth.providers.codex` one included.
#[test]
fn orphan_delegation_is_shared_with_api_keys() {
    for yaml in [
        "upstream:\n  codex:\n    orphan-delegation-compatibility: true\n",
        "oauth:\n  providers:\n    codex:\n      orphan-delegation-compatibility: true\n",
        "codex:\n  orphan-delegation-compatibility: true\n",
    ] {
        let cfg = Config::parse(&format!("client:\n  codex:\n    optimize-multi-agent-v2: true\n{yaml}")).unwrap();
        assert!(Settings::from_config(&cfg).orphan_delegation, "{yaml}");
        let api_key = Settings::from_config(&cfg.for_api_key());
        assert!(api_key.orphan_delegation && api_key.optimize_multi_agent_v2, "{yaml}");
    }
}
