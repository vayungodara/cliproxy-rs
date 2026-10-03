//! Devin request parsing (devin_executor.go `parseInteractionsPayload` and helpers):
//! the Interactions payload (or an untranslated OpenAI `messages` body) becomes the
//! system prompt, history prompts, tools and generation settings that
//! [`crate::devin_wire::build_chat_request`] encodes.
//!
//! Signatures, reasoning and images the translator dropped are recovered from the
//! client's original request (`supplementSignaturesFromOriginal`,
//! `supplementImagesFromOriginal`).

use cpa_common::gostr::trim_space;
use cpa_common::json::{self as gj, Kind, Res};
use cpa_common::signature::{Provider, detect_provider};

use crate::devin_wire::{
    DEFAULT_MAX_TOKENS, Image, Prompt, Tool, ToolCall, contains, is_codex_app_automation_update,
    sanitize_tool_description,
};

/// `devinEmptyToolResultPlaceholder`.
const EMPTY_TOOL_RESULT: &[u8] = b"{}";

/// What `parseInteractionsPayload` returns.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Parsed {
    pub system: Vec<u8>,
    pub prompts: Vec<Prompt>,
    pub tools: Vec<Tool>,
    pub temperature: Option<f64>,
    pub max_tokens: i64,
    pub session_id: Vec<u8>,
    pub cascade_id: Vec<u8>,
    pub thinking_level: String,
    pub budget: i64,
}

fn lower(b: &[u8]) -> String {
    String::from_utf8_lossy(trim_space(b)).to_lowercase()
}

fn s(r: &Res<'_>) -> Vec<u8> {
    r.bytes().into_owned()
}

/// `firstNonEmpty`: the first value that is not blank, untrimmed.
fn first_non_empty(values: &[Vec<u8>]) -> Vec<u8> {
    values
        .iter()
        .find(|v| !trim_space(v).is_empty())
        .cloned()
        .unwrap_or_default()
}

