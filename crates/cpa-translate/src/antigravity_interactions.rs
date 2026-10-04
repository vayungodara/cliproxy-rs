//! Interactions client, Antigravity upstream (internal/translator/antigravity/interactions):
//! ConvertInteractionsRequestToAntigravity, ConvertAntigravityResponseToInteractions and
//! its NonStream. The input steps convert exactly as for a Gemini upstream.

use std::collections::HashMap;

use cpa_common::json::{self as gj, Kind, Res};

use crate::antigravity_gemini::rewrite_function_names;
use crate::common::{
    format_rfc3339_utc, go_lower, now_nanos, now_unix, restore_sanitized_tool_name, sse_event, trim_space,
};
use crate::gemini::attach_default_safety_settings;
use crate::gemini_interactions::{
    copy_generation_config, copy_response_modalities, copy_system_instruction, first_existing, input_contents,
};
use crate::gemini_interactions_response::thought_signature;
use crate::responses_tools::{disambiguated_tool_name_map, map_sanitized_function_name, sanitized_function_name_map};
use crate::stream::GoStream;
use crate::{Error, Registered, ResponseCtx};

pub static PAIR: Registered = registered!(
    Interactions -> Antigravity,
    request: |ctx, body| Ok(convert(ctx.model, body, ctx.stream)),
    non_stream: non_stream,
    go_stream: go_stream,
    token_count: None,
);

pub(crate) const NAME_FIELDS: [&str; 2] = ["functionCall", "functionResponse"];

// ---------------------------------------------------------------------------------------
// Request

/// ConvertInteractionsRequestToAntigravity. Every edit Go makes lands under `request`, so
/// the request object is built on its own and spliced into the envelope: sjson edits of a
/// nested object produce the same bytes as edits of a standalone one.
fn convert(model: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    let root = gj::parse(raw);
    let names = sanitized_function_name_map(raw);
    let mut req = br#"{"contents":[]}"#.to_vec();
    if stream || root.get("stream").bool() {
        gj::set_bool(&mut req, "stream", true);
    }
    copy_system_instruction(&mut req, &root);
    copy_generation_config(&mut req, &root);
    copy_reasoning(&mut req, &root);
    copy_response_modalities(&mut req, &root);
    copy_tool_choice(&mut req, &root);
    let items = input_contents(&root.get("input"));
    gj::set_items(&mut req, "contents", &items);
    copy_tools(&mut req, &root, &names);
    if gj::get(&req, "toolConfig.functionCallingConfig.mode").bytes().as_ref() == b"NONE" {
        gj::delete(&mut req, "tools");
    }
    rewrite_function_names(
        &mut req,
        &names,
        "contents",
        &NAME_FIELDS,
        &["toolConfig.functionCallingConfig.allowedFunctionNames"],
    );
    let req = attach_default_safety_settings(req, "safetySettings");
    let mut out = br#"{"project":"","request":{"contents":[]},"model":""}"#.to_vec();
    gj::set_str(&mut out, "model", model);
    gj::set_raw(&mut out, "request", req);
    out
}

/// antigravityThinkingSummariesIncludeThoughts.
fn summary_includes_thoughts(summary: &Res<'_>) -> Option<bool> {
    if summary.kind != Kind::String {
        return None;
    }
    match go_lower(trim_space(&summary.s)).as_slice() {
        b"auto" => Some(true),
        b"none" => Some(false),
        _ => None,
    }
}

