//! Replays `tests/fixtures/go_helpers.json` against the Rust ports of Go's translator
//! helpers. Each record is a call one of Go's own tests made to a helper, with the
//! arguments and results Go saw (recorded by `tests/reference/harvest` in value mode; see
//! tests/reference/README.md). Arguments arrive tagged: `s` string, `sb`/`b` base64 bytes
//! (`b` also carries Go's nil), `json` a gjson.Result (`jb` its raw bytes), `bool`, `err`
//! an error, `null` a nil interface.

use std::collections::HashMap;

use base64::Engine;
use cpa_common::json as gj;
use serde_json::Value;

const ERR: &[u8] = b"<error>";

fn bytes(v: &Value) -> Vec<u8> {
    let b64 = |s: &Value| {
        base64::engine::general_purpose::STANDARD
            .decode(s.as_str().unwrap())
            .unwrap()
    };
    if let Some(s) = v.get("s") {
        s.as_str().unwrap().as_bytes().to_vec()
    } else if let Some(s) = v.get("sb").or_else(|| v.get("b")).or_else(|| v.get("jb")) {
        b64(s)
    } else {
        panic!("not a byte value: {v}")
    }
}

fn names(v: &Value) -> HashMap<Vec<u8>, Vec<u8>> {
    let map = v["map"].as_object().map(|m| m.iter()).into_iter().flatten();
    map.map(|(k, v)| (k.as_bytes().to_vec(), v.as_str().unwrap().as_bytes().to_vec()))
        .collect()
}

fn list(v: &Value) -> Vec<Vec<u8>> {
    v["list"].as_array().unwrap().iter().map(bytes).collect()
}

/// A list of byte strings, length-prefixed so item boundaries count. Go's nil and empty
/// lists compare equal.
fn list_out(items: &[Vec<u8>]) -> Vec<u8> {
    items
        .iter()
        .flat_map(|i| [format!("{}:", i.len()).into_bytes(), i.clone(), b"\n".to_vec()])
        .collect::<Vec<_>>()
        .concat()
}

/// Runs an in-place Rust transform on a copy of `raw`.
fn edit(raw: Vec<u8>, f: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut raw = raw;
    f(&mut raw);
    raw
}

fn flag(v: &Value) -> Vec<u8> {
    bool_out(v["bool"].as_bool().unwrap())
}

fn bool_out(b: bool) -> Vec<u8> {
    if b { b"true".to_vec() } else { b"false".to_vec() }
}

fn failed(v: &Value) -> bool {
    v.get("err").is_some()
}