fn message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// `parseInteractionsPayload`.
pub(crate) fn parse_interactions(payload: &[u8], original: &[u8]) -> Parsed {
    let root = gj::parse(payload);
    let orig = gj::parse(original);
    let mut p = Parsed {
        system: trim_space(&root.get("system_instruction").bytes()).to_vec(),
        ..Parsed::default()
    };
    if p.system.is_empty() {
        p.system = trim_space(&root.get("systemInstruction").bytes()).to_vec();
    }
    let mut gen_cfg = root.get("generation_config");
    if !gen_cfg.exists() {
        gen_cfg = root.get("generationConfig");
    }
    if gen_cfg.exists() {
        let t = gen_cfg.get("temperature");
        if t.exists() {
            p.temperature = Some(t.float());
        }
        p.max_tokens = gen_cfg.get("max_output_tokens").int();
        p.thinking_level = gen_cfg.get("thinking_level").str().into_owned();
        p.budget = gen_cfg.get("thinking_config.thinking_budget").int();
    }
    if p.temperature.is_none() {
        if orig.get("temperature").exists() {
            p.temperature = Some(orig.get("temperature").float());
        } else if root.get("temperature").exists() {
            p.temperature = Some(root.get("temperature").float());
        }
    }
    if p.max_tokens <= 0 {
        p.max_tokens = DEFAULT_MAX_TOKENS;
    }
    let session = |r: &Res<'_>| {
        first_non_empty(&[
            s(&r.get("session_id")),
            s(&r.get("sessionId")),
            s(&r.get("conversation_id")),
            s(&r.get("previous_interaction_id")),
        ])
    };
    p.session_id = trim_space(&session(&root)).to_vec();
    if p.session_id.is_empty() && !original.is_empty() {
        p.session_id = trim_space(&session(&orig)).to_vec();
    }
    p.cascade_id.clone_from(&p.session_id);

    let mut pending: Vec<Vec<u8>> = Vec::new();
    let input = root.get("input");
    if input.is_array() {
        for step in input.array() {
            match lower(&step.get("type").bytes()).as_str() {
                "user_input" => {
                    let (content, images) = step_content(&step);
                    p.prompts.push(Prompt {
                        message_id: message_id(),
                        source: 1,
                        content,
                        images,
                        ..Prompt::default()
                    });
                }
                "model_output" => {
                    let text = step_text(&step);
                    let (signature, signature_type) = step_signature(&step);
                    match p.prompts.last_mut().filter(|last| last.source == 2) {
                        Some(last) => {
                            join_into(&mut last.content, b"\n", &text);
                            if !signature.is_empty() && last.signature.is_empty() {
                                last.signature = signature;
                                last.signature_type = signature_type;
                            }
                        }
                        None => p.prompts.push(Prompt {
                            message_id: message_id(),
                            source: 2,
                            content: text,
                            signature,
                            signature_type,
                            ..Prompt::default()
                        }),
                    }
                }
                "thought" => {
                    let text = step_text(&step);
                    let (signature, signature_type) = step_signature(&step);
                    match p.prompts.last_mut().filter(|last| last.source == 2) {
                        Some(last) => {
                            join_into(&mut last.thinking, b"\n\n", &text);
                            if !signature.is_empty() && last.signature.is_empty() {
                                last.signature = signature;
                                last.signature_type = signature_type;
                            }
                        }
                        None => p.prompts.push(Prompt {
                            message_id: message_id(),
                            source: 2,
                            thinking: text,
                            signature,
                            signature_type,
                            ..Prompt::default()
                        }),
                    }
                }
                "function_call" => {
                    let id = first_non_empty(&[s(&step.get("id")), s(&step.get("call_id"))]);
                    let call = ToolCall {
                        id: id.clone(),
                        name: s(&step.get("name")),
                        arguments: arguments(&step.get("arguments")),
                    };
                    match p.prompts.last_mut().filter(|last| last.source == 2) {
                        Some(last) => last.tool_calls.push(call),
                        None => p.prompts.push(Prompt {
                            message_id: message_id(),
                            source: 2,
                            tool_calls: vec![call],
                            ..Prompt::default()
                        }),
                    }
                    pending.push(id);
                }
                "function_result" => {
                    let id = first_non_empty(&[s(&step.get("call_id")), s(&step.get("id"))]);
                    let (content, images) = function_result_content(&step);
                    p.prompts.push(tool_result(&mut pending, id, content, images));
                }
                _ => {}
            }
        }
    } else if root.get("messages").is_array() {
        // An untranslated OpenAI body.
        for m in root.get("messages").array() {
            match lower(&m.get("role").bytes()).as_str() {
                "system" | "developer" => {
                    if p.system.is_empty() {
                        p.system = s(&m.get("content"));
                    }
                }
                "user" => {
                    let (content, images) = step_content(&m);
                    p.prompts.push(Prompt {
                        message_id: message_id(),
                        source: 1,
                        content,
                        images,
                        ..Prompt::default()
                    });
                }
                "assistant" => {
                    let content = step_text(&m);
                    let mut tool_calls = Vec::new();
                    let calls = m.get("tool_calls");
                    if calls.is_array() {
                        for tc in calls.array() {
                            let id = first_non_empty(&[s(&tc.get("id")), s(&tc.get("call_id"))]);
                            let mut name = s(&tc.get("function.name"));
                            if name.is_empty() {
                                name = s(&tc.get("name"));
                            }
                            let mut args = tc.get("function.arguments");
                            if !args.exists() {
                                args = tc.get("arguments");
                            }
                            tool_calls.push(ToolCall {
                                id: id.clone(),
                                name,
                                arguments: arguments(&args),
                            });
                            pending.push(id);
                        }
                    }
                    p.prompts.push(Prompt {
                        message_id: message_id(),
                        source: 2,
                        content,
                        tool_calls,
                        ..Prompt::default()
                    });
                }
                "tool" => {
                    let id = first_non_empty(&[s(&m.get("tool_call_id")), s(&m.get("id")), s(&m.get("call_id"))]);
                    let (content, images) = function_result_content(&m);
                    p.prompts.push(tool_result(&mut pending, id, content, images));
                }
                _ => {}
            }
        }
    }
    if !original.is_empty() {
        supplement_signatures(original, &mut p.prompts);
        supplement_images(original, &mut p.prompts);
    }
    p.tools = tools(&root.get("tools"));
    p
}

