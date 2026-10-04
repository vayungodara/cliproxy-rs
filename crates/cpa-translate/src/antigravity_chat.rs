//! OpenAI Chat Completions client, Antigravity upstream
//! (internal/translator/antigravity/openai/chat-completions): ConvertOpenAIRequestToAntigravity,
//! ConvertAntigravityResponseToOpenAI and its NonStream (which unwraps the envelope and
//! reuses the Gemini Chat non-stream converter).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use cpa_common::json::{self as gj, Kind, Res};

use crate::antigravity_gemini::{has_response_payload, restore_names, sanitize_claude_signatures};
use crate::common::{
    go_lower, go_upper, normalize_openai_file_data, now_nanos, parse_rfc3339_unix, restore_sanitized_tool_name,
    sanitize_claude_tool_id, system_reminder_text, trim_space,
};
use crate::gemini::attach_default_safety_settings;
use crate::gemini_chat_request::{audio_mime, content_node, function_declaration, text_part};
use crate::responses_tools::{disambiguated_tool_name_map, map_sanitized_function_name, sanitized_function_name_map};
use crate::stream::GoStream;
use crate::{Error, Registered, ResponseCtx, thinking};

pub static PAIR: Registered = registered!(
    OpenAI -> Antigravity,
    request: |ctx, body| Ok(convert(ctx.model, body)),
    non_stream: non_stream,
    go_stream: go_stream,
    token_count: None,
);

const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";
const CONFIG: &str = "request.generationConfig";
const THINKING: &str = "request.generationConfig.thinkingConfig";

// ---------------------------------------------------------------------------------------
// Request

/// antigravityOpenAIInlineDataPart: camelCase `mimeType`, or `mime_type` for audio.
fn inline_part(mime: &[u8], data: &[u8], snake: bool) -> Vec<u8> {
    let (mut part, path) = if snake {
        (
            br#"{"inlineData":{"mime_type":"","data":""}}"#.to_vec(),
            "inlineData.mime_type",
        )
    } else {
        (
            br#"{"inlineData":{"mimeType":"","data":""}}"#.to_vec(),
            "inlineData.mimeType",
        )
    };
    gj::set_str(&mut part, path, mime);
    gj::set_str(&mut part, "inlineData.data", data);
    part
}

/// A `data:<mime>;base64,<data>` URL sliced the way Go does: after five bytes, the text
/// before `;` is the MIME type and the text after the next seven bytes is the data.
fn data_url_part(url: &[u8]) -> Option<Vec<u8>> {
    if url.len() <= 5 {
        return None;
    }
    let rest = &url[5..];
    let semi = rest.iter().position(|&c| c == b';')?;
    let tail = &rest[semi + 1..];
    (tail.len() > 7).then(|| inline_part(&rest[..semi], &tail[7..], false))
}

/// antigravityDemotedSystemText.
fn demoted_text(text: Vec<u8>, demoted: bool) -> Vec<u8> {
    if !demoted || trim_space(&text).is_empty() {
        return text;
    }
    system_reminder_text(&text)
}

