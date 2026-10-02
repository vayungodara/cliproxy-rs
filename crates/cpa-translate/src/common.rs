//! Shared translator helpers: internal/translator/common and the translator-facing parts
//! of internal/util. Strings are bytes, as in Go.

use cpa_common::json::{self as gj, Kind, Res};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// strings.TrimSpace on bytes: Unicode whitespace from both ends; invalid UTF-8 is not
/// whitespace.
pub fn trim_space(s: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < s.len() {
        match gj::decode_rune(&s[start..]) {
            (Some(c), n) if c.is_whitespace() => start += n,
            _ => break,
        }
    }
    let mut end = s.len();
    while end > start {
        let tail = &s[start..end];
        let from = tail.len().saturating_sub(4);
        let Some(i) = (from..tail.len()).rev().find(|&i| tail[i] & 0xC0 != 0x80) else {
            break;
        };
        match gj::decode_rune(&tail[i..]) {
            (Some(c), n) if i + n == tail.len() && c.is_whitespace() => end = start + i,
            _ => break,
        }
    }
    &s[start..end]
}

/// Trimmed, lowercased text for comparisons (strings.ToLower(strings.TrimSpace(x))).
pub fn norm(r: &Res<'_>) -> String {
    r.str().trim().to_lowercase()
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub fn now_nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}

// ---------------------------------------------------------------------------------------
// Claude user ID (common/claude_user_id.go)

pub fn derive_claude_user_id(raw: &[u8]) -> Vec<u8> {
    let root = gj::parse(raw);
    for path in ["metadata.user_id", "user"] {
        let v = root.get(path);
        if v.kind == Kind::String && !trim_space(&v.s).is_empty() {
            return v.s.to_vec();
        }
    }
    let mut seed: Vec<u8> = vec![];
    let write = |seed: &mut Vec<u8>, prefix: &str, value: &[u8]| {
        seed.extend_from_slice(prefix.as_bytes());
        seed.extend_from_slice(value);
    };
    let v = root.get("prompt_cache_key");
    if v.exists() && !trim_space(&v.bytes()).is_empty() {
        write(&mut seed, "prompt_cache_key:", trim_space(&v.bytes()));
    }
    if seed.is_empty() {
        for path in ["session_id", "sessionId"] {
            let v = root.get(path);
            if v.exists() && !trim_space(&v.bytes()).is_empty() {
                write(&mut seed, "session_id:", trim_space(&v.bytes()));
                break;
            }
        }
    }
    if seed.is_empty() {
        let conversation = root.get("conversation");
        let id = conversation.get("id").bytes();
        if !trim_space(&id).is_empty() {
            write(&mut seed, "conversation_id:", trim_space(&id));
        } else if conversation.kind == Kind::String {
            let text = conversation.bytes();
            if !trim_space(&text).is_empty() {
                write(&mut seed, "conversation_id:", trim_space(&text));
            }
        } else {
            let v = root.get("conversation_id");
            if v.exists() && !trim_space(&v.bytes()).is_empty() {
                write(&mut seed, "conversation_id:", trim_space(&v.bytes()));
            }
        }
    }
    if seed.is_empty() {
        let content = first_stable_request_content(&root);
        if !content.is_empty() {
            write(&mut seed, "content:", &content);
        }
    }
    if seed.is_empty() {
        let v = root.get("model");
        if v.exists() && !trim_space(&v.bytes()).is_empty() {
            write(&mut seed, "model:", trim_space(&v.bytes()));
        }
        for path in ["instructions", "system", "systemInstruction", "system_instruction"] {
            let v = root.get(path);
            if v.exists() {
                write(&mut seed, &format!(";{path}:"), &v.bytes());
            }
        }
    }
    if seed.is_empty() {
        return b"unknown".to_vec();
    }
    hex(&Sha256::digest(&seed)).into_bytes()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn joined_texts(content: &Res<'_>, accept: impl Fn(&Res<'_>) -> bool) -> Vec<u8> {
    if content.kind == Kind::String {
        return trim_space(&content.bytes()).to_vec();
    }
    if !content.is_array() {
        return vec![];
    }
    let mut texts: Vec<Vec<u8>> = vec![];
    content.each(|_, part| {
        if accept(&part) {
            let text = part.get("text");
            if text.exists() {
                let value = trim_space(&text.bytes()).to_vec();
                if !value.is_empty() {
                    texts.push(value);
                }
            }
        }
        true
    });
    trim_space(&texts.join(&b'\n')).to_vec()
}

