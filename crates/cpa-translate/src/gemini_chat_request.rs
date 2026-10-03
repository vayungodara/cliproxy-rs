//! OpenAI Chat Completions request -> Gemini generateContent request
//! (internal/translator/gemini/openai/chat-completions/gemini_openai_request.go).

use crate::{
    Registered,
    common::{self, go_lower, sanitize_function_name, trim_space},
    gemini::attach_default_safety_settings,
    gemini_chat_response as response,
};
use cpa_common::json::{self as gj, Kind, Res};
use std::collections::{HashMap, HashSet};

pub static PAIR: Registered = registered!(
    OpenAI -> Gemini,
    request: |ctx, body| Ok(convert(ctx.model, body)),
    non_stream: response::non_stream,
    go_stream: response::go_stream,
    token_count: None,
);

const SKIP_SIGNATURE: &[u8] = b"skip_thought_signature_validator";

pub(crate) fn text_part(text: &[u8]) -> Vec<u8> {
    let mut part = br#"{"text":""}"#.to_vec();
    gj::set_str(&mut part, "text", text);
    part
}

pub(crate) fn inline_data_part(mime: &[u8], data: &[u8]) -> Vec<u8> {
    let mut part = br#"{"inlineData":{"mime_type":"","data":""}}"#.to_vec();
    gj::set_str(&mut part, "inlineData.mime_type", mime);
    gj::set_str(&mut part, "inlineData.data", data);
    part
}

pub(crate) fn content_node(role: impl AsRef<[u8]>, parts: &[Vec<u8>]) -> Vec<u8> {
    let mut content = br#"{"role":"","parts":[]}"#.to_vec();
    gj::set_str(&mut content, "role", role.as_ref());
    gj::set_raw(&mut content, "parts", gj::join(parts));
    content
}

/// A `data:<mime>;base64,<data>` URL as an inline-data part. Go slices without checking
/// the scheme: after the first five bytes, the text before `;` is the MIME type and the
/// text after the next seven bytes (`base64,`) is the data.
fn data_url_part(url: &[u8]) -> Option<Vec<u8>> {
    if url.len() <= 5 {
        return None;
    }
    let rest = &url[5..];
    let semi = rest.iter().position(|&c| c == b';')?;
    let tail = &rest[semi + 1..];
    (tail.len() > 7).then(|| inline_data_part(&rest[..semi], &tail[7..]))
}

fn demoted_text(text: Vec<u8>, demoted: bool) -> Vec<u8> {
    if !demoted || trim_space(&text).is_empty() {
        return text;
    }
    common::system_reminder_text(&text)
}

pub(crate) fn audio_mime(format: &[u8]) -> Vec<u8> {
    match format {
        b"" | b"wav" => b"audio/wav".to_vec(),
        b"mp3" => b"audio/mpeg".to_vec(),
        b"ogg" => b"audio/ogg".to_vec(),
        b"flac" => b"audio/flac".to_vec(),
        b"aac" => b"audio/aac".to_vec(),
        b"webm" => b"audio/webm".to_vec(),
        b"pcm16" => b"audio/pcm".to_vec(),
        b"g711_ulaw" | b"g711_alaw" => b"audio/basic".to_vec(),
        other => [&b"audio/"[..], other].concat(),
    }
}

/// openAIToolCallGeminiThoughtSignature.
fn tool_call_signature(call: &Res<'_>) -> Vec<u8> {
    use cpa_common::signature::{BlockKind, gemini_replay_signature_or_bypass};
    for path in [
        "extra_content.google.thought_signature",
        "function.extra_content.google.thought_signature",
        "thoughtSignature",
        "thought_signature",
    ] {
        let sig = call.get(path);
        if sig.exists() {
            return gemini_replay_signature_or_bypass(sig.bytes(), BlockKind::GeminiFunctionCall).into_bytes();
        }
    }
    SKIP_SIGNATURE.to_vec()
}

fn is_number(r: &Res<'_>) -> bool {
    r.kind == Kind::Number
}

