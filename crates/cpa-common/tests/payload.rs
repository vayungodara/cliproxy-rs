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

/// Rules whose paths use gjson syntax the port does not read (subselectors, modifiers):
/// cliproxy-rs ignores them. Go leaves `subselector_override` as it is too, marks
/// `@reverse` in `modifier_override` without an edit, and applies the rule behind the
/// `{a}` condition in `subselector_condition`. `subselector_not_exist` is compared with
/// Go: the subselector exists there, so the rule does not apply on either side.
/// `unported_not_exist_7` (`a(|@reverse`) is the missing key `a(` to Go, so its rule
/// applies there. `query_subselector_path` reads a query path (`[k]`) the port does not.
const REFUSED: &[&str] = &[
    "subselector_override",
    "modifier_override",
    "subselector_condition",
    "unported_not_exist_7",
    "query_subselector_path",
];

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
    assert_eq!(cases.len(), 60);
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
        let touched: Vec<String> = touched.into_iter().collect();
        if REFUSED.contains(&name) {
            // docs/DIFFERENCES-FROM-GO.md: the rule is ignored, whatever Go did.
            assert_eq!(String::from_utf8(out).unwrap(), s("payload"), "{name}");
            assert!(touched.is_empty(), "{name} touched {touched:?}");
            continue;
        }
        assert_eq!(String::from_utf8(out).unwrap(), s("out"), "{name}");
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
    // Every section on one wildcard: defaults against an original without the array,
    // a whole-element default, a condition whose only match is the last element, and a
    // filter of fields inside elements.
    let cfg = Config::parse(
        r#"payload:
  default:
    - models: [{name: "m"}]
      params: {"items.#(k==3)#.x": 9}
    - models: [{name: "m"}]
      params: {"w.#(k==1)#": 5}
  default-raw:
    - models: [{name: "m"}]
      params: {"items.#(k==3)#.y": '{"a":1}'}
  override:
    - models: [{name: "m", exist: ["items.#(k==1)#"]}]
      params: {"items.#(k==1)#": "replaced"}
  override-raw:
    - models: [{name: "m", match: [{"items.#(k==2)#.n": 7}]}]
      params: {"items.#(k==2)#": '{"k":2,"n":4}'}
  filter:
    - models: [{name: "m"}]
      params: ["items.#(k==3)#.pad"]
"#,
    )
    .unwrap();
    let rules = payload::Rules::of(&cfg);
    let time = |n: usize| {
        let items: Vec<String> = (0..n)
            .map(|i| {
                let last = if i + 1 == n { 7 } else { 3 };
                format!(r#"{{"k":{},"n":{last},"pad":"{}"}}"#, 1 + i % 3, "p".repeat(40))
            })
            .collect();
        let body = format!(
            r#"{{"items":[{}],"w":[{}]}}"#,
            items.join(","),
            vec![r#"{"k":1}"#; n].join(",")
        );
        let original = format!(r#"{{"pad":"{}"}}"#, "p".repeat(20 * n));
        let req = payload::Request {
            target_executor: "openai",
            model: "m",
            requested_model: "m",
            protocol: "openai",
            from_protocol: "",
            root: "",
            original: original.as_bytes(),
            request_path: "",
            headers: None,
        };
        (0..3)
            .map(|_| {
                let start = std::time::Instant::now();
                let out = payload::apply(&rules, &req, body.clone().into_bytes());
                assert!(
                    out.starts_with(
                        br#"{"items":["replaced",{"k":2,"n":4},{"k":3,"n":3,"x":9,"y":{"a":1}},"replaced""#
                    ),
                    "{}",
                    String::from_utf8_lossy(&out[..200])
                );
                assert!(out.ends_with(br#",5,5]}"#));
                start.elapsed()
            })
            .min()
            .unwrap()
    };
    // The last element (n - 1 = 1 mod 3) has k == 2 and is the only one with n == 7.
    let (small, large) = (time(500), time(8000));
    let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-9);
    assert!(
        ratio < 64.0,
        "16x the elements took {ratio:.0}x the time ({small:?} to {large:?})"
    );
}
