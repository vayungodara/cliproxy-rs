//! Claude Messages request -> Codex (OpenAI Responses) request
//! (internal/translator/codex/claude/codex_claude_request.go).

use std::collections::{HashMap, HashSet};

use cpa_common::json::{self as gj, GoValue, Kind, Res};
use cpa_common::signature::{self as sig, BlockKind, Provider};
use sha2::{Digest, Sha256};

use crate::common::{self, go_lower, trim_space};
use crate::{Error, Registered, RequestCtx, codex_claude_response as response, openai_claude};

pub static PAIR: Registered = registered!(
    Claude -> Codex,
    request: |ctx, body| Ok(convert(ctx.model, body, false)),
    non_stream: response::non_stream,
    go_stream: response::go_stream,
    token_count: Some(openai_claude::claude_input_tokens),
);

/// ConvertClaudeRequestToCodexWithCompat: assistant thinking blocks with empty or
/// unknown-format signatures are kept for compatibility endpoints.
pub fn request_with_compat(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    Ok(convert(ctx.model, body, true))
}

const NAME_LIMIT: usize = 64;

/// shortenNameIfNeeded: at most 64 bytes, keeping `mcp__` plus the last `__` segment.
fn shorten_name(name: &[u8]) -> Vec<u8> {
    if name.len() <= NAME_LIMIT {
        return name.to_vec();
    }
    if name.starts_with(b"mcp__")
        && let Some(idx) = name.windows(2).rposition(|w| w == b"__")
        && idx > 0
    {
        let mut candidate = [&b"mcp__"[..], &name[idx + 2..]].concat();
        candidate.truncate(NAME_LIMIT);
        return candidate;
    }
    name[..NAME_LIMIT].to_vec()
}

/// buildShortNameMap: unique short names, suffixing `_1`, `_2`, ... on collisions.
fn short_name_map(names: &[Vec<u8>]) -> HashMap<Vec<u8>, Vec<u8>> {
    let mut used: HashSet<Vec<u8>> = HashSet::new();
    let mut map = HashMap::new();
    for name in names {
        let candidate = shorten_name(name);
        let unique = if used.contains(&candidate) {
            (1..)
                .map(|i| {
                    let suffix = format!("_{i}");
                    let allowed = NAME_LIMIT.saturating_sub(suffix.len());
                    [&candidate[..candidate.len().min(allowed)], suffix.as_bytes()].concat()
                })
                .find(|tmp| !used.contains(tmp))
                .unwrap()
        } else {
            candidate
        };
        used.insert(unique.clone());
        map.insert(name.clone(), unique);
    }
    map
}

fn tool_names(raw: &[u8]) -> Vec<Vec<u8>> {
    let tools = gj::get(raw, "tools");
    if !tools.is_array() {
        return vec![];
    }
    tools
        .array()
        .iter()
        .map(|t| t.get("name").bytes().into_owned())
        .filter(|n| !n.is_empty())
        .collect()
}

/// buildReverseMapFromClaudeOriginalToShort: original tool name -> short name.
fn original_to_short(raw: &[u8]) -> HashMap<Vec<u8>, Vec<u8>> {
    short_name_map(&tool_names(raw))
}

/// buildReverseMapFromClaudeOriginalShortToOriginal: short name -> original tool name.
pub(crate) fn short_to_original(raw: &[u8]) -> HashMap<Vec<u8>, Vec<u8>> {
    original_to_short(raw)
        .into_iter()
        .map(|(orig, short)| (short, orig))
        .collect()
}

fn mapped_name(map: &HashMap<Vec<u8>, Vec<u8>>, name: &[u8]) -> Vec<u8> {
    map.get(name).cloned().unwrap_or_else(|| shorten_name(name))
}

/// shortenCodexCallIDIfNeeded: call IDs within the Responses 64-byte limit, with a
/// sha256-derived suffix.
pub(crate) fn shorten_call_id(id: &[u8]) -> Vec<u8> {
    if id.len() <= NAME_LIMIT {
        return id.to_vec();
    }
    let sum = Sha256::digest(id);
    let suffix = format!("_{}", common::hex(&sum[..8]));
    [&id[..NAME_LIMIT - suffix.len()], suffix.as_bytes()].concat()
}

