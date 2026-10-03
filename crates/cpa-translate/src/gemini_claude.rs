//! Claude Messages request -> Gemini generateContent request
//! (internal/translator/gemini/claude/gemini_claude_request.go).

use crate::{
    Error, Registered, RequestCtx,
    common::{self, go_lower, sanitize_function_name, trim_space},
    gemini::{self, attach_default_safety_settings},
    gemini_chat_request::content_node,
    gemini_claude_response as response, openai_claude,
};
use cpa_common::json::{self as gj, Kind, Res};
use std::collections::HashMap;

pub static PAIR: Registered = registered!(
    Claude -> Gemini,
    request: |ctx, body| Ok(convert(ctx.model, body, false)),
    non_stream: response::non_stream,
    go_stream: response::go_stream,
    token_count: Some(openai_claude::claude_input_tokens),
);

/// ConvertClaudeRequestToGeminiWithCompat: compatibility endpoints keep assistant
/// thinking blocks (as thought parts) even without a Gemini signature.
pub fn request_with_compat(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    Ok(convert(ctx.model, body, true))
}

const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

fn text_part(text: &[u8]) -> Vec<u8> {
    let mut part = br#"{"text":""}"#.to_vec();
    gj::set_str(&mut part, "text", text);
    part
}

fn inline_part(mime: &[u8], data: &[u8]) -> Vec<u8> {
    let mut part = br#"{"inline_data":{"mime_type":"","data":""}}"#.to_vec();
    gj::set_str(&mut part, "inline_data.mime_type", mime);
    gj::set_str(&mut part, "inline_data.data", data);
    part
}

/// toolNameFromClaudeToolUseID: everything before the last `-`.
fn tool_name_from_id(id: &[u8]) -> Vec<u8> {
    match id.iter().rposition(|&c| c == b'-') {
        Some(i) => id[..i].to_vec(),
        None => vec![],
    }
}