/// ConvertOpenAIRequestToAntigravity.
fn convert(model: &str, raw: &[u8]) -> Vec<u8> {
    let names = sanitized_function_name_map(raw);
    let mut out = br#"{"project":"","request":{"contents":[]},"model":"gemini-2.5-pro"}"#.to_vec();
    gj::set_str(&mut out, "model", model);
    let config = gj::get(raw, "generationConfig");
    let config = if config.exists() {
        config
    } else {
        gj::get(raw, "generation_config")
    };
    if config.exists() {
        gj::set_raw(&mut out, CONFIG, &config.raw);
    }
    let effort = gj::get(raw, "reasoning_effort");
    if effort.exists() {
        let effort = go_lower(trim_space(&effort.bytes()));
        if effort == b"auto" {
            gj::set_int(&mut out, &format!("{THINKING}.thinkingBudget"), -1);
        } else if !effort.is_empty() {
            gj::set_str(&mut out, &format!("{THINKING}.thinkingLevel"), effort);
        }
    }
    normalize_thinking_config(&mut out);
    let summary = thinking::extract_summary(raw, "openai");
    out = thinking::apply_summary(out, "antigravity", summary);

    for (from, to) in [("temperature", "temperature"), ("top_p", "topP"), ("top_k", "topK")] {
        let v = gj::get(raw, from);
        if v.kind == Kind::Number {
            gj::set_f64(&mut out, &format!("{CONFIG}.{to}"), v.num);
        }
    }
    let max_tokens = gj::get(raw, "max_tokens");
    let max_completion = gj::get(raw, "max_completion_tokens");
    if max_tokens.kind == Kind::Number {
        gj::set_f64(&mut out, "request.generationConfig.maxOutputTokens", max_tokens.num);
    } else if max_completion.kind == Kind::Number {
        gj::set_f64(&mut out, "request.generationConfig.maxOutputTokens", max_completion.num);
    }
    let format = gj::get(raw, "response_format");
    if format.exists() {
        let kind = go_lower(trim_space(&format.get("type").bytes()));
        if kind == b"json_object" || kind == b"json_schema" {
            for key in [
                "responseSchema",
                "responseJsonSchema",
                "response_schema",
                "response_json_schema",
            ] {
                gj::delete(&mut out, &format!("{CONFIG}.{key}"));
            }
            gj::set_str(
                &mut out,
                "request.generationConfig.responseMimeType",
                "application/json",
            );
            let schema = format.get("json_schema.schema");
            if kind == b"json_schema" && schema.exists() {
                gj::set_raw(&mut out, "request.generationConfig.responseSchema", &schema.raw);
            }
        }
    }
    let n = gj::get(raw, "n");
    if n.kind == Kind::Number && n.int() > 1 {
        gj::set_int(&mut out, "request.generationConfig.candidateCount", n.int());
    }
    let modalities = gj::get(raw, "modalities");
    if modalities.is_array() {
        let mods: Vec<&str> = modalities
            .array()
            .iter()
            .filter_map(|m| match go_lower(&m.bytes()).as_slice() {
                b"text" => Some("TEXT"),
                b"image" => Some("IMAGE"),
                _ => None,
            })
            .collect();
        if !mods.is_empty() {
            gj::set_strs(&mut out, "request.generationConfig.responseModalities", &mods);
        }
    }
    let image_config = gj::get(raw, "image_config");
    if image_config.is_object() {
        for (from, to) in [("aspect_ratio", "aspectRatio"), ("image_size", "imageSize")] {
            let v = image_config.get(from);
            if v.kind == Kind::String {
                gj::set_str(&mut out, &format!("{CONFIG}.imageConfig.{to}"), &v.s);
            }
        }
    }

    let messages = gj::get(raw, "messages");
    if messages.is_array() {
        let (system, contents) = convert_messages(&messages.array(), &names);
        if !system.is_empty() {
            gj::set_raw(&mut out, "request.systemInstruction", content_node("user", &system));
        }
        gj::set_items(&mut out, "request.contents", &contents);
    }
    apply_tools(&mut out, raw, &names);
    apply_tool_choice(&mut out, raw, &names);
    if go_lower(model.as_bytes()).windows(6).any(|w| w == b"claude") {
        out = sanitize_claude_signatures(out);
    }
    attach_default_safety_settings(out, "request.safetySettings")
}

