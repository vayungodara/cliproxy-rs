//! `prepare_for_upload` against Go v8.0.35's `PrepareConfigFileForUpload` (f9437700),
//! recorded by `tests/reference/prepare_upload/main.go`: the outcome, the error, whether
//! the file was left alone, the prepared file's content (bcrypt hashes masked) and
//! whether its secret is the hash of the expected plaintext.
//!
//! Rust refuses four kinds of file that Go prepares (docs/DIFFERENCES-FROM-GO.md):
//! where re-emitting the alias-expanded tree would change another setting, and where
//! the secret's tag depends on a `%TAG` directive or the secret is reached through an
//! alias or merge in a file with a tag that could be binary, and any file with a
//! Unicode line separator, and a `!!binary` value that is not UTF-8, which the config
//! decoder refuses. Those cases expect Rust's refusal.
use serde_json::Value as Json;
use serde_yaml_ng::Value;

fn masked(v: Json) -> Json {
    match v {
        Json::Object(map) => Json::Object(map.into_iter().map(|(k, v)| (k, masked(v))).collect()),
        Json::Array(items) => Json::Array(items.into_iter().map(masked).collect()),
        Json::String(s) if s.len() == 60 && s.starts_with("$2") => Json::String("<bcrypt>".into()),
        v => v,
    }
}

const REWRITE_REFUSED: &[&str] = &[
    "aliased hex secret",
    "merge key beside a hex value",
    "hex key beside a merged secret",
    "aliased v8 binary secret",
    "aliased v8 binary secret beside a copy",
];
const REWRITE_REFUSAL: &str =
    "failed to update remote management key: rewriting the config file would change the text of another setting";
const TAG_REFUSED: &[&str] = &[
    "named handle binary secret",
    "named handle binary secret beside a copy",
    "redefined secondary handle secret",
    "redefined secondary handle secret beside a copy",
    "binary host beside a merged secret",
    "aliased legacy binary secret",
    "aliased legacy binary secret beside a copy",
    "binary secret through an aliased section",
    "binary secret through an aliased section beside a copy",
    "binary secret through a merge",
    "binary secret through a merge beside a copy",
];
const TAG_REFUSAL: &str = "the management key's YAML tag cannot be resolved for upload";
const LINE_REFUSED: &[&str] = &[
    "binary anchor after a NEL-ended comment",
    "binary anchor after a NEL-ended comment beside a copy",
    "line separator in a string",
];
const LINE_REFUSAL: &str = "the config file contains a Unicode line separator yaml-edit cannot read";
const NOT_UTF8_REFUSED: &[&str] = &["binary secret that is not UTF-8"];
const NOT_UTF8_REFUSAL: &str = "failed to parse config file: a !!binary value in the config file is not UTF-8 text";

#[test]
fn prepare_for_upload_matches_go() {
    let cases: Vec<Json> = serde_json::from_str(include_str!("fixtures/prepare_upload_go.json")).unwrap();
    let dir = std::env::temp_dir().join(format!("cpa-prepare-go-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    let mut failures = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let yaml = case["yaml"].as_str().unwrap();
        let plain = case["plain"].as_str().unwrap_or_default();
        // A plaintext that is not UTF-8 is recorded as base64.
        let plain_bytes = match case["plain_bytes"].as_str() {
            Some(b) => {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD.decode(b).unwrap()
            }
            None => plain.as_bytes().to_vec(),
        };
        let go_error = case["error"].as_str().unwrap();
        std::fs::write(&path, yaml).unwrap();
        let result = cpa_core::config::prepare_for_upload(&path);
        let saved = std::fs::read_to_string(&path).unwrap();
        let mut fail = |what: String| failures.push(format!("{name}: {what}"));
        let refusal = if REWRITE_REFUSED.contains(&name) {
            Some(REWRITE_REFUSAL)
        } else if TAG_REFUSED.contains(&name) {
            Some(TAG_REFUSAL)
        } else if LINE_REFUSED.contains(&name) {
            Some(LINE_REFUSAL)
        } else if NOT_UTF8_REFUSED.contains(&name) {
            Some(NOT_UTF8_REFUSAL)
        } else {
            None
        };
        if let Some(refusal) = refusal {
            match &result {
                Err(e) if format!("{e:#}") == refusal => {}
                other => fail(format!("{other:?}, expected {refusal:?}")),
            }
            if saved != yaml {
                fail("changed".into());
            }
            continue;
        }
        match (&result, go_error.is_empty()) {
            (Ok(()), true) => {}
            (Err(e), false) => {
                let text = format!("{e:#}");
                let kind = go_error.split(':').next().unwrap();
                // The refusal is Go's text; other errors share Go's leading context.
                let same = if go_error.starts_with("plaintext") {
                    e.to_string() == go_error
                } else {
                    text.starts_with(kind)
                };
                if !same {
                    fail(format!("error {text:?}, Go {go_error:?}"));
                }
                if !plain.is_empty() && text.contains(plain) {
                    fail(format!("the error quotes the secret: {text:?}"));
                }
            }
            (Ok(()), false) => fail(format!("accepted; Go: {go_error}")),
            (Err(e), true) => fail(format!("refused: {e:#}")),
        }
        if (saved == yaml) != case["unchanged"].as_bool().unwrap() {
            fail(format!(
                "unchanged {}, Go {}:\n{saved}",
                saved == yaml,
                case["unchanged"]
            ));
        }
        // An unchanged file is the input, which Go's generic decode reads (`!!binary`
        // decoded).
        if !case["result"].is_null() && saved != yaml {
            let mut doc = cpa_core::config::yaml_value(&saved).unwrap();
            cpa_core::config::expand_merges(&mut doc).unwrap();
            let got = masked(serde_json::to_value(&doc).unwrap());
            if got != case["result"] {
                fail(format!("prepared {got}, Go {}", case["result"]));
            }
            let secret = ["management", "remote-management"]
                .iter()
                .find_map(|s| doc.get(*s)?.get("secret-key"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let verifies = !plain_bytes.is_empty() && bcrypt::verify(&plain_bytes, secret).unwrap_or(false);
            if verifies != case["verifies"].as_bool().unwrap() {
                fail(format!(
                    "secret {secret:?} verifies {verifies}, Go {}",
                    case["verifies"]
                ));
            }
        }
        assert!(
            case["effective_same"].as_bool() != Some(false),
            "{name}: the recording changed Go's config"
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}