/// normalizeCodexServiceTier.
fn service_tier(r: &Res<'_>) -> &'static str {
    if r.kind != Kind::String {
        return "";
    }
    match go_lower(trim_space(&r.s)).as_slice() {
        b"fast" | b"priority" => "priority",
        _ => "",
    }
}

fn is_web_search_tool(kind: &[u8]) -> bool {
    kind == b"web_search_20250305" || kind == b"web_search_20260209"
}

/// convertClaudeToolChoiceToCodex.
fn tool_choice(choice: &Res<'_>, names: &HashMap<Vec<u8>, Vec<u8>>, web_search: &HashSet<Vec<u8>>) -> Vec<u8> {
    if !choice.exists() || choice.kind == Kind::Null {
        return br#""auto""#.to_vec();
    }
    let mut kind = choice.get("type").bytes().into_owned();
    if kind.is_empty() && choice.kind == Kind::String {
        kind = choice.s.to_vec();
    }
    match kind.as_slice() {
        b"any" => br#""required""#.to_vec(),
        b"none" => br#""none""#.to_vec(),
        b"tool" => {
            let name = choice.get("name").bytes().into_owned();
            if web_search.contains(&name) {
                return br#"{"type":"web_search"}"#.to_vec();
            }
            let name = mapped_name(names, &name);
            if name.is_empty() {
                return br#""auto""#.to_vec();
            }
            let mut out = br#"{"type":"function","name":""}"#.to_vec();
            gj::set_str(&mut out, "name", &name);
            out
        }
        _ => br#""auto""#.to_vec(),
    }
}

/// convertClaudeWebSearchToolToCodex.
fn web_search_tool(tool: &Res<'_>) -> Vec<u8> {
    let mut out = br#"{"type":"web_search"}"#.to_vec();
    let domains = tool.get("allowed_domains");
    if domains.is_array() {
        gj::set_raw(&mut out, "filters.allowed_domains", &domains.raw);
    }
    let location = tool.get("user_location");
    if location.is_object() {
        gj::set_raw(&mut out, "user_location", &location.raw);
    }
    out
}

fn strip_dialect_keywords(value: &mut GoValue) {
    let has_escape = |s: &str| common::has_unsupported_unicode_property_escape(s.as_bytes());
    match value {
        GoValue::Object(map) => {
            map.remove("$schema");
            map.remove("$id");
            if matches!(map.get("pattern"), Some(GoValue::String(p)) if has_escape(p)) {
                map.remove("pattern");
            }
            if let Some(GoValue::Object(patterns)) = map.get_mut("patternProperties") {
                patterns.retain(|k, _| !has_escape(k));
                patterns.values_mut().for_each(strip_dialect_keywords);
            }
            for key in common::SCHEMA_MAP_KEYWORDS {
                let key = std::str::from_utf8(key).unwrap_or_default();
                if key == "patternProperties" {
                    continue;
                }
                if let Some(GoValue::Object(sub)) = map.get_mut(key) {
                    sub.values_mut().for_each(strip_dialect_keywords);
                }
            }
            for key in common::SCHEMA_VALUE_KEYWORDS {
                match map.get_mut(std::str::from_utf8(key).unwrap_or_default()) {
                    Some(sub @ GoValue::Object(_)) => strip_dialect_keywords(sub),
                    Some(GoValue::Array(items)) => items.iter_mut().for_each(strip_dialect_keywords),
                    _ => {}
                }
            }
        }
        GoValue::Array(items) => items.iter_mut().for_each(strip_dialect_keywords),
        _ => {}
    }
}

/// normalizeToolParameters: an object schema with `properties`, without dialect keywords
/// or unsupported regex escapes, re-encoded without HTML escaping.
pub(crate) fn normalize_tool_parameters(raw: &[u8]) -> Vec<u8> {
    const DEFAULT: &[u8] = br#"{"type":"object","properties":{}}"#;
    let raw = trim_space(raw);
    if raw.is_empty() || raw == b"null" || !gj::valid(raw) {
        return DEFAULT.to_vec();
    }
    let Some(mut root) = GoValue::parse(raw).filter(|v| matches!(v, GoValue::Object(_))) else {
        return DEFAULT.to_vec();
    };
    strip_dialect_keywords(&mut root);
    let GoValue::Object(map) = &mut root else {
        return DEFAULT.to_vec();
    };
    let is_object = match map.get("type") {
        None | Some(GoValue::Null) => true,
        Some(GoValue::String(t)) if t.is_empty() => true,
        Some(GoValue::String(t)) => t == "object",
        Some(GoValue::Array(items)) => items.iter().any(|i| matches!(i, GoValue::String(s) if s == "object")),
        _ => false,
    };
    if matches!(map.get("type"), None | Some(GoValue::Null))
        || matches!(map.get("type"), Some(GoValue::String(t)) if t.is_empty())
    {
        map.insert("type".into(), GoValue::String("object".into()));
    }
    if is_object && matches!(map.get("properties"), None | Some(GoValue::Null)) {
        map.insert("properties".into(), GoValue::Object(Default::default()));
    }
    root.marshal_no_html()
}

/// codexSchemaMissesRequired: a declared property missing from its sibling `required`.
fn misses_required(schema: &Res<'_>) -> bool {
    if !schema.is_object() {
        return schema.is_array() && schema.array().iter().any(misses_required);
    }
    let properties = schema.get("properties");
    if properties.is_object() {
        let required = schema.get("required");
        if !required.is_array() {
            return !properties.map().is_empty();
        }
        let names: HashSet<Vec<u8>> = required
            .array()
            .iter()
            .filter(|i| i.kind == Kind::String)
            .map(|i| i.s.to_vec())
            .collect();
        if properties.map().iter().any(|(name, _)| !names.contains(name)) {
            return true;
        }
    }
    for key in common::SCHEMA_MAP_KEYWORDS {
        let children = schema.get(key);
        if children.is_object() {
            let mut miss = false;
            children.each(|_, child| {
                miss = misses_required(&child);
                !miss
            });
            if miss {
                return true;
            }
        }
    }
    common::SCHEMA_VALUE_KEYWORDS.iter().any(|key| {
        let child = schema.get(*key);
        child.exists() && misses_required(&child)
    })
}

/// codexClaudeTargetAcceptsGrokSignature.
fn accepts_grok_signature(model: &str) -> bool {
    let base = cpa_common::thinking::parse_suffix(model).model_name;
    go_lower(trim_space(base.as_bytes())).windows(4).any(|w| w == b"grok")
}

fn data_url(media_type: &[u8], data: &[u8]) -> Vec<u8> {
    [b"data:", media_type, b";base64,", data].concat()
}

/// An image source as a data URL: `data` or `base64`, with `media_type` or `mime_type`.
fn image_data_url(source: &Res<'_>) -> Option<Vec<u8>> {
    if !source.exists() {
        return None;
    }
    let mut data = source.get("data").bytes().into_owned();
    if data.is_empty() {
        data = source.get("base64").bytes().into_owned();
    }
    if data.is_empty() {
        return None;
    }
    let mut media = source.get("media_type").bytes().into_owned();
    if media.is_empty() {
        media = source.get("mime_type").bytes().into_owned();
    }
    if media.is_empty() {
        media = b"application/octet-stream".to_vec();
    }
    Some(data_url(&media, &data))
}

fn input_image(url: &[u8]) -> Vec<u8> {
    let mut part = br#"{"type":"input_image","image_url":""}"#.to_vec();
    gj::set_str(&mut part, "image_url", url);
    part
}

/// Message conversion state (the `messages` loop).
struct Messages<'m> {
    model: &'m str,
    preserve: bool,
    names: &'m HashMap<Vec<u8>, Vec<u8>>,
    items: Vec<Vec<u8>>,
    pending_tool_uses: Vec<Vec<u8>>,
    pending_reminders: Vec<Vec<u8>>,
}