/// normalizeAntigravityOpenAIThinkingConfig: snake-case and misplaced thinking fields of a
/// passed-through generationConfig move into camelCase `thinkingConfig`.
pub(crate) fn normalize_thinking_config(out: &mut Vec<u8>) {
    let include = format!("{THINKING}.includeThoughts");
    for prefix in ["thinking_config", "thinkingConfig"] {
        for key in ["includeThoughts", "include_thoughts"] {
            let source = format!("{CONFIG}.{prefix}.{key}");
            let value = gj::get(out, &source).into_owned();
            if value.exists() {
                set_bool_if_valid(out, &include, &value);
                if !value.is_bool() {
                    gj::delete(out, &source);
                }
            }
        }
        for (key, target) in [
            ("thinkingLevel", "thinkingLevel"),
            ("thinking_level", "thinkingLevel"),
            ("thinkingBudget", "thinkingBudget"),
            ("thinking_budget", "thinkingBudget"),
        ] {
            let value = gj::get(out, &format!("{CONFIG}.{prefix}.{key}")).into_owned();
            if value.exists() {
                let path = format!("{THINKING}.{target}");
                let current = gj::get(out, &path);
                if !(current.exists() && current.raw == value.raw) {
                    gj::set_raw(out, &path, &value.raw);
                }
            }
        }
    }
    for key in ["includeThoughts", "include_thoughts"] {
        let value = gj::get(out, &format!("{CONFIG}.{key}")).into_owned();
        if value.exists() {
            set_bool_if_valid(out, &include, &value);
        }
    }
    for path in [
        "thinking_config",
        "thinkingConfig.include_thoughts",
        "thinkingConfig.thinking_level",
        "thinkingConfig.thinking_budget",
        "includeThoughts",
        "include_thoughts",
    ] {
        let path = format!("{CONFIG}.{path}");
        if gj::get(out, &path).exists() {
            gj::delete(out, &path);
        }
    }
}

/// setAntigravityOpenAIBoolResultIfValid: booleans are copied when they differ.
fn set_bool_if_valid(out: &mut Vec<u8>, path: &str, value: &Res<'_>) {
    if !value.is_bool() {
        return;
    }
    let want = value.kind == Kind::True;
    if gj::get(out, path).kind != value.kind {
        gj::set_bool(out, path, want);
    }
}

