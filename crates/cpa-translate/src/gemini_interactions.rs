//! Gemini Interactions <-> Gemini generateContent, and the Interactions passthrough
//! (internal/translator/gemini/interactions): ConvertInteractionsRequestToGemini,
//! ConvertGeminiRequestToInteractions, ConvertInteractionsRequestToInteractions and the
//! shared conversions in interactions_gemini_common.go. The response sides live in
//! [`crate::gemini_interactions_response`].

use std::collections::BTreeMap;

use cpa_common::json::{self as gj, GoValue, Kind, Res};

use crate::common::{go_lower, go_runes, go_upper, normalize_openai_file_data, trim_space};
use crate::gemini::{reorder_user_parts, set_function_response_result};
use crate::gemini_chat_request::content_node;
use crate::gemini_interactions_response as response;
use crate::{Registered, stream};

/// Interactions client, Interactions upstream: everything passes through.
pub static PASSTHROUGH: Registered = registered!(
    Interactions -> Interactions,
    request: |_, body| Ok(body.to_vec()),
    non_stream: |_, body| Ok(body.to_vec()),
    go_stream: passthrough_stream,
    token_count: None,
);

/// Interactions client, Gemini upstream.
pub static INTERACTIONS_TO_GEMINI: Registered = registered!(
    Interactions -> Gemini,
    request: |ctx, body| Ok(interactions_to_gemini(ctx.model, body)),
    non_stream: response::gemini_to_interactions_non_stream,
    go_stream: response::gemini_to_interactions_stream,
    token_count: None,
);

/// Gemini client, Interactions upstream.
pub static GEMINI_TO_INTERACTIONS: Registered = registered!(
    Gemini -> Interactions,
    request: |ctx, body| Ok(gemini_to_interactions(ctx.model, body, ctx.stream)),
    non_stream: response::interactions_to_gemini_non_stream,
    go_stream: response::interactions_to_gemini_stream,
    token_count: None,
);

fn passthrough_stream(_: &crate::ResponseCtx<'_>) -> Box<dyn stream::GoStream> {
    Box::new(Passthrough)
}

/// ConvertInteractionsResponsePassthrough: every non-empty line as is.
struct Passthrough;

impl stream::GoStream for Passthrough {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, crate::Error> {
        Ok(if line.is_empty() { vec![] } else { vec![line.to_vec()] })
    }
}

// ---------------------------------------------------------------------------------------
// Small Go helpers

/// firstNonEmptyString: the first value that is not blank, trimmed.
pub(crate) fn first_trimmed(values: &[&[u8]]) -> Vec<u8> {
    values
        .iter()
        .map(|v| trim_space(v))
        .find(|v| !v.is_empty())
        .unwrap_or_default()
        .to_vec()
}

/// firstExistingPath / firstExistingInteractionResult.
pub(crate) fn first_existing<'a>(root: &Res<'a>, paths: &[&str]) -> Res<'a> {
    paths.iter().map(|p| root.get(*p)).find(Res::exists).unwrap_or_default()
}

fn string(res: &Res<'_>) -> Vec<u8> {
    res.bytes().into_owned()
}

/// gjson.Parse of `fmt.Sprintf(format, %q...)`: Go quotes each value, then reads it back
/// with gjson (escapes gjson does not know end the string, as in Go).
fn requoted(first: (&str, &[u8]), second: (&str, &[u8])) -> Res<'static> {
    let text = format!(
        "{{{}:{},{}:{}}}",
        cpa_common::gostr::quote(first.0),
        cpa_common::gostr::quote(first.1),
        cpa_common::gostr::quote(second.0),
        cpa_common::gostr::quote(second.1),
    );
    gj::parse(text.as_bytes()).into_owned()
}

// ---------------------------------------------------------------------------------------
// Gemini parts (interactions_gemini_common.go)

/// geminiTextPartJSON.
pub(crate) fn text_part(text: &[u8], thought: bool) -> Vec<u8> {
    let mut part = br#"{"text":""}"#.to_vec();
    gj::set_str(&mut part, "text", text);
    if thought {
        gj::set_bool(&mut part, "thought", true);
    }
    part
}

/// geminiInlineDataPartJSON.
fn inline_data_part(inline: &Res<'_>) -> Option<Vec<u8>> {
    let mut mime = string(&inline.get("mimeType"));
    if mime.is_empty() {
        mime = string(&inline.get("mime_type"));
    }
    let data = string(&inline.get("data"));
    if mime.is_empty() || data.is_empty() {
        return None;
    }
    let mut part = br#"{"inlineData":{"mimeType":"","data":""}}"#.to_vec();
    gj::set_str(&mut part, "inlineData.mimeType", &mime);
    gj::set_str(&mut part, "inlineData.data", &data);
    Some(part)
}

/// geminiFileDataPartJSON.
fn file_data_part(file: &Res<'_>) -> Option<Vec<u8>> {
    let mut mime = string(&file.get("mimeType"));
    if mime.is_empty() {
        mime = string(&file.get("mime_type"));
    }
    let mut uri = string(&file.get("fileUri"));
    if uri.is_empty() {
        uri = string(&file.get("file_uri"));
    }
    if mime.is_empty() || uri.is_empty() {
        return None;
    }
    let mut part = br#"{"fileData":{"mimeType":"","fileUri":""}}"#.to_vec();
    gj::set_str(&mut part, "fileData.mimeType", &mime);
    gj::set_str(&mut part, "fileData.fileUri", &uri);
    Some(part)
}

