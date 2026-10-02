//! Claude thinking signatures: single-layer `E…` (protobuf, first byte 0x12), double-layer
//! `R…` (base64 of the `E` form) and CAIS/CAQS `C…` (first byte 0x08)
//! (claude_validation.go, claude.go).

use gjson::Kind;

use super::wire::{self, BYTES, STD, VARINT, WireError};
use super::{Error, err};
use crate::json;

pub const MAX_CLAUDE_THINKING_SIGNATURE_LEN: usize = 32 * 1024 * 1024;

/// `ClaudeSignatureValidationOptions`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClaudeValidation {
    /// Only check for an `E`/`R` prefix after an optional cache prefix.
    pub prefix_only: bool,
    /// Accept any decodable `E`/`R` signature without inspecting the protobuf tree.
    pub base64_only: bool,
    /// Keep empty thinking placeholders with no signature.
    pub allow_empty_signature_with_empty_text: bool,
    /// Require the full Claude protobuf tree.
    pub strict: bool,
}

impl ClaudeValidation {
    pub const STRICT: Self = Self {
        prefix_only: false,
        base64_only: false,
        allow_empty_signature_with_empty_text: false,
        strict: true,
    };
}

/// `ClaudeSignatureTree`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeSignatureTree {
    pub encoding_layers: i64,
    pub channel_id: u64,
    pub field2: Option<u64>,
    pub routing_class: String,
    pub infrastructure_class: String,
    pub schema_features: String,
    pub model_text: String,
    pub legacy_route_hint: String,
    pub has_field7: bool,
}

/// `IsValidClaudeThinkingSignature`.
pub fn is_valid_claude_thinking_signature(raw: &str, opt: ClaudeValidation) -> bool {
    if opt.prefix_only {
        return has_claude_thinking_signature_prefix(raw);
    }
    if opt.base64_only {
        return has_decodable_claude_thinking_signature(raw);
    }
    normalize_claude_thinking_signature(raw, opt).is_ok()
}

/// `HasDecodableClaudeThinkingSignature`.
pub fn has_decodable_claude_thinking_signature(raw: &str) -> bool {
    let sig = strip_cache_prefix(raw);
    if sig.is_empty() || sig.len() > MAX_CLAUDE_THINKING_SIGNATURE_LEN {
        return false;
    }
    match sig.as_bytes()[0] {
        b'E' => STD.decode(sig).is_ok_and(|d| !d.is_empty()),
        b'R' => match STD.decode(sig) {
            Ok(decoded) if decoded.first() == Some(&b'E') => {
                // Go converts with string(decoded): invalid UTF-8 cannot decode as base64.
                std::str::from_utf8(&decoded)
                    .ok()
                    .and_then(|inner| STD.decode(inner).ok())
                    .is_some_and(|d| !d.is_empty())
            }
            _ => false,
        },
        _ => false,
    }
}

/// `HasClaudeThinkingSignaturePrefix`.
pub fn has_claude_thinking_signature_prefix(raw: &str) -> bool {
    matches!(strip_cache_prefix(raw).as_bytes().first(), Some(b'E' | b'R'))
}

/// Everything after the first `#`, trimmed (any prefix, not only known providers).
pub(crate) fn strip_cache_prefix(raw: &str) -> &str {
    let sig = raw.trim();
    match sig.find('#') {
        Some(i) => sig[i + 1..].trim(),
        None => sig,
    }
}

/// `ValidateClaudeThinkingSignatures`.
pub fn validate_claude_thinking_signatures(body: &str, opt: ClaudeValidation) -> Result<(), Error> {
    let messages = gjson::get(body, "messages");
    if messages.kind() != Kind::Array {
        return Ok(());
    }
    for (i, message) in messages.array().iter().enumerate() {
        let content = message.get("content");
        if content.kind() != Kind::Array {
            continue;
        }
        for (j, part) in content.array().iter().enumerate() {
            if json::go_str(&part.get("type")) != "thinking" {
                continue;
            }
            let raw = json::go_str(&part.get("signature"));
            let raw = raw.trim();
            if raw.is_empty() {
                return err(format!("messages[{i}].content[{j}]: missing thinking signature"));
            }
            if let Err(e) = normalize_claude_thinking_signature(raw, opt) {
                return err(format!("messages[{i}].content[{j}]: {e}"));
            }
        }
    }
    Ok(())
}