impl Messages<'_> {
    fn flush_reminders(&mut self) {
        self.items.append(&mut self.pending_reminders);
    }

    fn message(&mut self, message: &Res<'_>) {
        let role = message.get("role").bytes().into_owned();
        if role == b"system" {
            if let Some(text) = common::claude_message_system_reminder_text(&message.get("content")) {
                let mut item =
                    br#"{"type":"message","role":"user","content":[{"type":"input_text","text":""}]}"#.to_vec();
                gj::set_str(&mut item, "content.0.text", &text);
                if self.pending_tool_uses.is_empty() {
                    self.items.push(item);
                } else {
                    self.pending_reminders.push(item);
                }
            }
            return;
        }
        let content = message.get("content");
        let mut parts = if content.is_array() { content.array() } else { vec![] };
        if role == b"user" && !self.pending_tool_uses.is_empty() && content.is_array() {
            parts = common::align_claude_tool_results(parts, &self.pending_tool_uses);
        }
        self.pending_tool_uses.clear();
        let mut current: Vec<Vec<u8>> = vec![];
        let flush = |items: &mut Vec<Vec<u8>>, current: &mut Vec<Vec<u8>>| {
            if !current.is_empty() {
                let mut item = br#"{"type":"message","role":""}"#.to_vec();
                gj::set_str(&mut item, "role", &role);
                gj::set_raw(&mut item, "content", gj::join(current));
                items.push(item);
                current.clear();
            }
        };
        let text_part = |text: &[u8]| {
            let kind = if role == b"assistant" {
                "output_text"
            } else {
                "input_text"
            };
            let mut part = br#"{"type":"","text":""}"#.to_vec();
            gj::set_str(&mut part, "type", kind);
            gj::set_str(&mut part, "text", text);
            part
        };
        if content.is_array() {
            for part in &parts {
                match part.get("type").bytes().as_ref() {
                    b"text" => {
                        self.flush_reminders();
                        current.push(text_part(&part.get("text").bytes()));
                    }
                    b"thinking" => {
                        if role != b"assistant" {
                            continue;
                        }
                        let Some(signature) = self.reasoning_signature(part) else {
                            continue;
                        };
                        flush(&mut self.items, &mut current);
                        let mut item = br#"{"type":"reasoning","summary":[],"content":null}"#.to_vec();
                        gj::set_str(&mut item, "encrypted_content", &signature);
                        self.items.push(item);
                    }
                    b"image" => {
                        self.flush_reminders();
                        if let Some(url) = image_data_url(&part.get("source")) {
                            current.push(input_image(&url));
                        }
                    }
                    b"document" => {
                        self.flush_reminders();
                        let source = part.get("source");
                        if source.get("type").bytes().as_ref() != b"base64" {
                            continue;
                        }
                        let media = trim_space(&source.get("media_type").bytes()).to_vec();
                        if !eq_fold(&media, "application/pdf") {
                            continue;
                        }
                        let mut data = source.get("data").bytes().into_owned();
                        if data.is_empty() {
                            data = source.get("base64").bytes().into_owned();
                        }
                        if !data.is_empty() {
                            let mut file =
                                br#"{"type":"input_file","file_data":"","filename":"document.pdf"}"#.to_vec();
                            gj::set_str(&mut file, "file_data", data_url(&media, &data));
                            current.push(file);
                        }
                    }
                    b"tool_use" => {
                        flush(&mut self.items, &mut current);
                        let id = part.get("id").bytes().into_owned();
                        if !id.is_empty() {
                            self.pending_tool_uses.push(id.clone());
                        }
                        let mut call = br#"{"type":"function_call"}"#.to_vec();
                        gj::set_str(&mut call, "call_id", shorten_call_id(&id));
                        gj::set_str(&mut call, "name", mapped_name(self.names, &part.get("name").bytes()));
                        gj::set_str(&mut call, "arguments", &part.get("input").raw);
                        self.items.push(call);
                    }
                    b"tool_result" => {
                        flush(&mut self.items, &mut current);
                        self.items.push(tool_result(part));
                    }
                    _ => {}
                }
            }
            flush(&mut self.items, &mut current);
            self.flush_reminders();
        } else if content.kind == Kind::String {
            current.push(text_part(&content.s));
            flush(&mut self.items, &mut current);
            self.flush_reminders();
        }
    }

    /// appendReasoningContent's signature choice: GPT-compatible signatures, then (compat)
    /// empty or unknown-format ones, then Grok encrypted content for Grok targets.
    fn reasoning_signature(&self, part: &Res<'_>) -> Option<Vec<u8>> {
        let raw = part.get("signature").bytes().into_owned();
        if let Some(signature) = sig::compatible_signature_for_provider(Provider::Gpt, &raw) {
            return Some(signature.into_bytes());
        }
        let blank = trim_space(&raw).is_empty();
        if self.preserve
            && part.get("signature").kind == Kind::String
            && !blank
            && sig::detect_provider_for_block(&raw, BlockKind::ClaudeThinking) == Provider::Unknown
        {
            return Some(raw);
        }
        if self.preserve && blank {
            return Some(raw);
        }
        if !accepts_grok_signature(self.model) || sig::inspect_grok_encrypted_content(&raw).is_err() {
            return None;
        }
        Some(raw)
    }
}

