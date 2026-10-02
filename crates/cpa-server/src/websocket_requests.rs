//! Request normalization and event helpers for the Responses WebSocket
//! (openai_responses_websocket_requests.go, _prewarm.go, _forward.go).
//!
//! Every edit goes through the sjson-compatible editor below, so untouched request bytes
//! reach the executor exactly as the client sent them, as in Go.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use cpa_common::json as gj;
use gjson::Kind;

/// `sjson.SetRawBytes` through `cpa_common::json`; an edit sjson rejects keeps the input.
pub(crate) fn set_raw(json: &str, path: &str, raw: &str) -> String {
    edited(json, |out| gj::set_raw(out, path, raw))
}

/// `sjson.SetBytes` with a string value.
pub(crate) fn set_str(json: &str, path: &str, value: &str) -> String {
    edited(json, |out| gj::set_str(out, path, value))
}

/// `sjson.DeleteBytes`.
pub(crate) fn delete(json: &str, path: &str) -> String {
    edited(json, |out| gj::delete(out, path))
}

fn edited(json: &str, edit: impl FnOnce(&mut Vec<u8>) -> bool) -> String {
    let mut out = json.as_bytes().to_vec();
    if !edit(&mut out) {
        return json.to_owned();
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// encoding/json string encoding (`json.Marshal` escapes HTML).
fn go_quote(s: &str, escape_html: bool) -> String {
    let mut out = Vec::with_capacity(s.len() + 2);
    gj::marshal_str(&mut out, s.as_bytes(), escape_html);
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

pub(crate) const CREATE: &str = "response.create";
pub(crate) const APPEND: &str = "response.append";

/// `codexLocalCompactionSummaryPrefix`.
const COMPACTION_SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

/// `responsesWebsocketPreviousResponseNotFoundError`.
pub(crate) const PREVIOUS_NOT_FOUND: &str = r#"{"error":{"message":"Previous response is not available on this websocket; resend the full conversation input without previous_response_id","type":"invalid_request_error","code":"previous_response_not_found","param":"previous_response_id"}}"#;

/// `interfaces.ErrorMessage` for handler-local failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WsError {
    pub status: u16,
    pub message: String,
}

impl WsError {
    pub fn bad(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            message: message.into(),
        }
    }

    pub fn previous_not_found() -> Self {
        Self {
            status: 409,
            message: PREVIOUS_NOT_FOUND.into(),
        }
    }
}

fn requires_array() -> WsError {
    WsError::bad("websocket request requires array field: input")
}

fn text(v: &gjson::Value<'_>) -> String {
    v.str().trim().to_owned()
}

pub(crate) fn field(json: &str, path: &str) -> String {
    text(&gjson::get(json, path))
}

fn is_array(v: &gjson::Value<'_>) -> bool {
    v.kind() == Kind::Array
}

/// `sjson.SetBytes(json, "stream", true)`.
fn stream_true(json: &str) -> String {
    set_raw(json, "stream", "true")
}

/// `normalizeResponsesWebsocketRequestWithIncrementalState`. Returns the request to
/// execute and the transcript to remember for the next turn.
pub(crate) fn normalize(
    raw: &str,
    last_request: &str,
    last_output: &str,
    last_id: &str,
    pending: &[String],
    allow_incremental: bool,
    allow_bypass: bool,
) -> Result<(String, String), WsError> {
    match field(raw, "type").as_str() {
        CREATE if last_request.is_empty() => normalize_create(raw),
        CREATE | APPEND => normalize_subsequent(
            raw,
            last_request,
            last_output,
            last_id,
            pending,
            allow_incremental,
            allow_bypass,
        ),
        other => Err(WsError::bad(format!("unsupported websocket request type: {other}"))),
    }
}

/// `normalizeResponseCreateRequest`.
pub(crate) fn normalize_create(raw: &str) -> Result<(String, String), WsError> {
    let input = gjson::get(raw, "input");
    if input.exists() && !is_array(&input) {
        return Err(requires_array());
    }
    let mut normalized = stream_true(&delete(raw, "type"));
    if !gjson::get(&normalized, "input").exists() {
        normalized = set_raw(&normalized, "input", "[]");
    }
    if field(&normalized, "model").is_empty() {
        return Err(WsError::bad("missing model in response.create request"));
    }
    Ok((normalized.clone(), normalized))
}

/// `model` and `instructions` carried over from the previous request when absent.
fn inherit(mut normalized: String, last_request: &str) -> String {
    if !gjson::get(&normalized, "model").exists() {
        let model = field(last_request, "model");
        if !model.is_empty() {
            normalized = set_str(&normalized, "model", &model);
        }
    }
    if !gjson::get(&normalized, "instructions").exists() {
        let instructions = gjson::get(last_request, "instructions");
        if instructions.exists() {
            normalized = set_raw(&normalized, "instructions", instructions.json());
        }
    }
    normalized
}

fn normalize_subsequent(
    raw: &str,
    last_request: &str,
    last_output: &str,
    last_id: &str,
    pending: &[String],
    allow_incremental: bool,
    allow_bypass: bool,
) -> Result<(String, String), WsError> {
    if last_request.is_empty() {
        return Err(WsError::bad("websocket request received before response.create"));
    }
    let next = gjson::get(raw, "input");
    if !next.exists() || !is_array(&next) {
        return Err(requires_array());
    }
    // A compacted transcript replaces history instead of appending to it.
    if should_replace_transcript(raw, &next) {
        let normalized = transcript_replacement(raw, last_request);
        return Ok((normalized.clone(), normalized));
    }
    if allow_incremental {
        let mut prev = field(raw, "previous_response_id");
        if prev.is_empty() {
            if !satisfies_pending(&next, pending) {
                let normalized = transcript_replacement(raw, last_request);
                return Ok((normalized.clone(), normalized));
            }
            prev = last_id.trim().to_owned();
        }
        if !prev.is_empty() {
            let normalized = set_str(&delete(raw, "type"), "previous_response_id", &prev);
            let normalized = stream_true(&inherit(normalized, last_request));
            return Ok((normalized.clone(), normalized));
        }
    }
    let merged = if allow_bypass && contains_full_transcript(&next) {
        next.json().to_owned()
    } else {
        let append = if contains_full_transcript(&next) {
            without_compaction_items(&next)
        } else {
            next.json().to_owned()
        };
        merge_input(last_request, last_output, &append).map_err(WsError::bad)?
    };
    let normalized = delete(&delete(raw, "type"), "previous_response_id");
    let normalized = stream_true(&inherit(normalized, last_request));
    let normalized = set_raw(&normalized, "input", &merged);
    Ok((normalized.clone(), normalized))
}

/// `shouldReplaceWebsocketTranscript`.
fn should_replace_transcript(raw: &str, next: &gjson::Value<'_>) -> bool {
    let kind = field(raw, "type");
    if kind != CREATE && kind != APPEND {
        return false;
    }
    let prev = gjson::get(raw, "previous_response_id");
    if !text(&prev).is_empty() || !next.exists() || !is_array(next) {
        return false;
    }
    if kind == CREATE && !prev.exists() && has_local_compaction_summary(next) {
        return true;
    }
    next.array().iter().any(|item| match text(&item.get("type")).as_str() {
        "function_call" | "custom_tool_call" => true,
        "message" => text(&item.get("role")) == "assistant",
        _ => false,
    })
}

/// `inputHasCodexLocalCompactionSummary`.
fn has_local_compaction_summary(input: &gjson::Value<'_>) -> bool {
    if !is_array(input) {
        return false;
    }
    let mut found = false;
    for (index, item) in input.array().iter().enumerate() {
        let kind = text(&item.get("type"));
        if kind == "additional_tools" {
            let tools = item.get("tools");
            if index != 0 || text(&item.get("role")) != "developer" || !is_array(&tools) {
                return false;
            }
            if tools
                .array()
                .iter()
                .any(|t| t.kind() != Kind::Object || text(&t.get("type")).is_empty())
            {
                return false;
            }
            continue;
        }
        if !kind.is_empty() && kind != "message" {
            return false;
        }
        let role = text(&item.get("role"));
        if role != "user" && role != "developer" {
            return false;
        }
        if role == "user" && message_text(item).starts_with(&format!("{COMPACTION_SUMMARY_PREFIX}\n")) {
            found = true;
        }
    }
    found
}

fn message_text(message: &gjson::Value<'_>) -> String {
    let content = message.get("content");
    match content.kind() {
        Kind::String => content.str().to_owned(),
        Kind::Array => content
            .array()
            .iter()
            .filter(|p| text(&p.get("type")) == "input_text")
            .map(|p| p.get("text").str().to_owned())
            .collect(),
        _ => String::new(),
    }
}

/// `inputSatisfiesPendingToolCalls`.
fn satisfies_pending(input: &gjson::Value<'_>, pending: &[String]) -> bool {
    if pending.is_empty() {
        return true;
    }
    if !is_array(input) {
        return false;
    }
    let outputs: HashSet<String> = input
        .array()
        .iter()
        .filter(|i| is_tool_output(&text(&i.get("type"))))
        .map(|i| text(&i.get("call_id")))
        .filter(|id| !id.is_empty())
        .collect();
    pending
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .all(|id| outputs.contains(id))
}

/// `normalizeResponseTranscriptReplacement`.
pub(crate) fn transcript_replacement(raw: &str, last_request: &str) -> String {
    let normalized = delete(&delete(raw, "type"), "previous_response_id");
    stream_true(&inherit(normalized, last_request))
}

pub(crate) fn is_tool_call(kind: &str) -> bool {
    matches!(kind.trim(), "function_call" | "custom_tool_call")
}

pub(crate) fn is_tool_output(kind: &str) -> bool {
    matches!(kind.trim(), "function_call_output" | "custom_tool_call_output")
}

/// One input item with the metadata the merge and repair rules read.
#[derive(Debug, Clone)]
pub(crate) struct Item {
    pub raw: String,
    pub kind: String,
    pub id: String,
    pub call_id: String,
}

impl Item {
    /// `parseResponsesWebsocketInputItem`: metadata keys match case-insensitively; the
    /// last duplicate wins.
    pub fn parse(value: &gjson::Value<'_>) -> Self {
        let mut item = Item {
            raw: value.json().to_owned(),
            kind: String::new(),
            id: String::new(),
            call_id: String::new(),
        };
        if value.kind() == Kind::Object {
            value.each(|k, v| {
                let key = k.str();
                if key.eq_ignore_ascii_case("type") {
                    item.kind = text(&v);
                } else if key.eq_ignore_ascii_case("id") {
                    item.id = text(&v);
                } else if key.eq_ignore_ascii_case("call_id") {
                    item.call_id = text(&v);
                }
                true
            });
        }
        item
    }
}

pub(crate) fn parse_items(array: &gjson::Value<'_>) -> Vec<Item> {
    array.array().iter().map(Item::parse).collect()
}

/// `responsesWebsocketPreviousInputNoCopy`: the remembered request's `input` array.
/// Errors carry Go's text (`encoding/json` type mismatch on `input`).
fn previous_input(last_request: &str) -> Result<String, String> {
    if !gjson::valid(last_request) {
        return Err("invalid previous request input".into());
    }
    let root = gjson::parse(last_request);
    match root.kind() {
        Kind::Null => return Ok("[]".into()),
        Kind::Object => {}
        // ponytail: Go reports encoding/json's syntax/type text here; unreachable because
        // the remembered request is always one this handler normalized.
        _ => return Err("invalid previous request input".into()),
    }
    let (mut input, mut bad) = (None::<String>, None::<Kind>);
    root.each(|k, v| {
        if k.str().eq_ignore_ascii_case("input") {
            if v.kind() != Kind::Null && v.kind() != Kind::Array {
                bad.get_or_insert(v.kind());
            }
            input = (v.kind() == Kind::Array).then(|| v.json().to_owned());
        }
        true
    });
    if let Some(kind) = bad {
        return Err(unmarshal_error(kind));
    }
    Ok(input.unwrap_or_else(|| "[]".into()))
}

/// `json.UnmarshalTypeError` for a non-array `input` of the remembered request.
fn unmarshal_error(kind: Kind) -> String {
    let value = match kind {
        Kind::String => "string",
        Kind::Number => "number",
        Kind::True | Kind::False => "bool",
        _ => "object",
    };
    format!(
        "invalid previous request input: json: cannot unmarshal {value} into Go struct field .input of type []json.RawMessage"
    )
}

/// `mergeResponsesWebsocketInput`: previous input, then the previous response output,
/// then the new items; duplicate tool calls and ids are dropped.
pub(crate) fn merge_input(last_request: &str, last_output: &str, append: &str) -> Result<String, String> {
    let previous = previous_input(last_request)?;
    let mut items = parse_items(&gjson::parse(&previous));
    let output = last_output.trim();
    if output.starts_with('[') && gjson::valid(output) {
        let output = gjson::parse(output);
        if contains_full_transcript(&output) {
            items.retain(|i| i.kind != "compaction_trigger");
        }
        items.extend(parse_items(&output));
    }
    let append = match append.trim() {
        "" => "[]",
        a => a,
    };
    if !gjson::valid(append) {
        return Err("invalid request input".into());
    }
    let parsed = gjson::parse(append);
    match parsed.kind() {
        Kind::Array => items.extend(parse_items(&parsed)),
        Kind::Null => {}
        _ => return Err("invalid request input".into()),
    }
    let items = dedupe_ids(dedupe_calls(items));
    Ok(marshal(&items))
}

/// `dedupeResponsesWebsocketMergeFunctionCalls`: the first tool call per call_id wins.
pub(crate) fn dedupe_calls(items: Vec<Item>) -> Vec<Item> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|i| !(is_tool_call(&i.kind) && !i.call_id.is_empty()) || seen.insert(i.call_id.clone()))
        .collect()
}