/// `content += sep + text` when content is non-empty, else `content = text`.
fn join_into(target: &mut Vec<u8>, sep: &[u8], text: &[u8]) {
    if target.is_empty() {
        *target = text.to_vec();
    } else {
        target.extend_from_slice(sep);
        target.extend_from_slice(text);
    }
}

/// A function call's arguments: a string's value, else the raw JSON.
fn arguments(r: &Res<'_>) -> Vec<u8> {
    if r.kind == Kind::String {
        s(r)
    } else if r.exists() {
        r.raw().to_vec()
    } else {
        Vec::new()
    }
}

fn step_signature(step: &Res<'_>) -> (Vec<u8>, Vec<u8>) {
    let raw = first_non_empty(&[s(&step.get("signature")), s(&step.get("thought_signature"))]);
    let (bytes, kind) = parse_signature(&raw);
    (bytes, kind.as_bytes().to_vec())
}

/// `matchPendingToolCall`: a result answers the pending call with its ID (or the oldest
/// pending one when it has none); otherwise it becomes an orphaned user turn.
fn tool_result(pending: &mut Vec<Vec<u8>>, id: Vec<u8>, content: Vec<u8>, images: Vec<Image>) -> Prompt {
    let index = if !id.is_empty() {
        pending.iter().position(|p| *p == id)
    } else if !pending.is_empty() {
        Some(0)
    } else {
        None
    };
    match index {
        Some(i) => {
            let matched = pending.remove(i);
            Prompt {
                message_id: message_id(),
                source: 4,
                tool_call_id: if id.is_empty() { matched } else { id },
                content,
                images,
                ..Prompt::default()
            }
        }
        None => Prompt {
            message_id: message_id(),
            source: 1,
            original_tool_call_id: id,
            is_orphaned_tool: true,
            content,
            images,
            ..Prompt::default()
        },
    }
}

/// The tool list: Codex app namespaces flattened (minus `automation_update`), Gemini
/// function declarations expanded.
fn tools(list: &Res<'_>) -> Vec<Tool> {
    let mut out = Vec::new();
    if !list.is_array() {
        return out;
    }
    let mut push = |t: &Res<'_>| {
        let name = s(&t.get("name"));
        if name.is_empty() || is_codex_app_automation_update(b"", &name) {
            return;
        }
        let description = sanitize_tool_description(&name, &s(&t.get("description")));
        let mut parameters = t.get("parameters").raw().to_vec();
        if parameters.is_empty() {
            parameters = t.get("parametersJsonSchema").raw().to_vec();
        }
        out.push(Tool {
            name,
            description,
            parameters,
        });
    };
    for t in list.array() {
        if *t.get("type").bytes() == *b"namespace" && lower(&t.get("name").bytes()) == "mcp__codex_app" {
            let mut children = t.get("tools");
            if !children.exists() || !children.is_array() {
                children = t.get("children");
            }
            if children.exists() && children.is_array() {
                for c in children.array() {
                    if lower(&c.get("name").bytes()) == "automation_update" {
                        continue;
                    }
                    push(&c);
                }
            }
            continue;
        }
        let declarations = t.get("function_declarations");
        if declarations.exists() && declarations.is_array() {
            declarations.array().iter().for_each(&mut push);
            continue;
        }
        let declarations = t.get("functionDeclarations");
        if declarations.exists() && declarations.is_array() {
            declarations.array().iter().for_each(&mut push);
            continue;
        }
        push(&t);
    }
    out
}

