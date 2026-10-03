//! Claude Messages responses -> OpenAI Responses
//! (internal/translator/claude/openai/responses/claude_openai-responses_response.go),
//! including the apply_patch bridge: a declared `apply_patch` custom tool streams its
//! decoded patch input, and invalid or conflicting input fails the response.
//
// ponytail: Go finalizes pending tool items in map order, which is random when several
// are open at once; this port uses ascending block index.

use crate::apply_patch::CallState;
use crate::{
    Error, Registered, RequestCtx, ResponseCtx,
    claude_responses::{
        REDACTED_THINKING_PREFIX, ToolNames, split_qualified_call, tool_descriptors, tool_winners, web_search_call_id,
    },
    common::{self, now_unix, sse_event, trim_space},
    stream::GoStream,
};
use cpa_common::json::{self as gj, Kind, Res};
use std::collections::{BTreeMap, HashMap, HashSet};

pub static PAIR: Registered = registered!(
    OpenAIResponse -> Claude,
    request: request,
    non_stream: non_stream,
    go_stream: go_stream,
    token_count: None,
);

fn request(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    Ok(crate::claude_responses::convert(ctx.model, body, ctx.stream, false))
}

struct Winner {
    custom: bool,
    apply_patch: bool,
    /// The declaration's local name and namespace (an apply_patch call's identity).
    local_name: Vec<u8>,
    namespace: Vec<u8>,
}

struct Tools {
    winners: HashMap<Vec<u8>, Winner>,
    names: ToolNames,
}

impl Tools {
    fn new(request: &[u8]) -> Self {
        let root = gj::parse(request);
        let descriptors = tool_descriptors(&root);
        let winners = tool_winners(&descriptors)
            .into_iter()
            .map(|(name, order)| {
                let d = &descriptors[order];
                let custom = d.kind == b"custom";
                let apply_patch = custom && crate::apply_patch::is_custom_tool(&d.tool);
                let local_name = if d.direct { d.name.clone() } else { d.child_name.clone() };
                let winner = Winner {
                    custom,
                    apply_patch,
                    local_name,
                    namespace: d.namespace.clone(),
                };
                (name, winner)
            })
            .collect();
        Tools {
            winners,
            names: ToolNames::build(&root),
        }
    }

    fn winner(&self, claude_name: &[u8]) -> Option<&Winner> {
        self.winners.get(&self.names.identity(claude_name))
    }

    /// isApplyPatch: the original request's winning declaration decides.
    fn is_apply_patch(&self, name: &[u8]) -> bool {
        self.winner(name).is_some_and(|w| w.apply_patch)
    }

    /// Two names for one block that resolve to different tools.
    fn conflicting(&self, a: &[u8], b: &[u8]) -> bool {
        !a.is_empty() && !b.is_empty() && self.names.identity(a) != self.names.identity(b)
    }
}

/// validateApplyPatchSnapshots: equivalent JSON spellings are fine, a different complete
/// input is not.
fn validate_patch_snapshots(previous: &[u8], current: &[u8]) -> Result<(), String> {
    let mut call = CallState::default();
    if !previous.is_empty() {
        call.finish_arguments(previous)?;
    }
    call.finish_arguments(current).map(|_| ())
}

/// finishClaudeApplyPatchArguments: complete streamed JSON is a complete snapshot that a
/// later snapshot may confirm but not extend.
fn finish_patch_arguments(call: &mut CallState, arguments: &[u8], snapshot: &[u8]) -> Result<(String, String), String> {
    if snapshot.is_empty() {
        return call.finish_arguments(arguments);
    }
    if gj::valid(arguments) {
        call.finish_arguments(arguments)?;
    }
    call.finish_arguments(snapshot)
}

/// A content block start's `input`, when it carries a snapshot (Claude's empty `{}`
/// placeholder does not).
fn input_snapshot<'a>(cb: &Res<'a>) -> Option<Res<'a>> {
    let input = cb.get("input");
    (input.exists() && (!input.is_object() || !input.map().is_empty())).then_some(input)
}

fn patch_failure() -> Error {
    Error(crate::apply_patch::UPSTREAM_ERROR_MESSAGE.into())
}

/// pickRequestJSON: the original request when valid, else the translated one.
pub(crate) fn pick_request<'a>(original: &'a [u8], translated: &'a [u8]) -> &'a [u8] {
    if !original.is_empty() && gj::valid(original) {
        original
    } else if !translated.is_empty() && gj::valid(translated) {
        translated
    } else {
        b""
    }
}

/// common.SetResponsesToolCallIdentity with the request's namespace split.
fn with_identity(mut item: Vec<u8>, request: &[u8], qualified: &[u8], path: &str) -> Vec<u8> {
    let (name, namespace) = split_qualified_call(request, qualified);
    let at = |key: &str| {
        if path.is_empty() {
            key.to_owned()
        } else {
            format!("{path}.{key}")
        }
    };
    gj::set_str(&mut item, &at("name"), name);
    if namespace.is_empty() {
        gj::delete(&mut item, &at("namespace"));
    } else {
        gj::set_str(&mut item, &at("namespace"), namespace);
    }
    item
}

fn reasoning_carrier(block: &Res<'_>) -> Vec<u8> {
    if block.get("type").str() == "redacted_thinking" {
        let data = block.get("data").bytes();
        if data.is_empty() {
            return vec![];
        }
        return [REDACTED_THINKING_PREFIX, &data].concat();
    }
    block.get("signature").bytes().into_owned()
}

#[derive(Default, Clone, Copy)]
struct Usage {
    input: i64,
    output: i64,
    creation: i64,
    read: i64,
    present: bool,
}

impl Usage {
    fn merge(&mut self, usage: &Res<'_>) {
        if !usage.exists() {
            return;
        }
        self.present = true;
        for (key, slot) in [
            ("input_tokens", &mut self.input),
            ("output_tokens", &mut self.output),
            ("cache_creation_input_tokens", &mut self.creation),
            ("cache_read_input_tokens", &mut self.read),
        ] {
            let v = usage.get(key);
            if v.exists() {
                *slot = v.int();
            }
        }
    }

    /// (input, output, total, cached).
    fn totals(&self) -> (i64, i64, i64, i64) {
        let input = self.input.wrapping_add(self.creation).wrapping_add(self.read);
        (input, self.output, input.wrapping_add(self.output), self.read)
    }
}

fn incomplete(stop: &[u8]) -> bool {
    trim_space(stop).eq_ignore_ascii_case(b"max_tokens")
}

fn output_status(stop: &[u8]) -> &'static str {
    if incomplete(stop) { "incomplete" } else { "completed" }
}

fn web_search_query(input: &[u8]) -> Vec<u8> {
    if input.is_empty() {
        return vec![];
    }
    trim_space(&gj::get(input, "query").bytes()).to_vec()
}

fn web_search_results(content: &Res<'_>) -> Option<Vec<u8>> {
    if content.is_object() {
        return Some(content.raw.to_vec());
    }
    if !content.is_array() {
        return None;
    }
    let mut results = vec![];
    content.each(|_, entry| {
        if entry.get("type").str() == "web_search_tool_result_error"
            || !trim_space(&entry.get("url").bytes()).is_empty()
        {
            results.push(entry.raw.to_vec());
        }
        true
    });
    Some(gj::join(&results))
}

fn web_search_item(tool_use_id: &[u8], query: &[u8], results: Option<&Vec<u8>>) -> Vec<u8> {
    let mut item =
        br#"{"id":"","type":"web_search_call","status":"completed","action":{"type":"search","query":""}}"#.to_vec();
    gj::set_str(&mut item, "id", web_search_call_id(tool_use_id));
    gj::set_str(&mut item, "action.query", query);
    if let Some(results) = results.filter(|r| !r.is_empty()) {
        gj::set_raw(&mut item, "results", results);
    }
    item
}

/// json.Marshal of collected `citation.Value()` annotations.
fn annotations_json(values: &[Vec<u8>]) -> Vec<u8> {
    gj::join(values)
}