/// ConvertOpenAIRequestToGemini.
pub(crate) fn convert(model: &str, raw: &[u8]) -> Vec<u8> {
    let mut out = br#"{"contents":[]}"#.to_vec();
    gj::set_str(&mut out, "model", model);

    let config = gj::get(raw, "generationConfig");
    if config.exists() {
        gj::set_raw(&mut out, "generationConfig", &config.raw);
    }
    let effort = gj::get(raw, "reasoning_effort");
    if effort.exists() {
        let effort = go_lower(trim_space(&effort.bytes()));
        if effort == b"auto" {
            gj::set_int(&mut out, "generationConfig.thinkingConfig.thinkingBudget", -1);
        } else if !effort.is_empty() {
            gj::set_str(&mut out, "generationConfig.thinkingConfig.thinkingLevel", effort);
        }
    }
    for (from, to) in [
        ("temperature", "generationConfig.temperature"),
        ("top_p", "generationConfig.topP"),
        ("top_k", "generationConfig.topK"),
    ] {
        let v = gj::get(raw, from);
        if is_number(&v) {
            gj::set_f64(&mut out, to, v.num);
        }
    }
    let max_tokens = gj::get(raw, "max_tokens");
    let max_completion = gj::get(raw, "max_completion_tokens");
    if is_number(&max_tokens) {
        gj::set_f64(&mut out, "generationConfig.maxOutputTokens", max_tokens.num);
    } else if is_number(&max_completion) {
        gj::set_f64(&mut out, "generationConfig.maxOutputTokens", max_completion.num);
    }
    let n = gj::get(raw, "n");
    if is_number(&n) && n.int() > 1 {
        gj::set_int(&mut out, "generationConfig.candidateCount", n.int());
    }
    apply_response_format(&mut out, raw);
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
            gj::set_strs(&mut out, "generationConfig.responseModalities", &mods);
        }
    }
    let image_config = gj::get(raw, "image_config");
    if image_config.is_object() {
        for (from, to) in [
            ("aspect_ratio", "generationConfig.imageConfig.aspectRatio"),
            ("image_size", "generationConfig.imageConfig.imageSize"),
        ] {
            let v = image_config.get(from);
            if v.kind == Kind::String {
                gj::set_str(&mut out, to, &v.s);
            }
        }
    }

    let messages = gj::get(raw, "messages");
    if messages.is_array() {
        let (system, contents) = convert_messages(&messages.array());
        if !system.is_empty() {
            gj::set_raw(&mut out, "systemInstruction", content_node("user", &system));
        }
        let mut contents = contents;
        if contents
            .last()
            .is_some_and(|c| gj::get(c, "role").bytes().as_ref() == b"model")
        {
            contents.pop();
        }
        gj::set_items(&mut out, "contents", &contents);
    }

    apply_tools(&mut out, raw);
    attach_default_safety_settings(out, "safetySettings")
}

fn apply_response_format(out: &mut Vec<u8>, raw: &[u8]) {
    let format = gj::get(raw, "response_format");
    if !format.exists() {
        return;
    }
    match go_lower(trim_space(&format.get("type").bytes())).as_slice() {
        b"json_object" => {
            gj::set_str(out, "generationConfig.responseMimeType", "application/json");
        }
        b"json_schema" => {
            gj::set_str(out, "generationConfig.responseMimeType", "application/json");
            gj::delete(out, "generationConfig.responseSchema");
            let schema = format.get("json_schema.schema");
            if schema.exists() {
                gj::set_raw(out, "generationConfig.responseJsonSchema", &schema.raw);
            }
        }
        _ => {}
    }
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
                    if let Some((mime, data)) = common::normalize_openai_file_data(&filename, b"", &data) {
                        parts.push(inline_data_part(&mime, &data));
                    }
                }
                b"input_audio" => {
                    let data = item.get("input_audio.data").bytes();
                    if !data.is_empty() {
                        let mime = audio_mime(&item.get("input_audio.format").bytes());
                        parts.push(inline_data_part(&mime, &data));
                    }
                }
                _ => {}
            }
        }
    }
    parts
}

/// The system instruction parts and the contents for `messages`.
fn convert_messages(arr: &[Res<'_>]) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
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
            assistant(arr, i, m, &content, &mut contents);
        }
    }
    (system, contents)
}

