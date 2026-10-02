//! Replays every signature call Go's own test suites made at 6fecc6e (see
//! tests/reference/record), comparing outputs and error text exactly.

use serde_json::{Value, json};

use super::*;

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_else(|| panic!("expected string: {v}"))
}

fn b(v: &Value) -> Vec<u8> {
    crate::recorded_bytes(v)
}

fn enc(bytes: &[u8]) -> Value {
    crate::recorded_value(bytes)
}

fn provider(v: &str) -> Provider {
    match v {
        "" => Provider::Empty,
        "unknown" => Provider::Unknown,
        "claude" => Provider::Claude,
        "gemini" => Provider::Gemini,
        "gemini_bypass" => Provider::GeminiBypass,
        "gpt" => Provider::Gpt,
        "kimi" => Provider::Kimi,
        "grok" => Provider::Grok,
        "swe" => Provider::Swe,
        p => panic!("{p}"),
    }
}

fn block_kind(v: &str) -> BlockKind {
    match v {
        "" | "unknown" => BlockKind::Unknown,
        "claude_thinking" => BlockKind::ClaudeThinking,
        "gemini_model_part" => BlockKind::GeminiModelPart,
        "gemini_function_call" => BlockKind::GeminiFunctionCall,
        "gpt_reasoning" => BlockKind::GptReasoning,
        k => panic!("{k}"),
    }
}

fn claude_opt(v: &Value) -> ClaudeValidation {
    ClaudeValidation {
        prefix_only: v["prefix_only"].as_bool().unwrap(),
        base64_only: v["base64_only"].as_bool().unwrap(),
        allow_empty_signature_with_empty_text: v["allow_empty"].as_bool().unwrap(),
        strict: v["strict"].as_bool().unwrap(),
    }
}

fn gemini_opt(v: &Value) -> GeminiValidation {
    GeminiValidation {
        allow_bypass_sentinel: v["allow_bypass_sentinel"].as_bool().unwrap(),
        require_known_envelope: v["require_known_envelope"].as_bool().unwrap(),
        require_observed_marker: v["require_observed_marker"].as_bool().unwrap(),
    }
}

fn decision_json(d: &Decision) -> Value {
    json!({
        "target": d.target_provider.as_str(), "detected": d.detected_provider.as_str(),
        "block_kind": d.block_kind.as_str(), "compatible": d.compatible, "action": d.action.as_str(),
        "replacement": d.replacement_signature, "normalized": d.normalized_signature, "reason": d.reason,
    })
}

fn err<T>(r: &Result<T, Error>) -> Value {
    match r {
        Ok(_) => Value::Null,
        Err(e) => Value::String(e.0.clone()),
    }
}

fn tree_json(t: &ClaudeSignatureTree) -> Value {
    json!({
        "encoding_layers": t.encoding_layers, "channel_id": t.channel_id, "field2": t.field2,
        "routing_class": t.routing_class, "infrastructure_class": t.infrastructure_class,
        "schema_features": t.schema_features, "model_text": t.model_text,
        "legacy_route_hint": t.legacy_route_hint, "has_field7": t.has_field7,
    })
}

fn tree_result(r: Result<ClaudeSignatureTree, Error>) -> Value {
    json!({"tree": r.as_ref().ok().map(tree_json), "err": err(&r)})
}