fn first_stable_request_content(root: &Res<'_>) -> Vec<u8> {
    let messages = root.get("messages");
    if messages.is_array() {
        let mut content = vec![];
        messages.each(|_, message| {
            if norm(&message.get("role")) == "user" {
                content = joined_texts(&message.get("content"), |p| p.get("type").str() == "text");
                if !content.is_empty() {
                    return false;
                }
            }
            true
        });
        if !content.is_empty() {
            return content;
        }
    }
    let input = root.get("input");
    if input.exists() {
        if input.kind == Kind::String {
            let text = trim_space(&input.bytes()).to_vec();
            if !text.is_empty() {
                return text;
            }
        } else if input.is_array() {
            let mut content = vec![];
            input.each(|_, item| {
                if is_responses_user_item(&item) {
                    content = joined_texts(&item.get("content"), |p| {
                        matches!(&*p.get("type").str(), "input_text" | "output_text" | "text")
                    });
                    if !content.is_empty() {
                        return false;
                    }
                }
                true
            });
            if !content.is_empty() {
                return content;
            }
        }
    }
    let contents = root.get("contents");
    if contents.is_array() {
        let mut content = vec![];
        contents.each(|_, item| {
            let role = norm(&item.get("role"));
            if role.is_empty() || role == "user" {
                let parts = item.get("parts");
                if parts.is_array() {
                    let mut texts: Vec<Vec<u8>> = vec![];
                    parts.each(|_, part| {
                        if is_gemini_thought_part(&part) {
                            return true;
                        }
                        let text = part.get("text");
                        if text.exists() {
                            let value = trim_space(&text.bytes()).to_vec();
                            if !value.is_empty() {
                                texts.push(value);
                            }
                        }
                        true
                    });
                    if !texts.is_empty() {
                        content = texts.join(&b'\n');
                        return false;
                    }
                }
            }
            true
        });
        if !content.is_empty() {
            return content;
        }
    }
    vec![]
}

fn is_responses_user_item(item: &Res<'_>) -> bool {
    match norm(&item.get("role")).as_str() {
        "user" => true,
        "system" | "developer" | "assistant" => false,
        _ => norm(&item.get("type")) == "message",
    }
}

pub fn is_gemini_thought_part(part: &Res<'_>) -> bool {
    part.get("thought").bool()
}

// ---------------------------------------------------------------------------------------
// Cache control (common/cache_control.go)

fn valid_cache_control(cc: &Res<'_>) -> bool {
    if !cc.exists() || cc.kind == Kind::Null || !cc.is_object() {
        return false;
    }
    let t = cc.get("type");
    t.kind == Kind::String && &*t.bytes() == b"ephemeral"
}

pub fn attach_cache_control(mut dst: Vec<u8>, src: &Res<'_>) -> Vec<u8> {
    let cc = src.get("cache_control");
    if valid_cache_control(&cc) {
        gj::set_raw(&mut dst, "cache_control", &cc.raw);
    }
    dst
}

pub fn attach_message_cache_control(msg: Vec<u8>, src: &Res<'_>) -> Vec<u8> {
    let cc = src.get("cache_control");
    if !valid_cache_control(&cc) {
        return msg;
    }
    let content = gj::get(&msg, "content");
    if content.is_array() {
        let arr = content.array();
        if arr.is_empty() || arr[arr.len() - 1].get("cache_control").exists() {
            return msg;
        }
        let path = format!("content.{}.cache_control", arr.len() - 1);
        let mut out = msg.clone();
        return if gj::set_raw(&mut out, &path, &cc.raw) {
            out
        } else {
            msg
        };
    }
    if content.kind != Kind::String {
        return msg;
    }
    let mut part = br#"{"type":"text","text":""}"#.to_vec();
    gj::set_str(&mut part, "text", content.bytes());
    if !gj::set_raw(&mut part, "cache_control", &cc.raw) {
        return msg;
    }
    let mut out = msg.clone();
    if !gj::set_raw(&mut out, "content", "[]") {
        return msg;
    }
    gj::set_raw(&mut out, "content.-1", part);
    out
}

