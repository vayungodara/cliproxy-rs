//! Gemini Interactions request -> Codex (OpenAI Responses) request, and Codex events back
//! to Interactions responses (internal/translator/codex/interactions).

use cpa_common::json::{self as gj, Kind, Res};

use crate::common::{format_rfc3339_utc, go_lower, now_nanos, now_unix, sse_event, trim_space};
use crate::stream::GoStream;
use crate::{Error, Registered, ResponseCtx};

pub static PAIR: Registered = registered!(
    Interactions -> Codex,
    request: |ctx, body| Ok(convert(ctx.model, body, ctx.stream)),
    non_stream: non_stream,
    go_stream: go_stream,
    token_count: None,
);

const NAME_LIMIT: usize = 64;

/// shortenCodexToolNameIfNeeded.
fn shorten_name(name: &[u8]) -> Vec<u8> {
    if name.len() <= NAME_LIMIT {
        return name.to_vec();
    }
    if name.starts_with(b"mcp__")
        && let Some(idx) = name.windows(2).rposition(|w| w == b"__")
        && idx > 0
    {
        let mut candidate = [&b"mcp__"[..], &name[idx + 2..]].concat();
        candidate.truncate(NAME_LIMIT);
        return candidate;
    }
    name[..NAME_LIMIT].to_vec()
}

/// firstString: the first path that exists, even when empty.
fn first_string(value: &Res<'_>, paths: &[&str]) -> Vec<u8> {
    paths
        .iter()
        .map(|p| value.get(*p))
        .find(Res::exists)
        .map(|v| v.bytes().into_owned())
        .unwrap_or_default()
}

fn first_existing<'a>(value: &Res<'a>, paths: &[&str]) -> Res<'a> {
    paths
        .iter()
        .map(|p| value.get(*p))
        .find(Res::exists)
        .unwrap_or_default()
}

fn audio_format(mime: &[u8]) -> &'static str {
    match go_lower(trim_space(mime)).as_slice() {
        b"audio/wav" | b"audio/wave" | b"audio/x-wav" => "wav",
        b"audio/flac" => "flac",
        b"audio/opus" | b"audio/ogg" => "opus",
        b"audio/pcm" | b"audio/l16" => "pcm16",
        _ => "mp3",
    }
}

fn file_name(mime: &[u8]) -> &'static str {
    let lower = go_lower(trim_space(mime));
    match lower.as_slice() {
        b"application/pdf" => "document.pdf",
        b"text/plain" => "document.txt",
        b"text/csv" => "document.csv",
        b"application/json" => "document.json",
        b"application/xml" | b"text/xml" => "document.xml",
        _ if lower.starts_with(b"video/") => "video",
        _ => "document",
    }
}

/// interactionsCodexDefaultRole.
fn default_role(role: &[u8], fallback: &'static str) -> &'static str {
    match go_lower(trim_space(role)).as_slice() {
        b"model" | b"assistant" => "assistant",
        b"developer" | b"system" => "developer",
        b"user" => "user",
        _ if fallback == "assistant" || fallback == "developer" => fallback,
        _ => "user",
    }
}

fn text_part(role: &str, text: &[u8]) -> Vec<u8> {
    let mut part = br#"{"type":"","text":""}"#.to_vec();
    gj::set_str(
        &mut part,
        "type",
        if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        },
    );
    gj::set_str(&mut part, "text", text);
    part
}

fn message(role: &str, part: &[u8]) -> Vec<u8> {
    let mut message = br#"{"type":"message","role":"","content":[]}"#.to_vec();
    gj::set_str(&mut message, "role", role);
    gj::set_raw(&mut message, "content", gj::join(&[part]));
    message
}

fn input_image(url: &[u8]) -> Vec<u8> {
    let mut item = br#"{"type":"input_image","image_url":""}"#.to_vec();
    gj::set_str(&mut item, "image_url", url);
    item
}