/// copyInteractionsReasoningToAntigravity: the effort (or thinking level) sets the
/// thinking amount; only an explicit summary selector sets includeThoughts.
fn copy_reasoning(req: &mut Vec<u8>, root: &Res<'_>) {
    let reasoning = root.get("reasoning");
    if !reasoning.exists() {
        return;
    }
    let mut effort = go_lower(trim_space(&reasoning.get("effort").bytes()));
    if effort.is_empty() {
        effort = go_lower(trim_space(&reasoning.get("thinking_level").bytes()));
    }
    if effort == b"auto" {
        gj::set_int(req, "generationConfig.thinkingConfig.thinkingBudget", -1);
    } else if !effort.is_empty() {
        gj::set_str(req, "generationConfig.thinkingConfig.thinkingLevel", &effort);
    }
    let summary = reasoning.get("summary");
    if summary.exists()
        && let Some(include) = summary_includes_thoughts(&summary)
    {
        gj::set_bool(req, "generationConfig.thinkingConfig.includeThoughts", include);
    }
}

/// copyInteractionsToolChoiceToAntigravity (allowed names are kept untrimmed).
fn copy_tool_choice(req: &mut Vec<u8>, root: &Res<'_>) {
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
            let name = choice.get(path).bytes().into_owned();
            if !trim_space(&name).is_empty() {
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
    gj::set_str(req, "toolConfig.functionCallingConfig.mode", mode);
    if !allowed.is_empty() {
        gj::set_strs(req, "toolConfig.functionCallingConfig.allowedFunctionNames", &allowed);
    }
}

/// antigravityFunctionDeclarationJSON: a named declaration (or its nested `function`)
/// with its request-specific name and a default object schema.
fn function_declaration(decl: &Res<'_>, names: &HashMap<Vec<u8>, Vec<u8>>) -> Option<Vec<u8>> {
    let nested = decl.get("function");
    let f = if nested.is_object() { nested } else { decl.clone() };
    let name = f.get("name").bytes();
    if trim_space(&name).is_empty() {
        return None;
    }
    let mut out = br#"{"name":"","parametersJsonSchema":{"type":"object","properties":{}}}"#.to_vec();
    gj::set_str(&mut out, "name", map_sanitized_function_name(names, &name));
    let description = f.get("description");
    if description.exists() {
        gj::set_str(&mut out, "description", description.bytes());
    }
    let params = first_existing(&f, &["parametersJsonSchema", "parameters"]);
    if params.exists() {
        gj::set_raw(&mut out, "parametersJsonSchema", &params.raw);
    }
    for key in ["response", "responseJsonSchema"] {
        let value = f.get(key);
        if value.exists() {
            gj::set_raw(&mut out, key, &value.raw);
        }
    }
    Some(out)
}

/// A built-in tool node: `{"<camel>":{}}` filled from the first object-valued spelling.
fn builtin_node(tool: &Res<'_>, camel: &str, spellings: [&str; 2]) -> Vec<u8> {
    let mut node = format!(r#"{{"{camel}":{{}}}}"#).into_bytes();
    if let Some(value) = spellings.iter().map(|k| tool.get(*k)).find(Res::is_object) {
        gj::set_raw(&mut node, camel, &value.raw);
    }
    node
}

/// copyInteractionsToolsToAntigravity: declarations (deduplicated, in one tool) followed
/// by built-in tools; other entries pass through with snake-case built-in keys renamed.
fn copy_tools(req: &mut Vec<u8>, root: &Res<'_>, names: &HashMap<Vec<u8>, Vec<u8>>) {
    if gj::get(req, "toolConfig.functionCallingConfig.mode").bytes().as_ref() == b"NONE" {
        gj::delete(req, "tools");
        return;
    }
    let tools = root.get("tools");
    if !tools.exists() {
        return;
    }
    if !tools.is_array() {
        gj::set_raw(req, "tools", &tools.raw);
        return;
    }
    let mut declarations = vec![];
    let mut others = vec![];
    tools.each(|_, tool| {
        for key in ["functionDeclarations", "function_declarations"] {
            let decls = tool.get(key);
            if decls.is_array() {
                decls.each(|_, decl| {
                    declarations.extend(function_declaration(&decl, names));
                    true
                });
                return true;
            }
        }
        if tool.get("type").bytes().as_ref() == b"function" || tool.get("name").exists() {
            declarations.extend(function_declaration(&tool, names));
            return true;
        }
        let kind = tool.get("type").bytes().into_owned();
        match kind.as_slice() {
            b"url_context" => others.push(builtin_node(&tool, "urlContext", ["url_context", "urlContext"])),
            b"code_execution" => others.push(builtin_node(
                &tool,
                "codeExecution",
                ["code_execution", "codeExecution"],
            )),
            b"google_search" | b"web_search" => {
                others.push(builtin_node(&tool, "googleSearch", ["google_search", "googleSearch"]))
            }
            _ => {
                let mut raw = tool.raw.to_vec();
                if kind.is_empty() {
                    for (from, to) in [
                        ("url_context", "urlContext"),
                        ("code_execution", "codeExecution"),
                        ("google_search", "googleSearch"),
                        ("web_search", "googleSearch"),
                    ] {
                        let value = tool.get(from);
                        if value.exists() {
                            gj::set_raw(&mut raw, to, &value.raw);
                            gj::delete(&mut raw, from);
                        }
                    }
                }
                others.push(raw);
            }
        }
        true
    });
    let deduplicated = crate::antigravity_chat::deduplicate_declarations(&gj::join(&declarations));
    let has_function = deduplicated.len() > 2;
    if has_function || !others.is_empty() {
        let mut items = vec![];
        if has_function {
            let mut node = br#"{"functionDeclarations":[]}"#.to_vec();
            gj::set_raw(&mut node, "functionDeclarations", &deduplicated);
            items.push(node);
        }
        items.extend(others);
        gj::set_raw(req, "tools", gj::join(&items));
    }
}

// ---------------------------------------------------------------------------------------
// Responses

/// restoreAntigravityUsageMetadata: `cpaUsageMetadata` becomes `usageMetadata` when the
/// chunk has none.
fn restore_usage(root: Res<'static>) -> Res<'static> {
    if !root.get("usageMetadata").exists() {
        let usage = root.get("cpaUsageMetadata");
        if usage.exists() {
            let mut raw = root.raw.to_vec();
            gj::set_raw(&mut raw, "usageMetadata", &usage.raw);
            gj::delete(&mut raw, "cpaUsageMetadata");
            return gj::parse(&raw).into_owned();
        }
    }
    root
}

/// unwrapAntigravityResponse.
fn unwrap(payload: &[u8]) -> Res<'static> {
    let root = gj::parse(payload).into_owned();
    let response = root.get("response");
    restore_usage(if response.exists() { response.into_owned() } else { root })
}

/// restoreInteractionsFunctionNames (function calls and responses).
fn restore_names(root: Res<'static>, names: &HashMap<Vec<u8>, Vec<u8>>) -> Res<'static> {
    if !root.exists() || names.is_empty() {
        return root;
    }
    let mut raw = root.raw.to_vec();
    for (ci, candidate) in root.get("candidates").array().iter().enumerate() {
        for (pi, part) in candidate.get("content.parts").array().iter().enumerate() {
            for field in NAME_FIELDS {
                let name = part.get(&format!("{field}.name"));
                let current = name.bytes();
                if current.is_empty() {
                    continue;
                }
                let restored = restore_sanitized_tool_name(Some(names), &current);
                if name.kind == Kind::String && restored == *current {
                    continue;
                }
                gj::set_str(
                    &mut raw,
                    &format!("candidates.{ci}.content.parts.{pi}.{field}.name"),
                    &restored,
                );
            }
        }
    }
    gj::parse(&raw).into_owned()
}

/// antigravityStreamPayloads: a `data:` line's payload, the `response` (or item) of each
/// element of a JSON array, else the trimmed line.
fn stream_payloads(raw: &[u8]) -> Vec<Vec<u8>> {
    let trimmed = trim_space(raw);
    if let Some(data) = trimmed.strip_prefix(b"data:") {
        return vec![trim_space(data).to_vec()];
    }
    let root = gj::parse(trimmed);
    if root.is_array() {
        let mut payloads = vec![];
        root.each(|_, item| {
            let response = item.get("response");
            if response.exists() {
                payloads.push(response.raw.to_vec());
            } else if item.exists() {
                payloads.push(item.raw.to_vec());
            }
            true
        });
        if !payloads.is_empty() {
            return payloads;
        }
    }
    vec![trimmed.to_vec()]
}

/// antigravityUsageNode.
fn usage_node<'a>(root: &Res<'a>) -> Res<'a> {
    first_existing(root, &["usageMetadata", "usage_metadata", "cpaUsageMetadata"])
}

/// firstAntigravityUsageInt.
fn usage_int(usage: &Res<'_>, camel: &str, snake: &str) -> i64 {
    first_existing(usage, &[camel, snake]).int()
}

/// setInteractionsUsageFromAntigravity.
fn set_usage(out: &mut Vec<u8>, path: &str, root: &Res<'_>) {
    let usage = usage_node(root);
    if !usage.exists() {
        return;
    }
    let set = |out: &mut Vec<u8>, key: &str, value: i64| gj::set_int(out, &format!("{path}.{key}"), value);
    set(
        out,
        "input_tokens",
        usage_int(&usage, "promptTokenCount", "prompt_token_count"),
    );
    set(
        out,
        "output_tokens",
        usage_int(&usage, "candidatesTokenCount", "candidates_token_count"),
    );
    if first_existing(&usage, &["thoughtsTokenCount", "thoughts_token_count"]).exists() {
        set(
            out,
            "reasoning_tokens",
            usage_int(&usage, "thoughtsTokenCount", "thoughts_token_count"),
        );
    }
    set(
        out,
        "total_tokens",
        usage_int(&usage, "totalTokenCount", "total_token_count"),
    );
    if first_existing(&usage, &["cachedContentTokenCount", "cached_content_token_count"]).exists() {
        set(
            out,
            "cached_tokens",
            usage_int(&usage, "cachedContentTokenCount", "cached_content_token_count"),
        );
    }
}

/// setInteractionsStreamUsageFromAntigravity.
fn set_stream_usage(out: &mut Vec<u8>, path: &str, root: &Res<'_>) {
    let usage = usage_node(root);
    if !usage.exists() {
        return;
    }
    let input = usage_int(&usage, "promptTokenCount", "prompt_token_count");
    let set = |out: &mut Vec<u8>, key: &str, value: i64| gj::set_int(out, &format!("{path}.{key}"), value);
    set(
        out,
        "total_tokens",
        usage_int(&usage, "totalTokenCount", "total_token_count"),
    );
    set(out, "total_input_tokens", input);
    gj::set_raw(
        out,
        &format!("{path}.input_tokens_by_modality"),
        format!(r#"[{{"modality":"text","tokens":{input}}}]"#),
    );
    set(
        out,
        "total_cached_tokens",
        usage_int(&usage, "cachedContentTokenCount", "cached_content_token_count"),
    );
    set(
        out,
        "total_output_tokens",
        usage_int(&usage, "candidatesTokenCount", "candidates_token_count"),
    );
    set(out, "total_tool_use_tokens", 0);
    set(
        out,
        "total_thought_tokens",
        usage_int(&usage, "thoughtsTokenCount", "thoughts_token_count"),
    );
}

/// hasAntigravityStreamUsage.
fn has_stream_usage(root: &Res<'_>) -> bool {
    let usage = usage_node(root);
    usage.exists()
        && [
            "promptTokenCount",
            "candidatesTokenCount",
            "totalTokenCount",
            "thoughtsTokenCount",
            "cachedContentTokenCount",
            "prompt_token_count",
            "candidates_token_count",
            "total_token_count",
            "thoughts_token_count",
            "cached_content_token_count",
        ]
        .iter()
        .any(|p| usage.get(*p).exists())
}

/// antigravityThoughtStepJSON.
fn thought_step(signature: &[u8], text: &[u8]) -> Vec<u8> {
    let mut step = br#"{"type":"thought"}"#.to_vec();
    if !signature.is_empty() {
        gj::set_str(&mut step, "signature", signature);
    }
    if !text.is_empty() {
        let mut item = br#"{"type":"text","text":""}"#.to_vec();
        gj::set_str(&mut item, "text", text);
        gj::set_items(&mut step, "content", &[item]);
    }
    step
}

/// antigravityInlineDataToInteractionsStep: `None` without a MIME type or data.
fn inline_step(inline: &Res<'_>) -> Option<Vec<u8>> {
    let mime = first_nonempty(inline, &["mimeType", "mime_type"]);
    let data = inline.get("data").bytes();
    if mime.is_empty() || data.is_empty() {
        return None;
    }
    let lower = go_lower(&mime);
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
    gj::set_str(&mut item, "mime_type", &mime);
    gj::set_str(&mut item, "data", &data);
    let mut step = br#"{"type":"model_output","content":[]}"#.to_vec();
    gj::set_raw(&mut step, "content.-1", item);
    Some(step)
}

fn first_nonempty(r: &Res<'_>, paths: &[&str]) -> Vec<u8> {
    paths
        .iter()
        .map(|p| r.get(*p).bytes().into_owned())
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// The `id`, else `call_id`, of a function part as `call_id`.
fn set_call_id(step: &mut Vec<u8>, part: &Res<'_>) {
    let id = first_existing(part, &["id", "call_id"]);
    if id.exists() {
        gj::set_str(step, "call_id", id.bytes());
    }
}

/// antigravityPartToInteractionsSteps.
fn part_to_steps(part: &Res<'_>) -> Vec<Vec<u8>> {
    let signature = thought_signature(part);
    let with_signature = |step: Vec<u8>| {
        let mut steps = vec![step];
        if !signature.is_empty() {
            steps.push(thought_step(&signature, b""));
        }
        steps
    };
    let call = part.get("functionCall");
    if call.exists() {
        let mut steps = vec![];
        if !signature.is_empty() {
            steps.push(thought_step(&signature, b""));
        }
        let mut step = br#"{"type":"function_call","name":"","arguments":{}}"#.to_vec();
        gj::set_str(&mut step, "name", call.get("name").bytes());
        set_call_id(&mut step, &call);
        let args = call.get("args");
        if args.exists() {
            gj::set_raw(&mut step, "arguments", &args.raw);
        }
        steps.push(step);
        return steps;
    }
    let result = part.get("functionResponse");
    if result.exists() {
        let mut step = br#"{"type":"function_result","name":"","result":{}}"#.to_vec();
        gj::set_str(&mut step, "name", result.get("name").bytes());
        set_call_id(&mut step, &result);
        let response = result.get("response");
        if response.exists() {
            gj::set_raw(&mut step, "result", &response.raw);
        }
        return vec![step];
    }
    let text = part.get("text");
    if text.exists() {
        if part.get("thought").bool() {
            return vec![thought_step(&signature, &text.bytes())];
        }
        if text.bytes().is_empty() {
            return if signature.is_empty() {
                vec![]
            } else {
                vec![thought_step(&signature, b"")]
            };
        }
        let mut item = br#"{"type":"text","text":""}"#.to_vec();
        gj::set_str(&mut item, "text", text.bytes());
        let mut step = br#"{"type":"model_output","content":[]}"#.to_vec();
        gj::set_items(&mut step, "content", &[item]);
        return with_signature(step);
    }
    for key in ["inlineData", "inline_data"] {
        let inline = part.get(key);
        if inline.exists()
            && let Some(step) = inline_step(&inline)
        {
            return with_signature(step);
        }
    }
    if signature.is_empty() {
        vec![]
    } else {
        vec![thought_step(&signature, b"")]
    }
}

/// ConvertAntigravityResponseToInteractionsNonStream.
fn non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let root = restore_names(unwrap(body), &disambiguated_tool_name_map(ctx.original_request));
    let mut out = br#"{"id":"","object":"interaction","status":"completed","model":"","steps":[]}"#.to_vec();
    let mut id = root.get("responseId").bytes().into_owned();
    if id.is_empty() {
        id = format!("interaction_{}", now_nanos()).into_bytes();
    }
    gj::set_str(&mut out, "id", &id);
    gj::set_str(&mut out, "model", ctx.model);
    let mut steps = vec![];
    root.get("candidates.0.content.parts").each(|_, part| {
        steps.extend(part_to_steps(&part).into_iter().filter(|s| !s.is_empty()));
        true
    });
    gj::set_items(&mut out, "steps", &steps);
    set_usage(&mut out, "usage", &root);
    Ok(out)
}

fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(Stream {
        model: ctx.model.as_bytes().to_vec(),
        id: format!("interaction_{}", now_nanos()).into_bytes(),
        names: disambiguated_tool_name_map(ctx.original_request),
        ..Stream::default()
    })
}

/// antigravityToInteractionsStreamState.
#[derive(Default)]
struct Stream {
    model: Vec<u8>,
    id: Vec<u8>,
    names: HashMap<Vec<u8>, Vec<u8>>,
    started: bool,
    finished: bool,
    completed: bool,
    done: bool,
    step_open: bool,
    step_type: &'static str,
    step_index: i64,
    next_step: i64,
}

const TEXT_DELTA: &[u8] = br#"{"index":0,"delta":{"text":"","type":"text"},"event_type":"step.delta"}"#;
const THOUGHT_DELTA: &[u8] =
    br#"{"index":0,"delta":{"content":{"text":"","type":"text"},"type":"thought_summary"},"event_type":"step.delta"}"#;
const ARGUMENTS_DELTA: &[u8] =
    br#"{"index":0,"delta":{"arguments":"","type":"arguments_delta"},"event_type":"step.delta"}"#;
const RESULT_DELTA: &[u8] =
    br#"{"index":0,"delta":{"type":"function_result","name":"","result":{}},"event_type":"step.delta"}"#;
const SIGNATURE_DELTA: &[u8] =
    br#"{"index":0,"delta":{"signature":"","type":"thought_signature"},"event_type":"step.delta"}"#;

impl Stream {
    /// appendAntigravityInteractionsCompleted (usage only when the chunk is given).
    fn complete(&mut self, root: Option<&Res<'_>>, out: &mut Vec<Vec<u8>>) {
        let now = format_rfc3339_utc(now_unix());
        let mut completed = br#"{"interaction":{"id":"","status":"completed","usage":{},"created":"","updated":"","service_tier":"standard","object":"interaction","model":""},"event_type":"interaction.completed"}"#.to_vec();
        gj::set_str(&mut completed, "interaction.id", &self.id);
        gj::set_str(&mut completed, "interaction.created", &now);
        gj::set_str(&mut completed, "interaction.updated", &now);
        gj::set_str(&mut completed, "interaction.model", &self.model);
        if let Some(root) = root.filter(|r| r.exists()) {
            set_stream_usage(&mut completed, "interaction.usage", root);
        }
        out.push(sse_event("interaction.completed", &completed));
        self.completed = true;
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

    /// ensureAntigravityInteractionsStep / appendAntigravityInteractionsStepStart (a
    /// function call step carries `id` and `call_id`).
    fn ensure_step(&mut self, kind: &'static str, part: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        if self.step_open && self.step_type == kind {
            return;
        }
        self.stop_step(out);
        let step_id = format!("step_{}", now_nanos());
        self.step_index = self.next_step;
        self.next_step += 1;
        self.step_type = kind;
        self.step_open = true;
        let mut start = br#"{"index":0,"step":{"type":""},"event_type":"step.start"}"#.to_vec();
        gj::set_int(&mut start, "index", self.step_index);
        gj::set_str(&mut start, "step.type", kind);
        if kind == "function_call" {
            let id = first_existing(part, &["id", "call_id"]).bytes().into_owned();
            let id = if id.is_empty() { step_id.into_bytes() } else { id };
            gj::set_str(&mut start, "step.id", &id);
            gj::set_str(&mut start, "step.call_id", &id);
            gj::set_str(&mut start, "step.name", part.get("name").bytes());
            gj::set_raw(&mut start, "step.arguments", b"{}");
        }
        out.push(sse_event("step.start", &start));
    }

    fn delta(&self, template: &[u8], path: &str, value: &[u8], out: &mut Vec<Vec<u8>>) {
        let mut delta = template.to_vec();
        gj::set_int(&mut delta, "index", self.step_index);
        gj::set_str(&mut delta, path, value);
        out.push(sse_event("step.delta", &delta));
    }

    fn signature(&mut self, part: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        let signature = thought_signature(part);
        if !signature.is_empty() {
            self.ensure_step("thought", &Res::default(), out);
            self.delta(SIGNATURE_DELTA, "delta.signature", &signature, out);
        }
    }

    /// appendAntigravityPartToInteractionsStream.
    fn part(&mut self, part: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        let text = part.get("text");
        if text.exists() && !text.bytes().is_empty() {
            if part.get("thought").bool() {
                self.ensure_step("thought", &Res::default(), out);
                self.delta(THOUGHT_DELTA, "delta.content.text", &text.bytes(), out);
            } else {
                self.ensure_step("model_output", &Res::default(), out);
                self.delta(TEXT_DELTA, "delta.text", &text.bytes(), out);
            }
            self.signature(part, out);
            return;
        }
        let call = part.get("functionCall");
        if call.exists() {
            self.signature(part, out);
            self.ensure_step("function_call", &call, out);
            let args = call.get("args");
            let arguments: &[u8] = if args.exists() { &args.raw } else { b"{}" };
            self.delta(ARGUMENTS_DELTA, "delta.arguments", arguments, out);
            self.stop_step(out);
            return;
        }
        let result = part.get("functionResponse");
        if result.exists() {
            self.ensure_step("function_result", &result, out);
            let mut delta = RESULT_DELTA.to_vec();
            gj::set_int(&mut delta, "index", self.step_index);
            gj::set_str(&mut delta, "delta.name", result.get("name").bytes());
            let response = result.get("response");
            if response.exists() {
                gj::set_raw(&mut delta, "delta.result", &response.raw);
            }
            out.push(sse_event("step.delta", &delta));
            self.stop_step(out);
            return;
        }
        self.signature(part, out);
    }

    fn payload(&mut self, payload: &[u8], out: &mut Vec<Vec<u8>>) {
        if trim_space(payload) == b"[DONE]" {
            if !self.completed {
                self.stop_step(out);
                self.complete(None, out);
            }
            if !self.done {
                out.push(sse_event("done", b"[DONE]"));
                self.done = true;
            }
            return;
        }
        let root = restore_names(unwrap(payload), &self.names);
        if !root.exists() {
            return;
        }
        if !self.started {
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
        root.get("candidates.0.content.parts").each(|_, part| {
            self.part(&part, out);
            true
        });
        if root.get("candidates.0.finishReason").exists() && !self.finished {
            self.stop_step(out);
            self.finished = true;
        }
        if has_stream_usage(&root) && self.finished && !self.completed {
            self.complete(Some(&root), out);
        }
    }
}

impl GoStream for Stream {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let mut out = vec![];
        for payload in stream_payloads(line) {
            self.payload(&payload, &mut out);
        }
        Ok(out)
    }
}