fn replay(fn_name: &str, input: &Value) -> Option<Value> {
    Some(match fn_name {
        "detect" => json!(detect_provider_for_block(b(&input["raw"]), block_kind(s(&input["block_kind"]))).as_str()),
        "decide" => decision_json(&decide_compatibility_for_model(
            provider(s(&input["target"])),
            s(&input["model"]),
            b(&input["raw"]),
            block_kind(s(&input["block_kind"])),
        )),
        "antigravity_claude" => {
            let r = compatible_antigravity_claude_thinking_signature(b(input));
            json!({"sig": r.clone().unwrap_or_default(), "ok": r.is_some()})
        }
        "recognized" => json!(is_recognized_reasoning_signature(b(input))),
        "split_prefix" => {
            let raw = b(input);
            match split_provider_prefix(&raw) {
                Some((p, rest)) => json!({"provider": p.as_str(), "rest": enc(rest), "ok": true}),
                None => json!({"provider": "unknown", "rest": enc(&raw), "ok": false}),
            }
        }
        "provider_from_model" => json!(provider_from_model_name(s(input)).as_str()),
        "inspect_cais" => {
            let r = inspect_claude_cais_signature(b(input));
            let info = r.as_ref().ok().map(|i| {
                json!({"first_byte": i.first_byte, "envelope_version": i.envelope_version, "channel_id": i.channel_id,
                    "model_text": i.model_text, "block_kind": i.block_kind, "context_id": i.context_id,
                    "signature_len": i.signature_len})
            });
            json!({"info": info, "err": err(&r)})
        }
        "normalize_claude" => {
            let r = normalize_claude_thinking_signature(b(&input["raw"]), claude_opt(&input["opt"]));
            json!({"sig": r.clone().unwrap_or_default(), "err": err(&r)})
        }
        "normalize_claude_native" => {
            let r = normalize_claude_provider_native_thinking_signature(b(&input["raw"]), claude_opt(&input["opt"]));
            json!({"sig": r.clone().unwrap_or_default(), "err": err(&r)})
        }
        "inspect_claude_payload" => {
            let payload = wire::STD.decode(s(&input["payload"]).as_bytes()).unwrap();
            tree_result(inspect_claude_signature_payload(
                &payload,
                input["layers"].as_i64().unwrap(),
            ))
        }
        "inspect_claude_double" => tree_result(inspect_claude_double_layer_signature(b(input))),
        "inspect_claude_single" => tree_result(inspect_claude_single_layer_signature(b(input))),
        "valid_claude" => json!(is_valid_claude_thinking_signature(
            b(&input["raw"]),
            claude_opt(&input["opt"])
        )),
        "decodable_claude" => json!(has_decodable_claude_thinking_signature(b(input))),
        "validate_claude" => err(&validate_claude_thinking_signatures(
            &b(&input["body"]),
            claude_opt(&input["opt"]),
        )),
        "inspect_gemini" => {
            let r = inspect_gemini_thought_signature(b(&input["raw"]), gemini_opt(&input["opt"]));
            let info = r.as_ref().ok().map(|i| {
                json!({"is_bypass_sentinel": i.is_bypass_sentinel, "bypass_sentinel": i.bypass_sentinel,
                    "decoded_len": i.decoded_len, "first_byte": i.first_byte, "has_observed_marker": i.has_observed_marker,
                    "known_envelope": i.known_envelope, "envelope": i.envelope.map_or("", GeminiEnvelope::as_str),
                    "record_count": i.record_count, "opaque_payload_len": i.opaque_payload_len})
            });
            json!({"info": info, "err": err(&r)})
        }
        "validate_gemini" => err(&validate_gemini_thought_signatures(
            &b(&input["body"]),
            gemini_opt(&input["opt"]),
        )),
        "validate_pairing" => err(&validate_gemini_function_call_pairing(&b(input))),
        "inspect_gpt" => {
            let r = inspect_gpt_reasoning_signature(b(input));
            let info = r
                .as_ref()
                .ok()
                .map(|i| json!({"decoded_len": i.decoded_len, "ciphertext_len": i.ciphertext_len}));
            json!({"info": info, "err": err(&r)})
        }
        "inspect_grok" => {
            let r = inspect_grok_encrypted_content(b(input));
            let info = r
                .as_ref()
                .ok()
                .map(|i| json!({"raw_len": i.raw_len, "decoded_len": i.decoded_len}));
            json!({"info": info, "err": err(&r)})
        }
        "inspect_kimi" => {
            let r = inspect_kimi_thinking_signature(b(input));
            let info = r
                .as_ref()
                .ok()
                .map(|i| json!({"raw_len": i.raw_len, "decoded_len": i.decoded_len, "mode": i.mode.as_str()}));
            json!({"info": info, "err": err(&r)})
        }
        "sanitize_gemini" => enc(&sanitize_gemini_request_thought_signatures(
            &b(&input["body"]),
            s(&input["path"]),
        )),
        "gemini_replay" => json!(gemini_replay_signature_or_bypass(
            b(&input["raw"]),
            block_kind(s(&input["block_kind"]))
        )),
        "sanitize_claude_messages" => {
            let target = s(&input["target"]);
            let (out, report) = sanitize_claude_messages_signatures_for_target(
                &b(&input["body"]),
                &ClaudeMessagesSanitizeOptions {
                    target_provider: provider(target),
                    target_model: s(&input["model"]).into(),
                    drop_empty_messages: input["drop_empty_messages"].as_bool().unwrap(),
                    drop_tool_signatures: input["drop_tool_signatures"].as_bool().unwrap(),
                    drop_empty_thinking_placeholders: input["drop_empty_thinking_placeholders"].as_bool().unwrap(),
                    preserve_empty_thinking_blocks: input["preserve_empty_thinking_blocks"].as_bool().unwrap(),
                },
            );
            json!({"body": enc(&out), "target": report.target_provider.as_str(), "preserved": report.preserved,
                "dropped_blocks": report.dropped_blocks, "dropped_signatures": report.dropped_signatures,
                "replaced_signatures": report.replaced_signatures,
                "decisions": report.decisions.iter().map(decision_json).collect::<Vec<_>>()})
        }
        "strip_claude" => enc(&strip_invalid_claude_thinking_blocks(
            &b(&input["body"]),
            claude_opt(&input["opt"]),
        )),
        "strip_claude_empty" => enc(&strip_invalid_claude_thinking_blocks_and_empty_messages(
            &b(&input["body"]),
            claude_opt(&input["opt"]),
        )),
        _ => return None,
    })
}

