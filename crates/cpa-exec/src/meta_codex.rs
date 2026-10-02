//! Adapters for the shared Codex/Responses helpers the Meta executor calls but this
//! thread does not own. Each function names its owner; at integration the owner's module
//! replaces it and the Meta fixtures (tests/device_fixtures/meta) are the acceptance test.
//!
//! The ports here are exact for the inputs the fixtures cover. The two identity adapters
//! (reasoning encrypted-content sanitizer, Codex tool integer types) change nothing for
//! requests without reasoning input items or a Codex CLI User-Agent.

use std::collections::BTreeMap;

use bytes::Bytes;
use cpa_translate::{Error, StreamTranslator};
use gjson::Kind;
use http::HeaderMap;

use crate::kimi_json::{delete, gstr, join_array, set_raw, set_str, valid};

// ---------------------------------------------------------------------------------------
// ponytail: adapter, owner the translators thread (cpa-translate). Go's openai-response ->
// codex pair (internal/translator/codex/openai/responses). Call sites go through
// `cpa_translate::pair` first and fall back to these only while the pair is unregistered.
// ---------------------------------------------------------------------------------------

/// `ConvertOpenAIResponsesRequestToCodex`.
pub(crate) fn responses_to_codex(body: &str) -> String {
    let mut body = body.to_owned();
    let input = gjson::get(&body, "input");
    if input.kind() == Kind::String {
        let wrapped = set_str(
            r#"[{"type":"message","role":"user","content":[{"type":"input_text","text":""}]}]"#,
            "0.content.0.text",
            input.str(),
        );
        if let Ok(wrapped) = wrapped
            && let Ok(updated) = set_raw(&body, "input", &wrapped)
        {
            body = updated;
        }
    }
    body = required_bool(&body, "stream", true);
    body = required_bool(&body, "store", false);
    body = required_bool(&body, "parallel_tool_calls", true);
    body = required_include(&body);
    body = delete_existing(
        &body,
        &["max_output_tokens", "max_completion_tokens", "temperature", "top_p"],
    );
    let tier = gjson::get(&body, "service_tier");
    if tier.exists() {
        if tier.kind() == Kind::String {
            let current = tier.str().to_owned();
            match current.trim().to_ascii_lowercase().as_str() {
                "priority" | "fast" if current != "priority" => {
                    body = set_str(&body, "service_tier", "priority").unwrap_or(body);
                }
                "ultrafast" if current != "ultrafast" => {
                    body = set_str(&body, "service_tier", "ultrafast").unwrap_or(body);
                }
                "priority" | "fast" | "ultrafast" => {}
                _ => body = delete_existing(&body, &["service_tier"]),
            }
        } else {
            body = delete_existing(&body, &["service_tier"]);
        }
    }
    body = delete_existing(&body, &["truncation", "prompt_cache_options", "prompt_cache_retention"]);
    body = strip_cache_breakpoints(&body);
    if gjson::get(&body, "context_management").exists() {
        body = delete(&body, "context_management");
    }
    body = delete_existing(&body, &["user"]);
    body = system_role_to_developer(&body);
    body = builtin_tool_array(&body, "tools");
    body = builtin_tool_at(&body, "tool_choice.type");
    body = builtin_tool_array(&body, "tool_choice.tools");
    empty_function_call_arguments(&body)
}

fn required_bool(body: &str, path: &str, value: bool) -> String {
    let kind = gjson::get(body, path).kind();
    if (value && kind == Kind::True) || (!value && kind == Kind::False) {
        return body.to_owned();
    }
    set_raw(body, path, if value { "true" } else { "false" }).unwrap_or_else(|_| body.to_owned())
}

