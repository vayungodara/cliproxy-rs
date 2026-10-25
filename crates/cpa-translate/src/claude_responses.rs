//! OpenAI Responses request -> Claude Messages request, plus the tool naming and web
//! search mapping shared with the response side
//! (internal/translator/claude/openai/responses: *_request.go, *_tool_names.go,
//! *_web_search.go).

use crate::{
    claude_chat_request::{apply_effort, text_part},
    common::{self, trim_space},
    thinking,
};
use cpa_common::json::{self as gj, Kind, Res};
use cpa_core::gostr::GoStr;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};

pub(crate) const REDACTED_THINKING_PREFIX: &[u8] = b"claude-redacted-thinking:";
const WEB_SEARCH_ID_PREFIX: &[u8] = b"ws_";
const SERVER_TOOL_ID_PREFIX: &[u8] = b"srvtoolu_";

/// signature.CompatibleSignatureForProvider(Claude, raw). Signatures are base64 text, so
/// bytes that are not UTF-8 are never compatible.
fn compatible_claude_signature(raw: &[u8]) -> Option<Vec<u8>> {
    let raw = std::str::from_utf8(raw).ok()?;
    cpa_common::signature::compatible_signature_for_provider(cpa_common::signature::Provider::Claude, raw)
        .map(String::into_bytes)
}