fn convert(model: &str, raw: &[u8], preserve_thinking: bool) -> Vec<u8> {
    let mut out = br#"{"contents":[]}"#.to_vec();
    gj::set_str(&mut out, "model", model);

    let system = gj::get(raw, "system");
    if system.is_array() {
        let mut parts = vec![];
        system.each(|_, item| {
            let text = item.get("text");
            if item.get("type").bytes().as_ref() == b"text"
                && text.kind == Kind::String
                && !common::is_claude_code_attribution_text(&text.s)
            {
                parts.push(text_part(&text.s));
            }
            true
        });
        if !parts.is_empty() {
            gj::set_raw(&mut out, "systemInstruction", content_node("user", &parts));
        }
    } else if system.kind == Kind::String && !common::is_claude_code_attribution_text(&system.s) {
        let mut instruction = br#"{"parts":[]}"#.to_vec();
        gj::set_items(&mut instruction, "parts", &[text_part(&system.s)]);
        gj::set_raw(&mut out, "systemInstruction", instruction);
    }

    let messages = gj::get(raw, "messages");
    if messages.is_array() {
        let mut contents = vec![];
        let mut names: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        let mut pending: Vec<Vec<u8>> = vec![];
        messages.each(|_, message| {
            message_contents(&message, preserve_thinking, &mut names, &mut pending, &mut contents);
            true
        });
        if let Some(last) = contents.last() {
            let mut has_call = false;
            let last = gj::parse(last);
            if last.get("role").bytes().as_ref() == b"model" {
                last.get("parts").each(|_, part| {
                    has_call = part.get("functionCall").exists();
                    !has_call
                });
            }
            if has_call {
                contents.pop();
            }
        }
        gj::set_items(&mut out, "contents", &gemini::merge_adjacent_contents(contents));
    }

    let mut declarations = vec![];
    let mut has_strict = false;
    let tools = gj::get(raw, "tools");
    if tools.is_array() {
        tools.each(|_, tool| {
            if tool.get("strict").kind == Kind::True {
                has_strict = true;
            }
            declarations.extend(declaration(&tool));
            true
        });
        if !declarations.is_empty() {
            let mut node = br#"[{"functionDeclarations":[]}]"#.to_vec();
            gj::set_raw(&mut node, "0.functionDeclarations", gj::join(&declarations));
            gj::set_raw(&mut out, "tools", node);
        }
    }

    const MODE: &str = "toolConfig.functionCallingConfig.mode";
    let choice = gj::get(raw, "tool_choice");
    if choice.exists() && choice.kind != Kind::Null {
        let (kind, name) = if choice.is_object() {
            (
                choice.get("type").bytes().into_owned(),
                choice.get("name").bytes().into_owned(),
            )
        } else if choice.kind == Kind::String {
            (choice.s.to_vec(), vec![])
        } else {
            (vec![], vec![])
        };
        match kind.as_slice() {
            b"auto" => {
                gj::set_str(&mut out, MODE, if has_strict { "VALIDATED" } else { "AUTO" });
            }
            b"none" => {
                gj::set_str(&mut out, MODE, "NONE");
            }
            b"any" => {
                gj::set_str(&mut out, MODE, "ANY");
            }
            b"tool" => {
                gj::set_str(&mut out, MODE, "ANY");
                if !name.is_empty() {
                    gj::set_strs(
                        &mut out,
                        "toolConfig.functionCallingConfig.allowedFunctionNames",
                        &[sanitize_function_name(&name)],
                    );
                }
            }
            _ => {}
        }
    } else if has_strict && !declarations.is_empty() {
        gj::set_str(&mut out, MODE, "VALIDATED");
    }

    let thinking = gj::get(raw, "thinking");
    if thinking.is_object() {
        match thinking.get("type").bytes().as_ref() {
            b"enabled" => {
                let budget = thinking.get("budget_tokens");
                if budget.kind == Kind::Number {
                    gj::set_int(&mut out, "generationConfig.thinkingConfig.thinkingBudget", budget.int());
                }
            }
            b"adaptive" | b"auto" => {
                let effort = gj::get(raw, "output_config.effort");
                let effort = if effort.kind == Kind::String {
                    go_lower(trim_space(&effort.s))
                } else {
                    vec![]
                };
                if !effort.is_empty() {
                    gj::set_str(&mut out, "generationConfig.thinkingConfig.thinkingLevel", effort);
                } else {
                    let max = crate::thinking::lookup_model_info(model, "gemini")
                        .and_then(|m| m.thinking)
                        .map_or(0, |t| t.max);
                    if max > 0 {
                        gj::set_int(&mut out, "generationConfig.thinkingConfig.thinkingBudget", max);
                    } else {
                        gj::set_str(&mut out, "generationConfig.thinkingConfig.thinkingLevel", "high");
                    }
                }
            }
            _ => {}
        }
    }
    for (from, to) in [
        ("temperature", "generationConfig.temperature"),
        ("top_p", "generationConfig.topP"),
        ("top_k", "generationConfig.topK"),
    ] {
        let v = gj::get(raw, from);
        if v.kind == Kind::Number {
            gj::set_f64(&mut out, to, v.num);
        }
    }
    attach_default_safety_settings(out, "safetySettings")
}

/// A Claude tool as a Gemini function declaration (only tools with an object
/// `input_schema`).
fn declaration(tool: &Res<'_>) -> Option<Vec<u8>> {
    let schema = tool.get("input_schema");
    if !schema.is_object() {
        return None;
    }
    let cleaned = cpa_common::gemini_schema::for_gemini_json_schema(&schema.raw);
    let decl = gj::try_delete(&tool.raw, "input_schema").ok()?;
    let mut decl = gj::try_set_raw(&decl, "parametersJsonSchema", cleaned).ok()?;
    for path in [
        "strict",
        "input_examples",
        "type",
        "cache_control",
        "defer_loading",
        "eager_input_streaming",
    ] {
        if tool.get(path).exists() {
            gj::delete(&mut decl, path);
        }
    }
    let name = tool.get("name");
    let original = name.bytes();
    let sanitized = sanitize_function_name(&original);
    if name.kind != Kind::String || sanitized != *original {
        gj::set_str(&mut decl, "name", sanitized);
    }
    (gj::valid(&decl) && gj::parse(&decl).is_object()).then_some(decl)
}

