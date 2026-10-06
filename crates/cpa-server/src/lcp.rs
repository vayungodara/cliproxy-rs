//! Go's Merkle longest-common-prefix session matcher (sdk/cliproxy/session/lcp.go).
//!
//! Requests without an explicit session are reduced to protocol-independent canonical
//! turns, each turn to a bounded fingerprint, and the fingerprints to rolling Merkle
//! prefix keys. The matcher remembers which credential served each sequence, so a
//! conversation that grows, forks or is compacted keeps its credential and a stable
//! `lcp:v1:` session identity.
//!
//! Values are Go byte strings (`Vec<u8>`): fingerprints hash bytes, and samples cut
//! large values at byte offsets as Go does.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use cpa_common::gostr::{GoStr, lower_bytes, trim_space};
use cpa_common::json::{self, GoValue, Kind, Res};
use sha2::{Digest, Sha256};

const TURN_VERSION: &str = "cpa-session-turn-v1";
const LARGE_PART: usize = 16 * 1024;
const SPARSE_BYTES: usize = 12 * 1024;
const DEFAULT_MAX_TURNS: usize = 1024;
// ponytail: Go's caps, which bound entries, not bytes. With session affinity on and four
// interleaved agent sessions sending no session ID (counting allocator, 2026-10-06, MB of
// 1,024 kB), 150-turn conversations at one request every 3 s held 57.7 MB of heap after
// an hour and then 60.7 to 67.0 MB (the 1 h TTL), and 300-turn conversations at one
// request a second held 82.2 MB from 40 minutes on, under these caps. Each turn binds a
// new group, so prefix entries grow with the square of a conversation's length, and every
// fingerprint and prefix key is a 64-character hex string. Keeping 32-byte digests would
// cut that without moving the caps; lower caps would be a difference from Go.
const DEFAULT_MAX_GROUPS: usize = 4096;
const DEFAULT_MAX_PREFIXES: usize = 262_144;
const MAX_TURNS: usize = 4096;
const MAX_PARTS: usize = 256;
const MIN_COMPACTION_OVERLAP: usize = 2;
const PROBE_WINDOW: usize = 32;
const MAX_TAILS_PER_KEY: usize = 16;
// ponytail: requests nested deeper than this skip the matcher (see `Request::new`); far
// beyond any conversation. Iterative extraction would lift the ceiling.
const MAX_NESTING: usize = 128;

// Go RE2: `\d`, `\b` and `\s` are ASCII (`\s` is `[\t\n\f\r ]`, without `\v`); `(?i)`
// folds Unicode (the `k` of `think` also matches U+212A); `.` with `s` matches any rune,
// invalid bytes included.
static ISO8601: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
    regex::bytes::Regex::new(r"(?-u)\b\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?\b")
        .unwrap()
});
static UUID: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
    regex::bytes::Regex::new(
        r"(?-u)\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[1-5][0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}\b",
    )
    .unwrap()
});
static THINK: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
    regex::bytes::Regex::new(r"(?i)[\t\n\x0C\r ]*<(?:think|thinking)>(?s-u:.)*?</(?:think|thinking)>[\t\n\x0C\r ]*")
        .unwrap()
});

/// Go `CanonicalPart`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Part {
    pub kind: String,
    pub mime: String,
    pub value: Vec<u8>,
    pub digest: String,
    pub original_size: i64,
    pub sampled: bool,
}

/// Go `CanonicalTurn`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Turn {
    pub role: String,
    pub parts: Vec<Part>,
}