/// protobuf-go picks a regular or non-breaking space after "proto:" per binary
/// (internal/detrand) so callers cannot depend on it; the recording build chose U+00A0.
/// Only error strings are normalized, never bodies.
fn normalize_proto_separator(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                match value {
                    Value::String(text) if key == "err" => *text = text.replace("proto:\u{a0}", "proto: "),
                    other => normalize_proto_separator(other),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalize_proto_separator),
        _ => {}
    }
}

#[test]
fn replays_every_go_signature_call() {
    let ran = replay_all(crate::go_calls().map(str::to_owned));
    assert!(ran.len() >= 25, "{ran:?}");
}

/// Every corpus signature through every target, block kind and validator option
/// (tests/reference/record/signature_matrix_test.go.txt).
#[test]
fn replays_go_signature_matrix() {
    use std::io::BufRead;
    let gz = include_bytes!("../../tests/fixtures/go_signature_matrix.jsonl.gz");
    let lines = std::io::BufReader::new(flate2::read::GzDecoder::new(&gz[..])).lines();
    let ran = replay_all(lines.map(Result::unwrap));
    assert!(ran["decide"] > 9000, "{ran:?}");
}

fn replay_all(lines: impl Iterator<Item = String>) -> std::collections::BTreeMap<String, usize> {
    let mut ran = std::collections::BTreeMap::<String, usize>::new();
    let mut failures = Vec::new();
    for line in lines {
        let record: Value = serde_json::from_str(&line).unwrap();
        let fn_name = s(&record["fn"]);
        let Some(got) = replay(fn_name, &record["in"]) else {
            continue;
        };
        *ran.entry(fn_name.to_owned()).or_default() += 1;
        let mut want = record["out"].clone();
        if fn_name == "validate_pairing" || fn_name == "validate_claude" || fn_name == "validate_gemini" {
            // These return the error string itself as the output.
            if let Value::String(text) = &mut want {
                *text = text.replace("proto:\u{a0}", "proto: ");
            }
        }
        normalize_proto_separator(&mut want);
        if got != want {
            failures.push(format!(
                "{fn_name}\n  in:   {}\n  want: {want}\n  got:  {got}",
                record["in"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} signature records differ from Go:\n{}",
        failures.len(),
        ran.values().sum::<usize>(),
        failures.iter().take(15).cloned().collect::<Vec<_>>().join("\n")
    );
    ran
}

#[test]
fn length_caps_reject_before_decoding() {
    // The recorded Go cases for these caps exceed the fixture's 64 KiB line limit.
    let huge = format!("E{}", "A".repeat(MAX_CLAUDE_THINKING_SIGNATURE_LEN));
    assert_eq!(
        normalize_claude_thinking_signature(&huge, ClaudeValidation::default())
            .unwrap_err()
            .0,
        "signature exceeds maximum length (33554432 bytes)"
    );
    assert_eq!(
        inspect_gemini_thought_signature(&huge, GeminiValidation::default())
            .unwrap_err()
            .0,
        "Gemini thought signature exceeds maximum length (33554432 bytes)"
    );
    let exact = "A".repeat(MAX_CLAUDE_THINKING_SIGNATURE_LEN - 4) + "AAA=";
    assert!(inspect_gemini_thought_signature(&exact, GeminiValidation::default()).is_ok());
}

/// The two recorded sanitizer calls over 64 KiB (translator tests at 6fecc6e), rebuilt
/// byte for byte: large inline media must pass through unchanged and without copies of
/// the body being edited.
#[test]
fn large_inline_data_passes_through_like_go() {
    let cases = [
        (
            r#"{"project":"","request":{"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"image/png","data":""#,
            r#""}},{"text":"describe"}]}]},"model":"gemini-3-flash"}"#,
            4_194_464,
            "request.contents",
        ),
        (
            r#"{"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"video/mp4","data":""#,
            r#""}}]}],"safetySettings":[]}"#,
            20_971_630,
            "contents",
        ),
    ];
    for (prefix, suffix, total, path) in cases {
        let body = format!("{prefix}{}{suffix}", "A".repeat(total - prefix.len() - suffix.len()));
        assert_eq!(body.len(), total);
        assert_eq!(
            sanitize_gemini_request_thought_signatures(body.as_bytes(), path),
            body.as_bytes()
        );
    }
}
