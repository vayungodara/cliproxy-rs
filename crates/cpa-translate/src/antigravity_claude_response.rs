//! Antigravity responses -> Claude Messages (antigravity_claude_response.go and the
//! response half of web_search.go): ConvertAntigravityResponseToClaude, its NonStream and
//! the web-search grounding blocks.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine;
use cpa_common::json::{self as gj, GoValue, Res};
use cpa_common::signature::{self as sig, Provider};
use sha2::{Digest, Sha256};

use crate::antigravity_claude::{
    ANY, FUNCTION, NEXT, PREVIOUS, STANDALONE, TEXT, encode_carrier, has_typed_web_search_tool,
};
use crate::common::{go_runes, hex, now_nanos, restore_sanitized_tool_name, sanitize_claude_tool_id, trim_space};
use crate::replay_cache::{cache_signature, model_group};
use crate::responses_tools::disambiguated_tool_name_map;
use crate::stream::GoStream;
use crate::{Error, ResponseCtx};

/// toolUseIDCounter.
static TOOL_USE_IDS: AtomicU64 = AtomicU64::new(0);

const NONE: u8 = 0;
const CONTENT: u8 = 1;
const THINKING: u8 = 2;
const FUNCTION_CALL: u8 = 3;

/// common.AppendSSEEventString with three trailing newlines.
fn push_event(out: &mut Vec<u8>, event: &str, payload: &[u8]) {
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\n\n\n");
}

fn model_name(translated: &[u8]) -> String {
    String::from_utf8_lossy(&gj::get(translated, "model").bytes()).into_owned()
}

/// decodeSignature: an `R` (double base64) signature decoded once; undecodable ones
/// become empty.
fn decode_signature(signature: &[u8]) -> Vec<u8> {
    if signature.starts_with(b"R") {
        return crate::gemini_responses::go_base64(true)
            .decode(
                signature
                    .iter()
                    .copied()
                    .filter(|&c| c != b'\r' && c != b'\n')
                    .collect::<Vec<u8>>(),
            )
            .unwrap_or_default();
    }
    signature.to_vec()
}

/// formatClaudeSignatureValue: Claude-group models get the provider-native `E` form.
fn claude_signature_value(model: &str, signature: &[u8]) -> Vec<u8> {
    if model_group(model) == "claude" {
        decode_signature(signature)
    } else {
        signature.to_vec()
    }
}

/// formatGeminiClaudeCarrierValue: Gemini targets wrap the signature in a carrier.
fn carrier_value(model: &str, signature: &[u8], direction: &str, target: &str) -> Vec<u8> {
    if sig::provider_from_model_name(model) == Provider::Gemini {
        encode_carrier(signature, direction, target)
    } else {
        claude_signature_value(model, signature)
    }
}

/// util.GeminiClaudeToolUseID: a stable ID from the call ID, name and canonical args.
pub(crate) fn gemini_claude_tool_use_id(call_id: &[u8], name: &[u8], args: &[u8]) -> Vec<u8> {
    let (call_id, name) = (trim_space(call_id), trim_space(name));
    if call_id.is_empty() || name.is_empty() {
        return vec![];
    }
    let mut args = args.to_vec();
    if !trim_space(&args).is_empty() {
        args = match GoValue::parse_f64(&args) {
            Some(value) => value.marshal(),
            None => trim_space(&args).to_vec(),
        };
    }
    let sum = Sha256::digest([call_id, b"\x00", name, b"\x00", &args].concat());
    format!("cpa_gemini_{}", hex(&sum[..16])).into_bytes()
}

/// antigravityClaudeToolUseID.
fn tool_use_id(model: &str, call: &Res<'_>, fallback: &[u8]) -> Vec<u8> {
    if sig::provider_from_model_name(model) == Provider::Gemini {
        let stable = gemini_claude_tool_use_id(
            &call.get("id").bytes(),
            &call.get("name").bytes(),
            &call.get("args").raw,
        );
        if !stable.is_empty() {
            return stable;
        }
    }
    sanitize_claude_tool_id(fallback)
}

// ---------------------------------------------------------------------------------------
// Web search grounding (web_search.go)

/// shouldTranslateWebSearchGrounding: a typed web search tool in the client request and
/// Google Search in the upstream request.
fn translates_grounding(original: &[u8], translated: &[u8]) -> bool {
    let tools = gj::get(translated, "request.tools");
    has_typed_web_search_tool(original)
        && tools.is_array()
        && tools.array().iter().any(|t| t.get("googleSearch").exists())
}

