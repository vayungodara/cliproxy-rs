//! Go's `helps/devin_payload.go` (868e137): payload rules edit the Devin request's
//! business fields as JSON. Credentials, device metadata, turn ordinals and protocol
//! flags stay opaque; encoding after the barrier adds no defaults and sanitizes nothing.

use std::collections::BTreeMap;

use cpa_common::json::{self as gj, Res};

use crate::devin_wire::pb;

/// How a field's bytes read in the JSON view.
#[derive(Clone, Copy, PartialEq)]
enum Text {
    Plain,
    /// Base64 in the view.
    Binary,
    /// Raw JSON in the view when the bytes are valid JSON.
    Json,
}

struct Field {
    num: u32,
    name: &'static str,
    wire: u8,
    repeated: bool,
    text: Text,
    children: Option<&'static [Field]>,
}

const fn field(num: u32, name: &'static str, wire: u8) -> Field {
    Field {
        num,
        name,
        wire,
        repeated: false,
        text: Text::Plain,
        children: None,
    }
}

const fn message(num: u32, name: &'static str, repeated: bool, children: &'static [Field]) -> Field {
    Field {
        repeated,
        children: Some(children),
        ..field(num, name, pb::BYTES)
    }
}

const TOOL_CALLS: &[Field] = &[
    field(1, "id", pb::BYTES),
    field(2, "name", pb::BYTES),
    field(3, "arguments", pb::BYTES),
];
const IMAGES: &[Field] = &[field(1, "data", pb::BYTES), field(2, "mime_type", pb::BYTES)];
const PROMPTS: &[Field] = &[
    field(1, "id", pb::BYTES),
    field(2, "source", pb::VARINT),
    field(3, "content", pb::BYTES),
    message(6, "tool_calls", true, TOOL_CALLS),
    field(7, "tool_call_id", pb::BYTES),
    message(10, "images", true, IMAGES),
    field(11, "thinking", pb::BYTES),
    Field {
        text: Text::Binary,
        ..field(12, "signature", pb::BYTES)
    },
    field(18, "signature_type", pb::BYTES),
];
const COMPLETION_CONFIG: &[Field] = &[
    field(1, "enabled", pb::VARINT),
    field(2, "max_tokens", pb::VARINT),
    field(3, "parameter_3", pb::VARINT),
    field(5, "temperature", pb::FIXED64),
    field(7, "top_k", pb::VARINT),
    field(8, "top_p", pb::FIXED64),
];
const TOOLS: &[Field] = &[
    field(1, "name", pb::BYTES),
    field(2, "description", pb::BYTES),
    Field {
        text: Text::Json,
        ..field(3, "parameters", pb::BYTES)
    },
];
/// `devinPayloadFields`, in field-number order (the order Go encodes them).
const FIELDS: &[Field] = &[
    field(2, "system_prompt", pb::BYTES),
    message(3, "prompts", true, PROMPTS),
    message(8, "completion_config", false, COMPLETION_CONFIG),
    message(10, "tools", true, TOOLS),
    field(16, "cascade_id", pb::BYTES),
    field(21, "model", pb::BYTES),
];

/// A decoded value, marshalled as Go's `json.Marshal` writes the `map[string]any`.
enum View {
    Null,
    Str(Vec<u8>),
    /// `json.RawMessage`.
    Raw(Vec<u8>),
    Uint(u64),
    Float(f64),
    Object(BTreeMap<&'static str, View>),
    List(Vec<View>),
}

impl View {
    /// Fails on NaN or an infinity with Go's `UnsupportedValueError` text.
    fn write(&self, out: &mut Vec<u8>) -> Result<(), String> {
        match self {
            View::Null => out.extend_from_slice(b"null"),
            View::Str(s) => gj::marshal_str(out, s, true),
            View::Raw(raw) => out.extend_from_slice(&gj::compact(raw, true)),
            View::Uint(n) => out.extend_from_slice(n.to_string().as_bytes()),
            View::Float(f) => match gj::json_float(*f) {
                Some(text) => out.extend_from_slice(text.as_bytes()),
                None => {
                    let text = match f {
                        f if f.is_nan() => "NaN",
                        f if *f > 0.0 => "+Inf",
                        _ => "-Inf",
                    };
                    return Err(format!("json: unsupported value: {text}"));
                }
            },
            View::Object(map) => {
                out.push(b'{');
                for (i, (key, value)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    gj::marshal_str(out, key.as_bytes(), true);
                    out.push(b':');
                    value.write(out)?;
                }
                out.push(b'}');
            }
            View::List(items) => {
                out.push(b'[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    item.write(out)?;
                }
                out.push(b']');
            }
        }
        Ok(())
    }
}

/// `FinalizeDevinPayload`: the wire request to send and the JSON view Go logs in its
/// place. The wire is rebuilt only when `finalize` changed the view.
pub(crate) fn finalize(wire: &[u8], finalize: impl FnOnce(&[u8]) -> Vec<u8>) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut view = Vec::with_capacity(wire.len() + wire.len() / 4);
    View::Object(decode(wire, FIELDS)?).write(&mut view)?;
    let configured = finalize(&view);
    if configured == view {
        return Ok((wire.to_vec(), configured));
    }
    let out = encode(wire, &gj::parse(&configured), FIELDS)?;
    Ok((out, configured))
}