/// Refuses a request whose user turn is left empty by file or audio parts Claude cannot
/// receive (f51624aa).
/// `tool_search` is `upstream.claude.tool-search` as pinned for the request: with it,
/// Codex client tool_search (`{"type":"tool_search","execution":"client"}`) becomes a
/// Claude tool and its history a tool_use/tool_result pair (claude_openai-responses_
/// tool_search.go); without it the translation predates tool_search support.
pub(crate) fn convert(
    model: &str,
    input: &[u8],
    stream: bool,
    preserve_thinking: bool,
    tool_search: bool,
) -> Result<Vec<u8>, crate::Error> {
    let raw = normalize_codex_agent_messages(input);
    let mut drops = common::UserTurnDrops::default();
    let user_id = common::derive_claude_user_id(&raw);
    let mut out = br#"{"model":"","max_tokens":32000,"messages":[],"metadata":{}}"#.to_vec();
    gj::set_str(&mut out, "metadata.user_id", &user_id);
    gj::set_int(&mut out, "max_tokens", default_max_tokens(model));
    let root = gj::parse(&raw);
    apply_effort(&mut out, &root.get("reasoning.effort"), model);
    gj::set_str(&mut out, "model", model);
    let max_output = root.get("max_output_tokens");
    if max_output.exists() && max_output.kind != Kind::Null {
        let mut val = max_output.int();
        let limit = max_completion_tokens(model);
        if limit > 0 && val > limit {
            val = limit;
        }
        gj::set_int(&mut out, "max_tokens", val);
    }
    gj::set_bool(&mut out, "stream", stream);
    let tier = root.get("service_tier");
    if tier.kind == Kind::String && &*tier.s == b"priority" {
        gj::set_str(&mut out, "speed", "fast");
    }

    let mut system: Vec<Vec<u8>> = vec![];
    let append_system = |system: &mut Vec<Vec<u8>>, text: &[u8], cache: Option<&Res<'_>>| {
        if text.is_empty() {
            return;
        }
        let mut block = text_part(text);
        if let Some(source) = cache.filter(|s| s.exists()) {
            block = common::attach_cache_control(block, source);
        }
        system.push(block);
    };
    let instructions = root.get("instructions");
    if instructions.kind == Kind::String {
        append_system(&mut system, &instructions.s, None);
    }
    let input_items = root.get("input");
    if input_items.is_array() {
        input_items.each(|_, item| {
            if !is_system_role(&item.get("role")) {
                return true;
            }
            let start = system.len();
            let content = item.get("content");
            if content.kind == Kind::String {
                append_system(&mut system, &content.s, None);
            } else if content.is_array() {
                content.each(|_, part| {
                    match &*part.get("type").bytes() {
                        b"input_text" | b"output_text" | b"text" => {
                            append_system(&mut system, &part.get("text").bytes(), Some(&part))
                        }
                        _ => {
                            let kind = trim_space(&part.get("type").bytes()).to_vec();
                            if !kind.is_empty() {
                                let mut block = br#"{"type":""}"#.to_vec();
                                gj::set_str(&mut block, "type", kind);
                                system.push(block);
                            }
                        }
                    }
                    true
                });
            }
            if item.get("cache_control").exists()
                && system.len() > start
                && let Some(last) = system.last_mut()
                && !gj::get(last, "cache_control").exists()
            {
                *last = common::attach_cache_control(std::mem::take(last), &item);
            }
            true
        });
    }
    let mut format = root.get("text.format");
    if !format.exists() {
        format = root.get("response_format");
    }
    let instruction = common::claude_structured_output_instruction(&format);
    append_system(&mut system, &instruction, None);

    let descriptors = tool_descriptors(&root, tool_search);
    let winners = tool_winners(&descriptors);
    let names = ToolNames::with_descriptors(&root, &descriptors, &winners);
    // Native tool search declares discovered tools as deferred and replays search
    // results as tool_reference blocks, which keeps the cached prefix.
    let native = client_search_wins(&descriptors);
    let mut pending = Pending::default();
    let mut items: Vec<Res<'_>> = vec![];
    if input_items.exists() {
        if input_items.is_array() {
            items = normalize_tool_call_outputs(input_items.array());
        } else if input_items.kind == Kind::String {
            pending.parts(b"user", vec![text_part(&input_items.s)]);
        }
    }
    let mut last_result: HashMap<Vec<u8>, Res<'_>> = HashMap::new();
    for item in &items {
        if matches!(
            &*item.get("type").bytes(),
            b"function_call_output" | b"custom_tool_call_output"
        ) {
            let id = extract_call_id(item);
            if !id.is_empty() {
                last_result.insert(id, item.clone());
            }
        }
    }
    let mut emitted_results: HashSet<Vec<u8>> = HashSet::new();
    let mut emitted_uses: HashSet<Vec<u8>> = HashSet::new();
    for item in &items {
        if is_system_role(&item.get("role")) {
            continue;
        }
        let mut kind = item.get("type").bytes().into_owned();
        if kind.is_empty() && !item.get("role").bytes().is_empty() {
            kind = b"message".to_vec();
        }
        match kind.as_slice() {
            b"message" => {
                let (role, parts, dropped) = message_parts(item);
                if role == b"user" {
                    if let Some(dropped) = dropped {
                        drops.drop_part(dropped);
                    }
                    drops.end_turn(parts.len());
                }
                if !parts.is_empty() {
                    pending.parts(&role, parts);
                }
            }
            b"web_search_call" => {
                let blocks = web_search_call_to_claude(item);
                if !blocks.is_empty() {
                    pending.parts(b"assistant", blocks);
                }
            }
            b"reasoning" => {
                if let Some(part) = reasoning_to_thinking(item, preserve_thinking) {
                    pending.reasoning(part);
                }
            }
            b"function_call" | b"custom_tool_call" => {
                let raw_id = extract_call_id(item);
                let mut id = raw_id.clone();
                if id.is_empty() {
                    id = common::generate_claude_tool_call_id();
                }
                let id = common::sanitize_claude_tool_id(&id);
                if !raw_id.is_empty() {
                    emitted_uses.insert(raw_id);
                }
                let mut name = item.get("name").bytes().into_owned();
                let namespace = trim_space(&item.get("namespace").bytes()).to_vec();
                if !namespace.is_empty() {
                    name = qualify_namespace_name(&namespace, &name);
                }
                let mut tool_use = br#"{"type":"tool_use","id":"","name":"","input":{}}"#.to_vec();
                gj::set_str(&mut tool_use, "id", &id);
                gj::set_str(&mut tool_use, "name", names.claude_name(&name));
                if kind == b"custom_tool_call" {
                    gj::set_str(&mut tool_use, "input.input", item.get("input").bytes());
                } else {
                    let args = item.get("arguments").bytes();
                    if !args.is_empty() && gj::valid(&args) {
                        let parsed = gj::parse(&args);
                        if parsed.is_object() {
                            gj::set_raw(&mut tool_use, "input", &parsed.raw);
                        }
                    }
                }
                pending.tool_use(tool_use);
            }
            b"function_call_output" | b"custom_tool_call_output" => {
                let raw_id = extract_call_id(item);
                if !raw_id.is_empty() && !emitted_results.insert(raw_id.clone()) {
                    continue;
                }
                let output_item = last_result.get(&raw_id).unwrap_or(item);
                let output = output_item.get("output");
                if raw_id.is_empty() || !emitted_uses.contains(&raw_id) {
                    pending.parts(b"user", standalone_tool_output(&output));
                    continue;
                }
                let mut result = br#"{"type":"tool_result","tool_use_id":"","content":""}"#.to_vec();
                gj::set_str(&mut result, "tool_use_id", common::sanitize_claude_tool_id(&raw_id));
                // Responses reports an unfinished tool output as status
                // "incomplete"; is_error is Claude's only in-band equivalent.
                if trim_space(&output_item.get("status").bytes()).eq_ignore_ascii_case(b"incomplete") {
                    gj::set_bool(&mut result, "is_error", true);
                }
                pending.parts(b"user", vec![tool_result_content(result, &output)]);
            }
            b"tool_search_call" if tool_search => {
                let Some((tool_use, raw_id)) = tool_search_call_to_claude(item, &names.claude_name(TOOL_SEARCH)) else {
                    continue;
                };
                emitted_uses.insert(raw_id);
                pending.tool_use(tool_use);
            }
            b"tool_search_output" if tool_search => {
                let raw_id = trim_space(&item.get("call_id").bytes()).to_vec();
                if !raw_id.is_empty() && !emitted_results.insert(raw_id.clone()) {
                    continue;
                }
                if raw_id.is_empty() || !emitted_uses.contains(&raw_id) {
                    let mut text = br#"{"type":"text","text":""}"#.to_vec();
                    gj::set_str(&mut text, "text", tool_search_output_text(item, &names));
                    pending.parts(b"user", vec![text]);
                    continue;
                }
                let mut result = br#"{"type":"tool_result","tool_use_id":"","content":""}"#.to_vec();
                gj::set_str(&mut result, "tool_use_id", common::sanitize_claude_tool_id(&raw_id));
                let content = tool_search_output_content(item, &names, &descriptors, &winners, native);
                gj::set_raw(&mut result, "content", content);
                pending.parts(b"user", vec![result]);
            }
            _ => {}
        }
    }
    let mut messages = pending.finish();
    let had_messages = !messages.is_empty();
    if !preserve_thinking {
        strip_trailing_thinking(&mut messages);
    }
    messages = repair_tool_pairing(messages);
    if !preserve_thinking
        && rejects_assistant_prefill(model)
        && messages.last().is_some_and(|m| is_role(m, "assistant"))
    {
        messages.pop();
    }
    if messages.is_empty() && (!system.is_empty() || had_messages) {
        messages.push(br#"{"role":"user","content":[{"type":"text","text":""}]}"#.to_vec());
    }
    gj::set_items(&mut out, "messages", &messages);
    if !system.is_empty() {
        gj::set_raw(&mut out, "system", gj::join(&system));
    }

    let mut included: HashSet<Vec<u8>> = HashSet::new();
    let mut tools = vec![];
    // The deferred anchor goes between declared and discovered tools, so each
    // discovery only appends to tools[].
    let mut anchor_index = None;
    for d in &descriptors {
        if winners.get(&d.name) != Some(&d.order) {
            continue;
        }
        let Some(mut tool) = descriptor_to_claude(d, &names.claude_name(&d.name)) else {
            continue;
        };
        if native && d.priority == DISCOVERED {
            anchor_index.get_or_insert(tools.len());
            // Anthropic rejects cache_control on deferred tools.
            gj::delete(&mut tool, "cache_control");
            gj::set_bool(&mut tool, "defer_loading", true);
        }
        let name = gj::get(&tool, "name").bytes().into_owned();
        if !name.is_empty() {
            included.insert(d.name.clone());
            included.insert(name);
        }
        tools.push(tool);
    }
    if native && !tools.iter().any(|t| gj::get(t, "name").str() == DEFERRED_ANCHOR_NAME) {
        let at = anchor_index.unwrap_or(tools.len());
        tools.insert(at, DEFERRED_ANCHOR.to_vec());
    }
    let name_map = tool_name_map(&descriptors, &winners, &included);
    if !tools.is_empty() {
        gj::set_raw(&mut out, "tools", gj::join(&tools));
    }
    let choice = root.get("tool_choice");
    match choice.kind {
        Kind::String => match &*choice.s {
            b"auto" => {
                gj::set_raw(&mut out, "tool_choice", r#"{"type":"auto"}"#);
            }
            b"required" if !included.is_empty() => {
                gj::set_raw(&mut out, "tool_choice", r#"{"type":"any"}"#);
            }
            _ => {}
        },
        Kind::Json if matches!(&*choice.get("type").bytes(), b"function" | b"custom") => {
            let first = |paths: &[&str]| {
                paths
                    .iter()
                    .map(|p| choice.get(p).bytes().into_owned())
                    .find(|v| !v.is_empty())
                    .unwrap_or_default()
            };
            let mut name = first(&["function.name", "custom.name", "name"]);
            let namespace = first(&["namespace", "function.namespace", "custom.namespace"]);
            if !namespace.is_empty() {
                name = qualify_namespace_name(&namespace, &name);
            }
            if let Some(mapped) = name_map.get(&name).filter(|m| !m.is_empty()) {
                name = mapped.clone();
            }
            if included.contains(&name) {
                let mut c = br#"{"name":"","type":"tool"}"#.to_vec();
                gj::set_str(&mut c, "name", names.claude_name(&name));
                gj::set_raw(&mut out, "tool_choice", c);
            }
        }
        _ => {}
    }
    let out = thinking::apply_translated_summary_to_claude(out, &raw, "openai-response", model);
    drops.result().map(|()| out)
}

fn model_info_int(model: &str, key: &str) -> i64 {
    thinking::lookup_model_info(model, "claude")
        .and_then(|m| m.raw.get(key).and_then(serde_json::Value::as_i64))
        .unwrap_or(0)
}

fn max_completion_tokens(model: &str) -> i64 {
    model_info_int(model, "max_completion_tokens")
}

/// The `max_tokens` an OpenAI Chat or Responses request without one gets: the
/// registered model's output limit, except that Fable keeps its 64000 ceiling.
pub(crate) fn default_max_tokens(model: &str) -> i64 {
    let fable = model.trim().go_lower().contains("fable");
    let base = if fable { 64000 } else { 32000 };
    match max_completion_tokens(model) {
        limit if limit <= 0 || (fable && limit >= base) => base,
        limit => limit,
    }
}

fn is_system_role(role: &Res<'_>) -> bool {
    matches!(common::norm(role).as_str(), "system" | "developer")
}

fn is_role(message: &[u8], role: &str) -> bool {
    common::norm(&gj::get(message, "role")) == role
}

/// claudeModelRejectsAssistantPrefill: Fable, Opus, Sonnet and Haiku 5+ (Haiku since
/// 911f49cb) and Sonnet 4.6+, after any
/// provider namespace and `claude-` prefix. Eight-digit snapshot dates are not versions.
fn rejects_assistant_prefill(model: &str) -> bool {
    let model = model.trim().go_lower();
    let model = model.rsplit('/').next().unwrap_or_default();
    let model = model.strip_prefix("claude-").unwrap_or(model).replace('.', "-");
    let tokens: Vec<&str> = model.split('-').collect();
    if tokens[0] == "fable" {
        return true;
    }
    if tokens.len() < 2 || !matches!(tokens[0], "opus" | "sonnet" | "haiku") {
        return false;
    }
    let version = |token: &str| -> Option<u32> {
        (!token.is_empty() && token.len() < 8 && token.bytes().all(|b| b.is_ascii_digit()))
            .then(|| token.parse().ok())
            .flatten()
    };
    match version(tokens[1]) {
        Some(major) if major >= 5 => true,
        Some(4) => tokens[0] == "sonnet" && tokens.get(2).and_then(|t| version(t)).is_some_and(|minor| minor >= 6),
        _ => false,
    }
}

/// Builds one Claude message, collapsing a lone plain text block to a string.
fn claude_message(role: &[u8], parts: &[Vec<u8>]) -> Vec<u8> {
    let mut msg = br#"{"role":"","content":[]}"#.to_vec();
    gj::set_str(&mut msg, "role", role);
    set_content(&mut msg, parts);
    msg
}

fn set_content(msg: &mut Vec<u8>, parts: &[Vec<u8>]) {
    if parts.len() == 1 {
        let part = gj::parse(&parts[0]);
        if &*part.get("type").bytes() == b"text"
            && !part.get("cache_control").exists()
            && !part.get("citations").exists()
        {
            let text = part.get("text").bytes().into_owned();
            gj::set_str(msg, "content", text);
            return;
        }
    }
    gj::set_raw(msg, "content", gj::join(parts));
}

/// The pending-turn buffer: same-role parts merge, assistant tool_use blocks trail.
#[derive(Default)]
struct Pending {
    messages: Vec<Vec<u8>>,
    role: Vec<u8>,
    parts: Vec<Vec<u8>>,
    tool_uses: Vec<Vec<u8>>,
}

impl Pending {
    fn flush(&mut self) {
        if self.role.is_empty() {
            return;
        }
        let mut parts = std::mem::take(&mut self.parts);
        let tool_uses = std::mem::take(&mut self.tool_uses);
        if self.role == b"assistant" && !tool_uses.is_empty() {
            if let Some(separator) = thinking_separator(&parts) {
                parts.push(separator);
            }
            parts.extend(tool_uses);
        }
        if !parts.is_empty() {
            let msg = claude_message(&self.role, &parts);
            self.messages.push(msg);
        }
        self.role.clear();
    }

    fn switch_to(&mut self, role: &[u8]) {
        if !self.role.is_empty() && self.role != role {
            self.flush();
        }
        self.role = role.to_vec();
    }

    fn parts(&mut self, role: &[u8], parts: Vec<Vec<u8>>) {
        if role.is_empty() || parts.is_empty() {
            return;
        }
        self.switch_to(role);
        self.parts.extend(parts);
    }

    fn tool_use(&mut self, part: Vec<u8>) {
        self.switch_to(b"assistant");
        self.tool_uses.push(part);
    }

    fn reasoning(&mut self, part: Vec<u8>) {
        self.switch_to(b"assistant");
        self.parts.append(&mut self.tool_uses);
        if gj::get(&part, "type").str() == "thinking"
            && let Some(last) = self.parts.last_mut()
            && gj::get(last, "type").str() == "thinking"
        {
            *last = part;
            return;
        }
        self.parts.push(part);
    }

    fn finish(mut self) -> Vec<Vec<u8>> {
        self.flush();
        self.messages
    }
}

/// A web search result ending the parts keeps the latest thinking block before tool use.
fn thinking_separator(parts: &[Vec<u8>]) -> Option<Vec<u8>> {
    if gj::get(parts.last()?, "type").str() != "web_search_tool_result" {
        return None;
    }
    parts
        .iter()
        .rev()
        .find(|p| gj::get(p, "type").str() == "thinking")
        .cloned()
}

fn data_url(url: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let trimmed = url.strip_prefix(b"data:").unwrap_or(url);
    match trimmed.windows(8).position(|w| w == b";base64,") {
        Some(i) => {
            let media = if i == 0 {
                b"application/octet-stream".to_vec()
            } else {
                trimmed[..i].to_vec()
            };
            (media, trimmed[i + 8..].to_vec())
        }
        None => (b"application/octet-stream".to_vec(), vec![]),
    }
}

fn image_part(part: &Res<'_>) -> Option<Vec<u8>> {
    let mut url = part.get("image_url").bytes().into_owned();
    if url.is_empty() {
        url = part.get("url").bytes().into_owned();
    }
    if url.is_empty() {
        return None;
    }
    if url.starts_with(b"data:") {
        let (media, data) = data_url(&url);
        if data.is_empty() {
            return None;
        }
        let mut out = br#"{"type":"image","source":{"type":"base64","media_type":"","data":""}}"#.to_vec();
        gj::set_str(&mut out, "source.media_type", media);
        gj::set_str(&mut out, "source.data", data);
        return Some(out);
    }
    let mut out = br#"{"type":"image","source":{"type":"url","url":""}}"#.to_vec();
    gj::set_str(&mut out, "source.url", url);
    Some(out)
}

fn file_part(part: &Res<'_>) -> Option<Vec<u8>> {
    let file = part.get("file_data").bytes().into_owned();
    if file.is_empty() {
        return None;
    }
    let (mut media, mut data) = (b"application/octet-stream".to_vec(), file.clone());
    if file.starts_with(b"data:") {
        let trimmed = &file[5..];
        if let Some(i) = trimmed.windows(8).position(|w| w == b";base64,") {
            if i > 0 {
                media = trimmed[..i].to_vec();
            }
            data = trimmed[i + 8..].to_vec();
        }
    }
    let mut out = br#"{"type":"document","source":{"type":"base64","media_type":"","data":""}}"#.to_vec();
    gj::set_str(&mut out, "source.media_type", media);
    gj::set_str(&mut out, "source.data", data);
    Some(out)
}

/// One `message` item: its role, Claude content parts and the type of the first part
/// Claude cannot receive (a file without data, audio).
fn message_parts(item: &Res<'_>) -> (Vec<u8>, Vec<Vec<u8>>, Option<&'static str>) {
    let mut role: Vec<u8> = vec![];
    let mut parts = vec![];
    let mut dropped = None;
    let content = item.get("content");
    if content.is_array() {
        content.each(|_, part| {
            let kind = part.get("type").bytes().into_owned();
            match kind.as_slice() {
                b"input_text" | b"output_text" => {
                    let text = part.get("text");
                    if text.exists() {
                        let p = attach_citations(text_part(&text.bytes()), &part.get("annotations"));
                        parts.push(common::attach_cache_control(p, &part));
                    }
                    role = if kind == b"input_text" {
                        b"user".to_vec()
                    } else {
                        b"assistant".to_vec()
                    };
                }
                b"refusal" => {
                    let refusal = part.get("refusal").bytes();
                    if !refusal.is_empty() {
                        parts.push(common::attach_cache_control(text_part(&refusal), &part));
                    }
                    role = b"assistant".to_vec();
                }
                b"input_image" | b"input_file" => {
                    let built = if kind == b"input_image" {
                        image_part(&part)
                    } else {
                        file_part(&part)
                    };
                    if let Some(p) = built {
                        parts.push(common::attach_cache_control(p, &part));
                        if role.is_empty() {
                            role = b"user".to_vec();
                        }
                    } else if kind == b"input_file" {
                        dropped.get_or_insert("input_file");
                    }
                }
                // Claude has no audio block.
                b"input_audio" => {
                    dropped.get_or_insert("input_audio");
                }
                _ => {}
            }
            true
        });
    } else if content.kind == Kind::String && !content.s.is_empty() {
        parts.push(text_part(&content.s));
    }
    if role.is_empty() {
        role = match &*item.get("role").bytes() {
            b"assistant" => b"assistant".to_vec(),
            _ => b"user".to_vec(),
        };
    }
    if let Some(last) = parts.last_mut()
        && !gj::get(last, "cache_control").exists()
    {
        *last = common::attach_cache_control(std::mem::take(last), item);
    }
    (role, parts, dropped)
}

fn reasoning_to_thinking(item: &Res<'_>, preserve: bool) -> Option<Vec<u8>> {
    let encrypted = item.get("encrypted_content").bytes().into_owned();
    let trimmed = trim_space(&encrypted);
    if let Some(rest) = trimmed.strip_prefix(REDACTED_THINKING_PREFIX) {
        let data = trim_space(rest);
        if data.is_empty() {
            return None;
        }
        let mut part = br#"{"type":"redacted_thinking","data":""}"#.to_vec();
        gj::set_str(&mut part, "data", data);
        return Some(part);
    }
    let signature = match compatible_claude_signature(&encrypted) {
        Some(s) => s,
        None if preserve => encrypted,
        None => return None,
    };
    let mut text = reasoning_parts_text(&item.get("summary"));
    if text.is_empty() {
        text = reasoning_parts_text(&item.get("content"));
    }
    let mut part = br#"{"type":"thinking","thinking":"","signature":""}"#.to_vec();
    gj::set_str(&mut part, "thinking", text);
    gj::set_str(&mut part, "signature", signature);
    Some(part)
}

fn reasoning_parts_text(parts: &Res<'_>) -> Vec<u8> {
    let mut out = vec![];
    if parts.is_array() {
        parts.each(|_, part| {
            let text = part.get("text");
            if text.exists() {
                out.extend_from_slice(&text.bytes());
            } else if part.kind == Kind::String {
                out.extend_from_slice(&part.s);
            }
            true
        });
    }
    out
}

fn content_part(part: &Res<'_>) -> Option<Vec<u8>> {
    match &*part.get("type").bytes() {
        b"input_text" | b"output_text" => {
            let text = part.get("text");
            text.exists().then(|| text_part(&text.bytes()))
        }
        b"input_image" => image_part(part),
        b"input_file" => file_part(part),
        _ => None,
    }
}

fn visible(part: &[u8]) -> bool {
    gj::get(part, "type").str() != "text" || !trim_space(&gj::get(part, "text").bytes()).is_empty()
}

const EMPTY_RESULT: &[u8] = br#"{"type":"text","text":"Tool result was empty."}"#;

fn standalone_tool_output(output: &Res<'_>) -> Vec<Vec<u8>> {
    if output.is_array() {
        let mut parts = vec![];
        output.each(|_, part| {
            if let Some(p) = content_part(&part).filter(|p| visible(p)) {
                parts.push(p);
            }
            true
        });
        if !parts.is_empty() {
            return parts;
        }
    }
    let text = output.bytes();
    if output.is_array() || trim_space(&text).is_empty() {
        return vec![EMPTY_RESULT.to_vec()];
    }
    vec![text_part(&text)]
}

fn tool_result_content(mut result: Vec<u8>, output: &Res<'_>) -> Vec<u8> {
    if !output.is_array() {
        gj::set_str(&mut result, "content", output.bytes());
        return result;
    }
    let mut parts: Vec<Vec<u8>> = vec![];
    let mut media = false;
    output.each(|_, part| {
        if let Some(p) = content_part(&part) {
            media |= matches!(gj::get(&p, "type").str().as_ref(), "image" | "document");
            parts.push(p);
        }
        true
    });
    if parts.is_empty() {
        gj::set_str(&mut result, "content", &output.raw);
        return result;
    }
    if parts.len() == 1 && !media && gj::get(&parts[0], "type").str() == "text" {
        let text = gj::get(&parts[0], "text").bytes().into_owned();
        gj::set_str(&mut result, "content", text);
        return result;
    }
    gj::delete(&mut result, "content");
    gj::set_raw(&mut result, "content", gj::join(&parts));
    result
}

fn strip_trailing_thinking(messages: &mut Vec<Vec<u8>>) {
    let Some(last) = messages.last() else {
        return;
    };
    if !is_role(last, "assistant") {
        return;
    }
    let content = gj::get(last, "content");
    if !content.is_array() {
        return;
    }
    let parts = content.array();
    let mut end = parts.len();
    while end > 0
        && matches!(
            trim_space(&parts[end - 1].get("type").bytes()),
            b"thinking" | b"redacted_thinking"
        )
    {
        end -= 1;
    }
    if end == parts.len() {
        return;
    }
    if end == 0 {
        messages.pop();
        return;
    }
    let kept: Vec<Vec<u8>> = parts[..end].iter().map(|p| p.raw.to_vec()).collect();
    let mut msg = last.clone();
    set_content(&mut msg, &kept);
    *messages.last_mut().unwrap() = msg;
}

fn block_ids(content: &Res<'_>, kind: &str, key: &str) -> Vec<Vec<u8>> {
    let mut ids = vec![];
    content.each(|_, block| {
        if block.get("type").str() == kind {
            ids.push(block.get(key).bytes().into_owned());
        }
        true
    });
    ids
}

/// repairClaudeToolPairing: every tool_use gets a tool_result in the next user message
/// (synthesized as interrupted when missing), and tool_results without a matching
/// tool_use become text.
fn repair_tool_pairing(mut messages: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut previous: HashSet<Vec<u8>> = HashSet::new();
    let mut out = Vec::with_capacity(messages.len() + 1);
    for i in 0..messages.len() {
        let mut msg = messages[i].clone();
        let role = gj::get(&msg, "role").bytes().into_owned();
        if role == b"user"
            && let Some(rebuilt) = normalize_tool_result_message(&msg, &previous)
        {
            msg = rebuilt;
            messages[i] = msg.clone();
        }
        out.push(msg.clone());
        previous.clear();
        if role != b"assistant" {
            continue;
        }
        let ids: Vec<Vec<u8>> = block_ids(&gj::get(&msg, "content"), "tool_use", "id")
            .into_iter()
            .filter(|id| !id.is_empty())
            .collect();
        previous.extend(ids.iter().cloned());
        if ids.is_empty() {
            continue;
        }
        let next_user = i + 1 < messages.len() && &*gj::get(&messages[i + 1], "role").bytes() == b"user";
        let answered: HashSet<Vec<u8>> = if next_user {
            block_ids(&gj::get(&messages[i + 1], "content"), "tool_result", "tool_use_id")
                .into_iter()
                .collect()
        } else {
            HashSet::new()
        };
        let mut synthesized: Vec<Vec<u8>> = vec![];
        for id in &ids {
            if answered.contains(id) {
                continue;
            }
            let mut part = br#"{"type":"tool_result","tool_use_id":"","is_error":true,"content":"Tool call was interrupted before any output was recorded."}"#.to_vec();
            gj::set_str(&mut part, "tool_use_id", id);
            synthesized.push(part);
        }
        if synthesized.is_empty() {
            continue;
        }
        let mut user = br#"{"role":"user","content":[]}"#.to_vec();
        if next_user {
            let next = gj::get(&messages[i + 1], "content");
            if next.is_array() {
                next.each(|_, block| {
                    synthesized.push(block.raw.to_vec());
                    true
                });
            } else if next.kind == Kind::String {
                synthesized.push(text_part(&next.s));
            }
            gj::set_raw(&mut user, "content", gj::join(&synthesized));
            messages[i + 1] = user;
        } else {
            gj::set_raw(&mut user, "content", gj::join(&synthesized));
            out.push(user);
        }
    }
    out
}

fn normalize_tool_result_message(msg: &[u8], answered: &HashSet<Vec<u8>>) -> Option<Vec<u8>> {
    let content = gj::get(msg, "content");
    if !content.is_array() {
        return None;
    }
    let (mut results, mut others) = (vec![], vec![]);
    let (mut seen_other, mut changed) = (false, false);
    content.each(|_, block| {
        if block.get("type").str() == "tool_result" {
            if !answered.contains(&*block.get("tool_use_id").bytes()) {
                changed = true;
                seen_other = true;
                let text = tool_result_text_parts(&block);
                if text.is_empty() {
                    others.push(EMPTY_RESULT.to_vec());
                } else {
                    others.extend(text);
                }
                return true;
            }
            results.push(block.raw.to_vec());
            changed |= seen_other;
            return true;
        }
        seen_other = true;
        others.push(block.raw.to_vec());
        true
    });
    if !changed {
        return None;
    }
    results.extend(others);
    let mut user = br#"{"role":"user","content":[]}"#.to_vec();
    gj::set_raw(&mut user, "content", gj::join(&results));
    Some(user)
}

fn tool_result_text_parts(block: &Res<'_>) -> Vec<Vec<u8>> {
    let content = block.get("content");
    if content.is_array() {
        let mut parts = vec![];
        content.each(|_, part| {
            let mut raw = part.raw.to_vec();
            if part.get("type").bytes().is_empty() {
                gj::set_str(&mut raw, "type", "text");
            }
            if visible(&raw) {
                parts.push(raw);
            }
            true
        });
        return parts;
    }
    let text = content.bytes();
    if trim_space(&text).is_empty() {
        return vec![];
    }
    vec![text_part(&text)]
}

// ---------------------------------------------------------------------------------------
// Responses input helpers (common/responses.go)

/// common.ExtractResponsesCallID.
pub(crate) fn extract_call_id(node: &Res<'_>) -> Vec<u8> {
    for key in ["call_id", "tool_call_id", "callId"] {
        let id = trim_space(&node.get(key).bytes()).to_vec();
        if !id.is_empty() {
            return id;
        }
    }
    let id = trim_space(&node.get("id").bytes()).to_vec();
    if id.starts_with(b"fco_") { vec![] } else { id }
}

fn is_output_type(item: &Res<'_>) -> bool {
    matches!(
        &*item.get("type").bytes(),
        b"function_call_output" | b"custom_tool_call_output"
    )
}

/// common.NormalizeResponsesToolCallOutputs: assigns call IDs to outputs that lack one,
/// matching pending calls by explicit ID, then by name, then in order.
pub(crate) fn normalize_tool_call_outputs<'a>(items: Vec<Res<'a>>) -> Vec<Res<'a>> {
    let mut normalized = items.clone();
    let mut explicit: HashMap<Vec<u8>, i64> = HashMap::new();
    for item in &items {
        if is_output_type(item) {
            let id = extract_call_id(item);
            if !id.is_empty() {
                *explicit.entry(id).or_default() += 1;
            }
        }
    }
    let mut pending: Vec<Vec<u8>> = vec![];
    let mut pending_names: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut i = 0;
    while i < normalized.len() {
        let item = normalized[i].clone();
        match &*item.get("type").bytes() {
            b"function_call" | b"custom_tool_call" => {
                let id = extract_call_id(&item);
                if !id.is_empty() {
                    pending_names.insert(id.clone(), item.get("name").bytes().into_owned());
                    pending.push(id);
                }
                i += 1;
            }
            b"function_call_output" | b"custom_tool_call_output" => {
                let start = i;
                while i < normalized.len() && is_output_type(&normalized[i]) {
                    i += 1;
                }
                if pending.is_empty() {
                    continue;
                }
                let outputs: Vec<Res<'a>> = normalized[start..i].to_vec();
                let mut used = vec![false; outputs.len()];
                let mut matched: Vec<Option<usize>> = vec![None; pending.len()];
                for (p, id) in pending.iter().enumerate() {
                    if let Some(o) = (0..outputs.len()).find(|&o| !used[o] && extract_call_id(&outputs[o]) == *id) {
                        used[o] = true;
                        matched[p] = Some(o);
                        *explicit.entry(id.clone()).or_default() -= 1;
                    }
                }
                let unmatched_name = |o: &Res<'a>| trim_space(&o.get("name").bytes()).to_vec();
                for (p, id) in pending.iter().enumerate() {
                    if matched[p].is_some() || explicit.get(id).copied().unwrap_or(0) > 0 {
                        continue;
                    }
                    let expected = pending_names.get(id).cloned().unwrap_or_default();
                    if expected.is_empty() {
                        continue;
                    }
                    if let Some(o) = (0..outputs.len()).find(|&o| {
                        !used[o] && extract_call_id(&outputs[o]).is_empty() && {
                            let name = unmatched_name(&outputs[o]);
                            !name.is_empty() && name == expected
                        }
                    }) {
                        used[o] = true;
                        matched[p] = Some(o);
                    }
                }
                for (p, id) in pending.iter().enumerate() {
                    if matched[p].is_some() || explicit.get(id).copied().unwrap_or(0) > 0 {
                        continue;
                    }
                    let expected = pending_names.get(id).cloned().unwrap_or_default();
                    if let Some(o) = (0..outputs.len()).find(|&o| {
                        !used[o] && extract_call_id(&outputs[o]).is_empty() && {
                            let name = unmatched_name(&outputs[o]);
                            name.is_empty() || expected.is_empty() || name == expected
                        }
                    }) {
                        used[o] = true;
                        matched[p] = Some(o);
                    }
                }
                let mut remaining = vec![];
                for (p, id) in pending.iter().enumerate() {
                    let Some(o) = matched[p] else {
                        remaining.push(id.clone());
                        continue;
                    };
                    if *outputs[o].get("call_id").bytes() != **id {
                        let mut raw = outputs[o].raw.to_vec();
                        gj::set_str(&mut raw, "call_id", id);
                        normalized[start + o] = gj::parse(&raw).into_owned();
                    }
                }
                pending = remaining;
            }
            _ => i += 1,
        }
    }
    normalized
}