/// geminiInlineDataPartJSON(gjson.Parse(`{"mime_type":%q,"data":%q}`)).
fn quoted_inline_part(mime: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    inline_data_part(&requoted(("mime_type", mime), ("data", data)))
}

/// geminiInlineDataPartFromDataURL.
fn inline_part_from_data_url(url: &[u8]) -> Option<Vec<u8>> {
    let payload = url.strip_prefix(b"data:")?;
    let semi = payload.iter().position(|&c| c == b';')?;
    let data = payload[semi + 1..].strip_prefix(b"base64,")?;
    quoted_inline_part(&payload[..semi], data)
}

/// interactionsInputAudioMimeType.
fn input_audio_mime(format: &[u8]) -> &'static [u8] {
    match go_lower(trim_space(format)).as_slice() {
        b"wav" => b"audio/wav",
        b"flac" => b"audio/flac",
        b"opus" => b"audio/opus",
        b"pcm16" => b"audio/pcm",
        _ => b"audio/mpeg",
    }
}

/// geminiInlineDataToInteractionsContent.
pub(crate) fn inline_to_interactions_content(mime: &[u8], data: &[u8]) -> Vec<u8> {
    let lower = go_lower(mime);
    let kind = if lower.starts_with(b"image/") {
        "image"
    } else if lower.starts_with(b"audio/") {
        "audio"
    } else if lower.starts_with(b"video/") {
        "video"
    } else {
        "document"
    };
    let mut item = br#"{"type":"","mime_type":"","data":""}"#.to_vec();
    gj::set_str(&mut item, "type", kind);
    gj::set_str(&mut item, "mime_type", mime);
    gj::set_str(&mut item, "data", data);
    item
}

/// interactionsContentPartToGeminiPart.
pub(crate) fn content_part_to_gemini(part: &Res<'_>, thought: bool) -> Option<Vec<u8>> {
    let text = part.get("text");
    if text.exists() {
        return Some(text_part(&text.bytes(), thought));
    }
    for path in ["inline_data", "inlineData"] {
        let inline = part.get(path);
        if inline.exists() {
            return inline_data_part(&inline);
        }
    }
    match go_lower(trim_space(&part.get("type").bytes())).as_slice() {
        b"image" | b"audio" | b"video" | b"document" => {
            let mime = part.get("mime_type");
            if mime.exists() || part.get("mimeType").exists() {
                let mut mime_type = string(&mime);
                if mime_type.is_empty() {
                    mime_type = string(&part.get("mimeType"));
                }
                let data = string(&part.get("data"));
                if !data.is_empty() {
                    return quoted_inline_part(&mime_type, &data);
                }
            }
            let uri = part.get("file_uri");
            if uri.exists() || part.get("fileUri").exists() {
                let mut file_uri = string(&uri);
                if file_uri.is_empty() {
                    file_uri = string(&part.get("fileUri"));
                }
                let mut mime_type = string(&part.get("mime_type"));
                if mime_type.is_empty() {
                    mime_type = string(&part.get("mimeType"));
                }
                return file_data_part(&requoted(("mimeType", &mime_type), ("fileUri", &file_uri)));
            }
            let url = part.get("url");
            if url.exists() {
                return inline_part_from_data_url(&url.bytes());
            }
            None
        }
        b"image_url" => inline_part_from_data_url(&part.get("image_url.url").bytes()),
        b"input_audio" => quoted_inline_part(
            input_audio_mime(&part.get("input_audio.format").bytes()),
            &part.get("input_audio.data").bytes(),
        ),
        b"file" => {
            let filename = string(&part.get("file.filename"));
            let file_data = string(&part.get("file.file_data"));
            let (mime, data) = normalize_openai_file_data(&filename, b"", &file_data)?;
            quoted_inline_part(&mime, &data)
        }
        _ => None,
    }
}

/// interactionsNativeGeminiPart.
fn native_part(part: &Res<'_>) -> Option<Vec<u8>> {
    if part.get("text").exists() || part.get("functionCall").exists() || part.get("functionResponse").exists() {
        return Some(part.raw.to_vec());
    }
    for (path, file) in [
        ("inlineData", false),
        ("fileData", true),
        ("inline_data", false),
        ("file_data", true),
    ] {
        let value = part.get(path);
        if value.exists() {
            return if file {
                file_data_part(&value)
            } else {
                inline_data_part(&value)
            };
        }
    }
    None
}

/// Parts of a content node plus `extra` (appendGeminiContentPart(s)); unchanged when
/// sjson rejects the edit.
fn with_parts(content: &[u8], extra: &[Vec<u8>], reorder: bool) -> Vec<u8> {
    let mut parts: Vec<Vec<u8>> = gj::get(content, "parts")
        .array()
        .iter()
        .map(|p| p.raw.to_vec())
        .collect();
    parts.extend(extra.iter().cloned());
    if reorder {
        parts = reorder_user_parts(parts);
    }
    gj::try_set_raw(content, "parts", gj::join(&parts)).unwrap_or_else(|_| content.to_vec())
}

