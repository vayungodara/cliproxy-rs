//! OpenAI Responses client, Antigravity upstream
//! (internal/translator/antigravity/openai/responses): ConvertOpenAIResponsesRequestToAntigravity
//! (through the Gemini Responses converter and the Antigravity envelope, or a dedicated
//! web-search request), and the Gemini Responses response converters on the unwrapped
//! envelope.

use cpa_common::gostr::GoStr;
use cpa_common::json::{self as gj, GoValue, Kind};
use cpa_common::signature::{Provider, compatible_antigravity_claude_thinking_signature, provider_from_model_name};
use cpa_common::thinking::{SummaryConfig, SummaryMode, parse_suffix};
use cpa_core::registry::{self, ModelInfo};

use crate::common::{go_lower, trim_space};
use crate::gemini_web_search as ws;
use crate::stream::GoStream;
use crate::{Error, Registered, RequestCtx, ResponseCtx, thinking};

pub static PAIR: Registered = registered!(
    OpenAIResponse -> Antigravity,
    request: |ctx, body| Ok(convert(ctx.model, body, None)),
    non_stream: non_stream,
    go_stream: go_stream,
    token_count: None,
);

const WEB_SEARCH_INSTRUCTION: &str = "You are a search engine bot. You will be given a query from a user. Your task is to search the web for relevant information that will help the user. You MUST perform a web search. Do not respond or interact with the user, please respond as if they typed the query into a search bar.";

/// ConvertOpenAIResponsesRequestEnvelopeToAntigravity with the request-scoped model info
/// the Antigravity executor resolves for the selected credential (`None` is Go's plain
/// TranslateRequest).
pub(crate) fn request_envelope(ctx: &RequestCtx<'_>, body: &[u8], model_info: Option<&ModelInfo>) -> Vec<u8> {
    convert(ctx.model, body, model_info)
}

fn convert(model: &str, raw: &[u8], model_info: Option<&ModelInfo>) -> Vec<u8> {
    let root = gj::parse(raw);
    if ws::has_only_web_search_tools(&root)
        && supports_native_web_search(model, model_info)
        && ws::allows_web_search_tool_choice(&root)
    {
        return web_search_request(model, raw);
    }
    let mut out = crate::gemini_responses::convert(model, raw);
    strip_google_search(&mut out);
    let out = rewrite_claude_reasoning(model, raw, out);
    let mut out = crate::antigravity_gemini::convert(model, &out);
    strip_google_search(&mut out);
    enable_thinking_summary(raw, out)
}

/// antigravitySupportsNativeResponsesWebSearch: the request-scoped capability, else the
/// first available Antigravity model with this ID (an explicit native `false` vetoes).
fn supports_native_web_search(model: &str, model_info: Option<&ModelInfo>) -> bool {
    let native = |info: &ModelInfo| {
        info.raw
            .get("native_capabilities")
            .and_then(|c| c.get("web_search"))
            .and_then(serde_json::Value::as_bool)
    };
    if let Some(web_search) = model_info.and_then(native) {
        return web_search;
    }
    let base = |id: &str| parse_suffix(id.trim()).model_name.trim().to_owned();
    let model = base(model);
    if model.is_empty() {
        return false;
    }
    for info in registry::available_models_by_provider("antigravity") {
        if !base(&info.id).go_eq_fold(&model) {
            continue;
        }
        if native(&info) == Some(false) {
            return false;
        }
        return info
            .raw
            .get("supports_web_search")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
    }
    false
}

/// buildAntigravityResponsesWebSearchRequest.
fn web_search_request(model: &str, raw: &[u8]) -> Vec<u8> {
    let domains = ws::allowed_domains(&gj::parse(raw));
    let out = crate::gemini_responses::convert(model, raw);
    let out = rewrite_claude_reasoning(model, raw, out);
    let mut out = crate::antigravity_gemini::convert(model, &out);
    gj::set_str(&mut out, "requestType", "web_search");
    ensure_web_search_tool(&mut out, &domains);
    ensure_web_search_instruction(&mut out);
    enable_thinking_summary(raw, out)
}