/// antigravityGroundingMetadata.
fn grounding_metadata<'a>(root: &Res<'a>) -> Res<'a> {
    let gm = root.get("response.candidates.0.groundingMetadata");
    if gm.exists() {
        gm
    } else {
        root.get("candidates.0.groundingMetadata")
    }
}

/// antigravityTextContent.
fn text_content(root: &Res<'_>) -> Vec<u8> {
    let mut parts = root.get("response.candidates.0.content.parts");
    if !parts.is_array() {
        parts = root.get("candidates.0.content.parts");
    }
    let mut text = vec![];
    if parts.is_array() {
        for part in parts.array() {
            let t = part.get("text");
            if t.exists() {
                text.extend_from_slice(&t.bytes());
            }
        }
    }
    text
}

fn search_query(gm: &Res<'_>) -> Vec<u8> {
    let queries = gm.get("webSearchQueries");
    if queries.is_array()
        && let Some(first) = queries.array().first()
    {
        return first.bytes().into_owned();
    }
    vec![]
}

/// webSearchResultsFromGrounding: one result per distinct web URI.
fn search_results(gm: &Res<'_>) -> Vec<u8> {
    let mut results = b"[]".to_vec();
    let chunks = gm.get("groundingChunks");
    if !chunks.is_array() {
        return results;
    }
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for chunk in chunks.array() {
        let web = chunk.get("web");
        if !web.exists() {
            continue;
        }
        let uri = trim_space(&web.get("uri").bytes()).to_vec();
        if uri.is_empty() || !seen.insert(uri.clone()) {
            continue;
        }
        let mut result = br#"{"type":"web_search_result","page_age":null}"#.to_vec();
        let title = web.get("title");
        if title.exists() {
            gj::set_str(&mut result, "title", title.bytes());
        }
        gj::set_str(&mut result, "url", &uri);
        gj::set_raw(&mut results, "-1", result);
    }
    results
}

struct Support {
    start: i64,
    end: i64,
    urls: Vec<Vec<u8>>,
    title: Vec<u8>,
}

/// parseWebSearchGroundingSupports.
fn supports(gm: &Res<'_>) -> Vec<Support> {
    let chunks = gm.get("groundingChunks");
    if !chunks.is_array() {
        return vec![];
    }
    let data: Vec<(Vec<u8>, Vec<u8>)> = chunks
        .array()
        .iter()
        .map(|chunk| {
            let web = chunk.get("web");
            if web.exists() {
                (
                    web.get("uri").bytes().into_owned(),
                    web.get("title").bytes().into_owned(),
                )
            } else {
                (vec![], vec![])
            }
        })
        .collect();
    let list = gm.get("groundingSupports");
    if !list.is_array() {
        return vec![];
    }
    let mut out = vec![];
    for support in list.array() {
        let segment = support.get("segment");
        if !segment.exists() {
            continue;
        }
        let mut parsed = Support {
            start: segment.get("startIndex").int(),
            end: segment.get("endIndex").int(),
            urls: vec![],
            title: vec![],
        };
        let indices = support.get("groundingChunkIndices");
        if indices.is_array() {
            for index in indices.array() {
                let i = index.int();
                if i < 0 || i >= data.len() as i64 {
                    continue;
                }
                let (url, title) = &data[i as usize];
                parsed.urls.push(url.clone());
                if parsed.title.is_empty() {
                    parsed.title = title.clone();
                }
            }
        }
        out.push(parsed);
    }
    out
}

/// One text block of the answer, with its citation (a marshaled map) when cited.
struct CitedBlock {
    text: Vec<u8>,
    citation: Option<Vec<u8>>,
}

/// buildWebSearchCitedTextBlocks: the answer split at supported segments (byte offsets);
/// each cited segment carries its first source.
fn cited_blocks(text: &[u8], supports: &[Support]) -> Vec<CitedBlock> {
    let plain = |t: &[u8]| CitedBlock {
        text: t.to_vec(),
        citation: None,
    };
    if supports.is_empty() {
        return if text.is_empty() { vec![] } else { vec![plain(text)] };
    }
    let len = text.len() as i64;
    let slice = |start: i64, end: i64| &text[start.clamp(0, len) as usize..end.clamp(0, len) as usize];
    let mut blocks = vec![];
    let mut last_end = 0i64;
    for s in supports {
        if s.end <= last_end {
            continue;
        }
        if s.start > last_end {
            let end = s.start.min(len);
            if last_end < end {
                blocks.push(plain(slice(last_end, end)));
            }
        }
        let cited_start = s.start.max(last_end);
        let mut cited: &[u8] = b"";
        if cited_start < s.end {
            let (start, end) = (cited_start.min(len), s.end.min(len));
            if start < end {
                cited = slice(start, end);
            }
        }
        if !cited.is_empty() && !s.urls.is_empty() {
            // json.Marshal of the citation map: sorted keys.
            let citation = [
                &b"{\"cited_text\":"[..],
                &gj::quote(cited),
                b",\"title\":",
                &gj::quote(&s.title),
                b",\"type\":\"web_search_result_location\",\"url\":",
                &gj::quote(&s.urls[0]),
                b"}",
            ]
            .concat();
            blocks.push(CitedBlock {
                text: cited.to_vec(),
                citation: Some(citation),
            });
        }
        last_end = last_end.max(s.end);
    }
    if last_end < len {
        blocks.push(plain(&text[last_end as usize..]));
    }
    blocks
}