/// normalizeCodexAgentMessages: Codex `agent_message` items become user messages, with
/// encrypted parts turned into input text.
fn normalize_codex_agent_messages(payload: &[u8]) -> Vec<u8> {
    let input = gj::get(payload, "input");
    if !input.is_array() {
        return payload.to_vec();
    }
    let mut updated = payload.to_vec();
    let mut changed = false;
    for (index, item) in input.array().iter().enumerate() {
        if trim_space(&item.get("type").bytes()) != b"agent_message" {
            continue;
        }
        let item_path = format!("input.{index}");
        let content = item.get("content");
        if content.is_array() {
            for (part_index, part) in content.array().iter().enumerate() {
                if trim_space(&part.get("type").bytes()) != b"encrypted_content" {
                    continue;
                }
                let enc = part.get("encrypted_content");
                if enc.kind != Kind::String {
                    continue;
                }
                let path = format!("{item_path}.content.{part_index}");
                if !gj::set_str(&mut updated, &format!("{path}.type"), "input_text")
                    || !gj::set_str(&mut updated, &format!("{path}.text"), &enc.s)
                    || !gj::delete(&mut updated, &format!("{path}.encrypted_content"))
                {
                    return payload.to_vec();
                }
            }
        }
        if !gj::set_str(&mut updated, &format!("{item_path}.role"), "user")
            || !gj::set_str(&mut updated, &format!("{item_path}.type"), "message")
        {
            return payload.to_vec();
        }
        changed = true;
    }
    if changed { updated } else { payload.to_vec() }
}