/// Go `%q` of `string(sig[0])`: the first byte converted as a rune (U+0000..U+00FF).
pub(crate) fn first_char(sig: &str) -> String {
    go_quote(&char::from(sig.as_bytes()[0]).to_string())
}

fn check_len(sig: &str) -> Result<(), Error> {
    if sig.is_empty() {
        return err("empty signature");
    }
    if sig.len() > MAX_CLAUDE_THINKING_SIGNATURE_LEN {
        return err(format!(
            "signature exceeds maximum length ({MAX_CLAUDE_THINKING_SIGNATURE_LEN} bytes)"
        ));
    }
    Ok(())
}

/// `NormalizeClaudeThinkingSignature`: the double-layer `R` form.
pub fn normalize_claude_thinking_signature(raw: &str, opt: ClaudeValidation) -> Result<String, Error> {
    let sig = strip_cache_prefix(raw);
    check_len(sig)?;
    match sig.as_bytes()[0] {
        b'R' => {
            validate_double_layer(sig, opt)?;
            Ok(sig.to_owned())
        }
        b'E' => {
            validate_single_layer_content(sig, 1, opt)?;
            Ok(wire::std_encode(sig.as_bytes()))
        }
        _ => err(format!(
            "invalid signature: expected 'E' or 'R' prefix, got {}",
            first_char(sig)
        )),
    }
}

/// `NormalizeClaudeProviderNativeThinkingSignature`: the single-layer `E` form.
pub fn normalize_claude_provider_native_thinking_signature(raw: &str, opt: ClaudeValidation) -> Result<String, Error> {
    let sig = strip_cache_prefix(raw);
    check_len(sig)?;
    match sig.as_bytes()[0] {
        b'E' => {
            validate_single_layer_content(sig, 1, opt)?;
            Ok(sig.to_owned())
        }
        b'R' => {
            validate_double_layer(sig, opt)?;
            let decoded = STD
                .decode(sig)
                .map_err(|e| Error(format!("invalid double-layer signature: base64 decode failed: {e}")))?;
            Ok(String::from_utf8_lossy(&decoded).into_owned())
        }
        _ => err(format!(
            "invalid signature: expected 'E' or 'R' prefix, got {}",
            first_char(sig)
        )),
    }
}

/// Decodes the outer layer of an `R` signature into its inner `E` text.
fn double_layer_inner(sig: &str) -> Result<String, Error> {
    let decoded = STD
        .decode(sig)
        .map_err(|e| Error(format!("invalid double-layer signature: base64 decode failed: {e}")))?;
    match decoded.first() {
        None => err("invalid double-layer signature: empty after decode"),
        Some(&b'E') => Ok(String::from_utf8_lossy(&decoded).into_owned()),
        Some(b) => err(format!(
            "invalid double-layer signature: inner does not start with 'E', got 0x{b:02x}"
        )),
    }
}

fn validate_double_layer(sig: &str, opt: ClaudeValidation) -> Result<(), Error> {
    let inner = double_layer_inner(sig)?;
    validate_single_layer_content(&inner, 2, opt)
}

fn validate_single_layer_content(sig: &str, layers: i64, opt: ClaudeValidation) -> Result<(), Error> {
    let decoded = STD
        .decode(sig)
        .map_err(|e| Error(format!("invalid single-layer signature: base64 decode failed: {e}")))?;
    match decoded.first() {
        None => return err("invalid single-layer signature: empty after decode"),
        Some(&0x12) => {}
        Some(b) => {
            return err(format!(
                "invalid Claude signature: expected first byte 0x12, got 0x{b:02x}"
            ));
        }
    }
    if !opt.strict {
        return Ok(());
    }
    inspect_claude_signature_payload(&decoded, layers).map(|_| ())
}