fn required_include(body: &str) -> String {
    let current = gjson::get(body, "include");
    let values = current.array();
    if current.kind() == Kind::Array
        && values.len() == 1
        && values[0].kind() == Kind::String
        && values[0].str() == "reasoning.encrypted_content"
    {
        return body.to_owned();
    }
    set_raw(body, "include", r#"["reasoning.encrypted_content"]"#).unwrap_or_else(|_| body.to_owned())
}

fn delete_existing(body: &str, paths: &[&str]) -> String {
    let mut body = body.to_owned();
    for path in paths {
        if gjson::get(&body, path).exists() {
            body = delete(&body, path);
        }
    }
    body
}

fn strip_cache_breakpoints(body: &str) -> String {
    if !body.contains(r#""prompt_cache_breakpoint""#) {
        return body.to_owned();
    }
    let input = gjson::get(body, "input");
    let items = input.array();
    if input.kind() != Kind::Array || items.is_empty() {
        return body.to_owned();
    }
    let mut changed = false;
    let mut rebuilt = Vec::with_capacity(items.len());
    for item in &items {
        let mut raw = item.json().to_owned();
        for path in ["content", "output"] {
            let parts = item.get(path);
            if parts.kind() != Kind::Array {
                continue;
            }
            let Some(stripped) = strip_breakpoint_parts(&parts) else {
                continue;
            };
            if let Ok(updated) = set_raw(&raw, path, &stripped) {
                raw = updated;
                changed = true;
            }
        }
        if item.get("prompt_cache_breakpoint").exists() {
            raw = delete(&raw, "prompt_cache_breakpoint");
            changed = true;
        }
        rebuilt.push(raw);
    }
    if !changed {
        return body.to_owned();
    }
    set_raw(body, "input", &join_array(&rebuilt)).unwrap_or_else(|_| body.to_owned())
}

fn strip_breakpoint_parts(parts: &gjson::Value<'_>) -> Option<String> {
    let parts = parts.array();
    if !parts.iter().any(|p| p.get("prompt_cache_breakpoint").exists()) {
        return None;
    }
    let rebuilt: Vec<String> = parts
        .iter()
        .map(|part| {
            let raw = part.json().to_owned();
            if part.get("prompt_cache_breakpoint").exists() {
                delete(&raw, "prompt_cache_breakpoint")
            } else {
                raw
            }
        })
        .collect();
    Some(join_array(&rebuilt))
}

fn system_role_to_developer(body: &str) -> String {
    let input = gjson::get(body, "input");
    let items = input.array();
    if input.kind() != Kind::Array || items.is_empty() {
        return body.to_owned();
    }
    let is_system = |item: &gjson::Value<'_>| item.kind() == Kind::Object && item.get("role").str() == "system";
    if !items.iter().any(is_system) {
        return body.to_owned();
    }
    let mut rebuilt = Vec::with_capacity(items.len());
    for item in &items {
        let mut raw = item.json().to_owned();
        if is_system(item) {
            match set_raw(&raw, "role", r#""developer""#) {
                Ok(updated) => raw = updated,
                Err(_) => return body.to_owned(),
            }
        }
        // json.Marshal([]json.RawMessage) compacts each item with HTML escaping.
        rebuilt.push(go_compact(&raw));
    }
    set_raw(body, "input", &join_array(&rebuilt)).unwrap_or_else(|_| body.to_owned())
}

/// Go `json.Compact` with HTML escaping, as `json.Marshal` applies to a RawMessage.
fn go_compact(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let (mut in_string, mut escaped) = (false, false);
    for c in raw.chars() {
        if in_string {
            if escaped {
                escaped = false;
                out.push(c);
                continue;
            }
            match c {
                '\\' => {
                    escaped = true;
                    out.push(c);
                }
                '"' => {
                    in_string = false;
                    out.push(c);
                }
                '<' => out.push_str("\\u003c"),
                '>' => out.push_str("\\u003e"),
                '&' => out.push_str("\\u0026"),
                '\u{2028}' => out.push_str("\\u2028"),
                '\u{2029}' => out.push_str("\\u2029"),
                _ => out.push(c),
            }
        } else {
            match c {
                ' ' | '\t' | '\n' | '\r' => {}
                '"' => {
                    in_string = true;
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
    }
    out
}

fn builtin_tool_type(kind: &str) -> Option<&'static str> {
    matches!(kind, "web_search_preview" | "web_search_preview_2025_03_11").then_some("web_search")
}

fn builtin_tool_array(body: &str, path: &str) -> String {
    let tools = gjson::get(body, path);
    if tools.kind() != Kind::Array {
        return body.to_owned();
    }
    let mut changed = false;
    let items: Vec<String> = tools
        .array()
        .iter()
        .map(|tool| {
            let raw = tool.json().to_owned();
            match builtin_tool_type(tool.get("type").str()) {
                Some(normalized) => match set_str(&raw, "type", normalized) {
                    Ok(updated) => {
                        changed = true;
                        updated
                    }
                    Err(_) => raw,
                },
                None => raw,
            }
        })
        .collect();
    if !changed {
        return body.to_owned();
    }
    set_raw(body, path, &join_array(&items)).unwrap_or_else(|_| body.to_owned())
}

fn builtin_tool_at(body: &str, path: &str) -> String {
    match builtin_tool_type(&gstr(&gjson::get(body, path))) {
        Some(normalized) => set_str(body, path, normalized).unwrap_or_else(|_| body.to_owned()),
        None => body.to_owned(),
    }
}

fn empty_function_call_arguments(body: &str) -> String {
    let input = gjson::get(body, "input");
    let items = input.array();
    if input.kind() != Kind::Array || items.is_empty() {
        return body.to_owned();
    }
    let mut changed = false;
    let rebuilt: Vec<String> = items
        .iter()
        .map(|item| {
            let raw = item.json().to_owned();
            let args = item.get("arguments");
            if item.kind() == Kind::Object
                && item.get("type").str() == "function_call"
                && args.kind() == Kind::String
                && args.str().trim().is_empty()
                && let Ok(updated) = set_str(&raw, "arguments", "{}")
            {
                changed = true;
                return updated;
            }
            raw
        })
        .collect();
    if !changed {
        return body.to_owned();
    }
    set_raw(body, "input", &join_array(&rebuilt)).unwrap_or_else(|_| body.to_owned())
}

/// `translatorcommon.RequestModelName`.
fn request_model_name(original: &str, translated: &str) -> String {
    for raw in [original, translated] {
        if raw.is_empty() || !valid(raw) {
            continue;
        }
        for path in ["model", "request.model"] {
            let model = gjson::get(raw, path);
            if model.kind() == Kind::String && !model.str().trim().is_empty() {
                return model.str().to_owned();
            }
        }
    }
    String::new()
}

/// `ConvertCodexResponseToOpenAIResponses` (no apply_patch bridge): one scanned line in,
/// the same line out, with `response.model` filled on created/in_progress events.
pub(crate) struct CodexToResponses {
    model: String,
}

impl CodexToResponses {
    /// `RequestModelName(original, translated)`, else the client model.
    pub(crate) fn new(model: &str, original: &[u8], translated: &[u8]) -> Self {
        let mut name = request_model_name(
            std::str::from_utf8(original).unwrap_or_default(),
            std::str::from_utf8(translated).unwrap_or_default(),
        );
        if name.is_empty() {
            name = model.to_owned();
        }
        Self { model: name }
    }

    fn line(&self, line: &[u8]) -> Vec<u8> {
        let sse = line.starts_with(b"data:");
        let raw = if sse { go_trim_space(&line[5..]) } else { line };
        let Ok(text) = std::str::from_utf8(raw) else {
            return line.to_vec();
        };
        let kind = gjson::get(text, "type");
        let kind = kind.str();
        if (kind != "response.created" && kind != "response.in_progress")
            || gjson::get(text, "response.model").exists()
            || self.model.is_empty()
        {
            return line.to_vec();
        }
        match set_str(text, "response.model", &self.model) {
            Ok(updated) if updated != text => {
                if sse {
                    format!("data: {updated}").into_bytes()
                } else {
                    updated.into_bytes()
                }
            }
            _ => line.to_vec(),
        }
    }
}

impl StreamTranslator for CodexToResponses {
    fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, Error> {
        Ok(vec![Bytes::from(self.line(event))])
    }

    fn finish(&mut self) -> Result<Vec<Bytes>, Error> {
        Ok(Vec::new())
    }
}

/// `ConvertCodexResponseToOpenAIResponsesNonStream`: the `response` of a completed event,
/// a bare response object unchanged, anything else empty.
pub(crate) fn codex_to_responses_non_stream(completed: &str) -> String {
    let kind = gjson::get(completed, "type");
    if kind.str().is_empty() && gjson::get(completed, "output").kind() == Kind::Array {
        return completed.to_owned();
    }
    if !matches!(kind.str(), "response.completed" | "response.incomplete") {
        return String::new();
    }
    gjson::get(completed, "response").json().to_owned()
}

/// Go `bytes.TrimSpace`: ASCII space, `\v` and Unicode White_Space at both ends.
pub(crate) fn go_trim_space(b: &[u8]) -> &[u8] {
    if let Ok(text) = std::str::from_utf8(b) {
        return text.trim().as_bytes();
    }
    let space = |c: &u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let start = b.iter().position(|c| !space(c)).unwrap_or(b.len());
    let end = b.iter().rposition(|c| !space(c)).map_or(start, |e| e + 1);
    &b[start..end]
}

// ---------------------------------------------------------------------------------------
// ponytail: adapters, owner the Codex thread (codex_executor_request.go,
// codex_executor_terminal.go, codex_executor_tokens.go, openai_responses_signature.go).
// ---------------------------------------------------------------------------------------

/// `normalizeCodexInstructions`: a missing or null `instructions` becomes "".
pub(crate) fn normalize_codex_instructions(body: &str) -> String {
    let instructions = gjson::get(body, "instructions");
    if !instructions.exists() || instructions.kind() == Kind::Null {
        return set_str(body, "instructions", "").unwrap_or_else(|_| body.to_owned());
    }
    body.to_owned()
}

/// `sanitizeOpenAIResponsesReasoningEncryptedContent`. Identity until the Codex thread's
/// port lands: Go also strips orphan reasoning ids and clears reasoning content in input.
pub(crate) fn sanitize_reasoning_encrypted_content(body: String) -> String {
    body
}

/// Output items collected from `response.output_item.done` events.
#[derive(Default)]
pub(crate) struct OutputItems {
    by_index: BTreeMap<i64, String>,
    fallback: Vec<String>,
}

impl OutputItems {
    /// `xaiCollectOutputItemDone` / `collectCodexOutputItemDone`.
    pub(crate) fn collect(&mut self, event: &str) {
        let item = gjson::get(event, "item");
        if !matches!(item.kind(), Kind::Object | Kind::Array) {
            return;
        }
        let index = gjson::get(event, "output_index");
        if index.exists() {
            self.by_index.insert(index.i64(), item.json().to_owned());
        } else {
            self.fallback.push(item.json().to_owned());
        }
    }

    /// `patchCodexCompletedOutput`: rebuild an empty `response.output` from the collected
    /// items, or hydrate missing item ids from them.
    pub(crate) fn patch_completed(&self, event: &str) -> String {
        let output = gjson::get(event, "response.output");
        let items = output.array();
        if output.kind() == Kind::Array && !items.is_empty() {
            return self.hydrate_ids(event, &items);
        }
        if self.by_index.is_empty() && self.fallback.is_empty() {
            return event.to_owned();
        }
        let all: Vec<String> = self.by_index.values().chain(&self.fallback).cloned().collect();
        set_raw(event, "response.output", &join_array(&all)).unwrap_or_else(|_| event.to_owned())
    }

    fn hydrate_ids(&self, event: &str, items: &[gjson::Value<'_>]) -> String {
        let mut patched = event.to_owned();
        for (index, item) in items.iter().enumerate() {
            let id = item.get("id");
            if id.exists() && id.kind() != Kind::Null && (id.kind() != Kind::String || !id.str().trim().is_empty()) {
                continue;
            }
            let Some(done) = self.by_index.get(&(index as i64)) else {
                continue;
            };
            let done_id = gjson::get(done, "id");
            if done_id.kind() != Kind::String || done_id.str().trim().is_empty() {
                continue;
            }
            if let Ok(updated) = set_raw(&patched, &format!("response.output.{index}.id"), done_id.json()) {
                patched = updated;
            }
        }
        patched
    }
}

/// `countCodexInputTokens` with the O200kBase encoder.
pub(crate) fn count_codex_input_tokens(body: &str) -> Result<i64, String> {
    if body.is_empty() {
        return Ok(0);
    }
    let mut segments: Vec<String> = Vec::new();
    let mut push = |s: String| {
        let s = s.trim();
        if !s.is_empty() {
            segments.push(s.to_owned());
        }
    };
    let root = gjson::parse(body);
    push(gstr(&root.get("instructions")));
    let input = root.get("input");
    if input.kind() == Kind::Array {
        for item in input.array() {
            match item.get("type").str() {
                "message" => {
                    let content = item.get("content");
                    if content.kind() == Kind::Array {
                        for part in content.array() {
                            push(gstr(&part.get("text")));
                        }
                    }
                }
                "function_call" => {
                    push(gstr(&item.get("name")));
                    push(gstr(&item.get("arguments")));
                }
                "function_call_output" => push(gstr(&item.get("output"))),
                _ => push(gstr(&item.get("text"))),
            }
        }
    }
    let raw_or_string = |v: &gjson::Value<'_>| {
        if v.kind() == Kind::String {
            v.str().to_owned()
        } else {
            v.json().to_owned()
        }
    };
    let tools = root.get("tools");
    if tools.kind() == Kind::Array {
        for tool in tools.array() {
            push(gstr(&tool.get("name")));
            push(gstr(&tool.get("description")));
            let params = tool.get("parameters");
            if params.exists() {
                push(raw_or_string(&params));
            }
        }
    }
    let format = root.get("text.format");
    if format.exists() {
        push(gstr(&format.get("name")));
        let schema = format.get("schema");
        if schema.exists() {
            push(raw_or_string(&schema));
        }
    }
    let text = segments.join("\n");
    if text.is_empty() {
        return Ok(0);
    }
    static ENCODER: std::sync::OnceLock<Result<tiktoken_rs::CoreBPE, String>> = std::sync::OnceLock::new();
    let encoder = ENCODER
        .get_or_init(|| tiktoken_rs::o200k_base().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(Clone::clone)?;
    Ok(encoder.encode_ordinary(&text).len() as i64)
}

// ---------------------------------------------------------------------------------------
// ponytail: adapters, owner the server thread (cpa-common::payload; Go helps/).
// ---------------------------------------------------------------------------------------

/// `NormalizeCodexToolIntegerTypes`. Identity until cpa-common::payload lands: Go only
/// rewrites tool schemas for Codex CLI User-Agents.
pub(crate) fn normalize_codex_tool_integer_types(body: String, _headers: &HeaderMap) -> String {
    body
}

/// `EnsureResponsesUsageDetails`: Responses usage objects always carry
/// `output_tokens_details.reasoning_tokens` and `input_tokens_details.cached_tokens`.
pub(crate) fn ensure_responses_usage_details(payload: &[u8]) -> Vec<u8> {
    let trimmed = go_trim_space(payload);
    if trimmed.is_empty() {
        return payload.to_vec();
    }
    if trimmed[0] == b'{' {
        let Ok(text) = std::str::from_utf8(trimmed) else {
            return payload.to_vec();
        };
        if gjson::get(text, "object").str() == "response.compaction" {
            return payload.to_vec();
        }
        let updated = usage_details_at(&usage_details_at(text, "response.usage"), "usage");
        return if updated == text {
            payload.to_vec()
        } else {
            updated.into_bytes()
        };
    }
    if !payload.windows(5).any(|w| w == b"data:") {
        return payload.to_vec();
    }
    let mut lines: Vec<Vec<u8>> = payload.split(|b| *b == b'\n').map(<[u8]>::to_vec).collect();
    let mut modified = false;
    for line in &mut lines {
        if !go_trim_space(line).starts_with(b"data:") {
            continue;
        }
        let prefix = if line.starts_with(b"data: ") { 6 } else { 5 };
        let data = go_trim_space(&line[prefix.min(line.len())..]);
        if data.first() != Some(&b'{') {
            continue;
        }
        let Ok(text) = std::str::from_utf8(data) else {
            continue;
        };
        if gjson::get(text, "object").str() == "response.compaction" {
            continue;
        }
        let updated = usage_details_at(&usage_details_at(text, "response.usage"), "usage");
        if updated != text {
            let mut rebuilt = line[..prefix.min(line.len())].to_vec();
            rebuilt.extend_from_slice(updated.as_bytes());
            *line = rebuilt;
            modified = true;
        }
    }
    if modified { lines.join(&b'\n') } else { payload.to_vec() }
}

fn usage_details_at(json: &str, path: &str) -> String {
    let usage = gjson::get(json, path);
    if usage.kind() != Kind::Object {
        return json.to_owned();
    }
    let mut json = json.to_owned();
    for (details, field) in [
        ("output_tokens_details", "reasoning_tokens"),
        ("input_tokens_details", "cached_tokens"),
    ] {
        let node = usage.get(details);
        let edit = if !node.exists() {
            set_raw(&json, &format!("{path}.{details}.{field}"), "0")
        } else if node.kind() != Kind::Object {
            set_raw(&json, &format!("{path}.{details}"), &format!(r#"{{"{field}":0}}"#))
        } else {
            let value = node.get(field);
            if value.exists() && value.kind() != Kind::Null {
                continue;
            }
            set_raw(&json, &format!("{path}.{details}.{field}"), "0")
        };
        if let Ok(updated) = edit {
            json = updated;
        }
    }
    json
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_escapes_html_and_drops_whitespace() {
        assert_eq!(
            go_compact("{ \"a\" : \"<b> & \\\"c\\\"\",\n \"d\": [1, 2] }"),
            r#"{"a":"\u003cb\u003e \u0026 \"c\"","d":[1,2]}"#
        );
    }

    #[test]
    fn usage_details_fill_both_shapes() {
        let out = ensure_responses_usage_details(br#"{"usage":{"input_tokens":1,"output_tokens_details":null}}"#);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"usage":{"input_tokens":1,"output_tokens_details":{"reasoning_tokens":0},"input_tokens_details":{"cached_tokens":0}}}"#
        );
        let sse = b"event: x\ndata: {\"response\":{\"usage\":{\"input_tokens_details\":{\"cached_tokens\":3}}}}";
        assert_eq!(
            String::from_utf8(ensure_responses_usage_details(sse)).unwrap(),
            "event: x\ndata: {\"response\":{\"usage\":{\"input_tokens_details\":{\"cached_tokens\":3},\"output_tokens_details\":{\"reasoning_tokens\":0}}}}"
        );
    }
}