fn web_search_tool_use_id() -> Vec<u8> {
    format!("srvtoolu_{}", now_nanos()).into_bytes()
}

/// buildClaudeWebSearchContent.
fn web_search_content(tool_use_id: &[u8], text: &[u8], gm: &Res<'_>) -> Vec<u8> {
    let mut content = b"[]".to_vec();
    let mut tool_use = br#"{"type":"server_tool_use","id":"","name":"web_search","input":{}}"#.to_vec();
    gj::set_str(&mut tool_use, "id", tool_use_id);
    let query = search_query(gm);
    if !query.is_empty() {
        gj::set_str(&mut tool_use, "input.query", &query);
    }
    gj::set_raw(&mut content, "-1", tool_use);
    let mut result = br#"{"type":"web_search_tool_result","tool_use_id":"","content":[]}"#.to_vec();
    gj::set_str(&mut result, "tool_use_id", tool_use_id);
    gj::set_raw(&mut result, "content", search_results(gm));
    gj::set_raw(&mut content, "-1", result);
    for block in cited_blocks(text, &supports(gm)) {
        if block.text.is_empty() {
            continue;
        }
        let mut text_block = br#"{"type":"text","text":""}"#.to_vec();
        gj::set_str(&mut text_block, "text", &block.text);
        if let Some(citation) = &block.citation {
            gj::set_raw(&mut text_block, "citations", gj::join(&[citation]));
        }
        gj::set_raw(&mut content, "-1", text_block);
    }
    content
}

/// splitRunesForWebSearch: chunks of `size` runes (invalid bytes become U+FFFD).
fn split_runes(text: &[u8], size: usize) -> Vec<String> {
    let runes: Vec<char> = go_runes(text).collect();
    runes.chunks(size).map(|c| c.iter().collect()).collect()
}

/// appendClaudeWebSearchStreamBlocks: the server tool use, its results, then the cited
/// answer in 50-rune text deltas; returns the next block index.
fn web_search_stream_blocks(out: &mut Vec<u8>, start: i64, id: &[u8], text: &[u8], gm: &Res<'_>) -> i64 {
    let mut index = start;
    let id = String::from_utf8_lossy(id);
    push_event(
        out,
        "content_block_start",
        format!(r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"server_tool_use","id":"{id}","name":"web_search","input":{{}}}}}}"#).as_bytes(),
    );
    let query = search_query(gm);
    if !query.is_empty() {
        let mut query_json = b"{}".to_vec();
        gj::set_str(&mut query_json, "query", &query);
        let mut delta = format!(r#"{{"type":"content_block_delta","index":{index},"delta":{{"type":"input_json_delta","partial_json":""}}}}"#).into_bytes();
        gj::set_str(&mut delta, "delta.partial_json", &query_json);
        push_event(out, "content_block_delta", &delta);
    }
    push_event(
        out,
        "content_block_stop",
        format!(r#"{{"type":"content_block_stop","index":{index}}}"#).as_bytes(),
    );
    index += 1;
    let mut result = format!(r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"web_search_tool_result","tool_use_id":"{id}","content":[]}}}}"#).into_bytes();
    gj::set_raw(&mut result, "content_block.content", search_results(gm));
    push_event(out, "content_block_start", &result);
    push_event(
        out,
        "content_block_stop",
        format!(r#"{{"type":"content_block_stop","index":{index}}}"#).as_bytes(),
    );
    index += 1;
    for block in cited_blocks(text, &supports(gm)) {
        if block.text.is_empty() {
            continue;
        }
        let start = if block.citation.is_some() {
            format!(
                r#"{{"type":"content_block_start","index":{index},"content_block":{{"citations":[],"type":"text","text":""}}}}"#
            )
        } else {
            format!(r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"text","text":""}}}}"#)
        };
        push_event(out, "content_block_start", start.as_bytes());
        if let Some(citation) = &block.citation {
            let delta = [
                format!(
                    r#"{{"type":"content_block_delta","index":{index},"delta":{{"type":"citations_delta","citation":"#
                )
                .as_bytes(),
                citation,
                b"}}",
            ]
            .concat();
            push_event(out, "content_block_delta", &delta);
        }
        for chunk in split_runes(&block.text, 50) {
            let mut delta = format!(
                r#"{{"type":"content_block_delta","index":{index},"delta":{{"type":"text_delta","text":""}}}}"#
            )
            .into_bytes();
            gj::set_str(&mut delta, "delta.text", chunk.as_bytes());
            push_event(out, "content_block_delta", &delta);
        }
        push_event(
            out,
            "content_block_stop",
            format!(r#"{{"type":"content_block_stop","index":{index}}}"#).as_bytes(),
        );
        index += 1;
    }
    index
}

