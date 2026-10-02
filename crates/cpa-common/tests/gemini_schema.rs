//! Differential test for cpa_common::gemini_schema against the pinned Go cleaners
//! (fixtures from tests/reference/gemini_schema/main.go).

use cpa_common::gemini_schema as gs;
use serde_json::Value;

fn bytes(v: &Value) -> Vec<u8> {
    v.as_str().unwrap().chars().map(|c| c as u32 as u8).collect()
}

#[test]
fn matches_go_cleaners() {
    let fixtures: Vec<Value> = serde_json::from_str(include_str!("fixtures/gemini_schema.json")).expect("fixture JSON");
    type Clean = fn(&[u8]) -> Vec<u8>;
    let fns: [(&str, Clean); 6] = [
        ("gemini", gs::for_gemini),
        ("gemini_json_schema", gs::for_gemini_json_schema),
        ("antigravity", gs::for_antigravity),
        ("antigravity_tool", |s| gs::for_antigravity_tool(s, false)),
        ("antigravity_response", gs::for_antigravity_response),
        ("inline_local_refs", gs::inline_local_refs),
    ];
    let mut failures = vec![];
    let mut checked = 0;
    for f in &fixtures {
        let input = bytes(&f["input"]);
        for (name, clean) in fns {
            let want = bytes(&f["outputs"][name]);
            let got = clean(&input);
            checked += 1;
            if got != want {
                failures.push(format!(
                    "{name} {}\n  go:   {}\n  rust: {}",
                    String::from_utf8_lossy(&input),
                    String::from_utf8_lossy(&want),
                    String::from_utf8_lossy(&got)
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {checked} cleaner outputs differ:\n{}",
        failures.len(),
        failures.iter().take(12).cloned().collect::<Vec<_>>().join("\n")
    );
}
