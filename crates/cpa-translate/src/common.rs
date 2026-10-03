//! Shared translator helpers: internal/translator/common and the translator-facing parts
//! of internal/util. Strings are bytes, as in Go.

use cpa_common::json::{self as gj, Kind, Res};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
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

/// Replaces every rune outside `[a-zA-Z0-9_-]` (each invalid byte counts as one) with `_`
/// (also Codex's sanitizeToolName).
pub fn sanitize(s: &[u8]) -> Vec<u8> {
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

// ---------------------------------------------------------------------------------------
// Go string helpers on bytes

/// strings.TrimLeftFunc(s, unicode.IsSpace).
pub fn trim_left_space(s: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < s.len() {
        match gj::decode_rune(&s[start..]) {
            (Some(c), n) if c.is_whitespace() => start += n,
            _ => break,
        }
    }
    &s[start..]
}

/// Each rune of `s` as Go's `range` and `[]rune` see it: invalid bytes are U+FFFD, one
/// per byte.
pub fn go_runes(s: &[u8]) -> impl Iterator<Item = char> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        if i >= s.len() {
            return None;
        }
        let (c, n) = gj::decode_rune(&s[i..]);
        i += n.max(1);
        Some(c.unwrap_or(char::REPLACEMENT_CHARACTER))
    })
}

/// strings.ToLower: Go's simple case mapping, and invalid bytes become U+FFFD once any
/// rune is not ASCII.
pub fn go_lower(s: &[u8]) -> Vec<u8> {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0; 4];
    for c in go_runes(s) {
        out.extend_from_slice(cpa_common::gostr::to_lower_rune(c).encode_utf8(&mut buf).as_bytes());
    }
    out
}

/// util.IsClaudeCodeAttributionSystemText: Claude Code's billing header line.
pub fn is_claude_code_attribution_text(text: &[u8]) -> bool {
    trim_left_space(text).starts_with(b"x-anthropic-billing-header:")
}

/// util.HasUnsupportedUnicodePropertyEscape: `\p{`, `\P{` or `\0` outside an escaped
/// backslash.
pub fn has_unsupported_unicode_property_escape(pattern: &[u8]) -> bool {
    let mut i = 0;
    while i < pattern.len() {
        if pattern[i] != b'\\' {
            i += 1;
            continue;
        }
        let Some(&next) = pattern.get(i + 1) else { break };
        if matches!(next, b'p' | b'P') && pattern.get(i + 2) == Some(&b'{') {
            return true;
        }
        if next == b'0' {
            return true;
        }
        i += 2;
    }
    false
}

/// util.SchemaMapKeywords: keywords whose values map names to subschemas.
pub const SCHEMA_MAP_KEYWORDS: [&[u8]; 6] = [
    b"properties",
    b"$defs",
    b"definitions",
    b"patternProperties",
    b"dependentSchemas",
    b"dependencies",
];

/// util.SchemaValueKeywords: keywords holding one subschema or a list of them.
pub const SCHEMA_VALUE_KEYWORDS: [&[u8]; 16] = [
    b"items",
    b"prefixItems",
    b"contains",
    b"additionalProperties",
    b"propertyNames",
    b"unevaluatedProperties",
    b"unevaluatedItems",
    b"additionalItems",
    b"contentSchema",
    b"anyOf",
    b"oneOf",
    b"allOf",
    b"not",
    b"if",
    b"then",
    b"else",
];

/// util.FixJSON: converts single-quoted strings to double-quoted ones. Go walks `[]rune`,
/// so invalid bytes come out as U+FFFD.
pub fn fix_json(input: &[u8]) -> Vec<u8> {
    let runes: Vec<char> = go_runes(input).collect();
    let mut out = Vec::with_capacity(input.len());
    let mut buf = [0; 4];
    let mut put = |out: &mut Vec<u8>, c: char| out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    let (mut in_double, mut in_single, mut escaped) = (false, false, false);
    let mut i = 0;
    while i < runes.len() {
        let r = runes[i];
        if in_double {
            put(&mut out, r);
            if escaped {
                escaped = false;
            } else if r == '\\' {
                escaped = true;
            } else if r == '"' {
                in_double = false;
            }
        } else if in_single {
            if escaped {
                escaped = false;
                match r {
                    'n' | 'r' | 't' | 'b' | 'f' | '/' | '"' => {
                        out.push(b'\\');
                        put(&mut out, r);
                    }
                    '\\' => out.extend_from_slice(b"\\\\"),
                    '\'' => out.push(b'\''),
                    'u' => {
                        out.extend_from_slice(b"\\u");
                        for _ in 0..4 {
                            match runes.get(i + 1) {
                                Some(&p) if p.is_ascii_hexdigit() => {
                                    put(&mut out, p);
                                    i += 1;
                                }
                                _ => break,
                            }
                        }
                    }
                    _ => {
                        out.push(b'\\');
                        put(&mut out, r);
                    }
                }
            } else if r == '\\' {
                escaped = true;
            } else if r == '\'' {
                out.push(b'"');
                in_single = false;
            } else if r == '"' {
                out.extend_from_slice(b"\\\"");
            } else {
                put(&mut out, r);
            }
        } else if r == '"' {
            in_double = true;
            put(&mut out, r);
        } else if r == '\'' {
            in_single = true;
            out.push(b'"');
        } else {
            put(&mut out, r);
        }
        i += 1;
    }
    if in_single {
        out.push(b'"');
    }
    out
}

