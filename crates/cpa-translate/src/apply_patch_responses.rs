//! The apply_patch Responses bridge for executors whose upstream speaks the Responses
//! wire but cannot take Codex's freeform `custom` apply_patch tool.
//!
//! - [`normalize_request`] (translator/common NormalizeApplyPatchResponsesRequest) and
//!   [`normalize_executor_request`] (helps NormalizeApplyPatchResponsesRequest) declare the
//!   winning custom `apply_patch` tool as a JSON function with one `input` string and turn
//!   custom patch history into function calls.
//! - [`Bridge`] (common ApplyPatchResponsesBridge) turns the upstream's function-call
//!   events and terminal envelopes back into the custom tool the client declared, failing
//!   the response once on invalid or conflicting patch input.
//! - [`State`] (helps ApplyPatchResponsesState) is what Kimi, Meta and xAI own per
//!   request: the bridge plus xAI's folded namespace dispatchers, `[DONE]` and EOF checks,
//!   and SSE framing.
//!
//! Everything works on JSON event payloads except [`State::stream`] and
//! [`State::finish_stream`], which take and return SSE lines. A failure yields one
//! `response.failed` payload plus an error; the executor then ends the response with HTTP
//! 502 and [`crate::APPLY_PATCH_UPSTREAM_ERROR`], as Go's executors do.

use std::collections::{HashMap, HashSet};

use cpa_common::json::{self as gj, AnyValue, Kind, Res};
use cpa_core::format::Format;

use crate::apply_patch::{self, CallState, InputDecoder};
use crate::claude_responses::qualify_namespace_name;
use crate::common::trim_space;
use crate::responses_tools;

/// A transform result: zero or more payloads (or SSE lines) and, on failure, the error
/// Go returns next to them (its text is internal; clients only see the sanitized event).
pub type Transformed = (Vec<Vec<u8>>, Option<String>);

/// The winning declaration for one qualified tool name.
#[derive(Clone, Default)]
struct Tool {
    name: Vec<u8>,
    local_name: Vec<u8>,
    namespace: Vec<u8>,
    /// applypatch.IsCustomTool on the declaration.
    patch: bool,
}

/// util.CollectResponsesToolWinners over `raw`.
fn winner_tools(raw: &[u8]) -> HashMap<Vec<u8>, Tool> {
    let root = gj::parse(raw);
    let descriptors = responses_tools::descriptors(&root);
    responses_tools::winners(&descriptors)
        .into_iter()
        .map(|(name, order)| {
            let d = &descriptors[order];
            let tool = Tool {
                name: name.clone(),
                local_name: d.local_name.clone(),
                namespace: d.namespace.clone(),
                patch: apply_patch::is_custom_tool(&d.tool),
            };
            (name, tool)
        })
        .collect()
}

/// common.JoinRawArray.
fn join_raw_array(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![b'['];
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(item);
    }
    out.push(b']');
    out
}

fn str_of(r: &Res<'_>) -> Vec<u8> {
    r.bytes().into_owned()
}

// ---------------------------------------------------------------------------------------
// Request normalization

