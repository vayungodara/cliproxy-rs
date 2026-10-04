//! OpenAI Responses <-> Gemini Interactions requests
//! (internal/translator/openai/interactions/responses/interactions_openai_responses_request.go).
//! The response sides live in [`crate::responses_interactions_response`].

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use cpa_common::json::{self as gj, Kind, Res};

use crate::claude_responses::qualify_namespace_name;
use crate::common::{go_lower, trim_space};
use crate::gemini_interactions::first_existing;
use crate::openai_interactions::{
    antigravity_name_to_client, antigravity_name_to_upstream, first_nonblank, is_antigravity, json_string_value,
    text_step,
};
use crate::responses_interactions_response as response;
use crate::responses_tools::{self as tools, Identity};
use crate::{Registered, apply_patch};

/// OpenAI Responses client, Interactions upstream.
pub static RESPONSES_TO_INTERACTIONS: Registered = registered!(
    OpenAIResponse -> Interactions,
    request: |ctx, body| Ok(responses_to_interactions(ctx.model, body, ctx.stream)),
    non_stream: response::interactions_to_responses_non_stream,
    go_stream: response::interactions_to_responses_stream,
    token_count: None,
);

/// Interactions client, OpenAI Responses upstream.
pub static INTERACTIONS_TO_RESPONSES: Registered = registered!(
    Interactions -> OpenAIResponse,
    request: |ctx, body| Ok(interactions_to_responses(ctx.model, body, ctx.stream)),
    non_stream: response::responses_to_interactions_non_stream,
    go_stream: response::responses_to_interactions_stream,
    token_count: None,
);

fn string(res: &Res<'_>) -> Vec<u8> {
    res.bytes().into_owned()
}

/// requestModel: the requested model unless blank, else the body's.
fn request_model(model: &str, root: &Res<'_>) -> Vec<u8> {
    if trim_space(model.as_bytes()).is_empty() {
        string(&root.get("model"))
    } else {
        model.as_bytes().to_vec()
    }
}

/// isDevinModel.
fn is_devin(model: &[u8]) -> bool {
    go_lower(trim_space(model)).windows(5).any(|w| w == b"devin")
}

/// strings.EqualFold, on Go's Unicode folding.
fn eq_fold(a: &[u8], b: &str) -> bool {
    use cpa_common::gostr::GoStr;
    String::from_utf8_lossy(a).go_eq_fold(b)
}

/// common.IsDevinCodexAppAutomationUpdate.
pub(crate) fn is_devin_automation_update(namespace: &[u8], tool: &[u8]) -> bool {
    let (namespace, tool) = (trim_space(namespace), trim_space(tool));
    (eq_fold(namespace, "mcp__codex_app") && eq_fold(tool, "automation_update"))
        || eq_fold(tool, "mcp__codex_app__automation_update")
}

const EXEC_TARGET: &[u8] = b"returning output or a session ID for ongoing interaction";
const EXEC_OBFUSCATED: &[u8] = b"returning output or an session ID for ongoing interaction";
const STDIN_TARGET: &[u8] = b"Writes characters to an existing unified exec session and returns recent output.";
const STDIN_OBFUSCATED: &[u8] = b"Writes characters to a existing unified exec session and returns recent output.";

