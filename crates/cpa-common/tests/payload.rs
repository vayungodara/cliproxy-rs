//! Replays Go `ApplyPayloadConfigWithTrackedPathsForExecutor` and
//! `ApplyCustomHeadersFromAttrs` goldens (tests/reference/payload/main.go).

use std::collections::BTreeMap;

use cpa_common::{headers, payload};
use cpa_core::config::Config;
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/payload_go.json")).unwrap()
}

fn header_map(pairs: &Value) -> HeaderMap {
    let mut out = HeaderMap::new();
    for pair in pairs.as_array().into_iter().flatten() {
        out.append(
            HeaderName::from_bytes(pair[0].as_str().unwrap().as_bytes()).unwrap(),
            HeaderValue::from_str(pair[1].as_str().unwrap()).unwrap(),
        );
    }
    out
}

#[test]
fn payload_rules_match_go() {
    let fixture = fixture();
    let cfg = Config::parse(fixture["config"].as_str().unwrap()).unwrap();
    let rules = payload::Rules::of(&cfg);
    assert!(
        std::sync::Arc::ptr_eq(&rules, &payload::Rules::of(&cfg)),
        "parsed once per snapshot"
    );
    assert!(
        !std::sync::Arc::ptr_eq(&rules, &payload::Rules::of(&cfg.clone())),
        "a clone is a new snapshot"
    );
    assert_eq!(rules.image_generation, payload::ImageGeneration::Chat);
    let cases = fixture["payload"].as_array().unwrap();
    assert_eq!(cases.len(), 24);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let s = |k: &str| case[k].as_str().unwrap_or_default();
        let headers = header_map(&case["headers"]);
        let tracked: Vec<&str> = case["tracked"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| t.as_str().unwrap())
            .collect();
        let req = payload::Request {
            target_executor: s("target"),
            model: s("model"),
            requested_model: s("requested_model"),
            protocol: s("protocol"),
            from_protocol: s("from_protocol"),
            root: s("root"),
            original: s("original").as_bytes(),
            request_path: s("request_path"),
            headers: Some(&headers),
        };
        let (out, touched) = payload::apply_tracked(&rules, &req, s("payload").as_bytes().to_vec(), &tracked);
        assert_eq!(String::from_utf8(out).unwrap(), s("out"), "{name}");
        let touched: Vec<String> = touched.into_iter().collect();
        let expected: Vec<String> = case["touched"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(touched, expected, "{name} touched");
    }
}

#[test]
fn custom_headers_match_go() {
    for case in fixture()["headers"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let attrs: BTreeMap<String, String> = case["attrs"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect();
        let client = header_map(&case["client"]);
        let session = case["session_id"].as_str().filter(|s| !s.is_empty());
        let got: BTreeMap<String, String> = headers::custom_headers(&attrs, &client, session)
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect();
        let expected: BTreeMap<String, String> = case["out"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.as_str().unwrap().to_owned()))
            .collect();
        assert_eq!(got, expected, "{name}");
    }
}

/// Go `payloadModelCandidates` dedups by `strings.ToLower`, and model patterns match
/// case-sensitively: final sigma stays a separate candidate, while İ lowers to i.
#[test]
fn model_candidates_dedup_on_go_lowercase() {
    let cfg = Config::parse(
        "payload:\n  override:\n    - models: [{name: \"ς\"}, {name: \"i\"}]\n      params: {hit: true}\n",
    )
    .unwrap();
    let rules = payload::Rules::of(&cfg);
    let run = |model: &str, requested: &str| {
        let req = payload::Request {
            target_executor: "openai-compatibility",
            model,
            requested_model: requested,
            protocol: "openai",
            from_protocol: "openai",
            root: "",
            original: b"",
            request_path: "/v1/chat/completions",
            headers: None,
        };
        String::from_utf8(payload::apply(&rules, &req, b"{}".to_vec())).unwrap()
    };
    assert_eq!(run("σ", "ς"), r#"{"hit":true}"#, "σ and ς are distinct candidates");
    assert_eq!(run("İ", "i"), "{}", "i collapses into İ");
}

/// Whole-element setters and whole-element reads on a `#(...)#` query edit the body in
/// one pass: 16 times the elements must cost far less than 256 times the time (one edit
/// of the whole body per element). Both sizes run in this process, so the ratio does not
/// depend on the machine.
#[test]
fn wildcard_rules_scale_linearly() {
    let cfg = Config::parse(
        r#"payload:
  override:
    - models: [{name: "m", exist: ["items.#(k==1)#"]}]
      params: {"items.#(k==1)#": "replaced"}
  override-raw:
    - models: [{name: "m", match: [{"items.#(k==2)#.n": 3}]}]
      params: {"items.#(k==2)#": '{"k":2,"n":4}'}
"#,
    )
    .unwrap();
    let rules = payload::Rules::of(&cfg);
    let req = payload::Request {
        target_executor: "openai",
        model: "m",
        requested_model: "m",
        protocol: "openai",
        from_protocol: "",
        root: "",
        original: b"",
        request_path: "",
        headers: None,
    };
    let time = |n: usize| {
        let items: Vec<String> = (0..n)
            .map(|i| format!(r#"{{"k":{},"n":3,"pad":"{}"}}"#, 1 + i % 2, "p".repeat(40)))
            .collect();
        let body = format!(r#"{{"items":[{}]}}"#, items.join(","));
        (0..3)
            .map(|_| {
                let start = std::time::Instant::now();
                let out = payload::apply(&rules, &req, body.clone().into_bytes());
                assert!(out.starts_with(br#"{"items":["replaced",{"k":2,"n":4}"#));
                start.elapsed()
            })
            .min()
            .unwrap()
    };
    let (small, large) = (time(500), time(8000));
    let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-9);
    assert!(
        ratio < 64.0,
        "16x the elements took {ratio:.0}x the time ({small:?} to {large:?})"
    );
}