/// unwrapCustomToolInput: the `input` string of the tool arguments, tolerating
/// truncated JSON.
pub(crate) fn unwrap_custom_tool_input(arguments: &[u8]) -> Vec<u8> {
    let trimmed = trim_space(arguments);
    let v = gj::get(trimmed, "input");
    if v.exists() {
        return if v.kind == Kind::String {
            v.s.to_vec()
        } else {
            v.raw.to_vec()
        };
    }
    let Some(idx) = trimmed.windows(7).position(|w| w == b"\"input\"") else {
        return arguments.to_vec();
    };
    let rest = trim_space(&trimmed[idx + 7..]);
    let Some(rest) = rest.strip_prefix(b":") else {
        return arguments.to_vec();
    };
    let rest = trim_space(rest);
    let Some(content) = rest.strip_prefix(b"\"") else {
        return arguments.to_vec();
    };
    let hex4 = |s: &[u8]| -> Option<u32> {
        let h = s.get(..4)?;
        if !h.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        u32::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok()
    };
    let push_rune = |out: &mut Vec<u8>, r: u32| {
        let c = char::from_u32(r).unwrap_or('\u{FFFD}');
        out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
    };
    let mut out = vec![];
    let mut escape = false;
    let mut i = 0;
    while i < content.len() {
        let c = content[i];
        if escape {
            escape = false;
            match c {
                b'"' | b'\\' | b'/' => out.push(c),
                b'b' => out.push(8),
                b'f' => out.push(12),
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                b't' => out.push(b'\t'),
                b'u' => {
                    if i + 4 < content.len()
                        && let Some(r) = hex4(&content[i + 1..])
                    {
                        if (0xD800..0xE000).contains(&r)
                            && i + 10 < content.len()
                            && &content[i + 5..i + 7] == b"\\u"
                            && let Some(r2) = hex4(&content[i + 7..])
                        {
                            let pair = if (0xD800..0xDC00).contains(&r) && (0xDC00..0xE000).contains(&r2) {
                                0x10000 + ((r - 0xD800) << 10) + (r2 - 0xDC00)
                            } else {
                                0xFFFD
                            };
                            push_rune(&mut out, pair);
                            i += 11;
                            continue;
                        }
                        push_rune(&mut out, r);
                        i += 5;
                        continue;
                    }
                    out.extend_from_slice(b"\\u");
                }
                _ => {
                    out.push(b'\\');
                    out.push(c);
                }
            }
        } else if c == b'\\' {
            escape = true;
        } else if c == b'"' {
            break;
        } else {
            out.push(c);
        }
        i += 1;
    }
    if escape {
        out.push(b'\\');
    }
    out
}

struct WebSearch {
    tool_use_id: Vec<u8>,
    output_index: i64,
    input: Vec<u8>,
    results: Option<Vec<u8>>,
    emitted: bool,
    status: &'static str,
}

impl WebSearch {
    fn render(&self) -> Vec<u8> {
        web_search_item(&self.tool_use_id, &web_search_query(&self.input), self.results.as_ref())
    }
}

struct MessageItem {
    id: Vec<u8>,
    output_index: i64,
    text: Vec<u8>,
    annotations: Vec<Vec<u8>>,
    status: &'static str,
}

struct ReasoningItem {
    id: Vec<u8>,
    output_index: i64,
    text: Vec<u8>,
    signature: Vec<u8>,
    status: &'static str,
}

/// claudeToResponsesState. Maps mirror Go's so presence checks keep their meaning.
struct State {
    model: Vec<u8>,
    original: Vec<u8>,
    translated: Vec<u8>,
    request: Vec<u8>,
    tools: Tools,
    completed: bool,
    /// ApplyPatchErrorState: an invalid apply_patch call failed the response.
    tool_error: bool,
    patch_calls: HashMap<i64, CallState>,
    func_input_snapshot: HashMap<i64, Vec<u8>>,
    func_snapshot_errors: HashSet<i64>,
    func_identity_conflicts: HashSet<i64>,
    seq: i64,
    response_id: Vec<u8>,
    created_at: i64,
    next_output_index: i64,
    current_msg_id: Vec<u8>,
    current_fc_id: Vec<u8>,
    in_text_block: bool,
    in_func_block: bool,
    message_open: bool,
    content_part_open: bool,
    message_output_index: i64,
    func_item_added: HashSet<i64>,
    func_args_sent: HashMap<i64, usize>,
    func_block_stopped: HashSet<i64>,
    func_args: BTreeMap<i64, Vec<u8>>,
    func_args_done: HashSet<i64>,
    func_item_done: HashSet<i64>,
    func_item_status: HashMap<i64, &'static str>,
    func_names: HashMap<i64, Vec<u8>>,
    func_call_ids: BTreeMap<i64, Vec<u8>>,
    func_custom: HashMap<i64, bool>,
    func_output_indices: HashMap<i64, i64>,
    text: Vec<u8>,
    annotations: Vec<Vec<u8>>,
    message_items: Vec<MessageItem>,
    reasoning_active: bool,
    reasoning_deltas_done: bool,
    reasoning_item_id: Vec<u8>,
    reasoning: Vec<u8>,
    reasoning_signature: Vec<u8>,
    reasoning_index: i64,
    reasoning_items: Vec<ReasoningItem>,
    web_by_block: HashMap<i64, usize>,
    web_by_tool: HashMap<Vec<u8>, usize>,
    web: Vec<WebSearch>,
    stop_reason: Vec<u8>,
    usage: Usage,
}

pub fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    let request = pick_request(ctx.original_request, ctx.translated_request).to_vec();
    Box::new(State {
        model: ctx.model.as_bytes().to_vec(),
        original: ctx.original_request.to_vec(),
        translated: ctx.translated_request.to_vec(),
        tools: Tools::new(&request),
        request,
        completed: false,
        tool_error: false,
        patch_calls: HashMap::new(),
        func_input_snapshot: HashMap::new(),
        func_snapshot_errors: HashSet::new(),
        func_identity_conflicts: HashSet::new(),
        seq: 0,
        response_id: vec![],
        created_at: 0,
        next_output_index: 0,
        current_msg_id: vec![],
        current_fc_id: vec![],
        in_text_block: false,
        in_func_block: false,
        message_open: false,
        content_part_open: false,
        message_output_index: -1,
        func_item_added: HashSet::new(),
        func_args_sent: HashMap::new(),
        func_block_stopped: HashSet::new(),
        func_args: BTreeMap::new(),
        func_args_done: HashSet::new(),
        func_item_done: HashSet::new(),
        func_item_status: HashMap::new(),
        func_names: HashMap::new(),
        func_call_ids: BTreeMap::new(),
        func_custom: HashMap::new(),
        func_output_indices: HashMap::new(),
        text: vec![],
        annotations: vec![],
        message_items: vec![],
        reasoning_active: false,
        reasoning_deltas_done: false,
        reasoning_item_id: vec![],
        reasoning: vec![],
        reasoning_signature: vec![],
        reasoning_index: -1,
        reasoning_items: vec![],
        web_by_block: HashMap::new(),
        web_by_tool: HashMap::new(),
        web: vec![],
        stop_reason: vec![],
        usage: Usage::default(),
    })
}

fn event(name: &str, payload: &[u8]) -> Vec<u8> {
    sse_event(name, payload)
}

impl State {
    fn next_seq(&mut self) -> i64 {
        self.seq += 1;
        self.seq
    }

    fn allocate(&mut self) -> i64 {
        let i = self.next_output_index;
        self.next_output_index += 1;
        i
    }

    fn message_output_index(&mut self) -> i64 {
        if self.message_output_index < 0 {
            self.message_output_index = self.allocate();
        }
        self.message_output_index
    }

    fn function_output_index(&mut self, idx: i64) -> i64 {
        if let Some(&i) = self.func_output_indices.get(&idx) {
            return i;
        }
        let i = self.allocate();
        self.func_output_indices.insert(idx, i);
        i
    }

    fn call_id(&self, idx: i64) -> Vec<u8> {
        self.func_call_ids.get(&idx).cloned().unwrap_or_default()
    }

    fn name(&self, idx: i64) -> Vec<u8> {
        self.func_names.get(&idx).cloned().unwrap_or_default()
    }