/// `parseDataURL`.
fn parse_data_url(raw: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let raw = trim_space(raw);
    if !raw.starts_with(b"data:") {
        return None;
    }
    let comma = raw.iter().position(|b| *b == b',')?;
    let header = &raw[5..comma];
    let data = raw[comma + 1..].to_vec();
    let first = header.split(|b| *b == b';').next().unwrap_or_default();
    let mut mime = trim_space(first).to_vec();
    if mime.is_empty() {
        mime = b"image/png".to_vec();
    }
    Some((mime, data))
}

/// `mimeExtension`.
fn mime_extension(mime: &[u8]) -> &'static str {
    match lower(mime).as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    }
}

/// `extractDevinImage`.
fn image(part: &Res<'_>) -> Option<Image> {
    if !matches!(
        lower(&part.get("type").bytes()).as_str(),
        "image" | "input_image" | "image_url"
    ) {
        return None;
    }
    let mut data = trim_space(&part.get("data").bytes()).to_vec();
    let mut mime = trim_space(&part.get("mime_type").bytes()).to_vec();
    if data.is_empty() {
        data = trim_space(&part.get("source.data").bytes()).to_vec();
        if mime.is_empty() {
            mime = trim_space(&part.get("source.media_type").bytes()).to_vec();
        }
    }
    if data.is_empty() {
        let url = first_non_empty(&[
            s(&part.get("image_url.url")),
            s(&part.get("image_url")),
            s(&part.get("url")),
        ]);
        if let Some((m, d)) = parse_data_url(&url) {
            data = d;
            if mime.is_empty() {
                mime = m;
            }
        }
    }
    if data.is_empty() {
        data = trim_space(&part.get("inline_data.data").bytes()).to_vec();
        if mime.is_empty() {
            mime = trim_space(&part.get("inline_data.mime_type").bytes()).to_vec();
        }
    }
    if data.is_empty() {
        return None;
    }
    if mime.is_empty() {
        mime = b"image/png".to_vec();
    }
    Some(Image { base64: data, mime })
}