pub fn attach_tool_message_cache_control(msg: Vec<u8>, src: &Res<'_>) -> Vec<u8> {
    let mut raw_cc = first_part_cache_control(src);
    if raw_cc.is_empty() {
        let cc = src.get("cache_control");
        if valid_cache_control(&cc) {
            raw_cc = cc.raw.to_vec();
        }
    }
    if raw_cc.is_empty() {
        return msg;
    }
    let content = gj::get(&msg, "content");
    if content.is_array() {
        let arr = content.array();
        let Some(target) = arr.iter().position(|b| b.get("type").str() == "tool_result") else {
            return msg;
        };
        let mut out = msg.clone();
        if gj::set_raw(&mut out, &format!("content.{target}.cache_control"), &raw_cc) {
            return out;
        }
    }
    msg
}

fn first_part_cache_control(src: &Res<'_>) -> Vec<u8> {
    let mut content = src.get("content");
    if !content.exists() {
        content = src.clone();
    }
    if content.is_array() {
        let mut raw = vec![];
        content.each(|_, part| {
            let cc = part.get("cache_control");
            if valid_cache_control(&cc) {
                raw = cc.raw.to_vec();
                return false;
            }
            true
        });
        return raw;
    }
    if content.is_object() {
        let cc = content.get("cache_control");
        if valid_cache_control(&cc) {
            return cc.raw.to_vec();
        }
    }
    vec![]
}

// ---------------------------------------------------------------------------------------
// Claude messages (common/claude_messages.go)

/// Merges consecutive same-role Claude messages; assistant tool_use parts move after the
/// other parts of the merged turn.
#[derive(Default)]
pub struct ClaudeMessages {
    messages: Vec<Vec<u8>>,
    role: Vec<u8>,
    content: Vec<Vec<u8>>,
    tool_use: Vec<Vec<u8>>,
}

impl ClaudeMessages {
    pub fn append(&mut self, message: &[u8]) {
        if message.is_empty() {
            return;
        }
        let root = gj::parse(message);
        let role = root.get("role").bytes().into_owned();
        if role != b"user" && role != b"assistant" {
            return;
        }
        let parts = content_parts(&root.get("content"));
        if parts.is_empty() {
            return;
        }
        if !self.role.is_empty() && self.role != role {
            self.flush();
        }
        for part in parts {
            if role == b"assistant" && gj::get(&part, "type").str() == "tool_use" {
                self.tool_use.push(part);
            } else {
                self.content.push(part);
            }
        }
        self.role = role;
    }

    pub fn flush(&mut self) {
        if self.role.is_empty() {
            return;
        }
        let mut parts = std::mem::take(&mut self.content);
        parts.append(&mut self.tool_use);
        if !parts.is_empty() {
            let mut message = br#"{"role":"","content":[]}"#.to_vec();
            gj::set_str(&mut message, "role", &self.role);
            gj::set_raw(&mut message, "content", gj::join(&parts));
            self.messages.push(message);
        }
        self.role.clear();
    }

    pub fn messages(mut self) -> Vec<Vec<u8>> {
        self.flush();
        self.messages
    }
}

fn content_parts(content: &Res<'_>) -> Vec<Vec<u8>> {
    if !content.exists() || content.kind == Kind::Null {
        return vec![];
    }
    if content.kind == Kind::String {
        if content.s.is_empty() {
            return vec![];
        }
        let mut part = br#"{"type":"text","text":""}"#.to_vec();
        gj::set_str(&mut part, "text", content.bytes());
        return vec![part];
    }
    if !content.is_array() {
        return vec![];
    }
    let mut parts = vec![];
    content.each(|_, part| {
        if part.is_object() {
            parts.push(part.raw.to_vec());
        }
        true
    });
    parts
}

// ---------------------------------------------------------------------------------------
// Tool IDs and names (common/request.go, util/claude_tool_id.go)