/// `dedupeResponsesWebsocketInputItems`: the last item per id wins, but never at the cost
/// of dropping a tool call whose output is still present.
pub(crate) fn dedupe_ids(items: Vec<Item>) -> Vec<Item> {
    let referenced: HashSet<&str> = items
        .iter()
        .filter(|i| is_tool_output(&i.kind) && !i.call_id.is_empty())
        .map(|i| i.call_id.as_str())
        .collect();
    let mut keep: HashMap<&str, (usize, bool)> = HashMap::new();
    for (index, item) in items.iter().enumerate() {
        if item.id.is_empty() {
            continue;
        }
        let is_referenced = !item.call_id.is_empty() && referenced.contains(item.call_id.as_str());
        match keep.get(item.id.as_str()) {
            Some(&(_, kept_referenced)) if !is_referenced && kept_referenced => {}
            _ => {
                keep.insert(item.id.as_str(), (index, is_referenced));
            }
        }
    }
    let keep: HashMap<String, usize> = keep.into_iter().map(|(k, (i, _))| (k.to_owned(), i)).collect();
    items
        .into_iter()
        .enumerate()
        .filter(|(index, item)| item.id.is_empty() || keep.get(&item.id) == Some(index))
        .map(|(_, item)| item)
        .collect()
}

pub(crate) fn marshal(items: &[Item]) -> String {
    let mut out = String::with_capacity(2 + items.iter().map(|i| i.raw.len() + 1).sum::<usize>());
    out.push('[');
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&item.raw);
    }
    out.push(']');
    out
}