/// util.CanonicalToolName: trimmed, leading underscores dropped, lowercased.
fn canonical_tool_name(name: &[u8]) -> Vec<u8> {
    let trimmed = trim_space(name);
    let start = trimmed.iter().position(|&c| c != b'_').unwrap_or(trimmed.len());
    go_lower(&trimmed[start..])
}

/// util.ToolNameMapFromClaudeRequest: canonical name -> declared name for the request's
/// tools, first declaration winning. `None` when the request is invalid or names none.
pub fn tool_name_map_from_claude_request(raw: &[u8]) -> Option<HashMap<Vec<u8>, Vec<u8>>> {
    if raw.is_empty() || !gj::valid(raw) {
        return None;
    }
    let tools = gj::get(raw, "tools");
    if !tools.exists() || !tools.is_array() {
        return None;
    }
    let mut out = HashMap::new();
    tools.each(|_, tool| {
        let mut name = trim_space(&tool.get("name").bytes()).to_vec();
        if name.is_empty() {
            name = trim_space(&tool.get("function.name").bytes()).to_vec();
        }
        let key = canonical_tool_name(&name);
        if !name.is_empty() && !key.is_empty() {
            out.entry(key).or_insert(name);
        }
        true
    });
    (!out.is_empty()).then_some(out)
}

/// util.MapToolName: the declared spelling of `name`, or `name` unchanged.
pub fn map_tool_name(map: Option<&HashMap<Vec<u8>, Vec<u8>>>, name: &[u8]) -> Vec<u8> {
    if let (false, Some(map)) = (name.is_empty(), map)
        && let Some(mapped) = map.get(&canonical_tool_name(name))
        && !mapped.is_empty()
    {
        return mapped.clone();
    }
    name.to_vec()
}

/// thinking.GetThinkingText on bytes: `text`, then `thinking` as a string or an object
/// holding `text` or `thinking`.
// ponytail: cpa_common::thinking::get_thinking_text takes a UTF-8 gjson value; this keeps
// Go's byte handling for translator inputs.
pub fn thinking_text(part: &Res<'_>) -> Vec<u8> {
    let text = part.get("text");
    if text.kind == Kind::String {
        return text.s.to_vec();
    }
    let thinking = part.get("thinking");
    if thinking.kind == Kind::String {
        return thinking.s.to_vec();
    }
    if thinking.is_object() {
        for key in ["text", "thinking"] {
            let inner = thinking.get(key);
            if inner.kind == Kind::String {
                return inner.s.to_vec();
            }
        }
    }
    vec![]
}

// ---------------------------------------------------------------------------------------
// Claude system text (common/claude_system.go)

/// `<system-reminder>\n{text}\n</system-reminder>` (common.SystemReminderText).
pub fn system_reminder_text(text: &[u8]) -> Vec<u8> {
    [&b"<system-reminder>\n"[..], text, b"\n</system-reminder>"].concat()
}

/// common.ClaudeMessageSystemReminderText: a Claude `system`-role message's text parts
/// (attribution lines dropped) joined with newlines and wrapped as a reminder.
pub fn claude_message_system_reminder_text(content: &Res<'_>) -> Option<Vec<u8>> {
    let mut parts: Vec<Vec<u8>> = vec![];
    let mut keep = |text: Vec<u8>| {
        if !text.is_empty() && !is_claude_code_attribution_text(&text) {
            parts.push(text);
        }
    };
    if content.kind == Kind::String {
        keep(content.s.to_vec());
    } else if content.is_array() {
        content.each(|_, item| {
            if item.get("type").bytes().as_ref() == b"text" {
                keep(item.get("text").bytes().into_owned());
            }
            true
        });
    }
    let text = parts.join(&b'\n');
    (!parts.is_empty() && !trim_space(&text).is_empty()).then(|| system_reminder_text(&text))
}

