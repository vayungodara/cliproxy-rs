//! Replays Go-generated apply_patch Responses bridge vectors
//! (tests/fixtures/apply_patch_responses.json, from tests/reference/apply_patch.go):
//! every op's payloads and error text must match Go byte for byte.
use cpa_core::format::Format;
use cpa_translate::apply_patch_responses::{Bridge, State, normalize_executor_request, normalize_request};
use cpa_translate::{ResponseCtx, codex_responses_non_stream_with_bridge, pair};
use serde_json::Value;

/// The generator stores each byte as one rune.
fn bytes(v: &Value) -> Vec<u8> {
    v.as_str()
        .unwrap_or_default()
        .chars()
        .map(|c| u8::try_from(c as u32).unwrap())
        .collect()
}

fn text(v: &[u8]) -> String {
    String::from_utf8_lossy(v).into_owned()
}

type Got = (Vec<Vec<u8>>, Option<String>);

fn run(s: &Value) -> (Vec<Got>, Option<String>) {
    let ops = s["ops"].as_array().unwrap();
    let mut results = vec![];
    let kind = s["kind"].as_str().unwrap();
    match kind {
        "bridge" => {
            let mut b = Bridge::new(&bytes(&s["request"]));
            for op in ops {
                let input = bytes(&op["in"]);
                results.push(match op["op"].as_str().unwrap() {
                    "transform" => b.transform(&input),
                    "non_stream" => match b.transform_non_stream(&input) {
                        Ok(out) => (vec![out], None),
                        Err(e) => (vec![], Some(e)),
                    },
                    "finish" => (vec![], b.finish().err()),
                    "check_identity" => (vec![], b.check_identity(&input).err()),
                    "fail" => b.fail(&text(&input)),
                    other => panic!("op {other}"),
                });
            }
            (results, b.tool_input_error().map(str::to_owned))
        }
        "state" => {
            let source = Format::parse(s["source"].as_str().unwrap()).unwrap();
            let mut st = State::new(source, &bytes(&s["original"]), &bytes(&s["declarations"]));
            for op in ops {
                let input = bytes(&op["in"]);
                results.push(match op["op"].as_str().unwrap() {
                    "add_dispatcher" => {
                        st.add_dispatcher(op["name"].as_str().unwrap(), op["namespace"].as_str().unwrap());
                        (vec![], None)
                    }
                    "transform" => st.transform(&input),
                    "stream" => st.stream(&input),
                    "finish" => (vec![], st.finish().err()),
                    "bridge_finish" => (vec![], st.bridge.finish().err()),
                    "finish_stream" => st.finish_stream(),
                    "remember_event" => {
                        st.remember_dispatcher_event(&input);
                        (vec![], None)
                    }
                    "remember_args" => {
                        st.remember_dispatcher_arguments(&input);
                        (vec![], None)
                    }
                    "active" => (vec![st.active().to_string().into_bytes()], None),
                    other => panic!("op {other}"),
                });
            }
            (results, st.bridge.tool_input_error().map(str::to_owned))
        }
        "normalize" | "normalize_executor" => {
            let original = bytes(&s["original"]);
            for op in ops {
                let input = bytes(&op["in"]);
                let out = if kind == "normalize" {
                    normalize_request(&input)
                } else {
                    normalize_executor_request(&input, (!original.is_empty()).then_some(original.as_slice()))
                };
                results.push(match out {
                    Ok(out) => (vec![out], None),
                    Err(e) => (vec![], Some(e)),
                });
            }
            (results, None)
        }
        "codex_stream" | "codex_non_stream" => {
            let (original, translated) = (bytes(&s["original"]), bytes(&s["declarations"]));
            let ctx = ResponseCtx {
                model: s["model"].as_str().unwrap(),
                original_request: &original,
                translated_request: &translated,
            };
            let bridged = s["bridged"].as_bool().unwrap_or(false);
            let codex = pair(Format::OpenAIResponse, Format::Codex).unwrap();
            if kind == "codex_stream" {
                let mut go = cpa_translate::codex_responses_go_stream_with_bridge(
                    &ctx,
                    bridged.then(|| Bridge::new(&bytes(&s["request"]))),
                );
                for op in ops {
                    results.push((go.line(&bytes(&op["in"])).unwrap(), None));
                }
                let failed = go.tool_input_failed();
                return (results, failed.then(|| "<failed>".to_owned()));
            }
            let mut bridge = Bridge::new(&bytes(&s["request"]));
            for op in ops {
                let body = bytes(&op["in"]);
                let out = if bridged {
                    codex_responses_non_stream_with_bridge(&ctx, &body, &mut bridge)
                } else {
                    (codex.non_stream)(&ctx, &body)
                };
                results.push(match out {
                    Ok(out) => (vec![out], None),
                    Err(e) => {
                        assert_eq!(e.0, cpa_translate::APPLY_PATCH_UPSTREAM_ERROR);
                        (vec![], None)
                    }
                });
            }
            (results, bridge.tool_input_error().map(str::to_owned))
        }
        other => panic!("kind {other}"),
    }
}

#[test]
fn apply_patch_responses_match_go() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/apply_patch_responses.json");
    let scenarios: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert!(scenarios.len() > 100, "generator lost scenarios");
    let mut failures = vec![];
    let mut ops = 0;
    for s in &scenarios {
        let name = format!("{} {}", s["kind"].as_str().unwrap(), s["name"].as_str().unwrap());
        let (got, tool_error) = run(s);
        let want = s["results"].as_array().unwrap();
        assert_eq!(got.len(), want.len(), "{name}: result count");
        for (i, ((out, err), want)) in got.iter().zip(want).enumerate() {
            ops += 1;
            let want_out: Vec<Vec<u8>> = want["out"].as_array().unwrap().iter().map(bytes).collect();
            let want_err = want["err"].as_str().map(|_| text(&bytes(&want["err"])));
            if *out != want_out || *err != want_err {
                failures.push(format!(
                    "{name} op {i}:\n  got  {:?} {err:?}\n  want {:?} {want_err:?}",
                    out.iter().map(|o| text(o)).collect::<Vec<_>>(),
                    want_out.iter().map(|o| text(o)).collect::<Vec<_>>()
                ));
            }
        }
        let want_tool = s["tool_error"].as_str().map(|_| text(&bytes(&s["tool_error"])));
        let tool_matches = if s["kind"] == "codex_stream" {
            tool_error.is_some() == want_tool.is_some()
        } else {
            tool_error == want_tool
        };
        if !tool_matches {
            failures.push(format!("{name}: tool error {tool_error:?}, want {want_tool:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {ops} ops differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