/// `InspectClaudeDoubleLayerSignature`.
pub fn inspect_claude_double_layer_signature(sig: &str) -> Result<ClaudeSignatureTree, Error> {
    let inner = double_layer_inner(sig)?;
    inspect_single_layer(&inner, 2)
}

/// `InspectClaudeSingleLayerSignature`.
pub fn inspect_claude_single_layer_signature(sig: &str) -> Result<ClaudeSignatureTree, Error> {
    inspect_single_layer(sig, 1)
}

fn inspect_single_layer(sig: &str, layers: i64) -> Result<ClaudeSignatureTree, Error> {
    let decoded = STD
        .decode(sig)
        .map_err(|e| Error(format!("invalid single-layer signature: base64 decode failed: {e}")))?;
    if decoded.is_empty() {
        return err("invalid single-layer signature: empty after decode");
    }
    inspect_claude_signature_payload(&decoded, layers)
}

/// `InspectClaudeSignaturePayload`.
pub fn inspect_claude_signature_payload(payload: &[u8], layers: i64) -> Result<ClaudeSignatureTree, Error> {
    match payload.first() {
        None => return err("invalid Claude signature: empty payload"),
        Some(&0x12) => {}
        Some(b) => {
            return err(format!(
                "invalid Claude signature: expected first byte 0x12, got 0x{b:02x}"
            ));
        }
    }
    let container = extract_bytes_field(payload, 2, "top-level protobuf")?;
    let channel = extract_bytes_field(container, 1, "Claude Field 2 container")?;
    inspect_channel_block(channel, layers)
}

fn inspect_channel_block(channel: &[u8], layers: i64) -> Result<ClaudeSignatureTree, Error> {
    let mut tree = ClaudeSignatureTree {
        encoding_layers: layers,
        routing_class: "unknown".into(),
        infrastructure_class: "infra_unknown".into(),
        schema_features: "unknown_schema_features".into(),
        ..Default::default()
    };
    let mut have_channel = false;
    let mut has6 = false;
    let mut has7 = false;
    walk_fields(channel, |num, typ, raw| {
        match num {
            1 => {
                if typ != VARINT {
                    return err("invalid Claude signature: Field 2.1.1 channel_id must be varint");
                }
                tree.channel_id = varint_field(raw, "Field 2.1.1 channel_id")?;
                have_channel = true;
            }
            2 => {
                if typ != VARINT {
                    return err("invalid Claude signature: Field 2.1.2 field2 must be varint");
                }
                tree.field2 = Some(varint_field(raw, "Field 2.1.2 field2")?);
            }
            6 => {
                if typ != BYTES {
                    return err("invalid Claude signature: Field 2.1.6 model_text must be bytes");
                }
                let bytes = bytes_field(raw, "Field 2.1.6 model_text")?;
                let Ok(text) = std::str::from_utf8(bytes) else {
                    return err("invalid Claude signature: Field 2.1.6 model_text is not valid UTF-8");
                };
                tree.model_text = text.to_owned();
                has6 = true;
            }
            7 => {
                if typ != VARINT {
                    return err("invalid Claude signature: Field 2.1.7 must be varint");
                }
                varint_field(raw, "Field 2.1.7")?;
                has7 = true;
                tree.has_field7 = true;
            }
            _ => {}
        }
        Ok(())
    })?;
    if !have_channel {
        return err("invalid Claude signature: missing Field 2.1.1 channel_id");
    }
    match tree.channel_id {
        11 => tree.routing_class = "routing_class_11".into(),
        12 => tree.routing_class = "routing_class_12".into(),
        _ => {}
    }
    tree.infrastructure_class = match tree.field2 {
        None => "infra_default",
        Some(1) => "infra_aws",
        Some(2) => "infra_google",
        Some(_) => "infra_unknown",
    }
    .into();
    if has6 {
        tree.schema_features = "extended_model_tagged_schema".into();
    } else if !has7 && (70..=72).contains(&channel.len()) {
        tree.schema_features = "compact_schema".into();
    }
    if tree.channel_id == 11 {
        tree.legacy_route_hint = match (tree.field2, tree.encoding_layers) {
            (None, _) => "legacy_default_group",
            (Some(1), _) => "legacy_aws_group",
            (Some(2), 2) => "legacy_vertex_direct",
            (Some(2), 1) => "legacy_vertex_proxy",
            _ => "",
        }
        .into();
    }
    Ok(tree)
}