// ---------------------------------------------------------------------------------------
// Interactions request -> Gemini request

/// ConvertInteractionsRequestToGemini.
pub(crate) fn interactions_to_gemini(model: &str, raw: &[u8]) -> Vec<u8> {
    let root = gj::parse(raw);
    let mut out = br#"{"model":"","contents":[]}"#.to_vec();
    if !model.is_empty() && root.get("model").exists() {
        gj::set_str(&mut out, "model", model);
    }
    copy_system_instruction(&mut out, &root);
    copy_generation_config(&mut out, &root);
    copy_response_modalities(&mut out, &root);
    copy_tools(&mut out, &root);
    copy_tool_choice(&mut out, &root);
    let tier = root.get("service_tier");
    if tier.kind == Kind::String {
        gj::set_str(&mut out, "service_tier", tier.bytes());
    }
    let items = input_contents(&root.get("input"));
    gj::set_items(&mut out, "contents", &items);
    out
}

/// copyInteractionsSystemInstruction.
pub(crate) fn copy_system_instruction(out: &mut Vec<u8>, root: &Res<'_>) {
    let sys = root.get("system_instruction");
    if !sys.exists() {
        return;
    }
    let text = sys.get("text");
    let text = if sys.kind == Kind::String {
        Some(sys.bytes())
    } else if text.exists() && !sys.get("parts").exists() {
        Some(text.bytes())
    } else {
        None
    };
    match text {
        Some(text) => {
            let mut instruction = br#"{"parts":[{"text":""}]}"#.to_vec();
            gj::set_str(&mut instruction, "parts.0.text", text);
            gj::set_raw(out, "systemInstruction", instruction);
        }
        None => {
            gj::set_raw(out, "systemInstruction", &sys.raw);
        }
    }
}

/// copyInteractionsGenerationConfig.
pub(crate) fn copy_generation_config(out: &mut Vec<u8>, root: &Res<'_>) {
    let cfg = root.get("generation_config");
    if cfg.exists() {
        gj::set_raw(out, "generationConfig", rename_keys(&cfg.raw, to_camel));
    } else {
        let cfg = root.get("generationConfig");
        if !cfg.exists() {
            return;
        }
        gj::set_raw(out, "generationConfig", &cfg.raw);
    }
    normalize_generation_config(out);
}

/// Moves `generationConfig.<from>` to `generationConfig.thinkingConfig.<to>`.
fn move_into_thinking_config(out: &mut Vec<u8>, from: &str, to: &str) {
    let value = gj::get(out, &format!("generationConfig.{from}"));
    if value.exists() {
        let raw = value.raw.into_owned();
        gj::set_raw(out, &format!("generationConfig.thinkingConfig.{to}"), raw);
        gj::delete(out, &format!("generationConfig.{from}"));
    }
}

/// normalizeInteractionsGenerationConfig.
fn normalize_generation_config(out: &mut Vec<u8>) {
    if gj::get(out, "generationConfig.toolChoice").exists() {
        gj::delete(out, "generationConfig.toolChoice");
    }
    move_into_thinking_config(out, "thinkingLevel", "thinkingLevel");
    move_into_thinking_config(out, "thinkingBudget", "thinkingBudget");
    move_into_thinking_config(out, "includeThoughts", "includeThoughts");
    let summaries = gj::get(out, "generationConfig.thinkingSummaries");
    if summaries.exists() {
        let include = (summaries.kind == Kind::String)
            .then(|| match go_lower(trim_space(&summaries.bytes())).as_slice() {
                b"auto" => Some(true),
                b"none" => Some(false),
                _ => None,
            })
            .flatten();
        if let Some(include) = include {
            gj::set_bool(out, "generationConfig.thinkingConfig.includeThoughts", include);
        }
        gj::delete(out, "generationConfig.thinkingSummaries");
    }
}

/// copyInteractionsResponseModalities.
pub(crate) fn copy_response_modalities(out: &mut Vec<u8>, root: &Res<'_>) {
    let mut mods = root.get("response_modalities");
    if !mods.exists() {
        mods = root.get("responseModalities");
    }
    if !mods.exists() || !mods.is_array() {
        return;
    }
    let mut modalities: Vec<&str> = vec![];
    mods.each(|_, m| {
        match go_lower(trim_space(&m.bytes())).as_slice() {
            b"text" => modalities.push("TEXT"),
            b"image" => modalities.push("IMAGE"),
            b"audio" => modalities.push("AUDIO"),
            _ => {}
        }
        true
    });
    if !modalities.is_empty() {
        gj::set_strs(out, "generationConfig.responseModalities", &modalities);
    }
}