    fn finalize_web_search(&mut self, i: usize, status: &'static str) -> Vec<Vec<u8>> {
        if self.web[i].emitted {
            return vec![];
        }
        self.web[i].emitted = true;
        self.web[i].status = status;
        let mut done =
            br#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{}}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut done, "sequence_number", seq);
        gj::set_int(&mut done, "output_index", self.web[i].output_index);
        let mut rendered = self.web[i].render();
        gj::set_str(&mut rendered, "status", status);
        gj::set_raw(&mut done, "item", rendered);
        vec![event("response.output_item.done", &done)]
    }

    /// failToolInput: the first failure ends the response with `response.failed`.
    fn fail_tool_input(&mut self) -> Vec<Vec<u8>> {
        if self.tool_error {
            return vec![];
        }
        self.tool_error = true;
        let seq = self.next_seq();
        vec![event(
            "response.failed",
            &crate::apply_patch::failure(&self.response_id, seq),
        )]
    }

    fn emit_func_item(&mut self, idx: i64, force: bool) -> Result<Vec<Vec<u8>>, Error> {
        if self.func_item_added.contains(&idx) || self.tool_error {
            return Ok(vec![]);
        }
        let mut name = self.name(idx);
        let mut call_id = self.call_id(idx);
        if force && name.is_empty() && self.tools.winners.len() == 1 {
            let identities: Vec<Vec<u8>> = self.tools.winners.keys().cloned().collect();
            for identity in identities {
                if self.tools.is_apply_patch(&identity) {
                    name = self.tools.names.claude_name(&identity);
                    self.func_names.insert(idx, name.clone());
                }
            }
        }
        if self.tools.is_apply_patch(&name)
            && (self.func_identity_conflicts.contains(&idx) || self.func_snapshot_errors.contains(&idx))
        {
            return Ok(self.fail_tool_input());
        }
        if !force && (name.is_empty() || call_id.is_empty()) {
            return Ok(vec![]);
        }
        if call_id.is_empty() {
            call_id = format!("call_{}_{idx}", String::from_utf8_lossy(&self.response_id)).into_bytes();
            self.func_call_ids.insert(idx, call_id.clone());
        }
        let custom = self.tools.winner(&name).is_some_and(|w| w.custom);
        self.func_custom.insert(idx, custom);
        let output_index = self.function_output_index(idx);
        let (mut item, prefix): (Vec<u8>, &[u8]) = if custom {
            (br#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"custom_tool_call","status":"in_progress","input":"","call_id":"","name":""}}"#.to_vec(), b"ctc_")
        } else {
            (br#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"function_call","status":"in_progress","arguments":"","call_id":"","name":""}}"#.to_vec(), b"fc_")
        };
        let item_id = [prefix, &call_id].concat();
        if custom && self.tools.is_apply_patch(&name) {
            let winner = self.tools.winner(&name);
            let call = CallState {
                item_id: item_id.clone(),
                call_id: call_id.clone(),
                name: winner.map(|w| w.local_name.clone()).unwrap_or_default(),
                namespace: winner.map(|w| w.namespace.clone()).unwrap_or_default(),
                output_index,
                ..CallState::default()
            };
            self.patch_calls.insert(idx, call);
        }
        gj::set_str(&mut item, "item.id", &item_id);
        gj::set_str(&mut item, "item.call_id", &call_id);
        item = with_identity(item, &self.request, &name, "item");
        let seq = self.next_seq();
        gj::set_int(&mut item, "sequence_number", seq);
        gj::set_int(&mut item, "output_index", output_index);
        self.func_item_added.insert(idx);
        Ok(vec![event("response.output_item.added", &item)])
    }

    fn emit_pending_args(&mut self, idx: i64) -> Vec<Vec<u8>> {
        if !self.func_item_added.contains(&idx) || self.tool_error {
            return vec![];
        }
        let sent = self.func_args_sent.get(&idx).copied().unwrap_or(0);
        let Some(buf) = self.func_args.get(&idx).filter(|b| b.len() > sent) else {
            return vec![];
        };
        let fragment = buf[sent..].to_vec();
        self.func_args_sent.insert(idx, buf.len());
        if self.func_custom.get(&idx).copied().unwrap_or(false) {
            let Some(call) = self.patch_calls.get_mut(&idx) else {
                return vec![];
            };
            return match call.push_arguments(&fragment) {
                Err(_) => self.fail_tool_input(),
                Ok(delta) if delta.is_empty() => vec![],
                Ok(delta) => {
                    let seq = self.next_seq();
                    let payload = self.patch_calls[&idx].input_delta(&delta, seq);
                    vec![event("response.custom_tool_call_input.delta", &payload)]
                }
            };
        }
        let mut msg = br#"{"type":"response.function_call_arguments.delta","sequence_number":0,"item_id":"","output_index":0,"delta":""}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut msg, "sequence_number", seq);
        gj::set_str(&mut msg, "item_id", [&b"fc_"[..], &self.call_id(idx)].concat());
        let output_index = self.function_output_index(idx);
        gj::set_int(&mut msg, "output_index", output_index);
        gj::set_str_no_html(&mut msg, "delta", fragment);
        vec![event("response.function_call_arguments.delta", &msg)]
    }

    fn finalize_func_item(&mut self, idx: i64, status: &'static str) -> Result<Vec<Vec<u8>>, Error> {
        if self.func_item_done.contains(&idx) || self.tool_error {
            return Ok(vec![]);
        }
        let mut out = self.emit_func_item(idx, true)?;
        out.extend(self.emit_pending_args(idx));
        if self.tool_error {
            return Ok(out);
        }
        self.func_item_done.insert(idx);
        self.func_item_status.insert(idx, status);
        let output_index = self.function_output_index(idx);
        let custom = self.func_custom.get(&idx).copied().unwrap_or(false);
        let mut args = self.func_args.get(&idx).cloned().unwrap_or_default();
        if !custom && args.is_empty() && status == "completed" {
            args = b"{}".to_vec();
        }
        let mut call_id = self.call_id(idx);
        if call_id.is_empty() {
            call_id = self.current_fc_id.clone();
        }
        let name = self.name(idx);
        let id = [&b"ctc_"[..], &call_id].concat();
        let mut input = vec![];
        if custom {
            let snapshot = self.func_input_snapshot.get(&idx).cloned().unwrap_or_default();
            if let Some(call) = self.patch_calls.get_mut(&idx) {
                let (tail, full) = match finish_patch_arguments(call, &args, &snapshot) {
                    Ok(done) => done,
                    Err(_) => {
                        out.extend(self.fail_tool_input());
                        return Ok(out);
                    }
                };
                input = full.into_bytes();
                if !tail.is_empty() {
                    let seq = self.next_seq();
                    let payload = self.patch_calls[&idx].input_delta(&tail, seq);
                    out.push(event("response.custom_tool_call_input.delta", &payload));
                }
            } else {
                input = unwrap_custom_tool_input(&args);
            }
        }
        let first_done = self.func_args_done.insert(idx);
        if custom {
            if first_done && self.patch_calls.contains_key(&idx) {
                let seq = self.next_seq();
                let payload = self.patch_calls[&idx].input_done(&String::from_utf8_lossy(&input), seq);
                out.push(event("response.custom_tool_call_input.done", &payload));
            } else if first_done {
                let mut done = br#"{"type":"response.custom_tool_call_input.done","sequence_number":0,"item_id":"","output_index":0,"input":""}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut done, "sequence_number", seq);
                gj::set_str(&mut done, "item_id", &id);
                gj::set_int(&mut done, "output_index", output_index);
                gj::set_str(&mut done, "input", &input);
                out.push(event("response.custom_tool_call_input.done", &done));
            }
            let mut item = br#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"custom_tool_call","status":"completed","input":"","call_id":"","name":""}}"#.to_vec();
            let seq = self.next_seq();
            gj::set_int(&mut item, "sequence_number", seq);
            gj::set_int(&mut item, "output_index", output_index);
            gj::set_str(&mut item, "item.id", &id);
            gj::set_str(&mut item, "item.status", status);
            gj::set_str(&mut item, "item.input", &input);
            gj::set_str(&mut item, "item.call_id", &call_id);
            item = with_identity(item, &self.request, &name, "item");
            out.push(event("response.output_item.done", &item));
        } else {
            let id = [&b"fc_"[..], &call_id].concat();
            if first_done {
                let mut done = br#"{"type":"response.function_call_arguments.done","sequence_number":0,"item_id":"","output_index":0,"arguments":""}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut done, "sequence_number", seq);
                gj::set_str(&mut done, "item_id", &id);
                gj::set_int(&mut done, "output_index", output_index);
                gj::set_str_no_html(&mut done, "arguments", &args);
                out.push(event("response.function_call_arguments.done", &done));
            }
            let mut item = br#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"function_call","status":"completed","arguments":"","call_id":"","name":""}}"#.to_vec();
            let seq = self.next_seq();
            gj::set_int(&mut item, "sequence_number", seq);
            gj::set_int(&mut item, "output_index", output_index);
            gj::set_str(&mut item, "item.id", &id);
            gj::set_str(&mut item, "item.status", status);
            gj::set_str_no_html(&mut item, "item.arguments", &args);
            gj::set_str(&mut item, "item.call_id", &call_id);
            item = with_identity(item, &self.request, &name, "item");
            out.push(event("response.output_item.done", &item));
        }
        self.in_func_block = false;
        Ok(out)
    }

    fn finalize_reasoning_deltas(&mut self) -> Vec<Vec<u8>> {
        if !self.reasoning_active || self.reasoning_deltas_done {
            return vec![];
        }
        self.reasoning_deltas_done = true;
        let full = self.reasoning.clone();
        let mut text = br#"{"type":"response.reasoning_summary_text.done","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"text":""}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut text, "sequence_number", seq);
        gj::set_str(&mut text, "item_id", &self.reasoning_item_id);
        gj::set_int(&mut text, "output_index", self.reasoning_index);
        gj::set_str(&mut text, "text", &full);
        let mut part = br#"{"type":"response.reasoning_summary_part.done","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut part, "sequence_number", seq);
        gj::set_str(&mut part, "item_id", &self.reasoning_item_id);
        gj::set_int(&mut part, "output_index", self.reasoning_index);
        gj::set_str(&mut part, "part.text", &full);
        vec![
            event("response.reasoning_summary_text.done", &text),
            event("response.reasoning_summary_part.done", &part),
        ]
    }

    fn finalize_reasoning_item(&mut self, status: &'static str) -> Vec<Vec<u8>> {
        if !self.reasoning_active && self.reasoning_item_id.is_empty() {
            return vec![];
        }
        let mut out = self.finalize_reasoning_deltas();
        let full = self.reasoning.clone();
        let mut item = br#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"reasoning","status":"completed","encrypted_content":"","summary":[]}}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut item, "sequence_number", seq);
        gj::set_int(&mut item, "output_index", self.reasoning_index);
        gj::set_str(&mut item, "item.id", &self.reasoning_item_id);
        gj::set_str(&mut item, "item.status", status);
        gj::set_str(&mut item, "item.encrypted_content", &self.reasoning_signature);
        let mut summary = br#"{"type":"summary_text","text":""}"#.to_vec();
        gj::set_str(&mut summary, "text", &full);
        gj::set_items(&mut item, "item.summary", &[summary]);
        out.push(event("response.output_item.done", &item));
        self.reasoning_items.push(ReasoningItem {
            id: std::mem::take(&mut self.reasoning_item_id),
            output_index: self.reasoning_index,
            text: full,
            signature: std::mem::take(&mut self.reasoning_signature),
            status,
        });
        self.reasoning_active = false;
        self.reasoning.clear();
        self.reasoning_index = -1;
        out
    }

    fn finalize_message(&mut self) -> Vec<Vec<u8>> {
        if !self.message_open {
            return vec![];
        }
        let full = self.text.clone();
        let output_index = self.message_output_index();
        let status = output_status(&self.stop_reason);
        let annotations = annotations_json(&self.annotations);
        let mut done = br#"{"type":"response.output_text.done","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"text":"","logprobs":[]}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut done, "sequence_number", seq);
        gj::set_str(&mut done, "item_id", &self.current_msg_id);
        gj::set_int(&mut done, "output_index", output_index);
        gj::set_str(&mut done, "text", &full);
        let mut part = br#"{"type":"response.content_part.done","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut part, "sequence_number", seq);
        gj::set_str(&mut part, "item_id", &self.current_msg_id);
        gj::set_int(&mut part, "output_index", output_index);
        gj::set_str(&mut part, "part.text", &full);
        if !self.annotations.is_empty() {
            gj::set_raw(&mut part, "part.annotations", &annotations);
        }
        let mut item = br#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":""}],"role":"assistant"}}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut item, "sequence_number", seq);
        gj::set_int(&mut item, "output_index", output_index);
        gj::set_str(&mut item, "item.id", &self.current_msg_id);
        gj::set_str(&mut item, "item.status", status);
        gj::set_str(&mut item, "item.content.0.text", &full);
        if !self.annotations.is_empty() {
            gj::set_raw(&mut item, "item.content.0.annotations", &annotations);
        }
        self.message_items.push(MessageItem {
            id: std::mem::take(&mut self.current_msg_id),
            output_index,
            text: full,
            annotations: std::mem::take(&mut self.annotations),
            status,
        });
        self.in_text_block = false;
        self.message_open = false;
        self.content_part_open = false;
        self.message_output_index = -1;
        self.text.clear();
        vec![
            event("response.output_text.done", &done),
            event("response.content_part.done", &part),
            event("response.output_item.done", &item),
        ]
    }

    fn reset(&mut self) {
        self.text.clear();
        self.annotations.clear();
        self.message_items.clear();
        self.reasoning.clear();
        self.reasoning_active = false;
        self.reasoning_deltas_done = false;
        self.next_output_index = 0;
        self.in_text_block = false;
        self.in_func_block = false;
        self.message_open = false;
        self.content_part_open = false;
        self.current_msg_id.clear();
        self.current_fc_id.clear();
        self.message_output_index = -1;
        self.reasoning_item_id.clear();
        self.reasoning_signature.clear();
        self.reasoning_index = -1;
        self.reasoning_items.clear();
        self.stop_reason.clear();
        self.func_item_added.clear();
        self.func_args_sent.clear();
        self.func_block_stopped.clear();
        self.func_args.clear();
        self.func_args_done.clear();
        self.func_item_done.clear();
        self.func_item_status.clear();
        self.func_names.clear();
        self.func_call_ids.clear();
        self.func_custom.clear();
        self.func_output_indices.clear();
        self.patch_calls.clear();
        self.func_input_snapshot.clear();
        self.func_snapshot_errors.clear();
        self.func_identity_conflicts.clear();
        self.usage = Usage::default();
    }

    fn convert(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        if self.completed || self.tool_error {
            return Ok(vec![]);
        }
        let Some(rest) = line.strip_prefix(b"data:") else {
            return Ok(vec![]);
        };
        let payload = trim_space(rest).to_vec();
        let root = gj::parse(&payload);
        let mut out = vec![];
        match &*root.get("type").bytes() {
            b"message_start" => {
                let msg = root.get("message");
                if !msg.exists() {
                    return Ok(out);
                }
                self.response_id = msg.get("id").bytes().into_owned();
                self.created_at = now_unix();
                self.reset();
                self.usage.merge(&msg.get("usage"));
                let mut model = common::request_model_name(&self.original, &self.translated);
                if model.is_empty() {
                    model = self.model.clone();
                }
                for (name, template) in [
                    ("response.created", &br#"{"type":"response.created","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"in_progress","background":false,"error":null,"output":[]}}"#[..]),
                    ("response.in_progress", br#"{"type":"response.in_progress","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"in_progress","output":[]}}"#),
                ] {
                    let mut ev = template.to_vec();
                    let seq = self.next_seq();
                    gj::set_int(&mut ev, "sequence_number", seq);
                    gj::set_str(&mut ev, "response.id", &self.response_id);
                    gj::set_int(&mut ev, "response.created_at", self.created_at);
                    if !model.is_empty() {
                        gj::set_str(&mut ev, "response.model", &model);
                    }
                    out.push(event(name, &ev));
                }
            }
            b"content_block_start" => self.block_start(&root, &mut out)?,
            b"content_block_delta" => {
                let d = root.get("delta");
                if !d.exists() {
                    return Ok(out);
                }
                match &*d.get("type").bytes() {
                    b"text_delta" => {
                        let t = d.get("text");
                        if t.exists() {
                            let text = t.bytes().into_owned();
                            let mut msg = br#"{"type":"response.output_text.delta","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"delta":"","logprobs":[]}"#.to_vec();
                            let seq = self.next_seq();
                            gj::set_int(&mut msg, "sequence_number", seq);
                            gj::set_str(&mut msg, "item_id", &self.current_msg_id);
                            let index = self.message_output_index();
                            gj::set_int(&mut msg, "output_index", index);
                            gj::set_str(&mut msg, "delta", &text);
                            out.push(event("response.output_text.delta", &msg));
                            self.text.extend_from_slice(&text);
                        }
                    }
                    b"input_json_delta" => {
                        let idx = root.get("index").int();
                        let pj = d.get("partial_json");
                        if let Some(&w) = self.web_by_block.get(&idx) {
                            if pj.exists() {
                                self.web[w].input.extend_from_slice(&pj.bytes());
                            }
                            return Ok(vec![]);
                        }
                        if pj.exists() {
                            self.func_args.entry(idx).or_default().extend_from_slice(&pj.bytes());
                            out.extend(self.emit_pending_args(idx));
                        }
                    }
                    b"thinking_delta" => {
                        let t = d.get("thinking");
                        if self.reasoning_active && t.exists() {
                            let text = t.bytes().into_owned();
                            self.reasoning.extend_from_slice(&text);
                            let mut msg = br#"{"type":"response.reasoning_summary_text.delta","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"delta":""}"#.to_vec();
                            let seq = self.next_seq();
                            gj::set_int(&mut msg, "sequence_number", seq);
                            gj::set_str(&mut msg, "item_id", &self.reasoning_item_id);
                            gj::set_int(&mut msg, "output_index", self.reasoning_index);
                            gj::set_str(&mut msg, "delta", &text);
                            out.push(event("response.reasoning_summary_text.delta", &msg));
                        }
                    }
                    b"signature_delta" => {
                        let sig = d.get("signature").bytes();
                        if self.reasoning_active && !sig.is_empty() {
                            self.reasoning_signature = sig.into_owned();
                        }
                        return Ok(vec![]);
                    }
                    b"citations_delta" => {
                        let citation = d.get("citation");
                        if citation.exists()
                            && let Some(v) = citation.value_json()
                        {
                            self.annotations.push(v);
                        }
                        return Ok(vec![]);
                    }
                    _ => {}
                }
            }
            b"content_block_stop" => {
                self.func_block_stopped.insert(root.get("index").int());
                if self.in_text_block {
                    self.in_text_block = false;
                } else if self.in_func_block {
                    self.in_func_block = false;
                } else if self.reasoning_active {
                    out.extend(self.finalize_reasoning_deltas());
                }
            }
            b"message_delta" => {
                self.usage.merge(&root.get("usage"));
                let stop = root.get("delta.stop_reason");
                if stop.exists() {
                    self.stop_reason = stop.bytes().into_owned();
                }
                return Ok(vec![]);
            }
            b"message_stop" => self.message_stop(&mut out)?,
            _ => {}
        }
        Ok(out)
    }

    fn block_start(&mut self, root: &Res<'_>, out: &mut Vec<Vec<u8>>) -> Result<(), Error> {
        let cb = root.get("content_block");
        if !cb.exists() {
            return Ok(());
        }
        let idx = root.get("index").int();
        let kind = cb.get("type").bytes().into_owned();
        if kind != b"text" {
            out.extend(self.finalize_message());
        }
        if self.reasoning_active || !self.reasoning_item_id.is_empty() {
            out.extend(self.finalize_reasoning_item("completed"));
        }
        let pending: Vec<i64> = self.func_call_ids.keys().copied().collect();
        for prev in pending {
            if self.func_item_done.contains(&prev) || prev == idx {
                continue;
            }
            let name = self.name(prev);
            if (self.tools.is_apply_patch(&name) || name.is_empty()) && !self.func_block_stopped.contains(&prev) {
                continue;
            }
            out.extend(self.finalize_func_item(prev, "completed")?);
            if self.tool_error {
                return Ok(());
            }
        }
        for i in 0..self.web.len() {
            if !self.web[i].emitted && self.web[i].results.is_some() {
                out.extend(self.finalize_web_search(i, "completed"));
            }
        }
        match kind.as_slice() {
            b"text" => {
                self.in_text_block = true;
                let output_index = self.message_output_index();
                if self.current_msg_id.is_empty() {
                    self.current_msg_id = format!(
                        "msg_{}_{}",
                        String::from_utf8_lossy(&self.response_id),
                        self.message_items.len()
                    )
                    .into_bytes();
                }
                if !self.message_open {
                    let mut item = br#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"message","status":"in_progress","content":[],"role":"assistant"}}"#.to_vec();
                    let seq = self.next_seq();
                    gj::set_int(&mut item, "sequence_number", seq);
                    gj::set_int(&mut item, "output_index", output_index);
                    gj::set_str(&mut item, "item.id", &self.current_msg_id);
                    out.push(event("response.output_item.added", &item));
                    self.message_open = true;
                }
                if !self.content_part_open {
                    let mut part = br#"{"type":"response.content_part.added","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}"#.to_vec();
                    let seq = self.next_seq();
                    gj::set_int(&mut part, "sequence_number", seq);
                    gj::set_str(&mut part, "item_id", &self.current_msg_id);
                    gj::set_int(&mut part, "output_index", output_index);
                    out.push(event("response.content_part.added", &part));
                    self.content_part_open = true;
                }
            }
            b"tool_use" => {
                self.in_func_block = true;
                let call_id = cb.get("id").bytes().into_owned();
                let name = cb.get("name").bytes().into_owned();
                let old_id = self.call_id(idx);
                let old_name = self.name(idx);
                // Pending identity evidence must survive later matching updates.
                if !call_id.is_empty() && !old_id.is_empty() && call_id != old_id {
                    self.func_identity_conflicts.insert(idx);
                }
                if (self.tools.is_apply_patch(&old_name) || self.tools.is_apply_patch(&name))
                    && (self.func_identity_conflicts.contains(&idx) || self.tools.conflicting(&name, &old_name))
                {
                    out.extend(self.fail_tool_input());
                    return Ok(());
                }
                if !self.func_item_added.contains(&idx) && (!call_id.is_empty() || old_id.is_empty()) {
                    self.func_call_ids.insert(idx, call_id);
                }
                if !name.is_empty() && !self.func_item_added.contains(&idx) {
                    self.func_names.insert(idx, name);
                }
                self.current_fc_id = self.call_id(idx);
                self.function_output_index(idx);
                self.func_args.entry(idx).or_default();
                if let Some(input) = input_snapshot(&cb) {
                    let previous = self.func_input_snapshot.get(&idx).cloned().unwrap_or_default();
                    if validate_patch_snapshots(&previous, &input.raw).is_err() {
                        self.func_snapshot_errors.insert(idx);
                    }
                    // A finished call compares late snapshots without emitting more input.
                    if self.func_item_done.contains(&idx)
                        && let Some(call) = self.patch_calls.get_mut(&idx)
                        && call.finish_arguments(&input.raw).is_err()
                    {
                        out.extend(self.fail_tool_input());
                        return Ok(());
                    }
                    self.func_input_snapshot.insert(idx, input.raw.to_vec());
                }
                if self.tools.is_apply_patch(&self.name(idx)) && self.func_snapshot_errors.contains(&idx) {
                    out.extend(self.fail_tool_input());
                    return Ok(());
                }
                out.extend(self.emit_func_item(idx, false)?);
                out.extend(self.emit_pending_args(idx));
            }
            b"server_tool_use" => {
                if cb.get("name").str() == "web_search" {
                    let tool_use_id = cb.get("id").bytes().into_owned();
                    let output_index = self.allocate();
                    self.web.push(WebSearch {
                        tool_use_id: tool_use_id.clone(),
                        output_index,
                        input: vec![],
                        results: None,
                        emitted: false,
                        status: "",
                    });
                    let i = self.web.len() - 1;
                    self.web_by_block.insert(idx, i);
                    self.web_by_tool.insert(tool_use_id.clone(), i);
                    let mut added = br#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"web_search_call","status":"in_progress","action":{"type":"search","query":""}}}"#.to_vec();
                    let seq = self.next_seq();
                    gj::set_int(&mut added, "sequence_number", seq);
                    gj::set_int(&mut added, "output_index", output_index);
                    gj::set_str(&mut added, "item.id", web_search_call_id(&tool_use_id));
                    out.push(event("response.output_item.added", &added));
                }
            }
            b"web_search_tool_result" => {
                if let Some(&i) = self.web_by_tool.get(&*cb.get("tool_use_id").bytes()) {
                    self.web[i].results = web_search_results(&cb.get("content"));
                }
            }
            b"thinking" | b"redacted_thinking" => {
                self.reasoning_active = true;
                self.reasoning_deltas_done = false;
                self.reasoning_index = self.allocate();
                self.reasoning.clear();
                self.reasoning_signature = reasoning_carrier(&cb);
                self.reasoning_item_id =
                    format!("rs_{}_{idx}", String::from_utf8_lossy(&self.response_id)).into_bytes();
                let mut item = br#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"reasoning","status":"in_progress","encrypted_content":"","summary":[]}}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut item, "sequence_number", seq);
                gj::set_int(&mut item, "output_index", self.reasoning_index);
                gj::set_str(&mut item, "item.id", &self.reasoning_item_id);
                gj::set_str(&mut item, "item.encrypted_content", &self.reasoning_signature);
                out.push(event("response.output_item.added", &item));
                let mut part = br#"{"type":"response.reasoning_summary_part.added","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut part, "sequence_number", seq);
                gj::set_str(&mut part, "item_id", &self.reasoning_item_id);
                gj::set_int(&mut part, "output_index", self.reasoning_index);
                out.push(event("response.reasoning_summary_part.added", &part));
            }
            _ => {}
        }
        Ok(())
    }

    fn message_stop(&mut self, out: &mut Vec<Vec<u8>>) -> Result<(), Error> {
        let status = output_status(&self.stop_reason);
        if self.reasoning_active || !self.reasoning_item_id.is_empty() {
            out.extend(self.finalize_reasoning_item(status));
        }
        out.extend(self.finalize_message());
        let pending: Vec<i64> = self.func_call_ids.keys().copied().collect();
        for idx in pending {
            if !self.func_item_done.contains(&idx) {
                out.extend(self.finalize_func_item(idx, status)?);
                if self.tool_error {
                    return Ok(());
                }
            }
        }
        for i in 0..self.web.len() {
            if !self.web[i].emitted {
                out.extend(self.finalize_web_search(i, status));
            }
        }
        let event_type = if incomplete(&self.stop_reason) {
            "response.incomplete"
        } else {
            "response.completed"
        };
        let mut completed = br#"{"type":"","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"","background":false,"error":null}}"#.to_vec();
        gj::set_str(&mut completed, "type", event_type);
        let seq = self.next_seq();
        gj::set_int(&mut completed, "sequence_number", seq);
        gj::set_str(&mut completed, "response.id", &self.response_id);
        gj::set_int(&mut completed, "response.created_at", self.created_at);
        gj::set_str(&mut completed, "response.status", status);
        if incomplete(&self.stop_reason) {
            gj::set_raw(
                &mut completed,
                "response.incomplete_details",
                r#"{"reason":"max_output_tokens"}"#,
            );
        }
        copy_request_fields(&mut completed, &self.request, "response.", Echo::default());
        let mut outputs = br#"{"arr":[]}"#.to_vec();
        for r in &self.reasoning_items {
            let mut item =
                br#"{"id":"","type":"reasoning","status":"completed","encrypted_content":"","summary":[]}"#.to_vec();
            gj::set_str(&mut item, "id", &r.id);
            gj::set_str(
                &mut item,
                "status",
                if r.status.is_empty() { "completed" } else { r.status },
            );
            gj::set_str(&mut item, "encrypted_content", &r.signature);
            let mut summary = br#"{"type":"summary_text","text":""}"#.to_vec();
            gj::set_str(&mut summary, "text", &r.text);
            gj::set_items(&mut item, "summary", &[summary]);
            gj::set_raw(&mut outputs, &format!("arr.{}", r.output_index), item);
        }
        for m in &self.message_items {
            let mut item = br#"{"id":"","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":""}],"role":"assistant"}"#.to_vec();
            gj::set_str(&mut item, "id", &m.id);
            gj::set_str(&mut item, "status", m.status);
            gj::set_str(&mut item, "content.0.text", &m.text);
            if !m.annotations.is_empty() {
                gj::set_raw(&mut item, "content.0.annotations", annotations_json(&m.annotations));
            }
            gj::set_raw(&mut outputs, &format!("arr.{}", m.output_index), item);
        }
        for w in &self.web {
            let mut rendered = w.render();
            gj::set_str(
                &mut rendered,
                "status",
                if w.status.is_empty() { "completed" } else { w.status },
            );
            gj::set_raw(&mut outputs, &format!("arr.{}", w.output_index), rendered);
        }
        for (&idx, buf) in &self.func_args {
            let status = self.func_item_status.get(&idx).copied().unwrap_or("completed");
            let custom = self.func_custom.get(&idx).copied().unwrap_or(false);
            let mut args: Vec<u8> = if !custom && status == "completed" {
                b"{}".to_vec()
            } else {
                vec![]
            };
            if !buf.is_empty() {
                args = buf.clone();
            }
            let mut call_id = self.call_id(idx);
            if call_id.is_empty() && !self.current_fc_id.is_empty() {
                call_id = self.current_fc_id.clone();
            }
            let name = self.name(idx);
            let index = self.func_output_indices.get(&idx).copied().unwrap_or(0);
            let item = if custom {
                let mut item =
                    br#"{"id":"","type":"custom_tool_call","status":"completed","input":"","call_id":"","name":""}"#
                        .to_vec();
                gj::set_str(&mut item, "id", [&b"ctc_"[..], &call_id].concat());
                gj::set_str(&mut item, "status", status);
                let input = match self.patch_calls.get(&idx) {
                    Some(call) => call.decoder.input().as_bytes().to_vec(),
                    None => unwrap_custom_tool_input(&args),
                };
                gj::set_str(&mut item, "input", input);
                gj::set_str(&mut item, "call_id", &call_id);
                with_identity(item, &self.request, &name, "")
            } else {
                let mut item =
                    br#"{"id":"","type":"function_call","status":"completed","arguments":"","call_id":"","name":""}"#
                        .to_vec();
                gj::set_str(&mut item, "id", [&b"fc_"[..], &call_id].concat());
                gj::set_str(&mut item, "status", status);
                gj::set_str_no_html(&mut item, "arguments", &args);
                gj::set_str(&mut item, "call_id", &call_id);
                with_identity(item, &self.request, &name, "")
            };
            gj::set_raw(&mut outputs, &format!("arr.{index}"), item);
        }
        if gj::get(&outputs, "arr.#").int() > 0 {
            let arr = gj::get(&outputs, "arr").raw.to_vec();
            gj::set_raw(&mut completed, "response.output", arr);
        }
        let reasoning_len: usize = self.reasoning_items.iter().map(|r| r.text.len()).sum();
        let reasoning_tokens = (reasoning_len / 4) as i64;
        if self.usage.present || reasoning_tokens > 0 {
            let (input, output, total, cached) = self.usage.totals();
            gj::set_int(&mut completed, "response.usage.input_tokens", input);
            gj::set_int(
                &mut completed,
                "response.usage.input_tokens_details.cached_tokens",
                cached,
            );
            gj::set_int(&mut completed, "response.usage.output_tokens", output);
            gj::set_int(
                &mut completed,
                "response.usage.output_tokens_details.reasoning_tokens",
                reasoning_tokens,
            );
            if total > 0 || self.usage.present {
                gj::set_int(&mut completed, "response.usage.total_tokens", total);
            }
        }
        self.completed = true;
        out.push(event(event_type, &completed));
        Ok(())
    }
}

/// Fallbacks for request fields the echo copies.
#[derive(Default, Clone, Copy)]
pub(crate) struct Echo<'a> {
    /// Fills `model` when the request has none (the upstream's model).
    pub model: Option<&'a [u8]>,
    /// Takes `max_output_tokens` from `max_tokens` (a Chat Completions request).
    pub max_tokens: bool,
}

/// The request echo Go copies into response.completed and the non-stream response.
pub(crate) fn copy_request_fields(out: &mut Vec<u8>, request: &[u8], prefix: &str, echo: Echo<'_>) {
    if request.is_empty() {
        return;
    }
    let req = gj::parse(request);
    let at = |key: &str| format!("{prefix}{key}");
    for key in [
        "instructions",
        "max_output_tokens",
        "max_tool_calls",
        "model",
        "parallel_tool_calls",
        "previous_response_id",
        "prompt_cache_key",
        "reasoning",
        "safety_identifier",
        "service_tier",
        "store",
        "temperature",
        "text",
        "tool_choice",
        "tools",
        "top_logprobs",
        "top_p",
        "truncation",
        "user",
        "metadata",
    ] {
        let mut v = req.get(key);
        if !v.exists() && key == "max_output_tokens" && echo.max_tokens {
            v = req.get("max_tokens");
        }
        if !v.exists() {
            if key == "model"
                && let Some(fallback) = echo.model
            {
                gj::set_str(out, &at(key), fallback);
            }
            continue;
        }
        match key {
            "max_output_tokens" | "max_tool_calls" | "top_logprobs" => {
                gj::set_int(out, &at(key), v.int());
            }
            "parallel_tool_calls" | "store" => {
                gj::set_bool(out, &at(key), v.bool());
            }
            "temperature" | "top_p" => {
                gj::set_f64(out, &at(key), v.float());
            }
            "reasoning" | "text" | "tool_choice" | "tools" | "user" | "metadata" => set_value(out, &at(key), &v),
            _ => {
                gj::set_str(out, &at(key), v.bytes());
            }
        }
    }
}

/// `sjson.SetBytes(out, path, v.Value())`: scalars take sjson's typed paths (strings set
/// with conditional escaping, float64 via FormatFloat); maps and slices go through
/// json.Marshal.
fn set_value(out: &mut Vec<u8>, path: &str, v: &Res<'_>) {
    match v.kind {
        Kind::String => {
            gj::set_str(out, path, &v.s);
        }
        Kind::Number => {
            gj::set_f64(out, path, v.num);
        }
        Kind::True | Kind::False => {
            gj::set_bool(out, path, v.kind == Kind::True);
        }
        Kind::Null => {
            gj::set_raw(out, path, "null");
        }
        // ponytail: Go's sjson leaves the document nil when Marshal fails (NaN inside);
        // that case keeps the previous document here.
        Kind::Json => {
            if let Some(json) = v.value_json() {
                gj::set_raw(out, path, json);
            }
        }
    }
}

impl GoStream for State {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        self.convert(line)
    }

    fn tool_input_failed(&self) -> bool {
        self.tool_error
    }

    /// FinalizeToolInput: a patch-enabled stream that ends before message_stop fails.
    fn finalize_tool_input(&mut self) -> Vec<Vec<u8>> {
        if self.tool_error || self.completed {
            return vec![];
        }
        if !self
            .tools
            .winners
            .keys()
            .any(|identity| self.tools.is_apply_patch(identity))
        {
            return vec![];
        }
        self.tool_error = true;
        let seq = self.next_seq();
        vec![event(
            "response.failed",
            &crate::apply_patch::failure(&self.response_id, seq),
        )]
    }
}

#[derive(Default)]
struct OutputItem {
    kind: &'static str,
    id: Vec<u8>,
    call_id: Vec<u8>,
    name: Vec<u8>,
    text: Vec<u8>,
    signature: Vec<u8>,
    annotations: Vec<Vec<u8>>,
    args: Vec<u8>,
    results: Option<Vec<u8>>,
    input_snapshot: Vec<u8>,
}

/// Buffered Claude SSE to one Responses object. An invalid or conflicting apply_patch
/// call is an error carrying Go's upstream message (Go's executors answer 502 and drop
/// the body).
pub fn non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let request = pick_request(ctx.original_request, ctx.translated_request);
    let tools = Tools::new(request);
    let mut out = br#"{"id":"","object":"response","created_at":0,"status":"completed","background":false,"error":null,"incomplete_details":null,"output":[],"usage":{"input_tokens":0,"input_tokens_details":{"cached_tokens":0},"output_tokens":0,"output_tokens_details":{},"total_tokens":0}}"#.to_vec();
    let (mut response_id, mut created_at, mut stop) = (vec![], 0, vec![]);
    let mut usage = Usage::default();
    let mut items: Vec<OutputItem> = vec![];
    let mut by_block: HashMap<i64, usize> = HashMap::new();
    let mut web_by_tool: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut message_count = 0;
    let mut active_message: Option<usize> = None;
    let mut pending_annotations: Vec<Vec<u8>> = vec![];
    let mut identity_conflicts: HashSet<i64> = HashSet::new();
    let mut snapshot_errors: HashSet<i64> = HashSet::new();
    for line in body.split(|&c| c == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(chunk) = line.strip_prefix(b"data:") else {
            continue;
        };
        let root = gj::parse(chunk);
        let ev = root.get("type").bytes().into_owned();
        if ev == b"message_stop" {
            break;
        }
        let new_item =
            |items: &mut Vec<OutputItem>, by_block: &mut HashMap<i64, usize>, kind: &'static str, idx: i64| {
                items.push(OutputItem {
                    kind,
                    ..OutputItem::default()
                });
                by_block.insert(idx, items.len() - 1);
                items.len() - 1
            };
        match ev.as_slice() {
            b"message_start" => {
                let msg = root.get("message");
                if msg.exists() {
                    response_id = msg.get("id").bytes().into_owned();
                    created_at = now_unix();
                    usage.merge(&msg.get("usage"));
                }
            }
            b"content_block_start" => {
                let cb = root.get("content_block");
                if !cb.exists() {
                    continue;
                }
                let idx = root.get("index").int();
                let kind = cb.get("type").bytes().into_owned();
                if kind != b"text" {
                    active_message = None;
                }
                match kind.as_slice() {
                    b"text" => {
                        let i = match active_message {
                            Some(i) => {
                                by_block.insert(idx, i);
                                i
                            }
                            None => {
                                let i = new_item(&mut items, &mut by_block, "message", idx);
                                items[i].id = format!("msg_{}_{message_count}", String::from_utf8_lossy(&response_id))
                                    .into_bytes();
                                message_count += 1;
                                i
                            }
                        };
                        items[i].annotations.append(&mut pending_annotations);
                        active_message = Some(i);
                    }
                    b"tool_use" => {
                        let name = cb.get("name").bytes().into_owned();
                        let kind = if tools.winner(&name).is_some_and(|w| w.custom) {
                            "custom_tool_call"
                        } else {
                            "function_call"
                        };
                        let i = match by_block.get(&idx) {
                            Some(&i) => i,
                            None => new_item(&mut items, &mut by_block, kind, idx),
                        };
                        let call_id = cb.get("id").bytes().into_owned();
                        if !call_id.is_empty() && !items[i].call_id.is_empty() && call_id != items[i].call_id {
                            identity_conflicts.insert(idx);
                        }
                        if (tools.is_apply_patch(&items[i].name) || tools.is_apply_patch(&name))
                            && (identity_conflicts.contains(&idx) || tools.conflicting(&name, &items[i].name))
                        {
                            return Err(patch_failure());
                        }
                        if !name.is_empty() {
                            items[i].name = name;
                            items[i].kind = kind;
                        }
                        if !call_id.is_empty() {
                            items[i].call_id = call_id;
                        }
                        if let Some(input) = input_snapshot(&cb) {
                            if validate_patch_snapshots(&items[i].input_snapshot, &input.raw).is_err() {
                                snapshot_errors.insert(idx);
                            }
                            items[i].input_snapshot = input.raw.to_vec();
                        }
                        if tools.is_apply_patch(&items[i].name) && snapshot_errors.contains(&idx) {
                            return Err(patch_failure());
                        }
                        let prefix: &[u8] = if items[i].kind == "custom_tool_call" {
                            b"ctc_"
                        } else {
                            b"fc_"
                        };
                        items[i].id = [prefix, &items[i].call_id].concat();
                    }
                    b"server_tool_use" => {
                        if cb.get("name").str() != "web_search" {
                            continue;
                        }
                        let tool_use_id = cb.get("id").bytes().into_owned();
                        let i = new_item(&mut items, &mut by_block, "web_search_call", idx);
                        items[i].id = web_search_call_id(&tool_use_id);
                        items[i].call_id = tool_use_id.clone();
                        web_by_tool.insert(tool_use_id, i);
                        let input = cb.get("input");
                        if input.is_object() && !web_search_query(&input.raw).is_empty() {
                            items[i].args.extend_from_slice(&input.raw);
                        }
                    }
                    b"web_search_tool_result" => {
                        if let Some(&i) = web_by_tool.get(&*cb.get("tool_use_id").bytes()) {
                            items[i].results = web_search_results(&cb.get("content"));
                        }
                    }
                    b"thinking" | b"redacted_thinking" => {
                        let i = new_item(&mut items, &mut by_block, "reasoning", idx);
                        items[i].id = format!("rs_{}_{idx}", String::from_utf8_lossy(&response_id)).into_bytes();
                        items[i].signature = reasoning_carrier(&cb);
                    }
                    _ => {}
                }
            }
            b"content_block_delta" => {
                let d = root.get("delta");
                if !d.exists() {
                    continue;
                }
                let item = by_block.get(&root.get("index").int()).copied();
                let kind = item.map(|i| items[i].kind);
                match &*d.get("type").bytes() {
                    b"text_delta" if kind == Some("message") => {
                        let t = d.get("text");
                        if t.exists() {
                            items[item.unwrap()].text.extend_from_slice(&t.bytes());
                        }
                    }
                    b"input_json_delta"
                        if matches!(kind, Some("function_call" | "custom_tool_call" | "web_search_call")) =>
                    {
                        let pj = d.get("partial_json");
                        if pj.exists() {
                            items[item.unwrap()].args.extend_from_slice(&pj.bytes());
                        }
                    }
                    b"thinking_delta" if kind == Some("reasoning") => {
                        let t = d.get("thinking");
                        if t.exists() {
                            items[item.unwrap()].text.extend_from_slice(&t.bytes());
                        }
                    }
                    b"signature_delta" if kind == Some("reasoning") => {
                        let sig = d.get("signature").bytes();
                        if !sig.is_empty() {
                            items[item.unwrap()].signature = sig.into_owned();
                        }
                    }
                    b"citations_delta" => {
                        let citation = d.get("citation");
                        if citation.exists()
                            && let Some(v) = citation.value_json()
                        {
                            match (kind, active_message) {
                                (Some("message"), _) => items[item.unwrap()].annotations.push(v),
                                (_, Some(a)) => items[a].annotations.push(v),
                                _ => pending_annotations.push(v),
                            }
                        }
                    }
                    _ => {}
                }
            }
            b"message_delta" => {
                usage.merge(&root.get("usage"));
                let v = root.get("delta.stop_reason");
                if v.exists() {
                    stop = v.bytes().into_owned();
                }
            }
            _ => {}
        }
    }
    let status = output_status(&stop);
    gj::set_str(&mut out, "id", &response_id);
    gj::set_int(&mut out, "created_at", created_at);
    gj::set_str(&mut out, "status", status);
    if incomplete(&stop) {
        gj::set_raw(&mut out, "incomplete_details", r#"{"reason":"max_output_tokens"}"#);
    }
    copy_request_fields(&mut out, request, "", Echo::default());
    let mut outputs = vec![];
    let count = items.len();
    for (i, it) in items.iter().enumerate() {
        let item_status = if status == "incomplete" && i == count - 1 {
            "incomplete"
        } else {
            "completed"
        };
        let item = match it.kind {
            "reasoning" => {
                let mut item =
                    br#"{"id":"","type":"reasoning","status":"completed","encrypted_content":"","summary":[]}"#
                        .to_vec();
                gj::set_str(&mut item, "id", &it.id);
                gj::set_str(&mut item, "status", item_status);
                gj::set_str(&mut item, "encrypted_content", &it.signature);
                let mut summary = br#"{"type":"summary_text","text":""}"#.to_vec();
                gj::set_str(&mut summary, "text", &it.text);
                gj::set_raw(&mut item, "summary", gj::join(&[summary]));
                item
            }
            "web_search_call" => {
                let mut item = web_search_item(&it.call_id, &web_search_query(&it.args), it.results.as_ref());
                gj::set_str(&mut item, "status", item_status);
                item
            }
            "message" => {
                let mut item = br#"{"id":"","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":""}],"role":"assistant"}"#.to_vec();
                gj::set_str(&mut item, "id", &it.id);
                gj::set_str(&mut item, "status", item_status);
                gj::set_str(&mut item, "content.0.text", &it.text);
                if !it.annotations.is_empty() {
                    gj::set_raw(&mut item, "content.0.annotations", annotations_json(&it.annotations));
                }
                item
            }
            "custom_tool_call" => {
                let mut item =
                    br#"{"id":"","type":"custom_tool_call","status":"completed","input":"","call_id":"","name":""}"#
                        .to_vec();
                gj::set_str(&mut item, "id", &it.id);
                gj::set_str(&mut item, "status", item_status);
                let input = if tools.is_apply_patch(&it.name) {
                    let mut call = CallState::default();
                    call.push_arguments(&it.args).map_err(|_| patch_failure())?;
                    let (_, full) =
                        finish_patch_arguments(&mut call, &it.args, &it.input_snapshot).map_err(|_| patch_failure())?;
                    full.into_bytes()
                } else {
                    unwrap_custom_tool_input(&it.args)
                };
                gj::set_str(&mut item, "input", input);
                gj::set_str(&mut item, "call_id", &it.call_id);
                with_identity(item, request, &it.name, "")
            }
            _ => {
                let mut args = it.args.clone();
                if args.is_empty() && item_status == "completed" {
                    args = b"{}".to_vec();
                }
                let mut item =
                    br#"{"id":"","type":"function_call","status":"completed","arguments":"","call_id":"","name":""}"#
                        .to_vec();
                gj::set_str(&mut item, "id", &it.id);
                gj::set_str(&mut item, "status", item_status);
                gj::set_str_no_html(&mut item, "arguments", &args);
                gj::set_str(&mut item, "call_id", &it.call_id);
                with_identity(item, request, &it.name, "")
            }
        };
        outputs.push(item);
    }
    if !outputs.is_empty() {
        gj::set_raw(&mut out, "output", gj::join(&outputs));
    }
    let (input, output, total, cached) = usage.totals();
    for (path, v) in [
        ("usage.input_tokens", input),
        ("usage.input_tokens_details.cached_tokens", cached),
        ("usage.output_tokens", output),
        ("usage.total_tokens", total),
    ] {
        if v != 0 {
            gj::set_int(&mut out, path, v);
        }
    }
    let reasoning_len: usize = items
        .iter()
        .filter(|i| i.kind == "reasoning")
        .map(|i| i.text.len())
        .sum();
    let reasoning_tokens = (reasoning_len / 4) as i64;
    if reasoning_tokens > 0 {
        gj::set_int(
            &mut out,
            "usage.output_tokens_details.reasoning_tokens",
            reasoning_tokens,
        );
    }
    Ok(out)
}