/// `[Image N: pasted_image_N.ext]` lines for `images`.
fn image_headers(images: &[Image]) -> Vec<u8> {
    images
        .iter()
        .enumerate()
        .map(|(i, img)| {
            format!(
                "[Image {}: pasted_image_{}.{}]",
                i + 1,
                i + 1,
                mime_extension(&img.mime)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes()
}

/// Prefixes `text` with the image headers unless it already names an image.
fn with_image_headers(text: Vec<u8>, images: &[Image]) -> Vec<u8> {
    if images.is_empty() || contains(&text, b"[Image ") {
        return text;
    }
    let mut out = image_headers(images);
    if !text.is_empty() {
        out.extend_from_slice(b"\n\n");
        out.extend_from_slice(&text);
    }
    out
}

/// Every key of `obj`, in document order (duplicates included).
fn keys(obj: &Res<'_>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    obj.each(|k, _| {
        out.push(k.bytes().into_owned());
        true
    });
    out
}

/// `isProtocolWrapperObject`.
fn is_wrapper(obj: &Res<'_>, key: &str) -> bool {
    if !obj.is_object() || !obj.get(key).exists() {
        return false;
    }
    let names = keys(obj);
    if lower(&obj.get("type").bytes()) == "tool_result" {
        return names.iter().all(|k| {
            k.as_slice() == key.as_bytes()
                || matches!(
                    k.as_slice(),
                    b"type" | b"tool_use_id" | b"id" | b"is_error" | b"cache_control"
                )
        });
    }
    let mut has = false;
    for k in &names {
        if k.as_slice() == key.as_bytes() {
            has = true;
        } else if k.as_slice() != b"cache_control" {
            return false;
        }
    }
    has
}

/// A `{"type":"text","text":...}` part with nothing else but `cache_control`.
fn is_pure_text(item: &Res<'_>) -> bool {
    keys(item)
        .iter()
        .all(|k| matches!(k.as_slice(), b"type" | b"text" | b"cache_control"))
}

/// `extractFunctionResultTarget`.
fn result_target(target: &Res<'_>) -> (Vec<u8>, Vec<Image>) {
    if !target.exists() {
        return Default::default();
    }
    if target.kind == Kind::String {
        return (s(target), Vec::new());
    }
    if let Some(img) = image(target) {
        return (Vec::new(), vec![img]);
    }
    if target.is_object() {
        for key in ["content", "output", "result"] {
            if is_wrapper(target, key) {
                return result_target(&target.get(key));
            }
        }
        if lower(&target.get("type").bytes()) == "text" && is_pure_text(target) {
            return (s(&target.get("text")), Vec::new());
        }
        return (target.raw().to_vec(), Vec::new());
    }
    if target.is_array() {
        let mut texts: Vec<Vec<u8>> = Vec::new();
        let mut images = Vec::new();
        let mut structured = false;
        for item in target.array() {
            if let Some(img) = image(&item) {
                images.push(img);
                structured = true;
                continue;
            }
            if item.is_object() {
                if let Some(key) = ["content", "output", "result"]
                    .into_iter()
                    .find(|k| is_wrapper(&item, k))
                {
                    structured = true;
                    let (text, imgs) = result_target(&item.get(key));
                    if !text.is_empty() {
                        texts.push(text);
                    }
                    images.extend(imgs);
                    continue;
                }
                if lower(&item.get("type").bytes()) == "text" {
                    if is_pure_text(&item) {
                        structured = true;
                        let text = s(&item.get("text"));
                        if !text.is_empty() {
                            texts.push(text);
                        }
                    } else {
                        let raw = trim_space(item.raw());
                        if !raw.is_empty() {
                            texts.push(raw.to_vec());
                        }
                    }
                    continue;
                }
            }
            let raw = trim_space(item.raw());
            if !raw.is_empty() {
                texts.push(raw.to_vec());
            }
        }
        if structured || !images.is_empty() {
            return (texts.join(&b'\n'), images);
        }
        return (target.raw().to_vec(), Vec::new());
    }
    (target.raw().to_vec(), Vec::new())
}

/// `extractFunctionResultContent`.
fn function_result_content(step: &Res<'_>) -> (Vec<u8>, Vec<Image>) {
    let mut target = step.get("result");
    if !target.exists() {
        target = step.get("output");
    }
    if !target.exists() {
        target = step.get("content");
    }
    if !target.exists() {
        return (EMPTY_TOOL_RESULT.to_vec(), Vec::new());
    }
    let (text, images) = result_target(&target);
    let mut text = with_image_headers(text, &images);
    if trim_space(&text).is_empty() && images.is_empty() {
        text = EMPTY_TOOL_RESULT.to_vec();
    }
    (text, images)
}

/// `extractInteractionsStepContent`: text parts joined by newlines, plus images.
fn step_content(step: &Res<'_>) -> (Vec<u8>, Vec<Image>) {
    let content = step.get("content");
    let mut texts: Vec<Vec<u8>> = Vec::new();
    let mut images = Vec::new();
    if content.kind == Kind::String {
        texts.push(s(&content));
    } else if content.is_array() {
        for part in content.array() {
            if let Some(img) = image(&part) {
                images.push(img);
            } else {
                let text = s(&part.get("text"));
                if !text.is_empty() {
                    texts.push(text);
                }
            }
        }
    } else if step.get("text").exists() {
        texts.push(s(&step.get("text")));
    }
    let text = texts.join(&b'\n');
    (with_image_headers(text, &images), images)
}

/// `extractInteractionsStepText`.
fn step_text(step: &Res<'_>) -> Vec<u8> {
    let content = step.get("content");
    if content.kind == Kind::String {
        return s(&content);
    }
    if content.is_array() {
        return content
            .array()
            .iter()
            .map(|p| s(&p.get("text")))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(&b'\n');
    }
    s(&step.get("text"))
}

/// Go `base64.StdEncoding.DecodeString`: padding required, CR and LF ignored, non-zero
/// trailing bits accepted.
fn std_base64(s: &[u8]) -> Option<Vec<u8>> {
    use base64::Engine;
    use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
    const GO_STD: GeneralPurpose = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
    );
    let cleaned: Vec<u8> = s.iter().copied().filter(|b| *b != b'\r' && *b != b'\n').collect();
    GO_STD.decode(cleaned).ok()
}

