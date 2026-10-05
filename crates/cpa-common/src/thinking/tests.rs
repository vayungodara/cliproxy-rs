//! Replays every thinking call Go's own test suites made (internal/thinking/...,
//! internal/translator/..., executor/helps and the test/ E2E matrix at 6fecc6e),
//! recorded with Go's outputs by tests/reference/record. Each record carries the
//! registry lookups Go performed during that call, so the replay sees the same model
//! capabilities (including the test-registered models) without a registry.

use std::cell::RefCell;

use cpa_core::registry::ThinkingSupport;
use serde_json::Value;

use super::*;

type LookupTable = Vec<(String, String, Option<ModelCaps>)>;

thread_local! {
    static LOOKUPS: RefCell<Option<LookupTable>> = const { RefCell::new(None) };
    static MISSED: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn lookup_override(model: &str, provider: &str) -> Option<Option<ModelCaps>> {
    LOOKUPS.with(|l| {
        let table = l.borrow();
        let table = table.as_ref()?;
        match table.iter().find(|(m, p, _)| m == model && p == provider) {
            Some((_, _, info)) => Some(info.clone()),
            None => {
                MISSED.with(|m| m.borrow_mut().push((model.into(), provider.into())));
                Some(None)
            }
        }
    })
}

pub(crate) fn caps(v: &Value) -> Option<ModelCaps> {
    if v.is_null() {
        return None;
    }
    let thinking = v.get("thinking").filter(|t| !t.is_null()).map(|t| ThinkingSupport {
        min: t["min"].as_i64().unwrap(),
        max: t["max"].as_i64().unwrap(),
        zero_allowed: t["zero_allowed"].as_bool().unwrap(),
        dynamic_allowed: t["dynamic_allowed"].as_bool().unwrap(),
        levels: t["levels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_owned())
            .collect(),
    });
    Some(ModelCaps {
        id: v["id"].as_str().unwrap().into(),
        kind: v["type"].as_str().unwrap().into(),
        thinking,
        user_defined: v["user_defined"].as_bool().unwrap(),
        support_configuration_update: v["support_configuration_update"].as_bool().unwrap(),
        max_completion_tokens: v["max_completion_tokens"].as_i64().unwrap(),
    })
}

fn config(v: &Value) -> Config {
    Config {
        mode: match v["mode"].as_str().unwrap() {
            "budget" => Mode::Budget,
            "level" => Mode::Level,
            "none" => Mode::None,
            "auto" => Mode::Auto,
            m => panic!("{m}"),
        },
        budget: v["budget"].as_i64().unwrap(),
        level: v["level"].as_str().unwrap().into(),
    }
}

fn config_json(c: &Config) -> Value {
    serde_json::json!({"mode": c.mode.as_str(), "budget": c.budget, "level": c.level})
}

fn summary(v: &Value) -> SummaryConfig {
    SummaryConfig {
        mode: match v["mode"].as_str().unwrap() {
            "unspecified" => SummaryMode::Unspecified,
            "disabled" => SummaryMode::Disabled,
            "enabled" => SummaryMode::Enabled,
            m => panic!("{m}"),
        },
        detail: v["detail"].as_str().unwrap().into(),
    }
}

fn summary_json(s: &SummaryConfig) -> Value {
    let mode = match s.mode {
        SummaryMode::Unspecified => "unspecified",
        SummaryMode::Disabled => "disabled",
        SummaryMode::Enabled => "enabled",
    };
    serde_json::json!({"mode": mode, "detail": s.detail})
}

fn err_json(e: &Error) -> Value {
    serde_json::json!({"message": e.message, "code": e.code.map_or("", |c| c.as_str())})
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_else(|| panic!("expected string: {v}"))
}

fn b(v: &Value) -> Vec<u8> {
    crate::recorded_bytes(v)
}

fn enc(bytes: &[u8]) -> Value {
    crate::recorded_value(bytes)
}

/// Runs one thinking record; `None` when the record is not a thinking call.
fn replay(fn_name: &str, input: &Value) -> Option<Value> {
    let out = match fn_name {
        "apply_thinking" => {
            let info = caps(&input["info"]);
            let result = if input["resolved"].as_bool().unwrap() {
                apply_thinking_with_model_info_and_summary(
                    &b(&input["body"]),
                    &b(&input["source"]),
                    s(&input["model"]),
                    s(&input["from"]),
                    s(&input["to"]),
                    s(&input["provider"]),
                    info.as_ref(),
                    summary(&input["summary"]),
                    input["updates_changed"].as_bool().unwrap(),
                )
            } else {
                apply_thinking_with_source_and_summary(
                    &b(&input["body"]),
                    &b(&input["source"]),
                    s(&input["model"]),
                    s(&input["from"]),
                    s(&input["to"]),
                    s(&input["provider"]),
                    summary(&input["summary"]),
                    input["updates_changed"].as_bool().unwrap(),
                )
            };
            match result {
                Ok(body) => serde_json::json!({"body": enc(&body), "err": null}),
                Err(e) => serde_json::json!({"body": e.body.as_deref().map(enc), "err": err_json(&e)}),
            }
        }
        "validate_config" => {
            let info = caps(&input["info"]);
            match validate_config(
                config(&input["config"]),
                info.as_ref(),
                s(&input["from"]),
                s(&input["to"]),
                input["from_suffix"].as_bool().unwrap(),
            ) {
                Ok(c) => serde_json::json!({"config": config_json(&c), "err": null}),
                Err(e) => serde_json::json!({"config": null, "err": err_json(&e)}),
            }
        }
        "extract_summary" => summary_json(&extract_summary_config(&b(&input["body"]), s(&input["format"]))),
        "extract_explicit_summary" => summary_json(&extract_explicit_summary_config(
            &b(&input["body"]),
            s(&input["format"]),
        )),
        "extract_translated_summary" => summary_json(&extract_translated_summary_config(
            &b(&input["body"]),
            s(&input["from"]),
            s(&input["to"]),
        )),
        "apply_summary" => {
            let info = caps(&input["info"]);
            enc(&summary::apply_summary_config_for_provider(
                &b(&input["body"]),
                s(&input["format"]),
                s(&input["model"]),
                s(&input["provider"]),
                info.as_ref(),
                summary(&input["config"]),
            ))
        }
        "extract_reasoning_effort" => Value::String(extract_reasoning_effort(
            &b(&input["body"]),
            s(&input["provider"]),
            s(&input["model"]),
        )),
        "extract_translated_reasoning_effort" => Value::String(extract_translated_reasoning_effort(
            &b(&input["body"]),
            s(&input["provider"]),
        )),
        "strip_thinking" => enc(&strip_thinking_config(&b(&input["body"]), s(&input["provider"]))),
        "parse_suffix" => {
            let r = parse_suffix(s(input));
            serde_json::json!({"model_name": r.model_name, "has_suffix": r.has_suffix, "raw_suffix": r.raw_suffix})
        }
        "applier" => {
            let info = caps(&input["info"]);
            match apply_provider(
                s(&input["provider"]),
                &b(&input["body"]),
                &config(&input["config"]),
                info.as_ref(),
            )
            .expect("registered applier")
            {
                Ok(body) => serde_json::json!({"body": enc(&body), "err": null}),
                Err(e) => serde_json::json!({"err": e.message}),
            }
        }
        "translated_summary" => summary_json(&translated_request_summary_config(
            &b(&input["body"]),
            &b(&input["current"]),
            &b(&input["original"]),
            s(&input["model"]),
            s(&input["from"]),
            s(&input["to"]),
            input["has_request_transformer"].as_bool().unwrap(),
        )),
        _ => return None,
    };
    Some(out)
}

#[test]
fn replays_every_go_thinking_call() {
    let ran = replay_all(crate::go_calls().map(str::to_owned));
    assert!(ran["apply_thinking"] > 300, "{ran:?}");
    assert!(ran["applier"] > 200 && ran["validate_config"] > 200, "{ran:?}");
}

/// Every applier over modes x capabilities x body shapes, and the resolved pipeline over
/// source x target x capability x suffix (tests/reference/record/matrix_test.go.txt).
#[test]
fn replays_go_thinking_matrix() {
    use std::io::BufRead;
    let gz = include_bytes!("../../tests/fixtures/go_thinking_matrix.jsonl.gz");
    let lines = std::io::BufReader::new(flate2::read::GzDecoder::new(&gz[..])).lines();
    let ran = replay_all(lines.map(Result::unwrap));
    assert_eq!(ran["applier"], 13291, "{ran:?}");
    assert_eq!(ran["apply_thinking"], 6720, "{ran:?}");
}

fn replay_all(lines: impl Iterator<Item = String>) -> std::collections::BTreeMap<String, usize> {
    let mut ran = std::collections::BTreeMap::<String, usize>::new();
    let mut failures = Vec::new();
    for line in lines {
        let line = line.as_str();
        let record: Value = serde_json::from_str(line).unwrap();
        let fn_name = s(&record["fn"]).to_owned();
        let table = record["lookups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                (
                    s(&l["model"]).to_owned(),
                    s(&l["provider"]).to_owned(),
                    caps(&l["info"]),
                )
            })
            .collect();
        LOOKUPS.with(|l| *l.borrow_mut() = Some(table));
        MISSED.with(|m| m.borrow_mut().clear());
        let got = replay(&fn_name, &record["in"]);
        LOOKUPS.with(|l| *l.borrow_mut() = None);
        let Some(got) = got else { continue };
        *ran.entry(fn_name.clone()).or_default() += 1;
        let want = record["out"].clone();
        let missed = MISSED.with(|m| m.borrow().clone());
        if got != want || !missed.is_empty() {
            failures.push(format!(
                "{fn_name}\n  in:   {}\n  want: {want}\n  got:  {got}\n  unrecorded lookups: {missed:?}",
                record["in"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} thinking records differ from Go:\n{}",
        failures.len(),
        ran.values().sum::<usize>(),
        failures.iter().take(15).cloned().collect::<Vec<_>>().join("\n")
    );
    ran
}

/// The Kimi thread's Go-derived ApplyRequestThinking vectors
/// (cpa-exec/tests/device_fixtures/kimi/vectors.json), against the pinned catalog.
#[test]
fn kimi_thread_request_vectors() {
    let vectors: Vec<Value> = serde_json::from_str(include_str!(
        "../../../cpa-exec/tests/device_fixtures/kimi/vectors.json"
    ))
    .unwrap();
    let mut count = 0;
    for v in vectors.iter().filter(|v| v["fn"] == "thinking") {
        let input = s(&v["in"]).as_bytes();
        let (from, to) = (s(&v["from"]), s(&v["to"]));
        // Go's registry: only the Codex target registers translators from these formats.
        let has_request_transformer = to == "codex" && matches!(from, "openai" | "openai-response");
        let got = apply_request_thinking(&RequestThinking {
            body: input,
            payload: input,
            original: input,
            model: s(&v["model"]),
            from,
            to,
            provider: "kimi",
            resolved: None,
            has_request_transformer,
            updates_changed: false,
        });
        match v["err"].as_str() {
            Some(want) => assert_eq!(got.unwrap_err().message, want, "{v}"),
            None => assert_eq!(got.unwrap(), s(&v["out"]).as_bytes(), "{v}"),
        }
        count += 1;
    }
    assert_eq!(count, 16);
}

#[test]
fn suffix_and_conversion_edges() {
    assert_eq!(parse_suffix("a(b)(c)").model_name, "a(b)");
    assert!(!parse_suffix("a(b").has_suffix);
    assert_eq!(parse_numeric_suffix("08192"), Some(8192));
    assert_eq!(parse_numeric_suffix("+5"), Some(5), "strconv.Atoi accepts a plus sign");
    assert_eq!(parse_numeric_suffix("-1"), None);
    assert_eq!(parse_numeric_suffix("9223372036854775808"), None);
    assert_eq!(convert_budget_to_level(-2), None);
    assert_eq!(convert_budget_to_level(512), Some(LEVEL_MINIMAL));
    assert_eq!(convert_budget_to_level(513), Some(LEVEL_LOW));
    assert_eq!(convert_budget_to_level(24577), Some(LEVEL_XHIGH));
    assert_eq!(map_to_claude_effort("xhigh", false), Some("high"));
    assert_eq!(map_to_claude_effort("", true), None);
}

/// `reasoning_effort: "none"` (thinking off) on models that refuse
/// `thinking: {"type": "disabled"}` becomes the lowest effort; other models keep
/// Go's `disabled` (docs/DIFFERENCES-FROM-GO.md).
#[test]
fn thinking_off_uses_the_lowest_effort_where_disabled_is_refused() {
    let off = |model: &str, body: &str| {
        let out = apply_thinking(body.as_bytes(), model, "claude", "claude", "claude").unwrap();
        let out: Value = serde_json::from_slice(&out).unwrap();
        (out["thinking"].clone(), out["output_config"].clone())
    };
    let disabled = |model: &str| {
        format!(r#"{{"model":"{model}","max_tokens":64,"thinking":{{"type":"disabled"}},"messages":[]}}"#)
    };
    for model in ["claude-opus-5-5", "claude-fable-5-1", "claude-sonnet-5-5"] {
        assert_eq!(
            off(model, &disabled(model)),
            (
                serde_json::json!({"type": "adaptive"}),
                serde_json::json!({"effort": "low"})
            ),
            "{model}"
        );
    }
    // An OpenAI Chat `reasoning_effort: "none"`, as the executor applies it: the
    // translated body carries `disabled`, the source carries the effort.
    let source = br#"{"model":"claude-opus-5-5","reasoning_effort":"none","messages":[]}"#;
    let out = apply_thinking_with_source_and_summary(
        disabled("claude-opus-5-5").as_bytes(),
        source,
        "claude-opus-5-5",
        "openai",
        "claude",
        "claude",
        SummaryConfig::default(),
        false,
    )
    .unwrap();
    let out: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(out["thinking"], serde_json::json!({"type": "adaptive"}), "{out}");
    assert_eq!(out["output_config"], serde_json::json!({"effort": "low"}), "{out}");
    for model in ["claude-opus-4-8", "claude-sonnet-5"] {
        let (thinking, output) = off(model, &disabled(model));
        assert_eq!(thinking, serde_json::json!({"type": "disabled"}), "{model}");
        assert!(output.is_null(), "{model}");
    }
}