fn assistant(arr: &[Res<'_>], i: usize, m: &Res<'_>, content: &Res<'_>, contents: &mut Vec<Vec<u8>>) {
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
    let mut called: Vec<(Vec<u8>, Vec<u8>)> = vec![];
    for call in calls.array() {
        if call.get("type").bytes().as_ref() != b"function" {
            continue;
        }
        let name = sanitize_function_name(&call.get("function.name").bytes());
        if name.is_empty() {
            continue;
        }
        let mut part = br#"{"functionCall":{"name":""}}"#.to_vec();
        gj::set_str(&mut part, "functionCall.name", &name);
        gj::set_raw(&mut part, "functionCall.args", call.get("function.arguments").bytes());
        gj::set_str(&mut part, "thoughtSignature", tool_call_signature(&call));
        parts.push(part);
        called.push((call.get("id").bytes().into_owned(), name));
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
                responses.insert(id, next.get("content").raw.to_vec());
            }
        }
    }
    let mut response_parts = vec![];
    for (id, name) in &called {
        let mut part = br#"{"functionResponse":{"name":"","response":{"result":""}}}"#.to_vec();
        gj::set_str(&mut part, "functionResponse.name", name);
        let response = responses
            .get(id)
            .filter(|r| !r.is_empty())
            .map_or(&b"{}"[..], Vec::as_slice);
        // sjson stores a []byte value as a JSON string.
        gj::set_str(&mut part, "functionResponse.response.result", response);
        response_parts.push(part);
    }
    if !response_parts.is_empty() {
        contents.push(content_node("user", &response_parts));
    }
}