// ---------------------------------------------------------------------------------------
// Tool declarations and Claude-safe names (*_request.go, *_tool_names.go)

/// util.QualifyResponsesNamespaceToolName.
pub(crate) fn qualify_namespace_name(namespace: &[u8], child: &[u8]) -> Vec<u8> {
    let child = trim_space(child);
    let namespace = trim_space(namespace);
    if child.is_empty() || namespace.is_empty() || child.starts_with(b"mcp__") {
        return child.to_vec();
    }
    let mut prefixed = namespace.to_vec();
    prefixed.extend_from_slice(b"__");
    if child == namespace || child.starts_with(&prefixed) {
        return child.to_vec();
    }
    if namespace.ends_with(b"__") {
        return [namespace, child].concat();
    }
    [&prefixed[..], child].concat()
}

fn unsupported_builtin(kind: &[u8]) -> bool {
    matches!(
        kind,
        b"image_generation" | b"file_search" | b"code_interpreter" | b"computer_use_preview"
    )
}

pub(crate) fn tool_name(tool: &Res<'_>) -> Vec<u8> {
    let name = trim_space(&tool.get("name").bytes()).to_vec();
    if !name.is_empty() {
        return name;
    }
    trim_space(&tool.get("function.name").bytes()).to_vec()
}

#[derive(Clone)]
pub(crate) struct Descriptor<'a> {
    pub name: Vec<u8>,
    pub child_name: Vec<u8>,
    pub namespace: Vec<u8>,
    pub kind: Vec<u8>,
    pub tool: Res<'a>,
    /// 0 top-level tools, 1 additional_tools, [`DISCOVERED`] tool_search_output results.
    pub priority: u8,
    pub direct: bool,
    pub order: usize,
    /// The Codex client tool_search declaration, which exists only with tool search on.
    pub client_search: bool,
}