// ---------------------------------------------------------------------------------------
// Stream

pub(crate) fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(Stream {
        model: model_name(ctx.translated_request),
        names: disambiguated_tool_name_map(ctx.original_request),
        web_search_mode: translates_grounding(ctx.original_request, ctx.translated_request),
        ..Stream::default()
    })
}

/// Params of ConvertAntigravityResponseToClaude.
#[derive(Default)]
struct Stream {
    model: String,
    names: HashMap<Vec<u8>, Vec<u8>>,
    web_search_mode: bool,
    out: Vec<u8>,
    has_first_response: bool,
    response_type: u8,
    index: i64,
    has_finish_reason: bool,
    finish_reason: Vec<u8>,
    has_usage: bool,
    prompt_tokens: i64,
    candidates_tokens: i64,
    thoughts_tokens: i64,
    total_tokens: i64,
    cached_tokens: i64,
    sent_final: bool,
    has_tool_use: bool,
    has_content: bool,
    has_semantic: bool,
    last_semantic_kind: &'static str,
    has_web_search_tool: bool,
    web_search_requests: i64,
    web_search_text: Vec<u8>,
    thinking_text: Vec<u8>,
    thinking_signed: bool,
}

impl Stream {
    fn event(&mut self, event: &str, payload: &[u8]) {
        push_event(&mut self.out, event, payload);
    }