/// (got, want) for one recorded call; `None` for a function without a replay arm.
fn replay(fun: &str, a: &[Value], r: &[Value]) -> Option<(Vec<u8>, Vec<u8>)> {
    let arg = |i: usize| bytes(&a[i]);
    let want = || bytes(&r[0]);
    Some(match fun {
        "IsCustomTool" => (
            bool_out(crate::apply_patch::is_custom_tool(&gj::parse(&arg(0)))),
            flag(&r[0]),
        ),
        "Description" => (crate::apply_patch::description(&gj::parse(&arg(0))), want()),
        "WrapInput" => (crate::apply_patch::wrap_input(&arg(0)), want()),
        "UnwrapInput" => (
            crate::apply_patch::unwrap_input(&arg(0)).map_or(ERR.to_vec(), String::into_bytes),
            if failed(&r[1]) { ERR.to_vec() } else { want() },
        ),
        "EscapeInputFragment" => (crate::apply_patch::escape_input_fragment(&arg(0)), want()),
        "Parameters" => (crate::apply_patch::PARAMETERS.as_bytes().to_vec(), want()),
        "DeriveClaudeUserID" => (crate::common::derive_claude_user_id(&arg(0)), want()),
        "AttachCacheControl" => (crate::common::attach_cache_control(arg(0), &gj::parse(&arg(1))), want()),
        "AttachMessageCacheControl" => (
            crate::common::attach_message_cache_control(arg(0), &gj::parse(&arg(1))),
            want(),
        ),
        "AttachToolMessageCacheControl" => (
            crate::common::attach_tool_message_cache_control(arg(0), &gj::parse(&arg(1))),
            want(),
        ),
        "SystemReminderText" => (crate::common::system_reminder_text(&arg(0)), want()),
        "RequestModelName" => (crate::common::request_model_name(&arg(0), &arg(1)), want()),
        "ContainsJSONRef" => (
            bool_out(crate::gemini::contains_json_ref(&gj::parse(&arg(0)))),
            flag(&r[0]),
        ),
        "BuildClaudeStructuredOutputInstruction" => (
            crate::common::claude_structured_output_instruction(&gj::parse(&arg(0))),
            want(),
        ),
        "AntigravityToolNameToUpstream" => (
            crate::openai_interactions::antigravity_name_to_upstream(&arg(0)),
            want(),
        ),
        "AntigravityUpstreamToolNameToClient" => {
            (crate::openai_interactions::antigravity_name_to_client(&arg(0)), want())
        }
        "ObfuscateExecCommandDescription" => (
            crate::responses_interactions::obfuscate_exec_command_description(&arg(0)),
            want(),
        ),
        "ObfuscateWriteStdinDescription" => (
            crate::responses_interactions::obfuscate_write_stdin_description(&arg(0)),
            want(),
        ),
        "IsDevinCodexAppAutomationUpdate" => (
            bool_out(crate::responses_interactions::is_devin_automation_update(
                &arg(0),
                &arg(1),
            )),
            flag(&r[0]),
        ),
        "SanitizeDevinToolDescription" => (
            crate::responses_interactions::sanitize_devin_description(&arg(0), arg(1)),
            want(),
        ),
        "NormalizeOpenAIFileData" => {
            let pair = |mime: Vec<u8>, data: Vec<u8>| [mime, b"\n".to_vec(), data].concat();
            let got = crate::common::normalize_openai_file_data(&arg(0), &arg(1), &arg(2))
                .map_or(ERR.to_vec(), |(mime, data)| pair(mime, data));
            let ok = r[2]["bool"].as_bool().unwrap();
            (
                got,
                if ok {
                    pair(bytes(&r[0]), bytes(&r[1]))
                } else {
                    ERR.to_vec()
                },
            )
        }
        "NormalizeClaudeToolInputSchema" => {
            let schema = arg(0);
            let nil = a[0]["nil"].as_bool().unwrap_or(false);
            (
                crate::common::normalize_claude_tool_input_schema((!nil).then_some(&schema[..])),
                want(),
            )
        }
        "HasUnsupportedUnicodePropertyEscape" => (
            bool_out(crate::common::has_unsupported_unicode_property_escape(&arg(0))),
            flag(&r[0]),
        ),
        "GeminiClaudeToolUseID" => (
            crate::antigravity_claude_response::gemini_claude_tool_use_id(&arg(0), &arg(1), &arg(2)),
            want(),
        ),
        "IsClaudeCodeAttributionSystemText" => (
            bool_out(crate::common::is_claude_code_attribution_text(&arg(0))),
            flag(&r[0]),
        ),
        "fixCLIToolResponse" => (
            crate::antigravity_gemini::fix_cli_tool_response(&arg(0)).unwrap_or_else(|| ERR.to_vec()),
            if failed(&r[1]) { ERR.to_vec() } else { want() },
        ),
        "removeEmptyGeminiFunctionTools" => (
            edit(arg(0), crate::antigravity_gemini::remove_empty_function_tools),
            want(),
        ),
        "rewriteGeminiFunctionNames" => {
            // The arguments of the Rust call site in antigravity_gemini::convert.
            let got = edit(arg(0), |raw| {
                crate::antigravity_gemini::rewrite_function_names(
                    raw,
                    &names(&a[1]),
                    "request.contents",
                    &crate::antigravity_gemini::NAME_FIELDS,
                    &[
                        "request.toolConfig.functionCallingConfig.allowedFunctionNames",
                        "request.tool_config.function_calling_config.allowed_function_names",
                    ],
                )
            });
            (got, want())
        }
        "rewriteInteractionsFunctionNames" => {
            // Rust rewrites before wrapping the request (`contents`); Go after (`request.contents`).
            let got = edit(arg(0), |raw| {
                crate::antigravity_gemini::rewrite_function_names(
                    raw,
                    &names(&a[1]),
                    "request.contents",
                    &crate::antigravity_interactions::NAME_FIELDS,
                    &["request.toolConfig.functionCallingConfig.allowedFunctionNames"],
                )
            });
            (got, want())
        }
        "normalizeAntigravityOpenAIThinkingConfig" => {
            (edit(arg(0), crate::antigravity_chat::normalize_thinking_config), want())
        }
        "normalizeClaudeToolSchema" => (crate::claude_gemini::normalize_schema(&gj::parse(&arg(0))), want()),
        "lowercaseClaudeToolSchemaTypes" => (crate::claude_gemini::lowercase_types(arg(0)), want()),
        "cleanGeminiCodexToolParameters" => (crate::codex_gemini::clean_parameters(&gj::parse(&arg(0))), want()),
        "cleanedCodexToolParameters" => (
            crate::codex_interactions::cleaned_parameters(&gj::parse(&arg(0))),
            want(),
        ),
        "setInteractionsCodexRawIfDifferent" => {
            let path = String::from_utf8(arg(1)).unwrap();
            let value = arg(2);
            (
                edit(arg(0), |raw| {
                    crate::codex_interactions::set_raw_if_different(raw, &path, &gj::parse(&value))
                }),
                want(),
            )
        }
        "MergeAdjacentGeminiContents" => (
            list_out(&crate::gemini::merge_adjacent_contents(list(&a[0]))),
            list_out(&list(&r[0])),
        ),
        "ContentHasGeminiFunctionResponse" => (bool_out(crate::gemini::has_function_response(&arg(0))), flag(&r[0])),
        "ReorderGeminiUserParts" => (
            list_out(&crate::gemini::reorder_user_parts(list(&a[0]))),
            list_out(&list(&r[0])),
        ),
        "MergeAdjacentGeminiUserContents" => (
            list_out(&crate::gemini_responses::merge_adjacent_user_contents(list(&a[0]))),
            list_out(&list(&r[0])),
        ),
        "SplitGeminiFunctionResponseTurns" => (
            list_out(&crate::antigravity_claude::split_function_response_turns(list(&a[0]))),
            list_out(&list(&r[0])),
        ),
        "SetGeminiFunctionResponseResult" => {
            let path = String::from_utf8(arg(1)).unwrap();
            let value = arg(2);
            (
                edit(arg(0), |part| {
                    crate::gemini::set_function_response_result(part, &path, &gj::parse(&value))
                }),
                want(),
            )
        }
        "AlignOpenAIToolCallMessages" => (
            list_out(&crate::common::align_openai_tool_call_messages_with(
                list(&a[0]),
                &list(&a[1]),
            )),
            list_out(&list(&r[0])),
        ),
        "restoreUsageMetadata" => (crate::antigravity_gemini::restore_usage(arg(0)), want()),
        "normalizeToolParameters" => (crate::codex_claude::normalize_tool_parameters(&arg(0)), want()),
        _ => return None,
    })
}

#[test]
fn go_helper_calls_match() {
    let doc: Value = serde_json::from_str(include_str!("../tests/fixtures/go_helpers.json")).unwrap();
    let calls = doc["calls"].as_array().unwrap();
    assert!(calls.len() > 200, "fixture holds {} calls", calls.len());
    let mut failures = Vec::new();
    for call in calls {
        let name = call["name"].as_str().unwrap();
        let fun = call["fn"].as_str().unwrap().rsplit('.').next().unwrap();
        let (args, results) = (call["args"].as_array().unwrap(), call["results"].as_array().unwrap());
        match replay(fun, args, results) {
            Some((got, want)) if got != want => failures.push(format!(
                "{name} {fun}:\n   got {}\n  want {}",
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(&want)
            )),
            Some(_) => {}
            None => failures.push(format!("{name}: no replay arm for {fun}")),
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} calls differ:\n{}",
        failures.len(),
        calls.len(),
        failures.join("\n")
    );
}