fn message_contents(
    message: &Res<'_>,
    preserve_thinking: bool,
    names: &mut HashMap<Vec<u8>, Vec<u8>>,
    pending: &mut Vec<Vec<u8>>,
    contents: &mut Vec<Vec<u8>>,
) {
    let role_res = message.get("role");
    if role_res.kind != Kind::String {
        return;
    }
    let original = role_res.s.to_vec();
    let system_like = original == b"system" || original == b"developer";
    let preceding = if system_like { vec![] } else { std::mem::take(pending) };
    let role: Vec<u8> = match original.as_slice() {
        b"assistant" => b"model".to_vec(),
        b"system" | b"developer" => b"user".to_vec(),
        other => other.to_vec(),
    };
    let content = message.get("content");
    if system_like {
        if let Some(text) = common::claude_message_system_reminder_text(&content) {
            contents.push(content_node(&role, &[text_part(&text)]));
        }
        return;
    }
    if content.kind == Kind::String {
        contents.push(content_node(&role, &[text_part(&content.s)]));
        return;
    }
    if !content.is_array() {
        return;
    }
    let mut blocks = vec![];
    content.each(|_, block| {
        blocks.push(block);
        true
    });
    if original == b"user" {
        blocks = common::align_claude_tool_results(blocks, &preceding);
    }
    let mut parts = vec![];
    for block in &blocks {
        match block.get("type").bytes().as_ref() {
            b"text" => {
                let text = block.get("text").bytes();
                if !text.is_empty() {
                    parts.push(text_part(&text));
                }
            }
            b"thinking" if preserve_thinking => {
                use cpa_common::signature::{BlockKind, gemini_replay_signature_or_bypass};
                let mut part = br#"{"text":"","thought":true,"thoughtSignature":""}"#.to_vec();
                gj::set_str(&mut part, "text", block.get("thinking").bytes());
                let signature =
                    gemini_replay_signature_or_bypass(block.get("signature").bytes(), BlockKind::GeminiModelPart);
                gj::set_str(&mut part, "thoughtSignature", signature);
                parts.push(part);
            }
            b"tool_use" => {
                let name = block.get("name").bytes().into_owned();
                let id = block.get("id").bytes().into_owned();
                if !id.is_empty() && !name.is_empty() {
                    names.insert(id.clone(), name.clone());
                }
                let name = sanitize_function_name(&name);
                let args = block.get("input").bytes().into_owned();
                if gj::parse(&args).is_object() && gj::valid(&args) {
                    let mut part = br#"{"thoughtSignature":"","functionCall":{"name":"","args":{}}}"#.to_vec();
                    gj::set_str(&mut part, "thoughtSignature", SKIP_SIGNATURE);
                    if !id.is_empty() {
                        gj::set_str(&mut part, "functionCall.id", &id);
                    }
                    gj::set_str(&mut part, "functionCall.name", &name);
                    gj::set_raw(&mut part, "functionCall.args", &args);
                    parts.push(part);
                    if original == b"assistant" {
                        pending.push(id);
                    }
                }
            }
            b"tool_result" => {
                let id = block.get("tool_use_id").bytes().into_owned();
                if id.is_empty() {
                    continue;
                }
                let mut name = names.get(&id).cloned().unwrap_or_default();
                if name.is_empty() {
                    name = tool_name_from_id(&id);
                }
                if name.is_empty() {
                    name = id.clone();
                }
                let result = gemini::claude_tool_result(&block.get("content"));
                let mut part = br#"{"functionResponse":{"name":"","response":{"result":""}}}"#.to_vec();
                gj::set_str(&mut part, "functionResponse.id", &id);
                gj::set_str(&mut part, "functionResponse.name", sanitize_function_name(&name));
                if result.raw {
                    gemini::set_function_response_raw(&mut part, "functionResponse.response.result", &result.result);
                } else {
                    gj::set_str(&mut part, "functionResponse.response.result", &result.result);
                }
                parts.push(part);
                for (mime, data) in &result.images {
                    parts.push(inline_part(mime, data));
                }
            }
            b"image" => {
                let source = block.get("source");
                if source.get("type").bytes().as_ref() != b"base64" {
                    continue;
                }
                let (mime, data) = (source.get("media_type").bytes(), source.get("data").bytes());
                if !mime.is_empty() && !data.is_empty() {
                    parts.push(inline_part(&mime, &data));
                }
            }
            _ => {}
        }
    }
    if role == b"user" {
        parts = gemini::reorder_user_parts(parts);
    }
    contents.push(content_node(&role, &parts));
}