/// copyInteractionsToolChoice.
fn copy_tool_choice(out: &mut Vec<u8>, root: &Res<'_>) {
    let choice = first_existing(
        root,
        &[
            "tool_choice",
            "generation_config.tool_choice",
            "generationConfig.toolChoice",
        ],
    );
    if !choice.exists() {
        return;
    }
    let mut allowed: Vec<Vec<u8>> = vec![];
    let mode = if choice.kind == Kind::String {
        match go_lower(trim_space(&choice.bytes())).as_slice() {
            b"none" => "NONE",
            b"auto" => "AUTO",
            b"required" | b"any" => "ANY",
            _ => "",
        }
    } else if choice.is_object() {
        let mut name_at = |path: &str| {
            let name = trim_space(&choice.get(path).bytes()).to_vec();
            if !name.is_empty() {
                allowed.push(name);
            }
        };
        match go_lower(trim_space(&choice.get("type").bytes())).as_slice() {
            b"none" => "NONE",
            b"auto" => "AUTO",
            b"required" | b"any" => "ANY",
            b"function" => {
                name_at("function.name");
                "ANY"
            }
            b"tool" => {
                name_at("name");
                "ANY"
            }
            _ => "",
        }
    } else {
        ""
    };
    if mode.is_empty() {
        return;
    }
    gj::set_str(out, "toolConfig.functionCallingConfig.mode", mode);
    if !allowed.is_empty() {
        gj::set_strs(out, "toolConfig.functionCallingConfig.allowedFunctionNames", &allowed);
    }
}

/// toCamelCase: `_` separators dropped and the byte after each uppercased (strings.ToUpper
/// of that single byte, so a non-ASCII lead byte becomes U+FFFD).
fn to_camel(key: &[u8]) -> Vec<u8> {
    let mut parts = key.split(|&c| c == b'_');
    let mut out = parts.next().unwrap_or_default().to_vec();
    for part in parts {
        if part.is_empty() {
            continue;
        }
        out.extend_from_slice(&go_upper(&part[..1]));
        out.extend_from_slice(&part[1..]);
    }
    out
}

/// toSnakeCase: `_` before every ASCII capital after the first rune, then strings.ToLower
/// (ranging over the key turns invalid bytes into U+FFFD).
fn to_snake(key: &[u8]) -> Vec<u8> {
    let mut snake = String::with_capacity(key.len() + 4);
    for (i, c) in go_runes(key).enumerate() {
        if i > 0 && c.is_ascii_uppercase() {
            snake.push('_');
        }
        snake.push(c);
    }
    go_lower(snake.as_bytes())
}

/// convertSnakeCaseKeysToCamelCase / convertCamelCaseKeysToSnakeCase: every object key
/// renamed, rebuilt with sjson sets on unescaped paths (arrays through `.-1` appends).
fn rename_keys(raw: &[u8], rename: fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    let root = gj::parse(raw);
    if !root.exists() {
        return raw.to_vec();
    }
    let mut out = b"{}".to_vec();
    copy_renamed(&mut out, b"", &root, rename);
    out
}

fn copy_renamed(out: &mut Vec<u8>, path: &[u8], node: &Res<'_>, rename: fn(&[u8]) -> Vec<u8>) {
    if node.is_object() {
        node.each(|key, value| {
            let mut child = path.to_vec();
            if !child.is_empty() {
                child.push(b'.');
            }
            child.extend_from_slice(&rename(&key.bytes()));
            copy_renamed(out, &child, &value, rename);
            true
        });
    } else if node.is_array() {
        let child = [path, b".-1"].concat();
        node.each(|_, value| {
            copy_renamed(out, &child, &value, rename);
            true
        });
    } else {
        gj::set_raw(out, path, &node.raw);
    }
}

/// A value `json.Marshal` writes the way the translators build them: `json.RawMessage`,
/// Go strings, `map[string]any` (sorted keys), slices, and decoded `any` values.
enum Marshal {
    Raw(Vec<u8>),
    Str(Vec<u8>),
    Map(BTreeMap<Vec<u8>, Marshal>),
    List(Vec<Marshal>),
    Go(GoValue),
}

impl Marshal {
    fn map<const N: usize>(entries: [(&str, Marshal); N]) -> Self {
        Self::Map(entries.into_iter().map(|(k, v)| (k.as_bytes().to_vec(), v)).collect())
    }

    /// `None` where Marshal fails (a RawMessage that is not valid JSON).
    fn marshal(&self, out: &mut Vec<u8>) -> Option<()> {
        match self {
            Self::Raw(raw) => {
                if !gj::std_valid(raw) {
                    return None;
                }
                out.extend_from_slice(&gj::compact(raw, true));
            }
            Self::Str(s) => gj::marshal_str(out, s, true),
            Self::Go(value) => out.extend_from_slice(&value.marshal()),
            Self::List(items) => {
                out.push(b'[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    item.marshal(out)?;
                }
                out.push(b']');
            }
            Self::Map(map) => {
                out.push(b'{');
                for (i, (key, value)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    gj::marshal_str(out, key, true);
                    out.push(b':');
                    value.marshal(out)?;
                }
                out.push(b'}');
            }
        }
        Some(())
    }

    fn insert(&mut self, key: &str, value: Marshal) {
        if let Self::Map(map) = self {
            map.insert(key.as_bytes().to_vec(), value);
        }
    }
}

/// `json.Marshal(entries)`, or the original tools when there are none or Marshal fails.
fn set_marshaled_tools(out: &mut Vec<u8>, tools: &Res<'_>, entries: Vec<Marshal>) {
    let mut raw = vec![];
    if entries.is_empty() || Marshal::List(entries).marshal(&mut raw).is_none() {
        gj::set_raw(out, "tools", &tools.raw);
    } else {
        gj::set_raw(out, "tools", raw);
    }
}

