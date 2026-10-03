//! Gemini -> OpenAI Responses responses
//! (internal/translator/gemini/openai/responses/gemini_openai-responses_response.go).
//!
//! Gemini sends complete parts; the stream state turns them into Responses output items
//! (reasoning, message, function/custom tool call, web search call) and keeps thought
//! signatures as `encrypted_content`, detached carriers, or cached trailing signatures
//! (see `gemini_responses`). Calls to a declared `apply_patch` custom tool go through the
//! patch input decoder; an invalid or conflicting call ends the response with
//! `response.failed` (Go's ToolInputError contract).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use cpa_common::json::{self as gj, Res};

use crate::apply_patch::{self, CallState};
use crate::claude_responses_response::{Echo, copy_request_fields, pick_request};
use crate::common::{
    go_runes, now_nanos, now_unix, parse_rfc3339_unix, request_model_name, restore_sanitized_tool_name,
    sanitized_tool_name_map, sse_event, trim_space,
};
use crate::gemini_responses::{
    ANY, BYPASS_SIGNATURE, FUNCTION, NEXT, PREVIOUS, STANDALONE, TEXT, cache_text_signatures, encode_carrier,
};
use crate::gemini_web_search::{self as ws, PartMapping};
use crate::responses_tools::{Identity, reverse_identity_map, unwrap_custom_tool_input};
use crate::stream::GoStream;
use crate::{Error, ResponseCtx};

pub static PAIR: crate::Registered = registered!(
    OpenAIResponse -> Gemini,
    request: |ctx, body| Ok(crate::gemini_responses::convert(ctx.model, body)),
    non_stream: non_stream,
    go_stream: go_stream,
    token_count: None,
);

static RESPONSE_IDS: AtomicU64 = AtomicU64::new(0);
static FUNCTION_CALL_IDS: AtomicU64 = AtomicU64::new(0);