    fn stop_event(&mut self) {
        let payload = format!(r#"{{"type":"content_block_stop","index":{}}}"#, self.index);
        self.event("content_block_stop", payload.as_bytes());
    }

    fn delta(&mut self, delta_json: &str, path: &str, value: &[u8]) {
        let mut data = format!(
            r#"{{"type":"content_block_delta","index":{},"delta":{delta_json}}}"#,
            self.index
        )
        .into_bytes();
        gj::set_str(&mut data, path, value);
        self.event("content_block_delta", &data);
    }

    fn start_block(&mut self, block_json: &str) {
        let payload = format!(
            r#"{{"type":"content_block_start","index":{},"content_block":{block_json}}}"#,
            self.index
        );
        self.event("content_block_start", payload.as_bytes());
    }

    fn signature_delta(&mut self, signature: &[u8], direction: &str, target: &str) {
        let value = carrier_value(&self.model, signature, direction, target);
        self.delta(
            r#"{"type":"signature_delta","signature":""}"#,
            "delta.signature",
            &value,
        );
        self.thinking_signed = true;
        self.has_content = true;
    }

    /// appendThinkingSignature: also caches the signature for the thinking text so far.
    fn thinking_signature(&mut self, signature: &[u8], direction: &str, target: &str) {
        if signature.is_empty() || self.response_type != THINKING {
            return;
        }
        if !self.thinking_text.is_empty() {
            let text = std::mem::take(&mut self.thinking_text);
            cache_signature(&self.model, &text, signature);
        }
        self.signature_delta(signature, direction, target);
    }

    /// closeCurrentBlock.
    fn close_block(&mut self) {
        if self.response_type == NONE {
            return;
        }
        self.stop_event();
        self.index += 1;
        self.response_type = NONE;
        self.thinking_signed = false;
    }

    /// appendPartSignature: the open unsigned thinking block takes it; a trailing text
    /// signature is only cached; otherwise it goes on an empty thinking carrier block
    /// (true).
    fn part_signature(&mut self, signature: &[u8], direction: &str, target: &str) -> bool {
        if signature.is_empty() {
            return false;
        }
        if self.response_type == THINKING && !self.thinking_signed {
            self.thinking_signature(signature, direction, target);
            return false;
        }
        if direction == PREVIOUS && target == TEXT {
            cache_signature(&self.model, b"", signature);
            return false;
        }
        self.close_block();
        self.start_block(r#"{"type":"thinking","thinking":""}"#);
        self.response_type = THINKING;
        self.thinking_signed = false;
        self.has_content = true;
        self.signature_delta(signature, direction, target);
        true
    }

    fn part(&mut self, part: &Res<'_>) {
        let text = part.get("text");
        let call = part.get("functionCall");
        let mut signature = part.get("thoughtSignature");
        if !signature.exists() {
            signature = part.get("thought_signature");
        }
        let signature_bytes = signature.bytes().into_owned();
        let has_signature = signature.exists() && !signature_bytes.is_empty() && !call.exists();
        if has_signature && text.bytes().is_empty() {
            let (direction, target) = if self.has_semantic {
                (PREVIOUS, self.last_semantic_kind)
            } else {
                (NEXT, ANY)
            };
            self.part_signature(&signature_bytes, direction, target);
            return;
        }
        if text.exists() {
            let text = text.bytes().into_owned();
            if part.get("thought").bool() {
                if !text.is_empty() {
                    self.has_semantic = true;
                    self.last_semantic_kind = TEXT;
                    if self.response_type == THINKING && self.thinking_signed {
                        self.close_block();
                    }
                    if self.response_type == THINKING {
                        self.thinking_text.extend_from_slice(&text);
                        self.delta(r#"{"type":"thinking_delta","thinking":""}"#, "delta.thinking", &text);
                        self.has_content = true;
                    } else {
                        if self.response_type != NONE {
                            self.stop_event();
                            self.index += 1;
                        }
                        self.start_block(r#"{"type":"thinking","thinking":""}"#);
                        self.thinking_signed = false;
                        self.delta(r#"{"type":"thinking_delta","thinking":""}"#, "delta.thinking", &text);
                        self.response_type = THINKING;
                        self.has_content = true;
                        self.thinking_text = text.clone();
                    }
                }
                if has_signature {
                    self.thinking_signature(&signature_bytes, STANDALONE, TEXT);
                }
                return;
            }
            let targets_visible = has_signature && self.part_signature(&signature_bytes, NEXT, TEXT);
            if self.response_type == CONTENT {
                self.delta(r#"{"type":"text_delta","text":""}"#, "delta.text", &text);
                self.has_content = true;
            } else if !text.is_empty() {
                if self.response_type != NONE {
                    self.stop_event();
                    self.index += 1;
                }
                self.start_block(r#"{"type":"text","text":""}"#);
                self.delta(r#"{"type":"text_delta","text":""}"#, "delta.text", &text);
                self.response_type = CONTENT;
                self.has_content = true;
            }
            if !text.is_empty() {
                self.has_semantic = true;
                self.last_semantic_kind = TEXT;
                if targets_visible {
                    self.close_block();
                }
            }
            return;
        }
        if !call.exists() {
            return;
        }
        if model_group(&self.model) != "claude" {
            self.part_signature(&signature_bytes, NEXT, FUNCTION);
        }
        self.has_tool_use = true;
        let name = restore_sanitized_tool_name(Some(&self.names), &call.get("name").bytes());
        if self.response_type == FUNCTION_CALL {
            self.stop_event();
            self.index += 1;
            self.response_type = NONE;
        }
        if self.response_type != NONE {
            self.stop_event();
            self.index += 1;
        }
        let mut data = format!(
            r#"{{"type":"content_block_start","index":{},"content_block":{{"type":"tool_use","id":"","name":"","input":{{}}}}}}"#,
            self.index
        )
        .into_bytes();
        let n = TOOL_USE_IDS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let fallback = [&name[..], format!("-{}-{n}", now_nanos()).as_bytes()].concat();
        gj::set_str(
            &mut data,
            "content_block.id",
            tool_use_id(&self.model, &call, &fallback),
        );
        gj::set_str(&mut data, "content_block.name", &name);
        if model_group(&self.model) == "claude" && !signature_bytes.is_empty() {
            gj::set_str(
                &mut data,
                "content_block.signature",
                claude_signature_value(&self.model, &signature_bytes),
            );
        }
        self.event("content_block_start", &data);
        let args = call.get("args");
        if args.exists() {
            self.delta(
                r#"{"type":"input_json_delta","partial_json":""}"#,
                "delta.partial_json",
                &args.raw,
            );
        }
        self.response_type = FUNCTION_CALL;
        self.has_content = true;
        self.has_semantic = true;
        self.last_semantic_kind = FUNCTION;
    }

    /// appendFinalEvents.
    fn final_events(&mut self, force: bool) {
        if self.sent_final || (!self.has_usage && !force) || !self.has_content {
            return;
        }
        if self.response_type != NONE {
            self.stop_event();
            self.response_type = NONE;
        }
        let stop_reason = if self.has_tool_use {
            "tool_use"
        } else if self.finish_reason == b"MAX_TOKENS" {
            "max_tokens"
        } else {
            "end_turn"
        };
        let mut output = self.candidates_tokens.wrapping_add(self.thoughts_tokens);
        if output == 0 && self.total_tokens > 0 {
            output = self.total_tokens.wrapping_sub(self.prompt_tokens).max(0);
        }
        let mut delta = format!(
            r#"{{"type":"message_delta","delta":{{"stop_reason":"{stop_reason}","stop_sequence":null}},"usage":{{"input_tokens":{},"output_tokens":{output}}}}}"#,
            self.prompt_tokens
        )
        .into_bytes();
        if self.web_search_requests > 0 {
            gj::set_int(
                &mut delta,
                "usage.server_tool_use.web_search_requests",
                self.web_search_requests,
            );
        }
        if self.cached_tokens > 0 {
            gj::set_int(&mut delta, "usage.cache_read_input_tokens", self.cached_tokens);
        }
        self.event("message_delta", &delta);
        self.sent_final = true;
    }

    fn translate(&mut self, raw: &[u8]) -> Vec<Vec<u8>> {
        if raw == b"[DONE]" {
            if self.has_first_response && !self.has_content {
                self.start_block(r#"{"type":"text","text":""}"#);
                self.response_type = CONTENT;
                self.has_content = true;
            }
            if !self.has_content {
                return vec![];
            }
            self.final_events(true);
            self.event("message_stop", br#"{"type":"message_stop"}"#);
            return vec![std::mem::take(&mut self.out)];
        }
        let root = gj::parse(raw);
        if !self.has_first_response {
            let mut start = br#"{"type": "message_start", "message": {"id": "msg_1nZdL29xx5MUA1yADyHTEsnR8uuvGzszyY", "type": "message", "role": "assistant", "content": [], "model": "claude-3-5-sonnet-20241022", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 0, "output_tokens": 0}}}"#.to_vec();
            let prompt = gj::get(raw, "response.cpaUsageMetadata.promptTokenCount");
            if prompt.exists() {
                gj::set_int(&mut start, "message.usage.input_tokens", prompt.int());
            }
            let candidates = gj::get(raw, "response.cpaUsageMetadata.candidatesTokenCount");
            if candidates.exists() && !self.web_search_mode {
                gj::set_int(&mut start, "message.usage.output_tokens", candidates.int());
            }
            let version = gj::get(raw, "response.modelVersion");
            if version.exists() {
                gj::set_str(&mut start, "message.model", version.bytes());
            }
            let id = gj::get(raw, "response.responseId");
            if id.exists() {
                gj::set_str(&mut start, "message.id", id.bytes());
            }
            self.event("message_start", &start);
            self.has_first_response = true;
        }
        let mut handled_grounding = false;
        if self.web_search_mode && !self.has_web_search_tool {
            let gm = grounding_metadata(&root);
            if gm.exists() {
                let mut text = std::mem::take(&mut self.web_search_text);
                text.extend(text_content(&root));
                let mut out = std::mem::take(&mut self.out);
                self.index = web_search_stream_blocks(&mut out, self.index, &web_search_tool_use_id(), &text, &gm);
                self.out = out;
                self.has_web_search_tool = true;
                self.web_search_requests = 1;
                self.has_content = true;
                self.response_type = NONE;
                handled_grounding = true;
            }
        }
        let parts = gj::get(raw, "response.candidates.0.content.parts");
        if parts.is_array() && self.web_search_mode && !self.has_web_search_tool && !handled_grounding {
            for part in parts.array() {
                if part.get("thought").bool() || part.get("functionCall").exists() {
                    continue;
                }
                let text = part.get("text");
                if text.exists() {
                    self.web_search_text.extend_from_slice(&text.bytes());
                }
            }
        } else if parts.is_array() && !handled_grounding {
            for part in parts.array() {
                self.part(&part);
            }
        }
        let finish = gj::get(raw, "response.candidates.0.finishReason");
        if finish.exists() {
            self.has_finish_reason = true;
            self.finish_reason = finish.bytes().into_owned();
        }
        let usage = gj::get(raw, "response.usageMetadata");
        if usage.exists() {
            self.has_usage = true;
            self.cached_tokens = usage.get("cachedContentTokenCount").int();
            self.prompt_tokens = usage.get("promptTokenCount").int().wrapping_sub(self.cached_tokens);
            self.candidates_tokens = usage.get("candidatesTokenCount").int();
            self.thoughts_tokens = usage.get("thoughtsTokenCount").int();
            self.total_tokens = usage.get("totalTokenCount").int();
            if self.candidates_tokens == 0 && self.total_tokens > 0 {
                self.candidates_tokens = self
                    .total_tokens
                    .wrapping_sub(self.prompt_tokens)
                    .wrapping_sub(self.thoughts_tokens)
                    .max(0);
            }
        }
        if self.web_search_mode
            && !self.has_web_search_tool
            && self.has_finish_reason
            && !self.web_search_text.is_empty()
        {
            let text = std::mem::take(&mut self.web_search_text);
            self.start_block(r#"{"type":"text","text":""}"#);
            self.delta(r#"{"type":"text_delta","text":""}"#, "delta.text", &text);
            self.response_type = CONTENT;
            self.has_content = true;
        }
        if self.has_usage && self.has_finish_reason {
            self.final_events(false);
        }
        vec![std::mem::take(&mut self.out)]
    }
}

impl GoStream for Stream {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        Ok(self.translate(line))
    }
}

// ---------------------------------------------------------------------------------------
// Non-stream

/// ConvertAntigravityResponseToClaudeNonStream.
pub(crate) fn non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let names = disambiguated_tool_name_map(ctx.original_request);
    let model = model_name(ctx.translated_request);
    let root = gj::parse(body);
    let usage = |key: &str| root.get(&format!("response.usageMetadata.{key}")).int();
    let (prompt, candidates, thoughts, total, cached) = (
        usage("promptTokenCount"),
        usage("candidatesTokenCount"),
        usage("thoughtsTokenCount"),
        usage("totalTokenCount"),
        usage("cachedContentTokenCount"),
    );
    let mut output = candidates.wrapping_add(thoughts);
    if output == 0 && total > 0 {
        output = total.wrapping_sub(prompt).max(0);
    }
    let mut out = br#"{"id":"","type":"message","role":"assistant","model":"","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}"#.to_vec();
    gj::set_str(&mut out, "id", root.get("response.responseId").bytes());
    gj::set_str(&mut out, "model", root.get("response.modelVersion").bytes());
    gj::set_int(&mut out, "usage.input_tokens", prompt);
    gj::set_int(&mut out, "usage.output_tokens", output);
    if cached > 0 {
        gj::set_int(&mut out, "usage.cache_read_input_tokens", cached);
    }
    if translates_grounding(ctx.original_request, ctx.translated_request) {
        let gm = grounding_metadata(&root);
        if gm.exists() {
            let content = web_search_content(&web_search_tool_use_id(), &text_content(&root), &gm);
            gj::set_raw(&mut out, "content", content);
            gj::set_str(&mut out, "stop_reason", "end_turn");
            gj::set_int(&mut out, "usage.server_tool_use.web_search_requests", 1);
            return Ok(out);
        }
    }

    let mut acc = Blocks {
        model: &model,
        blocks: vec![],
        text: vec![],
        thinking: vec![],
        thinking_signature: vec![],
        direction: STANDALONE,
        target: TEXT,
    };
    let mut tool_ids = 0;
    let mut has_tool_call = false;
    let mut has_semantic = false;
    let mut last_kind = ANY;
    let claude_target = model_group(&model) == "claude";
    let parts = root.get("response.candidates.0.content.parts");
    if parts.is_array() {
        for part in parts.array() {
            let mut sig_res = part.get("thoughtSignature");
            if !sig_res.exists() {
                sig_res = part.get("thought_signature");
            }
            let signature = sig_res.bytes().into_owned();
            let call = part.get("functionCall");
            if call.exists() {
                let mut on_thought = false;
                if !claude_target
                    && !signature.is_empty()
                    && !acc.thinking.is_empty()
                    && acc.thinking_signature.is_empty()
                {
                    acc.thinking_signature = signature.clone();
                    acc.direction = NEXT;
                    acc.target = FUNCTION;
                    on_thought = true;
                }
                acc.flush_thinking();
                acc.flush_text();
                has_tool_call = true;
                let name = restore_sanitized_tool_name(Some(&names), &call.get("name").bytes());
                tool_ids += 1;
                if !claude_target && !signature.is_empty() && !on_thought {
                    acc.carrier(&signature, NEXT, FUNCTION);
                }
                let mut block = br#"{"type":"tool_use","id":"","name":"","input":{}}"#.to_vec();
                gj::set_str(
                    &mut block,
                    "id",
                    tool_use_id(&model, &call, format!("tool_{tool_ids}").as_bytes()),
                );
                gj::set_str(&mut block, "name", &name);
                if claude_target && !signature.is_empty() {
                    gj::set_str(&mut block, "signature", claude_signature_value(&model, &signature));
                }
                let args = call.get("args");
                if args.exists() && !args.raw.is_empty() && gj::valid(&args.raw) && args.is_object() {
                    gj::set_raw(&mut block, "input", &args.raw);
                }
                acc.blocks.push(block);
                has_semantic = true;
                last_kind = FUNCTION;
                continue;
            }
            let text = part.get("text").bytes().into_owned();
            if part.get("thought").bool() {
                acc.flush_text();
                if !acc.thinking_signature.is_empty() {
                    acc.flush_thinking();
                }
                if !text.is_empty() {
                    acc.thinking.extend_from_slice(&text);
                    has_semantic = true;
                    last_kind = TEXT;
                }
                if !signature.is_empty() {
                    if !acc.thinking.is_empty() {
                        acc.thinking_signature = signature;
                        acc.direction = STANDALONE;
                        acc.target = TEXT;
                        acc.flush_thinking();
                    } else if has_semantic && last_kind == TEXT {
                        cache_signature(&model, b"", &signature);
                    } else if has_semantic {
                        acc.carrier(&signature, PREVIOUS, last_kind);
                    } else {
                        acc.carrier(&signature, NEXT, ANY);
                    }
                }
                continue;
            }
            let mut visible_carrier = false;
            if !signature.is_empty() {
                if !acc.thinking.is_empty() && acc.thinking_signature.is_empty() {
                    acc.thinking_signature = signature;
                    acc.direction = NEXT;
                    acc.target = TEXT;
                    acc.flush_thinking();
                } else {
                    acc.flush_thinking();
                    acc.flush_text();
                    if !text.is_empty() {
                        acc.carrier(&signature, NEXT, TEXT);
                        visible_carrier = true;
                    } else if has_semantic && last_kind == TEXT {
                        cache_signature(&model, b"", &signature);
                    } else if has_semantic {
                        acc.carrier(&signature, PREVIOUS, last_kind);
                    } else {
                        acc.carrier(&signature, NEXT, ANY);
                    }
                }
            }
            if !text.is_empty() {
                acc.flush_thinking();
                acc.text.extend_from_slice(&text);
                has_semantic = true;
                last_kind = TEXT;
                if visible_carrier {
                    acc.flush_text();
                }
            }
        }
    }
    acc.flush_thinking();
    acc.flush_text();
    if !acc.blocks.is_empty() {
        gj::set_raw(&mut out, "content", gj::join(&acc.blocks));
    }
    let stop_reason = if has_tool_call {
        "tool_use"
    } else if root.get("response.candidates.0.finishReason").bytes().as_ref() == b"MAX_TOKENS" {
        "max_tokens"
    } else {
        "end_turn"
    };
    gj::set_str(&mut out, "stop_reason", stop_reason);
    if prompt == 0 && output == 0 && !root.get("response.usageMetadata").exists() {
        gj::delete(&mut out, "usage");
    }
    Ok(out)
}

/// The non-stream content blocks being assembled.
struct Blocks<'a> {
    model: &'a str,
    blocks: Vec<Vec<u8>>,
    text: Vec<u8>,
    thinking: Vec<u8>,
    thinking_signature: Vec<u8>,
    direction: &'static str,
    target: &'static str,
}

impl Blocks<'_> {
    fn flush_text(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let mut block = br#"{"type":"text","text":""}"#.to_vec();
        gj::set_str(&mut block, "text", std::mem::take(&mut self.text));
        self.blocks.push(block);
    }

    fn flush_thinking(&mut self) {
        if self.thinking.is_empty() && self.thinking_signature.is_empty() {
            return;
        }
        let mut block = br#"{"type":"thinking","thinking":""}"#.to_vec();
        gj::set_str(&mut block, "thinking", std::mem::take(&mut self.thinking));
        let signature = std::mem::take(&mut self.thinking_signature);
        if !signature.is_empty() {
            gj::set_str(
                &mut block,
                "signature",
                carrier_value(self.model, &signature, self.direction, self.target),
            );
        }
        self.blocks.push(block);
        self.direction = STANDALONE;
        self.target = TEXT;
    }

    /// An empty thinking block carrying a signature.
    fn carrier(&mut self, signature: &[u8], direction: &str, target: &str) {
        if signature.is_empty() {
            return;
        }
        let mut carrier = br#"{"type":"thinking","thinking":"","signature":""}"#.to_vec();
        gj::set_str(
            &mut carrier,
            "signature",
            carrier_value(self.model, signature, direction, target),
        );
        self.blocks.push(carrier);
    }
}