/// `json.Unmarshal(raw, &map[string]any)` when it yields a non-nil map.
fn unmarshal_object(raw: &[u8]) -> Option<BTreeMap<String, GoValue>> {
    if !gj::std_valid(raw) {
        return None;
    }
    match GoValue::parse_f64(raw)? {
        GoValue::Object(map) => Some(map),
        _ => None,
    }
}

/// copyInteractionsTools.
fn copy_tools(out: &mut Vec<u8>, root: &Res<'_>) {
    let tools = root.get("tools");
    if !tools.exists() {
        return;
    }
    if !tools.is_array() {
        gj::set_raw(out, "tools", &tools.raw);
        return;
    }
    let mut entries = vec![];
    let mut native = false;
    tools.each(|_, tool| {
        if tool.get("functionDeclarations").exists() {
            native = true;
            return false;
        }
        let tool_type = tool.get("type").bytes().into_owned();
        let builtin = |snake: &str, camel: &str, key: &str| {
            let raw = [snake, camel]
                .iter()
                .map(|p| tool.get(*p))
                .find(|v| v.exists() && v.is_object())
                .map_or_else(|| b"{}".to_vec(), |v| v.raw.to_vec());
            Some(Marshal::map([(key, Marshal::Raw(raw))]))
        };
        let entry = match tool_type.as_slice() {
            b"url_context" => builtin("url_context", "urlContext", "urlContext"),
            b"code_execution" => builtin("code_execution", "codeExecution", "codeExecution"),
            b"google_search" | b"web_search" => builtin("google_search", "googleSearch", "googleSearch"),
            _ => {
                let decls = tool.get("function_declarations");
                let name = tool.get("name");
                if decls.exists() && decls.is_array() {
                    Some(Marshal::map([(
                        "functionDeclarations",
                        Marshal::Raw(decls.raw.to_vec()),
                    )]))
                } else if name.exists() {
                    let mut decl = Marshal::map([("name", Marshal::Str(name.bytes().into_owned()))]);
                    let description = tool.get("description");
                    if description.exists() {
                        decl.insert("description", Marshal::Str(description.bytes().into_owned()));
                    }
                    let params = tool.get("parameters");
                    if params.exists() {
                        decl.insert("parameters", Marshal::Raw(params.raw.to_vec()));
                    }
                    Some(Marshal::map([("functionDeclarations", Marshal::List(vec![decl]))]))
                } else {
                    unmarshal_object(&tool.raw).map(|mut map| {
                        if tool_type.is_empty() {
                            for (from, to) in [
                                ("url_context", "urlContext"),
                                ("code_execution", "codeExecution"),
                                ("google_search", "googleSearch"),
                                ("web_search", "googleSearch"),
                            ] {
                                if let Some(value) = map.remove(from) {
                                    map.insert(to.to_owned(), value);
                                }
                            }
                        }
                        Marshal::Go(GoValue::Object(map))
                    })
                }
            }
        };
        entries.extend(entry);
        true
    });
    if native {
        gj::set_raw(out, "tools", &tools.raw);
        return;
    }
    set_marshaled_tools(out, &tools, entries);
}

/// geminiInteractionsInputContext: the Gemini contents built from Interactions input,
/// with model turns merged and thought signatures carried to the next model part.
#[derive(Default)]
struct Input {
    items: Vec<Vec<u8>>,
    in_model_turn: bool,
    last_step: &'static str,
    pending_signature: Vec<u8>,
}

fn signature_carrier(signature: &[u8]) -> Vec<u8> {
    let mut carrier = text_part(b"", false);
    gj::set_str(&mut carrier, "thoughtSignature", signature);
    carrier
}

fn step_signature(item: &Res<'_>) -> Vec<u8> {
    first_trimmed(&[
        &item.get("signature").bytes(),
        &item.get("thought_signature").bytes(),
        &item.get("thoughtSignature").bytes(),
    ])
}

/// interactionsGeminiContentRole.
fn content_role(role: &[u8], default_role: &str) -> &'static str {
    match go_lower(trim_space(role)).as_slice() {
        b"model" | b"assistant" => "model",
        b"user" => "user",
        _ if default_role == "model" => "model",
        _ => "user",
    }
}

impl Input {
    fn last_role_is(&self, role: &str) -> bool {
        self.items
            .last()
            .is_some_and(|c| gj::get(c, "role").bytes().as_ref() == role.as_bytes())
    }

    /// Appends to the current model turn, or opens a new model content.
    fn model_parts(&mut self, parts: Vec<Vec<u8>>, in_turn_only: bool) {
        if (!in_turn_only || self.in_model_turn) && self.last_role_is("model") {
            let last = self.items.last_mut().expect("checked");
            *last = with_parts(last, &parts, false);
        } else {
            self.items.push(content_node("model", &parts));
        }
    }

    /// flushPendingGeminiSignature.
    fn flush_signature(&mut self) {
        if self.pending_signature.is_empty() {
            return;
        }
        let carrier = signature_carrier(&std::mem::take(&mut self.pending_signature));
        self.model_parts(vec![carrier], false);
    }