/// The system instruction parts and the contents for `messages`.
fn convert_messages(arr: &[Res<'_>], names: &HashMap<Vec<u8>, Vec<u8>>) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut system = vec![];
    let mut contents = vec![];
    let mut in_conversation = false;
    for (i, m) in arr.iter().enumerate() {
        let role = m.get("role").bytes().into_owned();
        let content = m.get("content");
        let system_like = role == b"system" || role == b"developer";
        if system_like && arr.len() > 1 && !in_conversation {
            if content.kind == Kind::String {
                system.push(text_part(&content.s));
            } else if content.is_object() && content.get("type").bytes().as_ref() == b"text" {
                system.push(text_part(&content.get("text").bytes()));
            } else if content.is_array() {
                for item in content.array() {
                    system.push(text_part(&item.get("text").bytes()));
                }
            }
        } else if role == b"user" || system_like {
            in_conversation = true;
            let parts = user_parts(&content, system_like);
            if !parts.is_empty() {
                contents.push(content_node("user", &parts));
            }
        } else if role == b"assistant" {
            in_conversation = true;
            assistant(arr, i, m, &content, names, &mut contents);
        }
    }
    (system, contents)
}

fn user_parts(content: &Res<'_>, demoted: bool) -> Vec<Vec<u8>> {
    let mut parts = vec![];
    if content.kind == Kind::String {
        parts.push(text_part(&demoted_text(content.s.to_vec(), demoted)));
    } else if content.is_object() && content.get("type").bytes().as_ref() == b"text" {
        parts.push(text_part(&demoted_text(
            content.get("text").bytes().into_owned(),
            demoted,
        )));
    } else if content.is_array() {
        for item in content.array() {
            match item.get("type").bytes().as_ref() {
                b"text" => {
                    let text = item.get("text").bytes().into_owned();
                    if !text.is_empty() {
                        parts.push(text_part(&demoted_text(text, demoted)));
                    }
                }
                b"image_url" => parts.extend(data_url_part(&item.get("image_url.url").bytes())),
                b"video_url" => parts.extend(data_url_part(&item.get("video_url.url").bytes())),
                b"file" => {
                    let filename = item.get("file.filename").bytes();
                    let data = item.get("file.file_data").bytes();
                    if let Some((mime, data)) = normalize_openai_file_data(&filename, b"", &data) {
                        parts.push(inline_part(&mime, &data, false));
                    }
                }
                b"input_audio" => {
                    let data = item.get("input_audio.data").bytes();
                    if !data.is_empty() {
                        let mime = audio_mime(&item.get("input_audio.format").bytes());
                        parts.push(inline_part(&mime, &data, true));
                    }
                }
                _ => {}
            }
        }
    }
    parts
}

fn assistant(
    arr: &[Res<'_>],
    i: usize,
    m: &Res<'_>,
    content: &Res<'_>,
    names: &HashMap<Vec<u8>, Vec<u8>>,
    contents: &mut Vec<Vec<u8>>,
) {
    let mut parts = vec![];
    let reasoning = m.get("reasoning_content");
    if reasoning.kind == Kind::String && !reasoning.s.is_empty() {
        let mut part = text_part(&reasoning.s);
        gj::set_bool(&mut part, "thought", true);
        parts.push(part);
    }
    if content.kind == Kind::String && !content.s.is_empty() {
        parts.push(text_part(&content.s));
    } else if content.is_array() {
        for item in content.array() {
            match item.get("type").bytes().as_ref() {
                b"text" => {
                    let text = item.get("text").bytes();
                    if !text.is_empty() {
                        parts.push(text_part(&text));
                    }
                }
                b"image_url" => parts.extend(data_url_part(&item.get("image_url.url").bytes())),
                _ => {}
            }
        }
    }
    let calls = m.get("tool_calls");
    if !calls.is_array() {
        if !parts.is_empty() {
            contents.push(content_node("model", &parts));
        }
        return;
    }
    // (client ID, function ID, function name) per emitted call.
    let mut called: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = vec![];
    let mut used: HashSet<Vec<u8>> = HashSet::new();
    for call in calls.array() {
        if call.get("type").bytes().as_ref() != b"function" {
            continue;
        }
        let raw_id = call.get("id").bytes().into_owned();
        let base = sanitize_claude_tool_id(&raw_id);
        let mut id = base.clone();
        let mut suffix = 1;
        while !used.insert(id.clone()) {
            id = [&base[..], format!("_{suffix}").as_bytes()].concat();
            suffix += 1;
        }
        let name = map_sanitized_function_name(names, &call.get("function.name").bytes());
        if name.is_empty() {
            continue;
        }
        let args = call.get("function.arguments").bytes();
        let mut part = br#"{"functionCall":{"id":"","name":""}}"#.to_vec();
        gj::set_str(&mut part, "functionCall.id", &id);
        gj::set_str(&mut part, "functionCall.name", &name);
        if gj::valid(&args) {
            gj::set_raw(&mut part, "functionCall.args", &args);
        } else {
            // sjson stores a []byte value as a JSON string.
            gj::set_str(&mut part, "functionCall.args.params", &args);
        }
        gj::set_str(&mut part, "thoughtSignature", SKIP_SIGNATURE);
        parts.push(part);
        called.push((raw_id, id, name));
    }
    if !parts.is_empty() {
        contents.push(content_node("model", &parts));
    }
    let mut responses: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    for next in &arr[i + 1..] {
        let role = next.get("role").bytes();
        if role.as_ref() == b"assistant" {
            break;
        }
        if role.as_ref() == b"tool" {
            let id = next.get("tool_call_id").bytes().into_owned();
            if !id.is_empty() {
                responses.insert(id, next.get("content").bytes().into_owned());
            }
        }
    }
    let mut response_parts = vec![];
    for (raw_id, id, name) in &called {
        let mut part = br#"{"functionResponse":{"id":"","name":""}}"#.to_vec();
        gj::set_str(&mut part, "functionResponse.id", id);
        gj::set_str(&mut part, "functionResponse.name", name);
        let response = responses
            .get(raw_id)
            .filter(|r| !r.is_empty())
            .map_or(&b"{}"[..], Vec::as_slice);
        gj::set_str(&mut part, "functionResponse.response.result", response);
        response_parts.push(part);
    }
    if !response_parts.is_empty() {
        contents.push(content_node("user", &response_parts));
    }
}

/// Function declarations (renamed, deduplicated, without `strict`) and passed-through
/// Google Search, code execution and URL context tools.
fn apply_tools(out: &mut Vec<u8>, raw: &[u8], names: &HashMap<Vec<u8>, Vec<u8>>) {
    let tools = gj::get(raw, "tools");
    let list = tools.array();
    if !tools.is_array() || list.is_empty() {
        return;
    }
    let mut declarations = vec![];
    let (mut search, mut code, mut url) = (vec![], vec![], vec![]);
    for t in &list {
        if t.get("type").bytes().as_ref() == b"function" {
            let f = t.get("function");
            if f.is_object() {
                let Some(mut decl) = function_declaration(&f) else {
                    continue;
                };
                let name = f.get("name");
                let original = name.bytes();
                let mapped = map_sanitized_function_name(names, &original);
                if name.kind != Kind::String || mapped != *original {
                    gj::set_str(&mut decl, "name", &mapped);
                }
                if gj::get(&decl, "strict").exists() {
                    gj::delete(&mut decl, "strict");
                }
                declarations.push(decl);
            }
        }
        for (key, wrapper, nodes) in [
            ("google_search", "googleSearch", &mut search),
            ("code_execution", "codeExecution", &mut code),
            ("url_context", "urlContext", &mut url),
        ] {
            let v = t.get(key);
            if v.exists() {
                let mut node = b"{}".to_vec();
                gj::set_raw(&mut node, wrapper, &v.raw);
                nodes.push(node);
            }
        }
    }
    let deduplicated = deduplicate_declarations(&gj::join(&declarations));
    let has_function = deduplicated.len() > 2;
    if has_function || !search.is_empty() || !code.is_empty() || !url.is_empty() {
        let mut items = vec![];
        if has_function {
            let mut node = br#"{"functionDeclarations":[]}"#.to_vec();
            gj::set_raw(&mut node, "functionDeclarations", &deduplicated);
            items.push(node);
        }
        items.extend(search);
        items.extend(code);
        items.extend(url);
        gj::set_raw(out, "request.tools", gj::join(&items));
    }
}

/// util.DeduplicateFunctionDeclarations: the first declaration of each non-empty name.
pub(crate) fn deduplicate_declarations(raw: &[u8]) -> Vec<u8> {
    let parsed = gj::parse(raw);
    if !parsed.is_array() {
        return raw.to_vec();
    }
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let kept: Vec<Vec<u8>> = parsed
        .array()
        .iter()
        .filter(|d| {
            let name = d.get("name").bytes().into_owned();
            name.is_empty() || seen.insert(name)
        })
        .map(|d| d.raw.to_vec())
        .collect();
    gj::join(&kept)
}

/// applyOpenAIToolChoiceToAntigravity.
fn apply_tool_choice(out: &mut Vec<u8>, raw: &[u8], names: &HashMap<Vec<u8>, Vec<u8>>) {
    let choice = gj::get(raw, "tool_choice");
    if !choice.exists() {
        return;
    }
    let mut allowed: Vec<u8> = vec![];
    let mode = if choice.kind == Kind::String {
        match go_lower(trim_space(&choice.bytes())).as_slice() {
            b"none" => "NONE",
            b"auto" => "AUTO",
            b"required" | b"any" => "ANY",
            _ => "",
        }
    } else if choice.is_object() {
        match go_lower(trim_space(&choice.get("type").bytes())).as_slice() {
            b"none" => "NONE",
            b"function" => {
                allowed = choice.get("function.name").bytes().into_owned();
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
    gj::set_str(out, "request.toolConfig.functionCallingConfig.mode", mode);
    if mode == "NONE" {
        gj::delete(out, "request.tools");
    }
    if !trim_space(&allowed).is_empty() {
        let mapped = map_sanitized_function_name(names, &allowed);
        gj::set_strs(
            out,
            "request.toolConfig.functionCallingConfig.allowedFunctionNames",
            &[mapped],
        );
    }
}

// ---------------------------------------------------------------------------------------
// Responses

/// ConvertAntigravityResponseToOpenAINonStream: the unwrapped response with restored
/// function names through the Gemini Chat converter; no envelope gives an empty body.
fn non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let response = gj::get(body, "response");
    if !response.exists() {
        return Ok(vec![]);
    }
    let restored = restore_names(
        response.raw.to_vec(),
        ctx.original_request,
        &["functionCall", "functionResponse"],
    );
    crate::gemini_chat_response::non_stream(ctx, &restored)
}

/// The Antigravity package's own functionCallIDCounter.
static FUNCTION_CALL_IDS: AtomicU64 = AtomicU64::new(0);

fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(Stream {
        names: disambiguated_tool_name_map(ctx.original_request),
        ..Stream::default()
    })
}

/// ConvertAntigravityResponseToOpenAI: one chunk per upstream line for the first
/// candidate; the finish reason waits for a chunk that also carries usage, and `[DONE]`
/// closes a stream that produced output without one.
#[derive(Default)]
struct Stream {
    created: i64,
    function_index: i64,
    saw_response: bool,
    saw_tool_call: bool,
    saw_finish_reason: bool,
    upstream_finish: Vec<u8>,
    model_version: Vec<u8>,
    response_id: Vec<u8>,
    pending_usage: Vec<u8>,
    names: HashMap<Vec<u8>, Vec<u8>>,
}

const CHUNK: &[u8] = br#"{"id":"","object":"chat.completion.chunk","created":12345,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}"#;

/// setOpenAIUsageMetadata (completion tokens exclude thoughts here).
fn set_usage(t: &mut Vec<u8>, usage: &Res<'_>) {
    let cached = usage.get("cachedContentTokenCount").int();
    gj::set_int(t, "usage.completion_tokens", usage.get("candidatesTokenCount").int());
    let total = usage.get("totalTokenCount");
    if total.exists() {
        gj::set_int(t, "usage.total_tokens", total.int());
    }
    let thoughts = usage.get("thoughtsTokenCount").int();
    gj::set_int(t, "usage.prompt_tokens", usage.get("promptTokenCount").int());
    if thoughts > 0 {
        gj::set_int(t, "usage.completion_tokens_details.reasoning_tokens", thoughts);
    }
    if cached > 0 {
        gj::set_int(t, "usage.prompt_tokens_details.cached_tokens", cached);
    }
}

impl Stream {
    /// resolveOpenAIFinishReason.
    fn finish_reason(&self) -> (&'static str, Vec<u8>) {
        let reason = if self.saw_tool_call {
            "tool_calls"
        } else if self.upstream_finish == b"MAX_TOKENS" {
            "max_tokens"
        } else {
            "stop"
        };
        let native = if self.upstream_finish.is_empty() {
            b"stop".to_vec()
        } else {
            go_lower(&self.upstream_finish)
        };
        (reason, native)
    }

    fn set_finish(&self, t: &mut Vec<u8>) {
        let (reason, native) = self.finish_reason();
        gj::set_str(t, "choices.0.finish_reason", reason);
        gj::set_str(t, "choices.0.native_finish_reason", native);
    }

    fn terminal_chunk(&self) -> Vec<u8> {
        let mut t = br#"{"id":"","object":"chat.completion.chunk","created":0,"model":"model","choices":[{"index":0,"delta":{},"finish_reason":"stop","native_finish_reason":"stop"}]}"#.to_vec();
        gj::set_int(&mut t, "created", self.created);
        if !self.model_version.is_empty() {
            gj::set_str(&mut t, "model", &self.model_version);
        }
        if !self.response_id.is_empty() {
            gj::set_str(&mut t, "id", &self.response_id);
        }
        if !self.pending_usage.is_empty() {
            set_usage(&mut t, &gj::parse(&self.pending_usage));
        }
        self.set_finish(&mut t);
        t
    }

    fn part(&mut self, t: &mut Vec<u8>, part: &Res<'_>) {
        let text = part.get("text");
        let call = part.get("functionCall");
        let mut signature = part.get("thoughtSignature");
        if !signature.exists() {
            signature = part.get("thought_signature");
        }
        let mut data = part.get("inlineData");
        if !data.exists() {
            data = part.get("inline_data");
        }
        let has_signature = signature.exists() && !signature.bytes().is_empty();
        if has_signature && !(text.exists() || call.exists() || data.exists()) {
            return;
        }
        if text.exists() {
            let path = if part.get("thought").bool() {
                "choices.0.delta.reasoning_content"
            } else {
                "choices.0.delta.content"
            };
            gj::set_str(t, path, text.bytes());
            gj::set_str(t, "choices.0.delta.role", "assistant");
        } else if call.exists() {
            self.saw_tool_call = true;
            let mut index = self.function_index;
            self.function_index += 1;
            let calls = gj::get(t, "choices.0.delta.tool_calls");
            if calls.is_array() {
                index = calls.array().len() as i64;
            } else {
                gj::set_raw(t, "choices.0.delta.tool_calls", b"[]");
            }
            let name = restore_sanitized_tool_name(Some(&self.names), &call.get("name").bytes());
            let n = FUNCTION_CALL_IDS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            let mut item =
                br#"{"id": "","index": 0,"type": "function","function": {"name": "","arguments": ""}}"#.to_vec();
            gj::set_str(
                &mut item,
                "id",
                [&name[..], format!("-{}-{n}", now_nanos()).as_bytes()].concat(),
            );
            gj::set_int(&mut item, "index", index);
            gj::set_str(&mut item, "function.name", &name);
            let args = call.get("args");
            if args.exists() {
                gj::set_str(&mut item, "function.arguments", &args.raw);
            }
            gj::set_str(t, "choices.0.delta.role", "assistant");
            gj::set_raw(t, "choices.0.delta.tool_calls.-1", item);
        } else if data.exists() {
            let payload = data.get("data").bytes();
            if payload.is_empty() {
                return;
            }
            let mut mime = data.get("mimeType").bytes();
            if mime.is_empty() {
                mime = data.get("mime_type").bytes();
            }
            if mime.is_empty() {
                mime = std::borrow::Cow::Borrowed(b"image/png");
            }
            let url = [&b"data:"[..], &mime, b";base64,", &payload].concat();
            if !gj::get(t, "choices.0.delta.images").is_array() {
                gj::set_raw(t, "choices.0.delta.images", b"[]");
            }
            let index = gj::get(t, "choices.0.delta.images").array().len();
            let mut image = br#"{"type":"image_url","image_url":{"url":""}}"#.to_vec();
            gj::set_int(&mut image, "index", index as i64);
            gj::set_str(&mut image, "image_url.url", url);
            gj::set_str(t, "choices.0.delta.role", "assistant");
            gj::set_raw(t, "choices.0.delta.images.-1", image);
        }
    }
}

impl GoStream for Stream {
    fn line(&mut self, raw: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        if raw == b"[DONE]" {
            if self.saw_response && !self.saw_finish_reason {
                self.saw_finish_reason = true;
                return Ok(vec![self.terminal_chunk()]);
            }
            return Ok(vec![]);
        }
        if !self.saw_response {
            self.saw_response = has_response_payload(raw);
        }
        let mut t = CHUNK.to_vec();
        let version = gj::get(raw, "response.modelVersion");
        if version.exists() {
            self.model_version = version.bytes().into_owned();
            gj::set_str(&mut t, "model", &self.model_version);
        }
        let create_time = gj::get(raw, "response.createTime");
        if create_time.exists()
            && let Some(created) = parse_rfc3339_unix(&create_time.bytes())
        {
            self.created = created;
        }
        gj::set_int(&mut t, "created", self.created);
        let id = gj::get(raw, "response.responseId");
        if id.exists() {
            self.response_id = id.bytes().into_owned();
            gj::set_str(&mut t, "id", &self.response_id);
        }
        let finish = gj::get(raw, "response.candidates.0.finishReason");
        if finish.exists() {
            self.upstream_finish = go_upper(&finish.bytes());
        }
        let usage = gj::get(raw, "response.usageMetadata");
        if usage.exists() {
            set_usage(&mut t, &usage);
        } else {
            let pending = gj::get(raw, "response.cpaUsageMetadata");
            if pending.exists() {
                self.pending_usage = pending.raw.to_vec();
            }
        }
        let parts = gj::get(raw, "response.candidates.0.content.parts");
        if parts.is_array() {
            for part in parts.array() {
                self.part(&mut t, &part);
            }
        }
        if !self.upstream_finish.is_empty() && usage.exists() {
            self.set_finish(&mut t);
            self.saw_finish_reason = true;
        }
        Ok(vec![t])
    }
}
