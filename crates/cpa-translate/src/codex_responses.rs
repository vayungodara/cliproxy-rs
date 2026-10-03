//! OpenAI Responses <-> Codex (internal/translator/codex/openai/responses).
//!
//! Requests keep the client's bytes except for Codex's required fields and removals.
//! Responses pass through, adding the requested model to `response.created` and
//! `response.in_progress` when upstream omits it. An executor may own an apply_patch
//! [`Bridge`] for the request ([`stream_with_bridge`], [`non_stream_with_bridge`]); native
//! Codex never installs one.

use crate::apply_patch_responses::Bridge;
use crate::{Error, Registered, RequestCtx, ResponseCtx, StreamTranslator, common, stream};
use cpa_common::json::{self as gj, Kind};

pub static PAIR: Registered = registered!(
    OpenAIResponse -> Codex,
    request: request,
    non_stream: non_stream,
    go_stream: go_stream,
    token_count: None,
);

fn request(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let _ = ctx;
    Ok(convert(body))
}

/// ConvertOpenAIResponsesRequestToCodex.
pub(crate) fn convert(body: &[u8]) -> Vec<u8> {
    let mut raw = body.to_vec();
    let input = gj::get(&raw, "input");
    if input.kind == Kind::String {
        let mut wrapped = br#"[{"type":"message","role":"user","content":[{"type":"input_text","text":""}]}]"#.to_vec();
        gj::set_str(&mut wrapped, "0.content.0.text", input.bytes());
        gj::set_raw(&mut raw, "input", wrapped);
    }
    set_required_bool(&mut raw, "stream", true);
    set_required_bool(&mut raw, "store", false);
    set_required_bool(&mut raw, "parallel_tool_calls", true);
    set_required_include(&mut raw);
    delete_fields(
        &mut raw,
        &["max_output_tokens", "max_completion_tokens", "temperature", "top_p"],
    );
    let tier = gj::get(&raw, "service_tier");
    if tier.exists() {
        if tier.kind == Kind::String {
            let value = tier.bytes().into_owned();
            match tier.str().trim().to_lowercase().as_str() {
                "priority" | "fast" => {
                    if value != b"priority" {
                        gj::set_str(&mut raw, "service_tier", "priority");
                    }
                }
                "ultrafast" => {
                    if value != b"ultrafast" {
                        gj::set_str(&mut raw, "service_tier", "ultrafast");
                    }
                }
                _ => delete_fields(&mut raw, &["service_tier"]),
            }
        } else {
            delete_fields(&mut raw, &["service_tier"]);
        }
    }
    delete_fields(
        &mut raw,
        &["truncation", "prompt_cache_options", "prompt_cache_retention"],
    );
    raw = strip_cache_breakpoints(raw);
    if gj::get(&raw, "context_management").exists() {
        gj::delete(&mut raw, "context_management");
    }
    delete_fields(&mut raw, &["user"]);
    raw = system_role_to_developer(raw);
    raw = normalize_builtin_tools(raw);
    normalize_empty_function_call_arguments(raw)
}

fn set_required_bool(raw: &mut Vec<u8>, path: &str, value: bool) {
    let current = gj::get(raw, path).kind;
    if (value && current == Kind::True) || (!value && current == Kind::False) {
        return;
    }
    gj::set_bool(raw, path, value);
}