/// `inputContainsFullTranscript`: compaction markers mean the client resent everything.
pub(crate) fn contains_full_transcript(input: &gjson::Value<'_>) -> bool {
    is_array(input)
        && input
            .array()
            .iter()
            .any(|i| matches!(i.get("type").str(), "compaction" | "compaction_summary"))
}

fn without_compaction_items(input: &gjson::Value<'_>) -> String {
    let items = input.array();
    let kept: Vec<&str> = items
        .iter()
        .filter(|i| !matches!(i.get("type").str(), "compaction" | "compaction_summary"))
        .map(|i| i.json())
        .collect();
    format!("[{}]", kept.join(","))
}

/// `normalizeResponsesWebsocketPassthroughRequest`: a turn for the pinned upstream
/// socket keeps its type and `previous_response_id`.
pub(crate) fn normalize_passthrough(raw: &str, model: &str) -> Result<String, WsError> {
    if !gjson::valid(raw) {
        return Err(WsError::bad("invalid websocket request JSON"));
    }
    let kind = field(raw, "type");
    if kind != CREATE && kind != APPEND {
        return Err(WsError::bad(format!("unsupported websocket request type: {kind}")));
    }
    let mut normalized = raw.to_owned();
    if field(&normalized, "model").is_empty() {
        let model = model.trim();
        if model.is_empty() {
            return Err(WsError::bad("missing model in response.create request"));
        }
        normalized = set_str(&normalized, "model", model);
    }
    Ok(stream_true(&normalized))
}

