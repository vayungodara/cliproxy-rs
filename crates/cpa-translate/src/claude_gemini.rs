//! Gemini client, Claude upstream (internal/translator/claude/gemini):
//! ConvertGeminiRequestToClaude, ConvertClaudeResponseToGemini, its NonStream and
//! GeminiTokenCount.

use std::collections::HashMap;

use cpa_common::json::{self as gj, AnyValue, Kind, Res};
use cpa_common::signature::{BlockKind, gemini_replay_signature_or_bypass};

use crate::claude_interactions::Accumulator;
use crate::common::{
    derive_claude_user_id, format_rfc3339_utc, go_lower, is_gemini_thought_part, now_unix,
    sanitize_claude_function_name, trim_space,
};
use crate::stream::GoStream;
use crate::{Error, Registered, ResponseCtx, thinking};

pub static PAIR: Registered = registered!(
    Gemini -> Claude,
    request: |ctx, body| Ok(convert(ctx.model, body, ctx.stream)),
    non_stream: non_stream,
    go_stream: go_stream,
    token_count: Some(crate::gemini::token_count),
);

fn string(res: &Res<'_>) -> Vec<u8> {
    res.bytes().into_owned()
}

// ---------------------------------------------------------------------------------------
// Request

/// ConvertGeminiRequestToClaude.
fn convert(model: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    let mut out = br#"{"model":"","max_tokens":32000,"messages":[],"metadata":{}}"#.to_vec();
    gj::set_str(&mut out, "metadata.user_id", derive_claude_user_id(raw));
    let root = gj::parse(raw);
    gj::set_str(&mut out, "model", model);
    let tier = root.get("service_tier");
    if tier.kind == Kind::String {
        gj::set_str(&mut out, "service_tier", tier.bytes());
    }
    let config = root.get("generationConfig");
    if config.exists() {
        copy_generation_config(&mut out, &config, model);
    }
    let mut messages = Accumulator::default();
    let parts = root.get("system_instruction.parts");
    if root.get("system_instruction").exists() && parts.is_array() {
        let mut system = vec![];
        parts.each(|_, part| {
            if is_gemini_thought_part(&part) {
                return true;
            }
            let text = part.get("text");
            if text.exists() {
                if !system.is_empty() {
                    system.push(b'\n');
                }
                system.extend_from_slice(&text.bytes());
            }
            true
        });
        if !system.is_empty() {
            let mut message = br#"{"role":"user","content":[{"type":"text","text":""}]}"#.to_vec();
            gj::set_str(&mut message, "content.0.text", &system);
            messages.append(&message);
            messages.flush();
        }
    }
    let contents = root.get("contents");
    if contents.is_array() {
        let mut ids = ToolIds::default();
        contents.each(|_, content| {
            let role = match content.get("role").bytes().as_ref() {
                b"model" => b"assistant".to_vec(),
                b"function" | b"tool" => b"user".to_vec(),
                other => other.to_vec(),
            };
            let mut items = vec![];
            let parts = content.get("parts");
            if parts.is_array() {
                parts.each(|_, part| {
                    items.extend(ids.part(&part, &role));
                    true
                });
            }
            if !items.is_empty() {
                let mut message = br#"{"role":"","content":[]}"#.to_vec();
                gj::set_str(&mut message, "role", &role);
                gj::set_raw(&mut message, "content", gj::join(&items));
                messages.append(&message);
            }
            true
        });
    }
    gj::set_items(&mut out, "messages", &messages.into_messages());
    copy_tools(&mut out, &root);
    let tool_config = root.get("tool_config");
    if tool_config.exists() {
        set_tool_choice(&mut out, &tool_config.get("function_calling_config"));
    } else {
        let tool_config = root.get("toolConfig");
        if tool_config.exists() {
            set_tool_choice(&mut out, &tool_config.get("functionCallingConfig"));
        }
    }
    gj::set_bool(&mut out, "stream", stream);
    thinking::apply_translated_summary_to_claude(out, raw, "gemini", model)
}