fn set_required_include(raw: &mut Vec<u8>) {
    let current = gj::get(raw, "include");
    let values = current.array();
    if current.is_array()
        && values.len() == 1
        && values[0].kind == Kind::String
        && &*values[0].bytes() == b"reasoning.encrypted_content"
    {
        return;
    }
    gj::set_raw(raw, "include", r#"["reasoning.encrypted_content"]"#);
}

fn delete_fields(raw: &mut Vec<u8>, paths: &[&str]) {
    for path in paths {
        if gj::get(raw, path).exists() {
            gj::delete(raw, path);
        }
    }
}

/// Rebuilds `input` with `edit` applied to each item; unchanged when nothing changed.
fn rebuild_input(raw: Vec<u8>, mut edit: impl FnMut(&gj::Res<'_>, &mut Vec<u8>) -> bool) -> Vec<u8> {
    let input = gj::get(&raw, "input");
    if !input.is_array() {
        return raw;
    }
    let items = input.array();
    if items.is_empty() {
        return raw;
    }
    let mut changed = false;
    let mut rebuilt = Vec::with_capacity(items.len());
    for item in &items {
        let mut item_raw = item.raw.to_vec();
        changed |= edit(item, &mut item_raw);
        rebuilt.push(item_raw);
    }
    if !changed {
        return raw;
    }
    let mut out = raw.clone();
    gj::set_raw(&mut out, "input", gj::join(&rebuilt));
    out
}

fn normalize_empty_function_call_arguments(raw: Vec<u8>) -> Vec<u8> {
    rebuild_input(raw, |item, item_raw| {
        let args = item.get("arguments");
        if item.is_object()
            && item.get("type").str() == "function_call"
            && args.kind == Kind::String
            && common::trim_space(&args.s).is_empty()
        {
            return gj::set_str(item_raw, "arguments", "{}");
        }
        false
    })
}

fn strip_cache_breakpoints(raw: Vec<u8>) -> Vec<u8> {
    if !raw.windows(25).any(|w| w == b"\"prompt_cache_breakpoint\"") {
        return raw;
    }
    rebuild_input(raw, |item, item_raw| {
        let mut changed = false;
        for path in ["content", "output"] {
            let array = item.get(path);
            if !array.is_array() {
                continue;
            }
            let parts = array.array();
            if !parts.iter().any(|p| p.get("prompt_cache_breakpoint").exists()) {
                continue;
            }
            let mut part_changed = false;
            let rebuilt: Vec<Vec<u8>> = parts
                .iter()
                .map(|p| {
                    let mut part = p.raw.to_vec();
                    if p.get("prompt_cache_breakpoint").exists() {
                        part_changed |= gj::delete(&mut part, "prompt_cache_breakpoint");
                    }
                    part
                })
                .collect();
            if part_changed && gj::set_raw(item_raw, path, gj::join(&rebuilt)) {
                changed = true;
            }
        }
        if item.get("prompt_cache_breakpoint").exists() && gj::delete(item_raw, "prompt_cache_breakpoint") {
            changed = true;
        }
        changed
    })
}

/// convertSystemRoleToDeveloper. Go re-marshals the input through `[]json.RawMessage`,
/// which compacts every item and HTML-escapes it, and gives up if any item is invalid.
fn system_role_to_developer(raw: Vec<u8>) -> Vec<u8> {
    let input = gj::get(&raw, "input");
    if !input.is_array() {
        return raw;
    }
    let items = input.array();
    let system = |item: &gj::Res<'_>| item.is_object() && &*item.get("role").bytes() == b"system";
    if !items.iter().any(system) {
        return raw;
    }
    let mut rebuilt = Vec::with_capacity(items.len());
    for item in &items {
        let mut item_raw = item.raw.to_vec();
        if system(item) && !gj::set_raw(&mut item_raw, "role", r#""developer""#) {
            return raw;
        }
        if !gj::valid(&item_raw) {
            return raw;
        }
        rebuilt.push(item_raw);
    }
    let mut out = raw.clone();
    gj::set_raw(&mut out, "input", gj::compact(&gj::join(&rebuilt), true));
    out
}

fn builtin_tool_type(kind: &[u8]) -> Option<&'static str> {
    matches!(kind, b"web_search_preview" | b"web_search_preview_2025_03_11").then_some("web_search")
}

fn normalize_builtin_tools(raw: Vec<u8>) -> Vec<u8> {
    let mut raw = normalize_tool_array(raw, "tools");
    if let Some(kind) = builtin_tool_type(&gj::get(&raw, "tool_choice.type").bytes()) {
        gj::set_str(&mut raw, "tool_choice.type", kind);
    }
    normalize_tool_array(raw, "tool_choice.tools")
}

fn normalize_tool_array(raw: Vec<u8>, path: &str) -> Vec<u8> {
    let tools = gj::get(&raw, path);
    if !tools.is_array() {
        return raw;
    }
    let mut changed = false;
    let mut items = vec![];
    tools.each(|_, tool| {
        let mut item = tool.raw.to_vec();
        if let Some(kind) = builtin_tool_type(&tool.get("type").bytes())
            && gj::set_str(&mut item, "type", kind)
        {
            changed = true;
        }
        items.push(item);
        true
    });
    if !changed {
        return raw;
    }
    let mut out = raw.clone();
    gj::set_raw(&mut out, path, gj::join(&items));
    out
}

fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn stream::GoStream> {
    Box::new(events(ctx, None))
}

/// The Go-shaped line translator behind [`stream_with_bridge`], for golden tests.
#[doc(hidden)]
pub fn go_stream_with_bridge(ctx: &ResponseCtx<'_>, bridge: Option<Bridge>) -> Box<dyn stream::GoStream> {
    Box::new(events(ctx, bridge))
}

fn events(ctx: &ResponseCtx<'_>, bridge: Option<Bridge>) -> Events {
    Events {
        model: ctx.model.as_bytes().to_vec(),
        original: ctx.original_request.to_vec(),
        translated: ctx.translated_request.to_vec(),
        bridge,
    }
}

struct Events {
    model: Vec<u8>,
    original: Vec<u8>,
    translated: Vec<u8>,
    /// Go's executor-owned `param` bridge (responsesBridge).
    bridge: Option<Bridge>,
}

impl stream::GoStream for Events {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let (sse, payload) = match line.strip_prefix(b"data:") {
            Some(rest) => (true, common::trim_space(rest)),
            None => (false, line),
        };
        let updated = with_model(payload, &self.model, &self.original, &self.translated);
        let Some(bridge) = &mut self.bridge else {
            if updated == payload {
                return Ok(vec![line.to_vec()]);
            }
            let mut out = vec![];
            if sse {
                out.extend_from_slice(b"data: ");
            }
            out.extend_from_slice(&updated);
            return Ok(vec![out]);
        };
        // The failure stays on the bridge (tool_input_failed), as Go's param does.
        let (outputs, _) = bridge.transform(&updated);
        Ok(outputs
            .into_iter()
            .map(|o| if sse { [&b"data: "[..], &o].concat() } else { o })
            .collect())
    }

    fn tool_input_failed(&self) -> bool {
        self.bridge.as_ref().is_some_and(|b| b.tool_input_error().is_some())
    }
}