fn extract_bytes_field<'a>(msg: &'a [u8], field: i64, scope: &str) -> Result<&'a [u8], Error> {
    let mut value = None;
    walk_fields(msg, |num, typ, raw| {
        if num != field {
            return Ok(());
        }
        if typ != BYTES {
            return err(format!("invalid Claude signature: {scope} field {field} must be bytes"));
        }
        value = Some(bytes_field(raw, &format!("{scope} field {field}"))?);
        Ok(())
    })?;
    value.ok_or_else(|| Error(format!("invalid Claude signature: missing {scope} field {field}")))
}

/// `walkClaudeProtobufFields`: visits every field with its raw value bytes.
pub(crate) fn walk_fields<'a>(
    msg: &'a [u8],
    mut visit: impl FnMut(i64, u8, &'a [u8]) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut offset = 0;
    while offset < msg.len() {
        let ((num, typ), n) = wire::consume_tag(&msg[offset..]).map_err(|e| {
            Error(format!(
                "invalid Claude signature: malformed protobuf tag: {}",
                e.text()
            ))
        })?;
        offset += n;
        let len = wire::consume_field_value(num, typ, &msg[offset..]).map_err(|e| {
            Error(format!(
                "invalid Claude signature: malformed protobuf field {num}: {}",
                e.text()
            ))
        })?;
        visit(num, typ, &msg[offset..offset + len])?;
        offset += len;
    }
    Ok(())
}

fn decode_failure(label: &str, e: WireError) -> Error {
    Error(format!(
        "invalid Claude signature: failed to decode {label}: {}",
        e.text()
    ))
}

fn varint_field(raw: &[u8], label: &str) -> Result<u64, Error> {
    wire::consume_varint(raw)
        .map(|(v, _)| v)
        .map_err(|e| decode_failure(label, e))
}

fn bytes_field<'a>(raw: &'a [u8], label: &str) -> Result<&'a [u8], Error> {
    wire::consume_bytes(raw)
        .map(|(v, _)| v)
        .map_err(|e| decode_failure(label, e))
}

/// `ClaudeCAISSignatureInfo`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeCaisInfo {
    pub first_byte: u8,
    pub envelope_version: u64,
    pub channel_id: u64,
    pub model_text: String,
    pub block_kind: String,
    pub context_id: String,
    pub signature_len: usize,
}

/// `IsValidClaudeCAISSignature`.
pub fn is_valid_claude_cais_signature(raw: &str) -> bool {
    inspect_claude_cais_signature(raw).is_ok()
}

