//! Request-body edits the Gemini-family executors apply after translation
//! (gemini_executor.go, helps/gemini_content_turns.go, helps/payload_mutations.go).
//!
//! Every edit is byte-level through `cpa_common::json`, so key order, escaping and
//! malformed input behave like Go's gjson/sjson.

use std::io::Read;
use std::sync::OnceLock;

use cpa_common::json::{self as gj, Kind};

const EMPTY_USER_TURN: &[u8] = br#"{"role":"user","parts":[{"text":""}]}"#;

/// `helps.SetStringIfDifferent`.
pub(crate) fn set_str_if_different(body: Vec<u8>, path: &str, value: &str) -> Vec<u8> {
    let current = gj::get(&body, path);
    if current.kind == Kind::String && *current.bytes() == *value.as_bytes() {
        return body;
    }
    gj::try_set_str(&body, path, value).unwrap_or(body)
}

/// `helps.SetBoolIfDifferent`.
pub(crate) fn set_bool_if_different(body: Vec<u8>, path: &str, value: bool) -> Vec<u8> {
    let current = gj::get(&body, path).kind;
    if (value && current == Kind::True) || (!value && current == Kind::False) {
        return body;
    }
    gj::try_set_raw(&body, path, if value { "true" } else { "false" }).unwrap_or(body)
}

/// `sjson.DeleteBytes` whose error Go discards. A failed delete leaves the body as is.
pub(crate) fn delete(mut body: Vec<u8>, path: &str) -> Vec<u8> {
    gj::delete(&mut body, path);
    body
}

/// `helps.EnsureGeminiLeadingUserContent`: a leading `model` turn gets an empty user
/// turn in front of it.
pub(crate) fn ensure_leading_user_content(body: Vec<u8>, path: &str) -> Vec<u8> {
    if *gj::get(&body, &format!("{path}.0.role")).bytes() != *b"model" {
        return body;
    }
    let contents = gj::get(&body, path);
    if !contents.is_array() {
        return body;
    }
    let items = contents.array();
    if items.is_empty() {
        return body;
    }
    let mut raw: Vec<&[u8]> = Vec::with_capacity(items.len() + 1);
    raw.push(EMPTY_USER_TURN);
    raw.extend(items.iter().map(|c| c.raw()));
    let joined = gj::join(&raw);
    gj::try_set_raw(&body, path, joined).unwrap_or(body)
}

/// `helps.EnsureGeminiTrailingUserContent`: a final `model`/`assistant` turn gets an
/// empty user turn after it, unless it carries a function response.
pub(crate) fn ensure_trailing_user_content(body: Vec<u8>, path: &str) -> Vec<u8> {
    let contents = gj::get(&body, path);
    if !contents.is_array() {
        return body;
    }
    let items = contents.array();
    let Some(last) = items.last() else {
        return body;
    };
    let role = last.get("role").bytes();
    let has_function_response = {
        let parts = last.get("parts");
        parts.is_array() && parts.array().iter().any(|p| p.get("functionResponse").exists())
    };
    if !matches!(&*role, b"model" | b"assistant") || has_function_response {
        return body;
    }
    let mut raw: Vec<&[u8]> = items.iter().map(|c| c.raw()).collect();
    raw.push(EMPTY_USER_TURN);
    let joined = gj::join(&raw);
    gj::try_set_raw(&body, path, joined).unwrap_or(body)
}

/// `capGeminiMaxOutputTokens`: a numeric `maxOutputTokens` above the registry's output
/// limit (or, without one, its completion limit) is lowered to that limit.
pub(crate) fn cap_max_output_tokens(mut body: Vec<u8>, model: &str) -> Vec<u8> {
    let current = gj::get(&body, "generationConfig.maxOutputTokens");
    if current.kind != Kind::Number {
        return body;
    }
    let Some(info) = cpa_core::registry::lookup_model(model, Some("gemini")) else {
        return body;
    };
    let field = |k: &str| info.raw.get(k).and_then(serde_json::Value::as_i64).unwrap_or(0);
    let mut limit = field("outputTokenLimit");
    if limit <= 0 {
        limit = field("max_completion_tokens");
    }
    if limit <= 0 || current.int() <= limit {
        return body;
    }
    gj::set_int(&mut body, "generationConfig.maxOutputTokens", limit);
    body
}

const IMAGE_PREVIEW_MODEL: &str = "gemini-2.5-flash-image-preview";
const IMAGE_PROMPT: &str = r#"{"text": "Based on the following requirements, create an image within the uploaded picture. The new content *MUST* completely cover the entire area of the original picture, maintaining its exact proportions, and *NO* blank areas should appear."}"#;