fn apply_tools(out: &mut Vec<u8>, raw: &[u8]) {
    let mut allowed: HashSet<Vec<u8>> = HashSet::new();
    let mut is_allowed_tools = false;
    let mut allowed_mode = b"auto".to_vec();
    let choice = gj::get(raw, "tool_choice");
    if choice.is_object() && choice.get("type").bytes().as_ref() == b"allowed_tools" {
        is_allowed_tools = true;
        let mut list = choice.get("allowed_tools.tools").array();
        if list.is_empty() {
            list = choice.get("tools").array();
        }
        for t in list {
            let mut name = trim_space(&t.get("function.name").bytes()).to_vec();
            if name.is_empty() {
                name = trim_space(&t.get("name").bytes()).to_vec();
            }
            if !name.is_empty() {
                allowed.insert(name);
            }
        }
        let mut mode = go_lower(trim_space(&choice.get("allowed_tools.mode").bytes()));
        if mode.is_empty() {
            mode = go_lower(trim_space(&choice.get("mode").bytes()));
        }
        if !mode.is_empty() {
            allowed_mode = mode;
        }
    }

    let mut declared: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut sanitized_counts: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut declarations: Vec<Vec<u8>> = vec![];
    let mut has_strict = false;
    let tools = gj::get(raw, "tools");
    let tool_list = tools.array();
    if tools.is_array() && !tool_list.is_empty() {
        let mut search = vec![];
        let mut code = vec![];
        let mut url = vec![];
        for t in &tool_list {
            if t.get("type").bytes().as_ref() == b"function" {
                let f = t.get("function");
                if f.is_object() {
                    let name = f.get("name");
                    let original = name.bytes().into_owned();
                    if is_allowed_tools && !allowed.contains(&original) {
                        continue;
                    }
                    let sanitized = sanitize_function_name(&original);
                    *sanitized_counts.entry(sanitized.clone()).or_default() += 1;
                    declared.insert(original.clone(), sanitized.clone());
                    let Some(mut decl) = function_declaration(&f) else {
                        continue;
                    };
                    if name.kind != Kind::String || sanitized != original {
                        gj::set_str(&mut decl, "name", &sanitized);
                    }
                    let params = gj::get(&decl, "parametersJsonSchema").into_owned();
                    if params.exists() {
                        let cleaned = cpa_common::gemini_schema::for_gemini_json_schema(&params.raw);
                        if cleaned != *params.raw {
                            gj::set_raw(&mut decl, "parametersJsonSchema", cleaned);
                        }
                    }
                    let mut strict = gj::get(&decl, "strict").into_owned();
                    let decl_has_strict = strict.exists();
                    if !strict.exists() {
                        strict = f.get("strict").into_owned();
                        if !strict.exists() {
                            strict = t.get("strict").into_owned();
                        }
                    }
                    if strict.exists() {
                        has_strict |= strict.kind == Kind::True;
                        if decl_has_strict {
                            gj::delete(&mut decl, "strict");
                        }
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
        if !declarations.is_empty() || !search.is_empty() || !code.is_empty() || !url.is_empty() {
            let mut items = vec![];
            if !declarations.is_empty() {
                let mut node = br#"{"functionDeclarations":[]}"#.to_vec();
                gj::set_raw(&mut node, "functionDeclarations", gj::join(&declarations));
                items.push(node);
            }
            items.extend(search);
            items.extend(code);
            items.extend(url);
            gj::set_raw(out, "tools", gj::join(&items));
        }
    }

    const MODE: &str = "toolConfig.functionCallingConfig.mode";
    const ALLOWED: &str = "toolConfig.functionCallingConfig.allowedFunctionNames";
    if sanitized_counts.values().any(|&c| c > 1) {
        gj::set_str(out, MODE, "NONE");
    } else if is_allowed_tools {
        if declarations.is_empty() {
            gj::set_str(out, MODE, "NONE");
        } else if allowed_mode == b"required" || allowed_mode == b"any" {
            gj::set_str(out, MODE, "ANY");
            let names: Vec<Vec<u8>> = declarations
                .iter()
                .map(|d| gj::get(d, "name").bytes().into_owned())
                .collect();
            gj::set_strs(out, ALLOWED, &names);
        } else {
            gj::set_str(out, MODE, if has_strict { "VALIDATED" } else { "AUTO" });
        }
    } else if choice.exists() && choice.kind != Kind::Null {
        let kind = if choice.kind == Kind::String {
            go_lower(trim_space(&choice.s))
        } else if choice.is_object() {
            go_lower(trim_space(&choice.get("type").bytes()))
        } else {
            vec![]
        };
        match kind.as_slice() {
            b"auto" => gj::set_str(out, MODE, if has_strict { "VALIDATED" } else { "AUTO" }),
            b"none" => gj::set_str(out, MODE, "NONE"),
            b"required" | b"any" => gj::set_str(out, MODE, "ANY"),
            b"function" | b"tool" => {
                let mut name = trim_space(&choice.get("function.name").bytes()).to_vec();
                if name.is_empty() {
                    name = trim_space(&choice.get("name").bytes()).to_vec();
                }
                match declared.get(&name) {
                    Some(sanitized) if sanitized_counts.get(sanitized) == Some(&1) => {
                        gj::set_str(out, MODE, "ANY");
                        gj::set_strs(out, ALLOWED, std::slice::from_ref(sanitized))
                    }
                    _ => gj::set_str(out, MODE, "NONE"),
                }
            }
            _ => gj::set_str(out, MODE, "NONE"),
        };
    } else if has_strict && !declarations.is_empty() {
        gj::set_str(out, MODE, "VALIDATED");
    }

    if gj::get(raw, "parallel_tool_calls").kind == Kind::False {
        gj::set_str(out, MODE, "NONE");
        gj::delete(out, ALLOWED);
    }
}

/// The function object with `parameters` renamed to `parametersJsonSchema`, or an empty
/// object schema added. `None` where Go's sjson edits fail and the tool is skipped.
pub(crate) fn function_declaration(f: &Res<'_>) -> Option<Vec<u8>> {
    let raw = f.raw.to_vec();
    if f.get("parameters").exists() {
        // util.RenameKey, falling back to an empty object schema when it fails.
        let value = gj::get(&raw, "parameters");
        if let Ok(renamed) = gj::try_set_raw(&raw, "parametersJsonSchema", &value.raw)
            .and_then(|next| gj::try_delete(&next, "parameters"))
        {
            return Some(renamed);
        }
    }
    let with_type = gj::try_set_str(&raw, "parametersJsonSchema.type", "object").ok()?;
    gj::try_set_raw(&with_type, "parametersJsonSchema.properties", b"{}").ok()
}