/// `shouldHandleResponsesWebsocketPrewarmLocally`: `response.create` with
/// `generate: false` is answered locally.
pub(crate) fn is_local_prewarm(raw: &str) -> bool {
    let generate = gjson::get(raw, "generate");
    field(raw, "type") == CREATE && generate.exists() && !go_bool(&generate)
}

/// gjson `Result.Bool()`.
fn go_bool(v: &gjson::Value<'_>) -> bool {
    match v.kind() {
        Kind::True => true,
        Kind::String => matches!(v.str().to_ascii_lowercase().as_str(), "1" | "t" | "true"),
        Kind::Number => v.f64() != 0.0,
        _ => false,
    }
}

/// `normalizeResponsesWebsocketPrewarmFollowup`: the warm-up input never reached
/// upstream, so the follow-up is merged onto it as a full transcript.
pub(crate) fn prewarm_followup(raw: &str, warmup: &str) -> Result<(String, String), WsError> {
    let kind = field(raw, "type");
    if kind != CREATE && kind != APPEND {
        return Err(WsError::bad(format!("unsupported websocket request type: {kind}")));
    }
    let input = gjson::get(raw, "input");
    if !is_array(&input) {
        return Err(requires_array());
    }
    let merged = merge_input(warmup, "[]", input.json()).map_err(WsError::bad)?;
    let normalized = set_raw(&transcript_replacement(raw, warmup), "input", &merged);
    Ok((normalized.clone(), normalized))
}

