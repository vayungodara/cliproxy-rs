//! tidwall/sjson v1.2.5 edits and gjson v1.18.0 reads recorded from Go over a cross
//! product of documents, paths and values (tests/reference/sjson/main.go), checked
//! against [`crate::json`].

use serde_json::Value;

use crate::json;

#[test]
fn json_matches_go_sjson_and_gjson_vectors() {
    let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/sjson_go.json")).unwrap();
    let edits = fixture["edits"].as_array().unwrap();
    assert!(edits.len() > 7_000, "{}", edits.len());
    let mut failures = Vec::new();
    for case in edits {
        let doc = case["json"].as_str().unwrap().as_bytes();
        let path = case["path"].as_str().unwrap();
        let value = case["value"].as_str().unwrap_or_default();
        // Go callers ignore sjson errors and keep the original bytes.
        let got = match case["op"].as_str().unwrap() {
            "delete" => json::try_delete(doc, path),
            "set_raw" => json::try_set_raw(doc, path, value),
            "set_str" => json::try_set_str(doc, path, value),
            op => panic!("{op}"),
        }
        .unwrap_or_else(|_| doc.to_vec());
        if got != case["out"].as_str().unwrap().as_bytes() {
            failures.push(format!("{case}\n  got: {}", String::from_utf8_lossy(&got)));
        }
    }
    for case in fixture["reads"].as_array().unwrap() {
        let r = json::get(
            case["json"].as_str().unwrap().as_bytes(),
            case["path"].as_str().unwrap(),
        );
        let got = (r.bytes().into_owned(), r.int(), r.bool(), r.exists());
        let want = (
            case["string"].as_str().unwrap().as_bytes().to_vec(),
            case["int"].as_i64().unwrap(),
            case["bool"].as_bool().unwrap(),
            case["exists"].as_bool().unwrap(),
        );
        if got != want {
            failures.push(format!("{case}\n  got: {got:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} vectors differ from Go:\n{}",
        failures.len(),
        failures.iter().take(10).cloned().collect::<Vec<_>>().join("\n")
    );
}