fn sha_hex(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

/// Go `writeFingerprintField`.
fn field(hash: &mut Sha256, value: &[u8]) {
    hash.update(value.len().to_string().as_bytes());
    hash.update(b":");
    hash.update(value);
    hash.update(b"\0");
}

/// Go `FastTurnFingerprint`.
pub fn fingerprint(turn: &Turn) -> String {
    let turn = normalize_turn(turn);
    let mut hash = Sha256::new();
    field(&mut hash, TURN_VERSION.as_bytes());
    field(&mut hash, turn.role.as_bytes());
    for part in &turn.parts {
        field(&mut hash, part.kind.as_bytes());
        field(&mut hash, part.mime.as_bytes());
        field(&mut hash, part.original_size.to_string().as_bytes());
        // A sampled system part's digest covers unmasked timestamps; the masked sample
        // stands in for it.
        if turn.role != "system" || !part.sampled {
            field(&mut hash, part.digest.as_bytes());
        }
        let large = part.original_size > LARGE_PART as i64 || part.value.len() > LARGE_PART;
        if !part.sampled && large {
            field(&mut hash, &sparse_sample(&part.value, SPARSE_BYTES));
        } else {
            field(&mut hash, &part.value);
        }
    }
    format!("{:x}", hash.finalize())
}

/// Go `ExtractCanonicalTurns`: `format` is the Go translator format name; empty infers
/// it from the payload.
pub fn extract(format: &str, payload: &[u8]) -> Vec<Turn> {
    if payload.is_empty() {
        return Vec::new();
    }
    let root = json::parse(payload);
    if !root.exists() {
        return Vec::new();
    }
    let format = match format.trim() {
        "" => infer_format(&root),
        f => f,
    };
    let is = |name: &str| format.go_eq_fold(name);
    let mut turns = Vec::new();
    if is("claude") {
        messages_turns(&mut turns, &root, true);
    } else if is("gemini") || is("antigravity") {
        gemini_turns(&mut turns, &root);
    } else if is("interactions") {
        interaction_turns(&mut turns, &root);
    } else if is("openai-response") || is("codex") {
        responses_turns(&mut turns, &root);
    } else {
        messages_turns(&mut turns, &root, false);
    }
    turns
        .iter()
        .map(normalize_turn)
        .filter(|t| !t.role.is_empty() && !t.parts.is_empty())
        .take(MAX_TURNS)
        .collect()
}

/// gjson `Result.String()`, trimmed and lowercased as Go's `strings.ToLower(strings.TrimSpace(..))`.
fn lower_trim(value: &Res<'_>) -> String {
    lower_bytes(trim_space(&value.bytes()))
}

fn infer_format(root: &Res<'_>) -> &'static str {
    let mut root = root.clone();
    let request = root.get("request");
    if request.exists() && !root.get("contents").exists() {
        root = request;
    }
    if root.get("contents").is_array()
        || root.get("systemInstruction").exists()
        || root.get("system_instruction").exists()
    {
        return "gemini";
    }
    if root.get("instructions").exists() {
        return "openai-response";
    }
    let input = root.get("input");
    if input.exists() {
        if input.kind == Kind::String {
            return "interactions";
        }
        for item in input.array() {
            let typ = lower_trim(&item.get("type"));
            if typ.contains("user_input") || typ.contains("instruction") {
                return "interactions";
            }
        }
        return "openai-response";
    }
    if root.get("system").exists() {
        return "claude";
    }
    "openai"
}

fn has_capacity(turns: &[Turn]) -> bool {
    turns.len() < MAX_TURNS
}

fn append_turn(turns: &mut Vec<Turn>, role: &str, parts: Vec<Part>) {
    if !has_capacity(turns) || parts.is_empty() {
        return;
    }
    turns.push(Turn {
        role: role.to_owned(),
        parts: limit_parts(Vec::new(), parts),
    });
}

fn messages_turns(turns: &mut Vec<Turn>, root: &Res<'_>, top_level_system: bool) {
    if top_level_system {
        let system = root.get("system");
        if system.exists() {
            append_turn(turns, "system", parts_from(&system));
        }
    }
    root.get("messages").each(|_, message| {
        if !has_capacity(turns) {
            return false;
        }
        let mut role = canonical_role(&message.get("role").bytes());
        if role.is_empty() {
            role = "unknown".into();
        }
        let content = message.get("content");
        let mut parts = parts_from(&content);
        for key in ["tool_calls", "tool_call", "function_call", "tool_use"] {
            let value = message.get(key);
            if value.exists() {
                parts.extend(parts_from(&value));
            }
        }
        if parts.is_empty() && !content.exists() {
            parts = parts_from(&message);
        }
        append_turn(turns, &role, parts);
        true
    });
}

fn responses_turns(turns: &mut Vec<Turn>, root: &Res<'_>) {
    let instructions = root.get("instructions");
    if instructions.exists() {
        append_turn(turns, "system", parts_from(&instructions));
    }
    if !has_capacity(turns) {
        return;
    }
    let input = root.get("input");
    if !input.exists() {
        return;
    }
    if input.kind == Kind::String {
        append_turn(turns, "user", parts_from(&input));
        return;
    }
    input.each(|_, item| {
        if !has_capacity(turns) {
            return false;
        }
        let typ = lower_trim(&item.get("type"));
        if typ == "reasoning" || typ == "response.output_text" {
            return true;
        }
        let mut role = canonical_role(&item.get("role").bytes());
        if role.is_empty() {
            role = if typ.contains("function_call_output") || typ.contains("tool_result") {
                "tool"
            } else if typ.contains("function_call") || typ.contains("tool_call") {
                "assistant"
            } else if typ.contains("compaction") {
                "system"
            } else {
                "unknown"
            }
            .into();
        }
        let content = item.get("content");
        let mut parts = parts_from(&content);
        if parts.is_empty() && !content.exists() {
            parts = parts_from(&item);
        }
        append_turn(turns, &role, parts);
        true
    });
}

fn gemini_turns(turns: &mut Vec<Turn>, root: &Res<'_>) {
    let mut root = root.clone();
    let request = root.get("request");
    if request.exists() && !root.get("contents").exists() {
        root = request;
    }
    let mut cached = root.get("cachedContent");
    if !cached.exists() {
        cached = root.get("cached_content");
    }
    if cached.exists() {
        let mut part = text_part(&cached);
        part.kind = "resource".into();
        append_turn(turns, "system", vec![part]);
    }
    let system = root.get("systemInstruction");
    if system.exists() {
        append_turn(turns, "system", parts_from(&system));
    } else {
        let system = root.get("system_instruction");
        if system.exists() {
            append_turn(turns, "system", parts_from(&system));
        }
    }
    root.get("contents").each(|_, content| {
        if !has_capacity(turns) {
            return false;
        }
        let mut role = canonical_role(&content.get("role").bytes());
        if role.is_empty() {
            role = "unknown".into();
        }
        let content_parts = content.get("parts");
        let mut parts = parts_from(&content_parts);
        if parts.is_empty() && !content_parts.exists() {
            parts = parts_from(&content);
        }
        append_turn(turns, &role, parts);
        true
    });
}

fn interaction_turns(turns: &mut Vec<Turn>, root: &Res<'_>) {
    let system = root.get("system_instruction");
    if system.exists() {
        append_turn(turns, "system", parts_from(&system));
    } else {
        let system = root.get("systemInstruction");
        if system.exists() {
            append_turn(turns, "system", parts_from(&system));
        }
    }
    interaction_value(turns, &root.get("input"), "");
}

fn interaction_value(turns: &mut Vec<Turn>, value: &Res<'_>, inherited: &str) -> bool {
    if !value.exists() {
        return true;
    }
    if !has_capacity(turns) {
        return false;
    }
    if value.is_array() {
        value.each(|_, child| interaction_value(turns, &child, inherited));
        return has_capacity(turns);
    }
    if value.kind != Kind::Json {
        append_turn(turns, &default_interaction_role(inherited), parts_from(value));
        return has_capacity(turns);
    }
    let steps = value.get("steps");
    if steps.is_array() {
        let mut role = canonical_role(&value.get("role").bytes());
        if role.is_empty() {
            role = inherited.to_owned();
        }
        steps.each(|_, child| interaction_value(turns, &child, &role));
        return has_capacity(turns);
    }
    let typ = lower_trim(&value.get("type"));
    let mut role = canonical_role(&value.get("role").bytes());
    if role.is_empty() {
        role = if typ.contains("system") || typ.contains("developer") {
            "system".into()
        } else if typ.contains("user") {
            "user".into()
        } else if typ.contains("model") || typ.contains("assistant") {
            "assistant".into()
        } else if typ.contains("tool") || typ.contains("function") {
            "tool".into()
        } else {
            default_interaction_role(inherited)
        };
    }
    let content = value.get("content");
    let mut parts = parts_from(&content);
    if parts.is_empty() && !content.exists() {
        parts = parts_from(value);
    }
    append_turn(turns, &role, parts);
    has_capacity(turns)
}

fn default_interaction_role(inherited: &str) -> String {
    match canonical_role(inherited.as_bytes()) {
        role if role.is_empty() => "user".into(),
        role => role,
    }
}

/// Go `canonicalPartsFromJSON`.
fn parts_from(value: &Res<'_>) -> Vec<Part> {
    if !value.exists() {
        return Vec::new();
    }
    match value.kind {
        Kind::String => vec![text_part(value)],
        Kind::Number | Kind::True | Kind::False => vec![Part {
            kind: "value".into(),
            value: value.raw().to_vec(),
            original_size: value.raw().len() as i64,
            ..Part::default()
        }],
        Kind::Json => {
            if is_reasoning(value) {
                return Vec::new();
            }
            if value.is_array() {
                let mut parts = Vec::new();
                let mut dropped = 0;
                value.each(|_, child| {
                    if parts.len() >= MAX_PARTS {
                        dropped += 1;
                        return true;
                    }
                    let added = parts_from(&child);
                    parts = limit_parts(std::mem::take(&mut parts), added);
                    true
                });
                if dropped > 0 {
                    parts = limit_count(parts, dropped);
                }
                return parts;
            }
            let text = value.get("text");
            if text.kind == Kind::String {
                return vec![text_part(&text)];
            }
            let content = value.get("content");
            if content.exists() {
                return parts_from(&content);
            }
            let parts = value.get("parts");
            if parts.exists() {
                return parts_from(&parts);
            }
            let typ = lower_trim(&value.get("type"));
            if matches!(typ.as_str(), "input_text" | "output_text" | "text") && text.exists() {
                return vec![text_part(&text)];
            }
            if typ.contains("tool") || typ.contains("function_call") || typ == "function" {
                return vec![json_part(format!("tool:{typ}"), value)];
            }
            if let Some(kind) = gemini_tool_kind(value) {
                return vec![json_part(kind.into(), value)];
            }
            let media = [
                "image_url",
                "inlineData",
                "inline_data",
                "fileData",
                "file_data",
                "source",
            ];
            if media.iter().any(|key| value.get(*key).exists()) {
                return vec![json_part("media".into(), value)];
            }
            vec![json_part("json".into(), value)]
        }
        Kind::Null => vec![json_part("value".into(), value)],
    }
}

/// Go `canonicalTextPart`.
fn text_part(value: &Res<'_>) -> Part {
    let text = normalize_text(&value.bytes(), false);
    if text.len() > LARGE_PART {
        return Part {
            kind: "text".into(),
            value: sparse_sample(&text, SPARSE_BYTES),
            digest: sha_hex(&text),
            original_size: text.len() as i64,
            sampled: true,
            ..Part::default()
        };
    }
    Part {
        kind: "text".into(),
        original_size: text.len() as i64,
        value: text,
        ..Part::default()
    }
}

/// Go `canonicalJSONPart`: small values as Go re-marshals them after decoding into `any`
/// (sorted keys, float64 numbers, HTML-escaped); large ones sampled with a digest.
fn json_part(kind: String, value: &Res<'_>) -> Part {
    let raw = value.raw();
    if raw.len() > LARGE_PART {
        return Part {
            kind,
            value: sparse_sample(raw, SPARSE_BYTES),
            digest: sha_hex(raw),
            original_size: raw.len() as i64,
            sampled: true,
            ..Part::default()
        };
    }
    let raw = GoValue::parse_f64(raw).map_or_else(|| raw.to_vec(), |v| v.marshal());
    Part {
        kind,
        original_size: raw.len() as i64,
        value: raw,
        ..Part::default()
    }
}

fn is_marker(part: &Part) -> bool {
    part.kind == "value" && part.value.starts_with(b"<truncated:")
}

/// Go `limitCanonicalParts`: at most 256 parts, the rest folded into one marker that
/// counts them.
fn limit_parts(mut parts: Vec<Part>, mut added: Vec<Part>) -> Vec<Part> {
    if added.is_empty() {
        return parts;
    }
    let mut existing_dropped = 0;
    let has_marker = added.last().is_some_and(is_marker);
    if has_marker {
        existing_dropped = added.pop().map_or(0, |m| m.original_size);
    }
    let space = MAX_PARTS as i64 - parts.len() as i64;
    if space <= 0 {
        return limit_count(parts, added.len() as i64 + existing_dropped);
    }
    let space = space as usize;
    if added.len() > space {
        let dropped = (added.len() - space) as i64 + existing_dropped;
        added.truncate(space);
        parts.extend(added);
        return limit_count(parts, dropped);
    }
    parts.extend(added);
    if has_marker || existing_dropped > 0 {
        return limit_count(parts, existing_dropped);
    }
    parts
}

/// Go `limitCanonicalPartsCount`.
fn limit_count(mut parts: Vec<Part>, dropped: i64) -> Vec<Part> {
    if dropped <= 0 {
        return parts;
    }
    if let Some(marker) = parts.last_mut().filter(|p| is_marker(p)) {
        marker.original_size += dropped;
        marker.value = format!("<truncated:{} parts>", marker.original_size).into_bytes();
        return parts;
    }
    parts.push(Part {
        kind: "value".into(),
        value: format!("<truncated:{dropped} parts>").into_bytes(),
        original_size: dropped,
        ..Part::default()
    });
    parts
}

fn is_reasoning(value: &Res<'_>) -> bool {
    if value.kind != Kind::Json || value.is_array() {
        return false;
    }
    let typ = lower_trim(&value.get("type"));
    if matches!(typ.as_str(), "thinking" | "reasoning" | "thought") || typ.contains("reasoning") {
        return true;
    }
    value.get("thought").kind == Kind::True
}

fn gemini_tool_kind(value: &Res<'_>) -> Option<&'static str> {
    if value.get("functionCall").exists() || value.get("function_call").exists() {
        return Some("tool:function_call");
    }
    if value.get("functionResponse").exists() || value.get("function_response").exists() {
        return Some("tool:function_response");
    }
    None
}

/// Go `canonicalRole`.
fn canonical_role(role: &[u8]) -> String {
    let role = lower_bytes(trim_space(role));
    match role.as_str() {
        "system" | "developer" => "system".into(),
        "assistant" | "model" | "ai" => "assistant".into(),
        "tool" | "function" => "tool".into(),
        _ => role,
    }
}

/// Go `normalizeCanonicalTurn`.
fn normalize_turn(turn: &Turn) -> Turn {
    let role = canonical_role(turn.role.as_bytes());
    let mut parts = Vec::with_capacity(turn.parts.len());
    for part in &turn.parts {
        if part.value.is_empty() {
            continue;
        }
        let mut part = part.clone();
        part.kind = lower_bytes(trim_space(part.kind.as_bytes()));
        part.mime = lower_bytes(trim_space(part.mime.as_bytes()));
        if part.original_size <= 0 {
            part.original_size = part.value.len() as i64;
        }
        if part.kind == "text" {
            if role == "system" {
                part.value = normalize_text(&part.value, true);
            } else if !part.sampled {
                part.value = normalize_text(&part.value, false);
            }
            if !part.sampled {
                part.original_size = part.value.len() as i64;
            }
        }
        if !part.value.is_empty() {
            parts.push(part);
        }
    }
    let mut parts = limit_parts(Vec::new(), parts);
    // Parallel tool calls in any order fingerprint the same.
    let tool_indexes: Vec<usize> = parts
        .iter()
        .enumerate()
        .filter(|(_, p)| p.kind.starts_with("tool:") || p.kind.contains("function_call"))
        .map(|(i, _)| i)
        .collect();
    if tool_indexes.len() > 1 {
        let mut tools: Vec<Part> = tool_indexes.iter().map(|&i| parts[i].clone()).collect();
        tools.sort_by(|a, b| a.value.cmp(&b.value).then_with(|| a.digest.cmp(&b.digest)));
        for (i, tool) in tool_indexes.into_iter().zip(tools) {
            parts[i] = tool;
        }
    }
    Turn { role, parts }
}

/// Go `normalizeText`: newlines unified, think blocks dropped, system timestamps and
/// UUIDs masked, then trimmed.
fn normalize_text(value: &[u8], mask_system_dynamics: bool) -> Vec<u8> {
    let mut value = value.to_vec();
    if value.contains(&b'\r') {
        let mut out = Vec::with_capacity(value.len());
        let mut i = 0;
        while i < value.len() {
            if value[i] == b'\r' {
                out.push(b'\n');
                if value.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
            } else {
                out.push(value[i]);
            }
            i += 1;
        }
        value = out;
    }
    let value = THINK.replace_all(&value, regex::bytes::NoExpand(b" "));
    let value = if mask_system_dynamics {
        let value = ISO8601.replace_all(&value, regex::bytes::NoExpand(b"<timestamp>"));
        UUID.replace_all(&value, regex::bytes::NoExpand(b"<uuid>")).into_owned()
    } else {
        value.into_owned()
    };
    trim_space(&value).to_vec()
}

/// Go `sparseSample`: head, middle and tail thirds of `limit` bytes.
fn sparse_sample(value: &[u8], limit: usize) -> Vec<u8> {
    if value.len() <= limit {
        return value.to_vec();
    }
    let head = limit / 3;
    let middle = limit / 3;
    let tail = limit - head - middle;
    let middle_start = (value.len() - middle) / 2;
    let mut out = Vec::with_capacity(limit);
    out.extend_from_slice(&value[..head]);
    out.extend_from_slice(&value[middle_start..middle_start + middle]);
    out.extend_from_slice(&value[value.len() - tail..]);
    out
}

/// Go `PrepareExt`: what a request contributes to matching.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prepared {
    /// One per turn, at most 4096.
    pub fingerprints: Vec<String>,
    /// The first prefix that includes a non-system turn (0: none).
    pub min_prefix: usize,
    /// The fingerprints of the last 32 turns.
    pub tails: Vec<String>,
    /// Go `EnvironmentDigest` over the system turns.
    pub environment: String,
}

impl Prepared {
    pub fn new(turns: &[Turn]) -> Self {
        let fingerprints: Vec<String> = turns.iter().take(MAX_TURNS).map(fingerprint).collect();
        let start = turns.len().saturating_sub(PROBE_WINDOW);
        Self {
            fingerprints,
            min_prefix: turns
                .iter()
                .position(|t| canonical_role(t.role.as_bytes()) != "system")
                .map_or(0, |i| i + 1),
            tails: turns[start..].iter().map(fingerprint).collect(),
            environment: environment_digest(turns),
        }
    }

    /// Go's guard before matching or binding.
    pub fn usable(&self) -> bool {
        !self.fingerprints.is_empty() && self.min_prefix > 0 && self.min_prefix <= self.fingerprints.len()
    }
}

/// Go `EnvironmentDigest`: every system turn's fingerprint, empty without one.
pub fn environment_digest(turns: &[Turn]) -> String {
    let mut hash = Sha256::new();
    let mut any = false;
    for turn in turns.iter().filter(|t| canonical_role(t.role.as_bytes()) == "system") {
        any = true;
        field(&mut hash, fingerprint(turn).as_bytes());
    }
    if any {
        format!("{:x}", hash.finalize())
    } else {
        String::new()
    }
}

/// Go `fallbackEnvironmentDigest` and `environmentDigest`: the leading system-turn
/// fingerprints.
fn fallback_environment(fingerprints: &[String], min_prefix: usize) -> String {
    if min_prefix <= 1 || fingerprints.is_empty() {
        return String::new();
    }
    let mut hash = Sha256::new();
    for fp in &fingerprints[..(min_prefix - 1).min(fingerprints.len())] {
        field(&mut hash, fp.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

fn last_window(fingerprints: &[String]) -> Vec<String> {
    fingerprints[fingerprints.len().saturating_sub(PROBE_WINDOW)..].to_vec()
}

/// Go `rollingPrefixKeys`: `n:hex` where each hash chains the previous one.
fn rolling_prefix_keys(fingerprints: &[String]) -> Vec<String> {
    let mut previous = [0u8; 32];
    fingerprints
        .iter()
        .enumerate()
        .map(|(i, fp)| {
            let mut hash = Sha256::new();
            hash.update(previous);
            hash.update(b"\0");
            hash.update(fp.as_bytes());
            previous = hash.finalize().into();
            let hex: String = previous.iter().map(|b| format!("{b:02x}")).collect();
            format!("{}:{hex}", i + 1)
        })
        .collect()
}

fn sequence_key(fingerprints: &[String]) -> String {
    rolling_prefix_keys(fingerprints).pop().unwrap_or_default()
}

fn session_id(namespace: &str, first_prefix: &str) -> String {
    let seed = format!("cli-proxy-api:lcp-session:v1\0{namespace}\0{first_prefix}");
    format!("lcp:v1:{}", sha_hex(seed.as_bytes()))
}

fn compaction_session_id(namespace: &str, parent: &str, sequence: &str) -> String {
    let seed = format!("cli-proxy-api:lcp-compaction-session:v1\0{namespace}\0{parent}\0{sequence}");
    format!("lcp:v1:{}", sha_hex(seed.as_bytes()))
}

/// What a request without an explicit session brings to selection (Go `pickLCP`'s
/// inputs): its caller scope and prepared turns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    caller_scope: String,
    prepared: Prepared,
}

impl Request {
    /// `None` when Go skips the matcher: an anonymous caller (no caller scope) or no
    /// usable turns (empty, or system turns only). Also `None` for a body nested deeper
    /// than [`MAX_NESTING`], which Go walks on its growable stack: extraction recurses
    /// once per level, and a native stack would overflow and abort the server.
    pub fn new(format: &str, body: &[u8], caller: &str) -> Option<Self> {
        let caller_scope = cpa_common::session::caller_scope(caller);
        if caller_scope.is_empty() || nests_deeper(body, MAX_NESTING) {
            return None;
        }
        let prepared = Prepared::new(&extract(format, body));
        prepared.usable().then_some(Self { caller_scope, prepared })
    }

    /// Go `lcpAffinityNamespace`. With session affinity on, Go always selects through
    /// the mixed-provider picker, so the provider is `mixed`.
    pub fn namespace(&self, model: &str) -> String {
        format!(
            "lcp:v1::mixed::{}::{}",
            crate::scheduler::canonical_model(model),
            self.caller_scope
        )
    }

    pub fn prepared(&self) -> &Prepared {
        &self.prepared
    }
}

/// Whether `body` has more than `limit` arrays and objects open at once (brackets
/// inside strings do not count).
fn nests_deeper(body: &[u8], limit: usize) -> bool {
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for &c in body {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > limit {
                    return true;
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

/// Go `MerklePrefixMatch` (and `MerklePrefixBindResult`, which has no credential or
/// prefix length).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Match {
    pub auth: String,
    pub session: String,
    pub parent: String,
    pub prefix_length: usize,
    pub fork: bool,
    pub compaction: bool,
    pub node_kind: String,
    /// The access generation that guards later removal.
    pub access: u64,
}

impl Match {
    /// The session hierarchy Go's selector writes into the request metadata after a
    /// pick (`node_kind`, `is_fork`, `is_compaction`): a plain hit carries none.
    pub fn node(&self) -> (&'static str, bool, bool) {
        if self.fork {
            ("fork", true, false)
        } else if self.compaction {
            ("compaction", false, true)
        } else {
            ("", false, false)
        }
    }
}

/// Go `MerklePrefixMatcherConfig`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Limits {
    pub ttl: Duration,
    pub max_turns: usize,
    pub max_groups: usize,
    pub max_prefixes: usize,
}

struct Group {
    key: String,
    namespace: String,
    auth: String,
    session: String,
    parent: String,
    min_prefix: usize,
    fork: bool,
    compaction: bool,
    node_kind: String,
    environment: String,
    fingerprints: Vec<String>,
    tails: Vec<String>,
    prefix_keys: Vec<String>,
    expires: Instant,
    last_access: u64,
    lru: u64,
}

impl Group {
    /// The tail window indexed for compaction (Go falls back to all fingerprints).
    fn tail_window(&self) -> &[String] {
        if self.tails.is_empty() {
            &self.fingerprints
        } else {
            &self.tails
        }
    }
}

fn tail_key(tails: &[String]) -> Option<String> {
    let n = tails.len();
    (n >= MIN_COMPACTION_OVERLAP).then(|| format!("{}\0{}", tails[n - 2], tails[n - 1]))
}

#[derive(Default)]
struct Namespace {
    /// Sequence key to group.
    groups: HashMap<String, u64>,
    /// Prefix key to the groups (by sequence key) whose trajectory has that prefix.
    prefixes: HashMap<String, HashMap<String, u64>>,
    /// The last two tail fingerprints to the groups ending with them.
    tails: HashMap<String, BTreeSet<u64>>,
}

/// Go `MerklePrefixMatcher`: bounded (by groups and prefix entries, least recently used
/// first), expiring after the TTL, with no background task. Callers serialize access.
pub struct Matcher {
    limits: Limits,
    namespaces: HashMap<String, Namespace>,
    groups: HashMap<u64, Group>,
    /// LRU order: sequence number to group.
    lru: BTreeMap<u64, u64>,
    next_group: u64,
    next_lru: u64,
    group_count: usize,
    prefix_count: usize,
    /// Monotonic across `clear`, so a pre-clear generation never evicts a newer binding.
    access: u64,
    operations: u64,
}

impl Default for Matcher {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl Matcher {
    /// Go `NewMerklePrefixMatcherWithConfig`, defaults included.
    pub fn new(limits: Limits) -> Self {
        let mut limits = limits;
        if limits.ttl.is_zero() {
            limits.ttl = Duration::from_secs(3600);
        }
        if limits.max_turns == 0 {
            limits.max_turns = DEFAULT_MAX_TURNS;
        }
        if limits.max_groups == 0 {
            limits.max_groups = DEFAULT_MAX_GROUPS;
        }
        if limits.max_prefixes == 0 {
            limits.max_prefixes = DEFAULT_MAX_PREFIXES;
        }
        limits.max_prefixes = limits.max_prefixes.max(limits.max_turns);
        Self {
            limits,
            namespaces: HashMap::new(),
            groups: HashMap::new(),
            lru: BTreeMap::new(),
            next_group: 0,
            next_lru: 0,
            group_count: 0,
            prefix_count: 0,
            access: 0,
            operations: 0,
        }
    }

    /// A fresh matcher on `limits` that keeps counting access generations, so a lease
    /// picked before the change never evicts a binding made after it.
    pub fn reset(&mut self, limits: Limits) {
        let access = self.access;
        *self = Self::new(limits);
        self.access = access;
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Go's public-method defaults for missing tails and environment, then
    /// `sanitizeFingerprints` (at most `max_turns` fingerprints).
    fn context<'p>(&self, prepared: &'p Prepared) -> Option<(&'p [String], Vec<String>, String)> {
        let tails = if prepared.tails.is_empty() {
            last_window(&prepared.fingerprints)
        } else {
            prepared.tails.clone()
        };
        let environment = if prepared.environment.is_empty() {
            fallback_environment(&prepared.fingerprints, prepared.min_prefix)
        } else {
            prepared.environment.clone()
        };
        if !prepared.usable() {
            return None;
        }
        let fingerprints = &prepared.fingerprints[..prepared.fingerprints.len().min(self.limits.max_turns)];
        (prepared.min_prefix <= fingerprints.len()).then_some((fingerprints, tails, environment))
    }

    /// Go `MatchFingerprintsWithContext`: the longest known prefix, a fork of it, or a
    /// compaction continuation.
    pub fn find(&mut self, namespace: &str, prepared: &Prepared, now: Instant) -> Option<Match> {
        if namespace.is_empty() {
            return None;
        }
        let (fingerprints, tails, environment) = self.context(prepared)?;
        self.tick(now);
        self.match_locked(namespace, fingerprints, &tails, &environment, prepared.min_prefix, now)
    }

    /// Go `BindFingerprintsWithContext`: records the sequence for `auth`; the result
    /// carries its session identity. `None` when nothing could be bound.
    pub fn bind(&mut self, namespace: &str, prepared: &Prepared, auth: &str, now: Instant) -> Option<Match> {
        let auth = auth.trim();
        if namespace.trim().is_empty() || auth.is_empty() {
            return None;
        }
        let (fingerprints, tails, environment) = self.context(prepared)?;
        self.tick(now);
        let bound = self.bind_locked(
            namespace,
            fingerprints,
            tails,
            environment,
            prepared.min_prefix,
            auth,
            now,
        );
        (!bound.session.is_empty()).then_some(bound)
    }

    /// Go `TouchFingerprintsWithContext`: refreshes the sequence if `auth` still owns it,
    /// or binds a new extension. False when another credential took it over.
    pub fn touch(&mut self, namespace: &str, prepared: &Prepared, auth: &str, now: Instant) -> bool {
        let auth = auth.trim();
        if namespace.trim().is_empty() || auth.is_empty() {
            return false;
        }
        let Some((fingerprints, tails, environment)) = self.context(prepared) else {
            return false;
        };
        self.tick(now);
        self.touch_locked(
            namespace,
            fingerprints,
            tails,
            environment,
            prepared.min_prefix,
            auth,
            now,
        )
    }

    /// Go `RemoveFingerprintsBefore`: drops the exact sequence bound to `auth` unless it
    /// was refreshed after `generation` (0: unconditionally).
    pub fn remove(
        &mut self,
        namespace: &str,
        fingerprints: &[String],
        auth: &str,
        generation: u64,
        now: Instant,
    ) -> bool {
        if namespace.is_empty() || auth.is_empty() || fingerprints.is_empty() {
            return false;
        }
        let fingerprints = &fingerprints[..fingerprints.len().min(self.limits.max_turns)];
        self.tick(now);
        let Some(ns) = self.namespaces.get(namespace) else {
            return false;
        };
        let Some(&id) = ns.groups.get(&sequence_key(fingerprints)) else {
            return false;
        };
        let group = &self.groups[&id];
        if group.auth != auth || (generation > 0 && group.last_access > generation) {
            return false;
        }
        self.remove_group(id);
        true
    }

    /// Go `InvalidateAuth`.
    pub fn invalidate(&mut self, auth: &str) {
        if auth.is_empty() {
            return;
        }
        let ids: Vec<u64> = self.ordered_ids(|g| g.auth == auth);
        ids.into_iter().for_each(|id| {
            self.remove_group(id);
        });
    }

    /// Drops the bindings of credentials `keep` rejects (removed credentials).
    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        let ids = self.ordered_ids(|g| !keep(&g.auth));
        ids.into_iter().for_each(|id| {
            self.remove_group(id);
        });
    }

    /// Go `Clear`; the access counter keeps counting.
    pub fn clear(&mut self) {
        self.namespaces.clear();
        self.groups.clear();
        self.lru.clear();
        self.group_count = 0;
        self.prefix_count = 0;
    }

    /// Go `LookupSession`: the credentials bound to an LCP session (sorted) and its
    /// namespace, without refreshing anything; expired groups are dropped.
    pub fn lookup(&mut self, session: &str, now: Instant) -> Option<(Vec<String>, String)> {
        if session.is_empty() {
            return None;
        }
        let expired = self.ordered_ids(|g| g.session == session && now >= g.expires);
        expired.into_iter().for_each(|id| {
            self.remove_group(id);
        });
        let active = self.ordered_ids(|g| g.session == session);
        let namespace = self.groups.get(active.first()?)?.namespace.clone();
        let mut auths: Vec<String> = active
            .iter()
            .map(|id| self.groups[id].auth.clone())
            .filter(|a| !a.is_empty())
            .collect();
        auths.sort();
        auths.dedup();
        (!auths.is_empty()).then_some((auths, namespace))
    }

    /// Group IDs (creation order) whose group satisfies `keep`.
    fn ordered_ids(&self, keep: impl Fn(&Group) -> bool) -> Vec<u64> {
        let mut ids: Vec<u64> = self.groups.iter().filter(|(_, g)| keep(g)).map(|(id, _)| *id).collect();
        ids.sort_unstable();
        ids
    }

    /// Go `prepareLocked`: an expiry sweep every 128 operations.
    fn tick(&mut self, now: Instant) {
        self.operations += 1;
        if self.operations.is_multiple_of(128) {
            let expired = self.ordered_ids(|g| now >= g.expires);
            expired.into_iter().for_each(|id| {
                self.remove_group(id);
            });
        }
    }

    fn next_access(&mut self) -> u64 {
        self.access += 1;
        self.access
    }

    fn move_to_back(&mut self, id: u64) {
        let seq = self.next_lru;
        self.next_lru += 1;
        if let Some(group) = self.groups.get_mut(&id) {
            self.lru.remove(&group.lru);
            group.lru = seq;
            self.lru.insert(seq, id);
        }
    }

    /// Go `touchLocked`.
    #[allow(clippy::too_many_arguments)]
    fn touch_locked(
        &mut self,
        namespace: &str,
        fingerprints: &[String],
        tails: Vec<String>,
        environment: String,
        min_prefix: usize,
        auth: &str,
        now: Instant,
    ) -> bool {
        let key = sequence_key(fingerprints);
        let existing = self
            .namespaces
            .entry(namespace.to_owned())
            .or_default()
            .groups
            .get(&key)
            .copied();
        let Some(id) = existing else {
            self.bind_locked(namespace, fingerprints, tails, environment, min_prefix, auth, now);
            return true;
        };
        if now >= self.groups[&id].expires {
            self.remove_group(id);
            self.bind_locked(namespace, fingerprints, tails, environment, min_prefix, auth, now);
            return true;
        }
        let group = self.groups.get_mut(&id).expect("indexed group");
        if group.auth != auth {
            if group.auth == "home-pending" || group.auth.is_empty() {
                group.auth = auth.to_owned();
            } else {
                // A delayed success never overwrites a failover's newer binding.
                return false;
            }
        }
        let access = self.next_access();
        let ttl = self.limits.ttl;
        let group = self.groups.get_mut(&id).expect("indexed group");
        group.expires = now + ttl;
        group.last_access = access;
        group.environment = environment;
        if !tails.is_empty() && group.tails != tails {
            let mut group = self.remove_group(id).expect("indexed group");
            group.tails = tails;
            self.add_group(id, group);
        }
        self.move_to_back(id);
        true
    }

    /// Go `bindLocked`.
    #[allow(clippy::too_many_arguments)]
    fn bind_locked(
        &mut self,
        namespace: &str,
        fingerprints: &[String],
        mut tails: Vec<String>,
        mut environment: String,
        min_prefix: usize,
        auth: &str,
        now: Instant,
    ) -> Match {
        if tails.is_empty() {
            tails = last_window(fingerprints);
        }
        if environment.is_empty() {
            environment = fallback_environment(fingerprints, min_prefix);
        }
        let key = sequence_key(fingerprints);
        let existing = self
            .namespaces
            .entry(namespace.to_owned())
            .or_default()
            .groups
            .get(&key)
            .copied();
        if let Some(id) = existing {
            let live = now < self.groups[&id].expires;
            let old = self.remove_group(id).expect("indexed group");
            if live {
                // Rebinding keeps the sequence's identity and lineage.
                let access = self.next_access();
                let group = Group {
                    key,
                    namespace: namespace.to_owned(),
                    auth: auth.to_owned(),
                    session: old.session.clone(),
                    parent: old.parent.clone(),
                    min_prefix,
                    fork: old.fork,
                    compaction: old.compaction,
                    node_kind: old.node_kind.clone(),
                    environment,
                    fingerprints: fingerprints.to_vec(),
                    tails,
                    prefix_keys: old.prefix_keys,
                    expires: now + self.limits.ttl,
                    last_access: access,
                    lru: 0,
                };
                let id = self.new_id();
                let access = self.add_group(id, group);
                return Match {
                    session: old.session,
                    parent: old.parent,
                    fork: old.fork,
                    compaction: old.compaction,
                    node_kind: old.node_kind,
                    access,
                    ..Match::default()
                };
            }
        }
        let mut auth = auth.to_owned();
        let found = self.match_locked(namespace, fingerprints, &tails, &environment, min_prefix, now);
        let mut bound = Match::default();
        if let Some(found) = found {
            if found.compaction && !found.auth.is_empty() && auth.is_empty() {
                auth = found.auth.clone();
            }
            bound = Match {
                auth: String::new(),
                prefix_length: 0,
                ..found
            };
        }
        let prefix_keys = rolling_prefix_keys(fingerprints);
        if bound.session.is_empty() {
            let first = match prefix_keys.len() {
                0 => String::new(),
                n => prefix_keys[if min_prefix > 0 && min_prefix <= n {
                    min_prefix - 1
                } else {
                    0
                }]
                .clone(),
            };
            bound.session = session_id(namespace, &first);
        }
        let access = self.next_access();
        let group = Group {
            key,
            namespace: namespace.to_owned(),
            auth,
            session: bound.session.clone(),
            parent: bound.parent.clone(),
            min_prefix,
            fork: bound.fork,
            compaction: bound.compaction,
            node_kind: bound.node_kind.clone(),
            environment,
            fingerprints: fingerprints.to_vec(),
            tails,
            prefix_keys,
            expires: now + self.limits.ttl,
            last_access: access,
            lru: 0,
        };
        let id = self.new_id();
        bound.access = self.add_group(id, group);
        bound
    }

    fn new_id(&mut self) -> u64 {
        self.next_group += 1;
        self.next_group
    }

    /// Go `addGroupLocked`: indexes the group, then evicts least recently used groups
    /// past the bounds. Returns the group's access generation.
    fn add_group(&mut self, id: u64, mut group: Group) -> u64 {
        if group.prefix_keys.is_empty() {
            group.prefix_keys = rolling_prefix_keys(&group.fingerprints);
        }
        let ns = self.namespaces.entry(group.namespace.clone()).or_default();
        ns.groups.insert(group.key.clone(), id);
        for prefix in &group.prefix_keys {
            ns.prefixes
                .entry(prefix.clone())
                .or_default()
                .insert(group.key.clone(), id);
        }
        if let Some(tail) = tail_key(group.tail_window()) {
            ns.tails.entry(tail).or_default().insert(id);
        }
        group.last_access = self.next_access();
        group.lru = self.next_lru;
        self.lru.insert(self.next_lru, id);
        self.next_lru += 1;
        self.group_count += 1;
        self.prefix_count += group.prefix_keys.len();
        let access = group.last_access;
        self.groups.insert(id, group);
        while self.group_count > self.limits.max_groups || self.prefix_count > self.limits.max_prefixes {
            let Some((_, &oldest)) = self.lru.first_key_value() else {
                break;
            };
            self.remove_group(oldest);
        }
        access
    }

    /// Go `removeGroupLocked`.
    fn remove_group(&mut self, id: u64) -> Option<Group> {
        let group = self.groups.remove(&id)?;
        if let Some(ns) = self.namespaces.get_mut(&group.namespace) {
            if ns.groups.get(&group.key) == Some(&id) {
                ns.groups.remove(&group.key);
            }
            for prefix in &group.prefix_keys {
                if let Some(bucket) = ns.prefixes.get_mut(prefix) {
                    bucket.remove(&group.key);
                    if bucket.is_empty() {
                        ns.prefixes.remove(prefix);
                    }
                }
            }
            if let Some(tail) = tail_key(group.tail_window())
                && let Some(bucket) = ns.tails.get_mut(&tail)
            {
                bucket.remove(&id);
                if bucket.is_empty() {
                    ns.tails.remove(&tail);
                }
            }
            if ns.groups.is_empty() {
                self.namespaces.remove(&group.namespace);
            }
        }
        self.lru.remove(&group.lru);
        self.group_count = self.group_count.saturating_sub(1);
        self.prefix_count = self.prefix_count.saturating_sub(group.prefix_keys.len());
        Some(group)
    }

    /// Go `matchLocked`: binary search for the longest live prefix.
    fn match_locked(
        &mut self,
        namespace: &str,
        fingerprints: &[String],
        tails: &[String],
        environment: &str,
        min_prefix: usize,
        now: Instant,
    ) -> Option<Match> {
        if !self.namespaces.contains_key(namespace)
            || fingerprints.is_empty()
            || min_prefix == 0
            || min_prefix > fingerprints.len()
        {
            return None;
        }
        let prefix_keys = rolling_prefix_keys(fingerprints);
        let (mut low, mut high) = (min_prefix, fingerprints.len());
        let mut best = None;
        let mut best_len = 0;
        while low <= high {
            let middle = low + (high - low) / 2;
            match self.newest_matching(namespace, &prefix_keys[middle - 1], &fingerprints[..middle], now) {
                Some(id) => {
                    best = Some(id);
                    best_len = middle;
                    low = middle + 1;
                }
                None => high = middle - 1,
            }
        }
        let Some(id) = best else {
            return self.match_compaction(
                namespace,
                fingerprints,
                tails,
                environment,
                &prefix_keys,
                min_prefix,
                0,
                now,
            );
        };
        let access = self.next_access();
        let ttl = self.limits.ttl;
        let group = self.groups.get_mut(&id).expect("indexed group");
        group.expires = now + ttl;
        group.last_access = access;
        let (mut session, mut parent, mut node_kind) =
            (group.session.clone(), group.parent.clone(), group.node_kind.clone());
        let (auth, group_compaction, group_len) = (group.auth.clone(), group.compaction, group.fingerprints.len());
        self.move_to_back(id);
        let mut fork = false;
        // A request that leaves the known trajectory after the common prefix is a fork,
        // unless it keeps a trailing run of the parent (an in-place compaction).
        if best_len < group_len && fingerprints.len() > best_len {
            if let Some(found) = self.match_compaction(
                namespace,
                fingerprints,
                tails,
                environment,
                &prefix_keys,
                min_prefix,
                best_len,
                now,
            ) {
                return Some(found);
            }
            fork = true;
            node_kind = "fork".into();
            parent = session_id(namespace, &prefix_keys[best_len - 1]);
            session = session_id(namespace, &prefix_keys[best_len]);
        }
        Some(Match {
            auth,
            session,
            parent,
            prefix_length: best_len,
            fork,
            compaction: group_compaction && !fork,
            node_kind,
            access,
        })
    }

    /// Go `newestMatchingGroup`: the longest live trajectory with this exact prefix,
    /// most recently accessed first.
    fn newest_matching(&self, namespace: &str, prefix: &str, fingerprints: &[String], now: Instant) -> Option<u64> {
        let bucket = self.namespaces.get(namespace)?.prefixes.get(prefix)?;
        let mut best: Option<&Group> = None;
        let mut best_id = None;
        for id in bucket.values() {
            let Some(group) = self.groups.get(id) else { continue };
            if now >= group.expires
                || group.min_prefix > fingerprints.len()
                || group.fingerprints.len() < fingerprints.len()
                || group.fingerprints[..fingerprints.len()] != *fingerprints
            {
                continue;
            }
            let better = match best {
                None => true,
                Some(b) => {
                    group.fingerprints.len() > b.fingerprints.len()
                        || (group.fingerprints.len() == b.fingerprints.len()
                            && (group.last_access > b.last_access
                                || (group.last_access == b.last_access && group.expires > b.expires)))
                }
            };
            if better {
                best = Some(group);
                best_id = Some(*id);
            }
        }
        best_id
    }

    /// Go `matchCompactionLocked`: the one lineage leaf whose tail the request's tail
    /// continues after its history was summarized or truncated.
    #[allow(clippy::too_many_arguments)]
    fn match_compaction(
        &mut self,
        namespace: &str,
        fingerprints: &[String],
        tails: &[String],
        environment: &str,
        _prefix_keys: &[String],
        min_prefix: usize,
        best_len: usize,
        now: Instant,
    ) -> Option<Match> {
        let owned;
        let candidate_tail = if tails.is_empty() {
            owned = last_window(fingerprints);
            &owned[..]
        } else {
            tails
        };
        let n = candidate_tail.len();
        if n < MIN_COMPACTION_OVERLAP {
            return None;
        }
        let mut candidates: Vec<u64> = Vec::new();
        let mut best_overlap = 0;
        let mut overflow = false;
        for end in (MIN_COMPACTION_OVERLAP - 1..n).rev() {
            let key = format!("{}\0{}", candidate_tail[end - 1], candidate_tail[end]);
            let ns = self.namespaces.get_mut(namespace)?;
            let Some(bucket) = ns.tails.get_mut(&key) else { continue };
            // Lazily prune expired groups from the bucket.
            let groups = &self.groups;
            bucket.retain(|id| groups.get(id).is_some_and(|g| now < g.expires));
            if bucket.is_empty() {
                ns.tails.remove(&key);
                continue;
            }
            if bucket.len() > MAX_TAILS_PER_KEY {
                overflow = true;
                continue;
            }
            for id in bucket.iter() {
                let group = &groups[id];
                if group.environment != environment {
                    continue;
                }
                let parent_tail = group.tail_window();
                let t_len = parent_tail.len();
                if t_len < MIN_COMPACTION_OVERLAP {
                    continue;
                }
                let overlap = overlap(candidate_tail, parent_tail, end, t_len - 1);
                if overlap >= MIN_COMPACTION_OVERLAP
                    && is_compaction_overlap(
                        fingerprints.len(),
                        n,
                        end,
                        group.fingerprints.len(),
                        t_len,
                        t_len - 1,
                        overlap,
                        min_prefix,
                        best_len,
                    )
                {
                    if overlap > best_overlap {
                        best_overlap = overlap;
                        candidates = vec![*id];
                    } else if overlap == best_overlap && !candidates.iter().any(|c| groups[c].session == group.session)
                    {
                        candidates.push(*id);
                    }
                }
            }
        }
        if overflow || candidates.is_empty() {
            return None;
        }
        let leaves: Vec<u64> = candidates
            .iter()
            .copied()
            .filter(|c1| {
                let s1 = &self.groups[c1].session;
                !candidates.iter().any(|c2| {
                    let s2 = &self.groups[c2].session;
                    s1 != s2 && self.is_ancestor(namespace, s1, s2)
                })
            })
            .collect();
        // Several unrelated lineages with the same overlap are ambiguous.
        let [id] = leaves[..] else { return None };
        let access = self.next_access();
        let ttl = self.limits.ttl;
        let group = self.groups.get_mut(&id).expect("indexed group");
        group.expires = now + ttl;
        group.last_access = access;
        let (auth, parent) = (group.auth.clone(), group.session.clone());
        self.move_to_back(id);
        Some(Match {
            auth,
            session: compaction_session_id(namespace, &parent, &sequence_key(fingerprints)),
            parent,
            prefix_length: best_overlap,
            fork: false,
            compaction: true,
            node_kind: "compaction".into(),
            access,
        })
    }

    /// Go `isAncestorSession`: walks parent links (at most 32) from `descendant`.
    fn is_ancestor(&self, namespace: &str, ancestor: &str, descendant: &str) -> bool {
        let Some(ns) = self.namespaces.get(namespace) else {
            return false;
        };
        if ancestor.is_empty() || descendant.is_empty() || ancestor == descendant {
            return false;
        }
        let mut ids: Vec<u64> = ns.groups.values().copied().collect();
        ids.sort_unstable();
        let mut current = descendant.to_owned();
        for _ in 0..32 {
            let parent = ids
                .iter()
                .filter_map(|id| self.groups.get(id))
                .find(|g| g.session == current && !g.parent.is_empty())
                .map(|g| g.parent.clone());
            let Some(parent) = parent else { return false };
            if parent == ancestor {
                return true;
            }
            current = parent;
        }
        false
    }
}

/// Go `calculateOverlap`: the common run ending at both positions, at most 32.
fn overlap(candidate: &[String], parent: &[String], candidate_end: usize, parent_end: usize) -> usize {
    let mut overlap = 0;
    while overlap < PROBE_WINDOW
        && candidate_end >= overlap
        && parent_end >= overlap
        && candidate[candidate_end - overlap] == parent[parent_end - overlap]
    {
        overlap += 1;
    }
    overlap
}

/// Go `isCompactionOverlap`, in Go's signed arithmetic.
#[allow(clippy::too_many_arguments)]
fn is_compaction_overlap(
    full_candidate: usize,
    candidate_tail: usize,
    candidate_end: usize,
    full_parent: usize,
    parent_tail: usize,
    parent_end: usize,
    overlap: usize,
    min_prefix: usize,
    best_len: usize,
) -> bool {
    let [
        full_candidate,
        candidate_tail,
        candidate_end,
        full_parent,
        parent_tail,
        parent_end,
        overlap,
        min_prefix,
        best_len,
    ] = [
        full_candidate,
        candidate_tail,
        candidate_end,
        full_parent,
        parent_tail,
        parent_end,
        overlap,
        min_prefix,
        best_len,
    ]
    .map(|v| v as i64);
    let candidate_start_in_tail = candidate_end - overlap + 1;
    let parent_start = full_parent - 1 - (parent_tail - 1 - parent_end) - overlap + 1;
    let allowed_start = min_prefix.max(best_len + 1);
    if candidate_start_in_tail > 0 {
        let candidate_start = full_candidate - 1 - (candidate_tail - 1 - candidate_end) - overlap + 1;
        if candidate_start > allowed_start {
            return false;
        }
    }
    // History was reduced: early candidate turns were summarized, or early parent turns
    // were truncated away.
    candidate_end - overlap + 1 > 0 || parent_start > 0
}

#[cfg(test)]
#[path = "lcp_tests.rs"]
mod tests;