fn input_file(field: &str, value: &[u8], filename: &[u8]) -> Vec<u8> {
    let mut item = format!(r#"{{"type":"input_file","{field}":"","filename":""}}"#).into_bytes();
    gj::set_str(&mut item, field, value);
    gj::set_str(&mut item, "filename", filename);
    item
}

/// interactionsCodexImagePart.
fn image_part(part: &Res<'_>) -> Option<Vec<u8>> {
    let url = part.get("url");
    if url.exists() {
        return Some(input_image(&url.bytes()));
    }
    let uri = first_string(part, &["file_uri", "fileUri"]);
    if !uri.is_empty() {
        return Some(input_image(&uri));
    }
    let mime = first_string(part, &["mime_type", "mimeType"]);
    let data = part.get("data").bytes().into_owned();
    if mime.is_empty() || data.is_empty() {
        return None;
    }
    Some(input_image(&[b"data:", &mime[..], b";base64,", &data[..]].concat()))
}

/// interactionsCodexAudioPart.
fn audio_part(part: &Res<'_>) -> Option<Vec<u8>> {
    let mime = first_string(part, &["mime_type", "mimeType"]);
    let data = part.get("data").bytes().into_owned();
    if mime.is_empty() || data.is_empty() {
        return None;
    }
    let mut item = br#"{"type":"input_audio","input_audio":{"data":"","format":""}}"#.to_vec();
    gj::set_str(&mut item, "input_audio.data", &data);
    gj::set_str(&mut item, "input_audio.format", audio_format(&mime));
    Some(item)
}

/// interactionsCodexFilePart.
fn file_part(part: &Res<'_>) -> Option<Vec<u8>> {
    let file_data = part.get("file.file_data").bytes();
    if !file_data.is_empty() {
        return Some(input_file("file_data", &file_data, &part.get("file.filename").bytes()));
    }
    let mime = first_string(part, &["mime_type", "mimeType"]);
    let uri = first_string(part, &["file_uri", "fileUri", "url"]);
    if !uri.is_empty() {
        return Some(input_file("file_url", &uri, file_name(&mime).as_bytes()));
    }
    let data = part.get("data").bytes().into_owned();
    if mime.is_empty() || data.is_empty() {
        return None;
    }
    Some(input_file("file_data", &data, file_name(&mime).as_bytes()))
}

/// interactionsCodexInlinePart: re-parsed from `{"mime_type":%q,"data":%q}`, as Go does.
fn inline_part(inline: &Res<'_>) -> Option<Vec<u8>> {
    let mime = first_string(inline, &["mime_type", "mimeType"]);
    let data = inline.get("data").bytes().into_owned();
    if mime.is_empty() || data.is_empty() {
        return None;
    }
    let quoted = format!(
        r#"{{"mime_type":{},"data":{}}}"#,
        cpa_common::gostr::quote(&mime),
        cpa_common::gostr::quote(&data)
    );
    let part = gj::parse(quoted.as_bytes());
    let lower = go_lower(&mime);
    if lower.starts_with(b"image/") {
        image_part(&part)
    } else if lower.starts_with(b"audio/") {
        audio_part(&part)
    } else {
        file_part(&part)
    }
}

/// interactionsCodexFileDataPart.
fn file_data_part(file: &Res<'_>) -> Option<Vec<u8>> {
    let mime = first_string(file, &["mime_type", "mimeType"]);
    let uri = first_string(file, &["file_uri", "fileUri"]);
    if uri.is_empty() {
        return None;
    }
    if go_lower(&mime).starts_with(b"image/") {
        return Some(input_image(&uri));
    }
    Some(input_file("file_url", &uri, file_name(&mime).as_bytes()))
}

/// interactionsCodexMessagePart.
fn message_part(part: &Res<'_>, role: &str) -> Option<Vec<u8>> {
    let text = part.get("text");
    if text.exists() {
        return Some(text_part(role, &text.bytes()));
    }
    match go_lower(trim_space(&part.get("type").bytes())).as_slice() {
        b"text" | b"" => None,
        b"image" => image_part(part),
        b"image_url" => Some(input_image(&part.get("image_url.url").bytes())),
        b"audio" => audio_part(part),
        b"input_audio" => {
            let mut item = br#"{"type":"input_audio","input_audio":{}}"#.to_vec();
            let audio = part.get("input_audio");
            if audio.exists() {
                gj::set_raw(&mut item, "input_audio", &audio.raw);
            }
            Some(item)
        }
        b"video" | b"document" | b"file" => file_part(part),
        _ => {
            let inline = first_existing(part, &["inline_data", "inlineData"]);
            if inline.exists() {
                return inline_part(&inline);
            }
            let file = first_existing(part, &["file_data", "fileData"]);
            if file.exists() {
                return file_data_part(&file);
            }
            None
        }
    }
}

/// interactionsCodexContentText.
fn content_text(content: &Res<'_>) -> Vec<u8> {
    if content.kind == Kind::String {
        return content.s.to_vec();
    }
    if content.is_object() {
        return content.get("text").bytes().into_owned();
    }
    let mut out: Vec<u8> = vec![];
    if content.is_array() {
        content.each(|_, part| {
            let text = part.get("text").bytes();
            if !text.is_empty() {
                if !out.is_empty() {
                    out.push(b'\n');
                }
                out.extend_from_slice(&text);
            }
            true
        });
    }
    out
}

/// interactionsCodexCallID.
fn call_id(step: &Res<'_>) -> Vec<u8> {
    let id = trim_space(&step.get("call_id").bytes()).to_vec();
    if !id.is_empty() {
        return id;
    }
    trim_space(&step.get("id").bytes()).to_vec()
}

/// interactionsCodexJSONString / interactionsCodexOutputString for existing values.
fn json_string(value: &Res<'_>) -> Vec<u8> {
    if value.kind == Kind::String {
        value.s.to_vec()
    } else {
        value.raw.to_vec()
    }
}

struct Input {
    items: Vec<Vec<u8>>,
}

impl Input {
    fn text(&mut self, role: &str, text: &[u8]) {
        self.items.push(message(role, &text_part(role, text)));
    }

    fn content(&mut self, content: &Res<'_>, role: &str) {
        if !content.exists() {
            return;
        }
        if content.kind == Kind::String {
            self.text(role, &content.s);
        } else if content.is_array() {
            content.each(|_, part| {
                if let Some(item) = message_part(&part, role).filter(|i| !i.is_empty()) {
                    self.items.push(message(role, &item));
                }
                true
            });
        } else if content.is_object()
            && let Some(item) = message_part(content, role).filter(|i| !i.is_empty())
        {
            self.items.push(message(role, &item));
        }
    }

    fn step(&mut self, step: &Res<'_>, role: &'static str) {
        if step.kind == Kind::String {
            self.text(role, &step.s);
            return;
        }
        let steps = step.get("steps");
        if steps.is_array() {
            let role = default_role(&step.get("role").bytes(), role);
            steps.each(|_, nested| {
                self.step(&nested, role);
                true
            });
            return;
        }
        match go_lower(trim_space(&step.get("type").bytes())).as_slice() {
            b"function_call" => {
                let mut item = br#"{"type":"function_call"}"#.to_vec();
                let name = step.get("name");
                if name.exists() {
                    gj::set_str(&mut item, "name", shorten_name(&name.bytes()));
                }
                let id = call_id(step);
                if !id.is_empty() {
                    gj::set_str(&mut item, "call_id", &id);
                }
                let args = first_existing(step, &["arguments", "args"]);
                if args.exists() {
                    gj::set_str(&mut item, "arguments", json_string(&args));
                }
                self.items.push(item);
            }
            b"function_result" | b"function_call_output" => {
                let mut item = br#"{"type":"function_call_output"}"#.to_vec();
                let id = call_id(step);
                if !id.is_empty() {
                    gj::set_str(&mut item, "call_id", &id);
                }
                let output = first_existing(step, &["result", "output"]);
                if output.exists() {
                    gj::set_str(&mut item, "output", json_string(&output));
                }
                self.items.push(item);
            }
            b"model_output" | b"assistant" => self.content(&step.get("content"), "assistant"),
            b"thought" | b"reasoning" => {
                let mut text = content_text(&step.get("content"));
                if text.is_empty() {
                    text = step.get("text").bytes().into_owned();
                }
                let mut item = br#"{"type":"reasoning"}"#.to_vec();
                if !text.is_empty() {
                    gj::set_str(&mut item, "content", &text);
                }
                let id = step.get("id");
                if id.exists() {
                    gj::set_str(&mut item, "id", id.bytes());
                }
                self.items.push(item);
            }
            _ => {
                let role = default_role(&step.get("role").bytes(), role);
                let content = step.get("content");
                let text = step.get("text");
                if content.exists() {
                    self.content(&content, role);
                } else if text.exists() {
                    self.text(role, &text.bytes());
                }
            }
        }
    }
}

/// interactionsCodexReasoningEffort.
fn reasoning_effort(cfg: &Res<'_>) -> Option<Vec<u8>> {
    for path in [
        "thinking_level",
        "thinkingLevel",
        "thinking_config.thinking_level",
        "thinking_config.thinkingLevel",
        "thinkingConfig.thinking_level",
        "thinkingConfig.thinkingLevel",
        "reasoning.effort",
    ] {
        let value = cfg.get(path);
        if value.exists() {
            let effort = go_lower(trim_space(&value.bytes()));
            if !effort.is_empty() {
                return Some(effort);
            }
        }
    }
    for path in [
        "thinking_budget",
        "thinkingBudget",
        "thinking_config.thinking_budget",
        "thinking_config.thinkingBudget",
        "thinkingConfig.thinking_budget",
        "thinkingConfig.thinkingBudget",
    ] {
        let value = cfg.get(path);
        if value.exists()
            && let Some(level) = cpa_common::thinking::convert_budget_to_level(value.int())
        {
            return Some(level.as_bytes().to_vec());
        }
    }
    None
}

/// interactionsCodexReasoningSummary.
fn reasoning_summary(cfg: &Res<'_>) -> Option<&'static str> {
    for path in ["thinking_summaries", "thinkingSummaries", "reasoning.summary"] {
        let value = cfg.get(path);
        if value.kind == Kind::String {
            match go_lower(trim_space(&value.s)).as_slice() {
                b"auto" => return Some("auto"),
                b"none" => return Some("none"),
                _ => {}
            }
        }
    }
    for path in [
        "include_thoughts",
        "includeThoughts",
        "thinking_config.include_thoughts",
        "thinking_config.includeThoughts",
        "thinkingConfig.include_thoughts",
        "thinkingConfig.includeThoughts",
    ] {
        match cfg.get(path).kind {
            Kind::True => return Some("auto"),
            Kind::False => return Some("none"),
            _ => {}
        }
    }
    None
}

/// Generation config fields copied raw (copyRawPaths).
// ponytail: Go ranges over a map here, so its output key order (and which alias wins when
// both spellings are present) varies between runs; this uses declaration order, the later
// alias winning. Goldens compare these outputs against every order Go produced.
const COPY_RAW_PATHS: [(&str, &str); 21] = [
    ("max_output_tokens", "max_output_tokens"),
    ("maxOutputTokens", "max_output_tokens"),
    ("max_tokens", "max_output_tokens"),
    ("temperature", "temperature"),
    ("top_p", "top_p"),
    ("topP", "top_p"),
    ("presence_penalty", "presence_penalty"),
    ("presencePenalty", "presence_penalty"),
    ("frequency_penalty", "frequency_penalty"),
    ("frequencyPenalty", "frequency_penalty"),
    ("parallel_tool_calls", "parallel_tool_calls"),
    ("parallelToolCalls", "parallel_tool_calls"),
    ("response_format", "response_format"),
    ("responseFormat", "response_format"),
    ("text", "text"),
    ("verbosity", "text.verbosity"),
    ("truncation", "truncation"),
    ("tool_choice", "tool_choice"),
    ("toolChoice", "tool_choice"),
    ("service_tier", "service_tier"),
    ("serviceTier", "service_tier"),
];

/// cleanedCodexToolParameters.
pub(crate) fn cleaned_parameters(params: &Res<'_>) -> Vec<u8> {
    let mut cleaned = params.raw.to_vec();
    if params.get("$schema").exists() {
        gj::delete(&mut cleaned, "$schema");
    }
    if params.get("additionalProperties").kind != Kind::False {
        gj::set_bool(&mut cleaned, "additionalProperties", false);
    }
    cleaned
}

/// codexToolFromDeclaration, as json.Marshal writes the map (sorted keys); `None` when
/// the parameters are not valid JSON (Marshal fails).
fn tool_from_declaration(declaration: &Res<'_>) -> Option<Vec<u8>> {
    let mut out = b"{".to_vec();
    let description = declaration.get("description");
    if description.exists() {
        out.extend_from_slice(b"\"description\":");
        out.extend_from_slice(&gj::quote(description.bytes()));
        out.push(b',');
    }
    out.extend_from_slice(b"\"name\":");
    out.extend_from_slice(&gj::quote(shorten_name(&declaration.get("name").bytes())));
    let params = first_existing(
        declaration,
        &["parameters", "parametersJsonSchema", "parameters_json_schema"],
    );
    if params.exists() {
        let cleaned = cleaned_parameters(&params);
        if !gj::std_valid(&cleaned) {
            return None;
        }
        out.extend_from_slice(b",\"parameters\":");
        out.extend_from_slice(&gj::compact(&cleaned, true));
    }
    out.extend_from_slice(br#","strict":false,"type":"function"}"#);
    Some(out)
}

pub(crate) fn set_raw_if_different(out: &mut Vec<u8>, path: &str, value: &Res<'_>) {
    let current = gj::get(out, path);
    if current.exists() && current.raw == value.raw {
        return;
    }
    if let Ok(updated) = gj::try_set_raw(out, path, &value.raw) {
        *out = updated;
    }
}

/// ConvertInteractionsRequestToCodex.
fn convert(model: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    let root = gj::parse(raw);
    let mut out = br#"{"model":"","instructions":"","input":[]}"#.to_vec();
    gj::set_str(&mut out, "model", model);
    if stream || root.get("stream").bool() {
        gj::set_bool(&mut out, "stream", true);
    }

    let system = first_existing(&root, &["system_instruction", "systemInstruction"]);
    if system.exists() {
        let text = system.get("text");
        let parts = system.get("parts");
        if system.kind == Kind::String {
            gj::set_str(&mut out, "instructions", &system.s);
        } else if text.kind == Kind::String {
            gj::set_str(&mut out, "instructions", &text.s);
        } else if parts.is_array() {
            let joined = content_text(&parts);
            if !joined.is_empty() {
                gj::set_str(&mut out, "instructions", &joined);
            }
        }
    }

    let cfg = first_existing(&root, &["generation_config", "generationConfig"]);
    if cfg.exists() {
        let reasoning = cfg.get("reasoning");
        if reasoning.exists() {
            gj::set_raw(&mut out, "reasoning", &reasoning.raw);
        }
        if let Some(effort) = reasoning_effort(&cfg) {
            gj::set_str(&mut out, "reasoning.effort", &effort);
        }
        if let Some(summary) = reasoning_summary(&cfg) {
            gj::set_str(&mut out, "reasoning.summary", summary);
        }
        for (source, target) in COPY_RAW_PATHS {
            let value = cfg.get(source);
            if value.exists() {
                gj::set_raw(&mut out, target, &value.raw);
            }
        }
    } else {
        let reasoning = root.get("reasoning");
        if reasoning.exists() {
            gj::set_raw(&mut out, "reasoning", &reasoning.raw);
        }
    }

    let mut input = Input { items: vec![] };
    let value = root.get("input");
    if value.kind == Kind::String {
        input.text("user", &value.s);
    } else if value.is_array() {
        value.each(|_, step| {
            input.step(&step, "user");
            true
        });
    } else if value.get("steps").is_array() {
        let role = default_role(&value.get("role").bytes(), "user");
        value.get("steps").each(|_, step| {
            input.step(&step, role);
            true
        });
    } else if value.exists() {
        input.step(&value, "user");
    }
    gj::set_items(&mut out, "input", &input.items);

    let tools = root.get("tools");
    if tools.exists() {
        if !tools.is_array() {
            gj::set_raw(&mut out, "tools", &tools.raw);
        } else {
            let mut normalized: Vec<Option<Vec<u8>>> = vec![];
            tools.each(|_, tool| {
                let declarations = first_existing(&tool, &["function_declarations", "functionDeclarations"]);
                if declarations.exists() {
                    if declarations.is_array() {
                        declarations.each(|_, d| {
                            if d.get("name").exists() {
                                normalized.push(tool_from_declaration(&d));
                            }
                            true
                        });
                    }
                } else if tool.get("name").exists() {
                    normalized.push(tool_from_declaration(&tool));
                }
                true
            });
            match normalized.into_iter().collect::<Option<Vec<_>>>() {
                Some(list) if !list.is_empty() => {
                    gj::set_raw(&mut out, "tools", gj::join(&list));
                    if !gj::get(&out, "tool_choice").exists() {
                        gj::set_str(&mut out, "tool_choice", "auto");
                    }
                }
                _ => {
                    gj::set_raw(&mut out, "tools", &tools.raw);
                }
            }
        }
    }

    let tier = root.get("service_tier");
    if tier.kind == Kind::String && matches!(go_lower(trim_space(&tier.s)).as_slice(), b"priority" | b"fast") {
        let current = gj::get(&out, "service_tier");
        if current.kind != Kind::String || current.s.as_ref() != b"priority" {
            gj::set_str(&mut out, "service_tier", "priority");
        }
    }
    let choice = root.get("tool_choice");
    if choice.exists() {
        set_raw_if_different(&mut out, "tool_choice", &choice);
    }
    for path in ["parallel_tool_calls", "store", "metadata", "include", "truncation"] {
        let value = root.get(path);
        if value.exists() {
            set_raw_if_different(&mut out, path, &value);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// Responses (interactions_codex_response.go)

/// `interaction_%d` from the current UnixNano.
fn interaction_id() -> Vec<u8> {
    format!("interaction_{}", now_nanos()).into_bytes()
}

#[derive(Default)]
struct State {
    started: bool,
    completed: bool,
    done: bool,
    step_open: bool,
    step_type: &'static str,
    step_index: i64,
    next_step: i64,
    id: Vec<u8>,
    model: Vec<u8>,
    created_at: i64,
    has_output_text: bool,
    call_name: Vec<u8>,
    call_id: Vec<u8>,
}

pub fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(State {
        id: interaction_id(),
        model: ctx.model.as_bytes().to_vec(),
        ..Default::default()
    })
}

/// codexItemCallID.
fn item_call_id(item: &Res<'_>) -> Vec<u8> {
    call_id(item)
}

/// codexContentText: a string `text` or `content`.
fn codex_content_text(content: &Res<'_>) -> Vec<u8> {
    for path in ["text", "content"] {
        let value = content.get(path);
        if value.kind == Kind::String {
            return value.s.to_vec();
        }
    }
    vec![]
}

fn joined(items: &Res<'_>, text_of: impl Fn(&Res<'_>) -> Vec<u8>) -> Vec<u8> {
    let mut out: Vec<u8> = vec![];
    items.each(|_, part| {
        let text = text_of(&part);
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

/// codexReasoningText: string or array content, else the summary.
fn reasoning_text(item: &Res<'_>) -> Vec<u8> {
    let content = item.get("content");
    if content.exists() {
        if content.kind == Kind::String {
            return content.s.to_vec();
        }
        if content.is_array() {
            return joined(&content, |part| {
                let text = codex_content_text(part);
                if text.is_empty() {
                    part.get("summary_text").bytes().into_owned()
                } else {
                    text
                }
            });
        }
    }
    let summary = item.get("summary");
    if summary.kind == Kind::String {
        return summary.s.to_vec();
    }
    if summary.is_array() {
        return joined(&summary, codex_content_text);
    }
    vec![]
}

/// codexArgumentsJSON.
fn arguments_json(arguments: &Res<'_>) -> Option<Vec<u8>> {
    if !arguments.exists() {
        return None;
    }
    if arguments.kind == Kind::String {
        let parsed = gj::parse(&arguments.s);
        return Some(if parsed.exists() && parsed.is_object() {
            arguments.s.to_vec()
        } else {
            b"{}".to_vec()
        });
    }
    arguments.is_object().then(|| arguments.raw.to_vec())
}

/// mimeTypeFromCodexOutputFormat.
fn image_mime(format: &[u8]) -> Vec<u8> {
    if format.is_empty() {
        return b"image/png".to_vec();
    }
    if format.contains(&b'/') {
        return format.to_vec();
    }
    match go_lower(format).as_slice() {
        b"jpg" | b"jpeg" => b"image/jpeg".to_vec(),
        b"webp" => b"image/webp".to_vec(),
        b"gif" => b"image/gif".to_vec(),
        _ => b"image/png".to_vec(),
    }
}

/// setCodexInteractionsUsage.
fn set_usage(out: &mut Vec<u8>, path: &str, usage: &Res<'_>, stream: bool) {
    if !usage.exists() {
        return;
    }
    let or = |a: i64, b: &str| if a == 0 { usage.get(b).int() } else { a };
    let input = or(usage.get("input_tokens").int(), "prompt_tokens");
    let output = or(usage.get("output_tokens").int(), "completion_tokens");
    let mut total = usage.get("total_tokens").int();
    if total == 0 {
        total = input.wrapping_add(output);
    }
    let reasoning = or(
        usage.get("output_tokens_details.reasoning_tokens").int(),
        "reasoning_tokens",
    );
    let cached = or(usage.get("input_tokens_details.cached_tokens").int(), "cached_tokens");
    let at = |k: &str| format!("{path}.{k}");
    if stream {
        gj::set_int(out, &at("total_tokens"), total);
        gj::set_int(out, &at("total_input_tokens"), input);
        gj::set_raw(
            out,
            &at("input_tokens_by_modality"),
            format!(r#"[{{"modality":"text","tokens":{input}}}]"#),
        );
        gj::set_int(out, &at("total_cached_tokens"), cached);
        gj::set_int(out, &at("total_output_tokens"), output);
        gj::set_int(out, &at("total_tool_use_tokens"), 0);
        gj::set_int(out, &at("total_thought_tokens"), reasoning);
        return;
    }
    gj::set_int(out, &at("input_tokens"), input);
    gj::set_int(out, &at("output_tokens"), output);
    gj::set_int(out, &at("total_tokens"), total);
    if reasoning > 0 {
        gj::set_int(out, &at("reasoning_tokens"), reasoning);
    }
    if cached > 0 {
        gj::set_int(out, &at("cached_tokens"), cached);
    }
}

fn delta_event(template: &[u8], index: i64, path: &str, value: &[u8]) -> Vec<u8> {
    let mut delta = template.to_vec();
    gj::set_int(&mut delta, "index", index);
    gj::set_str(&mut delta, path, value);
    sse_event("step.delta", &delta)
}

const TEXT_DELTA: &[u8] = br#"{"index":0,"delta":{"text":"","type":"text"},"event_type":"step.delta"}"#;
const THOUGHT_DELTA: &[u8] =
    br#"{"index":0,"delta":{"content":{"text":"","type":"text"},"type":"thought_summary"},"event_type":"step.delta"}"#;
const ARGUMENTS_DELTA: &[u8] =
    br#"{"index":0,"delta":{"arguments":"","type":"arguments_delta"},"event_type":"step.delta"}"#;

impl State {
    fn created(&mut self, response: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        if self.started {
            return;
        }
        let id = response.get("id").bytes();
        if !id.is_empty() {
            self.id = id.into_owned();
        }
        let model = response.get("model").bytes();
        if !model.is_empty() {
            self.model = model.into_owned();
        }
        let created_at = response.get("created_at");
        if created_at.exists() {
            self.created_at = created_at.int();
        }
        let mut created = br#"{"interaction":{"id":"","status":"in_progress","object":"interaction","model":""},"event_type":"interaction.created"}"#.to_vec();
        gj::set_str(&mut created, "interaction.id", &self.id);
        gj::set_str(&mut created, "interaction.model", &self.model);
        out.push(sse_event("interaction.created", &created));
        let mut status =
            br#"{"interaction_id":"","status":"in_progress","event_type":"interaction.status_update"}"#.to_vec();
        gj::set_str(&mut status, "interaction_id", &self.id);
        out.push(sse_event("interaction.status_update", &status));
        self.started = true;
    }

    fn completed(&mut self, response: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        if self.completed {
            return;
        }
        let now = now_unix();
        let created = if self.created_at > 0 { self.created_at } else { now };
        let mut completed = br#"{"interaction":{"id":"","status":"completed","usage":{},"created":"","updated":"","service_tier":"standard","object":"interaction","model":""},"event_type":"interaction.completed"}"#.to_vec();
        gj::set_str(&mut completed, "interaction.id", &self.id);
        gj::set_str(&mut completed, "interaction.created", format_rfc3339_utc(created));
        gj::set_str(&mut completed, "interaction.updated", format_rfc3339_utc(now));
        gj::set_str(&mut completed, "interaction.model", &self.model);
        let status = response.get("status").bytes();
        if !status.is_empty() {
            gj::set_str(&mut completed, "interaction.status", &status);
        }
        set_usage(&mut completed, "interaction.usage", &response.get("usage"), true);
        out.push(sse_event("interaction.completed", &completed));
        self.completed = true;
    }

    fn finish(&mut self, out: &mut Vec<Vec<u8>>) {
        if !self.done {
            out.push(sse_event("done", b"[DONE]"));
            self.done = true;
        }
    }

    fn stop_step(&mut self, out: &mut Vec<Vec<u8>>) {
        if !self.step_open {
            return;
        }
        let mut stop = br#"{"index":0,"event_type":"step.stop"}"#.to_vec();
        gj::set_int(&mut stop, "index", self.step_index);
        out.push(sse_event("step.stop", &stop));
        self.step_open = false;
        self.step_type = "";
    }

    fn ensure_step(&mut self, kind: &'static str, item: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        if self.step_open && self.step_type == kind {
            return;
        }
        self.stop_step(out);
        self.step_index = self.next_step;
        self.next_step += 1;
        self.step_open = true;
        self.step_type = kind;
        let mut start = br#"{"index":0,"step":{"type":""},"event_type":"step.start"}"#.to_vec();
        gj::set_int(&mut start, "index", self.step_index);
        gj::set_str(&mut start, "step.type", kind);
        if kind == "function_call" {
            let mut name = item.get("name").bytes().into_owned();
            if name.is_empty() {
                name = self.call_name.clone();
            }
            let mut id = item_call_id(item);
            if id.is_empty() {
                id = self.call_id.clone();
            }
            if id.is_empty() {
                id = format!("step_{}", now_nanos()).into_bytes();
            }
            gj::set_str(&mut start, "step.id", &id);
            gj::set_str(&mut start, "step.call_id", &id);
            gj::set_str(&mut start, "step.name", &name);
            gj::set_raw(&mut start, "step.arguments", b"{}");
        }
        out.push(sse_event("step.start", &start));
    }

    fn item_done(&mut self, item: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        match item.get("type").bytes().as_ref() {
            b"message" => {
                if !self.has_output_text {
                    item.get("content").each(|_, content| {
                        let text = codex_content_text(&content);
                        if !text.is_empty() {
                            self.ensure_step("model_output", item, out);
                            out.push(delta_event(TEXT_DELTA, self.step_index, "delta.text", &text));
                        }
                        true
                    });
                }
            }
            b"reasoning" => {
                let text = reasoning_text(item);
                if !text.is_empty() {
                    self.ensure_step("thought", item, out);
                    out.push(delta_event(THOUGHT_DELTA, self.step_index, "delta.content.text", &text));
                }
            }
            b"function_call" | b"tool_call" => {
                self.ensure_step("function_call", item, out);
                out.push(delta_event(
                    ARGUMENTS_DELTA,
                    self.step_index,
                    "delta.arguments",
                    &item.get("arguments").bytes(),
                ));
            }
            b"image_generation_call" => {
                let result = item.get("result").bytes().into_owned();
                if !result.is_empty() {
                    self.ensure_step("model_output", item, out);
                    let mut delta = br#"{"index":0,"delta":{"content":{"type":"image","mime_type":"","data":""},"type":"content"},"event_type":"step.delta"}"#.to_vec();
                    gj::set_int(&mut delta, "index", self.step_index);
                    gj::set_str(
                        &mut delta,
                        "delta.content.mime_type",
                        image_mime(&item.get("output_format").bytes()),
                    );
                    gj::set_str(&mut delta, "delta.content.data", &result);
                    out.push(sse_event("step.delta", &delta));
                }
            }
            _ => return,
        }
        self.stop_step(out);
    }
}

impl GoStream for State {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let mut payload = trim_space(line);
        if let Some(rest) = payload.strip_prefix(b"data:") {
            payload = trim_space(rest);
        }
        let mut out = vec![];
        if payload == b"[DONE]" {
            self.stop_step(&mut out);
            if !self.completed {
                self.completed(&Res::default(), &mut out);
            }
            self.finish(&mut out);
            return Ok(out);
        }
        if payload.is_empty() {
            return Ok(out);
        }
        let root = gj::parse(payload);
        let response = root.get("response");
        match root.get("type").bytes().as_ref() {
            b"response.created" => self.created(&response, &mut out),
            b"response.output_item.added" => {
                self.created(&response, &mut out);
                let item = root.get("item");
                match item.get("type").bytes().as_ref() {
                    b"message" => self.ensure_step("model_output", &item, &mut out),
                    b"reasoning" => self.ensure_step("thought", &item, &mut out),
                    b"function_call" | b"tool_call" => {
                        self.call_name = item.get("name").bytes().into_owned();
                        self.call_id = item_call_id(&item);
                        self.ensure_step("function_call", &item, &mut out);
                    }
                    _ => {}
                }
            }
            b"response.output_text.delta" => {
                self.created(&response, &mut out);
                self.ensure_step("model_output", &Res::default(), &mut out);
                out.push(delta_event(
                    TEXT_DELTA,
                    self.step_index,
                    "delta.text",
                    &root.get("delta").bytes(),
                ));
                self.has_output_text = true;
            }
            b"response.reasoning_summary_text.delta" | b"response.reasoning_text.delta" => {
                self.created(&response, &mut out);
                self.ensure_step("thought", &Res::default(), &mut out);
                out.push(delta_event(
                    THOUGHT_DELTA,
                    self.step_index,
                    "delta.content.text",
                    &root.get("delta").bytes(),
                ));
            }
            b"response.function_call_arguments.delta" => {
                self.created(&response, &mut out);
                self.ensure_step("function_call", &root.get("item"), &mut out);
                out.push(delta_event(
                    ARGUMENTS_DELTA,
                    self.step_index,
                    "delta.arguments",
                    &root.get("delta").bytes(),
                ));
            }
            b"response.output_item.done" => {
                self.created(&Res::default(), &mut out);
                self.item_done(&root.get("item"), &mut out);
            }
            b"response.completed" | b"response.incomplete" => {
                self.created(&response, &mut out);
                self.stop_step(&mut out);
                self.completed(&response, &mut out);
                self.finish(&mut out);
            }
            _ => {}
        }
        Ok(out)
    }
}

/// ConvertCodexResponseToInteractionsNonStream.
pub fn non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let root = gj::parse(body);
    let mut response = root.get("response");
    if !response.exists() {
        response = root.clone();
    }
    let mut out = br#"{"id":"","object":"interaction","status":"completed","model":"","steps":[]}"#.to_vec();
    let status = response.get("status").bytes();
    if !status.is_empty() {
        gj::set_str(&mut out, "status", &status);
    }
    let mut id = response.get("id").bytes().into_owned();
    if id.is_empty() {
        id = interaction_id();
    }
    gj::set_str(&mut out, "id", &id);
    let model = response.get("model").bytes();
    if model.is_empty() {
        gj::set_str(&mut out, "model", ctx.model);
    } else {
        gj::set_str(&mut out, "model", &model);
    }
    let mut steps: Vec<Vec<u8>> = vec![];
    response.get("output").each(|_, item| {
        match item.get("type").bytes().as_ref() {
            b"message" => {
                let mut contents = vec![];
                item.get("content").each(|_, content| {
                    let text = codex_content_text(&content);
                    if !text.is_empty() {
                        let mut part = br#"{"type":"text","text":""}"#.to_vec();
                        gj::set_str(&mut part, "text", &text);
                        contents.push(part);
                    }
                    true
                });
                if !contents.is_empty() {
                    let mut step = br#"{"type":"model_output","content":[]}"#.to_vec();
                    gj::set_items(&mut step, "content", &contents);
                    steps.push(step);
                }
            }
            b"reasoning" => {
                let text = reasoning_text(&item);
                if !text.is_empty() {
                    let mut step = br#"{"type":"thought","content":[{"type":"text","text":""}]}"#.to_vec();
                    gj::set_str(&mut step, "content.0.text", &text);
                    steps.push(step);
                }
            }
            b"function_call" | b"tool_call" => {
                let mut step = br#"{"type":"function_call","name":"","arguments":{}}"#.to_vec();
                gj::set_str(&mut step, "name", item.get("name").bytes());
                let id = item_call_id(&item);
                if !id.is_empty() {
                    gj::set_str(&mut step, "call_id", &id);
                }
                if let Some(args) = arguments_json(&item.get("arguments")).filter(|a| !a.is_empty()) {
                    gj::set_raw(&mut step, "arguments", &args);
                }
                steps.push(step);
            }
            b"image_generation_call" => {
                let result = item.get("result").bytes();
                if !result.is_empty() {
                    let mut step =
                        br#"{"type":"model_output","content":[{"type":"image","mime_type":"","data":""}]}"#.to_vec();
                    gj::set_str(
                        &mut step,
                        "content.0.mime_type",
                        image_mime(&item.get("output_format").bytes()),
                    );
                    gj::set_str(&mut step, "content.0.data", &result);
                    steps.push(step);
                }
            }
            _ => {}
        }
        true
    });
    gj::set_items(&mut out, "steps", &steps);
    set_usage(&mut out, "usage", &response.get("usage"), false);
    Ok(out)
}