/// common.AlignClaudeToolResults: when a user turn answers exactly the pending tool uses,
/// its tool_result parts are reordered (in their own slots) to follow the tool_use order.
pub fn align_claude_tool_results<'a>(parts: Vec<Res<'a>>, tool_use_ids: &[Vec<u8>]) -> Vec<Res<'a>> {
    if tool_use_ids.is_empty() {
        return parts;
    }
    let slots: Vec<usize> = (0..parts.len())
        .filter(|&i| parts[i].get("type").bytes().as_ref() == b"tool_result")
        .collect();
    if slots.len() != tool_use_ids.len() {
        return parts;
    }
    let mut used = vec![false; slots.len()];
    let mut order = Vec::with_capacity(slots.len());
    for id in tool_use_ids {
        let found = slots.iter().enumerate().position(|(n, &slot)| {
            !used[n] && !id.is_empty() && parts[slot].get("tool_use_id").bytes().as_ref() == id.as_slice()
        });
        let Some(n) = found else { return parts };
        used[n] = true;
        order.push(slots[n]);
    }
    let mut out = parts.clone();
    for (slot, from) in slots.iter().zip(order) {
        out[*slot] = parts[from].clone();
    }
    out
}

// ---------------------------------------------------------------------------------------
// OpenAI tool-call ordering (common/openai_tools.go)

/// common.AlignOpenAIToolCallMessages: moves each `tool` reply directly after the
/// assistant message that made the call, when every call ID of that assistant message is
/// unambiguous and answered exactly once later on.
pub fn align_openai_tool_call_messages(messages: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    align_openai_tool_call_messages_with(messages, &[])
}

/// AlignOpenAIToolCallMessages with extra call IDs treated as ambiguous (trimmed).
pub fn align_openai_tool_call_messages_with(messages: Vec<Vec<u8>>, extra_ambiguous: &[Vec<u8>]) -> Vec<Vec<u8>> {
    if messages.len() <= 1 {
        return messages;
    }
    struct Assistant {
        index: usize,
        call_ids: Vec<Vec<u8>>,
        has_empty: bool,
    }
    let mut assistants = vec![];
    let mut assistant_by_call: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut ambiguous: HashSet<Vec<u8>> = extra_ambiguous
        .iter()
        .map(|id| trim_space(id).to_vec())
        .filter(|id| !id.is_empty())
        .collect();
    let mut tools_by_call: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    for (i, raw) in messages.iter().enumerate() {
        match gj::get(raw, "role").bytes().as_ref() {
            b"assistant" => {
                let calls = gj::get(raw, "tool_calls");
                if !calls.is_array() {
                    continue;
                }
                let calls = calls.array();
                if calls.is_empty() {
                    continue;
                }
                let mut call_ids = vec![];
                let mut has_empty = false;
                for call in &calls {
                    let id = call.get("id").bytes().into_owned();
                    if id.is_empty() {
                        ambiguous.insert(vec![]);
                        has_empty = true;
                        continue;
                    }
                    if assistant_by_call.insert(id.clone(), i).is_some() {
                        ambiguous.insert(id.clone());
                    }
                    call_ids.push(id);
                }
                if !call_ids.is_empty() || has_empty {
                    assistants.push(Assistant {
                        index: i,
                        call_ids,
                        has_empty,
                    });
                }
            }
            b"tool" => {
                let id = gj::get(raw, "tool_call_id").bytes().into_owned();
                if id.is_empty() {
                    ambiguous.insert(vec![]);
                } else {
                    let indexes = tools_by_call.entry(id.clone()).or_default();
                    indexes.push(i);
                    if indexes.len() > 1 {
                        ambiguous.insert(id);
                    }
                }
            }
            _ => {}
        }
    }
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    'assistants: for a in &assistants {
        if a.has_empty {
            continue;
        }
        let mut matched = vec![];
        for id in &a.call_ids {
            match tools_by_call.get(id).map(Vec::as_slice) {
                Some(&[tool]) if !ambiguous.contains(id) && tool > a.index => matched.push(tool),
                _ => continue 'assistants,
            }
        }
        matched.sort_unstable();
        if matched
            .iter()
            .enumerate()
            .any(|(offset, &tool)| tool != a.index + offset + 1)
        {
            groups.insert(a.index, matched);
        }
    }
    if groups.is_empty() {
        return messages;
    }
    let moved: HashSet<usize> = groups.values().flatten().copied().collect();
    let mut out = Vec::with_capacity(messages.len());
    for (i, message) in messages.iter().enumerate() {
        if moved.contains(&i) {
            continue;
        }
        out.push(message.clone());
        if let Some(tools) = groups.get(&i) {
            out.extend(tools.iter().map(|&t| messages[t].clone()));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// Function names and file data (util/util.go, common/file_data.go)

/// util.SanitizeFunctionName: runes outside `[a-zA-Z0-9_.:-]` (each invalid byte counts
/// as one) become `_`, a leading non-letter gets a `_` prefix, at most 64 bytes.
pub fn sanitize_function_name(name: &[u8]) -> Vec<u8> {
    if name.is_empty() {
        return vec![];
    }
    let mut s: Vec<u8> = go_runes(name)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-') {
                c as u8
            } else {
                b'_'
            }
        })
        .collect();
    if !(s[0].is_ascii_alphabetic() || s[0] == b'_') {
        s.truncate(63);
        s.insert(0, b'_');
    }
    s.truncate(64);
    s
}