/// `InspectClaudeCAISSignature`.
pub fn inspect_claude_cais_signature(raw: &str) -> Result<ClaudeCaisInfo, Error> {
    let sig = strip_cache_prefix(raw);
    check_len(sig)?;
    if sig.as_bytes()[0] != b'C' {
        return err(format!(
            "invalid Claude CAIS signature: expected 'C' prefix, got {}",
            first_char(sig)
        ));
    }
    let decoded = STD
        .decode(sig)
        .map_err(|e| Error(format!("invalid Claude CAIS signature: base64 decode failed: {e}")))?;
    match decoded.first() {
        None => return err("invalid Claude CAIS signature: empty after decode"),
        Some(&0x08) => {}
        Some(b) => {
            return err(format!(
                "invalid Claude CAIS signature: expected first byte 0x08, got 0x{b:02x}"
            ));
        }
    }
    let mut info = ClaudeCaisInfo {
        first_byte: decoded[0],
        ..Default::default()
    };
    let mut container = None;
    walk_fields(&decoded, |num, typ, raw| {
        match num {
            1 => info.envelope_version = cais_varint(raw, typ, "CAIS top-level field 1 envelope version")?,
            2 => container = Some(cais_bytes(raw, typ, "CAIS top-level field 2 container")?),
            3 => {
                cais_varint(raw, typ, "CAIS top-level field 3 trailer")?;
            }
            _ => {}
        }
        Ok(())
    })?;
    let container =
        container.ok_or_else(|| Error("invalid Claude CAIS signature: missing top-level field 2 container".into()))?;
    let mut channel = None;
    let mut container_signature: &[u8] = &[];
    walk_fields(container, |num, typ, raw| {
        match num {
            1 => channel = Some(cais_bytes(raw, typ, "CAIS container field 1 channel block")?),
            5 => container_signature = cais_bytes(raw, typ, "CAIS container field 5 signature bytes")?,
            _ => {}
        }
        Ok(())
    })?;
    let channel = channel
        .ok_or_else(|| Error("invalid Claude CAIS signature: missing container field 1 channel block".into()))?;
    let (mut have_channel, mut have_signature, mut have_model) = (false, false, false);
    walk_fields(channel, |num, typ, raw| {
        match num {
            1 => {
                info.channel_id = cais_varint(raw, typ, "CAIS channel field 1 channel_id")?;
                have_channel = true;
            }
            3 => {
                cais_varint(raw, typ, "CAIS channel field 3 version")?;
            }
            5 => {
                let value = cais_bytes(raw, typ, "CAIS channel field 5 signature bytes")?;
                if value.is_empty() {
                    return err("invalid Claude CAIS signature: channel field 5 signature bytes must not be empty");
                }
                info.signature_len = value.len();
                have_signature = true;
            }
            6 => {
                let value = cais_utf8(raw, typ, "CAIS channel field 6 model_text")?;
                if !value.starts_with("claude-") {
                    return err(format!(
                        "invalid Claude CAIS signature: channel field 6 model_text must start with \"claude-\", got {}",
                        go_quote(value)
                    ));
                }
                info.model_text = value.to_owned();
                have_model = true;
            }
            7 => {
                cais_varint(raw, typ, "CAIS channel field 7")?;
            }
            8 => info.block_kind = cais_utf8(raw, typ, "CAIS channel field 8 block kind")?.to_owned(),
            11 => {
                let value = cais_utf8(raw, typ, "CAIS channel field 11 context id")?;
                if !is_canonical_uuid(value.as_bytes()) {
                    return err(format!(
                        "invalid Claude CAIS signature: channel field 11 context id must be a canonical UUID, got {}",
                        go_quote(value)
                    ));
                }
                info.context_id = value.to_owned();
            }
            _ => {}
        }
        Ok(())
    })?;
    if !have_signature && info.envelope_version >= 4 && !container_signature.is_empty() {
        info.signature_len = container_signature.len();
        have_signature = true;
    }
    if !have_channel {
        return err("invalid Claude CAIS signature: missing channel field 1 channel_id");
    }
    if !have_signature {
        return err("invalid Claude CAIS signature: missing signature bytes");
    }
    if !have_model && info.envelope_version < 4 {
        return err("invalid Claude CAIS signature: missing channel field 6 model_text");
    }
    if info.envelope_version >= 4 && info.block_kind != "thinking" && info.block_kind != "narration" {
        return err(format!(
            "invalid Claude CAQS signature: expected block kind \"thinking\" or \"narration\", got {}",
            go_quote(&info.block_kind)
        ));
    }
    Ok(info)
}

/// Go `%q` for a valid UTF-8 string (strconv.Quote).
pub(crate) fn go_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if is_go_printable(c) => out.push(c),
            c if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    out.push('"');
    out
}

/// Approximates `strconv.IsPrint` for non-ASCII runes.
fn is_go_printable(c: char) -> bool {
    !c.is_control() && !matches!(c as u32, 0xad | 0x2028 | 0x2029 | 0xfeff | 0xfff9..=0xfffb | 0xe000..=0xf8ff)
}