/// A `tool_result` block as a `function_call_output` item.
fn tool_result(part: &Res<'_>) -> Vec<u8> {
    let mut output = br#"{"type":"function_call_output"}"#.to_vec();
    gj::set_str(
        &mut output,
        "call_id",
        shorten_call_id(&part.get("tool_use_id").bytes()),
    );
    let content = part.get("content");
    let mut items = vec![];
    if content.is_array() {
        for block in content.array() {
            match block.get("type").bytes().as_ref() {
                b"image" => {
                    if let Some(url) = image_data_url(&block.get("source")) {
                        items.push(input_image(&url));
                    }
                }
                b"text" => {
                    let mut text = br#"{"type":"input_text","text":""}"#.to_vec();
                    gj::set_str(&mut text, "text", block.get("text").bytes());
                    items.push(text);
                }
                _ => {}
            }
        }
    }
    if items.is_empty() {
        gj::set_str(&mut output, "output", content.bytes());
    } else {
        gj::set_raw(&mut output, "output", gj::join(&items));
    }
    output
}

fn eq_fold(value: &[u8], token: &str) -> bool {
    use cpa_common::gostr::GoStr;
    String::from_utf8_lossy(value).go_eq_fold(token)
}

/// convertClaudeRequestToCodex.
fn convert(model: &str, raw: &[u8], preserve: bool) -> Vec<u8> {
    let mut out = br#"{"model":"","instructions":"","input":[]}"#.to_vec();
    let root = gj::parse(raw);
    let names = original_to_short(raw);
    gj::set_str(&mut out, "model", model);
    let mut state = Messages {
        model,
        preserve,
        names: &names,
        items: vec![],
        pending_tool_uses: vec![],
        pending_reminders: vec![],
    };

    let system = root.get("system");
    if system.exists() {
        let mut parts: Vec<Vec<u8>> = vec![];
        let mut add = |text: &[u8]| {
            if !text.is_empty() && !common::is_claude_code_attribution_text(text) {
                let mut part = br#"{"type":"input_text","text":""}"#.to_vec();
                gj::set_str(&mut part, "text", text);
                parts.push(part);
            }
        };
        if system.kind == Kind::String {
            add(&system.s);
        } else if system.is_array() {
            for block in system.array() {
                if block.get("type").bytes().as_ref() == b"text" {
                    add(&block.get("text").bytes());
                }
            }
        }
        if !parts.is_empty() {
            let mut message = br#"{"type":"message","role":"developer"}"#.to_vec();
            gj::set_raw(&mut message, "content", gj::join(&parts));
            state.items.push(message);
        }
    }

    let messages = root.get("messages");
    if messages.is_array() {
        for message in messages.array() {
            state.message(&message);
        }
        state.flush_reminders();
    }

    let tools = root.get("tools");
    let mut tool_items: Vec<Vec<u8>> = vec![];
    if tools.is_array() {
        let web_search: HashSet<Vec<u8>> = tools
            .array()
            .iter()
            .filter(|t| is_web_search_tool(&t.get("type").bytes()))
            .map(|t| t.get("name").bytes().into_owned())
            .filter(|n| !n.is_empty())
            .collect();
        gj::set_raw(
            &mut out,
            "tool_choice",
            tool_choice(&root.get("tool_choice"), &names, &web_search),
        );
        for tool in tools.array() {
            if is_web_search_tool(&tool.get("type").bytes()) {
                tool_items.push(web_search_tool(&tool));
                continue;
            }
            let mut item = tool.raw.to_vec();
            let kind = tool.get("type");
            if kind.kind != Kind::String || kind.s.as_ref() != b"function" {
                gj::set_str(&mut item, "type", "function");
            }
            let name = tool.get("name");
            if name.exists() {
                let original = name.bytes().into_owned();
                let short = mapped_name(&names, &original);
                if name.kind != Kind::String || short != original {
                    gj::set_str(&mut item, "name", &short);
                }
            }
            gj::set_raw(
                &mut item,
                "parameters",
                normalize_tool_parameters(&tool.get("input_schema").raw),
            );
            for path in ["input_schema", "parameters.$schema", "cache_control", "defer_loading"] {
                if gj::get(&item, path).exists() {
                    gj::delete(&mut item, path);
                }
            }
            if gj::get(&item, "strict").kind != Kind::False {
                gj::set_bool(&mut item, "strict", false);
            }
            tool_items.push(item);
        }
    }

    let disable_parallel = root.get("tool_choice.disable_parallel_tool_use");
    gj::set_bool(
        &mut out,
        "parallel_tool_calls",
        !(disable_parallel.exists() && disable_parallel.bool()),
    );

    let mut effort: Vec<u8> = b"medium".to_vec();
    let thinking = root.get("thinking");
    if thinking.is_object() {
        match thinking.get("type").bytes().as_ref() {
            b"enabled" => {
                let budget = thinking.get("budget_tokens");
                if budget.exists()
                    && let Some(level) = cpa_common::thinking::convert_budget_to_level(budget.int())
                    && !level.is_empty()
                {
                    effort = level.as_bytes().to_vec();
                }
            }
            b"adaptive" | b"auto" => {
                // Claude 4.6 adaptive thinking may carry its effort in output_config.
                let explicit = root.get("output_config.effort");
                let explicit = if explicit.kind == Kind::String {
                    go_lower(trim_space(&explicit.s))
                } else {
                    vec![]
                };
                effort = if explicit.is_empty() {
                    b"xhigh".to_vec()
                } else {
                    explicit
                };
            }
            b"disabled" => {
                if let Some(level) = cpa_common::thinking::convert_budget_to_level(0)
                    && !level.is_empty()
                {
                    effort = level.as_bytes().to_vec();
                }
            }
            _ => {}
        }
    }
    gj::set_str(&mut out, "reasoning.effort", &effort);
    let mut tier = service_tier(&root.get("service_tier"));
    let speed = root.get("speed");
    if speed.kind == Kind::String && speed.s.as_ref() == b"fast" {
        tier = "priority";
    }
    if !tier.is_empty() {
        gj::set_str(&mut out, "service_tier", tier);
    }
    gj::set_bool(&mut out, "stream", true);
    gj::set_bool(&mut out, "store", false);
    gj::set_strs(&mut out, "include", &["reasoning.encrypted_content"]);

    let format = root.get("output_config.format");
    if format.is_object() && format.get("type").bytes().as_ref() == b"json_schema" && format.get("schema").is_object() {
        let mut name = format.get("name").bytes().into_owned();
        if name.is_empty() {
            name = b"cli_proxy_structured_output".to_vec();
        }
        let schema = format.get("schema");
        let strict = format.get("strict").kind != Kind::False && !misses_required(&schema);
        let mut translated = br#"{"type":"json_schema","name":"","strict":true,"schema":{}}"#.to_vec();
        gj::set_str(&mut translated, "name", &name);
        gj::set_bool(&mut translated, "strict", strict);
        gj::set_raw(&mut translated, "schema", &schema.raw);
        gj::set_raw(&mut out, "text.format", translated);
    }
    if tools.is_array() {
        gj::set_raw(&mut out, "tools", gj::join(&tool_items));
    }
    gj::set_items(&mut out, "input", &state.items);
    out
}