/// Tools a tool_search_output loaded rank after every declared tool, so they append to
/// tools[] in discovery order.
const DISCOVERED: u8 = 2;

/// responsesToolDescriptors: top-level tools, then additional_tools items, then (with
/// tool search) the tools tool_search_output items loaded, with namespace children
/// qualified. While the client tool_search declaration owns its name, declared
/// `defer_loading` tools stay hidden until a tool_search_output loads them.
pub(crate) fn tool_descriptors<'a>(root: &Res<'a>, tool_search: bool) -> Vec<Descriptor<'a>> {
    let mut sources: Vec<(Res<'a>, u8)> = vec![];
    let tools = root.get("tools");
    if tools.is_array() {
        sources.push((tools, 0));
    }
    let input = root.get("input");
    if input.is_array() {
        input.each(|_, item| {
            if item.get("type").str() == "additional_tools" {
                let tools = item.get("tools");
                if tools.is_array() {
                    sources.push((tools, 1));
                }
            }
            true
        });
        if tool_search {
            input.each(|_, item| {
                if item.get("type").str() == "tool_search_output" {
                    let tools = item.get("tools");
                    if tools.is_array() {
                        sources.push((tools, DISCOVERED));
                    }
                }
                true
            });
        }
    }
    let mut out: Vec<Descriptor<'a>> = vec![];
    for (tools, priority) in sources {
        append_descriptors(&mut out, &tools, priority, tool_search);
    }
    if !client_search_wins(&out) {
        return out;
    }
    let mut kept: Vec<Descriptor<'a>> = Vec::with_capacity(out.len());
    for mut d in out {
        if d.priority != DISCOVERED && d.tool.get("defer_loading").bool() {
            continue;
        }
        d.order = kept.len();
        kept.push(d);
    }
    kept
}