static EXEC_REGEX: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
    regex::bytes::Regex::new(r"(?i)returning output or a session ID for ongoing interaction").expect("valid")
});
static STDIN_REGEX: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
    regex::bytes::Regex::new(
        r"(?i)Writes characters to an existing unified exec session and returns recent output(\.?)",
    )
    .expect("valid")
});

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn replace_all(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut rest = haystack;
    while let Some(i) = rest.windows(from.len()).position(|w| w == from) {
        out.extend_from_slice(&rest[..i]);
        out.extend_from_slice(to);
        rest = &rest[i + from.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// ObfuscateExecCommandDescription / ObfuscateWriteStdinDescription.
fn obfuscate(
    desc: &[u8],
    target: &[u8],
    obfuscated: &[u8],
    regex: &regex::bytes::Regex,
    replacement: &[u8],
) -> Vec<u8> {
    if contains(desc, obfuscated) {
        desc.to_vec()
    } else if contains(desc, target) {
        replace_all(desc, target, obfuscated)
    } else {
        regex.replace_all(desc, replacement).into_owned()
    }
}

/// common.SanitizeDevinToolDescription.
pub(crate) fn sanitize_devin_description(tool: &[u8], desc: Vec<u8>) -> Vec<u8> {
    if desc.is_empty() {
        return desc;
    }
    let tool = go_lower(trim_space(tool));
    let mut desc = desc;
    if tool == b"exec_command" || tool.ends_with(b"__exec_command") {
        desc = obfuscate_exec_command_description(&desc);
    }
    if tool == b"write_stdin" || tool.ends_with(b"__write_stdin") {
        desc = obfuscate_write_stdin_description(&desc);
    }
    desc
}

/// common.ObfuscateExecCommandDescription.
pub(crate) fn obfuscate_exec_command_description(desc: &[u8]) -> Vec<u8> {
    obfuscate(desc, EXEC_TARGET, EXEC_OBFUSCATED, &EXEC_REGEX, EXEC_OBFUSCATED)
}

/// common.ObfuscateWriteStdinDescription.
pub(crate) fn obfuscate_write_stdin_description(desc: &[u8]) -> Vec<u8> {
    obfuscate(
        desc,
        STDIN_TARGET,
        STDIN_OBFUSCATED,
        &STDIN_REGEX,
        b"Writes characters to a existing unified exec session and returns recent output$1",
    )
}

/// setJSONValue: a string holding valid JSON is set raw (untrimmed).
pub(crate) fn set_json_value(out: &mut Vec<u8>, path: &str, value: &Res<'_>, fallback: &[u8]) {
    if !value.exists() {
        gj::set_raw(out, path, fallback);
    } else if value.kind == Kind::String {
        let text = value.bytes();
        if gj::valid(&text) {
            gj::set_raw(out, path, &text);
        } else {
            gj::set_str(out, path, &text);
        }
    } else {
        gj::set_raw(out, path, &value.raw);
    }
}

/// The tool name of a Responses item, qualified by its namespace.
fn qualified_name(item: &Res<'_>) -> Vec<u8> {
    let name = string(&item.get("name"));
    let namespace = string(&item.get("namespace"));
    if !namespace.is_empty() && !name.is_empty() {
        qualify_namespace_name(&namespace, &name)
    } else {
        name
    }
}

fn call_id(item: &Res<'_>) -> Vec<u8> {
    first_nonblank(&[&item.get("call_id").bytes(), &item.get("id").bytes()])
}

// ---------------------------------------------------------------------------------------
// Responses request -> Interactions request

/// ConvertOpenAIResponsesRequestToInteractions.
pub(crate) fn responses_to_interactions(model: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    let root = gj::parse(raw);
    let mut out = br#"{"model":"","input":[]}"#.to_vec();
    let model = request_model(model, &root);
    gj::set_str(&mut out, "model", &model);
    let stream_value = root.get("stream");
    if stream_value.exists() {
        gj::set_bool(&mut out, "stream", stream_value.bool());
    } else if stream {
        gj::set_bool(&mut out, "stream", true);
    }
    let instructions = root.get("instructions");
    if instructions.exists() {
        gj::set_str(&mut out, "system_instruction", instructions_text(&instructions));
    }
    let previous = first_nonblank(&[
        &root.get("previous_response_id").bytes(),
        &root.get("previous_interaction_id").bytes(),
    ]);
    if !previous.is_empty() {
        gj::set_str(&mut out, "previous_interaction_id", &previous);
    }
    let environment = first_nonblank(&[&root.get("environment_id").bytes(), &root.get("environment.id").bytes()]);
    if !environment.is_empty() {
        gj::set_str(&mut out, "environment_id", &environment);
    }
    let agent_config = root.get("agent_config");
    if agent_config.exists() {
        gj::set_raw(&mut out, "agent_config", &agent_config.raw);
    }
    let antigravity = is_antigravity(&model);
    let devin = is_devin(&model) || !antigravity;
    let input = root.get("input");
    if input.exists() {
        set_input(&mut out, &input, antigravity);
    }
    append_tools(&mut out, &root, antigravity, devin);
    copy_tool_choice(&mut out, &root.get("tool_choice"), antigravity, devin);
    let effort = root.get("reasoning.effort");
    if effort.kind == Kind::String {
        gj::set_str(
            &mut out,
            "generation_config.thinking_level",
            go_lower(trim_space(&effort.bytes())),
        );
    }
    let summary = root.get("reasoning.summary");
    if summary.kind == Kind::String {
        gj::set_str(&mut out, "generation_config.thinking_summaries", summary.bytes());
    }
    let format = first_existing(&root, &["response_format", "text.format"]);
    if format.exists() {
        gj::set_raw(&mut out, "response_format", &format.raw);
    }
    let max = first_existing(&root, &["max_output_tokens", "max_tokens", "max_completion_tokens"]);
    if antigravity {
        if max.exists() && !root.get("agent_config.max_total_tokens").exists() {
            gj::set_int(&mut out, "agent_config.max_total_tokens", max.int());
        }
        for knob in [
            "temperature",
            "top_p",
            "top_k",
            "stop_sequences",
            "max_output_tokens",
            "presence_penalty",
            "frequency_penalty",
            "candidate_count",
        ] {
            gj::delete(&mut out, &format!("generation_config.{knob}"));
        }
    } else {
        if max.exists() {
            gj::set_int(&mut out, "generation_config.max_output_tokens", max.int());
        }
        for key in ["temperature", "top_p", "presence_penalty", "frequency_penalty"] {
            let value = root.get(key);
            if value.exists() {
                gj::set_f64(&mut out, &format!("generation_config.{key}"), value.float());
            }
        }
        let stop = root.get("stop");
        if stop.exists() {
            gj::set_raw(&mut out, "generation_config.stop_sequences", &stop.raw);
        }
    }
    out
}

/// The `tool_choice` of a Responses request as the Interactions generation config's.
fn copy_tool_choice(out: &mut Vec<u8>, choice: &Res<'_>, antigravity: bool, devin: bool) {
    if !choice.exists() {
        return;
    }
    if !choice.is_object() {
        gj::set_raw(out, "generation_config.tool_choice", &choice.raw);
        return;
    }
    let mut name = first_nonblank(&[
        &choice.get("function.name").bytes(),
        &choice.get("name").bytes(),
        &choice.get("custom.name").bytes(),
    ]);
    let namespace = first_nonblank(&[
        &choice.get("namespace").bytes(),
        &choice.get("function.namespace").bytes(),
        &choice.get("custom.namespace").bytes(),
    ]);
    if !namespace.is_empty() && !name.is_empty() {
        name = qualify_namespace_name(&namespace, &name);
    }
    if devin && (is_devin_automation_update(&namespace, &name) || is_devin_automation_update(b"", &name)) {
        return;
    }
    if antigravity && !name.is_empty() {
        name = antigravity_name_to_upstream(&name);
    }
    let mut raw = choice.raw.to_vec();
    if !name.is_empty() {
        for path in ["function.name", "name", "custom.name"] {
            if choice.get(path).exists() {
                gj::set_str(&mut raw, path, &name);
                break;
            }
        }
    }
    gj::set_raw(out, "generation_config.tool_choice", raw);
}

/// responsesInstructionsText.
fn instructions_text(instructions: &Res<'_>) -> Vec<u8> {
    if instructions.kind == Kind::String {
        return string(instructions);
    }
    let text = instructions.get("text");
    if text.exists() {
        return string(&text);
    }
    let parts = instructions.get("content");
    if parts.is_array() {
        let mut out = vec![];
        parts.each(|_, part| {
            out.extend_from_slice(&part.get("text").bytes());
            true
        });
        return out;
    }
    string(instructions)
}

/// setResponsesInputOnInteractions.
fn set_input(out: &mut Vec<u8>, input: &Res<'_>, antigravity: bool) {
    let mut names: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut items = vec![];
    if input.kind == Kind::String {
        items.push(text_step("user_input", &input.bytes()));
    } else if input.is_array() {
        input.each(|_, item| {
            items.extend(input_item(&item, &mut names, antigravity));
            true
        });
    } else if input.is_object() {
        items.extend(input_item(input, &mut names, antigravity));
    }
    if !items.is_empty() {
        gj::set_raw(out, "input", gj::join(&items));
    }
}

/// responsesInputItemToInteractions.
fn input_item(item: &Res<'_>, names: &mut HashMap<Vec<u8>, Vec<u8>>, antigravity: bool) -> Option<Vec<u8>> {
    let kind = string(&item.get("type"));
    match kind.as_slice() {
        b"message" => {
            let role = item.get("role").bytes();
            let step_type = if matches!(role.as_ref(), b"assistant" | b"model") {
                "model_output"
            } else {
                "user_input"
            };
            let mut step = br#"{"type":"","content":[]}"#.to_vec();
            gj::set_str(&mut step, "type", step_type);
            append_content(&mut step, &item.get("content"));
            Some(step)
        }
        b"function_call" | b"custom_tool_call" => {
            let id = call_id(item);
            let name = qualified_name(item);
            if !id.is_empty() && !name.is_empty() {
                names.insert(id, name);
            }
            Some(function_call_step(item, antigravity, kind == b"custom_tool_call"))
        }
        b"function_call_output" | b"custom_tool_call_output" => Some(function_output_step(item, names, antigravity)),
        b"input_text" | b"output_text" | b"text" => {
            let step_type = if kind == b"output_text" {
                "model_output"
            } else {
                "user_input"
            };
            Some(text_step(step_type, &item.get("text").bytes()))
        }
        b"input_image" | b"output_image" => {
            let step_type = if kind == b"output_image" {
                "model_output"
            } else {
                "user_input"
            };
            let mut step = br#"{"type":"","content":[]}"#.to_vec();
            gj::set_str(&mut step, "type", step_type);
            if let Some(part) = content_part(item) {
                gj::set_items(&mut step, "content", &[part]);
            }
            Some(step)
        }
        _ => {
            let content = item.get("content");
            if !content.exists() {
                return None;
            }
            let mut step = br#"{"type":"user_input","content":[]}"#.to_vec();
            append_content(&mut step, &content);
            Some(step)
        }
    }
}

/// appendResponsesContentToInteractions.
fn append_content(step: &mut Vec<u8>, content: &Res<'_>) {
    let mut items = vec![];
    if content.kind == Kind::String {
        let mut part = br#"{"type":"text","text":""}"#.to_vec();
        gj::set_str(&mut part, "text", content.bytes());
        items.push(part);
    } else if content.is_array() {
        content.each(|_, part| {
            items.extend(content_part(&part));
            true
        });
    } else if content.is_object() {
        items.extend(content_part(content));
    }
    gj::set_items(step, "content", &items);
}

/// responsesContentPartToInteractions.
pub(crate) fn content_part(part: &Res<'_>) -> Option<Vec<u8>> {
    let text_part = |text: &[u8]| {
        let mut out = br#"{"type":"text","text":""}"#.to_vec();
        gj::set_str(&mut out, "text", text);
        out
    };
    match part.get("type").bytes().as_ref() {
        b"input_text" | b"output_text" | b"text" => return Some(text_part(&part.get("text").bytes())),
        b"input_image" | b"output_image" => return Some(image_part(part)),
        _ => {}
    }
    let text = part.get("text");
    text.exists().then(|| text_part(&text.bytes()))
}

/// parseDataURL (Responses package): any `data:` URL with a comma; the MIME type
/// defaults to application/octet-stream.
fn parse_data_url(value: &[u8]) -> Option<(&[u8], &[u8])> {
    let rest = value.strip_prefix(b"data:")?;
    let comma = rest.iter().position(|&c| c == b',')?;
    let header = &rest[..comma];
    let mime = &header[..header.iter().position(|&c| c == b';').unwrap_or(header.len())];
    let mime: &[u8] = if mime.is_empty() {
        b"application/octet-stream"
    } else {
        mime
    };
    Some((mime, &rest[comma + 1..]))
}

/// responsesImagePartToInteractions.
fn image_part(part: &Res<'_>) -> Vec<u8> {
    let mut out = br#"{"type":"image"}"#.to_vec();
    let url = first_nonblank(&[&part.get("image_url").bytes(), &part.get("url").bytes()]);
    if let Some((mime, data)) = parse_data_url(&url) {
        gj::set_str(&mut out, "mime_type", mime);
        gj::set_str(&mut out, "data", data);
        return out;
    }
    let data = string(&part.get("data"));
    if !data.is_empty() {
        gj::set_str(&mut out, "data", &data);
        let mime = string(&part.get("mime_type"));
        if !mime.is_empty() {
            gj::set_str(&mut out, "mime_type", &mime);
        }
        return out;
    }
    if !url.is_empty() {
        gj::set_str(&mut out, "image_url", &url);
    }
    out
}

/// responsesFunctionCallToInteractions / responsesCustomToolCallToInteractions.
pub(crate) fn function_call_step(item: &Res<'_>, antigravity: bool, custom: bool) -> Vec<u8> {
    let mut out = br#"{"type":"function_call","name":"","arguments":{}}"#.to_vec();
    let mut name = qualified_name(item);
    if antigravity {
        name = antigravity_name_to_upstream(&name);
    }
    gj::set_str(&mut out, "name", &name);
    let id = call_id(item);
    if !id.is_empty() {
        gj::set_str(&mut out, "call_id", &id);
    }
    let input = item.get("input");
    if custom && input.exists() {
        gj::set_str(&mut out, "arguments.input", input.bytes());
    } else {
        set_json_value(&mut out, "arguments", &item.get("arguments"), b"{}");
    }
    out
}

/// responsesFunctionOutputToInteractions.
fn function_output_step(item: &Res<'_>, names: &HashMap<Vec<u8>, Vec<u8>>, antigravity: bool) -> Vec<u8> {
    let mut out = br#"{"type":"function_result","name":"","result":{}}"#.to_vec();
    let id = call_id(item);
    let mut name = qualified_name(item);
    if name.is_empty() && !id.is_empty() {
        name = names.get(&id).cloned().unwrap_or_default();
    }
    if !name.is_empty() {
        if antigravity {
            name = antigravity_name_to_upstream(&name);
        }
        gj::set_str(&mut out, "name", &name);
    }
    if !id.is_empty() {
        gj::set_str(&mut out, "call_id", &id);
    }
    set_json_value(&mut out, "result", &first_existing(item, &["output", "result"]), b"{}");
    out
}

const CUSTOM_PARAMETERS: &[u8] = br#"{"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}"#;

/// appendResponsesToolsToInteractions: the winning declaration per tool name.
fn append_tools(out: &mut Vec<u8>, root: &Res<'_>, antigravity: bool, devin: bool) {
    if !root.exists() {
        return;
    }
    let wrapped;
    let target = if root.is_array() {
        wrapped = [&b"{\"tools\":"[..], &root.raw, b"}"].concat();
        gj::parse(&wrapped)
    } else {
        root.clone()
    };
    let descriptors = tools::descriptors(&target);
    if descriptors.is_empty() {
        return;
    }
    let winners = tools::winners(&descriptors);
    let mut seen: HashSet<&[u8]> = HashSet::new();
    let mut items = vec![];
    for descriptor in &descriptors {
        if winners.get(&descriptor.name) != Some(&descriptor.order) || !seen.insert(&descriptor.name) {
            continue;
        }
        if devin
            && (is_devin_automation_update(&descriptor.namespace, &descriptor.local_name)
                || is_devin_automation_update(b"", &descriptor.name))
        {
            continue;
        }
        let mut name = descriptor.name.clone();
        if antigravity {
            name = antigravity_name_to_upstream(&name);
        }
        let mut item = br#"{"type":"function","name":""}"#.to_vec();
        gj::set_str(&mut item, "name", &name);
        let patch = apply_patch::is_custom_tool(&descriptor.tool);
        let mut desc = if patch {
            apply_patch::description(&descriptor.tool)
        } else {
            tools::tool_description(&descriptor.tool)
        };
        if !desc.is_empty() {
            if devin {
                desc = sanitize_devin_description(&descriptor.name, desc);
                if !descriptor.local_name.is_empty() && descriptor.local_name != descriptor.name {
                    desc = sanitize_devin_description(&descriptor.local_name, desc);
                }
            }
            gj::set_str(&mut item, "description", &desc);
        }
        if patch {
            gj::set_raw(&mut item, "parameters", apply_patch::PARAMETERS);
        } else if descriptor.custom {
            gj::set_raw(&mut item, "parameters", CUSTOM_PARAMETERS);
        } else if let Some(params) = tools::tool_parameters(&descriptor.tool) {
            gj::set_raw(&mut item, "parameters", &params.raw);
        }
        items.push(item);
    }
    if !items.is_empty() {
        gj::set_raw(out, "tools", gj::join(&items));
    }
}

// ---------------------------------------------------------------------------------------
// Interactions request -> Responses request

/// ConvertInteractionsRequestToOpenAIResponses.
pub(crate) fn interactions_to_responses(model: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    let root = gj::parse(raw);
    let mut out = br#"{"model":"","input":[]}"#.to_vec();
    let model = request_model(model, &root);
    gj::set_str(&mut out, "model", &model);
    if stream || root.get("stream").bool() {
        gj::set_bool(&mut out, "stream", true);
    }
    let instructions = system_instruction_text(&root);
    if !instructions.is_empty() {
        gj::set_str(&mut out, "instructions", &instructions);
    }
    let previous = first_nonblank(&[
        &root.get("previous_interaction_id").bytes(),
        &root.get("previous_response_id").bytes(),
    ]);
    if !previous.is_empty() {
        gj::set_str(&mut out, "previous_response_id", &previous);
    }
    let environment = first_nonblank(&[&root.get("environment_id").bytes(), &root.get("environment.id").bytes()]);
    if !environment.is_empty() {
        gj::set_str(&mut out, "environment_id", &environment);
    }
    let agent_config = root.get("agent_config");
    if agent_config.exists() {
        gj::set_raw(&mut out, "agent_config", &agent_config.raw);
    }
    let antigravity = is_antigravity(&model);
    let input = root.get("input");
    if input.exists() {
        let mut items = vec![];
        if input.kind == Kind::String {
            items.push(text_message(&input.bytes()));
        } else if input.is_array() {
            input.each(|_, item| {
                items.extend(interactions_item(&item, antigravity));
                true
            });
        } else if input.is_object() {
            items.extend(interactions_item(&input, antigravity));
        }
        if !items.is_empty() {
            gj::set_raw(&mut out, "input", gj::join(&items));
        }
    }
    let tools = root.get("tools");
    if tools.is_array() {
        let mut items = vec![];
        tools.each(|_, tool| {
            items.extend(tool_to_responses(&tool, antigravity));
            let decls = tool.get("function_declarations");
            if decls.is_array() {
                decls.each(|_, decl| {
                    items.extend(tool_to_responses(&decl, antigravity));
                    true
                });
            }
            true
        });
        if !items.is_empty() {
            gj::set_raw(&mut out, "tools", gj::join(&items));
        }
    }
    let choice = first_existing(&root, &["generation_config.tool_choice", "tool_choice"]);
    if choice.exists() {
        gj::set_raw(&mut out, "tool_choice", &choice.raw);
    }
    let effort = [
        "generation_config.thinking_level",
        "generation_config.thinkingConfig.thinkingLevel",
        "generation_config.thinkingConfig.thinking_level",
        "generation_config.thinking_config.thinking_level",
    ]
    .iter()
    .map(|p| root.get(*p))
    .find(|v| v.kind == Kind::String)
    .map(|v| go_lower(trim_space(&v.bytes())))
    .unwrap_or_default();
    if !effort.is_empty() {
        gj::set_str(&mut out, "reasoning.effort", &effort);
    }
    let summary = root.get("generation_config.thinking_summaries");
    if summary.kind == Kind::String {
        gj::set_str(&mut out, "reasoning.summary", summary.bytes());
    }
    let modalities = root.get("response_modalities");
    if modalities.exists() {
        gj::set_raw(&mut out, "modalities", &modalities.raw);
    }
    let tier = root.get("service_tier");
    if tier.kind == Kind::String {
        gj::set_str(&mut out, "service_tier", tier.bytes());
    }
    let format = root.get("response_format");
    if format.exists() {
        gj::set_raw(&mut out, "text.format", &format.raw);
    }
    out
}

/// interactionsSystemInstructionText.
fn system_instruction_text(root: &Res<'_>) -> Vec<u8> {
    let sys = root.get("system_instruction");
    if !sys.exists() {
        return vec![];
    }
    if sys.kind == Kind::String {
        return string(&sys);
    }
    let text = sys.get("text");
    if text.exists() {
        return string(&text);
    }
    let mut out = vec![];
    let parts = sys.get("parts");
    if parts.is_array() {
        parts.each(|_, part| {
            out.extend_from_slice(&part.get("text").bytes());
            true
        });
    }
    out
}

/// interactionsTextMessage.
fn text_message(text: &[u8]) -> Vec<u8> {
    let mut item = br#"{"type":"message","role":"user","content":[{"type":"input_text","text":""}]}"#.to_vec();
    gj::set_str(&mut item, "content.0.text", text);
    item
}

/// interactionsInputItemToResponses.
fn interactions_item(item: &Res<'_>, antigravity: bool) -> Option<Vec<u8>> {
    match item.get("type").bytes().as_ref() {
        b"user_input" => Some(message(item, "user")),
        b"model_output" => Some(message(item, "assistant")),
        b"thought" => {
            let mut out = br#"{"type":"reasoning","summary":[]}"#.to_vec();
            gj::set_items(
                &mut out,
                "summary",
                &summary_parts(&content_texts(&item.get("content"))),
            );
            Some(out)
        }
        b"function_call" => Some(function_call_item(item, antigravity, &HashMap::new())),
        b"function_result" => {
            let mut out = br#"{"type":"function_call_output","call_id":"","output":""}"#.to_vec();
            let id = call_id(item);
            if !id.is_empty() {
                gj::set_str(&mut out, "call_id", &id);
            }
            let mut name = string(&item.get("name"));
            if !name.is_empty() {
                if antigravity {
                    name = antigravity_name_to_client(&name);
                }
                gj::set_str(&mut out, "name", &name);
            }
            let result = first_existing(item, &["result", "output"]);
            gj::set_str(&mut out, "output", json_string_value(&result, b""));
            Some(out)
        }
        _ if item.kind == Kind::String => Some(text_message(&item.bytes())),
        _ => None,
    }
}

/// Reasoning summary parts for the given texts.
pub(crate) fn summary_parts(texts: &[Vec<u8>]) -> Vec<Vec<u8>> {
    texts
        .iter()
        .map(|text| {
            let mut part = br#"{"type":"summary_text","text":""}"#.to_vec();
            gj::set_str(&mut part, "text", text);
            part
        })
        .collect()
}

/// interactionsMessageToResponses.
fn message(item: &Res<'_>, role: &str) -> Vec<u8> {
    let mut parts = vec![];
    let content = item.get("content");
    if content.kind == Kind::String {
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
        gj::set_str(&mut part, "text", content.bytes());
        parts.push(part);
    } else {
        content.each(|_, part| {
            parts.extend(content_part_to_responses(&part, role));
            true
        });
    }
    let mut out = br#"{"type":"message","role":"","content":[]}"#.to_vec();
    gj::set_str(&mut out, "role", role);
    gj::set_items(&mut out, "content", &parts);
    out
}

/// interactionsContentTexts.
pub(crate) fn content_texts(content: &Res<'_>) -> Vec<Vec<u8>> {
    if content.kind == Kind::String {
        return vec![string(content)];
    }
    let mut texts = vec![];
    if content.is_array() {
        content.each(|_, part| {
            let text = first_nonblank(&[&part.get("text").bytes(), &part.get("content.text").bytes()]);
            if !text.is_empty() {
                texts.push(text);
            }
            true
        });
    }
    texts
}

/// interactionsMediaDataURL (Responses package).
fn media_data_url(part: &Res<'_>) -> Vec<u8> {
    let url = first_nonblank(&[
        &part.get("image_url").bytes(),
        &part.get("file_data").bytes(),
        &part.get("url").bytes(),
    ]);
    if !url.is_empty() {
        return url;
    }
    let data = string(&part.get("data"));
    if data.is_empty() {
        return vec![];
    }
    let mut mime = string(&part.get("mime_type"));
    if mime.is_empty() {
        mime = b"application/octet-stream".to_vec();
    }
    [&b"data:"[..], &mime, b";base64,", &data].concat()
}

/// interactionsContentPartToResponses.
pub(crate) fn content_part_to_responses(part: &Res<'_>, role: &str) -> Option<Vec<u8>> {
    let mut kind = string(&part.get("type"));
    if kind.is_empty() && part.get("text").exists() {
        kind = b"text".to_vec();
    }
    let assistant = role == "assistant";
    let mut out;
    match kind.as_slice() {
        b"text" => {
            out = br#"{"type":"","text":""}"#.to_vec();
            gj::set_str(&mut out, "type", if assistant { "output_text" } else { "input_text" });
            gj::set_str(&mut out, "text", part.get("text").bytes());
        }
        b"image" => {
            out = br#"{"type":""}"#.to_vec();
            gj::set_str(&mut out, "type", if assistant { "output_image" } else { "input_image" });
            let url = media_data_url(part);
            if !url.is_empty() {
                gj::set_str(&mut out, "image_url", &url);
            }
        }
        b"audio" => {
            out = br#"{"type":"output_text","text":""}"#.to_vec();
            let mime = string(&part.get("mime_type"));
            let format = if mime.is_empty() {
                b"unknown".to_vec()
            } else {
                match mime.iter().position(|&c| c == b'/') {
                    Some(slash) if slash + 1 < mime.len() => mime[slash + 1..].to_vec(),
                    _ => mime,
                }
            };
            let text = [&b"Audio content: inline data (Format: "[..], &format, b")"].concat();
            gj::set_str(&mut out, "text", &text);
        }
        b"video" | b"document" => {
            out = br#"{"type":""}"#.to_vec();
            gj::set_str(&mut out, "type", if assistant { "output_file" } else { "input_file" });
            let url = media_data_url(part);
            if !url.is_empty() {
                gj::set_str(&mut out, "file_data", &url);
            }
            let filename = string(&part.get("filename"));
            if !filename.is_empty() {
                gj::set_str(&mut out, "filename", &filename);
            }
        }
        _ => return None,
    }
    Some(out)
}

/// interactionsFunctionCallToResponsesWithIdentity.
pub(crate) fn function_call_item(
    item: &Res<'_>,
    antigravity: bool,
    identities: &HashMap<Vec<u8>, Identity>,
) -> Vec<u8> {
    let raw_name = string(&item.get("name"));
    let mut name = if antigravity {
        antigravity_name_to_client(&raw_name)
    } else {
        raw_name.clone()
    };
    let mut namespace = vec![];
    let mut custom = false;
    if let Some(identity) = identities.get(&raw_name).or_else(|| identities.get(&name)) {
        name = identity.name.clone();
        namespace = identity.namespace.clone();
        custom = identity.custom;
    }
    let id = call_id(item);
    let arguments = json_string_value(&item.get("arguments"), b"{}");
    let mut out = if custom {
        br#"{"type":"custom_tool_call","call_id":"","name":"","input":""}"#.to_vec()
    } else {
        br#"{"type":"function_call","call_id":"","name":"","arguments":"{}"}"#.to_vec()
    };
    if !id.is_empty() {
        gj::set_str(&mut out, "call_id", &id);
    }
    if !namespace.is_empty() {
        gj::set_str(&mut out, "namespace", &namespace);
    }
    gj::set_str(&mut out, "name", &name);
    if custom {
        gj::set_str(&mut out, "input", tools::unwrap_custom_tool_input(&arguments));
    } else {
        gj::set_str_no_html(&mut out, "arguments", &arguments);
    }
    out
}

/// responsesToolFromInteractionsTool.
fn tool_to_responses(tool: &Res<'_>, antigravity: bool) -> Option<Vec<u8>> {
    let mut name = first_nonblank(&[&tool.get("name").bytes(), &tool.get("function.name").bytes()]);
    if name.is_empty() {
        return None;
    }
    if antigravity {
        name = antigravity_name_to_client(&name);
    }
    let mut out = br#"{"type":"function","name":""}"#.to_vec();
    gj::set_str(&mut out, "name", &name);
    let description = first_existing(tool, &["description", "function.description"]);
    if description.exists() {
        gj::set_str(&mut out, "description", description.bytes());
    }
    let params = first_existing(tool, &["parameters", "function.parameters", "parametersJsonSchema"]);
    if params.exists() {
        gj::set_raw(&mut out, "parameters", &params.raw);
    }
    Some(out)
}

/// interactionsToolIdentityMap: the client identity of every winning Responses tool,
/// plus (for antigravity) its renamed upstream name.
pub(crate) fn tool_identity_map(raw: &[u8], antigravity: bool) -> HashMap<Vec<u8>, Identity> {
    let mut root = gj::parse(raw);
    let request = root.get("request");
    if request.exists() {
        root = request;
    }
    let descriptors = tools::descriptors(&root);
    let winners = tools::winners(&descriptors);
    let identity = |order: usize| {
        let d = &descriptors[order];
        Identity {
            name: d.local_name.clone(),
            namespace: d.namespace.clone(),
            custom: d.custom,
            apply_patch: apply_patch::is_custom_tool(&d.tool),
        }
    };
    let mut identities: HashMap<Vec<u8>, Identity> = winners
        .iter()
        .map(|(name, &order)| (name.clone(), identity(order)))
        .collect();
    if antigravity {
        for (name, &order) in &winners {
            identities
                .entry(antigravity_name_to_upstream(name))
                .or_insert_with(|| identity(order));
        }
    }
    identities
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values from Go 1.26 (SanitizeDevinToolDescription, regexp (?i) folding).
    #[test]
    fn devin_descriptions_match_go() {
        let exec = sanitize_devin_description(
            b" Exec_Command ",
            "Runs, RETURNING output or a \u{17f}ession ID for ongoing interaction."
                .as_bytes()
                .to_vec(),
        );
        assert_eq!(
            exec,
            b"Runs, returning output or an session ID for ongoing interaction."
        );
        let stdin = sanitize_devin_description(
            b"ns__write_stdin",
            b"WRITES characters to an existing unified exec session and returns recent output!".to_vec(),
        );
        assert_eq!(
            stdin,
            b"Writes characters to a existing unified exec session and returns recent output!"
        );
        let kept = b"returning output or an session ID for ongoing interaction, returning output or a session ID for ongoing interaction";
        assert_eq!(sanitize_devin_description(b"exec_command", kept.to_vec()), kept);
        assert!(is_devin_automation_update(b" MCP__codex_app ", b"Automation_Update"));
        assert!(is_devin_automation_update(b"", b"mcp__codex_app__automation_update"));
        assert!(!is_devin_automation_update(b"mcp__codex_app", b"other"));
    }
}