/// generationConfig: max tokens, top_p, stop sequences and thinking.
fn copy_generation_config(out: &mut Vec<u8>, config: &Res<'_>, model: &str) {
    let max = config.get("maxOutputTokens");
    if max.exists() {
        gj::set_int(out, "max_tokens", max.int());
    }
    let top_p = config.get("topP");
    if top_p.exists() {
        gj::set_f64(out, "top_p", top_p.float());
    }
    let stops = config.get("stopSequences");
    if stops.is_array() {
        let values: Vec<Vec<u8>> = stops.array().iter().map(string).collect();
        if !values.is_empty() {
            gj::set_strs(out, "stop_sequences", &values);
        }
    }
    let thinking_config = config.get("thinkingConfig");
    if !thinking_config.is_object() {
        return;
    }
    let levels = thinking::lookup_model_info(model, "claude")
        .and_then(|m| m.thinking)
        .map(|t| t.levels)
        .unwrap_or_default();
    let adaptive = !levels.is_empty();
    let supports_max = adaptive && thinking::has_level(&levels, "max");
    let set_adaptive = |out: &mut Vec<u8>, level: &[u8]| {
        let mapped = std::str::from_utf8(level)
            .ok()
            .and_then(|l| thinking::map_to_claude_effort(l, supports_max));
        gj::set_str(out, "thinking.type", "adaptive");
        gj::delete(out, "thinking.budget_tokens");
        gj::set_str(out, "output_config.effort", mapped.map_or(level, str::as_bytes));
    };
    let disable = |out: &mut Vec<u8>| {
        gj::set_str(out, "thinking.type", "disabled");
        gj::delete(out, "thinking.budget_tokens");
        if adaptive {
            gj::delete(out, "output_config.effort");
        }
    };
    let mut level = thinking_config.get("thinkingLevel");
    if !level.exists() {
        level = thinking_config.get("thinking_level");
    }
    if level.exists() {
        let level = go_lower(trim_space(&level.bytes()));
        match level.as_slice() {
            b"" => {}
            b"none" => disable(out),
            _ if adaptive => set_adaptive(out, &level),
            b"auto" => {
                gj::set_str(out, "thinking.type", "enabled");
                gj::delete(out, "thinking.budget_tokens");
            }
            _ => {
                let budget = std::str::from_utf8(&level)
                    .ok()
                    .and_then(thinking::convert_level_to_budget);
                if let Some(budget) = budget {
                    gj::set_str(out, "thinking.type", "enabled");
                    gj::set_int(out, "thinking.budget_tokens", budget);
                }
            }
        }
        return;
    }
    let mut budget = thinking_config.get("thinkingBudget");
    if !budget.exists() {
        budget = thinking_config.get("thinking_budget");
    }
    if !budget.exists() {
        return;
    }
    let budget = budget.int();
    match budget {
        0 => disable(out),
        _ if adaptive => {
            if let Some(level) = thinking::convert_budget_to_level(budget) {
                set_adaptive(out, level.as_bytes());
            }
        }
        -1 => {
            gj::set_str(out, "thinking.type", "enabled");
            gj::delete(out, "thinking.budget_tokens");
        }
        _ => {
            gj::set_str(out, "thinking.type", "enabled");
            gj::set_int(out, "thinking.budget_tokens", budget);
        }
    }
}

/// Tool-use IDs paired across turns: generated in sequence when Gemini omits them, and
/// handed to responses in call order.
#[derive(Default)]
struct ToolIds {
    pending: Vec<Vec<u8>>,
    counter: u64,
}

/// getGeminiToolID: the trimmed `id`, else the trimmed `call_id`.
fn gemini_tool_id(value: &Res<'_>) -> Vec<u8> {
    let id = trim_space(&value.get("id").bytes()).to_vec();
    if !id.is_empty() {
        return id;
    }
    trim_space(&value.get("call_id").bytes()).to_vec()
}

impl ToolIds {
    fn generate(&mut self) -> Vec<u8> {
        self.counter += 1;
        format!("toolu_gemini_{:016}", self.counter).into_bytes()
    }