/// `syntheticResponsesWebsocketPrewarmPayloads`.
pub(crate) fn prewarm_payloads(request: &str, id: &str, created_at: i64) -> [String; 2] {
    let model = field(request, "model");
    let fill = |template: &str| {
        let mut payload = set_str(template, "response.id", id);
        payload = set_raw(&payload, "response.created_at", &created_at.to_string());
        if !model.is_empty() {
            payload = set_str(&payload, "response.model", &model);
        }
        payload
    };
    [
        fill(
            r#"{"type":"response.created","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"in_progress","background":false,"error":null,"output":[]}}"#,
        ),
        fill(
            r#"{"type":"response.completed","sequence_number":1,"response":{"id":"","object":"response","created_at":0,"status":"completed","background":false,"error":null,"output":[],"usage":{"input_tokens":0,"input_tokens_details":{"cached_tokens":0},"output_tokens":0,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":0}}}"#,
        ),
    ]
}

/// `websocketJSONPayloadsFromChunk`: JSON events from an SSE or raw chunk.
pub(crate) fn payloads_from_chunk(chunk: &[u8]) -> Vec<String> {
    let chunk = String::from_utf8_lossy(chunk);
    let strip = |line: &str| -> String {
        match line.strip_prefix("data:") {
            Some(rest) => rest.trim_ascii().to_owned(),
            None => line.to_owned(),
        }
    };
    let mut payloads = Vec::new();
    for line in chunk.split('\n') {
        let line = line.trim_ascii();
        if line.is_empty() || line.starts_with("event:") {
            continue;
        }
        let line = strip(line);
        if line.is_empty() || line == "[DONE]" {
            continue;
        }
        if gjson::valid(&line) {
            payloads.push(line);
        }
    }
    if payloads.is_empty() {
        let whole = strip(chunk.trim_ascii());
        if !whole.is_empty() && whole != "[DONE]" && gjson::valid(&whole) {
            payloads.push(whole);
        }
    }
    payloads
}