/// responsesClientToolSearchWins: the client tool_search declaration owns the
/// `tool_search` name. A same-named function that wins instead leaves no search tool,
/// so deferred tools must stay declared.
fn client_search_wins(descriptors: &[Descriptor<'_>]) -> bool {
    let mut winner: Option<&Descriptor<'_>> = None;
    for d in descriptors.iter().filter(|d| d.name == TOOL_SEARCH) {
        if winner.is_none_or(|w| precedes(d, w)) {
            winner = Some(d);
        }
    }
    winner.is_some_and(|w| w.client_search)
}

/// appendResponsesToolDescriptors: the descriptors of one tools array.
fn append_descriptors<'a>(out: &mut Vec<Descriptor<'a>>, tools: &Res<'a>, priority: u8, tool_search: bool) {
    let add = |out: &mut Vec<Descriptor<'a>>,
               tool: Res<'a>,
               name: Vec<u8>,
               child: Vec<u8>,
               ns: Vec<u8>,
               kind: &[u8],
               priority: u8,
               direct: bool| {
        if name.is_empty() {
            return;
        }
        let order = out.len();
        out.push(Descriptor {
            name,
            child_name: child,
            namespace: ns,
            kind: kind.to_vec(),
            tool,
            priority,
            direct,
            order,
            client_search: false,
        });
    };
    {
        tools.each(|_, tool| {
            let kind = trim_space(&tool.get("type").bytes()).to_vec();
            match kind.as_slice() {
                b"" | b"function" => add(
                    out,
                    tool.clone(),
                    tool_name(&tool),
                    vec![],
                    vec![],
                    b"function",
                    priority,
                    true,
                ),
                b"custom" => add(
                    out,
                    tool.clone(),
                    tool_name(&tool),
                    vec![],
                    vec![],
                    b"custom",
                    priority,
                    true,
                ),
                b"namespace" => {
                    let namespace = trim_space(&tool.get("name").bytes()).to_vec();
                    let children = tool.get("tools");
                    if children.is_array() {
                        children.each(|_, child| {
                            let child_name = tool_name(&child);
                            if child_name.is_empty() {
                                return true;
                            }
                            let qualified = qualify_namespace_name(&namespace, &child_name);
                            let ckind = trim_space(&child.get("type").bytes()).to_vec();
                            let ckind: &[u8] = match ckind.as_slice() {
                                b"" | b"function" => b"function",
                                b"custom" => b"custom",
                                _ => return true,
                            };
                            add(
                                out,
                                child.clone(),
                                qualified,
                                child_name,
                                namespace.clone(),
                                ckind,
                                priority,
                                false,
                            );
                            true
                        });
                    }
                }
                b"tool_search" if tool_search => {
                    // Hosted search cannot run on Claude, and the raw declaration must
                    // never reach Claude, so only client search is kept.
                    if is_client_tool_search(&tool) {
                        add(
                            out,
                            tool.clone(),
                            TOOL_SEARCH.to_vec(),
                            vec![],
                            vec![],
                            b"tool_search",
                            priority,
                            true,
                        );
                        if let Some(last) = out.last_mut() {
                            last.client_search = true;
                        }
                    }
                }
                b"web_search" => {
                    let access = tool.get("external_web_access");
                    if access.exists() && !access.bool() {
                        return true;
                    }
                    let mut name = trim_space(&tool.get("name").bytes()).to_vec();
                    if name.is_empty() {
                        name = b"web_search".to_vec();
                    }
                    add(out, tool.clone(), name, vec![], vec![], b"web_search", priority, true);
                }
                _ => {
                    if !unsupported_builtin(&kind) {
                        let name = trim_space(&tool.get("name").bytes()).to_vec();
                        add(out, tool.clone(), name, vec![], vec![], &kind, priority, true);
                    }
                }
            }
            true
        });
    }
}

/// responsesToolDescriptorPrecedes: top-level beats additional_tools beats discovered,
/// direct beats namespaced, then first wins.
fn precedes(d: &Descriptor<'_>, c: &Descriptor<'_>) -> bool {
    if d.priority != c.priority {
        d.priority < c.priority
    } else if d.direct != c.direct {
        d.direct
    } else {
        d.order < c.order
    }
}

/// responsesToolWinners: the descriptor each name resolves to ([`precedes`]).
pub(crate) fn tool_winners(descriptors: &[Descriptor<'_>]) -> HashMap<Vec<u8>, usize> {
    let mut winners: HashMap<Vec<u8>, usize> = HashMap::new();
    for d in descriptors {
        if winners.get(&d.name).is_none_or(|&w| precedes(d, &descriptors[w])) {
            winners.insert(d.name.clone(), d.order);
        }
    }
    winners
}

fn tool_name_map(
    descriptors: &[Descriptor<'_>],
    winners: &HashMap<Vec<u8>, usize>,
    accepted: &HashSet<Vec<u8>>,
) -> HashMap<Vec<u8>, Vec<u8>> {
    let mut map: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let wins = |d: &Descriptor<'_>| winners.get(&d.name) == Some(&d.order);
    for d in descriptors {
        if wins(d) && d.direct && accepted.contains(&d.name) {
            map.insert(d.name.clone(), d.name.clone());
        }
    }
    for d in descriptors {
        if !wins(d) || d.direct || d.child_name.is_empty() || !accepted.contains(&d.name) {
            continue;
        }
        map.entry(d.child_name.clone()).or_insert_with(|| d.name.clone());
    }
    map
}

fn tool_description(tool: &Res<'_>) -> Vec<u8> {
    let description = tool.get("description").bytes().into_owned();
    if !description.is_empty() {
        return description;
    }
    tool.get("function.description").bytes().into_owned()
}

fn tool_parameters<'a>(tool: &Res<'a>) -> Option<Res<'a>> {
    [
        "parameters",
        "parametersJsonSchema",
        "input_schema",
        "function.parameters",
        "function.parametersJsonSchema",
    ]
    .iter()
    .map(|p| tool.get(p))
    .find(Res::exists)
}

fn descriptor_to_claude(d: &Descriptor<'_>, claude_name: &[u8]) -> Option<Vec<u8>> {
    let mut name = claude_name.to_vec();
    if name.is_empty() && !d.direct {
        name = d.name.clone();
    }
    let name = trim_space(&name).to_vec();
    if d.client_search {
        return Some(tool_search_tool_to_claude(&d.tool, claude_name));
    }
    let fallback = || common::sanitize_claude_function_name(&tool_name(&d.tool));
    match d.kind.as_slice() {
        b"function" => {
            let name = if name.is_empty() { fallback() } else { name };
            if name.is_empty() {
                return None;
            }
            let mut t = br#"{"name":"","description":"","input_schema":{"type":"object","properties":{}}}"#.to_vec();
            gj::set_str(&mut t, "name", name);
            let description = tool_description(&d.tool);
            if !description.is_empty() {
                gj::set_str(&mut t, "description", description);
            }
            let params = tool_parameters(&d.tool);
            let schema = common::normalize_claude_tool_input_schema(Some(params.as_ref().map_or(&b""[..], |p| &p.raw)));
            gj::set_raw(&mut t, "input_schema", schema);
            t = common::attach_cache_control(t, &d.tool);
            if !gj::get(&t, "cache_control").exists() {
                t = common::attach_cache_control(t, &d.tool.get("function"));
            }
            Some(t)
        }
        b"custom" => {
            let name = if name.is_empty() { fallback() } else { name };
            if name.is_empty() {
                return None;
            }
            let mut t = br#"{"name":"","description":"","input_schema":{"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}}"#.to_vec();
            gj::set_str(&mut t, "name", name);
            let description = tool_description(&d.tool);
            if !description.is_empty() {
                gj::set_str(&mut t, "description", description);
            }
            if crate::apply_patch::is_custom_tool(&d.tool) {
                gj::set_str(&mut t, "description", crate::apply_patch::description(&d.tool));
                gj::set_raw(&mut t, "input_schema", crate::apply_patch::PARAMETERS);
            }
            Some(common::attach_cache_control(t, &d.tool))
        }
        b"web_search" => {
            let tool = &d.tool;
            let access = tool.get("external_web_access");
            if access.exists() && !access.bool() {
                return None;
            }
            let mut name = trim_space(&tool.get("name").bytes()).to_vec();
            if name.is_empty() {
                name = b"web_search".to_vec();
            }
            let mut t = br#"{"type":"web_search_20250305","name":""}"#.to_vec();
            gj::set_str(&mut t, "name", name);
            let max_uses = tool.get("max_uses");
            if max_uses.exists() {
                gj::set_int(&mut t, "max_uses", max_uses.int());
            }
            let domains = tool.get("filters.allowed_domains");
            if domains.is_array() {
                gj::set_raw(&mut t, "allowed_domains", &domains.raw);
            }
            let location = tool.get("user_location");
            if location.is_object() {
                gj::set_raw(&mut t, "user_location", &location.raw);
            }
            Some(t)
        }
        kind => {
            if unsupported_builtin(kind) || d.tool.get("name").bytes().is_empty() {
                return None;
            }
            Some(d.tool.raw.to_vec())
        }
    }
}