    fn end_model_turn(&mut self) {
        if self.in_model_turn {
            self.flush_signature();
            self.in_model_turn = false;
        }
    }

    fn text(&mut self, role: &str, text: &[u8]) {
        self.items.push(content_node(role, &[text_part(text, false)]));
    }

    /// appendInteractionsContentList.
    fn content_list(&mut self, role: &str, content: &Res<'_>) {
        let mut part = |part: &Res<'_>| {
            if let Some(part) = content_part_to_gemini(part, false) {
                self.items.push(content_node(role, &[part]));
            }
        };
        if content.is_array() {
            content.each(|_, p| {
                part(&p);
                true
            });
        } else if content.is_object() {
            part(content);
        } else if content.kind == Kind::String {
            self.text(role, &content.bytes());
        }
    }

    /// appendInteractionsNativeContent.
    fn native_content(&mut self, item: &Res<'_>, default_role: &str) {
        let parts = item.get("parts");
        if !parts.is_array() {
            return;
        }
        let mut items = vec![];
        parts.each(|_, p| {
            items.extend(native_part(&p));
            true
        });
        if !items.is_empty() {
            let role = content_role(&item.get("role").bytes(), default_role);
            self.items.push(content_node(role, &items));
        }
    }

    /// appendInteractionsStepToGemini.
    fn step(&mut self, item: &Res<'_>, default_role: &str) {
        if item.kind == Kind::String {
            self.end_model_turn();
            self.text(default_role, &item.bytes());
            self.last_step = "text";
            return;
        }
        let steps = item.get("steps");
        if steps.is_array() {
            let role = match item.get("role").bytes().as_ref() {
                b"model" | b"assistant" => "model",
                b"user" => "user",
                _ => default_role,
            };
            steps.each(|_, child| {
                self.step(&child, role);
                true
            });
            return;
        }
        match item.get("type").bytes().as_ref() {
            b"model_output" => {
                if !self.pending_signature.is_empty() {
                    let carrier = signature_carrier(&std::mem::take(&mut self.pending_signature));
                    self.model_parts(vec![carrier], true);
                }
                let parts = step_content_parts(item, &["content", "text"], false);
                if !parts.is_empty() {
                    self.model_parts(parts, true);
                }
                self.in_model_turn = true;
                self.last_step = "model_output";
            }
            b"thought" => {
                let signature = step_signature(item);
                if !signature.is_empty() {
                    if !self.pending_signature.is_empty() && self.pending_signature != signature {
                        let carrier = signature_carrier(&self.pending_signature);
                        self.model_parts(vec![carrier], true);
                    }
                    self.pending_signature = signature;
                }
                let parts = step_content_parts(item, &["content", "summary", "text"], true);
                if !parts.is_empty() {
                    self.model_parts(parts, true);
                }
                self.in_model_turn = true;
                self.last_step = "thought";
            }
            b"function_call" => {
                let mut part = function_call_part(item);
                let mut signature = step_signature(item);
                if !self.pending_signature.is_empty() {
                    let pending = std::mem::take(&mut self.pending_signature);
                    if signature.is_empty() {
                        signature = pending;
                    } else if pending != signature {
                        self.model_parts(vec![signature_carrier(&pending)], true);
                    }
                }
                if !signature.is_empty() {
                    gj::set_str(&mut part, "thoughtSignature", &signature);
                }
                self.model_parts(vec![part], true);
                self.in_model_turn = true;
                self.last_step = "function_call";
            }
            b"function_result" => {
                self.end_model_turn();
                let part = function_result_part(item);
                if self.last_step == "function_result" && self.last_role_is("user") {
                    let last = self.items.last_mut().expect("checked");
                    *last = with_parts(last, &[part], true);
                } else {
                    self.items.push(content_node("user", &[part]));
                }
                self.last_step = "function_result";
            }
            b"user_input" | b"" => {
                self.end_model_turn();
                if item.get("parts").exists() {
                    self.native_content(item, default_role);
                } else {
                    self.content_list(default_role, &item.get("content"));
                }
                self.last_step = "user_input";
            }
            _ => {
                self.end_model_turn();
                let text = item.get("text");
                if item.get("parts").exists() {
                    self.native_content(item, default_role);
                } else if item.get("content").exists() {
                    self.content_list(default_role, &item.get("content"));
                } else if text.exists() {
                    self.text(default_role, &text.bytes());
                }
                self.last_step = "default";
            }
        }
    }
}

/// extractInteractionsThoughtPartsToGemini / extractInteractionsStepContentPartsToGemini:
/// the first existing field of `paths`, as Gemini parts.
fn step_content_parts(step: &Res<'_>, paths: &[&str], thought: bool) -> Vec<Vec<u8>> {
    let content = first_existing(step, paths);
    let mut parts = vec![];
    if content.is_array() {
        content.each(|_, p| {
            parts.extend(content_part_to_gemini(&p, thought));
            true
        });
    } else if content.is_object() {
        parts.extend(content_part_to_gemini(&content, thought));
    } else if content.kind == Kind::String {
        parts.push(text_part(&content.bytes(), thought));
    }
    parts
}