/// `isCompleteResponsesWebsocketToolCall`.
pub(crate) fn is_complete_tool_call(item: &gjson::Value<'_>) -> bool {
    if item.kind() != Kind::Object {
        return false;
    }
    let (call_id, name) = (item.get("call_id"), item.get("name"));
    if call_id.kind() != Kind::String
        || call_id.str().trim().is_empty()
        || name.kind() != Kind::String
        || name.str().trim().is_empty()
    {
        return false;
    }
    let field = match text(&item.get("type")).as_str() {
        "function_call" => item.get("arguments"),
        "custom_tool_call" => item.get("input"),
        _ => return false,
    };
    field.kind() == Kind::String
}

/// Output items of the response in flight (`outputItemsByIndex` / `outputItemsFallback`)
/// plus the tool calls still waiting for outputs.
#[derive(Default)]
pub(crate) struct Turn {
    by_index: BTreeMap<i64, String>,
    fallback: Vec<String>,
    pending: BTreeSet<String>,
}

impl Turn {
    /// `response.created` starts a new response.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// `collectResponsesWebsocketOutputItem`.
    pub fn collect(&mut self, payload: &str) {
        if gjson::get(payload, "type").str() != "response.output_item.done" {
            return;
        }
        let item = gjson::get(payload, "item");
        if item.kind() != Kind::Object {
            return;
        }
        let index = gjson::get(payload, "output_index");
        if index.exists() {
            self.by_index.insert(index.i64(), item.json().to_owned());
        } else {
            self.fallback.push(item.json().to_owned());
        }
    }

    fn collected(&self) -> impl Iterator<Item = &String> {
        self.by_index.values().chain(self.fallback.iter())
    }

    /// `restoreResponsesWebsocketCompletionOutput`.
    pub fn restore_completion(&self, payload: String) -> String {
        let output = gjson::get(&payload, "response.output");
        if is_array(&output) && !output.array().is_empty() {
            return match self.reconcile_tool_calls(&output) {
                Some(reconciled) => set_raw(&payload, "response.output", &reconciled),
                None => payload,
            };
        }
        if self.by_index.is_empty() && self.fallback.is_empty() {
            return payload;
        }
        let output = self.completed_output(&payload);
        set_raw(&payload, "response.output", &output)
    }

    /// `reconcileResponsesWebsocketCompletionToolCalls`: complete tool calls seen in
    /// `output_item.done` replace their counterparts in the completion output.
    fn reconcile_tool_calls(&self, output: &gjson::Value<'_>) -> Option<String> {
        let mut collected: HashMap<String, String> = HashMap::new();
        for raw in self.collected() {
            let item = gjson::parse(raw);
            if is_complete_tool_call(&item) {
                collected.insert(text(&item.get("call_id")), raw.clone());
            }
        }
        if collected.is_empty() {
            return None;
        }
        let mut changed = false;
        let items: Vec<String> = output
            .array()
            .iter()
            .map(|item| {
                if is_tool_call(item.get("type").str())
                    && let Some(raw) = collected.get(&text(&item.get("call_id")))
                    && raw != item.json()
                {
                    changed = true;
                    return compact(raw);
                }
                compact(item.json())
            })
            .collect();
        changed.then(|| format!("[{}]", items.join(",")))
    }

    /// `responseCompletedOutputFromPayload`.
    pub fn completed_output(&self, payload: &str) -> String {
        let output = gjson::get(payload, "response.output");
        if is_array(&output) && !output.array().is_empty() {
            return output.json().to_owned();
        }
        if self.by_index.is_empty() && self.fallback.is_empty() {
            return "[]".into();
        }
        let items: Vec<String> = self
            .collected()
            .filter(|raw| {
                let item = gjson::parse(raw);
                !is_tool_call(item.get("type").str()) || is_complete_tool_call(&item)
            })
            .map(|raw| compact(raw))
            .collect();
        format!("[{}]", items.join(","))
    }