    /// One Gemini part as Claude content, if it converts.
    fn part(&mut self, part: &Res<'_>, role: &[u8]) -> Option<Vec<u8>> {
        if is_gemini_thought_part(part) {
            return None;
        }
        let text = part.get("text");
        if text.exists() {
            let mut out = br#"{"type":"text","text":""}"#.to_vec();
            gj::set_str(&mut out, "text", text.bytes());
            return Some(out);
        }
        let call = part.get("functionCall");
        if call.exists() && role == b"assistant" {
            let mut out = br#"{"type":"tool_use","id":"","name":"","input":{}}"#.to_vec();
            let mut id = gemini_tool_id(&call);
            if id.is_empty() {
                id = self.generate();
            }
            self.pending.push(id.clone());
            gj::set_str(&mut out, "id", &id);
            let name = call.get("name");
            if name.exists() {
                gj::set_str(&mut out, "name", sanitize_claude_function_name(&name.bytes()));
            }
            let args = call.get("args");
            if args.is_object() {
                gj::set_raw(&mut out, "input", &args.raw);
            }
            return Some(out);
        }
        let response = part.get("functionResponse");
        if response.exists() {
            let mut out = br#"{"type":"tool_result","tool_use_id":"","content":""}"#.to_vec();
            let custom = gemini_tool_id(&response);
            let id = if !custom.is_empty() {
                if let Some(i) = self.pending.iter().position(|p| *p == custom) {
                    self.pending.remove(i);
                }
                custom
            } else if !self.pending.is_empty() {
                self.pending.remove(0)
            } else {
                self.generate()
            };
            gj::set_str(&mut out, "tool_use_id", &id);
            let result = response.get("response.result");
            let body = response.get("response");
            if result.exists() {
                gj::set_str(&mut out, "content", result.bytes());
            } else if body.exists() {
                gj::set_str(&mut out, "content", &body.raw);
            }
            return Some(out);
        }
        let inline = first(part, "inlineData", "inline_data");
        if inline.exists() {
            return inline_part(&inline);
        }
        let file = first(part, "fileData", "file_data");
        if file.exists() {
            return file_part(&file);
        }
        None
    }
}

fn first<'a>(value: &Res<'a>, a: &str, b: &str) -> Res<'a> {
    let found = value.get(a);
    if found.exists() { found } else { value.get(b) }
}

fn text_part(text: &[u8]) -> Vec<u8> {
    let mut out = br#"{"type":"text","text":""}"#.to_vec();
    gj::set_str(&mut out, "text", text);
    out
}

fn first_string(value: &Res<'_>, a: &str, b: &str) -> Vec<u8> {
    let found = string(&value.get(a));
    if found.is_empty() { string(&value.get(b)) } else { found }
}

/// Image and document MIME types (by lowercase prefix).
fn media_kind(mime: &[u8]) -> Option<&'static str> {
    let lower = go_lower(mime);
    if lower.starts_with(b"image/") {
        Some("image")
    } else if lower.starts_with(b"application/") || lower.starts_with(b"text/") {
        Some("document")
    } else {
        None
    }
}

/// claudeContentPartFromGeminiInlineData.
fn inline_part(inline: &Res<'_>) -> Option<Vec<u8>> {
    let mime = first_string(inline, "mimeType", "mime_type");
    let data = string(&inline.get("data"));
    if mime.is_empty() || data.is_empty() {
        return None;
    }
    Some(match media_kind(&mime) {
        Some(kind) => {
            let mut out = br#"{"type":"","source":{"type":"base64","media_type":"","data":""}}"#.to_vec();
            gj::set_str(&mut out, "type", kind);
            gj::set_str(&mut out, "source.media_type", &mime);
            gj::set_str(&mut out, "source.data", &data);
            out
        }
        None => text_part(&[&b"Media content: inline data (Type: "[..], &mime, b")"].concat()),
    })
}

/// claudeContentPartFromGeminiFileData.
fn file_part(file: &Res<'_>) -> Option<Vec<u8>> {
    let uri = first_string(file, "fileUri", "file_uri");
    if uri.is_empty() {
        return None;
    }
    let mime = first_string(file, "mimeType", "mime_type");
    Some(match media_kind(&mime) {
        Some("image") => {
            let mut out = br#"{"type":"image","source":{"type":"url","url":""}}"#.to_vec();
            gj::set_str(&mut out, "source.url", &uri);
            out
        }
        Some(_) => {
            let mut out = br#"{"type":"document","source":{"type":"url","url":""}}"#.to_vec();
            gj::set_str(&mut out, "source.url", &uri);
            if !mime.is_empty() {
                gj::set_str(&mut out, "source.media_type", &mime);
            }
            out
        }
        None => {
            let mut info = [&b"File: "[..], &uri].concat();
            if !mime.is_empty() {
                info.extend_from_slice(b" (Type: ");
                info.extend_from_slice(&mime);
                info.push(b')');
            }
            text_part(&info)
        }
    })
}