/// `fixGeminiImageAspectRatio`: for the image preview model, an aspect ratio without
/// any inline image becomes a white canvas of that ratio placed before the first turn's
/// parts; `imageConfig` is always removed.
pub(crate) fn fix_image_aspect_ratio(model: &str, mut body: Vec<u8>) -> Vec<u8> {
    if model != IMAGE_PREVIEW_MODEL {
        return body;
    }
    let ratio = gj::get(&body, "generationConfig.imageConfig.aspectRatio");
    if !ratio.exists() {
        return body;
    }
    let contents = gj::get(&body, "contents").array();
    if !contents.is_empty() {
        let has_inline = contents
            .iter()
            .any(|c| c.get("parts").array().iter().any(|p| p.get("inlineData").exists()));
        if !has_inline {
            let mut part = br#"{"inlineData":{"mime_type":"image/png","data":""}}"#.to_vec();
            gj::set_str(&mut part, "inlineData.data", white_image_base64(&ratio.str()));
            let mut parts = b"[]".to_vec();
            gj::set_raw(&mut parts, "-1", IMAGE_PROMPT);
            gj::set_raw(&mut parts, "-1", &part);
            for existing in contents[0].get("parts").array() {
                gj::set_raw(&mut parts, "-1", existing.raw());
            }
            let mut next = body.clone();
            gj::set_raw(&mut next, "contents.0.parts", &parts);
            gj::set_raw(&mut next, "generationConfig.responseModalities", r#"["IMAGE", "TEXT"]"#);
            body = next;
        }
    }
    gj::delete(&mut body, "generationConfig.imageConfig");
    body
}

/// `util.CreateWhiteImageBase64`: a white PNG of the ratio's size (1024x1024 for
/// unknown ratios), base64 encoded.
///
/// ponytail: the PNGs are Go's own `png.Encode` output, embedded (1.3 KB gzipped) rather
/// than re-encoded, because Rust's deflate would not reproduce Go's bytes. Regenerate
/// with tests/reference/gemini/white_png.go.txt.
fn white_image_base64(ratio: &str) -> String {
    let (width, height) = match ratio {
        "2:3" => (832, 1248),
        "3:2" => (1248, 832),
        "3:4" => (864, 1184),
        "4:3" => (1184, 864),
        "4:5" => (896, 1152),
        "5:4" => (1152, 896),
        "9:16" => (768, 1344),
        "16:9" => (1344, 768),
        "21:9" => (1536, 672),
        _ => (1024, 1024),
    };
    let png = white_pngs()
        .iter()
        .find(|(w, h, _)| (*w, *h) == (width, height))
        .map(|(_, _, png)| png.as_slice())
        .unwrap_or_default();
    base64::engine::Engine::encode(&base64::engine::general_purpose::STANDARD, png)
}

/// `(width, height, png)` records: u16 width, u16 height, u32 length, bytes (big endian).
fn white_pngs() -> &'static [(u16, u16, Vec<u8>)] {
    static PNGS: OnceLock<Vec<(u16, u16, Vec<u8>)>> = OnceLock::new();
    PNGS.get_or_init(|| {
        let gz = include_bytes!("gemini_white_png.bin.gz");
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(&gz[..])
            .read_to_end(&mut raw)
            .expect("embedded white PNGs");
        let mut out = Vec::new();
        let mut rest = raw.as_slice();
        while rest.len() >= 8 {
            let width = u16::from_be_bytes([rest[0], rest[1]]);
            let height = u16::from_be_bytes([rest[2], rest[3]]);
            let len = u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
            out.push((width, height, rest[8..8 + len].to_vec()));
            rest = &rest[8 + len..];
        }
        out
    })
}

/// `sanitizeGeminiInteractionsUnsupportedInputIDs`: function calls carry `id` (copied
/// from `call_id`) and never `call_id`; other steps and every content part lose `id`.
pub(crate) fn sanitize_interactions_input_ids(mut body: Vec<u8>) -> Vec<u8> {
    let input = gj::get(&body, "input");
    if !input.is_array() {
        return body;
    }
    let items: Vec<gj::Res<'static>> = input.array().into_iter().map(gj::Res::into_owned).collect();
    for (i, item) in items.iter().enumerate() {
        if *item.get("type").bytes() == *b"function_call" {
            let call_id = item.get("call_id");
            if !item.get("id").exists() && call_id.exists() {
                gj::set_str(&mut body, &format!("input.{i}.id"), call_id.bytes());
            }
            if call_id.exists() {
                gj::delete(&mut body, &format!("input.{i}.call_id"));
            }
        } else if item.get("id").exists() {
            gj::delete(&mut body, &format!("input.{i}.id"));
        }
        let content = item.get("content");
        if !content.is_array() {
            continue;
        }
        for (j, part) in content.array().iter().enumerate() {
            if part.get("id").exists() {
                gj::delete(&mut body, &format!("input.{i}.content.{j}.id"));
            }
        }
    }
    body
}
