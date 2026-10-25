//! Differential test for cpa_common::gemini_schema against the pinned Go cleaners
//! (fixtures from tests/reference/gemini_schema/main.go, recorded at v8.0.35). One test per
//! cleaner, so nextest runs them in parallel: Go's own reference-budget inputs make the
//! cleaners quadratic for seconds per call, in Go as here.

use cpa_common::gemini_schema as gs;
use serde_json::Value;

fn bytes(v: &Value) -> Vec<u8> {
    v.as_str().unwrap().chars().map(|c| c as u32 as u8).collect()
}

fn matches_go(name: &str, clean: fn(&[u8]) -> Vec<u8>) {
    use std::io::Read;
    let gz = include_bytes!("fixtures/gemini_schema.json.gz");
    let mut text = String::new();
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_string(&mut text)
        .expect("fixture gzip");
    let fixtures: Vec<Value> = serde_json::from_str(&text).expect("fixture JSON");
    let mut failures = vec![];
    for f in &fixtures {
        let input = bytes(&f["input"]);
        let want = bytes(&f["outputs"][name]);
        let got = clean(&input);
        if got != want {
            let cut = |b: &[u8]| String::from_utf8_lossy(&b[..b.len().min(2000)]).into_owned();
            failures.push(format!(
                "{}\n  go:   {}\n  rust: {}",
                cut(&input),
                cut(&want),
                cut(&got)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{name}: {} of {} outputs differ:\n{}",
        failures.len(),
        fixtures.len(),
        failures.iter().take(12).cloned().collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn gemini_matches_go() {
    matches_go("gemini", gs::for_gemini);
}

#[test]
fn gemini_json_schema_matches_go() {
    matches_go("gemini_json_schema", gs::for_gemini_json_schema);
}

#[test]
fn antigravity_matches_go() {
    matches_go("antigravity", gs::for_antigravity);
}

#[test]
fn antigravity_tool_matches_go() {
    matches_go("antigravity_tool", |s| gs::for_antigravity_tool(s, false));
}

#[test]
fn antigravity_claude_tool_matches_go() {
    matches_go("antigravity_claude_tool", |s| gs::for_antigravity_claude_tool(s, false));
}

#[test]
fn antigravity_response_matches_go() {
    matches_go("antigravity_response", gs::for_antigravity_response);
}

#[test]
fn inline_local_refs_matches_go() {
    matches_go("inline_local_refs", gs::inline_local_refs);
}

/// A chain of 1,000 definitions in a 5-deep schema expands to about 2,000 nesting levels
/// within the inlining budget. Inlining and cleaning run on their own deep stack, so a
/// caller on a 2 MiB worker stack completes instead of aborting.
#[test]
fn a_definition_chain_expands_on_a_small_stack() {
    const N: usize = 1000;
    let mut defs = String::new();
    for i in 0..N {
        defs += &format!(
            r##""D{i}":{{"type":"object","properties":{{"a":{{"$ref":"#/$defs/D{}"}}}}}},"##,
            i + 1
        );
    }
    defs += &format!(r#""D{N}":{{"type":"string"}}"#);
    let schema = format!(
        r##"{{"type":"object","properties":{{"x":{{"type":"object","properties":{{"y":{{"$ref":"#/$defs/D0"}}}}}}}},"$defs":{{{defs}}}}}"##
    );
    let (cleaned, inlined) = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            (
                gs::for_gemini(schema.as_bytes()),
                gs::inline_local_refs(schema.as_bytes()),
            )
        })
        .unwrap()
        .join()
        .unwrap();
    for out in [&cleaned, &inlined] {
        let expanded = out.windows(5).filter(|w| w == br#""a":{"#).count();
        assert!(expanded >= N, "{expanded} of {N} references expanded");
    }
}