/// setResponsesModel: creation events without `response.model` get the request's model.
fn with_model(raw: &[u8], model: &[u8], original: &[u8], translated: &[u8]) -> Vec<u8> {
    let kind = gj::get(raw, "type").bytes();
    if (&*kind != b"response.created" && &*kind != b"response.in_progress") || gj::get(raw, "response.model").exists() {
        return raw.to_vec();
    }
    let mut name = common::request_model_name(original, translated);
    if name.is_empty() {
        name = model.to_vec();
    }
    let mut out = raw.to_vec();
    if !name.is_empty() {
        gj::set_str(&mut out, "response.model", name);
    }
    out
}

/// The openai-response:codex stream with an executor-owned apply_patch bridge for this
/// request (Go passes the bridge as the translator `param`). After each event, check
/// [`StreamTranslator::tool_input_failed`]: the event's frames end in `response.failed`
/// and the executor stops with HTTP 502 and [`crate::APPLY_PATCH_UPSTREAM_ERROR`].
pub fn stream_with_bridge(ctx: &ResponseCtx<'_>, bridge: Bridge) -> Box<dyn StreamTranslator> {
    let levels = crate::levels(&[ctx.original_request, ctx.translated_request]);
    stream::framed(
        cpa_core::format::Format::OpenAIResponse,
        cpa_core::format::Format::Codex,
        Box::new(events(ctx, Some(bridge))),
        levels,
    )
}

/// The openai-response:codex non-stream with an executor-owned apply_patch bridge. A
/// rejected body is `Err(APPLY_PATCH_UPSTREAM_ERROR)` (Go returns nil and the executor
/// answers 502); the bridge keeps the underlying error.
pub fn non_stream_with_bridge(ctx: &ResponseCtx<'_>, body: &[u8], bridge: &mut Bridge) -> Result<Vec<u8>, Error> {
    let levels = crate::levels(&[body, ctx.original_request, ctx.translated_request]);
    crate::deep_stack(levels, || match bridge.transform_non_stream(body) {
        Ok(converted) => non_stream(ctx, &converted),
        Err(_) => Err(Error(crate::APPLY_PATCH_UPSTREAM_ERROR.into())),
    })
}

fn non_stream(_: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let root = gj::parse(body);
    let kind = root.get("type").bytes();
    if kind.is_empty() && root.get("output").is_array() {
        return Ok(body.to_vec());
    }
    if &*kind != b"response.completed" && &*kind != b"response.incomplete" {
        return Ok(vec![]);
    }
    Ok(root.get("response").raw.to_vec())
}