fn cais_varint(raw: &[u8], typ: u8, label: &str) -> Result<u64, Error> {
    if typ != VARINT {
        return err(format!("invalid Claude CAIS signature: {label} must be varint"));
    }
    varint_field(raw, label)
}

fn cais_bytes<'a>(raw: &'a [u8], typ: u8, label: &str) -> Result<&'a [u8], Error> {
    if typ != BYTES {
        return err(format!("invalid Claude CAIS signature: {label} must be bytes"));
    }
    bytes_field(raw, label)
}

fn cais_utf8<'a>(raw: &'a [u8], typ: u8, label: &str) -> Result<&'a str, Error> {
    let value = cais_bytes(raw, typ, label)?;
    std::str::from_utf8(value).map_err(|_| Error(format!("invalid Claude CAIS signature: {label} must be valid UTF-8")))
}

pub(crate) fn is_canonical_uuid(s: &[u8]) -> bool {
    s.len() == 36
        && s.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// `StripInvalidClaudeThinkingBlocks`: drops thinking blocks whose signature fails `opt`.
pub fn strip_invalid_claude_thinking_blocks(payload: &str, opt: ClaudeValidation) -> String {
    let messages = gjson::get(payload, "messages");
    if messages.kind() != Kind::Array {
        return payload.to_owned();
    }
    let mut kept_messages = Vec::new();
    let mut modified = false;
    for message in messages.array() {
        let content = message.get("content");
        if content.kind() != Kind::Array {
            kept_messages.push(message.json().to_owned());
            continue;
        }
        let mut kept_parts = Vec::new();
        let mut stripped = false;
        for part in content.array() {
            if json::go_str(&part.get("type")) == "thinking" && should_strip(&part, opt) {
                stripped = true;
                continue;
            }
            kept_parts.push(part.json().to_owned());
        }
        if stripped {
            modified = true;
            kept_messages.push(json::set_raw(message.json(), "content", &json::join_array(&kept_parts)));
        } else {
            kept_messages.push(message.json().to_owned());
        }
    }
    if !modified {
        return payload.to_owned();
    }
    json::set_raw(payload, "messages", &json::join_array(&kept_messages))
}

/// `StripInvalidClaudeThinkingBlocksAndEmptyMessages`.
pub fn strip_invalid_claude_thinking_blocks_and_empty_messages(payload: &str, opt: ClaudeValidation) -> String {
    let stripped = strip_invalid_claude_thinking_blocks(payload, opt);
    if stripped == payload {
        return stripped;
    }
    let messages = gjson::get(&stripped, "messages");
    if messages.kind() != Kind::Array {
        return stripped;
    }
    let kept: Vec<String> = messages
        .array()
        .iter()
        .filter(|m| {
            let content = m.get("content");
            !(content.kind() == Kind::Array && content.array().is_empty())
        })
        .map(|m| m.json().to_owned())
        .collect();
    json::set_raw(&stripped, "messages", &json::join_array(&kept))
}

fn should_strip(part: &gjson::Value<'_>, opt: ClaudeValidation) -> bool {
    if opt.allow_empty_signature_with_empty_text && is_empty_thinking_placeholder(part) {
        return false;
    }
    !is_valid_claude_thinking_signature(&json::go_str(&part.get("signature")), opt)
}

pub(crate) fn is_empty_thinking_placeholder(part: &gjson::Value<'_>) -> bool {
    json::go_str(&part.get("signature")).trim().is_empty() && thinking_block_text(part).trim().is_empty()
}

/// `claudeThinkingBlockText` (same rules as `thinking.GetThinkingText`).
pub fn thinking_block_text(part: &gjson::Value<'_>) -> String {
    let text = part.get("text");
    if text.kind() == Kind::String {
        return text.str().to_owned();
    }
    let thinking = part.get("thinking");
    match thinking.kind() {
        Kind::String => thinking.str().to_owned(),
        Kind::Object => {
            for key in ["text", "thinking"] {
                let inner = thinking.get(key);
                if inner.kind() == Kind::String {
                    return inner.str().to_owned();
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}