const TOOLU_LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// `toolu_` plus 24 random alphanumerics (common.GenerateClaudeToolCallID).
pub fn generate_claude_tool_call_id() -> Vec<u8> {
    let mut rng = rand::rng();
    let mut out = b"toolu_".to_vec();
    out.extend((0..24).map(|_| TOOLU_LETTERS[rng.random_range(0..TOOLU_LETTERS.len())]));
    out
}

/// Replaces every rune outside `[a-zA-Z0-9_-]` (each invalid byte counts as one) with `_`.
fn sanitize(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let (c, n) = gj::decode_rune(&s[i..]);
        match c {
            Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '-' => out.push(c as u8),
            _ => out.push(b'_'),
        }
        i += n;
    }
    out
}

static TOOL_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn sanitize_claude_tool_id(id: &[u8]) -> Vec<u8> {
    let s = sanitize(id);
    if s.is_empty() {
        let n = TOOL_ID_COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        return format!("toolu_{}_{n}", now_nanos()).into_bytes();
    }
    s
}

pub fn sanitize_claude_function_name(name: &[u8]) -> Vec<u8> {
    if name.is_empty() {
        return vec![];
    }
    let mut s = sanitize(name);
    s.truncate(64);
    if s.is_empty() {
        s.push(b'_');
    }
    s
}

// ---------------------------------------------------------------------------------------
// Claude tool schema (util/claude_schema.go)

const EMPTY_SCHEMA: &[u8] = br#"{"type":"object","properties":{}}"#;

/// `json.Unmarshal(raw, &map[string]json.RawMessage)`; `None` for errors and `null`.
fn raw_object(raw: &[u8]) -> Option<BTreeMap<String, Vec<u8>>> {
    if !gj::valid(raw) {
        return None;
    }
    let root = gj::parse(raw);
    if !root.is_object() {
        return None;
    }
    let mut map = BTreeMap::new();
    root.each(|k, v| {
        map.insert(gj::go_unquote(&k.raw).unwrap_or_default(), v.raw.to_vec());
        true
    });
    Some(map)
}