/// buildGeminiFunctionCallPart.
fn function_call_part(item: &Res<'_>) -> Vec<u8> {
    let mut part = br#"{"functionCall":{"name":"","args":{}}}"#.to_vec();
    gj::set_str(&mut part, "functionCall.name", item.get("name").bytes());
    let id = first_existing(item, &["call_id", "id"]);
    if id.exists() {
        gj::set_str(&mut part, "functionCall.id", id.bytes());
    }
    let args = item.get("arguments");
    if args.exists() {
        gj::set_raw(&mut part, "functionCall.args", &args.raw);
    }
    part
}

/// buildGeminiFunctionResultPart.
fn function_result_part(item: &Res<'_>) -> Vec<u8> {
    let mut part = br#"{"functionResponse":{"name":"","response":{}}}"#.to_vec();
    gj::set_str(&mut part, "functionResponse.name", item.get("name").bytes());
    let id = first_existing(item, &["call_id", "id"]);
    if id.exists() {
        gj::set_str(&mut part, "functionResponse.id", id.bytes());
    }
    let result = item.get("result");
    if result.exists() {
        set_function_response_result(&mut part, "functionResponse.response", &result);
    }
    part
}

/// appendInteractionsInput.
pub(crate) fn input_contents(input: &Res<'_>) -> Vec<Vec<u8>> {
    let mut ctx = Input::default();
    if !input.exists() {
        return ctx.items;
    }
    if input.kind == Kind::String {
        ctx.text("user", &input.bytes());
        return ctx.items;
    }
    let steps = input.get("steps");
    if input.is_array() {
        input.each(|_, item| {
            ctx.step(&item, "user");
            true
        });
    } else if steps.is_array() {
        let role = match input.get("role").bytes().as_ref() {
            b"model" | b"assistant" => "model",
            _ => "user",
        };
        steps.each(|_, step| {
            ctx.step(&step, role);
            true
        });
    } else {
        ctx.step(input, "user");
    }
    ctx.flush_signature();
    ctx.items
}

// ---------------------------------------------------------------------------------------
// Gemini request -> Interactions request

/// ConvertGeminiRequestToInteractions.
pub(crate) fn gemini_to_interactions(model: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    let root = gj::parse(raw);
    let mut out = br#"{"model":"","input":[]}"#.to_vec();
    gj::set_str(&mut out, "model", model);
    let sys = first_existing(&root, &["systemInstruction", "system_instruction"]);
    let text = system_instruction_text(&sys);
    if !text.is_empty() {
        gj::set_str(&mut out, "system_instruction", text);
    }
    let cfg = root.get("generationConfig");
    if cfg.exists() {
        gj::set_raw(&mut out, "generation_config", rename_keys(&cfg.raw, to_snake));
        normalize_thinking_config(&mut out);
    }
    copy_gemini_tools(&mut out, &root);
    let mut items = vec![];
    root.get("contents").each(|_, content| {
        let role = content.get("role").bytes().into_owned();
        let step_type = if role == b"model" { "model_output" } else { "user_input" };
        content.get("parts").each(|_, part| {
            let text = part.get("text");
            if part.get("functionCall").exists()
                || part.get("functionResponse").exists()
                || (text.exists() && text.bytes().is_empty())
            {
                items.extend(response::part_to_steps(&part));
                return true;
            }
            let Some(item) = part_to_content(&part) else {
                return true;
            };
            let kind = if part.get("thought").bool() && role == b"model" {
                "thought"
            } else {
                step_type
            };
            let mut step = br#"{"type":"","content":[]}"#.to_vec();
            gj::set_str(&mut step, "type", kind);
            gj::set_items(&mut step, "content", &[item]);
            items.push(step);
            true
        });
        true
    });
    gj::set_items(&mut out, "input", &items);
    gj::set_bool(&mut out, "stream", stream);
    out
}

/// geminiSystemInstructionText.
fn system_instruction_text(sys: &Res<'_>) -> Vec<u8> {
    if !sys.exists() {
        return vec![];
    }
    if sys.kind == Kind::String {
        return string(sys);
    }
    let text = sys.get("text");
    if text.exists() && text.kind == Kind::String {
        return string(&text);
    }
    let parts = sys.get("parts");
    if !parts.is_array() {
        return vec![];
    }
    let mut out = vec![];
    parts.each(|_, part| {
        let text = part.get("text").bytes();
        if !text.is_empty() {
            if !out.is_empty() {
                out.push(b'\n');
            }
            out.extend_from_slice(&text);
        }
        true
    });
    out
}