fn decode(mut wire: &[u8], fields: &'static [Field]) -> Result<BTreeMap<&'static str, View>, String> {
    let mut out = BTreeMap::new();
    while !wire.is_empty() {
        let (num, kind, tag) = pb::consume_tag(wire).map_err(|e| e.to_string())?;
        wire = &wire[tag..];
        let n = pb::consume_field_value(num, kind, wire).map_err(|e| e.to_string())?;
        if let Some(field) = fields.iter().find(|f| i64::from(f.num) == num) {
            let value = match kind {
                pb::BYTES => {
                    let (data, _) = pb::consume_bytes(wire).map_err(|e| e.to_string())?;
                    match (field.children, field.text) {
                        (Some(children), _) => View::Object(decode(data, children)?),
                        (None, Text::Binary) => View::Str(
                            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data).into_bytes(),
                        ),
                        (None, Text::Json) if gj::std_valid(data) => View::Raw(data.to_vec()),
                        _ => View::Str(data.to_vec()),
                    }
                }
                pb::VARINT => View::Uint(pb::consume_varint(wire).map_err(|e| e.to_string())?.0),
                pb::FIXED64 => View::Float(f64::from_bits(pb::consume_fixed64(wire).map_err(|e| e.to_string())?.0)),
                _ => View::Null,
            };
            if field.repeated {
                match out.entry(field.name).or_insert_with(|| View::List(vec![])) {
                    View::List(items) => items.push(value),
                    _ => unreachable!("repeated fields hold lists"),
                }
            } else {
                out.insert(field.name, value);
            }
        }
        wire = &wire[n..];
    }
    Ok(out)
}

/// `encodeDevinPayload`: the fields outside the view, in their original order, then the
/// view's fields in field-number order. A nested message keeps only its view fields.
fn encode(mut original: &[u8], body: &Res<'_>, fields: &'static [Field]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(original.len());
    while !original.is_empty() {
        let (num, kind, tag) = pb::consume_tag(original).map_err(|e| e.to_string())?;
        let n = pb::consume_field_value(num, kind, &original[tag..]).map_err(|e| e.to_string())?;
        if !fields.iter().any(|f| i64::from(f.num) == num) {
            out.extend_from_slice(&original[..tag + n]);
        }
        original = &original[tag + n..];
    }
    for field in fields {
        let value = body.get(field.name);
        if !value.exists() || value.kind == gj::Kind::Null {
            continue;
        }
        let values = if field.repeated { value.array() } else { vec![value] };
        for item in values {
            match field.wire {
                pb::BYTES => {
                    let data = match (field.children, field.text) {
                        (Some(children), _) => encode(&[], &item, children)?,
                        (None, Text::Binary) => cpa_common::signature::std_base64_decode(&item.bytes())
                            .map_err(|e| format!("Devin payload {}: {e}", field.name))?,
                        (None, Text::Json) => item.raw().to_vec(),
                        (None, Text::Plain) => item.bytes().into_owned(),
                    };
                    pb::bytes(&mut out, field.num, &data);
                }
                pb::VARINT => pb::uint(&mut out, field.num, item.uint()),
                _ => pb::fixed64(&mut out, field.num, item.float().to_bits()),
            }
        }
    }
    Ok(out)
}

/// `DevinPayloadDefaultsSource`: the view fields the client's Interactions request set,
/// so `default` rules replace built-in values but never the caller's.
pub(crate) fn defaults_source(native: &[u8], interactions: &[u8]) -> Vec<u8> {
    const SOURCES: [(&str, &[&str]); 9] = [
        ("model", &["model"]),
        ("system_prompt", &["system_instruction", "systemInstruction"]),
        ("prompts", &["input"]),
        ("tools", &["tools"]),
        (
            "cascade_id",
            &["session_id", "sessionId", "conversation_id", "previous_interaction_id"],
        ),
        (
            "completion_config.max_tokens",
            &[
                "generation_config.max_output_tokens",
                "generationConfig.max_output_tokens",
            ],
        ),
        (
            "completion_config.temperature",
            &[
                "generation_config.temperature",
                "generationConfig.temperature",
                "temperature",
            ],
        ),
        (
            "completion_config.top_k",
            &["generation_config.top_k", "generationConfig.top_k"],
        ),
        (
            "completion_config.top_p",
            &["generation_config.top_p", "generationConfig.top_p"],
        ),
    ];
    let mut out = b"{}".to_vec();
    for (target, sources) in SOURCES {
        let Some(mut value) = sources
            .iter()
            .map(|source| gj::get(interactions, *source))
            .find(Res::exists)
        else {
            continue;
        };
        if matches!(target, "prompts" | "tools") {
            value = gj::get(native, target);
        }
        if value.exists() {
            gj::set_raw(&mut out, target, value.raw());
        }
    }
    out
}