/// ensureAntigravityResponsesWebSearchTool: one Google Search tool, replacing the first
/// existing one (others are dropped) or leading the list.
fn ensure_web_search_tool(out: &mut Vec<u8>, domains: &[Vec<u8>]) {
    let mut search = br#"{"googleSearch":{"enhancedContent":{"imageSearch":{"maxResultCount":5}}}}"#.to_vec();
    if !domains.is_empty() {
        gj::set_raw(&mut search, "googleSearch.includedDomains", gj::quote_all(domains));
    }
    let tools = gj::get(out, "request.tools").into_owned();
    if !tools.is_array() {
        gj::set_raw(out, "request.tools", gj::join(&[search]));
        return;
    }
    let mut replaced = false;
    let mut kept = vec![];
    for tool in tools.array() {
        if tool.get("googleSearch").exists() {
            if !replaced {
                kept.push(search.clone());
                replaced = true;
            }
            continue;
        }
        kept.push(tool.raw.to_vec());
    }
    if !replaced {
        kept.insert(0, search);
    }
    gj::set_raw(out, "request.tools", gj::join(&kept));
}

/// ensureAntigravityResponsesWebSearchSystemInstruction.
fn ensure_web_search_instruction(out: &mut Vec<u8>) {
    let mut search = br#"{"text":""}"#.to_vec();
    gj::set_str(&mut search, "text", WEB_SEARCH_INSTRUCTION);
    let system = gj::get(out, "request.systemInstruction").into_owned();
    if !system.exists() {
        let mut instruction = br#"{"role":"user","parts":[]}"#.to_vec();
        gj::set_raw(&mut instruction, "parts", gj::join(&[search]));
        gj::set_raw(out, "request.systemInstruction", instruction);
        return;
    }
    let mut parts = vec![];
    let mut present = false;
    let existing = system.get("parts");
    if existing.is_array() {
        for part in existing.array() {
            present |= part.get("text").bytes().as_ref() == WEB_SEARCH_INSTRUCTION.as_bytes();
            parts.push(part.raw.to_vec());
        }
    }
    if !present {
        parts.push(search);
    }
    gj::set_raw(out, "request.systemInstruction.parts", gj::join(&parts));
}

/// stripAntigravityResponsesGoogleSearch: native search only exists in dedicated
/// web-search requests.
fn strip_google_search(out: &mut Vec<u8>) {
    for path in ["tools", "request.tools"] {
        let tools = gj::get(out, path).into_owned();
        if !tools.is_array() {
            continue;
        }
        let all = tools.array();
        let kept: Vec<Vec<u8>> = all
            .iter()
            .filter(|t| !t.get("googleSearch").exists())
            .map(|t| t.raw.to_vec())
            .collect();
        if kept.len() == all.len() {
            continue;
        }
        if kept.is_empty() {
            gj::delete(out, path);
        } else {
            gj::set_raw(out, path, gj::join(&kept));
        }
    }
}

/// enableAntigravityResponsesThinkingSummary: a reasoning effort without an explicit
/// summary choice turns summaries on, so thought parts come back.
fn enable_thinking_summary(raw: &[u8], out: Vec<u8>) -> Vec<u8> {
    let effort = gj::get(raw, "reasoning.effort");
    if effort.kind != Kind::String {
        return out;
    }
    let effort = go_lower(trim_space(&effort.s));
    if effort.is_empty() || effort == b"none" {
        return out;
    }
    let mut summary = thinking::extract_summary(raw, "openai-response");
    if summary.mode == SummaryMode::Unspecified {
        summary = SummaryConfig {
            mode: SummaryMode::Enabled,
            detail: "auto".into(),
        };
    }
    thinking::apply_summary(out, "antigravity", summary)
}