/// translator/common NormalizeApplyPatchResponsesRequest: the winning custom `apply_patch`
/// declaration (top-level, `additional_tools` or namespace child) becomes a function with
/// an `input` string parameter, losing duplicates of a patch name are dropped, custom patch
/// history becomes `function_call`/`function_call_output`, and a custom tool choice of the
/// patch tool becomes a function choice.
pub fn normalize_request(raw: &[u8]) -> Result<Vec<u8>, String> {
    if !gj::valid(raw) {
        return Err("invalid Responses request JSON".into());
    }
    let root = gj::parse(raw);
    let descriptors = responses_tools::descriptors(&root);
    let winners = responses_tools::winners(&descriptors);
    let affected: HashSet<Vec<u8>> = descriptors
        .iter()
        .filter(|d| apply_patch::is_custom_tool(&d.tool))
        .map(|d| d.name.clone())
        .collect();
    let normalize_tools = |tools: &Res<'_>, namespace: &[u8]| -> Vec<u8> {
        fn walk(
            tools: &Res<'_>,
            namespace: &[u8],
            winners: &HashMap<Vec<u8>, usize>,
            affected: &HashSet<Vec<u8>>,
            descriptors: &[responses_tools::Descriptor<'_>],
        ) -> Vec<u8> {
            let mut items = vec![];
            for tool in tools.array() {
                let mut item = tool.raw.to_vec();
                if &*tool.get("type").bytes() == b"namespace" {
                    for key in ["tools", "children"] {
                        let children = tool.get(key);
                        if children.is_array() {
                            let normalized = walk(&children, &tool.get("name").bytes(), winners, affected, descriptors);
                            gj::set_raw(&mut item, key, normalized);
                            break;
                        }
                    }
                } else {
                    let mut name = str_of(&tool.get("name"));
                    if name.is_empty() {
                        name = str_of(&tool.get("function.name"));
                    }
                    let qualified = qualify_namespace_name(namespace, &name);
                    if let Some(&winner) = winners.get(&qualified)
                        && affected.contains(&qualified)
                    {
                        if descriptors[winner].tool.index != tool.index {
                            continue;
                        }
                        if apply_patch::is_custom_tool(&tool) {
                            gj::set_str(&mut item, "type", "function");
                            gj::set_str(&mut item, "description", apply_patch::description(&tool));
                            gj::set_raw(&mut item, "parameters", apply_patch::PARAMETERS);
                            gj::delete(&mut item, "format");
                        }
                    }
                }
                items.push(item);
            }
            join_raw_array(&items)
        }
        walk(tools, namespace, &winners, &affected, &descriptors)
    };
    let mut out = raw.to_vec();
    let tools = root.get("tools");
    if tools.is_array() {
        gj::set_raw(&mut out, "tools", normalize_tools(&tools, b""));
    }
    let input = root.get("input").array();
    let patch_history: HashSet<Vec<u8>> = input
        .iter()
        .filter(|item| {
            &*item.get("type").bytes() == b"custom_tool_call" && trim_space(&item.get("name").bytes()) == b"apply_patch"
        })
        .map(|item| str_of(&item.get("call_id")))
        .collect();
    for (i, item) in input.iter().enumerate() {
        let path = format!("input.{i}");
        match &*item.get("type").bytes() {
            b"additional_tools" => {
                let tools = item.get("tools");
                if tools.is_array() {
                    gj::set_raw(&mut out, &format!("{path}.tools"), normalize_tools(&tools, b""));
                }
            }
            b"custom_tool_call" => {
                if trim_space(&item.get("name").bytes()) != b"apply_patch" {
                    continue;
                }
                let patch = item.get("input");
                if patch.kind != Kind::String {
                    return Err("apply_patch history input must be a string".into());
                }
                gj::set_str(&mut out, &format!("{path}.type"), "function_call");
                gj::set_str(
                    &mut out,
                    &format!("{path}.arguments"),
                    apply_patch::wrap_input(&patch.s),
                );
                gj::delete(&mut out, &format!("{path}.input"));
            }
            b"custom_tool_call_output" if patch_history.contains(&*item.get("call_id").bytes()) => {
                gj::set_str(&mut out, &format!("{path}.type"), "function_call_output");
            }
            _ => {}
        }
    }
    fn normalize_choice(
        choice: &Res<'_>,
        winners: &HashMap<Vec<u8>, usize>,
        descriptors: &[responses_tools::Descriptor<'_>],
    ) -> Vec<u8> {
        let mut out = choice.raw.to_vec();
        let qualified = qualify_namespace_name(&choice.get("namespace").bytes(), &choice.get("name").bytes());
        if let Some(&winner) = winners.get(&qualified)
            && apply_patch::is_custom_tool(&descriptors[winner].tool)
            && &*choice.get("type").bytes() == b"custom"
        {
            gj::set_str(&mut out, "type", "function");
        }
        for (i, child) in choice.get("tools").array().iter().enumerate() {
            gj::set_raw(
                &mut out,
                &format!("tools.{i}"),
                normalize_choice(child, winners, descriptors),
            );
        }
        out
    }
    let choice = root.get("tool_choice");
    if choice.is_object() {
        gj::set_raw(
            &mut out,
            "tool_choice",
            normalize_choice(&choice, &winners, &descriptors),
        );
    }
    Ok(out)
}

/// helps NormalizeApplyPatchResponsesRequest: [`normalize_request`] after preferring the
/// Chat client's ordinary `apply_patch` functions over a custom one (when the executor
/// passes the client's original Chat request).
pub fn normalize_executor_request(body: &[u8], original: Option<&[u8]>) -> Result<Vec<u8>, String> {
    match original {
        Some(original) => normalize_request(&prefer_chat_function_patch_tools(original, body)),
        None => normalize_request(body),
    }
}

/// preferChatFunctionPatchTools: a Chat request that also declares an ordinary function
/// named like the custom patch tool keeps the function (the Chat converter's winner rule).
fn prefer_chat_function_patch_tools(original: &[u8], declarations: &[u8]) -> Vec<u8> {
    let ordinary: HashSet<Vec<u8>> = gj::get(original, "tools")
        .array()
        .iter()
        .filter(|tool| &*tool.get("type").bytes() == b"function")
        .map(|tool| str_of(&tool.get("function.name")))
        .collect();
    if ordinary.is_empty() {
        return declarations.to_vec();
    }
    let declared = gj::get(declarations, "tools").array();
    let available: HashSet<Vec<u8>> = declared
        .iter()
        .filter(|tool| &*tool.get("type").bytes() == b"function")
        .map(|tool| str_of(&tool.get("name")))
        .collect();
    let tools: Vec<Vec<u8>> = declared
        .iter()
        .filter(|tool| {
            let name = str_of(&tool.get("name"));
            !(apply_patch::is_custom_tool(tool) && ordinary.contains(&name) && available.contains(&name))
        })
        .map(|tool| tool.raw.to_vec())
        .collect();
    let mut out = declarations.to_vec();
    gj::set_raw(&mut out, "tools", join_raw_array(&tools));
    out
}

// ---------------------------------------------------------------------------------------
// Bridge

#[derive(Default)]
struct Record {
    state: CallState,
    kind: Vec<u8>,
    qualified: Vec<u8>,
    source: Vec<u8>,
    patch: bool,
    named: bool,
    added: bool,
    input_done: bool,
    item_done: bool,
    snapshot: Vec<u8>,
    completed_item: Vec<u8>,
    has_snapshot: bool,
    pending: Vec<Vec<u8>>,
    evidence: Option<String>,
}

impl Record {
    fn new() -> Self {
        Self {
            state: CallState {
                output_index: -1,
                ..CallState::default()
            },
            ..Self::default()
        }
    }

    fn identity_ready(&self) -> bool {
        !self.state.item_id.is_empty() && !self.state.call_id.is_empty() && self.state.output_index >= 0
    }
}

const IDENTITY_CONFLICT: &str = "conflicting apply_patch call identity";

/// common.ApplyPatchResponsesBridge: one response's apply_patch conversion, local to that
/// response. Identity evidence is kept even before a call receives its name.
pub struct Bridge {
    error: Option<String>,
    tools: HashMap<Vec<u8>, Tool>,
    records: Vec<Record>,
    by_item_id: HashMap<Vec<u8>, usize>,
    by_call_id: HashMap<Vec<u8>, usize>,
    by_output_index: HashMap<i64, usize>,
    sequence: i64,
    last_sequence: i64,
    response_id: Vec<u8>,
    failed: bool,
    terminal: bool,
    active: bool,
    converted: bool,
}

impl Bridge {
    /// NewApplyPatchResponsesBridge: resolves the client's original declarations, before
    /// any normalization. Inactive (pure passthrough) unless a custom `apply_patch` wins.
    pub fn new(original_request: &[u8]) -> Self {
        let tools = winner_tools(original_request);
        let active = tools.values().any(|t| t.patch);
        Self {
            error: None,
            tools,
            records: vec![],
            by_item_id: HashMap::new(),
            by_call_id: HashMap::new(),
            by_output_index: HashMap::new(),
            sequence: 0,
            last_sequence: 0,
            response_id: vec![],
            failed: false,
            terminal: false,
            active,
            converted: false,
        }
    }

    /// ToolInputError: the retained conversion failure, if any.
    pub fn tool_input_error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn next(&mut self) -> i64 {
        self.sequence += 1;
        self.sequence
    }

    fn failure(&mut self, err: String) -> Transformed {
        if self.failed || self.terminal {
            return (vec![], None);
        }
        self.failed = true;
        self.error = Some(err.clone());
        let sequence = self.next();
        (vec![apply_patch::failure(&self.response_id, sequence)], Some(err))
    }

    /// Fail: terminates the response with the same one-shot failure contract.
    pub fn fail(&mut self, err: &str) -> Transformed {
        self.failure(err.to_owned())
    }

    fn descriptor(&self, item: &Res<'_>) -> Option<Tool> {
        let name = qualify_namespace_name(&item.get("namespace").bytes(), &item.get("name").bytes());
        self.tools.get(&name).cloned()
    }

    /// resolve: checks every supplied identity key. Conflicting keys and several matched
    /// records keep their evidence until patch provenance is known.
    fn resolve(&mut self, event: &Res<'_>, item: &Res<'_>) -> Result<usize, String> {
        let ids = [str_of(&event.get("item_id")), str_of(&item.get("id"))];
        let calls = [str_of(&event.get("call_id")), str_of(&item.get("call_id"))];
        let index = event.get("output_index");
        let index_value = index.exists().then(|| index.int());
        let mut matched: HashSet<usize> = HashSet::new();
        for id in ids.iter().filter(|id| !id.is_empty()) {
            if let Some(&r) = self.by_item_id.get(id) {
                matched.insert(r);
            }
        }
        for id in calls.iter().filter(|id| !id.is_empty()) {
            if let Some(&r) = self.by_call_id.get(id) {
                matched.insert(r);
            }
        }
        if let Some(i) = index_value
            && let Some(&r) = self.by_output_index.get(&i)
        {
            matched.insert(r);
        }
        let r = match (0..self.records.len()).find(|i| matched.contains(i)) {
            Some(r) => r,
            None => {
                self.records.push(Record::new());
                self.records.len() - 1
            }
        };
        let mut bad = matched.len() > 1;
        {
            let state = &self.records[r].state;
            for id in ids.iter().filter(|id| !id.is_empty()) {
                if !state.item_id.is_empty() && state.item_id != *id {
                    bad = true;
                }
            }
            for id in calls.iter().filter(|id| !id.is_empty()) {
                if !state.call_id.is_empty() && state.call_id != *id {
                    bad = true;
                }
            }
            if let Some(i) = index_value
                && state.output_index >= 0
                && state.output_index != i
            {
                bad = true;
            }
        }
        if !ids[0].is_empty() && !ids[1].is_empty() && ids[0] != ids[1] {
            bad = true;
        }
        if !calls[0].is_empty() && !calls[1].is_empty() && calls[0] != calls[1] {
            bad = true;
        }
        let descriptor = self.descriptor(item);
        let mut incoming_patch =
            descriptor.as_ref().is_some_and(|d| d.patch) && &*item.get("type").bytes() != b"custom_tool_call";
        if bad {
            self.records[r].evidence = Some(IDENTITY_CONFLICT.into());
            for &candidate in &matched {
                self.records[candidate].evidence = Some(IDENTITY_CONFLICT.into());
                if self.records[candidate].patch {
                    incoming_patch = true;
                }
            }
            // Unmatched conflicting keys alias this record too, so their later patch
            // provenance cannot create a fresh record and erase the contradiction.
            for id in ids.iter().filter(|id| !id.is_empty()) {
                self.by_item_id.entry(id.clone()).or_insert(r);
            }
            for id in calls.iter().filter(|id| !id.is_empty()) {
                self.by_call_id.entry(id.clone()).or_insert(r);
            }
            if let Some(i) = index_value {
                self.by_output_index.entry(i).or_insert(r);
            }
            if self.records[r].patch || incoming_patch {
                return Err(IDENTITY_CONFLICT.into());
            }
        } else {
            for id in ids.iter().filter(|id| !id.is_empty()) {
                self.records[r].state.item_id = id.clone();
                self.by_item_id.insert(id.clone(), r);
            }
            for id in calls.iter().filter(|id| !id.is_empty()) {
                self.records[r].state.call_id = id.clone();
                self.by_call_id.insert(id.clone(), r);
            }
            if let Some(i) = index_value {
                self.records[r].state.output_index = i;
                self.by_output_index.insert(i, r);
            }
        }
        let record = &mut self.records[r];
        let kind = str_of(&item.get("type"));
        if !kind.is_empty() {
            if !record.kind.is_empty() && record.kind != kind {
                record.evidence = Some("conflicting apply_patch call type".into());
            }
            if record.kind.is_empty() {
                record.kind = kind;
            }
        }
        let name = str_of(&item.get("name"));
        if !name.is_empty() {
            let namespace = str_of(&item.get("namespace"));
            let qualified = match &descriptor {
                Some(d) => d.name.clone(),
                None => qualify_namespace_name(&namespace, &name),
            };
            if record.named && record.qualified != qualified {
                record.evidence = Some("conflicting apply_patch call name".into());
            }
            record.named = true;
            record.qualified = qualified;
            record.state.name = name;
            record.state.namespace = namespace;
            if let Some(d) = &descriptor {
                record.state.name = d.local_name.clone();
                record.state.namespace = d.namespace.clone();
            }
        }
        if incoming_patch {
            record.patch = true;
        }
        if record.patch
            && let Some(evidence) = &record.evidence
        {
            return Err(evidence.clone());
        }
        Ok(r)
    }

    /// CheckIdentity: records every key of `event` before a folded dispatcher reveals its
    /// child. The item's name and namespace are withheld: a dispatcher name is not the
    /// selected child's name.
    pub fn check_identity(&mut self, event: &[u8]) -> Result<(), String> {
        let root = gj::parse(event);
        let item = root.get("item");
        let mut stripped = vec![];
        if item.exists() {
            stripped = item.raw.to_vec();
            gj::delete(&mut stripped, "name");
            gj::delete(&mut stripped, "namespace");
        }
        let item = gj::parse(&stripped);
        self.resolve(&root, &item).map(|_| ())
    }

    fn restore_item(&self, mut item: Vec<u8>, r: usize, input: &[u8], added: bool) -> Vec<u8> {
        let record = &self.records[r];
        if record.patch {
            gj::set_str(&mut item, "type", "custom_tool_call");
            gj::delete(&mut item, "arguments");
            gj::set_str(&mut item, "input", input);
        }
        if let Some(d) = self.tools.get(&record.qualified)
            && !d.namespace.is_empty()
        {
            gj::set_str(&mut item, "name", &d.local_name);
            gj::set_str(&mut item, "namespace", &d.namespace);
        }
        if record.patch && !added {
            if !record.state.item_id.is_empty() {
                gj::set_str(&mut item, "id", &record.state.item_id);
            }
            if !record.state.call_id.is_empty() {
                gj::set_str(&mut item, "call_id", &record.state.call_id);
            }
            gj::set_str(&mut item, "name", &record.state.name);
        }
        item
    }

    fn item_event(&mut self, kind: &str, item: &[u8], r: usize) -> Vec<u8> {
        let mut out = br#"{"type":"","output_index":0,"sequence_number":0,"item":{}}"#.to_vec();
        gj::set_str(&mut out, "type", kind);
        gj::set_int(&mut out, "output_index", self.records[r].state.output_index);
        let sequence = self.next();
        gj::set_int(&mut out, "sequence_number", sequence);
        gj::set_raw(&mut out, "item", item);
        out
    }

    fn snapshot(&mut self, r: usize, arguments: &Res<'_>, last: bool) -> Result<(), String> {
        if !arguments.exists() {
            return Ok(());
        }
        if arguments.kind != Kind::String {
            return Err("apply_patch arguments snapshot must be a string".into());
        }
        if arguments.s.is_empty() && !last {
            return Ok(());
        }
        let mut decoder = InputDecoder::default();
        decoder.finish(&arguments.s)?;
        let record = &mut self.records[r];
        if record.has_snapshot {
            let mut previous = InputDecoder::default();
            let _ = previous.finish(&record.snapshot);
            if previous.input() != decoder.input() {
                return Err("conflicting apply_patch arguments snapshot".into());
            }
        }
        if !decoder.input().starts_with(record.state.decoder.input()) {
            return Err("apply_patch snapshot conflicts with streamed input".into());
        }
        record.snapshot = arguments.s.to_vec();
        record.has_snapshot = true;
        Ok(())
    }

    fn patch_event(&mut self, raw: &[u8], r: usize) -> Result<Vec<Vec<u8>>, String> {
        if !self.records[r].identity_ready() {
            return Err("unresolved apply_patch call identity".into());
        }
        let root = gj::parse(raw);
        let kind = str_of(&root.get("type"));
        let item = root.get("item");
        self.converted = true;
        let mut out = vec![];
        if item.exists() {
            if &*item.get("type").bytes() != b"function_call" {
                return Err("conflicting apply_patch call type".into());
            }
            self.snapshot(r, &item.get("arguments"), kind == b"response.output_item.done")?;
        }
        if !self.records[r].added {
            let mut added = if item.exists() {
                item.raw.to_vec()
            } else {
                br#"{"type":"function_call","name":"","arguments":""}"#.to_vec()
            };
            let state = &self.records[r].state;
            gj::set_str(&mut added, "name", &state.name);
            if !state.item_id.is_empty() {
                gj::set_str(&mut added, "id", &state.item_id);
            }
            if !state.call_id.is_empty() {
                gj::set_str(&mut added, "call_id", &state.call_id);
            }
            if !state.namespace.is_empty() {
                gj::set_str(&mut added, "namespace", &state.namespace);
            }
            let restored = self.restore_item(added, r, b"", true);
            let event = self.item_event("response.output_item.added", &restored, r);
            out.push(event);
            self.records[r].added = true;
        }
        match kind.as_slice() {
            b"response.function_call_arguments.delta" => {
                let fragment = str_of(&root.get("delta"));
                if self.records[r].input_done {
                    if !fragment.is_empty() {
                        return Err("apply_patch arguments received after completion".into());
                    }
                    return Ok(out);
                }
                let record = &mut self.records[r];
                record.source.extend_from_slice(&fragment);
                let delta = record.state.push_arguments(&fragment)?;
                if record.has_snapshot {
                    let mut snapshot = InputDecoder::default();
                    let _ = snapshot.finish(&record.snapshot);
                    if !snapshot.input().starts_with(record.state.decoder.input()) {
                        return Err("apply_patch stream conflicts with snapshot".into());
                    }
                }
                if !delta.is_empty() {
                    let sequence = self.next();
                    out.push(self.records[r].state.input_delta(&delta, sequence));
                }
            }
            b"response.function_call_arguments.done" | b"response.output_item.done" => {
                let arguments = if item.exists() {
                    item.get("arguments")
                } else {
                    root.get("arguments")
                };
                if arguments.exists() {
                    self.snapshot(r, &arguments, true)?;
                }
                let record = &mut self.records[r];
                let last = if record.has_snapshot {
                    record.snapshot.clone()
                } else {
                    record.source.clone()
                };
                let (tail, input) = record.state.finish_arguments(&last)?;
                if !record.input_done {
                    if !tail.is_empty() && !record.source.is_empty() {
                        let sequence = self.next();
                        out.push(self.records[r].state.input_delta(&tail, sequence));
                    }
                    let sequence = self.next();
                    out.push(self.records[r].state.input_done(&input, sequence));
                    self.records[r].input_done = true;
                }
                if kind == b"response.output_item.done" && !self.records[r].item_done {
                    let completed = self.restore_item(item.raw.to_vec(), r, input.as_bytes(), false);
                    self.records[r].completed_item = completed.clone();
                    out.push(self.item_event("response.output_item.done", &completed, r));
                    self.records[r].item_done = true;
                }
            }
            _ => {}
        }
        Ok(out)
    }

    fn transform_item_event(&mut self, raw: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let root = gj::parse(raw);
        let root_item = root.get("item");
        let mut item_raw = root_item.raw.to_vec();
        if !root_item.exists() && root.get("name").exists() {
            // Arguments events can carry late names and identities at the root.
            let mut identity = br#"{"type":"function_call"}"#.to_vec();
            for key in ["name", "namespace", "call_id"] {
                let value = root.get(key);
                if value.exists() {
                    AnyValue::from_res(&value).set(&mut identity, key);
                }
            }
            item_raw = identity;
        }
        let item = gj::parse(&item_raw);
        let r = self.resolve(&root, &item)?;
        let kind = str_of(&root.get("type"));
        {
            let record = &self.records[r];
            let waiting = (!record.named && (record.kind.is_empty() || record.kind == b"function_call"))
                || (record.patch && !record.identity_ready());
            if waiting {
                if record.patch {
                    let arguments = if root_item.exists() {
                        item.get("arguments")
                    } else {
                        root.get("arguments")
                    };
                    let last = kind == b"response.output_item.done" || kind == b"response.function_call_arguments.done";
                    self.snapshot(r, &arguments, last)?;
                }
                self.records[r].pending.push(raw.to_vec());
                return Ok(vec![]);
            }
        }
        let pending = std::mem::take(&mut self.records[r].pending);
        if self.records[r].patch {
            let mut out = vec![];
            for event in pending.iter().map(Vec::as_slice).chain([raw]) {
                out.extend(self.patch_event(event, r)?);
            }
            return Ok(out);
        }
        let mut out = pending;
        let mut raw = raw.to_vec();
        let namespaced = self
            .tools
            .get(&self.records[r].qualified)
            .is_some_and(|d| !d.namespace.is_empty());
        if root_item.exists() && self.records[r].kind == b"function_call" && namespaced {
            let restored = self.restore_item(item.raw.to_vec(), r, b"", false);
            gj::set_raw(&mut raw, "item", restored);
        }
        if kind == b"response.output_item.done" && item.exists() {
            self.records[r].item_done = true;
            self.records[r].completed_item = gj::get(&raw, "item").raw.to_vec();
        }
        out.push(raw);
        Ok(out)
    }

    fn envelope(&mut self, raw: &[u8], stream: bool) -> Result<(Vec<u8>, Vec<Vec<u8>>), String> {
        let root = gj::parse(raw);
        let (path, response) = if root.get("response").exists() {
            ("response.output", root.get("response"))
        } else {
            ("output", root.clone())
        };
        let output = response.get("output").array();
        let mut out = raw.to_vec();
        let mut preceding = vec![];
        let mut seen = HashSet::new();
        let mut items: Vec<Vec<u8>> = vec![];
        for (i, item) in output.iter().enumerate() {
            let mut index = i as i64;
            // A terminal snapshot may omit earlier completed items: position is not identity.
            let id = str_of(&item.get("id"));
            let call_id = str_of(&item.get("call_id"));
            let known = self
                .by_item_id
                .get(&id)
                .or_else(|| self.by_call_id.get(&call_id))
                .copied();
            match known {
                Some(k) if self.records[k].state.output_index >= 0 => index = self.records[k].state.output_index,
                None if !id.is_empty() || !call_id.is_empty() => {
                    if let Some(&previous) = self.by_output_index.get(&index) {
                        let p = &self.records[previous].state;
                        if !p.item_id.is_empty() || !p.call_id.is_empty() {
                            for record in &self.records {
                                if record.state.output_index >= index {
                                    index = record.state.output_index + 1;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            let event_raw = envelope_item(index, item);
            let event = gj::parse(&event_raw);
            let r = self.resolve(&event, item)?;
            seen.insert(r);
            let item_path = format!("{path}.{i}");
            if self.records[r].patch {
                let pending = std::mem::take(&mut self.records[r].pending);
                for source in pending {
                    preceding.extend(self.patch_event(&source, r)?);
                }
                preceding.extend(self.patch_event(&event_raw, r)?);
                let input = self.records[r].state.decoder.input().as_bytes().to_vec();
                let restored = self.restore_item(item.raw.to_vec(), r, &input, false);
                gj::set_raw(&mut out, &item_path, restored);
            } else if &*item.get("type").bytes() == b"function_call"
                && self
                    .tools
                    .get(&self.records[r].qualified)
                    .is_some_and(|d| !d.namespace.is_empty())
            {
                let restored = self.restore_item(item.raw.to_vec(), r, b"", false);
                gj::set_raw(&mut out, &item_path, restored);
            }
            items.push(gj::get(&out, &item_path).raw.to_vec());
        }
        for r in 0..self.records.len() {
            if seen.contains(&r) || (!self.records[r].patch && !self.converted) {
                continue;
            }
            if self.records[r].input_done && !self.records[r].item_done {
                let input = self.records[r].state.decoder.input().as_bytes().to_vec();
                let completed = self.restore_item(
                    br#"{"type":"function_call","status":"completed"}"#.to_vec(),
                    r,
                    &input,
                    false,
                );
                self.records[r].completed_item = completed.clone();
                preceding.push(self.item_event("response.output_item.done", &completed, r));
                self.records[r].item_done = true;
            }
            if self.records[r].item_done {
                let index = self.records[r].state.output_index;
                let index = if index < 0 || index as usize > items.len() {
                    items.len()
                } else {
                    index as usize
                };
                items.insert(index, self.records[r].completed_item.clone());
            }
        }
        if items.len() != output.len() {
            gj::set_raw(&mut out, path, join_raw_array(&items));
        }
        if stream {
            self.finish()?;
            for record in &mut self.records {
                preceding.append(&mut record.pending);
            }
            if self.converted {
                let sequence = self.next();
                gj::set_int(&mut out, "sequence_number", sequence);
            }
        }
        Ok((out, preceding))
    }

    /// Transform: converts one upstream event payload (no SSE framing). A failure is
    /// terminal and emitted once; nothing follows a terminal event.
    pub fn transform(&mut self, event: &[u8]) -> Transformed {
        if self.failed || self.terminal {
            return (vec![], None);
        }
        if !self.active {
            return (vec![event.to_vec()], None);
        }
        let root = gj::parse(event);
        let id = str_of(&root.get("response.id"));
        if !id.is_empty() {
            self.response_id = id;
        }
        let sequence = root.get("sequence_number").int();
        if sequence > self.sequence {
            self.sequence = sequence;
        }
        let kind = str_of(&root.get("type"));
        let result = match kind.as_slice() {
            b"response.output_item.added"
            | b"response.output_item.done"
            | b"response.function_call_arguments.delta"
            | b"response.function_call_arguments.done" => self.transform_item_event(event),
            b"response.completed" | b"response.incomplete" | b"response.done" => {
                self.envelope(event, true).map(|(last, mut preceding)| {
                    preceding.push(last);
                    self.terminal = true;
                    preceding
                })
            }
            b"response.failed" => {
                self.terminal = true;
                Ok(vec![event.to_vec()])
            }
            _ => Ok(vec![event.to_vec()]),
        };
        let mut out = match result {
            Ok(out) => out,
            Err(err) => return self.failure(err),
        };
        // Native custom payloads are opaque; only a stream that acquired a function patch
        // call renumbers its other events.
        let native_custom = kind.starts_with(b"response.custom_tool_call_input.")
            || &*root.get("item.type").bytes() == b"custom_tool_call";
        for event in &mut out {
            let mut sequence = gj::get(event, "sequence_number").int();
            if self.converted && !native_custom && sequence <= self.last_sequence {
                sequence = self.next();
                gj::set_int(event, "sequence_number", sequence);
            }
            if sequence > self.last_sequence {
                self.last_sequence = sequence;
            }
        }
        (out, None)
    }

    /// TransformNonStream: a bare response or a terminal event envelope.
    pub fn transform_non_stream(&mut self, response: &[u8]) -> Result<Vec<u8>, String> {
        if !self.active {
            return Ok(response.to_vec());
        }
        match self.envelope(response, false) {
            Ok((out, _)) => Ok(out),
            Err(err) => {
                self.failed = true;
                self.error = Some(err.clone());
                Err(err)
            }
        }
    }

    /// Finish: rejects acquired calls whose final arguments were never validated.
    pub fn finish(&self) -> Result<(), String> {
        if let Some(err) = &self.error {
            return Err(err.clone());
        }
        if self.terminal {
            return Ok(());
        }
        if self.records.iter().any(|r| r.patch && !r.input_done) {
            return Err("incomplete apply_patch tool arguments received from upstream".into());
        }
        Ok(())
    }
}

/// patchEnvelopeItem.
fn envelope_item(index: i64, item: &Res<'_>) -> Vec<u8> {
    let mut out = br#"{"type":"response.output_item.done","output_index":0,"item":{}}"#.to_vec();
    gj::set_int(&mut out, "output_index", index);
    gj::set_raw(&mut out, "item", &item.raw);
    out
}

// ---------------------------------------------------------------------------------------
// Executor state

#[derive(Default)]
struct DispatcherCall {
    namespace: Vec<u8>,
    events: Vec<Vec<u8>>,
    snapshots: Vec<Vec<u8>>,
    source: Vec<u8>,
    originals: Vec<Vec<u8>>,
    completed: bool,
    ordinary: bool,
    index: i64,
    name: Vec<u8>,
    arguments: Vec<u8>,
}

/// helps.ApplyPatchResponsesState: the bridge as owned by a non-Codex executor for one
/// request, plus xAI's folded namespace dispatchers (a dispatcher function standing for a
/// namespace, whose arguments wrap `{"name": child, "arguments": ...}`).
pub struct State {
    pub bridge: Bridge,
    tools: HashMap<Vec<u8>, Tool>,
    dispatchers: HashMap<Vec<u8>, Vec<u8>>,
    by_key: HashMap<Vec<u8>, usize>,
    records: Vec<DispatcherCall>,
    upstream: Option<Vec<u8>>,
    event_line: Option<Vec<u8>>,
    active: bool,
    failed: bool,
    closed: bool,
    transport_done: bool,
}

fn dispatcher_keys(root: &Res<'_>) -> Vec<Vec<u8>> {
    let mut keys = vec![];
    for path in ["item.id", "item_id"] {
        let id = root.get(path).bytes();
        if !id.is_empty() {
            keys.push([&b"item:"[..], &id].concat());
        }
    }
    for path in ["item.call_id", "call_id"] {
        let id = root.get(path).bytes();
        if !id.is_empty() {
            keys.push([&b"call:"[..], &id].concat());
        }
    }
    let index = root.get("output_index");
    if index.exists() {
        keys.push(format!("index:{}", index.int()).into_bytes());
    }
    keys
}

/// patchDispatcherArguments: a wrapper's `arguments` as a string (raw JSON if not one).
fn dispatcher_arguments(wrapper: &Res<'_>) -> Vec<u8> {
    let arguments = wrapper.get("arguments");
    if arguments.kind == Kind::String {
        arguments.s.to_vec()
    } else {
        arguments.raw.to_vec()
    }
}

fn dispatcher_event_name(root: &Res<'_>) -> Vec<u8> {
    if root.get("item").exists() {
        str_of(&root.get("item.name"))
    } else {
        str_of(&root.get("name"))
    }
}

fn is_terminal(kind: &[u8]) -> bool {
    matches!(kind, b"response.completed" | b"response.incomplete" | b"response.done")
}

impl State {
    /// NewApplyPatchResponsesState. `original` is the client's request; `declarations` the
    /// Responses-shaped request whose tools decide the winners (for Chat clients the
    /// ordinary function of a patch name wins over the custom tool).
    pub fn new(source: Format, original: &[u8], declarations: &[u8]) -> Self {
        let declarations = if source == Format::OpenAI {
            prefer_chat_function_patch_tools(original, declarations)
        } else {
            declarations.to_vec()
        };
        let tools = winner_tools(&declarations);
        let active = tools.values().any(|t| t.patch);
        Self {
            bridge: Bridge::new(&declarations),
            tools,
            dispatchers: HashMap::new(),
            by_key: HashMap::new(),
            records: vec![],
            upstream: None,
            event_line: None,
            active,
            failed: false,
            closed: false,
            transport_done: false,
        }
    }

    /// Whether a winning custom `apply_patch` declaration makes this state convert.
    pub fn active(&self) -> bool {
        self.active
    }

    /// AddDispatcher: marks an xAI folded dispatcher `name` for `namespace`, only when the
    /// namespace holds a winning custom patch tool.
    pub fn add_dispatcher(&mut self, name: &str, namespace: &str) {
        if self
            .tools
            .values()
            .any(|d| d.namespace == namespace.as_bytes() && d.patch)
        {
            self.dispatchers
                .insert(name.as_bytes().to_vec(), namespace.as_bytes().to_vec());
        }
    }

    fn dispatcher_namespace(&self, name: &[u8]) -> &[u8] {
        self.dispatchers.get(name).map_or(b"", Vec::as_slice)
    }

    fn dispatcher(&self, root: &Res<'_>) -> Option<usize> {
        let matched: HashSet<usize> = dispatcher_keys(root)
            .iter()
            .filter_map(|key| self.by_key.get(key).copied())
            .collect();
        (0..self.records.len()).find(|i| matched.contains(i))
    }

    fn new_candidate(&mut self, root: &Res<'_>) -> usize {
        self.records.push(DispatcherCall {
            index: -1,
            ..DispatcherCall::default()
        });
        let call = self.records.len() - 1;
        for key in dispatcher_keys(root) {
            self.by_key.entry(key).or_insert(call);
        }
        call
    }

    fn clear(&mut self) {
        self.by_key.clear();
        self.records.clear();
        self.upstream = None;
    }

    /// RememberDispatcherEvent: the upstream event as received, before the executor
    /// restores namespaces. Call it before [`Self::transform`] for the restored event.
    pub fn remember_dispatcher_event(&mut self, event: &[u8]) {
        if self.failed || self.closed || self.transport_done || self.dispatchers.is_empty() {
            return;
        }
        self.upstream = Some(event.to_vec());
        self.remember_dispatcher_arguments(event);
    }

    /// RememberDispatcherArguments: keeps a full `arguments.done` snapshot on every
    /// matched candidate, including unnamed calls.
    pub fn remember_dispatcher_arguments(&mut self, event: &[u8]) {
        if self.failed
            || self.closed
            || self.transport_done
            || self.dispatchers.is_empty()
            || &*gj::get(event, "type").bytes() != b"response.function_call_arguments.done"
        {
            return;
        }
        self.upstream = Some(event.to_vec());
        let root = gj::parse(event);
        if self.dispatcher(&root).is_none() {
            self.new_candidate(&root);
        }
        let mut seen = HashSet::new();
        for key in dispatcher_keys(&root) {
            if let Some(&call) = self.by_key.get(&key)
                && seen.insert(call)
            {
                self.records[call].snapshots.push(event.to_vec());
            }
        }
    }

    fn expand_dispatcher(&mut self, event: Vec<u8>, original: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let event_copy = event.clone();
        let root = gj::parse(&event_copy);
        let raw = gj::parse(original);
        let kind = str_of(&root.get("type"));
        let mut call = self.dispatcher(&root);
        let raw_name = dispatcher_event_name(&raw);
        let declared_namespace = self.dispatchers.get(&raw_name).cloned();
        let declared = declared_namespace.is_some();
        if call.is_none() && !self.dispatchers.is_empty() {
            let raw_function = &*raw.get("item.type").bytes() == b"function_call";
            let added = kind == b"response.output_item.added" && raw_function;
            if (declared && raw_function)
                || added
                || kind == b"response.function_call_arguments.delta"
                || kind == b"response.function_call_arguments.done"
            {
                call = Some(self.new_candidate(&root));
            }
        }
        let Some(c) = call else {
            return Ok(vec![event]);
        };
        self.bridge.check_identity(&event)?;
        for key in dispatcher_keys(&root) {
            self.by_key.entry(key).or_insert(c);
        }
        if self.records[c].index < 0 && root.get("output_index").exists() {
            self.records[c].index = root.get("output_index").int();
        }
        if self.records[c].ordinary && !declared {
            return Ok(vec![event]);
        }
        self.records[c].events.push(event.clone());
        self.records[c].originals.push(original.to_vec());
        if let Some(namespace) = declared_namespace {
            let record = &mut self.records[c];
            if !record.namespace.is_empty() && record.namespace != namespace {
                return Err("conflicting apply_patch dispatcher namespace".into());
            }
            record.namespace = namespace;
            record.ordinary = false;
        }
        if kind == b"response.function_call_arguments.delta" {
            let delta = str_of(&root.get("delta"));
            if self.records[c].completed && !delta.is_empty() {
                let record = &self.records[c];
                let child = qualify_namespace_name(&record.namespace, &record.name);
                if self.tools.get(&child).is_some_and(|t| t.patch) {
                    return Err("apply_patch dispatcher arguments received after completion".into());
                }
                return Ok(vec![event]);
            }
            self.records[c].source.extend_from_slice(&delta);
        }
        if self.records[c].namespace.is_empty() {
            if !raw_name.is_empty() {
                // A late ordinary name releases untouched arguments, even wrapper-shaped ones.
                let record = &mut self.records[c];
                record.ordinary = true;
                record.originals.clear();
                return Ok(std::mem::take(&mut record.events));
            }
            return Ok(vec![]);
        }
        let completed = self.records[c].completed;
        if kind == b"response.function_call_arguments.delta" && !completed {
            return Ok(vec![]);
        }
        let continuation = completed
            && matches!(
                kind.as_slice(),
                b"response.function_call_arguments.done"
                    | b"response.function_call_arguments.delta"
                    | b"response.output_item.added"
            );
        if kind != b"response.output_item.done" && !continuation {
            return Ok(vec![]);
        }
        let path =
            if kind == b"response.function_call_arguments.done" || kind == b"response.function_call_arguments.delta" {
                ""
            } else {
                "item."
            };
        let at = |field: &str| format!("{path}{field}");
        let namespace = self.records[c].namespace.clone();
        let mut wrappers: Vec<Vec<u8>> = vec![];
        {
            let record = &self.records[c];
            if !record.source.is_empty() {
                wrappers.push(record.source.clone());
            }
            for snapshot in &record.snapshots {
                let arguments = str_of(&gj::get(snapshot, "arguments"));
                if gj::get(&arguments, "name").exists() {
                    wrappers.push(arguments);
                }
            }
            for pending in &record.originals {
                let p = gj::parse(pending);
                let arguments = str_of(&p.get("item.arguments"));
                let name = dispatcher_event_name(&p);
                if (name.is_empty() || self.dispatcher_namespace(&name) == namespace.as_slice())
                    && gj::get(&arguments, "name").exists()
                {
                    wrappers.push(arguments);
                }
            }
            // Callers without a pre-restoration copy keep the full-source contract.
            if wrappers.is_empty() {
                for pending in &record.events {
                    let arguments = str_of(&gj::get(pending, "arguments"));
                    if !gj::get(&arguments, "name").bytes().is_empty() {
                        wrappers.push(arguments);
                    }
                }
            }
        }
        let mut source: Vec<u8> = vec![];
        for wrapper in &wrappers {
            if gj::valid(wrapper) && !gj::get(wrapper, "name").bytes().is_empty() {
                source = wrapper.clone();
            }
        }
        let source = gj::parse(&source);
        let mut event = event;
        let mut name = str_of(&root.get(&at("name")));
        if name.is_empty() || self.dispatcher_namespace(&name) == namespace.as_slice() {
            name = str_of(&source.get("name"));
            if name.is_empty() {
                name = self.records[c].name.clone();
            }
            gj::set_str(&mut event, &at("name"), &name);
            gj::set_str(&mut event, &at("namespace"), &namespace);
        }
        if root.get(&at("namespace")).bytes().is_empty() {
            gj::set_str(&mut event, &at("namespace"), &namespace);
        }
        if declared || dispatcher_event_name(&raw).is_empty() || kind == b"response.function_call_arguments.done" {
            let wrapper_raw = str_of(&raw.get(&at("arguments")));
            let wrapper = gj::parse(&wrapper_raw);
            if !wrapper.get("name").bytes().is_empty() {
                gj::set_str(&mut event, &at("arguments"), dispatcher_arguments(&wrapper));
            }
        }
        if !root.get(&at("arguments")).exists() {
            let mut encoded = dispatcher_arguments(&source);
            if encoded.is_empty() {
                encoded = self.records[c].arguments.clone();
            }
            if !encoded.is_empty() {
                gj::set_str(&mut event, &at("arguments"), &encoded);
            }
        }
        if let Some(last) = self.records[c].events.last_mut() {
            *last = event.clone();
        }
        let updated = gj::parse(&event);
        let child = self
            .tools
            .get(&qualify_namespace_name(&namespace, &name))
            .cloned()
            .unwrap_or_default();
        let patch = child.patch;
        let mut final_arguments = str_of(&updated.get(&at("arguments")));
        if kind == b"response.output_item.added" && final_arguments.is_empty() {
            final_arguments = self.records[c].arguments.clone();
        }
        if patch {
            for snapshot in &self.records[c].snapshots {
                if gj::get(snapshot, "arguments").kind != Kind::String {
                    return Err("apply_patch dispatcher arguments snapshot must be a string".into());
                }
            }
        }
        for wrapper_raw in &wrappers {
            let wrapper = gj::parse(wrapper_raw);
            let wrapped = qualify_namespace_name(&namespace, &wrapper.get("name").bytes());
            if !patch && !self.tools.get(&wrapped).is_some_and(|t| t.patch) {
                continue;
            }
            let input = apply_patch::unwrap_input(&dispatcher_arguments(&wrapper));
            let last = apply_patch::unwrap_input(&final_arguments);
            if !gj::valid(wrapper_raw)
                || *wrapper.get("name").bytes() != *name
                || input.is_none()
                || last.is_none()
                || input != last
            {
                return Err("conflicting apply_patch dispatcher arguments".into());
            }
        }
        // Completed aliases and source evidence stay until the response closes; repeated
        // snapshots validate only the new event and never replay completed progress.
        let start = if self.records[c].completed {
            self.records[c].events.len() - 1
        } else {
            0
        };
        let mut out = vec![];
        for i in start..self.records[c].events.len() {
            let mut pending = self.records[c].events[i].clone();
            let pending_copy = pending.clone();
            let p = gj::parse(&pending_copy);
            let original_root = gj::parse(&self.records[c].originals[i]);
            if patch {
                for namespace_path in ["namespace", "item.namespace"] {
                    let supplied = original_root.get(namespace_path).bytes();
                    if !supplied.is_empty() && *supplied != *namespace {
                        return Err("conflicting apply_patch dispatcher namespace".into());
                    }
                }
                for supplied in [dispatcher_event_name(&original_root), dispatcher_event_name(&p)] {
                    if !supplied.is_empty()
                        && self.dispatcher_namespace(&supplied) != namespace.as_slice()
                        && qualify_namespace_name(&namespace, &supplied) != child.name
                    {
                        return Err("conflicting apply_patch dispatcher child".into());
                    }
                }
            }
            let pending_kind = str_of(&p.get("type"));
            let pending_path = match pending_kind.as_slice() {
                b"response.function_call_arguments.delta" => {
                    if patch {
                        // A dispatcher envelope is not incremental child input.
                        continue;
                    }
                    ""
                }
                b"response.output_item.added" | b"response.output_item.done" => "item.",
                b"response.function_call_arguments.done" => "",
                _ => {
                    out.push(pending);
                    continue;
                }
            };
            if pending_kind != b"response.function_call_arguments.delta" {
                let pat = |field: &str| format!("{pending_path}{field}");
                let pending_name = p.get(&pat("name")).bytes();
                if pending_name.is_empty() || self.dispatcher_namespace(&pending_name) == namespace.as_slice() {
                    gj::set_str(&mut pending, &pat("name"), &name);
                    gj::set_str(&mut pending, &pat("namespace"), &namespace);
                }
                if p.get(&pat("namespace")).bytes().is_empty() {
                    gj::set_str(&mut pending, &pat("namespace"), &namespace);
                }
                // Only real upstream wrappers are unwrapped, not restored child contents.
                let arguments = original_root.get(&pat("arguments"));
                if patch && arguments.exists() && arguments.kind != Kind::String {
                    return Err("apply_patch dispatcher arguments snapshot must be a string".into());
                }
                let original_name = dispatcher_event_name(&original_root);
                if !arguments.bytes().is_empty()
                    && (original_name.is_empty()
                        || self.dispatcher_namespace(&original_name) == namespace.as_slice()
                        || pending_path.is_empty())
                {
                    let wrapper_raw = str_of(&arguments);
                    let wrapper = gj::parse(&wrapper_raw);
                    let wrapper_name = wrapper.get("name").bytes();
                    if !wrapper_name.is_empty() {
                        if patch && *wrapper_name != *name {
                            return Err("conflicting apply_patch dispatcher snapshot".into());
                        }
                        gj::set_str(&mut pending, &pat("arguments"), dispatcher_arguments(&wrapper));
                    }
                }
            }
            out.push(pending);
        }
        let record = &mut self.records[c];
        record.completed = true;
        record.name = name;
        record.arguments = final_arguments;
        Ok(out)
    }

    fn unfinished_dispatcher(&self) -> bool {
        self.records.iter().any(|c| !c.namespace.is_empty() && !c.completed)
    }

    fn fail(&mut self, err: String) -> Transformed {
        if self.failed {
            return (vec![], Some(err));
        }
        self.failed = true;
        self.clear();
        self.bridge.fail(&err)
    }

    /// Transform: one upstream event payload (no SSE framing) in; client payloads out.
    pub fn transform(&mut self, event: &[u8]) -> Transformed {
        if self.failed || self.transport_done {
            return (vec![], None);
        }
        if self.active && trim_space(event) == b"[DONE]" {
            if let Err(err) = self.finish() {
                return self.fail(err);
            }
            self.transport_done = true;
            return (vec![event.to_vec()], None);
        }
        if self.closed {
            return (vec![], None);
        }
        let original = self.upstream.take().unwrap_or_else(|| event.to_vec());
        let mut event = event.to_vec();
        let event_copy = event.clone();
        let root = gj::parse(&event_copy);
        let kind = str_of(&root.get("type"));
        let mut preceding = vec![];
        if is_terminal(&kind) {
            let original_items = gj::get(&original, "response.output").array();
            for (i, item) in root.get("response.output").array().iter().enumerate() {
                let mut done = br#"{"type":"response.output_item.done"}"#.to_vec();
                gj::set_raw(&mut done, "item", &item.raw);
                // Explicit IDs beat array position in a sparse terminal snapshot.
                let mut call = self.dispatcher(&gj::parse(&done));
                let id = item.get("id").bytes();
                let call_id = item.get("call_id").bytes();
                if call.is_none() && id.is_empty() && call_id.is_empty() {
                    gj::set_int(&mut done, "output_index", i as i64);
                    call = self.dispatcher(&gj::parse(&done));
                }
                let Some(c) = call else { continue };
                if self.records[c].ordinary {
                    continue;
                }
                let index = if self.records[c].index >= 0 {
                    self.records[c].index
                } else {
                    i as i64
                };
                gj::set_int(&mut done, "output_index", index);
                let mut original_done = done.clone();
                // Filtering may shift positions; restoration keeps both IDs.
                let matches = |candidate: &Res<'_>| {
                    *candidate.get("id").bytes() == *id && *candidate.get("call_id").bytes() == *call_id
                };
                if i < original_items.len() && matches(&original_items[i]) {
                    original_done = done.clone();
                    gj::set_raw(&mut original_done, "item", &original_items[i].raw);
                } else if (!id.is_empty() || !call_id.is_empty())
                    && let Some(found) = original_items.iter().find(|candidate| matches(candidate))
                {
                    original_done = done.clone();
                    gj::set_raw(&mut original_done, "item", &found.raw);
                }
                let events = match self.expand_dispatcher(done, &original_done) {
                    Ok(events) => events,
                    Err(err) => return self.fail(err),
                };
                if let Some(last) = events.last() {
                    let item = gj::get(last, "item").raw.to_vec();
                    gj::set_raw(&mut event, &format!("response.output.{i}"), item);
                }
                preceding.extend(events);
            }
            if self.unfinished_dispatcher() {
                return self.fail("incomplete apply_patch namespace dispatcher received from upstream".into());
            }
            // Unproven candidates stay ordinary; the bridge resolves or flushes them.
            for call in &mut self.records {
                if call.namespace.is_empty() && !call.ordinary {
                    preceding.append(&mut call.events);
                }
            }
        }
        let expanded = match self.expand_dispatcher(event, &original) {
            Ok(events) => events,
            Err(err) => return self.fail(err),
        };
        preceding.extend(expanded);
        let mut out = vec![];
        for e in preceding {
            let (converted, err) = self.bridge.transform(&e);
            out.extend(converted);
            if err.is_some() {
                self.failed = true;
                self.clear();
                return (out, err);
            }
        }
        if is_terminal(&kind) || kind == b"response.failed" {
            self.closed = true;
            self.clear();
        }
        (out, None)
    }

    /// Finish: the source response must have closed (and every patch call completed);
    /// the bridge's own Finish only checks arguments.
    pub fn finish(&self) -> Result<(), String> {
        self.bridge.finish()?;
        if self.closed || !self.active {
            return Ok(());
        }
        if self.unfinished_dispatcher() {
            return Err("incomplete apply_patch namespace dispatcher received from upstream".into());
        }
        Err("incomplete apply_patch source response received from upstream".into())
    }

    /// Stream: one upstream SSE line in, client SSE lines out. `event:` lines are held and
    /// renamed to match converted payloads; a premature `[DONE]` fails before any success
    /// marker. Each converted payload is a complete `data: …\n\n` frame.
    pub fn stream(&mut self, line: &[u8]) -> Transformed {
        if !self.active {
            return (vec![line.to_vec()], None);
        }
        if self.failed || self.transport_done {
            return (vec![], None);
        }
        if line.starts_with(b"event:") {
            self.event_line = Some(line.to_vec());
            return (vec![], None);
        }
        let Some(rest) = line.strip_prefix(b"data:") else {
            return (vec![line.to_vec()], None);
        };
        let payload = trim_space(rest);
        let (events, err) = if payload == b"[DONE]" {
            match self.finish() {
                Err(err) => self.fail(err),
                Ok(()) => {
                    // JSON completion and transport completion are separate boundaries.
                    self.transport_done = true;
                    self.clear();
                    self.event_line = None;
                    return (vec![line.to_vec()], None);
                }
            }
        } else {
            self.transform(payload)
        };
        if events.len() == 1 && events[0] == payload && err.is_none() {
            let mut out = vec![];
            if let Some(event_line) = self.event_line.take() {
                out.push(event_line);
            }
            out.push(line.to_vec());
            return (out, None);
        }
        let mut out = vec![];
        for event in &events {
            if let Some(event_line) = &self.event_line {
                if event.as_slice() == payload {
                    out.push(event_line.clone());
                } else {
                    out.push([&b"event: "[..], &gj::get(event, "type").bytes()].concat());
                }
            }
            out.push([&b"data: "[..], event, b"\n\n"].concat());
        }
        self.event_line = None;
        (out, err)
    }

    /// FinishStream: at EOF without a validated completion, the failure once (as SSE).
    pub fn finish_stream(&mut self) -> Transformed {
        if self.failed || self.transport_done {
            return (vec![], None);
        }
        match self.finish() {
            Ok(()) => (vec![], None),
            Err(err) => {
                let (events, err) = self.fail(err);
                let framed = events
                    .into_iter()
                    .map(|e| [&b"data: "[..], &e, b"\n\n"].concat())
                    .collect();
                (framed, err)
            }
        }
    }
}