/// normalizeGeminiThinkingConfigForInteractions.
fn normalize_thinking_config(out: &mut Vec<u8>) {
    let level = first_existing(
        &gj::parse(out),
        &[
            "generation_config.thinking_config.thinking_level",
            "generation_config.thinkingConfig.thinkingLevel",
            "generation_config.thinkingConfig.thinking_level",
        ],
    )
    .into_owned();
    if level.exists() {
        gj::set_str(
            out,
            "generation_config.thinking_level",
            go_lower(trim_space(&level.bytes())),
        );
    }
    let budget = first_existing(
        &gj::parse(out),
        &[
            "generation_config.thinking_config.thinking_budget",
            "generation_config.thinkingConfig.thinkingBudget",
            "generation_config.thinkingConfig.thinking_budget",
        ],
    )
    .into_owned();
    if budget.exists() {
        gj::set_raw(out, "generation_config.thinking_budget", &budget.raw);
    }
    if !gj::get(out, "generation_config.thinking_summaries").exists() {
        let include = first_existing(
            &gj::parse(out),
            &[
                "generation_config.thinking_config.include_thoughts",
                "generation_config.thinking_config.includeThoughts",
                "generation_config.thinkingConfig.include_thoughts",
                "generation_config.thinkingConfig.includeThoughts",
            ],
        )
        .into_owned();
        if include.exists() {
            let summary = if include.bool() { "auto" } else { "none" };
            gj::set_str(out, "generation_config.thinking_summaries", summary);
        }
    }
}

/// A built-in Gemini tool as an Interactions tool entry.
fn builtin_entry(value: &Res<'_>, kind: &str, key: &str) -> Marshal {
    let mut entry = Marshal::map([("type", Marshal::Str(kind.as_bytes().to_vec()))]);
    if value.is_object() && !value.map().is_empty() {
        entry.insert(key, Marshal::Raw(value.raw.to_vec()));
    }
    entry
}

/// A function declaration as an Interactions function tool.
fn function_entry(decl: &Res<'_>, name: &Res<'_>) -> Marshal {
    let mut entry = Marshal::map([
        ("type", Marshal::Str(b"function".to_vec())),
        ("name", Marshal::Str(name.bytes().into_owned())),
    ]);
    let description = decl.get("description");
    if description.exists() {
        entry.insert("description", Marshal::Str(description.bytes().into_owned()));
    }
    let params = first_existing(decl, &["parameters", "parametersJsonSchema"]);
    if params.exists() {
        entry.insert("parameters", Marshal::Raw(params.raw.to_vec()));
    }
    entry
}

/// copyGeminiToolsToInteractions.
fn copy_gemini_tools(out: &mut Vec<u8>, root: &Res<'_>) {
    let tools = root.get("tools");
    if !tools.exists() {
        return;
    }
    if !tools.is_array() {
        gj::set_raw(out, "tools", &tools.raw);
        return;
    }
    let mut entries = vec![];
    tools.each(|_, tool| {
        for (camel, snake, kind) in [
            ("urlContext", "url_context", "url_context"),
            ("codeExecution", "code_execution", "code_execution"),
            ("googleSearch", "google_search", "google_search"),
        ] {
            let value = first_existing(&tool, &[camel, snake]);
            if value.exists() {
                entries.push(builtin_entry(&value, kind, kind));
            }
        }
        let name = tool.get("name");
        if name.exists() {
            entries.push(function_entry(&tool, &name));
            return true;
        }
        first_existing(&tool, &["functionDeclarations", "function_declarations"]).each(|_, decl| {
            let name = decl.get("name");
            if name.exists() {
                entries.push(function_entry(&decl, &name));
            }
            true
        });
        true
    });
    set_marshaled_tools(out, &tools, entries);
}

/// geminiPartToInteractionsContent.
fn part_to_content(part: &Res<'_>) -> Option<Vec<u8>> {
    let text = part.get("text");
    if text.exists() {
        let mut item = br#"{"type":"text","text":""}"#.to_vec();
        gj::set_str(&mut item, "text", text.bytes());
        return Some(item);
    }
    let inline = part.get("inlineData");
    if inline.exists() {
        let mut mime = string(&inline.get("mimeType"));
        if mime.is_empty() {
            mime = string(&inline.get("mime_type"));
        }
        return Some(inline_to_interactions_content(&mime, &inline.get("data").bytes()));
    }
    let inline = part.get("inline_data");
    if inline.exists() {
        return Some(inline_to_interactions_content(
            &inline.get("mime_type").bytes(),
            &inline.get("data").bytes(),
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values from Go 1.26 (toCamelCase, toSnakeCase).
    #[test]
    fn key_case_conversion_matches_go() {
        assert_eq!(to_camel(b"thinking_level"), b"thinkingLevel");
        assert_eq!(to_camel(b"a__b_"), b"aB");
        assert_eq!(to_camel(b"_x"), b"X");
        assert_eq!(to_camel("a_\u{e9}t".as_bytes()), b"a\xef\xbf\xbd\xa9t");
        assert_eq!(to_camel(b"x_\xff"), "x\u{fffd}".as_bytes());
        assert_eq!(to_snake(b"thinkingConfig"), b"thinking_config");
        assert_eq!(to_snake(b"ABc"), b"a_bc");
        assert_eq!(to_snake(b"a\xffB"), "a\u{fffd}_b".as_bytes());
        assert_eq!(to_snake("\u{c9}A".as_bytes()), "\u{e9}_a".as_bytes());
    }

    #[test]
    fn data_url_split_matches_go() {
        assert!(inline_part_from_data_url(b"data:image/png,abc").is_none());
        assert!(inline_part_from_data_url(b"data:image/png;charset=x;base64,AA").is_none());
        assert_eq!(
            inline_part_from_data_url(b"data:image/png;base64,AA").unwrap(),
            br#"{"inlineData":{"mimeType":"image/png","data":"AA"}}"#
        );
    }
}
