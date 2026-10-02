//! Claude Messages request -> OpenAI Chat Completions request
//! (internal/translator/openai/claude/openai_claude_request.go).

use crate::{
    Error, Registered, RequestCtx,
    common::{self, go_lower, trim_space},
    openai_claude_response as response,
};
use cpa_common::json::{self as gj, AnyValue, Kind, Res};
use std::collections::HashMap;

pub static PAIR: Registered = registered!(
    Claude -> OpenAI,
    request: request,
    non_stream: response::non_stream,
    go_stream: response::go_stream,
    token_count: Some(claude_input_tokens),
);

/// common.ClaudeInputTokensJSON (Go's ClaudeTokenCount).
pub(crate) fn claude_input_tokens(count: i64) -> Vec<u8> {
    format!(r#"{{"input_tokens":{count}}}"#).into_bytes()
}

fn request(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    Ok(convert(ctx.model, body, ctx.stream, false))
}

/// ConvertClaudeRequestToOpenAIWithCompat: configured compatibility endpoints keep
/// assistant thinking text even without a GPT-compatible signature.
pub fn request_with_compat(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    Ok(convert(ctx.model, body, ctx.stream, true))
}

const TOOL_RESULT_IMAGE_PLACEHOLDER: &[u8] =
    b"[Tool returned image content; the images follow in the next user message.]";
const TOOL_RESULT_IMAGE_RELAY_NOTICE: &[u8] = b"Images returned by the preceding tool call(s):";

fn convert(model: &str, raw: &[u8], stream: bool, preserve_thinking: bool) -> Vec<u8> {
    let mut out = br#"{"model":"","messages":[]}"#.to_vec();
    let root = gj::parse(raw);
    gj::set_str(&mut out, "model", model);

    let max_tokens = root.get("max_tokens");
    if max_tokens.exists() {
        gj::set_int(&mut out, "max_tokens", max_tokens.int());
    }
    let (temperature, top_p) = (root.get("temperature"), root.get("top_p"));
    if temperature.exists() {
        gj::set_f64(&mut out, "temperature", temperature.float());
    } else if top_p.exists() {
        gj::set_f64(&mut out, "top_p", top_p.float());
    }
    let stop = root.get("stop_sequences");
    if stop.is_array() {
        let mut stops = vec![];
        stop.each(|_, v| {
            stops.push(v.bytes().into_owned());
            true
        });
        if !stops.is_empty() {
            gj::set_strs(&mut out, "stop", &stops);
        }
    }
    gj::set_bool(&mut out, "stream", stream);
    reasoning_effort(&mut out, &root);

    let mut items: Vec<Vec<u8>> = vec![];
    let system = root.get("system");
    let mut system_items = vec![];
    if system.kind == Kind::String {
        if !system.s.is_empty() && !common::is_claude_code_attribution_text(&system.s) {
            let mut item = br#"{"type":"text","text":""}"#.to_vec();
            gj::set_str(&mut item, "text", &system.s);
            system_items.push(item);
        }
    } else if system.is_array() {
        system.each(|_, item| {
            system_items.extend(content_part(&item));
            true
        });
    }
    if !system_items.is_empty() {
        let mut message = br#"{"role":"system","content":[]}"#.to_vec();
        gj::set_raw(&mut message, "content", gj::join(&system_items));
        items.push(message);
    }

    let messages = root.get("messages");
    if messages.is_array() {
        let mut turns = Turns {
            preserve_thinking,
            ..Turns::default()
        };
        messages.each(|_, message| {
            turns.message(&message, &mut items);
            true
        });
        items.append(&mut turns.reminders);
    }
    if !items.is_empty() {
        let items = common::align_openai_tool_call_messages(items);
        gj::set_items(&mut out, "messages", &items);
    }

    let tools = root.get("tools");
    if tools.is_array() {
        let mut tool_items = vec![];
        tools.each(|_, tool| {
            let mut item = br#"{"type":"function","function":{"name":"","description":""}}"#.to_vec();
            gj::set_str(&mut item, "function.name", tool.get("name").bytes());
            gj::set_str(&mut item, "function.description", tool.get("description").bytes());
            let schema = tool.get("input_schema");
            if schema.exists() && schema.kind != Kind::Null {
                let mut value = AnyValue::from_res(&schema);
                normalize_schema(&mut value);
                value.set(&mut item, "function.parameters");
            } else {
                gj::set_raw(
                    &mut item,
                    "function.parameters",
                    br#"{"type":"object","properties":{}}"#,
                );
            }
            tool_items.push(item);
            true
        });
        if !tool_items.is_empty() {
            gj::set_raw(&mut out, "tools", gj::join(&tool_items));
        }
    }

    let choice = root.get("tool_choice");
    if choice.exists() && choice.kind != Kind::Null {
        let mut kind = choice.get("type").bytes().into_owned();
        if kind.is_empty() && choice.kind == Kind::String {
            kind = choice.s.to_vec();
        }
        match kind.as_slice() {
            b"auto" => gj::set_str(&mut out, "tool_choice", "auto"),
            b"any" => gj::set_str(&mut out, "tool_choice", "required"),
            b"tool" => {
                let name = choice.get("name").bytes();
                if name.is_empty() {
                    gj::set_str(&mut out, "tool_choice", "none")
                } else {
                    let mut tc = br#"{"type":"function","function":{"name":""}}"#.to_vec();
                    gj::set_str(&mut tc, "function.name", name);
                    gj::set_raw(&mut out, "tool_choice", tc)
                }
            }
            // "none" and anything unrecognized fail closed.
            _ => gj::set_str(&mut out, "tool_choice", "none"),
        };
        if choice.get("disable_parallel_tool_use").kind == Kind::True {
            gj::set_bool(&mut out, "parallel_tool_calls", false);
        }
    }

    let user = root.get("user");
    if user.exists() {
        gj::set_str(&mut out, "user", user.bytes());
    }
    out
}

/// Claude `thinking` (with `output_config.effort`) -> `reasoning_effort`.
// ponytail: thinking.ConvertBudgetToLevel and the level constants come from
// cpa_common::thinking; the branch logic stays here because it reads raw bytes.
fn reasoning_effort(out: &mut Vec<u8>, root: &Res<'_>) {
    use cpa_common::thinking::convert_budget_to_level;
    let config = root.get("thinking");
    if !config.is_object() {
        return;
    }
    let kind = config.get("type");
    if !kind.exists() {
        return;
    }
    let explicit = root.get("output_config.effort");
    let explicit = (explicit.kind == Kind::String).then(|| go_lower(trim_space(&explicit.s)));
    match kind.bytes().as_ref() {
        b"enabled" => {
            let budget = config.get("budget_tokens");
            let effort = if budget.exists() {
                convert_budget_to_level(budget.int()).map(|e| e.as_bytes().to_vec())
            } else if let Some(e) = explicit.filter(|e| !e.is_empty()) {
                Some(e)
            } else {
                convert_budget_to_level(-1).map(|e| e.as_bytes().to_vec())
            };
            if let Some(effort) = effort.filter(|e| !e.is_empty()) {
                gj::set_str(out, "reasoning_effort", effort);
            }
        }
        b"adaptive" | b"auto" => {
            let effort = explicit.filter(|e| !e.is_empty()).unwrap_or_else(|| b"xhigh".to_vec());
            gj::set_str(out, "reasoning_effort", effort);
        }
        b"disabled" => {
            if let Some(effort) = convert_budget_to_level(0) {
                gj::set_str(out, "reasoning_effort", effort);
            }
        }
        _ => {}
    }
}

/// State carried across Claude messages: tool uses awaiting results, system reminders
/// deferred until those results are placed, and tool names by ID.
#[derive(Default)]
struct Turns {
    preserve_thinking: bool,
    pending_tool_uses: Vec<Vec<u8>>,
    reminders: Vec<Vec<u8>>,
    tool_names: HashMap<Vec<u8>, Vec<u8>>,
}

impl Turns {
    fn message(&mut self, message: &Res<'_>, items: &mut Vec<Vec<u8>>) {
        let role = message.get("role").bytes().into_owned();
        let content = message.get("content");
        if role == b"system" {
            if let Some(text) = common::claude_message_system_reminder_text(&content) {
                let mut msg = br#"{"role":"user","content":[{"type":"text","text":""}]}"#.to_vec();
                gj::set_str(&mut msg, "content.0.text", text);
                if self.pending_tool_uses.is_empty() {
                    items.push(msg);
                } else {
                    self.reminders.push(msg);
                }
            }
            return;
        }
        if content.kind == Kind::String {
            let mut msg = br#"{"role":"","content":""}"#.to_vec();
            gj::set_str(&mut msg, "role", &role);
            gj::set_str(&mut msg, "content", &content.s);
            items.push(msg);
            return;
        }
        if !content.is_array() {
            return;
        }
        let mut parts = vec![];
        content.each(|_, part| {
            parts.push(part);
            true
        });
        if role == b"user" {
            parts = common::align_claude_tool_results(parts, &self.pending_tool_uses);
        }
        let preceding_calls_pending = !self.pending_tool_uses.is_empty();
        self.pending_tool_uses.clear();

        let assistant = role == b"assistant";
        let mut content_items: Vec<Vec<u8>> = vec![];
        let mut reasoning: Vec<Vec<u8>> = vec![];
        let mut tool_calls: Vec<AnyValue> = vec![];
        let mut tool_results: Vec<Vec<u8>> = vec![];
        let mut images: Vec<Vec<u8>> = vec![];
        for part in &parts {
            match part.get("type").bytes().as_ref() {
                b"thinking" if assistant => {
                    if !self.maps_thinking(part) {
                        continue;
                    }
                    let text = common::thinking_text(part);
                    if !trim_space(&text).is_empty() {
                        reasoning.push(text);
                    }
                }
                b"text" | b"image" => content_items.extend(content_part(part)),
                b"tool_use" if assistant => {
                    let id = part.get("id").bytes().into_owned();
                    let name = part.get("name").bytes().into_owned();
                    if !id.is_empty() {
                        if !name.is_empty() {
                            self.tool_names.insert(id.clone(), name.clone());
                        }
                        self.pending_tool_uses.push(id.clone());
                    }
                    let mut call = br#"{"id":"","type":"function","function":{"name":"","arguments":""}}"#.to_vec();
                    gj::set_str(&mut call, "id", &id);
                    gj::set_str(&mut call, "function.name", &name);
                    let input = part.get("input");
                    let arguments: &[u8] = if input.exists() { &input.raw } else { b"{}" };
                    gj::set_str(&mut call, "function.arguments", arguments);
                    tool_calls.push(AnyValue::from_res(&gj::parse(&call)));
                }
                b"tool_result" => {
                    let id = part.get("tool_use_id").bytes().into_owned();
                    let mut result = br#"{"role":"tool","tool_call_id":"","content":""}"#.to_vec();
                    gj::set_str(&mut result, "tool_call_id", &id);
                    if let Some(name) = self.tool_names.get(&id).filter(|n| !n.is_empty()) {
                        gj::set_str(&mut result, "name", name);
                    }
                    let (text, relayed) = tool_result_content(&part.get("content"));
                    gj::set_str(&mut result, "content", text);
                    images.extend(relayed);
                    tool_results.push(result);
                }
                _ => {}
            }
        }

        let has_content = !content_items.is_empty();
        let reasoning = reasoning.join(&b"\n\n"[..]);
        if preceding_calls_pending && tool_results.is_empty() {
            items.append(&mut self.reminders);
        }
        // Tool replies must directly follow the assistant message that called them.
        items.append(&mut tool_results);
        // OpenAI tool messages cannot carry images: replay them in a user message.
        if !images.is_empty() {
            let mut notice = br#"{"type":"text","text":""}"#.to_vec();
            gj::set_str(&mut notice, "text", TOOL_RESULT_IMAGE_RELAY_NOTICE);
            let mut relay = vec![notice];
            relay.append(&mut images);
            if role == b"user" && has_content {
                relay.append(&mut content_items);
                content_items = relay;
            } else {
                let mut msg = br#"{"role":"user"}"#.to_vec();
                gj::set_raw(&mut msg, "content", gj::join(&relay));
                items.push(msg);
            }
        }
        items.append(&mut self.reminders);

        if assistant {
            if !has_content && reasoning.is_empty() && tool_calls.is_empty() {
                return;
            }
            let mut msg = br#"{"role":"assistant"}"#.to_vec();
            if has_content {
                gj::set_raw(&mut msg, "content", gj::join(&content_items));
            } else {
                gj::set_str(&mut msg, "content", "");
            }
            if !reasoning.is_empty() {
                gj::set_str(&mut msg, "reasoning_content", &reasoning);
            }
            if !tool_calls.is_empty() {
                AnyValue::Array(tool_calls).set(&mut msg, "tool_calls");
            }
            items.push(msg);
        } else if has_content {
            let mut msg = br#"{"role":""}"#.to_vec();
            gj::set_str(&mut msg, "role", &role);
            gj::set_raw(&mut msg, "content", gj::join(&content_items));
            items.push(msg);
        }
    }

    /// shouldMapClaudeThinkingToGPTReasoning: compat endpoints keep all thinking; others
    /// only thinking whose signature is GPT-compatible.
    fn maps_thinking(&self, part: &Res<'_>) -> bool {
        if self.preserve_thinking {
            return true;
        }
        let signature = part.get("signature").bytes().into_owned();
        if trim_space(&signature).is_empty() {
            return false;
        }
        gpt_compatible_signature(&signature)
    }
}

/// signature.CompatibleSignatureForProvider(GPT, raw). Signatures are base64 text, so
/// bytes that are not UTF-8 are never compatible.
fn gpt_compatible_signature(raw: &[u8]) -> bool {
    use cpa_common::signature::{Provider, compatible_signature_for_provider};
    std::str::from_utf8(raw).is_ok_and(|raw| compatible_signature_for_provider(Provider::Gpt, raw).is_some())
}

/// convertClaudeContentPart: text (non-blank, not an attribution line) and images.
fn content_part(part: &Res<'_>) -> Option<Vec<u8>> {
    match part.get("type").bytes().as_ref() {
        b"text" => {
            let text = part.get("text").bytes();
            if trim_space(&text).is_empty() || common::is_claude_code_attribution_text(&text) {
                return None;
            }
            let mut item = br#"{"type":"text","text":""}"#.to_vec();
            gj::set_str(&mut item, "text", text);
            Some(item)
        }
        b"image" => {
            let mut url = vec![];
            let source = part.get("source");
            if source.exists() {
                match source.get("type").bytes().as_ref() {
                    b"base64" => {
                        let mut media = source.get("media_type").bytes().into_owned();
                        if media.is_empty() {
                            media = b"application/octet-stream".to_vec();
                        }
                        let data = source.get("data").bytes();
                        if !data.is_empty() {
                            url = [&b"data:"[..], &media, b";base64,", &data].concat();
                        }
                    }
                    b"url" => url = source.get("url").bytes().into_owned(),
                    _ => {}
                }
            }
            if url.is_empty() {
                url = part.get("url").bytes().into_owned();
            }
            if url.is_empty() {
                return None;
            }
            let mut item = br#"{"type":"image_url","image_url":{"url":""}}"#.to_vec();
            gj::set_str(&mut item, "image_url.url", url);
            Some(item)
        }
        _ => None,
    }
}

/// convertClaudeToolResultContent: the tool message text plus images to relay.
fn tool_result_content(content: &Res<'_>) -> (Vec<u8>, Vec<Vec<u8>>) {
    if !content.exists() {
        return (vec![], vec![]);
    }
    if content.kind == Kind::String {
        return (content.s.to_vec(), vec![]);
    }
    let is_type = |item: &Res<'_>, t: &[u8]| item.get("type").bytes().as_ref() == t;
    if content.is_array() {
        let mut parts: Vec<Vec<u8>> = vec![];
        let mut images = vec![];
        content.each(|_, item| {
            if item.kind == Kind::String {
                parts.push(item.s.to_vec());
            } else if item.is_object() && is_type(&item, b"text") {
                parts.push(item.get("text").bytes().into_owned());
            } else if item.is_object() && is_type(&item, b"image") {
                match content_part(&item) {
                    Some(image) => images.push(image),
                    None => parts.push(item.raw.to_vec()),
                }
            } else if item.is_object() && item.get("text").kind == Kind::String {
                parts.push(item.get("text").s.to_vec());
            } else {
                parts.push(item.raw.to_vec());
            }
            true
        });
        let joined = parts.join(&b"\n\n"[..]);
        if trim_space(&joined).is_empty() {
            if !images.is_empty() {
                return (TOOL_RESULT_IMAGE_PLACEHOLDER.to_vec(), images);
            }
            return (content.raw.to_vec(), vec![]);
        }
        return (joined, images);
    }
    if content.is_object() {
        if is_type(content, b"image")
            && let Some(image) = content_part(content)
        {
            return (TOOL_RESULT_IMAGE_PLACEHOLDER.to_vec(), vec![image]);
        }
        let text = content.get("text");
        if text.kind == Kind::String {
            return (text.s.to_vec(), vec![]);
        }
    }
    (content.raw.to_vec(), vec![])
}

/// normalizeObjectSchemaProperties: object schemas get `properties`, unsupported regex
/// escapes are dropped, and `true` subschemas become `{}` (except additionalProperties).
fn normalize_schema(schema: &mut AnyValue) {
    match schema {
        AnyValue::Bool(true) => *schema = AnyValue::Object(Default::default()),
        AnyValue::Array(items) => items.iter_mut().for_each(normalize_schema),
        AnyValue::Object(map) => {
            if matches!(map.get(&b"type"[..]), Some(AnyValue::String(t)) if t == b"object") {
                map.entry(b"properties".to_vec())
                    .or_insert_with(|| AnyValue::Object(Default::default()));
            }
            if matches!(map.get(&b"pattern"[..]), Some(AnyValue::String(p)) if common::has_unsupported_unicode_property_escape(p))
            {
                map.remove(&b"pattern"[..]);
            }
            for key in common::SCHEMA_MAP_KEYWORDS {
                if let Some(AnyValue::Object(sub)) = map.get_mut(key) {
                    if key == b"patternProperties" {
                        sub.retain(|k, _| !common::has_unsupported_unicode_property_escape(k));
                    }
                    sub.values_mut().for_each(normalize_schema);
                }
            }
            for key in common::SCHEMA_VALUE_KEYWORDS {
                match map.get_mut(key) {
                    Some(AnyValue::Bool(true)) if key != b"additionalProperties" => {
                        map.insert(key.to_vec(), AnyValue::Object(Default::default()));
                    }
                    Some(sub @ AnyValue::Object(_)) => normalize_schema(sub),
                    Some(AnyValue::Array(items)) => items.iter_mut().for_each(normalize_schema),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}