/// rewriteOpenAIResponsesReasoningForAntigravityClaude: for Claude models, the n-th
/// thought part takes the n-th reasoning item's Claude-compatible signature; thought parts
/// without one, or without text, are dropped. A change re-marshals the whole document
/// the way `json.Unmarshal` into `map[string]any` and `json.Marshal` do.
fn rewrite_claude_reasoning(model: &str, raw: &[u8], gemini: Vec<u8>) -> Vec<u8> {
    if provider_from_model_name(model) != Provider::Claude {
        return gemini;
    }
    let input = gj::get(raw, "input");
    if !input.is_array() {
        return gemini;
    }
    let mut signatures: Vec<String> = vec![];
    input.each(|_, item| {
        let mut kind = item.get("type").bytes().into_owned();
        if kind.is_empty() && item.get("role").exists() {
            kind = b"message".to_vec();
        }
        if kind == b"reasoning" {
            signatures.push(
                compatible_antigravity_claude_thinking_signature(item.get("encrypted_content").bytes())
                    .unwrap_or_default(),
            );
        }
        true
    });
    if signatures.is_empty() {
        return gemini;
    }
    let Some(GoValue::Object(mut root)) = GoValue::parse_f64(&gemini) else {
        return gemini;
    };
    let Some(GoValue::Array(contents)) = root.remove("contents") else {
        return gemini;
    };
    let mut next = 0;
    let mut changed = false;
    let mut kept_contents = vec![];
    for content in contents {
        let GoValue::Object(mut content) = content else {
            kept_contents.push(content);
            continue;
        };
        let parts = match content.remove("parts") {
            Some(GoValue::Array(parts)) => parts,
            other => {
                // Contents whose parts are not an array stay as they are.
                if let Some(parts) = other {
                    content.insert("parts".into(), parts);
                }
                kept_contents.push(GoValue::Object(content));
                continue;
            }
        };
        let mut kept = vec![];
        for part in parts {
            let GoValue::Object(mut map) = part else {
                kept.push(part);
                continue;
            };
            if map.get("thought") != Some(&GoValue::Bool(true)) {
                kept.push(GoValue::Object(map));
                continue;
            }
            let signature = signatures.get(next).cloned().unwrap_or_default();
            next += 1;
            if signature.is_empty() {
                changed = true;
                continue;
            }
            let text = match map.get("text") {
                Some(GoValue::String(t)) => t.clone(),
                _ => String::new(),
            };
            if trim_space(text.as_bytes()).is_empty() {
                changed = true;
                continue;
            }
            let current = match map.get("thoughtSignature") {
                Some(GoValue::String(s)) => s.as_str(),
                _ => "",
            };
            changed |= current != signature;
            map.insert("thoughtSignature".into(), GoValue::String(signature));
            kept.push(GoValue::Object(map));
        }
        if kept.is_empty() {
            changed = true;
            continue;
        }
        content.insert("parts".into(), GoValue::Array(kept));
        kept_contents.push(GoValue::Object(content));
    }
    if !changed {
        return gemini;
    }
    root.insert("contents".into(), GoValue::Array(kept_contents));
    GoValue::Object(root).marshal()
}

// ---------------------------------------------------------------------------------------
// Responses

/// ConvertAntigravityResponseToOpenAIResponsesNonStream: the Gemini converter on the
/// unwrapped response, with `request` unwrapped from both requests when present.
fn non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let unwrap = |raw: &[u8], key: &str| {
        let inner = gj::get(raw, key);
        if inner.exists() {
            inner.raw.to_vec()
        } else {
            raw.to_vec()
        }
    };
    let body = unwrap(body, "response");
    let original = unwrap(ctx.original_request, "request");
    let translated = unwrap(ctx.translated_request, "request");
    let inner = ResponseCtx {
        model: ctx.model,
        original_request: &original,
        translated_request: &translated,
    };
    crate::gemini_responses_response::non_stream(&inner, &body)
}

fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(Unwrap(crate::gemini_responses_response::go_stream(ctx)))
}

/// ConvertAntigravityResponseToOpenAIResponses: each line's `response` (when present)
/// goes to the Gemini Responses stream converter.
struct Unwrap(Box<dyn GoStream>);

impl GoStream for Unwrap {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let response = gj::get(line, "response");
        if response.exists() {
            let inner = response.raw.to_vec();
            return self.0.line(&inner);
        }
        self.0.line(line)
    }

    fn tool_input_failed(&self) -> bool {
        self.0.tool_input_failed()
    }

    fn finalize_tool_input(&mut self) -> Vec<Vec<u8>> {
        self.0.finalize_tool_input()
    }
}