/// Claude-safe tool names: valid names are kept, invalid ones sanitized when that stays
/// unique, otherwise suffixed with a hash.
pub(crate) struct ToolNames {
    to_claude: HashMap<Vec<u8>, Vec<u8>>,
    from_claude: HashMap<Vec<u8>, Vec<u8>>,
}

fn valid_claude_name(name: &[u8]) -> bool {
    (1..=64).contains(&name.len())
        && name
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
}

impl ToolNames {
    pub(crate) fn build(root: &Res<'_>, tool_search: bool) -> Self {
        let descriptors = tool_descriptors(root, tool_search);
        let winners = tool_winners(&descriptors);
        Self::with_descriptors(root, &descriptors, &winners)
    }

    pub(crate) fn with_descriptors(
        root: &Res<'_>,
        descriptors: &[Descriptor<'_>],
        winners: &HashMap<Vec<u8>, usize>,
    ) -> Self {
        let mut names = ToolNames {
            to_claude: HashMap::new(),
            from_claude: HashMap::new(),
        };
        let mut taken: HashSet<Vec<u8>> = HashSet::new();
        let mut declared = vec![];
        for d in descriptors {
            if winners.get(&d.name) != Some(&d.order) {
                continue;
            }
            match d.kind.as_slice() {
                b"function" | b"custom" => declared.push(d.name.clone()),
                _ => names.assign(d.name.clone(), d.name.clone(), &mut taken),
            }
        }
        names.allocate(&declared, &mut taken);
        names.allocate(&history_tool_identities(root), &mut taken);
        names
    }

    pub(crate) fn claude_name(&self, identity: &[u8]) -> Vec<u8> {
        match self.to_claude.get(identity) {
            Some(name) => name.clone(),
            None => common::sanitize_claude_function_name(identity),
        }
    }

    pub(crate) fn identity(&self, claude_name: &[u8]) -> Vec<u8> {
        self.from_claude
            .get(claude_name)
            .cloned()
            .unwrap_or_else(|| claude_name.to_vec())
    }

    fn assign(&mut self, identity: Vec<u8>, name: Vec<u8>, taken: &mut HashSet<Vec<u8>>) {
        self.from_claude.insert(name.clone(), identity.clone());
        taken.insert(name.clone());
        self.to_claude.insert(identity, name);
    }

    fn allocate(&mut self, identities: &[Vec<u8>], taken: &mut HashSet<Vec<u8>>) {
        let mut changed = vec![];
        let mut seen = HashSet::new();
        for id in identities {
            if self.to_claude.contains_key(id) || id.is_empty() || !seen.insert(id.clone()) {
                continue;
            }
            if valid_claude_name(id) && !taken.contains(id) {
                self.assign(id.clone(), id.clone(), taken);
                continue;
            }
            changed.push(id.clone());
        }
        let mut count: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
        for id in &changed {
            *count.entry(common::sanitize_claude_function_name(id)).or_default() += 1;
        }
        let mut hashed = vec![];
        for id in changed {
            let base = common::sanitize_claude_function_name(&id);
            if count[&base] == 1 && !taken.contains(&base) {
                self.assign(id, base, taken);
            } else {
                hashed.push(id);
            }
        }
        hashed.sort();
        for id in hashed {
            let mut base = common::sanitize_claude_function_name(&id);
            base.truncate(53);
            for n in 0.. {
                let mut seed = id.clone();
                if n > 0 {
                    seed.push(0);
                    seed.extend_from_slice(n.to_string().as_bytes());
                }
                let digest = common::hex(&Sha256::digest(&seed));
                let mut name = base.clone();
                name.push(b'_');
                name.extend_from_slice(&digest.as_bytes()[..10]);
                if !taken.contains(&name) {
                    self.assign(id, name, taken);
                    break;
                }
            }
        }
    }
}

fn history_tool_identities(root: &Res<'_>) -> Vec<Vec<u8>> {
    let mut ids = vec![];
    let input = root.get("input");
    if input.is_array() {
        input.each(|_, item| {
            if matches!(&*item.get("type").bytes(), b"function_call" | b"custom_tool_call") {
                let mut name = item.get("name").bytes().into_owned();
                let namespace = trim_space(&item.get("namespace").bytes()).to_vec();
                if !namespace.is_empty() {
                    name = qualify_namespace_name(&namespace, &name);
                }
                if !name.is_empty() {
                    ids.push(name);
                }
            }
            true
        });
    }
    ids
}

/// splitResponsesQualifiedFunctionCallFromRequest: a Claude tool name back to the
/// client's `(name, namespace)`.
pub(crate) fn split_qualified_call(request: &[u8], qualified: &[u8], tool_search: bool) -> (Vec<u8>, Vec<u8>) {
    let qualified = trim_space(qualified);
    if qualified.is_empty() {
        return (vec![], vec![]);
    }
    let root = gj::parse(request);
    let descriptors = tool_descriptors(&root, tool_search);
    let winners = tool_winners(&descriptors);
    let identity = ToolNames::with_descriptors(&root, &descriptors, &winners).identity(qualified);
    match winners.get(&identity).map(|&o| &descriptors[o]) {
        Some(d) if !d.direct => (d.child_name.clone(), d.namespace.clone()),
        _ => (identity, vec![]),
    }
}

// ---------------------------------------------------------------------------------------
// Web search (*_web_search.go)

pub(crate) fn web_search_call_id(claude_tool_use_id: &[u8]) -> Vec<u8> {
    [WEB_SEARCH_ID_PREFIX, claude_tool_use_id].concat()
}

fn web_search_tool_use_id(item_id: &[u8]) -> Vec<u8> {
    let body = trim_space(item_id);
    let body = body.strip_prefix(WEB_SEARCH_ID_PREFIX).unwrap_or(body);
    let body = body.strip_prefix(SERVER_TOOL_ID_PREFIX).unwrap_or(body);
    let mut sanitized = vec![];
    let mut i = 0;
    while i < body.len() {
        let (c, n) = gj::decode_rune(&body[i..]);
        match c {
            Some(c) if c.is_ascii_alphanumeric() || c == '_' => sanitized.push(c as u8),
            _ => sanitized.push(b'_'),
        }
        i += n;
    }
    if sanitized.is_empty() {
        return vec![];
    }
    [SERVER_TOOL_ID_PREFIX, &sanitized].concat()
}

fn web_search_call_to_claude(item: &Res<'_>) -> Vec<Vec<u8>> {
    let id = web_search_tool_use_id(&item.get("id").bytes());
    if id.is_empty() {
        return vec![];
    }
    let mut tool_use = br#"{"type":"server_tool_use","id":"","name":"","input":{}}"#.to_vec();
    gj::set_str(&mut tool_use, "id", &id);
    gj::set_str(&mut tool_use, "name", "web_search");
    let query = ["action.query", "action.queries.0", "action.url"]
        .iter()
        .map(|p| trim_space(&item.get(p).bytes()).to_vec())
        .find(|q| !q.is_empty())
        .unwrap_or_default();
    if !query.is_empty() {
        gj::set_str(&mut tool_use, "input.query", query);
    }
    let mut result = br#"{"type":"web_search_tool_result","tool_use_id":"","content":[]}"#.to_vec();
    gj::set_str(&mut result, "tool_use_id", &id);
    let results = item.get("results");
    let content = if results.is_object() {
        Some(results.raw.to_vec())
    } else if results.is_array() {
        let mut blocks = vec![];
        results.each(|_, entry| {
            if entry.get("type").str() == "web_search_tool_result_error" {
                blocks.push(entry.raw.to_vec());
            } else if !trim_space(&entry.get("encrypted_content").bytes()).is_empty() {
                let mut block = entry.raw.to_vec();
                gj::set_str(&mut block, "type", "web_search_result");
                blocks.push(block);
            }
            true
        });
        (!blocks.is_empty()).then(|| gj::join(&blocks))
    } else {
        None
    };
    if let Some(content) = content {
        gj::set_raw(&mut result, "content", content);
    }
    vec![tool_use, result]
}

fn attach_citations(block: Vec<u8>, annotations: &Res<'_>) -> Vec<u8> {
    if !annotations.is_array() {
        return block;
    }
    let mut citations = vec![];
    annotations.each(|_, a| {
        if !trim_space(&a.get("encrypted_index").bytes()).is_empty() {
            citations.push(a.raw.to_vec());
        }
        true
    });
    if citations.is_empty() {
        return block;
    }
    let mut out = block.clone();
    if gj::set_raw(&mut out, "citations", gj::join(&citations)) {
        out
    } else {
        block
    }
}

// ---------------------------------------------------------------------------------------
// Tool search (claude_openai-responses_tool_search.go)
//
// Codex declares client-executed tool search as {"type":"tool_search","execution":
// "client"} and keeps deferred tools out of tools[]. The model calls it through a
// tool_search_call item, Codex runs the search and replays a tool_search_output item
// whose tools[] become callable from then on. Claude has no counterpart, so the
// declaration becomes an ordinary tool, the call and output a tool_use/tool_result pair,
// and every tool discovered in history is declared on later requests.
//
// With native tool search, discovered tools are declared with defer_loading and
// referenced through tool_reference blocks. Anthropic keeps the prompt cache when
// deferred tools are added, but not when a request goes from no deferred tool to one,
// so a constant deferred anchor is declared from the first request on and discoveries
// only append after it (97ba1e89).

/// Both the declaration type and the tool name, as Codex names the tool after its type.
pub(crate) const TOOL_SEARCH: &[u8] = b"tool_search";
/// Codex's item id prefix for tool_search_call.
pub(crate) const TOOL_SEARCH_ID_PREFIX: &[u8] = b"tsc_";
/// The constant deferred tool declared with native tool search; nothing references it.
const DEFERRED_ANCHOR_NAME: &str = "tool_search_deferred_anchor";
const DEFERRED_ANCHOR: &[u8] = br#"{"name":"tool_search_deferred_anchor","description":"Placeholder that is never loaded; tool search returns the real tools.","input_schema":{"type":"object","properties":{}},"defer_loading":true}"#;
const TOOL_SEARCH_SCHEMA: &[u8] = br#"{"type":"object","properties":{"query":{"type":"string","description":"Search query for deferred tools."},"limit":{"type":"number","description":"Maximum number of tools to return."}},"required":["query"],"additionalProperties":false}"#;

/// isResponsesClientToolSearch: hosted (server) search cannot run on Claude.
fn is_client_tool_search(tool: &Res<'_>) -> bool {
    trim_space(&tool.get("type").bytes()) == TOOL_SEARCH && trim_space(&tool.get("execution").bytes()) == b"client"
}

/// convertResponsesToolSearchToolToClaude: the client's declared schema, with the
/// required string `query` Codex parses.
fn tool_search_tool_to_claude(tool: &Res<'_>, claude_name: &[u8]) -> Vec<u8> {
    let parameters = tool.get("parameters");
    let mut schema = if parameters.is_object() {
        common::normalize_claude_tool_input_schema(Some(&parameters.raw))
    } else {
        TOOL_SEARCH_SCHEMA.to_vec()
    };
    if gj::get(&schema, "properties.query.type").str() != "string" {
        gj::set_raw(
            &mut schema,
            "properties.query",
            br#"{"type":"string","description":"Search query for deferred tools."}"#,
        );
    }
    let required = gj::get(&schema, "required");
    if !(required.is_array()
        && required
            .array()
            .iter()
            .any(|r| r.kind == Kind::String && &*r.s == b"query"))
    {
        gj::set_str(&mut schema, "required.-1", "query");
    }
    let mut t = br#"{"name":"","description":"","input_schema":{}}"#.to_vec();
    gj::set_str(&mut t, "name", claude_name);
    gj::set_str(&mut t, "description", tool_description(tool));
    gj::set_raw(&mut t, "input_schema", schema);
    t
}

/// convertResponsesToolSearchCallToClaude: a client tool_search_call as an assistant
/// tool_use, with its raw call id. Hosted calls have no call id.
fn tool_search_call_to_claude(item: &Res<'_>, claude_name: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let call_id = trim_space(&item.get("call_id").bytes()).to_vec();
    if call_id.is_empty() || trim_space(&item.get("execution").bytes()) != b"client" {
        return None;
    }
    let mut tool_use = br#"{"type":"tool_use","id":"","name":"","input":{}}"#.to_vec();
    gj::set_str(&mut tool_use, "id", common::sanitize_claude_tool_id(&call_id));
    gj::set_str(&mut tool_use, "name", claude_name);
    let arguments = item.get("arguments");
    let decoded;
    let arguments = if arguments.kind == Kind::String {
        decoded = arguments.s.to_vec();
        gj::parse(&decoded)
    } else {
        arguments
    };
    if arguments.is_object() {
        gj::set_raw(&mut tool_use, "input", &arguments.raw);
    }
    Some((tool_use, call_id))
}

/// The descriptors of the tools a tool_search_output loaded.
fn discovered<'a>(item: &Res<'a>) -> Vec<Descriptor<'a>> {
    let tools = item.get("tools");
    let mut out = vec![];
    append_descriptors(&mut out, &tools, DISCOVERED, true);
    out
}