    /// `recordPendingToolCallIDsFromPayload`.
    pub fn track_pending(&mut self, payload: &str) {
        self.track_item(&gjson::get(payload, "item"));
        let output = gjson::get(payload, "response.output");
        if is_array(&output) {
            for item in output.array() {
                self.track_item(&item);
            }
        }
    }

    fn track_item(&mut self, item: &gjson::Value<'_>) {
        if !item.exists() {
            return;
        }
        let kind = text(&item.get("type"));
        if is_tool_call(&kind) {
            if is_complete_tool_call(item) {
                self.pending.insert(text(&item.get("call_id")));
            }
        } else if is_tool_output(&kind) {
            let id = text(&item.get("call_id"));
            if !id.is_empty() {
                self.pending.remove(&id);
            }
        }
    }

    pub fn pending(&self) -> Vec<String> {
        self.pending.iter().filter(|id| !id.is_empty()).cloned().collect()
    }
}

/// `json.Marshal` of a `json.RawMessage` compacts it (and escapes HTML).
fn compact(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            match c {
                '\\' => {
                    out.push(c);
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                }
                '"' => {
                    in_string = false;
                    out.push(c);
                }
                '<' => out.push_str("\\u003c"),
                '>' => out.push_str("\\u003e"),
                '&' => out.push_str("\\u0026"),
                '\u{2028}' => out.push_str("\\u2028"),
                '\u{2029}' => out.push_str("\\u2029"),
                _ => out.push(c),
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if !c.is_ascii_whitespace() {
            out.push(c);
        }
    }
    out
}

/// Go `http.StatusText`.
pub(crate) fn status_text(status: u16) -> &'static str {
    match status {
        413 => "Request Entity Too Large",
        414 => "Request URI Too Long",
        416 => "Requested Range Not Satisfiable",
        422 => "Unprocessable Entity",
        418 => "I'm a teapot",
        s => http_status_text(s),
    }
}

fn http_status_text(status: u16) -> &'static str {
    axum::http::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or_default()
}

/// `handlers.BuildErrorResponseBodyWithError` (no terminal-auth classification).
fn error_body(status: u16, text: &str) -> String {
    let trimmed = text.trim();
    if !trimmed.is_empty() && gjson::valid(trimmed) {
        return trimmed.to_owned();
    }
    let (kind, code) = match status {
        401 => ("authentication_error", "invalid_api_key"),
        403 => ("permission_error", "insufficient_quota"),
        429 => ("rate_limit_error", "rate_limit_exceeded"),
        404 => ("invalid_request_error", "model_not_found"),
        408 => ("server_error", "request_timeout"),
        500.. => ("server_error", "internal_server_error"),
        _ => ("invalid_request_error", ""),
    };
    let mut body = format!(r#"{{"error":{{"message":{},"type":"{kind}""#, go_quote(text, true));
    if !code.is_empty() {
        body.push_str(&format!(r#","code":"{code}""#));
    }
    body.push_str("}}");
    body
}

/// `buildResponsesWebsocketErrorPayload`: `{"type":"error","status":S,"error":{...}}`.
// ponytail: `headers` from the error's Addon are not copied into the payload.
pub(crate) fn error_payload(status: u16, message: &str) -> String {
    let status = if status == 0 { 500 } else { status };
    let text = if message.trim().is_empty() {
        status_text(status).to_owned()
    } else {
        message.to_owned()
    };
    let body = error_body(status, &text);
    let mut payload = set_str("{}", "type", "error");
    payload = set_raw(&payload, "status", &status.to_string());
    if gjson::valid(&body) {
        let error = gjson::get(&body, "error");
        payload = if error.exists() {
            set_raw(&payload, "error", error.json())
        } else {
            set_raw(&payload, "error", &body)
        };
    }
    if !gjson::get(&payload, "error").exists() {
        payload = set_str(&payload, "error.type", "server_error");
        payload = set_str(&payload, "error.message", &text);
    }
    payload
}

/// `truncateWebsocketCloseReason`: at most `max` bytes, cut on a character boundary.
pub(crate) fn truncate_reason(reason: &str, max: usize) -> String {
    let mut out = String::new();
    for c in reason.chars() {
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
#[path = "websocket_requests_tests.rs"]
mod tests;