/// filepath.Ext: the suffix from the last dot of the final path element.
pub(crate) fn file_ext(name: &[u8]) -> &[u8] {
    for i in (0..name.len()).rev() {
        match name[i] {
            b'/' => break,
            b'.' => return &name[i..],
            _ => {}
        }
    }
    b""
}

/// common.NormalizeOpenAIFileData: the MIME type and base64 payload of OpenAI file
/// content, from a `data:` URL or (for raw base64) the filename's extension.
pub fn normalize_openai_file_data(filename: &[u8], fallback: &[u8], data: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if data.is_empty() {
        return None;
    }
    let mut fallback = fallback.to_vec();
    if fallback.is_empty() {
        let ext = file_ext(filename);
        let ext = go_lower(ext.strip_prefix(b".").unwrap_or(ext));
        fallback = crate::mime::mime_type(&ext).unwrap_or_default().as_bytes().to_vec();
    }
    if data.len() < 5 || !data[..5].eq_ignore_ascii_case(b"data:") {
        return (!fallback.is_empty()).then(|| (fallback, data.to_vec()));
    }
    let rest = &data[5..];
    let comma = rest.iter().position(|&c| c == b',')?;
    let (metadata, payload) = (&rest[..comma], &rest[comma + 1..]);
    if payload.is_empty() {
        return None;
    }
    let mut fields = metadata.split(|&c| c == b';');
    let mime = trim_space(fields.next().unwrap_or_default());
    if mime.is_empty() {
        return None;
    }
    use cpa_common::gostr::GoStr;
    fields
        .any(|f| String::from_utf8_lossy(trim_space(f)).go_eq_fold("base64"))
        .then(|| (mime.to_vec(), payload.to_vec()))
}

/// strings.ToUpper (Go's simple case mapping; invalid bytes become U+FFFD once any rune
/// is not ASCII).
pub fn go_upper(s: &[u8]) -> Vec<u8> {
    if s.is_ascii() {
        return s.to_ascii_uppercase();
    }
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0; 4];
    for c in go_runes(s) {
        out.extend_from_slice(cpa_common::gostr::to_upper_rune(c).encode_utf8(&mut buf).as_bytes());
    }
    out
}

/// util.SanitizedToolNameMap: sanitized name -> declared name for request tools (top-level
/// `name`) whose names need sanitizing; the first declaration wins.
pub fn sanitized_tool_name_map(raw: &[u8]) -> Option<HashMap<Vec<u8>, Vec<u8>>> {
    if raw.is_empty() || !gj::valid(raw) {
        return None;
    }
    let tools = gj::get(raw, "tools");
    if !tools.is_array() {
        return None;
    }
    let mut out = HashMap::new();
    tools.each(|_, tool| {
        let name = trim_space(&tool.get("name").bytes()).to_vec();
        if !name.is_empty() {
            let sanitized = sanitize_function_name(&name);
            if sanitized != name {
                out.entry(sanitized).or_insert(name);
            }
        }
        true
    });
    (!out.is_empty()).then_some(out)
}

/// util.RestoreSanitizedToolName.
pub fn restore_sanitized_tool_name(map: Option<&HashMap<Vec<u8>, Vec<u8>>>, name: &[u8]) -> Vec<u8> {
    map.and_then(|m| m.get(name))
        .filter(|_| !name.is_empty())
        .cloned()
        .unwrap_or_else(|| name.to_vec())
}