/// responsesToolSearchOutputText: discovered tools for the model, by name; their
/// definitions are declared in tools[].
fn tool_search_output_text(item: &Res<'_>, names: &ToolNames) -> Vec<u8> {
    let found: Vec<Vec<u8>> = discovered(item).iter().map(|d| names.claude_name(&d.name)).collect();
    if found.is_empty() {
        return b"No matching tools were found.".to_vec();
    }
    [&b"Tools now available to call: "[..], &found.join(&b", "[..]), b"."].concat()
}

/// responsesToolSearchOutputContent: the raw tool_result content of a paired
/// tool_search_output. With native tool search, a tool_reference for each tool that
/// loads only through search, so Claude expands its deferred definition here; tools
/// declared directly need none, and without any the text summary is used.
fn tool_search_output_content(
    item: &Res<'_>,
    names: &ToolNames,
    descriptors: &[Descriptor<'_>],
    winners: &HashMap<Vec<u8>, usize>,
    native: bool,
) -> Vec<u8> {
    if native {
        let mut references = vec![];
        let mut seen = HashSet::new();
        for d in discovered(item) {
            let Some(winner) = winners.get(&d.name).map(|&w| &descriptors[w]) else {
                continue;
            };
            if winner.priority != DISCOVERED || !matches!(winner.kind.as_slice(), b"function" | b"custom") {
                continue;
            }
            let claude_name = names.claude_name(&d.name);
            if !seen.insert(claude_name.clone()) {
                continue;
            }
            let mut reference = br#"{"type":"tool_reference","tool_name":""}"#.to_vec();
            gj::set_str(&mut reference, "tool_name", claude_name);
            references.push(reference);
        }
        if !references.is_empty() {
            return gj::join(&references);
        }
    }
    let mut text = br#"{"text":""}"#.to_vec();
    gj::set_str(&mut text, "text", tool_search_output_text(item, names));
    gj::get(&text, "text").raw.to_vec()
}
