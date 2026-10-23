//! `Config::parse` against Go v8.0.35's config decoder for `!!binary` and locally
//! tagged scalars,
//! recorded by `tests/reference/binary_strings/main.go`: the error, the management key
//! as decoded, `proxy-url`, `port` and `api-keys`. [`REFUSED`] names the cases Rust
//! refuses where Go loads (docs/DIFFERENCES-FROM-GO.md), with the refusal expected.
use cpa_core::config::Config;
use serde_json::Value as Json;

const TAG: &str = "a YAML tag in the config file is not supported";
const KEY: &str = "a tagged or aliased mapping key in the config file is not supported";
const ANCHOR: &str = "a YAML anchor name is defined more than once in the config file";
const PIN: &str =
    "a plugin's store.version or store.release-tag in the config file is a !!binary value, which is not supported";
const REFUSED: &[(&str, &str)] = &[
    ("binary plugin pin", PIN),
    ("aliased binary plugin pin", PIN),
    ("binary plugin pin through a merge", PIN),
    // Go pins the explicit `!keep 4.5.6`; the binary merge candidate is refused anyway.
    ("shadowed binary plugin pin", PIN),
    ("not UTF-8", "a !!binary value in the config file is not UTF-8 text"),
    ("redefined anchor around a binary secret", ANCHOR),
    ("redefined anchor", ANCHOR),
    ("locally tagged api-keys key", KEY),
    ("locally tagged port key", KEY),
    ("aliased port key", KEY),
    ("non-specific number secret", TAG),
    ("non-specific port", TAG),
    ("literal int tag", TAG),
    ("NUL in a binary tag", TAG),
    ("NUL in a tag directive", TAG),
    ("NUL in a tag after a block scalar holding a quote", TAG),
    ("NUL in a tag after a BOM", TAG),
    ("canonically tagged merge key", KEY),
    ("canonically tagged port key", KEY),
    ("non-specific document marker secret", TAG),
    ("non-specific boolean word secret", TAG),
];

#[test]
fn binary_scalars_decode_like_go() {
    let cases: Vec<Json> = serde_json::from_str(include_str!("fixtures/binary_strings_go.json")).unwrap();
    let mut failures = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let go_error = case["error"].as_str().unwrap();
        let mut fail = |what: String| failures.push(format!("{name}: {what}"));
        let config = match (Config::parse(case["yaml"].as_str().unwrap()), go_error.is_empty()) {
            (result, _) if REFUSED.iter().any(|(n, _)| *n == name) => {
                let want = REFUSED.iter().find(|(n, _)| *n == name).unwrap().1;
                match result {
                    Err(e) if e.to_string() == want => {}
                    other => fail(format!("{:?}, expected {want:?}", other.map(|_| ()))),
                }
                continue;
            }
            (Ok(config), true) => config,
            (Err(e), false) => {
                // The base64 error is Go's text; other errors only fail in both.
                if go_error.contains("!!binary value") && e.to_string() != go_error {
                    fail(format!("error {e:#}, Go {go_error:?}"));
                }
                continue;
            }
            (Ok(_), false) => {
                fail(format!("accepted; Go: {go_error}"));
                continue;
            }
            (Err(e), true) => {
                fail(format!("refused: {e:#}"));
                continue;
            }
        };
        let doc = &config.document;
        let text = |v: Option<&serde_yaml_ng::Value>| v.and_then(|v| v.as_str()).unwrap_or_default().to_owned();
        let got = serde_json::json!({
            "secret_key": config.management.secret_key,
            "proxy_url": text(doc.get("requests").and_then(|r| r.get("proxy-url"))),
            "port": config.port,
            "api_keys": (!config.api_keys.is_empty()).then_some(&config.api_keys),
        });
        for field in ["secret_key", "proxy_url", "port", "api_keys"] {
            if got[field] != case[field] {
                fail(format!("{field} {}, Go {}", got[field], case[field]));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}