/// `json.Unmarshal(raw, &[]string)`.
fn raw_strings(raw: &[u8]) -> Option<Vec<String>> {
    if !gj::valid(raw) {
        return None;
    }
    let root = gj::parse(raw);
    match root.kind {
        Kind::Null => Some(vec![]),
        Kind::Json if root.is_array() => root
            .array()
            .iter()
            .map(|v| match v.kind {
                Kind::String => gj::go_unquote(&v.raw),
                Kind::Null => Some(String::new()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

fn marshal_raw_map(map: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let mut out = vec![b'{'];
    for (i, (k, v)) in map.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        gj::marshal_str(&mut out, k.as_bytes(), true);
        out.push(b':');
        out.extend(gj::compact(v, true));
    }
    out.push(b'}');
    out
}

/// util.NormalizeClaudeToolInputSchema: always an object schema, with union branch
/// properties (and allOf requirements) folded into the root. Go re-marshals through maps,
/// so keys come out sorted and values compacted and HTML-escaped.
pub fn normalize_claude_tool_input_schema(schema: Option<&[u8]>) -> Vec<u8> {
    let Some(mut root) = schema.filter(|s| !s.is_empty()).and_then(raw_object) else {
        return EMPTY_SCHEMA.to_vec();
    };
    let object = |raw: Option<&Vec<u8>>| raw.and_then(|r| raw_object(r)).unwrap_or_default();
    let mut properties = object(root.get("properties"));
    for union in ["anyOf", "oneOf", "allOf"] {
        let Some(raw) = root.remove(union) else {
            continue;
        };
        let branches = if gj::valid(&raw) && gj::parse(&raw).is_array() {
            gj::parse(&raw).array().iter().map(|b| b.raw.to_vec()).collect()
        } else if gj::valid(&raw) && gj::parse(&raw).kind == Kind::Null {
            vec![]
        } else {
            continue;
        };
        for branch in branches {
            let Some(branch) = raw_object(&branch) else {
                continue;
            };
            if !schema_can_be_object(&branch) {
                continue;
            }
            for (name, property) in object(branch.get("properties")) {
                properties.entry(name).or_insert(property);
            }
            if union == "allOf" {
                merge_required(&mut root, branch.get("required"));
            }
        }
    }
    root.insert("type".into(), br#""object""#.to_vec());
    root.insert("properties".into(), marshal_raw_map(&properties));
    marshal_raw_map(&root)
}

fn schema_can_be_object(schema: &BTreeMap<String, Vec<u8>>) -> bool {
    let Some(raw) = schema.get("type") else {
        return true;
    };
    if gj::valid(raw) {
        let value = gj::parse(raw);
        match value.kind {
            Kind::String => return &*value.s == b"object",
            Kind::Null => return false,
            _ => {}
        }
    }
    raw_strings(raw).is_some_and(|types| types.iter().any(|t| t == "object"))
}

fn merge_required(root: &mut BTreeMap<String, Vec<u8>>, branch: Option<&Vec<u8>>) {
    let mut required = root.get("required").and_then(|r| raw_strings(r)).unwrap_or_default();
    let Some(names) = branch.and_then(|b| raw_strings(b)) else {
        return;
    };
    for name in names {
        if !required.contains(&name) {
            required.push(name);
        }
    }
    if !required.is_empty() {
        root.insert("required".into(), gj::quote_all(&required));
    }
}

// ---------------------------------------------------------------------------------------
// Structured output (common/claude_system.go)

const JSON_OBJECT_INSTRUCTION: &str = "You must format your entire response as a valid JSON object. Do not include any explanations, markdown code blocks (such as ```json), or any text outside of the JSON object.";

pub fn claude_structured_output_instruction(format: &Res<'_>) -> Vec<u8> {
    if !format.exists() {
        return vec![];
    }
    match norm(&format.get("type")).as_str() {
        "json_object" => JSON_OBJECT_INSTRUCTION.into(),
        "json_schema" => {
            let json_schema = format.get("json_schema");
            let mut schema = json_schema.get("schema");
            if !schema.exists() {
                schema = format.get("schema");
            }
            if !schema.exists() {
                return JSON_OBJECT_INSTRUCTION.into();
            }
            let mut out = b"You must format your entire response as valid JSON that conforms strictly to the following JSON schema:\n".to_vec();
            for (key, label) in [("name", "Schema Name: "), ("description", "Schema Description: ")] {
                let mut value = trim_space(&json_schema.get(key).bytes()).to_vec();
                if value.is_empty() {
                    value = trim_space(&format.get(key).bytes()).to_vec();
                }
                if !value.is_empty() {
                    out.extend_from_slice(label.as_bytes());
                    out.extend_from_slice(&value);
                    out.push(b'\n');
                }
            }
            out.extend_from_slice(b"JSON Schema:\n");
            out.extend_from_slice(&schema.raw);
            out.extend_from_slice(b"\nDo not include any explanations, markdown code blocks (such as ```json), or any text outside of the JSON object.");
            out
        }
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------------------
// Request model (common/request.go)

/// `event: <event>\ndata: <payload>\n\n` (common.SSEEventData).
pub fn sse_event(event: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(event.len() + payload.len() + 16);
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\n\n");
    out
}

/// common.RequestModelName: the first non-blank `model` or `request.model` string.
pub fn request_model_name(original: &[u8], request: &[u8]) -> Vec<u8> {
    for raw in [original, request] {
        if raw.is_empty() || !gj::valid(raw) {
            continue;
        }
        let root = gj::parse(raw);
        for path in ["model", "request.model"] {
            let model = root.get(path);
            if model.kind == Kind::String && !trim_space(&model.s).is_empty() {
                return model.s.to_vec();
            }
        }
    }
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_space_follows_go_unicode_rules() {
        assert_eq!(trim_space(" \u{a0}x\u{2003}\n".as_bytes()), b"x");
        // Invalid bytes are not whitespace and stop trimming.
        assert_eq!(trim_space(b" \xff "), b"\xff");
        assert_eq!(trim_space(b"   "), b"");
    }

    #[test]
    fn sanitize_counts_runes_not_bytes() {
        assert_eq!(sanitize_claude_function_name("é.b\u{ff}".as_bytes()), b"__b_");
        assert_eq!(sanitize_claude_function_name(b"\xff\xfe"), b"__");
        assert_eq!(sanitize_claude_function_name(&[b'a'; 70]).len(), 64);
        let id = sanitize_claude_tool_id(b"");
        assert!(id.starts_with(b"toolu_"));
    }
}