/// `time.Unix(secs, 0).Format(time.RFC3339Nano)` in UTC: `YYYY-MM-DDThh:mm:ssZ`.
// ponytail: Go formats in the process's local zone; this assumes UTC, as Go runs in its
// container image. Years beyond four digits print in full, as Go does.
pub fn format_rfc3339_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days (proleptic Gregorian, like Go).
    let z = days as i128 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i128::from(month <= 2);
    let year = if year < 0 {
        format!("-{:04}", -year)
    } else {
        format!("{year:04}")
    };
    format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `time.Parse(time.RFC3339Nano, s).Unix()`: `YYYY-MM-DDThh:mm:ss[.frac]` with `Z` or
/// `±hh:mm`. The hour may have one digit and the fraction may use a comma, as Go's general
/// layout parser allows.
pub fn parse_rfc3339_unix(s: &[u8]) -> Option<i64> {
    fn num(s: &[u8], at: &mut usize, min: usize, max: usize) -> Option<i64> {
        let start = *at;
        while *at < s.len() && *at - start < max && s[*at].is_ascii_digit() {
            *at += 1;
        }
        if *at - start < min {
            return None;
        }
        std::str::from_utf8(&s[start..*at]).ok()?.parse().ok()
    }
    fn lit(s: &[u8], at: &mut usize, c: u8) -> Option<()> {
        (s.get(*at) == Some(&c)).then(|| *at += 1)
    }
    let mut at = 0;
    let year = num(s, &mut at, 4, 4)?;
    lit(s, &mut at, b'-')?;
    let month = num(s, &mut at, 2, 2)?;
    lit(s, &mut at, b'-')?;
    let day = num(s, &mut at, 2, 2)?;
    lit(s, &mut at, b'T')?;
    let hour = num(s, &mut at, 1, 2)?;
    lit(s, &mut at, b':')?;
    let min = num(s, &mut at, 2, 2)?;
    lit(s, &mut at, b':')?;
    let sec = num(s, &mut at, 2, 2)?;
    if matches!(s.get(at), Some(b'.' | b',')) && s.get(at + 1).is_some_and(u8::is_ascii_digit) {
        at += 1;
        while s.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
    }
    let offset = match s.get(at) {
        Some(b'Z') => {
            at += 1;
            0
        }
        Some(&sign @ (b'+' | b'-')) => {
            at += 1;
            let hh = num(s, &mut at, 2, 2)?;
            lit(s, &mut at, b':')?;
            let mm = num(s, &mut at, 2, 2)?;
            // Go's fast path stops at 23; its general layout parser accepts 24.
            if hh > 24 || mm > 59 {
                return None;
            }
            let off = (hh * 60 + mm) * 60;
            if sign == b'-' { -off } else { off }
        }
        _ => return None,
    };
    if at != s.len() || !(1..=12).contains(&month) || hour > 23 || min > 59 || sec > 59 {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][month as usize - 1];
    if day < 1 || day > days_in {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hour * 3600 + min * 60 + sec - offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_formats_like_go_in_utc() {
        assert_eq!(format_rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339_utc(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(format_rfc3339_utc(-1), "1969-12-31T23:59:59Z");
        assert_eq!(format_rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_rfc3339_utc(253_402_300_800), "10000-01-01T00:00:00Z");
        assert_eq!(format_rfc3339_utc(-62_167_219_201), "-0001-12-31T23:59:59Z");
    }

    #[test]
    fn rfc3339_matches_go_unix_seconds() {
        assert_eq!(parse_rfc3339_unix(b"1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_unix(b"2025-01-01T00:00:00.123456789123Z"),
            Some(1735689600)
        );
        assert_eq!(parse_rfc3339_unix(b"2024-02-29T23:59:59+01:30"), Some(1709245799));
        assert_eq!(parse_rfc3339_unix(b"2025-01-01T00:00:00+24:00"), Some(1735603200));
        assert_eq!(parse_rfc3339_unix(b"2025-01-01T00:00:00.Z"), None);
        assert_eq!(parse_rfc3339_unix(b"0001-01-01T00:00:00Z"), Some(-62135596800));
        assert_eq!(parse_rfc3339_unix(b"2023-02-29T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_unix(b"2025-01-01T1:02:03,5-00:00"), Some(1735693323));
        assert_eq!(parse_rfc3339_unix(b"2025-01-01 00:00:00Z"), None);
        assert_eq!(parse_rfc3339_unix(b"2025-01-01T00:00:00"), None);
    }

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