/// `parseSignatureBytes`: the signature bytes Devin expects and their type
/// (`sealed`, `anthropic`, `openai`, `gemini`).
pub(crate) fn parse_signature(raw: &[u8]) -> (Vec<u8>, &'static str) {
    let sig = trim_space(raw);
    if sig.is_empty() {
        return (Vec::new(), "");
    }
    if sig.starts_with(b"sealed.v1.") {
        return (sig.to_vec(), "sealed");
    }
    for (prefix, kind) in [
        (&b"claude#"[..], "anthropic"),
        (b"gpt#", "openai"),
        (b"gemini#", "gemini"),
    ] {
        if let Some(rest) = sig.strip_prefix(prefix) {
            return (rest.to_vec(), kind);
        }
    }
    match detect_provider(sig) {
        Provider::Claude => return (sig.to_vec(), "anthropic"),
        Provider::Gpt => return (sig.to_vec(), "openai"),
        Provider::Gemini => return (sig.to_vec(), "gemini"),
        _ => {}
    }
    if sig.starts_with(b"AY") {
        return (sig.to_vec(), "gemini");
    }
    if let Some(decoded) = std_base64(sig).filter(|d| !d.is_empty()) {
        if decoded.starts_with(b"sealed.v1.") {
            return (decoded, "sealed");
        }
        match detect_provider(&decoded) {
            Provider::Claude => return (decoded, "anthropic"),
            Provider::Gpt => return (decoded, "openai"),
            Provider::Gemini => return (sig.to_vec(), "gemini"),
            _ => {}
        }
        if decoded.starts_with(b"CAQS") || decoded.starts_with(b"CAIS") {
            return (decoded, "anthropic");
        }
        if decoded.starts_with(b"gAAAA") {
            return (decoded, "openai");
        }
        if decoded[0] == 0x01 {
            return (sig.to_vec(), "gemini");
        }
    }
    (sig.to_vec(), signature_type(sig))
}

/// `detectSignatureType`.
fn signature_type(sig: &[u8]) -> &'static str {
    let sig = trim_space(sig);
    if sig.starts_with(b"sealed.v1.") {
        return "sealed";
    }
    if sig.starts_with(b"claude#") {
        return "anthropic";
    }
    if sig.starts_with(b"gpt#") {
        return "openai";
    }
    if sig.starts_with(b"gemini#") {
        return "gemini";
    }
    match detect_provider(sig) {
        Provider::Claude => return "anthropic",
        Provider::Gpt => return "openai",
        Provider::Gemini => return "gemini",
        _ => {}
    }
    if sig.starts_with(b"CAQS") || sig.starts_with(b"CAIS") {
        "anthropic"
    } else if sig.starts_with(b"gAAAA") {
        "openai"
    } else if sig.starts_with(b"AY") {
        "gemini"
    } else {
        "sealed"
    }
}