/// unwrapRequestRoot: a `request` wrapper holding the Responses request.
fn unwrap_request_root(root: Res<'_>) -> Res<'_> {
    let req = root.get("request");
    if req.exists() && (req.get("model").exists() || req.get("input").exists() || req.get("instructions").exists()) {
        req
    } else {
        root
    }
}

/// unwrapGeminiResponseRoot: Vertex-style `response` wrappers.
fn unwrap_response_root(root: Res<'_>) -> Res<'_> {
    let resp = root.get("response");
    if resp.exists()
        && (resp.get("candidates").exists() || resp.get("responseId").exists() || resp.get("usageMetadata").exists())
    {
        resp
    } else {
        root
    }
}

fn has_effective_google_search(raw: &[u8]) -> bool {
    if raw.is_empty() {
        return false;
    }
    if gj::get(raw, "requestType").bytes().as_ref() == b"web_search" {
        return true;
    }
    ["request.tools", "tools"].iter().any(|path| {
        let tools = gj::get(raw, *path);
        tools.is_array() && tools.array().iter().any(|t| t.get("googleSearch").exists())
    })
}

fn is_upstream_gemini_request(raw: &[u8]) -> bool {
    !raw.is_empty()
        && (gj::get(raw, "requestType").exists()
            || gj::get(raw, "contents").exists()
            || gj::get(raw, "request.contents").exists())
}

/// determineWebSearchStreamMode: whether visible text waits for the web search item.
fn web_search_stream_mode(model: &str, request_model: &[u8], original: &[u8], request: &[u8]) -> bool {
    if !original.is_empty() && !ws::allows_web_search_tool_choice(&unwrap_request_root(gj::parse(original))) {
        return false;
    }
    if !request.is_empty() {
        let root = unwrap_request_root(gj::parse(request));
        if root.get("tool_choice").exists() && !ws::allows_web_search_tool_choice(&root) {
            return false;
        }
        if is_upstream_gemini_request(request) || has_effective_google_search(request) {
            return has_effective_google_search(request);
        }
    }
    let picked = pick_request(original, request);
    if picked.is_empty() {
        return false;
    }
    let root = unwrap_request_root(gj::parse(picked));
    ws::has_web_search_tool(&root)
        && ws::allows_web_search_tool_choice(&root)
        && (ws::model_supports_web_search(model)
            || ws::model_supports_web_search(&String::from_utf8_lossy(request_model)))
}

/// common.SetResponsesToolCallIdentity.
fn set_identity(item: &mut Vec<u8>, name: &[u8], namespace: &[u8], path: &str) {
    let at = |key: &str| {
        if path.is_empty() {
            key.to_owned()
        } else {
            format!("{path}.{key}")
        }
    };
    gj::set_str(item, &at("name"), name);
    if namespace.is_empty() {
        gj::delete(item, &at("namespace"));
    } else {
        gj::set_str(item, &at("namespace"), namespace);
    }
}

fn event(name: &str, payload: &[u8]) -> Vec<u8> {
    sse_event(name, payload)
}

fn rune_count(text: &[u8]) -> i64 {
    go_runes(text).count() as i64
}

// ---------------------------------------------------------------------------------------
// Function call evidence (geminiRecordFunctionEvidence)

/// What the stream has seen of one function call across repeated snapshots.
#[derive(Default)]
struct Evidence {
    part_index: i64,
    has_part_index: bool,
    apply_patch: bool,
    raw_name: Vec<u8>,
    upstream_id: Vec<u8>,
    input: String,
    has_input: bool,
    err: Option<String>,
    patch_call: Option<CallState>,
}

impl Evidence {
    fn record_error(&mut self, err: impl Into<String>) {
        if self.err.is_none() {
            self.err = Some(err.into());
        }
    }
}

/// Evidence keyed by explicit part index, upstream ID or (for nameless unkeyed snapshots)
/// arrival; several keys may share one call.
#[derive(Default)]
struct EvidenceLog {
    items: Vec<Evidence>,
    keys: HashMap<Vec<u8>, usize>,
}

impl EvidenceLog {
    /// geminiRecordFunctionEvidence: full snapshots are evidence, never source prefixes.
    fn record(&mut self, identities: &HashMap<Vec<u8>, Identity>, fc: &Res<'_>, part_index: i64, valid: bool) -> usize {
        let name = fc.get("name").bytes().into_owned();
        let id = fc.get("id").bytes().into_owned();
        let mut keys: Vec<Vec<u8>> = vec![];
        if part_index >= 0 {
            keys.push(format!("part:{part_index}").into_bytes());
        }
        if !id.is_empty() {
            keys.push([&b"id:"[..], &id].concat());
        }
        if keys.is_empty() && name.is_empty() {
            keys.push(format!("unknown:{}", self.keys.len()).into_bytes());
        }
        let is_patch = |n: &[u8]| identities.get(n).is_some_and(|i| i.apply_patch);
        let mut patch_related = is_patch(&name);
        let mut found: Option<usize> = None;
        let mut conflict = false;
        for key in &keys {
            if let Some(&prior) = self.keys.get(key) {
                patch_related |= self.items[prior].apply_patch;
                match found {
                    None => found = Some(prior),
                    Some(current) if current != prior => conflict = true,
                    _ => {}
                }
            }
        }
        const CONFLICT: &str = "conflicting apply_patch call indexes";
        if conflict {
            if patch_related {
                // Reject before rebinding either call's aliases or provenance.
                self.items.push(Evidence {
                    apply_patch: true,
                    err: Some(CONFLICT.into()),
                    ..Default::default()
                });
                return self.items.len() - 1;
            }
            if let Some(found) = found {
                self.items[found].err = Some(CONFLICT.into());
            }
        }
        let index = found.unwrap_or_else(|| {
            self.items.push(Evidence::default());
            self.items.len() - 1
        });
        let evidence = &mut self.items[index];
        if part_index >= 0 {
            if evidence.has_part_index && evidence.part_index != part_index {
                evidence.record_error("conflicting apply_patch part index");
            } else {
                evidence.part_index = part_index;
                evidence.has_part_index = true;
            }
        }
        if is_patch(&name) {
            evidence.apply_patch = true;
        }
        if !name.is_empty() {
            if !evidence.raw_name.is_empty() && evidence.raw_name != name {
                evidence.record_error("conflicting apply_patch call name");
            } else {
                evidence.raw_name = name;
            }
        }
        if !id.is_empty() {
            if !evidence.upstream_id.is_empty() && evidence.upstream_id != id {
                evidence.record_error("conflicting apply_patch call ID");
            } else {
                evidence.upstream_id = id;
            }
        }
        let snapshot = CallState::default().finish_arguments(&fc.get("args").raw);
        if !valid {
            evidence.record_error("invalid Gemini apply_patch response JSON");
        }
        match snapshot {
            Err(err) => evidence.record_error(err),
            Ok((_, input)) => {
                if evidence.has_input && evidence.input != input {
                    evidence.record_error("conflicting apply_patch complete snapshots");
                }
                evidence.has_input = true;
                evidence.input = input;
            }
        }
        for key in keys {
            self.keys.insert(key, index);
        }
        index
    }

    /// geminiPendingIdentityError: with apply_patch declared, a call that never got a name.
    fn pending_identity_error(&self, identities: &HashMap<Vec<u8>, Identity>) -> Option<String> {
        if !identities.values().any(|i| i.apply_patch) {
            return None;
        }
        self.keys
            .values()
            .map(|&i| &self.items[i])
            .find(|e| e.raw_name.is_empty())
            .map(|e| {
                e.err
                    .clone()
                    .unwrap_or_else(|| "unresolved Gemini apply_patch call identity".into())
            })
    }
}

/// The identity of an upstream call: the declared tool, else the sanitized-name reversal.
fn resolve_identity(
    identities: &HashMap<Vec<u8>, Identity>,
    sanitized: Option<&HashMap<Vec<u8>, Vec<u8>>>,
    raw_name: &[u8],
) -> Identity {
    identities.get(raw_name).cloned().unwrap_or_else(|| Identity {
        name: restore_sanitized_tool_name(sanitized, raw_name),
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------------------
// Stream (ConvertGeminiResponseToOpenAIResponses)

struct CompletedMessage {
    id: Vec<u8>,
    text: Vec<u8>,
    annotations: Vec<Vec<u8>>,
}

struct CompletedReasoning {
    id: Vec<u8>,
    signature: Vec<u8>,
    text: Vec<u8>,
}

#[derive(Default)]
struct State {
    model: String,
    original: Vec<u8>,
    translated: Vec<u8>,
    request: Vec<u8>,
    tool_error: Option<String>,
    seq: i64,
    response_id: Vec<u8>,
    created_at: i64,
    started: bool,
    completed: bool,

    msg_opened: bool,
    msg_closed: bool,
    msg_index: i64,
    current_msg_id: Vec<u8>,
    item_text: Vec<u8>,

    reasoning_opened: bool,
    reasoning_index: i64,
    reasoning_item_id: Vec<u8>,
    reasoning_enc: Vec<u8>,
    reasoning_direction: &'static str,
    reasoning_target: &'static str,
    reasoning_buf: Vec<u8>,
    reasoning_pending_deltas: Vec<Vec<u8>>,
    reasoning_closed: bool,
    pending_reasoning_signature: Vec<u8>,
    detached_reasoning: HashMap<i64, (Vec<u8>, Vec<u8>)>,
    completed_messages: HashMap<i64, CompletedMessage>,
    completed_reasoning: HashMap<i64, CompletedReasoning>,
    seen_reasoning_signatures: HashSet<Vec<u8>>,
    last_semantic_kind: &'static str,
    hidden_text_signatures: HashMap<Vec<u8>, Vec<Vec<u8>>>,

    next_index: i64,
    func_args: BTreeMap<i64, Vec<u8>>,
    func_input: HashMap<i64, Vec<u8>>,
    func_custom: HashMap<i64, bool>,
    func_names: HashMap<i64, Vec<u8>>,
    func_namespaces: HashMap<i64, Vec<u8>>,
    func_call_ids: HashMap<i64, Vec<u8>>,
    func_done: HashSet<i64>,
    sanitized_names: Option<HashMap<Vec<u8>, Vec<u8>>>,
    identities: HashMap<Vec<u8>, Identity>,
    evidence: EvidenceLog,

    web_search_stream_mode: bool,
    ws_opened: bool,
    ws_done: bool,
    ws_index: i64,
    ws_item_id: Vec<u8>,
    ws_query: Vec<u8>,
    ws_queries: Vec<Vec<u8>>,
    ws_sources: Vec<Vec<u8>>,
    ws_buffered_deltas: Vec<Vec<u8>>,
    ws_buffered_parts: Vec<(i64, Vec<u8>)>,
    grounding: Option<Vec<u8>>,
    part_mappings: Vec<PartMapping>,
    current_logical_part_index: i64,
    current_part_kind: &'static str,
    has_seen_first_part: bool,
    text_part_run_active: bool,
    current_msg_rune_offset: i64,
    emitted_annotation_count: HashMap<i64, usize>,
}

pub fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    let request = pick_request(ctx.original_request, ctx.translated_request).to_vec();
    Box::new(State {
        model: ctx.model.to_owned(),
        original: ctx.original_request.to_vec(),
        translated: ctx.translated_request.to_vec(),
        sanitized_names: sanitized_tool_name_map(ctx.original_request),
        identities: reverse_identity_map(&request),
        request,
        last_semantic_kind: "",
        current_part_kind: "",
        reasoning_direction: "",
        reasoning_target: "",
        ..Default::default()
    })
}

fn grounding_exists(grounding: &Option<Vec<u8>>) -> bool {
    grounding.as_deref().is_some_and(|g| gj::parse(g).exists())
}

impl State {
    fn next_seq(&mut self) -> i64 {
        self.seq += 1;
        self.seq
    }

    fn reasoning_encrypted_content(&self) -> Vec<u8> {
        if self.reasoning_enc.is_empty() || self.reasoning_direction.is_empty() {
            return self.reasoning_enc.clone();
        }
        encode_carrier(&self.reasoning_enc, self.reasoning_direction, self.reasoning_target)
    }

    fn fill_web_search_query(&mut self) {
        if self.ws_query.is_empty()
            && let Some(first) = self.ws_queries.first()
        {
            self.ws_query = first.clone();
        }
        if self.ws_query.is_empty() && !self.request.is_empty() {
            self.ws_query = ws::extract_query(&unwrap_request_root(gj::parse(&self.request)));
        }
    }

    fn failed(&mut self, err: String, out: &mut Vec<Vec<u8>>) {
        self.tool_error.get_or_insert(err);
        self.completed = true;
        let seq = self.next_seq();
        out.push(event("response.failed", &apply_patch::failure(&self.response_id, seq)));
    }

    fn finalize_web_search(&mut self, out: &mut Vec<Vec<u8>>) {
        if !self.ws_opened || self.ws_done {
            return;
        }
        self.fill_web_search_query();
        let mut completed =
            br#"{"type":"response.web_search_call.completed","sequence_number":0,"output_index":0,"item_id":""}"#
                .to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut completed, "sequence_number", seq);
        gj::set_int(&mut completed, "output_index", self.ws_index);
        gj::set_str(&mut completed, "item_id", &self.ws_item_id);
        out.push(event("response.web_search_call.completed", &completed));
        let item = ws::web_search_call_item(&self.ws_item_id, &self.ws_query, &self.ws_queries, &self.ws_sources);
        let mut done = br#"{"type":"response.output_item.done","sequence_number":0,"output_index":0}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut done, "sequence_number", seq);
        gj::set_int(&mut done, "output_index", self.ws_index);
        gj::set_raw(&mut done, "item", &item);
        out.push(event("response.output_item.done", &done));
        self.ws_done = true;
    }

    fn reasoning_event(&mut self, template: &[u8]) -> Vec<u8> {
        let mut payload = template.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut payload, "sequence_number", seq);
        gj::set_str(&mut payload, "item_id", &self.reasoning_item_id);
        gj::set_int(&mut payload, "output_index", self.reasoning_index);
        payload
    }

    fn open_reasoning(&mut self, out: &mut Vec<Vec<u8>>) {
        if self.reasoning_opened
            || self.reasoning_closed
            || (self.reasoning_buf.is_empty() && self.reasoning_enc.is_empty())
        {
            return;
        }
        self.finalize_web_search(out);
        self.reasoning_opened = true;
        self.reasoning_index = self.next_index;
        self.next_index += 1;
        self.reasoning_item_id = [
            b"rs_",
            &self.response_id[..],
            format!("_{}", self.reasoning_index).as_bytes(),
        ]
        .concat();
        let mut item = br#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"reasoning","status":"in_progress","encrypted_content":"","summary":[]}}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut item, "sequence_number", seq);
        gj::set_int(&mut item, "output_index", self.reasoning_index);
        gj::set_str(&mut item, "item.id", &self.reasoning_item_id);
        gj::set_str(&mut item, "item.encrypted_content", self.reasoning_encrypted_content());
        out.push(event("response.output_item.added", &item));
        let added = self.reasoning_event(br#"{"type":"response.reasoning_summary_part.added","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}"#);
        out.push(event("response.reasoning_summary_part.added", &added));
        for delta in std::mem::take(&mut self.reasoning_pending_deltas) {
            let mut msg = self.reasoning_event(br#"{"type":"response.reasoning_summary_text.delta","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"delta":""}"#);
            gj::set_str(&mut msg, "delta", &delta);
            out.push(event("response.reasoning_summary_text.delta", &msg));
        }
    }

    fn finalize_reasoning(&mut self, out: &mut Vec<Vec<u8>>) {
        self.open_reasoning(out);
        if !self.reasoning_opened || self.reasoning_closed {
            return;
        }
        let full = self.reasoning_buf.clone();
        let mut text_done = self.reasoning_event(br#"{"type":"response.reasoning_summary_text.done","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"text":""}"#);
        gj::set_str(&mut text_done, "text", &full);
        out.push(event("response.reasoning_summary_text.done", &text_done));
        let mut part_done = self.reasoning_event(br#"{"type":"response.reasoning_summary_part.done","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}"#);
        gj::set_str(&mut part_done, "part.text", &full);
        out.push(event("response.reasoning_summary_part.done", &part_done));
        let mut item_done = br#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"reasoning","encrypted_content":"","summary":[{"type":"summary_text","text":""}]}}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut item_done, "sequence_number", seq);
        gj::set_str(&mut item_done, "item.id", &self.reasoning_item_id);
        gj::set_int(&mut item_done, "output_index", self.reasoning_index);
        let encrypted = self.reasoning_encrypted_content();
        gj::set_str(&mut item_done, "item.encrypted_content", &encrypted);
        gj::set_str(&mut item_done, "item.summary.0.text", &full);
        out.push(event("response.output_item.done", &item_done));
        self.completed_reasoning.insert(
            self.reasoning_index,
            CompletedReasoning {
                id: self.reasoning_item_id.clone(),
                signature: encrypted,
                text: full,
            },
        );
        self.reasoning_closed = true;
    }

    fn reset_reasoning(&mut self) {
        self.reasoning_opened = false;
        self.reasoning_closed = false;
        self.reasoning_index = 0;
        self.reasoning_item_id.clear();
        self.reasoning_enc.clear();
        self.reasoning_direction = "";
        self.reasoning_target = "";
        self.reasoning_buf.clear();
        self.reasoning_pending_deltas.clear();
    }

    fn open_web_search(&mut self, out: &mut Vec<Vec<u8>>) {
        if self.ws_opened {
            return;
        }
        self.finalize_reasoning(out);
        self.ws_opened = true;
        self.ws_index = self.next_index;
        self.next_index += 1;
        let id = self.response_id.strip_prefix(b"resp_").unwrap_or(&self.response_id);
        self.ws_item_id = [b"ws_", id].concat();
        self.fill_web_search_query();
        let mut added = br#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"web_search_call","status":"in_progress","action":{"type":"search","query":""}}}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut added, "sequence_number", seq);
        gj::set_int(&mut added, "output_index", self.ws_index);
        gj::set_str(&mut added, "item.id", &self.ws_item_id);
        gj::set_str(&mut added, "item.action.query", &self.ws_query);
        out.push(event("response.output_item.added", &added));
        let mut searching =
            br#"{"type":"response.web_search_call.searching","sequence_number":0,"output_index":0,"item_id":""}"#
                .to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut searching, "sequence_number", seq);
        gj::set_int(&mut searching, "output_index", self.ws_index);
        gj::set_str(&mut searching, "item_id", &self.ws_item_id);
        out.push(event("response.web_search_call.searching", &searching));
    }

    fn open_message(&mut self, out: &mut Vec<Vec<u8>>) {
        self.msg_opened = true;
        self.msg_index = self.next_index;
        self.next_index += 1;
        self.current_msg_id = [
            b"msg_",
            &self.response_id[..],
            format!("_{}", self.msg_index).as_bytes(),
        ]
        .concat();
        let mut item = br#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"message","status":"in_progress","content":[],"role":"assistant"}}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut item, "sequence_number", seq);
        gj::set_int(&mut item, "output_index", self.msg_index);
        gj::set_str(&mut item, "item.id", &self.current_msg_id);
        out.push(event("response.output_item.added", &item));
        let mut added = br#"{"type":"response.content_part.added","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut added, "sequence_number", seq);
        gj::set_str(&mut added, "item_id", &self.current_msg_id);
        gj::set_int(&mut added, "output_index", self.msg_index);
        out.push(event("response.content_part.added", &added));
        self.item_text.clear();
        self.current_msg_rune_offset = 0;
    }

    fn reopen_closed_message(&mut self) {
        if self.msg_closed {
            self.msg_opened = false;
            self.msg_closed = false;
            self.item_text.clear();
            self.current_msg_rune_offset = 0;
        }
    }

    fn text_delta(&mut self, delta: &[u8], out: &mut Vec<Vec<u8>>) {
        let mut msg = br#"{"type":"response.output_text.delta","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"delta":"","logprobs":[]}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut msg, "sequence_number", seq);
        gj::set_str(&mut msg, "item_id", &self.current_msg_id);
        gj::set_int(&mut msg, "output_index", self.msg_index);
        gj::set_str(&mut msg, "delta", delta);
        out.push(event("response.output_text.delta", &msg));
    }

    fn map_part(&mut self, part_index: i64, text: &[u8]) {
        match self.part_mappings.last_mut() {
            Some(last) if last.part_index == part_index && last.message_index == self.msg_index => {
                last.text.extend_from_slice(text)
            }
            _ => self.part_mappings.push(PartMapping {
                part_index,
                message_index: self.msg_index,
                start_rune: self.current_msg_rune_offset,
                text: text.to_vec(),
            }),
        }
        self.current_msg_rune_offset += rune_count(text);
    }

    fn flush_web_search_buffered_text(&mut self, out: &mut Vec<Vec<u8>>) {
        self.finalize_web_search(out);
        if self.ws_buffered_deltas.is_empty() {
            return;
        }
        self.reopen_closed_message();
        if !self.msg_opened {
            self.open_message(out);
        }
        for delta in std::mem::take(&mut self.ws_buffered_deltas) {
            self.item_text.extend_from_slice(&delta);
            self.text_delta(&delta, out);
        }
        for (part_index, text) in std::mem::take(&mut self.ws_buffered_parts) {
            self.map_part(part_index, &text);
        }
    }

    fn emit_new_citations(&mut self, msg_index: i64, item_id: &[u8], annotations: &[Vec<u8>], out: &mut Vec<Vec<u8>>) {
        let emitted = self.emitted_annotation_count.get(&msg_index).copied().unwrap_or(0);
        for (index, annotation) in annotations.iter().enumerate().skip(emitted) {
            let mut payload = br#"{"type":"response.output_text.annotation.added","sequence_number":0,"response_id":"","item_id":"","output_index":0,"content_index":0,"annotation_index":0}"#.to_vec();
            let seq = self.next_seq();
            gj::set_int(&mut payload, "sequence_number", seq);
            gj::set_str(&mut payload, "response_id", &self.response_id);
            gj::set_str(&mut payload, "item_id", item_id);
            gj::set_int(&mut payload, "output_index", msg_index);
            gj::set_int(&mut payload, "content_index", 0);
            gj::set_int(&mut payload, "annotation_index", index as i64);
            gj::set_raw(&mut payload, "annotation", annotation);
            out.push(event("response.output_text.annotation.added", &payload));
        }
        if annotations.len() > emitted {
            self.emitted_annotation_count.insert(msg_index, annotations.len());
        }
    }

    fn finalize_message(&mut self, out: &mut Vec<Vec<u8>>) {
        self.finalize_web_search(out);
        if !self.ws_buffered_deltas.is_empty() {
            self.flush_web_search_buffered_text(out);
        }
        if !self.msg_opened || self.msg_closed {
            return;
        }
        let full = self.item_text.clone();
        let mut citations: Vec<Vec<u8>> = vec![];
        if grounding_exists(&self.grounding) {
            let grounding = self.grounding.clone().unwrap_or_default();
            let map = ws::url_citations(&gj::parse(&grounding), &self.part_mappings, std::slice::from_ref(&full))
                .unwrap_or_default();
            citations = map.get(&self.msg_index).cloned().unwrap_or_default();
            if citations.is_empty() && self.completed_messages.is_empty() {
                citations = map.get(&0).cloned().unwrap_or_default();
            }
        }
        let id = self.current_msg_id.clone();
        self.emit_new_citations(self.msg_index, &id, &citations, out);
        let mut done = br#"{"type":"response.output_text.done","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"text":"","logprobs":[]}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut done, "sequence_number", seq);
        gj::set_str(&mut done, "item_id", &id);
        gj::set_int(&mut done, "output_index", self.msg_index);
        gj::set_str(&mut done, "text", &full);
        out.push(event("response.output_text.done", &done));
        let mut part_done = br#"{"type":"response.content_part.done","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut part_done, "sequence_number", seq);
        gj::set_str(&mut part_done, "item_id", &id);
        gj::set_int(&mut part_done, "output_index", self.msg_index);
        gj::set_str(&mut part_done, "part.text", &full);
        if !citations.is_empty() {
            gj::set_raw(&mut part_done, "part.annotations", gj::join(&citations));
        }
        out.push(event("response.content_part.done", &part_done));
        let mut item_done = br#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":""}],"role":"assistant"}}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut item_done, "sequence_number", seq);
        gj::set_int(&mut item_done, "output_index", self.msg_index);
        gj::set_str(&mut item_done, "item.id", &id);
        gj::set_str(&mut item_done, "item.content.0.text", &full);
        if !citations.is_empty() {
            gj::set_raw(&mut item_done, "item.content.0.annotations", gj::join(&citations));
        }
        out.push(event("response.output_item.done", &item_done));
        self.completed_messages.insert(
            self.msg_index,
            CompletedMessage {
                id,
                text: full,
                annotations: citations,
            },
        );
        self.msg_closed = true;
        self.current_msg_rune_offset = 0;
    }

    fn emit_late_citations(&mut self, out: &mut Vec<Vec<u8>>) {
        if !grounding_exists(&self.grounding) || self.completed_messages.is_empty() {
            return;
        }
        let texts: Vec<Vec<u8>> = (0..self.next_index)
            .filter_map(|i| self.completed_messages.get(&i).map(|m| m.text.clone()))
            .collect();
        let grounding = self.grounding.clone().unwrap_or_default();
        let Some(late) = ws::url_citations(&gj::parse(&grounding), &self.part_mappings, &texts) else {
            return;
        };
        for index in 0..self.next_index {
            let Some(message) = self.completed_messages.get(&index) else {
                continue;
            };
            let mut cites = late.get(&index).cloned().unwrap_or_default();
            if cites.is_empty() && self.completed_messages.len() == 1 {
                cites = late.get(&0).cloned().unwrap_or_default();
            }
            let annotations = ws::merge_citations(&message.annotations, &cites);
            let id = message.id.clone();
            self.emit_new_citations(index, &id, &annotations, out);
            if !annotations.is_empty()
                && let Some(message) = self.completed_messages.get_mut(&index)
            {
                message.annotations = annotations;
            }
        }
    }

    fn emit_detached_reasoning(&mut self, signature: &[u8], direction: &str, target: &str, out: &mut Vec<Vec<u8>>) {
        let signature = trim_space(signature).to_vec();
        if signature.is_empty() || self.seen_reasoning_signatures.contains(&signature) {
            return;
        }
        self.finalize_reasoning(out);
        self.finalize_message(out);
        let index = self.next_index;
        self.next_index += 1;
        let placement = if direction == PREVIOUS { "after" } else { "before" };
        let item_id = [
            b"rs_",
            &self.response_id[..],
            format!("_detached_{placement}_{index}").as_bytes(),
        ]
        .concat();
        let carrier = encode_carrier(&signature, direction, target);
        for (name, template) in [
            (
                "response.output_item.added",
                &br#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"reasoning","status":"in_progress","encrypted_content":"","summary":[]}}"#[..],
            ),
            (
                "response.output_item.done",
                br#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"reasoning","encrypted_content":"","summary":[]}}"#,
            ),
        ] {
            let mut payload = template.to_vec();
            let seq = self.next_seq();
            gj::set_int(&mut payload, "sequence_number", seq);
            gj::set_int(&mut payload, "output_index", index);
            gj::set_str(&mut payload, "item.id", &item_id);
            gj::set_str(&mut payload, "item.encrypted_content", &carrier);
            out.push(event(name, &payload));
        }
        self.detached_reasoning.insert(index, (item_id, carrier));
        self.seen_reasoning_signatures.insert(signature);
    }

    /// emitTrailingDetachedReasoning: a signature after the content it signs.
    fn emit_trailing_detached_reasoning(&mut self, signature: &[u8], out: &mut Vec<Vec<u8>>) {
        match self.last_semantic_kind {
            TEXT => {
                let signature = trim_space(signature).to_vec();
                if signature.is_empty() || self.seen_reasoning_signatures.contains(&signature) {
                    return;
                }
                self.finalize_reasoning(out);
                self.finalize_message(out);
                // Never bind a later thought signature to a message from before that thought.
                if !self.msg_opened || (self.reasoning_opened && self.reasoning_index > self.msg_index) {
                    self.emit_detached_reasoning(&signature, PREVIOUS, TEXT, out);
                    return;
                }
                let signatures = self
                    .hidden_text_signatures
                    .entry(self.current_msg_id.clone())
                    .or_default();
                signatures.push(signature.clone());
                let signatures = signatures.clone();
                if cache_text_signatures(&self.model, &self.current_msg_id, &self.item_text, &signatures) {
                    self.seen_reasoning_signatures.insert(signature);
                    return;
                }
                self.emit_detached_reasoning(&signature, PREVIOUS, TEXT, out);
            }
            FUNCTION => self.emit_detached_reasoning(signature, PREVIOUS, FUNCTION, out),
            _ => self.emit_detached_reasoning(signature, STANDALONE, ANY, out),
        }
    }

    fn start(&mut self, root: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        self.response_id = root.get("responseId").bytes().into_owned();
        if self.response_id.is_empty() {
            self.response_id = format!(
                "resp_{:x}_{}",
                now_nanos(),
                RESPONSE_IDS.fetch_add(1, Ordering::Relaxed) + 1
            )
            .into_bytes();
        }
        if !self.response_id.starts_with(b"resp_") {
            self.response_id = [&b"resp_"[..], &self.response_id].concat();
        }
        let create_time = root.get("createTime");
        if create_time.exists()
            && let Some(t) = parse_rfc3339_unix(&create_time.bytes())
        {
            self.created_at = t;
        }
        if self.created_at == 0 {
            self.created_at = now_unix();
        }
        let mut request_model = request_model_name(&self.original, &self.translated);
        if request_model.is_empty() {
            request_model = self.model.as_bytes().to_vec();
        }
        for (name, template) in [
            (
                "response.created",
                &br#"{"type":"response.created","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"in_progress","background":false,"error":null,"output":[]}}"#[..],
            ),
            (
                "response.in_progress",
                br#"{"type":"response.in_progress","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"in_progress","output":[]}}"#,
            ),
        ] {
            let mut payload = template.to_vec();
            let seq = self.next_seq();
            gj::set_int(&mut payload, "sequence_number", seq);
            gj::set_str(&mut payload, "response.id", &self.response_id);
            gj::set_int(&mut payload, "response.created_at", self.created_at);
            if !request_model.is_empty() {
                gj::set_str(&mut payload, "response.model", &request_model);
            }
            out.push(event(name, &payload));
        }
        self.started = true;
        self.next_index = 0;
        self.web_search_stream_mode =
            web_search_stream_mode(&self.model, &request_model, &self.original, &self.translated);
    }

    fn grounding_frame(&mut self, gm: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        self.grounding = ws::merge_grounding_metadata(self.grounding.as_deref(), gm);
        let merged = gj::parse(self.grounding.as_deref().unwrap_or_default()).into_owned();
        let queries = ws::grounding_queries(&merged);
        if !queries.is_empty() {
            if self.ws_query.is_empty() {
                self.ws_query = queries[0].clone();
            }
            self.ws_queries = queries;
        }
        let sources = ws::grounding_sources(&merged);
        if !sources.is_empty() {
            self.ws_sources = sources;
        }
        if !self.ws_opened && ws::has_valid_web_grounding(&merged) {
            self.open_web_search(out);
        }
        self.emit_late_citations(out);
    }

    /// One part of `candidates.0.content.parts`; false stops the chunk (Go's ForEach).
    fn part(&mut self, index_in_chunk: i64, part: &Res<'_>, valid: bool, out: &mut Vec<Vec<u8>>) -> bool {
        let explicit = [part.get("partIndex"), part.get("index")]
            .into_iter()
            .find(Res::exists)
            .map_or(-1, |p| p.int());
        let mut signature = trim_space(&part.get("thoughtSignature").bytes()).to_vec();
        if signature.is_empty() {
            signature = trim_space(&part.get("thought_signature").bytes()).to_vec();
        }
        let function_call = part.get("functionCall");
        let text = part.get("text");
        let text_value = text.bytes().into_owned();
        let is_thought = part.get("thought").bool();
        let kind = if is_thought {
            "thought"
        } else if function_call.exists() {
            "function"
        } else if text.exists() {
            "text"
        } else {
            "unknown"
        };
        let current_part_index = if explicit >= 0 {
            self.current_logical_part_index = explicit;
            self.current_part_kind = kind;
            self.has_seen_first_part = true;
            self.text_part_run_active = kind == "text";
            explicit
        } else if !self.has_seen_first_part {
            self.has_seen_first_part = true;
            self.current_logical_part_index = 0;
            self.current_part_kind = kind;
            if kind == "text" {
                self.text_part_run_active = true;
            }
            0
        } else {
            let advance = index_in_chunk > 0
                || kind != self.current_part_kind
                || kind == "function"
                || (kind == "text" && !self.text_part_run_active);
            if advance {
                self.current_logical_part_index += 1;
                self.current_part_kind = kind;
                self.text_part_run_active = kind == "text";
            }
            self.current_logical_part_index
        };

        if function_call.exists() && !self.pending_reasoning_signature.is_empty() {
            let pending = std::mem::take(&mut self.pending_reasoning_signature);
            if signature.is_empty() {
                self.emit_detached_reasoning(&pending, NEXT, FUNCTION, out);
            } else {
                self.emit_trailing_detached_reasoning(&pending, out);
            }
        }
        let reasoning_active = (self.reasoning_opened && !self.reasoning_closed)
            || (!self.reasoning_opened && (!self.reasoning_buf.is_empty() || !self.reasoning_enc.is_empty()));
        if !signature.is_empty() && !is_thought {
            if reasoning_active {
                if self.reasoning_enc.is_empty() || self.reasoning_enc == signature {
                    self.reasoning_enc = signature.clone();
                    (self.reasoning_direction, self.reasoning_target) = if function_call.exists() {
                        (NEXT, FUNCTION)
                    } else if text.exists() && !text_value.is_empty() {
                        (NEXT, TEXT)
                    } else {
                        (STANDALONE, TEXT)
                    };
                    self.seen_reasoning_signatures.insert(signature.clone());
                } else {
                    self.finalize_reasoning(out);
                    if function_call.exists() {
                        self.emit_detached_reasoning(&signature, NEXT, FUNCTION, out);
                    } else if !self.seen_reasoning_signatures.contains(&signature) {
                        self.pending_reasoning_signature = signature.clone();
                    }
                }
                if text.exists() && text_value.is_empty() && !function_call.exists() {
                    self.finalize_reasoning(out);
                    return true;
                }
            } else if function_call.exists() {
                self.emit_detached_reasoning(&signature, NEXT, FUNCTION, out);
            } else if text.exists() && !text_value.is_empty() {
                if !self.pending_reasoning_signature.is_empty() && self.pending_reasoning_signature != signature {
                    let pending = std::mem::take(&mut self.pending_reasoning_signature);
                    self.emit_trailing_detached_reasoning(&pending, out);
                }
                if !self.seen_reasoning_signatures.contains(&signature) {
                    self.pending_reasoning_signature = signature.clone();
                }
            } else if text.exists() {
                let pending = std::mem::take(&mut self.pending_reasoning_signature);
                if !pending.is_empty() && pending != signature {
                    self.emit_trailing_detached_reasoning(&pending, out);
                }
                if self.msg_opened || !self.func_done.is_empty() || !self.ws_buffered_deltas.is_empty() {
                    self.emit_trailing_detached_reasoning(&signature, out);
                } else if !self.seen_reasoning_signatures.contains(&signature) {
                    self.pending_reasoning_signature = signature.clone();
                }
                return true;
            }
        }

        if is_thought {
            if !self.ws_buffered_deltas.is_empty() {
                self.finalize_message(out);
            }
            if !self.pending_reasoning_signature.is_empty() && self.msg_opened && !self.msg_closed {
                let pending = std::mem::take(&mut self.pending_reasoning_signature);
                self.emit_trailing_detached_reasoning(&pending, out);
            }
            let mut incoming = vec![];
            if !signature.is_empty() && signature != BYPASS_SIGNATURE {
                let pending = std::mem::take(&mut self.pending_reasoning_signature);
                if !pending.is_empty() && pending != signature {
                    self.emit_detached_reasoning(&pending, STANDALONE, ANY, out);
                }
                incoming = signature.clone();
            } else if !self.pending_reasoning_signature.is_empty() {
                incoming = std::mem::take(&mut self.pending_reasoning_signature);
            }
            if self.reasoning_opened
                && !self.reasoning_closed
                && !incoming.is_empty()
                && !self.reasoning_enc.is_empty()
                && incoming != self.reasoning_enc
            {
                self.finalize_reasoning(out);
                self.reset_reasoning();
            }
            if self.reasoning_closed {
                self.finalize_message(out);
                self.reset_reasoning();
            } else if !self.reasoning_opened && self.reasoning_buf.is_empty() && self.msg_opened && !self.msg_closed {
                self.finalize_message(out);
            }
            if !incoming.is_empty() {
                self.reasoning_enc = incoming.clone();
                self.reasoning_direction = STANDALONE;
                self.reasoning_target = TEXT;
                self.seen_reasoning_signatures.insert(incoming);
            }
            if text.exists() && !text_value.is_empty() {
                self.last_semantic_kind = TEXT;
                self.reasoning_buf.extend_from_slice(&text_value);
                if self.reasoning_opened {
                    let mut msg = self.reasoning_event(br#"{"type":"response.reasoning_summary_text.delta","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"delta":""}"#);
                    gj::set_str(&mut msg, "delta", &text_value);
                    out.push(event("response.reasoning_summary_text.delta", &msg));
                } else {
                    self.reasoning_pending_deltas.push(text_value);
                }
            }
            if !self.reasoning_opened && !self.reasoning_enc.is_empty() {
                self.open_reasoning(out);
            }
            return true;
        }

        if text.exists() && !text_value.is_empty() {
            if signature.is_empty()
                && !self.pending_reasoning_signature.is_empty()
                && ((self.msg_opened && !self.msg_closed) || !self.ws_buffered_deltas.is_empty())
            {
                let pending = std::mem::take(&mut self.pending_reasoning_signature);
                self.emit_trailing_detached_reasoning(&pending, out);
            }
            self.finalize_reasoning(out);
            self.reopen_closed_message();
            if self.web_search_stream_mode && !self.ws_done {
                self.last_semantic_kind = TEXT;
                self.ws_buffered_deltas.push(text_value.clone());
                match self.ws_buffered_parts.last_mut() {
                    Some((index, buffered)) if *index == current_part_index => buffered.extend_from_slice(&text_value),
                    _ => self.ws_buffered_parts.push((current_part_index, text_value)),
                }
                self.text_part_run_active = true;
                return true;
            }
            if !self.msg_opened {
                self.open_message(out);
            }
            self.last_semantic_kind = TEXT;
            self.item_text.extend_from_slice(&text_value);
            self.map_part(current_part_index, &text_value);
            self.text_delta(&text_value, out);
            self.text_part_run_active = true;
            return true;
        }

        if function_call.exists() {
            return self.function_call(&function_call, explicit, valid, out);
        }
        true
    }

    fn function_call(&mut self, fc: &Res<'_>, explicit: i64, valid: bool, out: &mut Vec<Vec<u8>>) -> bool {
        // Responses streaming needs message done events before the next output item.
        self.finalize_reasoning(out);
        self.finalize_web_search(out);
        if !self.ws_buffered_deltas.is_empty() {
            self.flush_web_search_buffered_text(out);
        }
        self.finalize_message(out);
        self.last_semantic_kind = FUNCTION;

        let evidence_index = self.evidence.record(&self.identities, fc, explicit, valid);
        let evidence = &self.evidence.items[evidence_index];
        if evidence.apply_patch
            && let Some(err) = evidence.err.clone()
        {
            self.failed(err, out);
            return false;
        }
        if evidence.raw_name.is_empty() {
            return true;
        }
        let raw_name = if evidence.apply_patch {
            evidence.raw_name.clone()
        } else {
            fc.get("name").bytes().into_owned()
        };
        let identity = resolve_identity(&self.identities, self.sanitized_names.as_ref(), &raw_name);
        let args_raw = fc.get("args").raw.to_vec();
        if evidence.apply_patch && evidence.patch_call.is_some() {
            let patch = self.evidence.items[evidence_index].patch_call.as_mut().unwrap();
            if let Err(err) = patch.finish_arguments(&args_raw) {
                self.failed(err, out);
                return false;
            }
            return true;
        }
        let upstream_id = evidence.upstream_id.clone();

        let index = self.next_index;
        self.next_index += 1;
        self.func_args.entry(index).or_default();
        if identity.apply_patch {
            self.func_call_ids.insert(index, upstream_id);
        }
        if self.func_call_ids.get(&index).is_none_or(Vec::is_empty) {
            let id = format!(
                "call_{}_{}",
                now_nanos(),
                FUNCTION_CALL_IDS.fetch_add(1, Ordering::Relaxed) + 1
            );
            self.func_call_ids.insert(index, id.into_bytes());
        }
        let call_id = self.func_call_ids[&index].clone();
        let (name, namespace) = (identity.name.clone(), identity.namespace.clone());
        self.func_names.insert(index, name.clone());
        self.func_namespaces.insert(index, namespace.clone());
        self.func_custom.insert(index, identity.custom);
        let args = if fc.get("args").exists() {
            args_raw
        } else {
            b"{}".to_vec()
        };
        let buffer = self.func_args.get_mut(&index).unwrap();
        if buffer.is_empty() && !args.is_empty() {
            buffer.extend_from_slice(&args);
        }

        if identity.custom {
            let item_id = [&b"ctc_"[..], &call_id].concat();
            let mut input = unwrap_custom_tool_input(&args);
            let mut patch: Option<CallState> = None;
            if identity.apply_patch {
                let mut call = CallState {
                    item_id: item_id.clone(),
                    call_id: call_id.clone(),
                    name: name.clone(),
                    namespace: namespace.clone(),
                    output_index: index,
                    ..Default::default()
                };
                let mut result = call.finish_arguments(&args);
                if !valid {
                    result = Err("invalid Gemini apply_patch response JSON".into());
                }
                match result {
                    Err(err) => {
                        self.failed(err, out);
                        return false;
                    }
                    Ok((_, decoded)) => input = decoded.into_bytes(),
                }
                patch = Some(call);
            }
            self.func_input.insert(index, input.clone());
            let mut item = br#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"custom_tool_call","status":"in_progress","input":"","call_id":"","name":""}}"#.to_vec();
            let seq = self.next_seq();
            gj::set_int(&mut item, "sequence_number", seq);
            gj::set_int(&mut item, "output_index", index);
            gj::set_str(&mut item, "item.id", &item_id);
            gj::set_str(&mut item, "item.call_id", &call_id);
            set_identity(&mut item, &name, &namespace, "item");
            out.push(event("response.output_item.added", &item));
            let input_text = String::from_utf8_lossy(&input).into_owned();
            // Gemini delivers complete arguments; this delta is not an early preview.
            if let Some(patch) = &patch
                && !input.is_empty()
            {
                let seq = self.next_seq();
                out.push(event(
                    "response.custom_tool_call_input.delta",
                    &patch.input_delta(&input_text, seq),
                ));
            }
            if !self.func_done.contains(&index) {
                let mut input_done = br#"{"type":"response.custom_tool_call_input.done","sequence_number":0,"item_id":"","output_index":0,"input":""}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut input_done, "sequence_number", seq);
                gj::set_str(&mut input_done, "item_id", &item_id);
                gj::set_int(&mut input_done, "output_index", index);
                gj::set_str(&mut input_done, "input", &input);
                if let Some(patch) = &patch {
                    input_done = patch.input_done(&input_text, self.seq);
                }
                out.push(event("response.custom_tool_call_input.done", &input_done));
                let mut item_done = br#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"custom_tool_call","status":"completed","input":"","call_id":"","name":""}}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut item_done, "sequence_number", seq);
                gj::set_int(&mut item_done, "output_index", index);
                gj::set_str(&mut item_done, "item.id", &item_id);
                gj::set_str(&mut item_done, "item.input", &input);
                gj::set_str(&mut item_done, "item.call_id", &call_id);
                set_identity(&mut item_done, &name, &namespace, "item");
                out.push(event("response.output_item.done", &item_done));
                self.func_done.insert(index);
            }
            if patch.is_some() {
                self.evidence.items[evidence_index].patch_call = patch;
            }
        } else {
            let item_id = [&b"fc_"[..], &call_id].concat();
            let mut item = br#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"function_call","status":"in_progress","arguments":"","call_id":"","name":""}}"#.to_vec();
            let seq = self.next_seq();
            gj::set_int(&mut item, "sequence_number", seq);
            gj::set_int(&mut item, "output_index", index);
            gj::set_str(&mut item, "item.id", &item_id);
            gj::set_str(&mut item, "item.call_id", &call_id);
            set_identity(&mut item, &name, &namespace, "item");
            out.push(event("response.output_item.added", &item));
            if !args.is_empty() {
                let mut delta = br#"{"type":"response.function_call_arguments.delta","sequence_number":0,"item_id":"","output_index":0,"delta":""}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut delta, "sequence_number", seq);
                gj::set_str(&mut delta, "item_id", &item_id);
                gj::set_int(&mut delta, "output_index", index);
                gj::set_str_no_html(&mut delta, "delta", &args);
                out.push(event("response.function_call_arguments.delta", &delta));
            }
            if !self.func_done.contains(&index) {
                self.function_call_done(index, &call_id, &args, &name, &namespace, out);
                self.func_done.insert(index);
            }
        }
        true
    }

    fn function_call_done(
        &mut self,
        index: i64,
        call_id: &[u8],
        args: &[u8],
        name: &[u8],
        namespace: &[u8],
        out: &mut Vec<Vec<u8>>,
    ) {
        let item_id = [&b"fc_"[..], call_id].concat();
        let mut done = br#"{"type":"response.function_call_arguments.done","sequence_number":0,"item_id":"","output_index":0,"arguments":""}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut done, "sequence_number", seq);
        gj::set_str(&mut done, "item_id", &item_id);
        gj::set_int(&mut done, "output_index", index);
        gj::set_str_no_html(&mut done, "arguments", args);
        out.push(event("response.function_call_arguments.done", &done));
        let mut item_done = br#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"function_call","status":"completed","arguments":"","call_id":"","name":""}}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut item_done, "sequence_number", seq);
        gj::set_int(&mut item_done, "output_index", index);
        gj::set_str(&mut item_done, "item.id", &item_id);
        gj::set_str_no_html(&mut item_done, "item.arguments", args);
        gj::set_str(&mut item_done, "item.call_id", call_id);
        set_identity(&mut item_done, name, namespace, "item");
        out.push(event("response.output_item.done", &item_done));
    }

    fn finish(&mut self, root: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        if let Some(err) = self.evidence.pending_identity_error(&self.identities) {
            self.failed(err, out);
            return;
        }
        if !self.pending_reasoning_signature.is_empty() {
            let pending = std::mem::take(&mut self.pending_reasoning_signature);
            self.emit_trailing_detached_reasoning(&pending, out);
        }
        // Web search first (with all sources), then reasoning, then the message.
        self.finalize_web_search(out);
        self.finalize_reasoning(out);
        self.finalize_message(out);

        let open: Vec<i64> = self
            .func_args
            .keys()
            .copied()
            .filter(|i| !self.func_done.contains(i))
            .collect();
        for index in open {
            let call_id = self.func_call_ids.get(&index).cloned().unwrap_or_default();
            let name = self.func_names.get(&index).cloned().unwrap_or_default();
            let namespace = self.func_namespaces.get(&index).cloned().unwrap_or_default();
            if self.func_custom.get(&index).copied().unwrap_or(false) {
                let input = self.func_input.get(&index).cloned().unwrap_or_default();
                let item_id = [&b"ctc_"[..], &call_id].concat();
                let mut input_done = br#"{"type":"response.custom_tool_call_input.done","sequence_number":0,"item_id":"","output_index":0,"input":""}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut input_done, "sequence_number", seq);
                gj::set_str(&mut input_done, "item_id", &item_id);
                gj::set_int(&mut input_done, "output_index", index);
                gj::set_str(&mut input_done, "input", &input);
                out.push(event("response.custom_tool_call_input.done", &input_done));
                let mut item_done = br#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"custom_tool_call","status":"completed","input":"","call_id":"","name":""}}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut item_done, "sequence_number", seq);
                gj::set_int(&mut item_done, "output_index", index);
                gj::set_str(&mut item_done, "item.id", &item_id);
                gj::set_str(&mut item_done, "item.input", &input);
                gj::set_str(&mut item_done, "item.call_id", &call_id);
                set_identity(&mut item_done, &name, &namespace, "item");
                out.push(event("response.output_item.done", &item_done));
            } else {
                let args = self
                    .func_args
                    .get(&index)
                    .filter(|a| !a.is_empty())
                    .cloned()
                    .unwrap_or(b"{}".to_vec());
                self.function_call_done(index, &call_id, &args, &name, &namespace, out);
            }
            self.func_done.insert(index);
        }

        let mut completed = br#"{"type":"response.completed","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"completed","background":false,"error":null}}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut completed, "sequence_number", seq);
        gj::set_str(&mut completed, "response.id", &self.response_id);
        gj::set_int(&mut completed, "response.created_at", self.created_at);
        if !self.request.is_empty() {
            let root = unwrap_request_root(gj::parse(&self.request));
            copy_request_fields(&mut completed, &root.raw, "response.", Echo::default());
        }
        self.emit_late_citations(out);

        let mut outputs: Vec<Vec<u8>> = vec![];
        for index in 0..self.next_index {
            if self.ws_done && index == self.ws_index {
                outputs.push(ws::web_search_call_item(
                    &self.ws_item_id,
                    &self.ws_query,
                    &self.ws_queries,
                    &self.ws_sources,
                ));
            } else if let Some(r) = self.completed_reasoning.get(&index) {
                let mut item =
                    br#"{"id":"","type":"reasoning","encrypted_content":"","summary":[{"type":"summary_text","text":""}]}"#.to_vec();
                gj::set_str(&mut item, "id", &r.id);
                gj::set_str(&mut item, "encrypted_content", &r.signature);
                gj::set_str(&mut item, "summary.0.text", &r.text);
                outputs.push(item);
            } else if let Some(m) = self.completed_messages.get(&index) {
                outputs.push(message_item(&m.id, &m.text, &m.annotations));
            } else if let Some((id, signature)) = self.detached_reasoning.get(&index) {
                let mut item = br#"{"id":"","type":"reasoning","encrypted_content":"","summary":[]}"#.to_vec();
                gj::set_str(&mut item, "id", id);
                gj::set_str(&mut item, "encrypted_content", signature);
                outputs.push(item);
            } else if let Some(call_id) = self.func_call_ids.get(&index).filter(|c| !c.is_empty()) {
                let name = self.func_names.get(&index).cloned().unwrap_or_default();
                let namespace = self.func_namespaces.get(&index).cloned().unwrap_or_default();
                if self.func_custom.get(&index).copied().unwrap_or(false) {
                    let input = self.func_input.get(&index).cloned().unwrap_or_default();
                    outputs.push(custom_call_item(call_id, &input, &name, &namespace));
                } else {
                    let args = self
                        .func_args
                        .get(&index)
                        .filter(|a| !a.is_empty())
                        .cloned()
                        .unwrap_or(b"{}".to_vec());
                    outputs.push(function_call_item(call_id, &args, &name, &namespace));
                }
            }
        }
        if !outputs.is_empty() {
            gj::set_raw(&mut completed, "response.output", gj::join(&outputs));
        }
        if self.ws_done {
            gj::set_int(&mut completed, "response.tool_usage.web_search.num_requests", 1);
        }
        let usage = root.get("usageMetadata");
        if usage.exists() {
            set_usage(&mut completed, "response.usage.", &usage, true);
        }
        out.push(event("response.completed", &completed));
        self.completed = true;
    }
}

fn message_item(id: &[u8], text: &[u8], annotations: &[Vec<u8>]) -> Vec<u8> {
    let mut item = br#"{"id":"","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":""}],"role":"assistant"}"#.to_vec();
    gj::set_str(&mut item, "id", id);
    gj::set_str(&mut item, "content.0.text", text);
    if !annotations.is_empty() {
        gj::set_raw(&mut item, "content.0.annotations", gj::join(annotations));
    }
    item
}

fn custom_call_item(call_id: &[u8], input: &[u8], name: &[u8], namespace: &[u8]) -> Vec<u8> {
    let mut item =
        br#"{"id":"","type":"custom_tool_call","status":"completed","input":"","call_id":"","name":""}"#.to_vec();
    gj::set_str(&mut item, "id", [&b"ctc_"[..], call_id].concat());
    gj::set_str(&mut item, "input", input);
    gj::set_str(&mut item, "call_id", call_id);
    set_identity(&mut item, name, namespace, "");
    item
}

fn function_call_item(call_id: &[u8], args: &[u8], name: &[u8], namespace: &[u8]) -> Vec<u8> {
    let mut item =
        br#"{"id":"","type":"function_call","status":"completed","arguments":"","call_id":"","name":""}"#.to_vec();
    gj::set_str(&mut item, "id", [&b"fc_"[..], call_id].concat());
    gj::set_str_no_html(&mut item, "arguments", args);
    gj::set_str(&mut item, "call_id", call_id);
    set_identity(&mut item, name, namespace, "");
    item
}

/// The usage mapping: prompt tokens in, candidates plus thoughts out. The stream always
/// writes reasoning and total tokens; the non-stream body only when Gemini sent them.
fn set_usage(out: &mut Vec<u8>, prefix: &str, usage: &Res<'_>, defaults: bool) {
    let at = |k: &str| format!("{prefix}{k}");
    gj::set_int(out, &at("input_tokens"), usage.get("promptTokenCount").int());
    gj::set_int(
        out,
        &at("input_tokens_details.cached_tokens"),
        usage.get("cachedContentTokenCount").int(),
    );
    gj::set_int(
        out,
        &at("output_tokens"),
        usage
            .get("candidatesTokenCount")
            .int()
            .wrapping_add(usage.get("thoughtsTokenCount").int()),
    );
    for (key, source) in [
        ("output_tokens_details.reasoning_tokens", "thoughtsTokenCount"),
        ("total_tokens", "totalTokenCount"),
    ] {
        let v = usage.get(source);
        if v.exists() {
            gj::set_int(out, &at(key), v.int());
        } else if defaults {
            gj::set_int(out, &at(key), 0);
        }
    }
}

impl GoStream for State {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let mut raw = line;
        if let Some(rest) = raw.strip_prefix(b"data:") {
            raw = trim_space(rest);
        }
        let raw = trim_space(raw);
        if raw.is_empty() || self.completed {
            return Ok(vec![]);
        }
        let raw: &[u8] = if raw == b"[DONE]" {
            if !self.started {
                return Ok(vec![]);
            }
            br#"{"candidates":[{"finishReason":"STOP"}]}"#
        } else {
            raw
        };
        let parsed = gj::parse(raw);
        if !parsed.exists() {
            return Ok(vec![]);
        }
        let root = unwrap_response_root(parsed);
        let mut out = vec![];
        if !self.started {
            self.start(&root, &mut out);
        }
        let gm = ws::grounding_metadata(&root);
        if gm.exists() {
            self.grounding_frame(&gm, &mut out);
        }
        let parts = root.get("candidates.0.content.parts");
        if parts.is_array() {
            let valid = gj::valid(raw);
            for (index, part) in parts.array().iter().enumerate() {
                if !self.part(index as i64, part, valid, &mut out) {
                    break;
                }
            }
        }
        if self.completed {
            return Ok(out);
        }
        let finish_reason = root.get("candidates.0.finishReason");
        if finish_reason.exists() && !finish_reason.bytes().is_empty() {
            self.finish(&root, &mut out);
        }
        Ok(out)
    }

    fn tool_input_failed(&self) -> bool {
        self.tool_error.is_some()
    }

    /// FinalizeToolInput: a patch-enabled stream that ends without its terminator fails.
    fn finalize_tool_input(&mut self) -> Vec<Vec<u8>> {
        if self.tool_error.is_some() || self.completed || !self.identities.values().any(|i| i.apply_patch) {
            return vec![];
        }
        let mut out = vec![];
        self.failed(
            "upstream apply_patch stream ended before protocol completion".into(),
            &mut out,
        );
        out
    }
}

// ---------------------------------------------------------------------------------------
// Non-stream (ConvertGeminiResponseToOpenAIResponsesNonStream)

enum Output {
    Reasoning(usize),
    Message(usize),
    Function(usize),
    Detached(usize),
}

// The flush macros mirror Go's closures, which reset state their last call never reads.
#[allow(unused_assignments)]
pub fn non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let root = unwrap_response_root(gj::parse(body));
    let request = pick_request(ctx.original_request, ctx.translated_request);
    let sanitized = sanitized_tool_name_map(ctx.original_request);
    let identities = reverse_identity_map(request);

    let mut resp =
        br#"{"id":"","object":"response","created_at":0,"status":"completed","background":false,"error":null,"incomplete_details":null}"#
            .to_vec();
    let mut id = root.get("responseId").bytes().into_owned();
    if id.is_empty() {
        id = format!(
            "resp_{:x}_{}",
            now_nanos(),
            RESPONSE_IDS.fetch_add(1, Ordering::Relaxed) + 1
        )
        .into_bytes();
    }
    if !id.starts_with(b"resp_") {
        id = [&b"resp_"[..], &id].concat();
    }
    gj::set_str(&mut resp, "id", &id);
    let mut created_at = now_unix();
    let create_time = root.get("createTime");
    if create_time.exists()
        && let Some(t) = parse_rfc3339_unix(&create_time.bytes())
    {
        created_at = t;
    }
    gj::set_int(&mut resp, "created_at", created_at);
    let model_version = root.get("modelVersion");
    let model_version = model_version.exists().then(|| model_version.bytes().into_owned());
    if !request.is_empty() {
        let req = unwrap_request_root(gj::parse(request));
        copy_request_fields(
            &mut resp,
            &req.raw,
            "",
            Echo {
                model: model_version.as_deref(),
                ..Echo::default()
            },
        );
    } else if let Some(version) = &model_version {
        gj::set_str(&mut resp, "model", version);
    }
    let rid = id.strip_prefix(b"resp_").unwrap_or(&id).to_vec();

    // (text, signature, direction, target)
    let mut reasoning_outputs: Vec<(Vec<u8>, Vec<u8>, &str, &str)> = vec![];
    let mut function_outputs: Vec<(Vec<u8>, Vec<u8>)> = vec![];
    let mut message_outputs: Vec<(Vec<u8>, Vec<Vec<u8>>)> = vec![];
    let mut detached_outputs: Vec<(Vec<u8>, &str, &str)> = vec![];
    let mut order: Vec<Output> = vec![];
    let mut reasoning_signatures: HashSet<Vec<u8>> = HashSet::new();
    let (mut reasoning_text, mut reasoning_enc) = (Vec::new(), Vec::new());
    let (mut reasoning_direction, mut reasoning_target) = ("", "");
    let mut message_text: Vec<u8> = vec![];
    let mut message_signatures: Vec<Vec<u8>> = vec![];
    let mut part_mappings: Vec<PartMapping> = vec![];
    let mut rune_offset = 0i64;
    let mut tool_error: Option<String> = None;
    let mut evidence = EvidenceLog::default();
    let valid = gj::valid(body);

    macro_rules! flush_reasoning {
        () => {
            if !reasoning_text.is_empty() || !reasoning_enc.is_empty() {
                order.push(Output::Reasoning(reasoning_outputs.len()));
                if !reasoning_enc.is_empty() {
                    reasoning_signatures.insert(reasoning_enc.clone());
                }
                reasoning_outputs.push((
                    std::mem::take(&mut reasoning_text),
                    std::mem::take(&mut reasoning_enc),
                    reasoning_direction,
                    reasoning_target,
                ));
                (reasoning_direction, reasoning_target) = ("", "");
            }
        };
    }
    macro_rules! flush_message {
        () => {
            if !message_text.is_empty() {
                order.push(Output::Message(message_outputs.len()));
                message_outputs.push((
                    std::mem::take(&mut message_text),
                    std::mem::take(&mut message_signatures),
                ));
                rune_offset = 0;
            }
        };
    }
    macro_rules! detached {
        ($signature:expr, $direction:expr, $target:expr) => {
            order.push(Output::Detached(detached_outputs.len()));
            detached_outputs.push(($signature, $direction, $target));
        };
    }

    let parts = root.get("candidates.0.content.parts");
    if parts.is_array() {
        for (key, p) in parts.array().iter().enumerate() {
            let explicit = [p.get("partIndex"), p.get("index")].into_iter().find(Res::exists);
            let part_index = explicit.as_ref().map_or(key as i64, Res::int);
            let mut signature = trim_space(&p.get("thoughtSignature").bytes()).to_vec();
            if signature.is_empty() {
                signature = trim_space(&p.get("thought_signature").bytes()).to_vec();
            }
            if p.get("thought").bool() {
                flush_message!();
                rune_offset = 0;
                if !signature.is_empty() && !reasoning_enc.is_empty() && signature != reasoning_enc {
                    flush_reasoning!();
                }
                let text = p.get("text");
                if text.exists() {
                    reasoning_text.extend_from_slice(&text.bytes());
                }
                if !signature.is_empty() {
                    reasoning_enc = signature;
                    (reasoning_direction, reasoning_target) = (STANDALONE, TEXT);
                }
                continue;
            }
            let text = p.get("text").bytes().into_owned();
            if p.get("text").exists() && !text.is_empty() {
                let mut message_signature = vec![];
                if !signature.is_empty() {
                    if !reasoning_text.is_empty() && reasoning_enc.is_empty() {
                        reasoning_enc = signature;
                        (reasoning_direction, reasoning_target) = (NEXT, TEXT);
                    } else {
                        message_signature = signature;
                    }
                }
                flush_reasoning!();
                if message_signatures
                    .last()
                    .is_some_and(|last| message_signature.is_empty() || *last != message_signature)
                {
                    flush_message!();
                    rune_offset = 0;
                }
                part_mappings.push(PartMapping {
                    part_index,
                    message_index: message_outputs.len() as i64,
                    start_rune: rune_offset,
                    text: text.clone(),
                });
                rune_offset += rune_count(&text);
                message_text.extend_from_slice(&text);
                if !message_signature.is_empty() && message_signatures.last() != Some(&message_signature) {
                    message_signatures.push(message_signature);
                }
                continue;
            }
            let fc = p.get("functionCall");
            if fc.exists() {
                if !reasoning_text.is_empty() && reasoning_enc.is_empty() && !signature.is_empty() {
                    reasoning_enc = std::mem::take(&mut signature);
                    (reasoning_direction, reasoning_target) = (NEXT, FUNCTION);
                }
                flush_reasoning!();
                flush_message!();
                rune_offset = 0;
                let explicit_index = explicit.as_ref().map_or(-1, Res::int);
                let evidence_index = evidence.record(&identities, &fc, explicit_index, valid);
                let ev = &evidence.items[evidence_index];
                if ev.apply_patch
                    && let Some(err) = ev.err.clone()
                {
                    tool_error = Some(err);
                    break;
                }
                if ev.raw_name.is_empty() {
                    continue;
                }
                let raw_name = if ev.apply_patch {
                    ev.raw_name.clone()
                } else {
                    fc.get("name").bytes().into_owned()
                };
                let identity = resolve_identity(&identities, sanitized.as_ref(), &raw_name);
                if identity.apply_patch
                    && let Some(patch) = evidence.items[evidence_index].patch_call.as_mut()
                {
                    if let Err(err) = patch.finish_arguments(&fc.get("args").raw) {
                        tool_error = Some(err);
                        break;
                    }
                    continue;
                }
                let args = fc.get("args");
                let args = if args.exists() { args.raw.to_vec() } else { vec![] };
                let upstream_id = evidence.items[evidence_index].upstream_id.clone();
                let call_id = if identity.apply_patch && !upstream_id.is_empty() {
                    upstream_id
                } else {
                    format!(
                        "call_{:x}_{}",
                        now_nanos(),
                        FUNCTION_CALL_IDS.fetch_add(1, Ordering::Relaxed) + 1
                    )
                    .into_bytes()
                };
                let item = if identity.custom {
                    let mut input = unwrap_custom_tool_input(&args);
                    if identity.apply_patch {
                        let mut patch = CallState::default();
                        let mut result = patch.finish_arguments(&args);
                        if !valid {
                            result = Err("invalid Gemini apply_patch response JSON".into());
                        }
                        match result {
                            Err(err) => {
                                tool_error = Some(err);
                                break;
                            }
                            Ok((_, decoded)) => input = decoded.into_bytes(),
                        }
                        evidence.items[evidence_index].patch_call = Some(patch);
                    }
                    let mut item = br#"{"id":"","type":"custom_tool_call","status":"completed","input":"","call_id":"","name":""}"#.to_vec();
                    gj::set_str(&mut item, "id", [&b"ctc_"[..], &call_id].concat());
                    gj::set_str(&mut item, "call_id", &call_id);
                    gj::set_str(&mut item, "input", &input);
                    set_identity(&mut item, &identity.name, &identity.namespace, "");
                    item
                } else {
                    let mut item = br#"{"id":"","type":"function_call","status":"completed","arguments":"","call_id":"","name":""}"#.to_vec();
                    gj::set_str(&mut item, "id", [&b"fc_"[..], &call_id].concat());
                    gj::set_str(&mut item, "call_id", &call_id);
                    gj::set_str_no_html(&mut item, "arguments", &args);
                    set_identity(&mut item, &identity.name, &identity.namespace, "");
                    item
                };
                order.push(Output::Function(function_outputs.len()));
                function_outputs.push((item, signature));
                continue;
            }
            if !signature.is_empty() {
                if !reasoning_text.is_empty() {
                    if reasoning_enc.is_empty() {
                        reasoning_enc = signature;
                        (reasoning_direction, reasoning_target) = (STANDALONE, TEXT);
                    } else if reasoning_enc != signature {
                        flush_reasoning!();
                        detached!(signature, PREVIOUS, TEXT);
                    }
                } else if !message_text.is_empty() {
                    if message_signatures.is_empty() {
                        message_signatures.push(signature);
                    } else if message_signatures.last() != Some(&signature) {
                        flush_message!();
                        rune_offset = 0;
                        detached!(signature, PREVIOUS, TEXT);
                    }
                } else if !function_outputs.is_empty() {
                    detached!(signature, PREVIOUS, FUNCTION);
                } else {
                    detached!(signature, NEXT, ANY);
                }
            }
        }
    }

    if tool_error.is_none() {
        tool_error = evidence.pending_identity_error(&identities);
    }
    if tool_error.is_some() {
        return Err(Error(apply_patch::UPSTREAM_ERROR_MESSAGE.into()));
    }
    flush_reasoning!();
    flush_message!();

    let gm = ws::grounding_metadata(&root);
    let has_grounding = ws::has_valid_web_grounding(&gm);
    let mut ws_item = vec![];
    let mut citations: HashMap<i64, Vec<Vec<u8>>> = HashMap::new();
    if has_grounding {
        let queries = ws::grounding_queries(&gm);
        let mut query = queries.first().cloned().unwrap_or_default();
        if query.is_empty() && !request.is_empty() {
            query = ws::extract_query(&unwrap_request_root(gj::parse(request)));
        }
        let sources = ws::grounding_sources(&gm);
        ws_item = ws::web_search_call_item(&[&b"ws_"[..], &rid].concat(), &query, &queries, &sources);
        let texts: Vec<Vec<u8>> = message_outputs.iter().map(|m| m.0.clone()).collect();
        citations = ws::url_citations(&gm, &part_mappings, &texts).unwrap_or_default();
    }

    let mut outputs: Vec<Vec<u8>> = vec![];
    let mut seen_detached: HashSet<Vec<u8>> = HashSet::new();
    let mut detached_index = 0;
    let mut append_detached = |outputs: &mut Vec<Vec<u8>>, signature: &[u8], direction: &str, target: &str| {
        if signature.is_empty() || !seen_detached.insert(signature.to_vec()) {
            return;
        }
        let placement = if direction == PREVIOUS { "after" } else { "before" };
        let mut item = br#"{"id":"","type":"reasoning","encrypted_content":"","summary":[]}"#.to_vec();
        gj::set_str(
            &mut item,
            "id",
            [
                b"rs_",
                &rid[..],
                format!("_detached_{placement}_{detached_index}").as_bytes(),
            ]
            .concat(),
        );
        gj::set_str(
            &mut item,
            "encrypted_content",
            encode_carrier(signature, direction, target),
        );
        detached_index += 1;
        outputs.push(item);
    };
    let mut ws_appended = false;
    for output in &order {
        match *output {
            Output::Detached(i) => {
                let (signature, direction, target) = &detached_outputs[i];
                if !reasoning_signatures.contains(signature) {
                    append_detached(&mut outputs, signature, direction, target);
                }
            }
            Output::Reasoning(i) => {
                let (text, signature, direction, target) = &reasoning_outputs[i];
                let reasoning_id = if reasoning_outputs.len() > 1 {
                    [b"rs_", &rid[..], format!("_{i}").as_bytes()].concat()
                } else {
                    [&b"rs_"[..], &rid].concat()
                };
                let mut item = br#"{"id":"","type":"reasoning","encrypted_content":""}"#.to_vec();
                gj::set_str(&mut item, "id", &reasoning_id);
                let encrypted = if !signature.is_empty() && !direction.is_empty() {
                    encode_carrier(signature, direction, target)
                } else {
                    signature.clone()
                };
                gj::set_str(&mut item, "encrypted_content", &encrypted);
                if !text.is_empty() {
                    let mut summary = br#"{"type":"summary_text","text":""}"#.to_vec();
                    gj::set_str(&mut summary, "text", text);
                    gj::set_raw(&mut item, "summary", gj::join(&[summary]));
                }
                outputs.push(item);
            }
            Output::Message(i) => {
                if has_grounding && !ws_appended {
                    outputs.push(ws_item.clone());
                    ws_appended = true;
                }
                let (text, signatures) = &message_outputs[i];
                for signature in signatures {
                    if !reasoning_signatures.contains(signature) {
                        append_detached(&mut outputs, signature, NEXT, TEXT);
                    }
                }
                let id = [b"msg_", &rid[..], format!("_{i}").as_bytes()].concat();
                let annotations = citations.get(&(i as i64)).cloned().unwrap_or_default();
                outputs.push(message_item(&id, text, &annotations));
            }
            Output::Function(i) => {
                let (item, signature) = &function_outputs[i];
                append_detached(&mut outputs, signature, NEXT, FUNCTION);
                outputs.push(item.clone());
            }
        }
    }
    if has_grounding && !ws_appended {
        outputs.push(ws_item);
    }
    if !outputs.is_empty() {
        gj::set_raw(&mut resp, "output", gj::join(&outputs));
    }
    if has_grounding {
        gj::set_int(&mut resp, "tool_usage.web_search.num_requests", 1);
    }
    let usage = root.get("usageMetadata");
    if usage.exists() {
        set_usage(&mut resp, "usage.", &usage, false);
    }
    Ok(resp)
}
