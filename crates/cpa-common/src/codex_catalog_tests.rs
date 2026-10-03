//! Replays tests/fixtures/codex_catalog_go.json.gz, written by Go's catalog builder at
//! 6fecc6e with the registry facts it looked up (tests/reference/codex_client/README.md).

use super::*;
use serde_json::Value;

fn fixture() -> Value {
    use std::io::Read;
    let gz = include_bytes!("../tests/fixtures/codex_catalog_go.json.gz");
    let mut text = String::new();
    flate2::read::GzDecoder::new(&gz[..]).read_to_string(&mut text).unwrap();
    serde_json::from_str(&text).unwrap()
}

struct Recorded<'a>(&'a Value);

fn facts(v: &Value) -> Option<ModelFacts> {
    if v.is_null() {
        return None;
    }
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_owned();
    let strings = |k: &str| {
        v[k].as_array()
            .map(|a| a.iter().map(|x| x.as_str().unwrap().to_owned()).collect())
            .unwrap_or_default()
    };
    Some(ModelFacts {
        id: s("id"),
        kind: s("type"),
        owned_by: s("owned_by"),
        display_name: s("display_name"),
        description: s("description"),
        context_length: v["context_length"].as_i64().unwrap_or_default(),
        metadata_model_id: s("metadata_model_id"),
        thinking: (!v["thinking"].is_null()).then(|| serde_json::from_value(v["thinking"].clone()).unwrap()),
        explicit_thinking: v["explicit_thinking"].as_bool().unwrap_or_default(),
        input_modalities: strings("input_modalities"),
        explicit_input_modalities: v["explicit_input_modalities"].as_bool().unwrap_or_default(),
    })
}

impl Registry for Recorded<'_> {
    fn lookup(&self, id: &str, provider: Option<&str>) -> Option<ModelFacts> {
        let key = match provider {
            Some(p) => format!("{id}|{p}"),
            None => id.to_owned(),
        };
        let recorded = self.0["lookups"]
            .get(&key)
            .unwrap_or_else(|| panic!("Go never looked up {key:?}"));
        facts(recorded)
    }

    fn providers(&self, id: &str) -> Vec<String> {
        self.0["providers"][id]
            .as_array()
            .map(|a| a.iter().map(|p| p.as_str().unwrap().to_owned()).collect())
            .unwrap_or_default()
    }

    fn web_search(&self, id: &str) -> Option<bool> {
        self.0["web_search"][id].as_bool()
    }
}

#[test]
fn catalogs_match_go() {
    let fixture = fixture();
    let available: Vec<Map> = fixture["available"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| match GoValue::from_json(m) {
            GoValue::Object(map) => map,
            other => panic!("not an object: {other:?}"),
        })
        .collect();
    let registry = Recorded(&fixture);
    let patch_table = &fixture["apply_patch"];
    let patch = |id: &str| patch_table[id].as_bool().unwrap_or(false);
    let vectors = fixture["vectors"].as_array().unwrap();
    assert_eq!(vectors.len(), 5);
    for v in vectors {
        let capability: Option<&dyn Fn(&str) -> bool> = if v["apply_patch"] == true { Some(&patch) } else { None };
        let out = build_response(
            &available,
            &registry,
            capability,
            v["optimize"].as_bool().unwrap(),
            v["version"].as_str().unwrap(),
        );
        let got = String::from_utf8(marshal_compact(&out)).unwrap();
        let want = v["out"].as_str().unwrap();
        if got != want {
            let (g, w): (Value, Value) = (serde_json::from_str(&got).unwrap(), serde_json::from_str(want).unwrap());
            for (gm, wm) in g["models"]
                .as_array()
                .unwrap()
                .iter()
                .zip(w["models"].as_array().unwrap())
            {
                assert_eq!(gm, wm, "version {:?}: model {}", v["version"], wm["slug"]);
            }
            assert_eq!(got, want, "version {:?}", v["version"]);
        }
    }
}

#[test]
fn validation_matches_go() {
    let fixture = fixture();
    for case in fixture["validation"].as_array().unwrap() {
        let want = case["err"].as_str().unwrap();
        let got = validate(case["in"].as_str().unwrap().as_bytes())
            .err()
            .unwrap_or_default();
        if want.starts_with("decode Codex client model catalog:") {
            // encoding/json's wording differs; the prefix and the failure match.
            assert!(got.starts_with("decode Codex client model catalog:"), "{got}");
        } else {
            assert_eq!(got, want, "{}", case["in"]);
        }
    }
    assert!(validate(crate::codex_client::CLIENT_MODELS_JSON.as_bytes()).is_ok());
}

#[test]
fn versions_compare_like_go() {
    assert!(extended_levels(""));
    assert!(extended_levels("garbage"));
    assert!(extended_levels("0.144.0"));
    assert!(extended_levels("V0.144.0-rc1"));
    assert!(!extended_levels("0.143.99"));
    assert!(!extended_levels("0.143"));
    assert!(extended_levels("0.144"));
    // Go cuts at the first `-` or `+` before parsing, so only a bad part fails.
    assert_eq!(compare_versions("1.-2", "1"), Some(0));
    assert_eq!(compare_versions("1.x", "1"), None);
}