/// `supplementSignaturesFromOriginal`: assistant turns, in order, take the thinking
/// signature and text of the original Claude request's assistant messages.
fn supplement_signatures(original: &[u8], prompts: &mut [Prompt]) {
    let messages = gj::get(original, "messages");
    if !messages.is_array() {
        return;
    }
    let mut assistants: Vec<(Vec<u8>, &'static str, Vec<u8>)> = Vec::new();
    for m in messages.array() {
        if !m.get("role").str().eq_ignore_ascii_case("assistant") {
            continue;
        }
        let (mut signature, mut kind, mut thinking) = (Vec::new(), "", Vec::new());
        let content = m.get("content");
        if content.is_array() {
            for part in content.array() {
                if *part.get("type").bytes() != *b"thinking" {
                    continue;
                }
                let sig = s(&part.get("signature"));
                if !sig.is_empty() {
                    let (bytes, k) = parse_signature(&sig);
                    if !bytes.is_empty() {
                        signature = bytes;
                        kind = k;
                    }
                }
                let text = s(&part.get("thinking"));
                if !text.is_empty() {
                    thinking = text;
                }
            }
        }
        assistants.push((signature, kind, thinking));
    }
    let mut index = 0;
    for p in prompts.iter_mut().filter(|p| p.source == 2) {
        let Some((signature, kind, thinking)) = assistants.get(index) else {
            continue;
        };
        if p.signature.is_empty() && !signature.is_empty() {
            p.signature.clone_from(signature);
            p.signature_type = kind.as_bytes().to_vec();
        }
        if p.thinking.is_empty() && !thinking.is_empty() {
            p.thinking.clone_from(thinking);
        }
        index += 1;
    }
}

/// `supplementImagesFromOriginal`: user turns take their original images by position,
/// tool results by tool call ID.
fn supplement_images(original: &[u8], prompts: &mut [Prompt]) {
    let messages = gj::get(original, "messages");
    if !messages.is_array() {
        return;
    }
    let mut user_images: Vec<Vec<Image>> = Vec::new();
    let mut tool_images: Vec<(Vec<u8>, Vec<Image>)> = Vec::new();
    let mut add_tool = |id: Vec<u8>, imgs: Vec<Image>| {
        if imgs.is_empty() || id.is_empty() {
            return;
        }
        match tool_images.iter_mut().find(|(k, _)| *k == id) {
            Some((_, list)) => list.extend(imgs),
            None => tool_images.push((id, imgs)),
        }
    };
    let images_of = |container: &Res<'_>, content: &Res<'_>| -> Vec<Image> {
        if content.is_array() {
            content.array().iter().filter_map(image).collect()
        } else {
            image(container).into_iter().collect()
        }
    };
    for m in messages.array() {
        match lower(&m.get("role").bytes()).as_str() {
            "user" => {
                let mut imgs = Vec::new();
                let content = m.get("content");
                if content.is_array() {
                    for part in content.array() {
                        if lower(&part.get("type").bytes()) == "tool_result" {
                            let id = first_non_empty(&[s(&part.get("tool_use_id")), s(&part.get("id"))]);
                            let tool_imgs = images_of(&part, &part.get("content"));
                            add_tool(id, tool_imgs);
                        } else if let Some(img) = image(&part) {
                            imgs.push(img);
                        }
                    }
                }
                user_images.push(imgs);
            }
            "tool" => {
                let id = first_non_empty(&[s(&m.get("tool_call_id")), s(&m.get("id"))]);
                let tool_imgs = images_of(&m, &m.get("content"));
                add_tool(id, tool_imgs);
            }
            _ => {}
        }
    }
    let lookup = |id: &[u8]| {
        tool_images
            .iter()
            .find(|(k, _)| k.as_slice() == id)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    };
    let mut user_index = 0;
    for p in prompts.iter_mut() {
        match p.source {
            1 if p.is_orphaned_tool => {
                if p.images.is_empty()
                    && !p.original_tool_call_id.is_empty()
                    && let Some(imgs) = lookup(&p.original_tool_call_id)
                {
                    p.images = imgs;
                    p.content = with_image_headers(std::mem::take(&mut p.content), &p.images);
                }
            }
            1 => {
                if p.images.is_empty()
                    && let Some(imgs) = user_images.get(user_index).filter(|i| !i.is_empty())
                {
                    p.images = imgs.clone();
                    p.content = with_image_headers(std::mem::take(&mut p.content), &p.images);
                }
                user_index += 1;
            }
            4 => {
                if p.images.is_empty()
                    && !p.tool_call_id.is_empty()
                    && let Some(imgs) = lookup(&p.tool_call_id)
                {
                    p.images = imgs;
                    p.content = with_image_headers(std::mem::take(&mut p.content), &p.images);
                }
            }
            _ => {}
        }
    }
}