const DRAFT_07: &[u8] = b"http://json-schema.org/draft-07/schema#";

/// normalizeClaudeToolSchema: closed objects with the draft-07 `$schema`.
pub(crate) fn normalize_schema(params: &Res<'_>) -> Vec<u8> {
    let mut cleaned = params.raw.to_vec();
    if params.get("additionalProperties").kind != Kind::False {
        gj::set_bool(&mut cleaned, "additionalProperties", false);
    }
    let schema = params.get("$schema");
    if schema.kind != Kind::String || schema.bytes().as_ref() != DRAFT_07 {
        gj::set_str(&mut cleaned, "$schema", DRAFT_07);
    }
    cleaned
}

/// lowercaseClaudeToolSchemaTypes: every `type` value as a lowercase string.
pub(crate) fn lowercase_types(mut tool: Vec<u8>) -> Vec<u8> {
    for path in cpa_common::gemini_schema::walk(&gj::parse(&tool), b"type") {
        let value = gj::get(&tool, &path);
        let lower = go_lower(&value.bytes());
        if value.kind == Kind::String && lower == value.bytes().as_ref() {
            continue;
        }
        gj::set_str(&mut tool, &path, &lower);
    }
    tool
}

/// The Gemini function declarations as Claude tools (decoded and re-marshaled as Go's
/// `[]interface{}` of gjson values).
fn copy_tools(out: &mut Vec<u8>, root: &Res<'_>) {
    let tools = root.get("tools");
    if !tools.is_array() {
        return;
    }
    let mut converted = vec![];
    tools.each(|_, tool| {
        let decls = tool.get("functionDeclarations");
        if !decls.is_array() {
            return true;
        }
        decls.each(|_, decl| {
            let mut item = br#"{"name":"","description":"","input_schema":{"type":"object","properties":{}}}"#.to_vec();
            let name = decl.get("name");
            if name.exists() {
                gj::set_str(&mut item, "name", sanitize_claude_function_name(&name.bytes()));
            }
            let description = decl.get("description");
            if description.exists() {
                gj::set_str(&mut item, "description", description.bytes());
            }
            let params = first(&decl, "parameters", "parametersJsonSchema");
            if params.exists() {
                gj::set_raw(&mut item, "input_schema", normalize_schema(&params));
            } else {
                gj::set_raw(&mut item, "input_schema", br#"{"type":"object","properties":{}}"#);
            }
            let item = lowercase_types(item);
            converted.push(AnyValue::from_res(&gj::parse(&item)));
            true
        });
        true
    });
    if !converted.is_empty() {
        AnyValue::Array(converted).set(out, "tools");
    }
}

/// setClaudeToolChoiceFromGeminiToolConfig.
fn set_tool_choice(out: &mut Vec<u8>, calling: &Res<'_>) {
    let mode = calling.get("mode");
    if !mode.exists() {
        return;
    }
    match mode.bytes().as_ref() {
        b"AUTO" => {
            gj::set_raw(out, "tool_choice", br#"{"type":"auto"}"#);
        }
        b"NONE" => {
            gj::set_raw(out, "tool_choice", br#"{"type":"none"}"#);
        }
        b"ANY" => {
            let allowed = first(calling, "allowedFunctionNames", "allowed_function_names");
            let names = allowed.array();
            if allowed.is_array() && names.len() == 1 {
                let mut choice = br#"{"type":"tool","name":""}"#.to_vec();
                gj::set_str(&mut choice, "name", sanitize_claude_function_name(&names[0].bytes()));
                gj::set_raw(out, "tool_choice", choice);
            } else {
                gj::set_raw(out, "tool_choice", br#"{"type":"any"}"#);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------
// Responses

/// ConvertAnthropicResponseToGeminiParams.
#[derive(Default)]
struct State {
    model: Vec<u8>,
    created_at: i64,
    response_id: Vec<u8>,
    tool_names: HashMap<i64, Vec<u8>>,
    tool_args: HashMap<i64, Vec<u8>>,
    tool_ids: HashMap<i64, Vec<u8>>,
}

fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(State {
        model: ctx.model.as_bytes().to_vec(),
        ..State::default()
    })
}

fn replay_signature(signature: &[u8]) -> String {
    gemini_replay_signature_or_bypass(signature, BlockKind::GeminiModelPart)
}

fn signature_part(signature: &[u8]) -> Vec<u8> {
    let mut part = br#"{"thought":true,"thoughtSignature":""}"#.to_vec();
    gj::set_str(&mut part, "thoughtSignature", replay_signature(signature));
    part
}

/// The Gemini part for a text, thinking or signature delta.
fn delta_part(delta: &Res<'_>) -> Option<Vec<u8>> {
    let (template, path, value): (&[u8], &str, Vec<u8>) = match delta.get("type").bytes().as_ref() {
        b"text_delta" => (br#"{"text":""}"#, "text", string(&delta.get("text"))),
        b"thinking_delta" => (br#"{"thought":true,"text":""}"#, "text", string(&delta.get("thinking"))),
        b"signature_delta" => {
            let signature = string(&delta.get("signature"));
            return (!signature.is_empty()).then(|| signature_part(&signature));
        }
        _ => return None,
    };
    if value.is_empty() {
        return None;
    }
    let mut part = template.to_vec();
    gj::set_str(&mut part, path, &value);
    Some(part)
}

/// Gemini usage metadata (without the traffic type) from Claude usage.
fn usage_fields(target: &mut Vec<u8>, prefix: &str, usage: &Res<'_>) {
    let input = usage.get("input_tokens").int();
    let output = usage.get("output_tokens").int();
    let set = |target: &mut Vec<u8>, key: &str, value: i64| gj::set_int(target, &format!("{prefix}{key}"), value);
    set(target, "promptTokenCount", input);
    set(target, "candidatesTokenCount", output);
    set(target, "totalTokenCount", input.wrapping_add(output));
    let creation = usage.get("cache_creation_input_tokens");
    if creation.exists() {
        set(target, "cachedContentTokenCount", creation.int());
    }
    let read = usage.get("cache_read_input_tokens");
    if read.exists() {
        set(
            target,
            "cachedContentTokenCount",
            creation.int().wrapping_add(read.int()),
        );
    }
    let thinking = usage.get("thinking_tokens");
    if thinking.exists() {
        set(target, "thoughtsTokenCount", thinking.int());
    }
    gj::set_str(target, &format!("{prefix}trafficType"), "PROVISIONED_THROUGHPUT");
}

impl State {
    /// The function call completed by a content_block_stop, if any.
    fn finish_tool(&mut self, index: i64) -> Option<Vec<u8>> {
        let name = self.tool_names.get(&index).cloned().unwrap_or_default();
        let args = self
            .tool_args
            .get(&index)
            .map(|a| trim_space(a).to_vec())
            .unwrap_or_default();
        let id = self.tool_ids.get(&index).cloned().unwrap_or_default();
        if name.is_empty() && args.is_empty() {
            return None;
        }
        let mut call = br#"{"functionCall":{"name":"","args":{}}}"#.to_vec();
        if !name.is_empty() {
            gj::set_str(&mut call, "functionCall.name", &name);
        }
        if !args.is_empty() {
            gj::set_raw(&mut call, "functionCall.args", &args);
        }
        if !id.is_empty() {
            gj::set_str(&mut call, "functionCall.id", &id);
        }
        self.tool_args.remove(&index);
        self.tool_names.remove(&index);
        self.tool_ids.remove(&index);
        Some(call)
    }

    /// A tool_use block's name and ID (content_block_start).
    fn start_tool(&mut self, index: i64, block: &Res<'_>) {
        let name = block.get("name");
        if name.exists() {
            self.tool_names.insert(index, name.bytes().into_owned());
        }
        let id = string(&block.get("id"));
        if !id.is_empty() {
            self.tool_ids.insert(index, id);
        }
    }

    fn append_args(&mut self, index: i64, delta: &Res<'_>) {
        let args = self.tool_args.entry(index).or_default();
        let partial = delta.get("partial_json");
        if partial.exists() {
            args.extend_from_slice(&partial.bytes());
        }
    }
}

impl GoStream for State {
    /// ConvertClaudeResponseToGemini: one `data:` line at a time.
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let Some(payload) = line.strip_prefix(b"data:") else {
            return Ok(vec![]);
        };
        let root = gj::parse(trim_space(payload));
        let mut template = br#"{"candidates":[{"content":{"role":"model","parts":[]}}],"usageMetadata":{"trafficType":"PROVISIONED_THROUGHPUT"},"modelVersion":"","createTime":"","responseId":""}"#.to_vec();
        if !self.model.is_empty() {
            gj::set_str(&mut template, "modelVersion", &self.model);
        }
        if !self.response_id.is_empty() {
            gj::set_str(&mut template, "responseId", &self.response_id);
        }
        if self.created_at == 0 {
            self.created_at = now_unix();
        }
        // ponytail: Go formats createTime in the process's local zone; this assumes UTC.
        gj::set_str(&mut template, "createTime", format_rfc3339_utc(self.created_at));
        let index = root.get("index").int();
        let add_part = |template: &mut Vec<u8>, part: &[u8]| {
            gj::set_raw(template, "candidates.0.content.parts.-1", part);
        };
        Ok(match root.get("type").bytes().as_ref() {
            b"message_start" => {
                let message = root.get("message");
                if message.exists() {
                    self.response_id = string(&message.get("id"));
                    self.model = string(&message.get("model"));
                }
                vec![]
            }
            b"content_block_start" => {
                let block = root.get("content_block");
                match block.get("type").bytes().as_ref() {
                    b"tool_use" => {
                        self.start_tool(index, &block);
                        vec![]
                    }
                    b"thinking" => {
                        let signature = string(&block.get("signature"));
                        if signature.is_empty() {
                            vec![]
                        } else {
                            add_part(&mut template, &signature_part(&signature));
                            vec![template]
                        }
                    }
                    _ => vec![],
                }
            }
            b"content_block_delta" => {
                let delta = root.get("delta");
                if delta.get("type").bytes().as_ref() == b"input_json_delta" {
                    self.append_args(index, &delta);
                    return Ok(vec![]);
                }
                if let Some(part) = delta_part(&delta) {
                    add_part(&mut template, &part);
                }
                vec![template]
            }
            b"content_block_stop" => match self.finish_tool(index) {
                Some(call) => {
                    add_part(&mut template, &call);
                    gj::set_str(&mut template, "candidates.0.finishReason", "STOP");
                    vec![template]
                }
                None => vec![],
            },
            b"message_delta" => {
                let reason = root.get("delta.stop_reason");
                if reason.exists() {
                    let mapped = if reason.bytes().as_ref() == b"max_tokens" {
                        "MAX_TOKENS"
                    } else {
                        "STOP"
                    };
                    gj::set_str(&mut template, "candidates.0.finishReason", mapped);
                }
                let usage = root.get("usage");
                if usage.exists() {
                    usage_fields(&mut template, "usageMetadata.", &usage);
                }
                gj::set_str(&mut template, "candidates.0.finishReason", "STOP");
                vec![template]
            }
            b"error" => {
                let mut message = string(&root.get("error.message"));
                if message.is_empty() {
                    message = b"Unknown error occurred".to_vec();
                }
                let mut error = br#"{"error":{"code":400,"message":"","status":"INVALID_ARGUMENT"}}"#.to_vec();
                gj::set_str(&mut error, "error.message", &message);
                vec![error]
            }
            _ => vec![],
        })
    }
}

/// consolidateParts: adjacent text parts merge, as do adjacent thought parts (keeping
/// the last signature).
fn consolidate(parts: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut out = vec![];
    let (mut text, mut thought, mut signature) = (vec![], vec![], vec![]);
    let (mut has_text, mut has_thought) = (false, false);
    let flush_text = |out: &mut Vec<Vec<u8>>, text: &mut Vec<u8>, has_text: &mut bool| {
        if *has_text && !text.is_empty() {
            let mut part = br#"{"text":""}"#.to_vec();
            gj::set_str(&mut part, "text", &*text);
            out.push(part);
            text.clear();
            *has_text = false;
        }
    };
    let flush_thought =
        |out: &mut Vec<Vec<u8>>, thought: &mut Vec<u8>, signature: &mut Vec<u8>, has_thought: &mut bool| {
            if *has_thought && (!thought.is_empty() || !signature.is_empty()) {
                let mut part = br#"{"thought":true,"text":""}"#.to_vec();
                gj::set_str(&mut part, "text", &*thought);
                if !signature.is_empty() {
                    gj::set_str(&mut part, "thoughtSignature", &*signature);
                }
                out.push(part);
                thought.clear();
                signature.clear();
                *has_thought = false;
            }
        };
    for raw in parts {
        let part = gj::parse(&raw);
        let part_text = part.get("text");
        if !part.is_object() {
            flush_text(&mut out, &mut text, &mut has_text);
            flush_thought(&mut out, &mut thought, &mut signature, &mut has_thought);
            out.push(raw);
        } else if part.get("thought").kind == Kind::True {
            flush_text(&mut out, &mut text, &mut has_text);
            if part_text.kind == Kind::String {
                thought.extend_from_slice(&part_text.bytes());
                has_thought = true;
            }
            let sig = part.get("thoughtSignature");
            if sig.kind == Kind::String && !sig.bytes().is_empty() {
                signature = sig.bytes().into_owned();
                has_thought = true;
            }
        } else if part_text.kind == Kind::String {
            flush_thought(&mut out, &mut thought, &mut signature, &mut has_thought);
            text.extend_from_slice(&part_text.bytes());
            has_text = true;
        } else {
            flush_text(&mut out, &mut text, &mut has_text);
            flush_thought(&mut out, &mut thought, &mut signature, &mut has_thought);
            out.push(raw);
        }
    }
    flush_thought(&mut out, &mut thought, &mut signature, &mut has_thought);
    flush_text(&mut out, &mut text, &mut has_text);
    out
}

/// ConvertClaudeResponseToGeminiNonStream: a buffered Claude stream as one response.
fn non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let mut template = br#"{"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP"}],"usageMetadata":{"trafficType":"PROVISIONED_THROUGHPUT"},"modelVersion":"","createTime":"","responseId":""}"#.to_vec();
    gj::set_str(&mut template, "modelVersion", ctx.model);
    let mut st = State::default();
    let mut parts = vec![];
    let mut usage_json: Option<Vec<u8>> = None;
    let (mut response_id, mut created_at) = (vec![], 0);
    for line in body.split(|&c| c == b'\n') {
        let end = line.iter().rposition(|&c| c != b'\r').map_or(0, |i| i + 1);
        let line = &line[..end];
        let Some(payload) = line.strip_prefix(b"data:") else {
            continue;
        };
        let payload = trim_space(payload);
        if payload.is_empty() {
            continue;
        }
        let root = gj::parse(payload);
        let index = root.get("index").int();
        match root.get("type").bytes().as_ref() {
            b"message_start" => {
                let message = root.get("message");
                if message.exists() {
                    response_id = string(&message.get("id"));
                    created_at = now_unix();
                }
            }
            b"content_block_start" => {
                let block = root.get("content_block");
                match block.get("type").bytes().as_ref() {
                    b"tool_use" => st.start_tool(index, &block),
                    b"thinking" => {
                        let signature = string(&block.get("signature"));
                        if !signature.is_empty() {
                            parts.push(signature_part(&signature));
                        }
                    }
                    _ => {}
                }
            }
            b"content_block_delta" => {
                let delta = root.get("delta");
                if delta.get("type").bytes().as_ref() == b"input_json_delta" {
                    st.append_args(index, &delta);
                } else {
                    parts.extend(delta_part(&delta));
                }
            }
            b"content_block_stop" => parts.extend(st.finish_tool(index)),
            b"message_delta" => {
                let usage = root.get("usage");
                if usage.exists() {
                    let mut json = b"{}".to_vec();
                    usage_fields(&mut json, "", &usage);
                    usage_json = Some(json);
                }
            }
            _ => {}
        }
    }
    if !response_id.is_empty() {
        gj::set_str(&mut template, "responseId", &response_id);
    }
    if created_at > 0 {
        gj::set_str(&mut template, "createTime", format_rfc3339_utc(created_at));
    }
    let parts = consolidate(parts);
    if !parts.is_empty() {
        gj::set_raw(&mut template, "candidates.0.content.parts", gj::join(&parts));
    }
    if let Some(usage) = usage_json {
        gj::set_raw(&mut template, "usageMetadata", usage);
    }
    Ok(template)
}
