//! The Antigravity reasoning replay ledger (antigravity_reasoning_replay.go).
//!
//! Gemini-family models reject a function-call history whose native thought signatures
//! or call identities a client dropped or rewrote. After each completed turn the
//! executor records the turn's signatures and native function calls (`Accumulator`,
//! committed into `cpa_common::cache`'s replay cache per model and session). Before the
//! next request, `prepare` restores them into the matching parts of the history. A
//! signature finds its part by the part's text hash or, for legacy items, by position
//! under an unchanged context fingerprint. A native call is found by its ID, its
//! Claude-facing provenance ID, or its name and arguments.
// ponytail: Go applies items through a batch splicer that falls back to this sequential
// path; Go's differential tests assert both give identical payloads, so only the
// sequential path is ported. Go's debug-level replay logs are not reproduced.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use cpa_common::cache::{self, MAX_REPLAY_BYTES, MAX_REPLAY_ITEMS, ReplaySnapshot};
use cpa_common::gostr::{GoStr, lower_bytes, trim_space};
use cpa_common::json::{self as gj, AnyValue, GoValue, Kind, Res};
use cpa_common::signature as sig;
use cpa_core::exec::{ExecRequest, ExecStream};
use cpa_core::format::Format;
use futures_util::StreamExt;
use http::HeaderMap;
use sha2::{Digest, Sha256};

use crate::antigravity_request as ag;

const BYPASS: &[u8] = b"skip_thought_signature_validator";
const SIGNATURE_FIELDS: [&str; 3] = [
    "thoughtSignature",
    "thought_signature",
    "extra_content.google.thought_signature",
];

/// `strings.TrimSpace(r.Get(path).String())`.
fn field(r: &Res<'_>, path: &str) -> Vec<u8> {
    trim_space(&r.get(path).bytes()).to_vec()
}

fn text(r: &Res<'_>) -> String {
    String::from_utf8_lossy(trim_space(&r.bytes())).into_owned()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_model(content: &Res<'_>) -> bool {
    String::from_utf8_lossy(&field(content, "role")).go_eq_fold("model")
}

/// `antigravityHasNativeThoughtSignature`.
fn has_native_signature(signature: &[u8]) -> bool {
    let signature = trim_space(signature);
    !signature.is_empty() && signature != BYPASS
}

/// `out, _ = sjson.SetBytes(out, path, value)`: sjson returns nil on error.
fn set_str_go(out: &mut Vec<u8>, path: &str, value: &[u8]) {
    if !gj::set_str(out, path, value) {
        out.clear();
    }
}

/// `out, _ = sjson.DeleteBytes(out, path)`.
fn delete_go(out: &mut Vec<u8>, path: &str) {
    if !gj::delete(out, path) {
        out.clear();
    }
}

/// A JSON string as `json.Marshal` writes it.
fn quoted(s: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    gj::marshal_str(&mut out, s, true);
    out
}

/// `json.Marshal` of a Go map with these members, given in sorted key order.
fn object(members: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![b'{'];
    for (i, (key, value)) in members.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend(quoted(key.as_bytes()));
        out.push(b':');
        out.extend_from_slice(value);
    }
    out.push(b'}');
    out
}

/// `antigravityCanonicalReplayJSON`: Go's decode-then-marshal, else the trimmed text.
fn canonical(raw: &[u8]) -> Vec<u8> {
    match GoValue::parse_f64(raw) {
        Some(value) => value.marshal(),
        None => trim_space(raw).to_vec(),
    }
}

/// `antigravityFunctionCallKey`.
fn call_key(name: &[u8], args: &[u8], call_id: &[u8]) -> Vec<u8> {
    let name = trim_space(name);
    if name.is_empty() {
        return vec![];
    }
    let args = if trim_space(args).is_empty() {
        args.to_vec()
    } else {
        canonical(args)
    };
    let sum = Sha256::digest([name, b"\x00", &args, b"\x00", call_id].concat());
    format!("fc:{}", hex(&sum[..8])).into_bytes()
}

/// `antigravityReplayToolCallKeysFromPart`: the call's key with its own ID, if named.
fn tool_call_key(call: &Res<'_>) -> Option<Vec<u8>> {
    let call = gj::parse(call.raw());
    let mut call_id = field(&call, "call_id");
    if call_id.is_empty() {
        call_id = field(&call, "id");
    }
    let name = field(&call, "name");
    if name.is_empty() {
        return None;
    }
    let key = call_key(&name, call.get("args").raw(), &call_id);
    (!key.is_empty()).then_some(key)
}

fn tool_use_id(call_id: &[u8], name: &[u8], args: &[u8]) -> Vec<u8> {
    cpa_translate::gemini_claude_tool_use_id(call_id, name, args)
}

fn text_hash(kind: &str, text: &[u8]) -> String {
    hex(&Sha256::digest([kind.as_bytes(), b"\x00", text].concat()))
}

/// `antigravityReplayPartFingerprint`: the kind and hash of a text part, else empty.
fn fingerprint(part: &Res<'_>) -> (&'static str, String) {
    if part.get("functionCall").exists() || part.get("functionResponse").exists() {
        return ("", String::new());
    }
    let text = part.get("text");
    if !text.exists() {
        return ("", String::new());
    }
    let kind = if part.get("thought").bool() { "thought" } else { "text" };
    (kind, text_hash(kind, &text.bytes()))
}

/// `antigravityReplayPartOccurrence`.
fn part_occurrence(parts: &[Res<'_>], target: usize, kind: &str, hash: &str) -> i64 {
    parts[..target.min(parts.len())]
        .iter()
        .filter(|part| {
            let (k, f) = fingerprint(part);
            k == kind && f == hash
        })
        .count() as i64
}

/// `antigravitySetReplayItemContextHashValue`.
fn with_context_hash(mut item: Vec<u8>, hash: &str) -> Vec<u8> {
    if !hash.is_empty() {
        set_str_go(&mut item, "contextHash", hash.as_bytes());
    }
    item
}

/// `buildAntigravityThoughtSignatureItem` (`%q` is strconv.Quote).
fn signature_item(content: i64, part: i64, signature: &[u8], kind: &str, hash: &str) -> Vec<u8> {
    let mut item = format!(
        r#"{{"type":"thought_signature","thoughtSignature":{},"contentIndex":{content},"partIndex":{part}}}"#,
        cpa_common::gostr::quote(signature)
    )
    .into_bytes();
    if !kind.is_empty() {
        set_str_go(&mut item, "targetKind", kind.as_bytes());
    }
    if !hash.is_empty() {
        set_str_go(&mut item, "targetHash", hash.as_bytes());
    }
    item
}

/// `buildAntigravityFunctionCallPartItem`: `json.Marshal` of the item map; `None` when
/// Marshal fails on invalid raw arguments.
fn call_item(content: i64, part: i64, occurrence: i64, call: &Res<'_>, signature: &[u8]) -> Option<Vec<u8>> {
    let mut members = vec![];
    let args = call.get("args");
    if args.exists() {
        if args.kind == Kind::String {
            members.push(("args", quoted(&args.bytes())));
        } else {
            if !gj::std_valid(args.raw()) {
                return None;
            }
            members.push(("args", gj::compact(args.raw(), true)));
        }
    }
    let id = field(call, "id");
    if !id.is_empty() {
        members.push(("call_id", quoted(&id)));
    }
    members.push(("contentIndex", content.to_string().into_bytes()));
    members.push(("name", quoted(&call.get("name").bytes())));
    members.push(("partIndex", part.to_string().into_bytes()));
    members.push(("targetOccurrence", occurrence.to_string().into_bytes()));
    if !signature.is_empty() {
        members.push(("thoughtSignature", quoted(signature)));
    }
    members.push(("type", quoted(b"function_call_part")));
    Some(object(&members))
}

// ---------------------------------------------------------------------------------------
// Scope

/// The ledger entry one request reads and its turn commits to, and the cache state it
/// read (`antigravityReasoningReplayScope`).
#[derive(Clone, Default)]
pub(crate) struct Scope {
    model: String,
    session: String,
    snapshot: ReplaySnapshot,
}

impl Scope {
    pub(crate) fn valid(&self) -> bool {
        !self.model.trim().is_empty() && !self.session.trim().is_empty()
    }

    fn clear(&self) {
        cache::replay_delete_if_unchanged(&self.model, &self.session, &self.snapshot);
    }
}

/// The first non-empty trimmed value of a header (`antigravitySessionHeaderValue`).
fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get_all(name)
        .iter()
        .map(|v| String::from_utf8_lossy(trim_space(v.as_bytes())).into_owned())
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// `antigravityClaudeReplaySystemLane`: a hash of the Claude system prompt without its
/// `cache_control` markers, so a changed system prompt starts a new chain.
fn system_lane(payload: &[u8]) -> String {
    let system = gj::get(payload, "system");
    if !system.exists() {
        return String::new();
    }
    let Some(mut value) = GoValue::parse_f64(system.raw()) else {
        return String::new();
    };
    value.visit_mut(|v| {
        if let GoValue::Object(map) = v {
            map.retain(|key, _| !key.trim().go_eq_fold("cache_control"));
        }
    });
    hex(&Sha256::digest(value.marshal())[..16])
}

/// `antigravityReasoningReplayClientSessionKey`: an explicit downstream session, which
/// keeps identical prompts of separate client sessions on separate chains.
fn client_session_key(req: &ExecRequest) -> String {
    let raws: [&[u8]; 2] = [&req.original_body, &req.body];
    for raw in raws {
        if let Some(scope) = crate::replay::claude_code_session(raw, &req.headers) {
            let lane = system_lane(raw);
            return if lane.is_empty() {
                scope
            } else {
                format!("{scope}:context:{lane}")
            };
        }
    }
    for name in ["Session-Id", "Session_id"] {
        let value = header(&req.headers, name);
        if !value.is_empty() {
            return format!("responses:{value}");
        }
    }
    for raw in raws.into_iter().filter(|raw| !raw.is_empty()) {
        for path in ["session_id", "metadata.session_id"] {
            let value = text(&gj::get(raw, path));
            if !value.is_empty() {
                return format!("responses:{value}");
            }
        }
    }
    let execution = req.execution_session.as_deref().map(str::trim).unwrap_or_default();
    if !execution.is_empty() {
        return format!("execution:{execution}");
    }
    for raw in raws {
        let value = text(&gj::get(raw, "prompt_cache_key"));
        if !value.is_empty() {
            return format!("prompt-cache:{value}");
        }
    }
    let derived = req.derived_session.as_deref().map(str::trim).unwrap_or_default();
    if !derived.is_empty() {
        return format!("derived:{derived}");
    }
    String::new()
}

/// `antigravityReasoningReplayScopeFromPayload`: the payload's session ID, else the
/// stable (or random) session ID Go derives from it.
fn scope_from_payload(model: &str, payload: &[u8]) -> Scope {
    let mut session = String::new();
    if !payload.is_empty() {
        for path in ["sessionId", "session_id", "request.sessionId", "request.session_id"] {
            session = text(&gj::get(payload, path));
            if !session.is_empty() {
                break;
            }
        }
    }
    if session.is_empty() {
        let stable = ag::stable_session_id(payload);
        let stable = stable.trim();
        if !stable.is_empty() {
            session = stable.strip_prefix('-').unwrap_or(stable).to_owned();
            if session.is_empty() {
                session = stable.to_owned();
            }
        }
    }
    if session.is_empty() {
        return Scope::default();
    }
    Scope {
        model: model.trim().to_owned(),
        session: format!("session:{session}"),
        snapshot: ReplaySnapshot::default(),
    }
}

/// `antigravityReasoningReplayScopeFromRequest`.
fn scope_from_request(req: &ExecRequest, model: &str, payload: &[u8]) -> Scope {
    let session = client_session_key(req);
    if !session.is_empty() {
        return Scope {
            model: model.to_owned(),
            session,
            snapshot: ReplaySnapshot::default(),
        };
    }
    for raw in [payload, &req.body[..]] {
        let scope = scope_from_payload(model, raw);
        if scope.valid() {
            return scope;
        }
    }
    Scope::default()
}

// ---------------------------------------------------------------------------------------
// Tool schemas

type Schemas = HashMap<Vec<u8>, GoValue>;

/// `antigravityReplayToolSchemasFromRequests`: Claude tool schemas by declared and
/// sanitized name (the first declaration wins).
fn tool_schemas(raws: &[&[u8]]) -> Schemas {
    let mut out = Schemas::new();
    for raw in raws.iter().filter(|raw| !raw.is_empty()) {
        let names = cpa_translate::sanitized_function_name_map(raw);
        let tools = gj::get(raw, "tools");
        if !tools.is_array() {
            continue;
        }
        for tool in tools.array() {
            let mut candidates = vec![tool.clone()];
            let function = tool.get("function");
            if function.exists() {
                candidates.push(function);
            }
            for candidate in candidates {
                let name = field(&candidate, "name");
                if name.is_empty() {
                    continue;
                }
                let Some(schema) = ["input_schema", "parameters", "parametersJsonSchema"]
                    .iter()
                    .map(|path| candidate.get(path))
                    .find(|value| value.exists() && value.is_object())
                else {
                    continue;
                };
                let Some(value) = GoValue::parse_f64(schema.raw()) else {
                    continue;
                };
                let sanitized = cpa_translate::map_sanitized_function_name(&names, &name);
                for schema_name in [name, sanitized] {
                    if !schema_name.is_empty() {
                        out.entry(schema_name).or_insert_with(|| value.clone());
                    }
                }
            }
        }
    }
    out
}

/// `antigravityReplayJSONValue`: a JSON value, or a string holding one, decoded.
fn json_value(r: &Res<'_>) -> Option<GoValue> {
    let raw = if r.kind == Kind::String {
        r.bytes()
    } else {
        Cow::Borrowed(r.raw())
    };
    if trim_space(&raw).is_empty() {
        return None;
    }
    GoValue::parse_f64(&raw)
}

/// `antigravityNormalizeReplayToolValue`: object members equal to their schema default
/// are dropped, recursively.
fn normalize_value(value: &GoValue, schema: Option<&GoValue>) -> GoValue {
    let schema = match schema {
        Some(GoValue::Object(map)) => Some(map),
        _ => None,
    };
    match value {
        GoValue::Object(map) => {
            let properties = match schema.and_then(|s| s.get("properties")) {
                Some(GoValue::Object(properties)) => Some(properties),
                _ => None,
            };
            let mut out = BTreeMap::new();
            for (key, child) in map {
                let child_schema = properties.and_then(|p| p.get(key));
                let normalized = normalize_value(child, child_schema);
                if let Some(GoValue::Object(property)) = child_schema
                    && let Some(default) = property.get("default")
                    && normalized == normalize_value(default, child_schema)
                {
                    continue;
                }
                out.insert(key.clone(), normalized);
            }
            GoValue::Object(out)
        }
        GoValue::Array(items) => {
            let item_schema = schema.and_then(|s| s.get("items"));
            GoValue::Array(items.iter().map(|item| normalize_value(item, item_schema)).collect())
        }
        other => other.clone(),
    }
}

/// `antigravityFunctionCallMatchesReplayItem`: same name and arguments, or arguments
/// equal once schema defaults are dropped.
fn call_matches(call: &Res<'_>, item: &Res<'_>, schemas: &Schemas) -> bool {
    let name = field(item, "name");
    if name.is_empty() || field(call, "name") != name {
        return false;
    }
    let (current, native) = (call.get("args"), item.get("args"));
    if !current.exists() || !native.exists() {
        return false;
    }
    if canonical(current.raw()) == canonical(native.raw()) {
        return true;
    }
    let Some(schema) = schemas.get(&name) else {
        return false;
    };
    match (json_value(&current), json_value(&native)) {
        (Some(current), Some(native)) => {
            normalize_value(&current, Some(schema)) == normalize_value(&native, Some(schema))
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------
// Request index

struct Content<'a> {
    content: Res<'a>,
    parts: Vec<Res<'a>>,
}

#[derive(Clone)]
struct Located<'a> {
    content: usize,
    part: usize,
    part_res: Res<'a>,
    call: Res<'a>,
}

/// The running SHA-256 of the replay context, summed after every content boundary.
struct Fingerprints {
    hasher: Sha256,
    wrote: bool,
    sums: Vec<String>,
}

impl Fingerprints {
    fn write(&mut self, data: &[u8]) {
        if !data.is_empty() {
            self.hasher.update(data);
            self.wrote = true;
        }
    }

    /// The empty fingerprint until a byte was hashed.
    fn sum(&self) -> String {
        if self.wrote {
            hex(&self.hasher.clone().finalize())
        } else {
            String::new()
        }
    }
}

/// A view of one exact payload revision (`antigravityReplayRequestIndex`); rebuilt after
/// every change.
struct Index<'a> {
    valid: bool,
    contents: Vec<Content<'a>>,
    calls: HashMap<Vec<u8>, Located<'a>>,
    responses: HashMap<Vec<u8>, usize>,
    fingerprints: RefCell<Fingerprints>,
}

impl<'a> Index<'a> {
    fn new(payload: &'a [u8]) -> Self {
        let root = gj::get(payload, "request.contents");
        let valid = root.is_array();
        let mut contents = vec![];
        let mut calls = HashMap::new();
        let mut responses = HashMap::new();
        if valid {
            for (ci, content) in root.array().into_iter().enumerate() {
                let parts = content.get("parts");
                let parts = if parts.is_array() { parts.array() } else { vec![] };
                for (pi, part) in parts.iter().enumerate() {
                    let call = part.get("functionCall");
                    if call.exists() {
                        let id = field(&call, "id");
                        if !id.is_empty() {
                            calls.entry(id).or_insert_with(|| Located {
                                content: ci,
                                part: pi,
                                part_res: part.clone(),
                                call: call.clone(),
                            });
                        }
                    }
                    let response = part.get("functionResponse");
                    if response.exists() {
                        let id = field(&response, "id");
                        if !id.is_empty() {
                            responses.entry(id).or_insert(ci);
                        }
                    }
                }
                contents.push(Content { content, parts });
            }
        }
        let mut fingerprints = Fingerprints {
            hasher: Sha256::new(),
            wrote: false,
            sums: vec![],
        };
        if valid {
            for path in ["request.systemInstruction", "request.tools", "request.toolConfig"] {
                let value = gj::get(payload, path);
                if value.exists() {
                    fingerprints.write(path.as_bytes());
                    fingerprints.write(b"\x00");
                    fingerprints.write(&canonical(value.raw()));
                    fingerprints.write(b"\x00");
                }
            }
        }
        let first = fingerprints.sum();
        fingerprints.sums.push(first);
        Index {
            valid,
            contents,
            calls,
            responses,
            fingerprints: RefCell::new(fingerprints),
        }
    }

    /// The context fingerprint before content `before`: system instruction, tools and
    /// tool config, then each earlier content's role and parts without signatures.
    fn context(&self, before: i64) -> String {
        if !self.valid || before < 0 || before as usize > self.contents.len() {
            return String::new();
        }
        let before = before as usize;
        let mut f = self.fingerprints.borrow_mut();
        while f.sums.len() <= before {
            let content = &self.contents[f.sums.len() - 1];
            f.write(lower_bytes(trim_space(&content.content.get("role").bytes())).as_bytes());
            f.write(b"\x00");
            for part in &content.parts {
                let mut normalized = part.raw().to_vec();
                for path in SIGNATURE_FIELDS {
                    delete_go(&mut normalized, path);
                }
                f.write(&canonical(&normalized));
                f.write(b"\x00");
            }
            let sum = f.sum();
            f.sums.push(sum);
        }
        f.sums[before].clone()
    }

    fn context_matches(&self, item: &Res<'_>, content: i64) -> bool {
        let expected = field(item, "contextHash");
        expected.is_empty() || expected == self.context(content).as_bytes()
    }

    fn call(&self, id: &[u8]) -> Option<&Located<'a>> {
        self.calls.get(trim_space(id))
    }

    /// `pendingModelContentIndex`: where the answer's parts go, and its first part index.
    fn pending_model_content(&self) -> (i64, i64) {
        let Some(last) = self.contents.last() else {
            return (0, 0);
        };
        if is_model(&last.content) && !last.parts.iter().any(|p| p.get("functionResponse").exists()) {
            return (self.contents.len() as i64 - 1, last.parts.len() as i64);
        }
        (self.contents.len() as i64, 0)
    }

    /// `functionResponseContentIndexForReplay`: the content holding the call's response,
    /// under its native or provenance ID, and the ID it used.
    fn response_content(&self, item: &Res<'_>) -> Option<(usize, Vec<u8>)> {
        let call_id = field(item, "call_id");
        let stable = tool_use_id(&call_id, &field(item, "name"), item.get("args").raw());
        let mut candidates = vec![call_id.clone()];
        if !stable.is_empty() && stable != call_id {
            candidates.push(stable);
        }
        candidates
            .into_iter()
            .find_map(|id| self.responses.get(trim_space(&id)).map(|&ci| (ci, id)))
    }

    /// `functionCallPartLocationForReplayWithSchemas`.
    fn call_location(&self, item: &Res<'_>, schemas: &Schemas) -> Option<Located<'a>> {
        let name = field(item, "name");
        let args = item.get("args");
        if name.is_empty() || !args.exists() {
            return None;
        }
        let mut call_id = field(item, "call_id");
        if call_id.is_empty() {
            call_id = field(item, "id");
        }
        let stable = tool_use_id(&call_id, &name, args.raw());
        let mut candidates = vec![call_id.clone()];
        if !stable.is_empty() && stable != call_id {
            candidates.push(stable.clone());
        }
        for candidate in candidates.iter().filter(|c| !c.is_empty()) {
            let Some(location) = self.call(candidate) else {
                continue;
            };
            // An exact ID match decides: a drifted context or changed call rejects it.
            if self.context_matches(item, location.content as i64) && call_matches(&location.call, item, schemas) {
                return Some(location.clone());
            }
            return None;
        }
        let eligible = |part: &Res<'a>| {
            let call = part.get("functionCall");
            let id = call.get("id").bytes();
            let mismatched = ag::is_claude_provenance_id(&id) && *id != *stable;
            (call.exists() && !mismatched && call_matches(&call, item, schemas)).then_some(call)
        };
        let cached = item.get("contentIndex").int();
        let occurrence = item.get("targetOccurrence");
        if occurrence.exists() {
            if cached < 0 || cached as usize >= self.contents.len() || !self.context_matches(item, cached) {
                return None;
            }
            let content = cached as usize;
            return self.contents[content]
                .parts
                .iter()
                .enumerate()
                .filter_map(|(pi, part)| eligible(part).map(|call| (pi, part, call)))
                .nth(usize::try_from(occurrence.int()).ok()?)
                .map(|(part, part_res, call)| Located {
                    content,
                    part,
                    part_res: part_res.clone(),
                    call,
                });
        }
        let mut matches = vec![];
        for (ci, content) in self.contents.iter().enumerate() {
            if !self.context_matches(item, ci as i64) {
                continue;
            }
            for (pi, part) in content.parts.iter().enumerate() {
                if let Some(call) = eligible(part) {
                    matches.push(Located {
                        content: ci,
                        part: pi,
                        part_res: part.clone(),
                        call,
                    });
                }
            }
        }
        if matches.len() == 1 { matches.pop() } else { None }
    }

    /// `functionCallProvenanceLocation`: the call under the item's provenance ID, whatever
    /// the context.
    fn provenance_location(&self, item: &Res<'_>, schemas: &Schemas) -> Option<Located<'a>> {
        let name = field(item, "name");
        let args = item.get("args");
        let call_id = field(item, "call_id");
        if name.is_empty() || !args.exists() || call_id.is_empty() {
            return None;
        }
        let stable = tool_use_id(&call_id, &name, args.raw());
        if stable.is_empty() || stable == call_id {
            return None;
        }
        let location = self.call(&stable)?;
        call_matches(&location.call, item, schemas).then(|| location.clone())
    }

    /// `thoughtSignaturePartIndex`: the part a signature belongs to. A target hash pins
    /// it to an unchanged part regardless of context; a positional legacy item needs an
    /// unchanged context.
    fn signature_part(&self, item: &Res<'_>) -> Option<(usize, usize)> {
        let ci = item.get("contentIndex").int();
        if ci < 0 || ci as usize >= self.contents.len() {
            return None;
        }
        let content = &self.contents[ci as usize];
        if !is_model(&content.content) {
            return None;
        }
        let parts = &content.parts;
        let kind = field(item, "targetKind");
        let hash = field(item, "targetHash");
        let matches = |part: &Res<'_>| {
            let (k, f) = fingerprint(part);
            f.as_bytes() == hash && (kind.is_empty() || k.as_bytes() == kind)
        };
        let candidate = usize::try_from(item.get("partIndex").int())
            .ok()
            .filter(|&c| c < parts.len());
        let part = if !hash.is_empty() {
            let occurrence = item.get("targetOccurrence");
            if occurrence.exists() {
                let wanted = usize::try_from(occurrence.int()).ok()?;
                parts
                    .iter()
                    .enumerate()
                    .filter(|(_, part)| matches(part))
                    .nth(wanted)
                    .map(|(pi, _)| pi)
            } else {
                candidate
                    .filter(|&c| matches(&parts[c]))
                    .or_else(|| parts.iter().position(matches))
            }
        } else {
            if !self.context_matches(item, ci) {
                return None;
            }
            let semantic = |part: &Res<'_>| !fingerprint(part).0.is_empty();
            candidate
                .filter(|&c| parts[c].kind != Kind::Null && semantic(&parts[c]))
                .or_else(|| parts.iter().rposition(semantic))
        };
        part.map(|pi| (ci as usize, pi))
    }

    /// `reasoningReplayItemsFromRequest`: the signatures and calls the request already
    /// carries, as ledger items. `None` when the contents are not an array.
    fn items_from_request(&self) -> Option<Vec<Vec<u8>>> {
        if !self.valid {
            return None;
        }
        let mut items = vec![];
        for (ci, content) in self.contents.iter().enumerate() {
            if !is_model(&content.content) || content.parts.is_empty() {
                continue;
            }
            let mut occurrences: HashMap<Vec<u8>, i64> = HashMap::new();
            for (pi, part) in content.parts.iter().enumerate() {
                let mut signature = ag::native_part_signature(part);
                if !has_native_signature(&signature) {
                    signature.clear();
                }
                let call = part.get("functionCall");
                if call.exists() {
                    let key = call_key(&call.get("name").bytes(), call.get("args").raw(), b"");
                    let occurrence = occurrences.get(&key).copied().unwrap_or(0);
                    if !key.is_empty() {
                        occurrences.insert(key, occurrence + 1);
                    }
                    if let Some(item) = call_item(ci as i64, pi as i64, occurrence, &call, &signature) {
                        items.push(with_context_hash(item, &self.context(ci as i64)));
                    }
                    continue;
                }
                if signature.is_empty() {
                    continue;
                }
                let mut target = pi;
                let (mut kind, mut hash) = fingerprint(part);
                if hash.is_empty() && pi > 0 {
                    target = pi - 1;
                    (kind, hash) = fingerprint(&content.parts[target]);
                }
                if hash.is_empty() {
                    continue;
                }
                let mut item = signature_item(ci as i64, target as i64, &signature, kind, &hash);
                if !gj::set_int(
                    &mut item,
                    "targetOccurrence",
                    part_occurrence(&content.parts, target, kind, &hash),
                ) {
                    item.clear();
                }
                items.push(with_context_hash(item, &self.context(ci as i64)));
            }
        }
        Some(items)
    }

    /// One item of `filterAntigravityReasoningReplayItemsForRequestWithIndex`: whether
    /// replaying it can change the request.
    fn eligible(&self, item: &Res<'_>, schemas: &Schemas) -> bool {
        match field(item, "type").as_slice() {
            b"function_call_part" => {
                let signature = field(item, "thoughtSignature");
                if let Some(location) = self.call_location(item, schemas) {
                    let restore = field(&location.call, "id") != field(item, "call_id")
                        || canonical(location.call.get("args").raw()) != canonical(item.get("args").raw());
                    return restore
                        || !(signature.is_empty()
                            || has_native_signature(&location.part_res.get("thoughtSignature").bytes()));
                }
                // An exact provenance ID restores the native identity even under drift.
                if self.provenance_location(item, schemas).is_some() {
                    return true;
                }
                if field(item, "call_id").is_empty() {
                    return false;
                }
                let Some((response, _)) = self.response_content(item) else {
                    return false;
                };
                self.context_matches(item, response as i64)
                    || (response > 0
                        && is_model(&self.contents[response - 1].content)
                        && self.context_matches(item, response as i64 - 1))
            }
            b"thought_signature" => !self.signature_part(item).is_some_and(|(ci, pi)| {
                has_native_signature(&self.contents[ci].parts[pi].get("thoughtSignature").bytes())
            }),
            _ => false,
        }
    }

    /// One item of `insertAntigravityReasoningReplayItemsWithSchemas`: the updated
    /// payload, or `None` when the item changes nothing.
    fn insert(&self, payload: &[u8], item: &Res<'_>, schemas: &Schemas) -> Option<Vec<u8>> {
        match field(item, "type").as_slice() {
            b"thought_signature" => {
                let signature = field(item, "thoughtSignature");
                if signature.is_empty() {
                    return None;
                }
                let (ci, pi) = self.signature_part(item)?;
                let part_path = format!("request.contents.{ci}.parts.{pi}");
                let path = format!("{part_path}.thoughtSignature");
                if has_native_signature(&gj::get(payload, &path).bytes()) {
                    return None;
                }
                let content = item.get("contentIndex").int();
                let mut out = remove_signature_elsewhere(payload.to_vec(), content, &signature, &part_path);
                gj::set_str(&mut out, &path, &signature).then_some(out)
            }
            b"function_call_part" => self.merge_call(payload, item, schemas),
            _ => None,
        }
    }

    /// `mergeAntigravityFunctionCallPartReplayWithSchemas`. Every lookup reads this
    /// index before the payload changes.
    fn merge_call(&self, payload: &[u8], item: &Res<'_>, schemas: &Schemas) -> Option<Vec<u8>> {
        let name = field(item, "name");
        let args = item.get("args");
        let call_id = field(item, "call_id");
        let signature = field(item, "thoughtSignature");
        if name.is_empty() || !args.exists() {
            return None;
        }
        if let Some(location) = self.call_location(item, schemas) {
            let legacy = schemas.contains_key(&name);
            return restore_native_call(payload, location.content, location.part, item, legacy, true);
        }
        // The context drifted, but an exact provenance ID proves the call's identity, and
        // Gemini validates a signature's own integrity only: restore both.
        if let Some(location) = self.provenance_location(item, schemas) {
            return restore_native_call(payload, location.content, location.part, item, false, true);
        }
        if call_id.is_empty() {
            // Only an exact semantic match may carry a signature without a native ID.
            return None;
        }
        let stable = tool_use_id(&call_id, &name, args.raw());
        if self.call(&call_id).is_some() || (!stable.is_empty() && self.call(&stable).is_some()) {
            // The client changed a call that is still in the history: never sign it or
            // insert a second copy.
            return None;
        }
        if let Some((response, response_id)) = self.response_content(item) {
            if response > 0
                && is_model(&self.contents[response - 1].content)
                && self.context_matches(item, response as i64 - 1)
                && let Some(updated) = append_call(payload, response - 1, &name, &call_id, &signature, &args)
            {
                return Some(restore_response_identity(updated, &response_id, &call_id, &name));
            }
            if self.context_matches(item, response as i64)
                && let Some(updated) = insert_call_before(payload, response, &name, &call_id, &signature, &args)
            {
                return Some(restore_response_identity(updated, &response_id, &call_id, &name));
            }
        }
        let ci = resolve_content_index(payload, item.get("contentIndex").int());
        if ci < 0 || !self.context_matches(item, ci) {
            return None;
        }
        let pi = item.get("partIndex").int();
        let call = || {
            let mut members = vec![];
            if args.kind == Kind::String {
                members.push(("args", quoted(&args.bytes())));
            } else if let Some(parsed) = GoValue::parse_f64(args.raw()) {
                members.push(("args", parsed.marshal()));
            }
            if !call_id.is_empty() {
                members.push(("id", quoted(&call_id)));
            }
            members.push(("name", quoted(&name)));
            object(&members)
        };
        let Some(part_path) = existing_part_path(payload, ci, pi) else {
            let mut members = vec![("functionCall", call())];
            if !signature.is_empty() {
                members.push(("thoughtSignature", quoted(&signature)));
            }
            let mut out = payload.to_vec();
            return gj::set_raw(&mut out, &part_write_path(payload, ci, pi), object(&members)).then_some(out);
        };
        let mut out = payload.to_vec();
        let mut changed = false;
        let signature_path = format!("{part_path}.thoughtSignature");
        if !signature.is_empty() && !has_native_signature(&gj::get(&out, &signature_path).bytes()) {
            out = remove_signature_elsewhere(out, ci, &signature, &part_path);
            changed |= gj::set_str(&mut out, &signature_path, &signature);
        }
        let call_path = format!("{part_path}.functionCall");
        if !gj::get(&out, &call_path).exists() {
            changed |= gj::set_raw(&mut out, &call_path, call());
        }
        changed.then_some(out)
    }
}

/// `antigravityRemoveThoughtSignatureFromOtherParts`: one signature belongs to one part.
fn remove_signature_elsewhere(mut out: Vec<u8>, content: i64, signature: &[u8], keep: &str) -> Vec<u8> {
    let signature = trim_space(signature);
    let parts_path = format!("request.contents.{content}.parts");
    let parts = gj::get(&out, &parts_path);
    if signature.is_empty() || !parts.is_array() {
        return out;
    }
    let paths: Vec<String> = parts
        .array()
        .iter()
        .enumerate()
        .map(|(pi, part)| (format!("{parts_path}.{pi}"), part))
        .filter(|(path, part)| path != keep && ag::native_part_signature(part) == signature)
        .map(|(path, _)| path)
        .collect();
    for path in paths {
        for f in SIGNATURE_FIELDS {
            delete_go(&mut out, &format!("{path}.{f}"));
        }
    }
    out
}

/// `antigravityReasoningReplayResolveContentIndex`.
fn resolve_content_index(payload: &[u8], cached: i64) -> i64 {
    let contents = gj::get(payload, "request.contents");
    if !contents.is_array() {
        return cached;
    }
    if cached >= 0 && (cached as usize) < contents.array().len() {
        cached
    } else {
        -1
    }
}

/// `antigravityExistingReplayPartPath`.
fn existing_part_path(payload: &[u8], content: i64, part: i64) -> Option<String> {
    if content < 0 || part < 0 {
        return None;
    }
    let parts_path = format!("request.contents.{content}.parts");
    let parts = gj::get(payload, &parts_path);
    if !parts.is_array() {
        return None;
    }
    let parts = parts.array();
    match parts.get(part as usize) {
        Some(p) if p.kind != Kind::Null => Some(format!("{parts_path}.{part}")),
        _ => None,
    }
}

/// `antigravityReplayPartWritePath`.
fn part_write_path(payload: &[u8], content: i64, part: i64) -> String {
    if let Some(path) = existing_part_path(payload, content, part) {
        return path;
    }
    let parts_path = format!("request.contents.{content}.parts");
    if gj::get(payload, &parts_path).is_array() {
        format!("{parts_path}.-1")
    } else {
        format!("{parts_path}.0")
    }
}

/// A `functionCall` map with gjson's `args.Value()`, marshaled; `None` where Marshal
/// fails (infinite numbers).
fn value_call(name: &[u8], call_id: &[u8], args: &Res<'_>) -> Option<Vec<u8>> {
    let mut members = vec![];
    if args.exists() {
        members.push(("args", AnyValue::from_res(args).marshal()?));
    }
    if !call_id.is_empty() {
        members.push(("id", quoted(call_id)));
    }
    members.push(("name", quoted(name)));
    Some(object(&members))
}

/// `insertAntigravityModelFunctionCallBeforeContent`: a model turn with the call before
/// content `before`. The whole contents array is re-marshaled, as Go does.
fn insert_call_before(
    payload: &[u8],
    before: usize,
    name: &[u8],
    call_id: &[u8],
    signature: &[u8],
    args: &Res<'_>,
) -> Option<Vec<u8>> {
    let contents = gj::get(payload, "request.contents");
    if !contents.is_array() {
        return None;
    }
    let contents = contents.array();
    if before > contents.len() {
        return None;
    }
    let signature = if signature.is_empty() { BYPASS } else { signature };
    let part = object(&[
        ("functionCall", value_call(name, call_id, args)?),
        ("thoughtSignature", quoted(signature)),
    ]);
    let added = object(&[("parts", [b"[", &part[..], b"]"].concat()), ("role", quoted(b"model"))]);
    let mut items = Vec::with_capacity(contents.len() + 1);
    for content in &contents[..before] {
        items.push(AnyValue::from_res(content).marshal()?);
    }
    items.push(added);
    for content in &contents[before..] {
        items.push(AnyValue::from_res(content).marshal()?);
    }
    let mut out = payload.to_vec();
    gj::set_raw(&mut out, "request.contents", gj::join(&items)).then_some(out)
}

/// `appendAntigravityFunctionCallToModelContent`: a parallel call appended to a model
/// turn; the turn's first call gets the bypass signature when it has none.
fn append_call(
    payload: &[u8],
    content: usize,
    name: &[u8],
    call_id: &[u8],
    signature: &[u8],
    args: &Res<'_>,
) -> Option<Vec<u8>> {
    let content_path = format!("request.contents.{content}");
    let parts_path = format!("{content_path}.parts");
    let role = gj::get(payload, &format!("{content_path}.role"));
    let parts = gj::get(payload, &parts_path);
    if !String::from_utf8_lossy(trim_space(&role.bytes())).go_eq_fold("model") || !parts.is_array() {
        return None;
    }
    let mut members = vec![("functionCall", value_call(name, call_id, args)?)];
    let mut signature = signature.to_vec();
    if signature.is_empty() {
        let mut has_call = false;
        parts.each(|_, part| {
            has_call = part.get("functionCall").exists();
            !has_call
        });
        if !has_call {
            signature = BYPASS.to_vec();
        }
    }
    if !signature.is_empty() {
        members.push(("thoughtSignature", quoted(&signature)));
    }
    let mut out = payload.to_vec();
    gj::set_raw(&mut out, &format!("{parts_path}.-1"), object(&members)).then_some(out)
}

/// Every function response under `current` takes the native ID and name.
fn rewrite_response_ids(mut out: Vec<u8>, current: &[u8], native_id: &[u8], native_name: &[u8]) -> Vec<u8> {
    let mut paths = vec![];
    gj::get(&out, "request.contents").each(|content_key, content| {
        content.get("parts").each(|part_key, part| {
            let response = part.get("functionResponse");
            if response.exists() && field(&response, "id") == current {
                paths.push(format!(
                    "request.contents.{}.parts.{}.functionResponse",
                    content_key.int(),
                    part_key.int()
                ));
            }
            true
        });
        true
    });
    for path in paths {
        set_str_go(&mut out, &format!("{path}.id"), native_id);
        set_str_go(&mut out, &format!("{path}.name"), native_name);
    }
    out
}

/// `restoreAntigravityFunctionResponseReplayIdentity`.
fn restore_response_identity(payload: Vec<u8>, current: &[u8], native_id: &[u8], native_name: &[u8]) -> Vec<u8> {
    let (current, native_id, native_name) = (trim_space(current), trim_space(native_id), trim_space(native_name));
    if current.is_empty() || native_id.is_empty() || native_name.is_empty() || current == native_id {
        return payload;
    }
    rewrite_response_ids(payload, current, native_id, native_name)
}

/// `antigravityFunctionResponsesCanRestoreID`: no response under `current` names another
/// tool.
fn responses_can_restore_id(payload: &[u8], current: &[u8], native_name: &[u8]) -> bool {
    if current.is_empty() {
        return true;
    }
    let contents = gj::get(payload, "request.contents");
    if !contents.is_array() {
        return false;
    }
    let mut valid = true;
    contents.each(|_, content| {
        content.get("parts").each(|_, part| {
            let response = part.get("functionResponse");
            if !response.exists() || field(&response, "id") != current {
                return true;
            }
            let name = field(&response, "name");
            valid = name.is_empty() || name == b"unknown" || name == native_name;
            valid
        });
        valid
    });
    valid
}

/// `antigravityNativeFunctionCallJSON`.
fn native_call(item: &Res<'_>, fallback_id: &[u8]) -> Option<Vec<u8>> {
    let name = field(item, "name");
    let args = item.get("args");
    if name.is_empty() || !args.exists() {
        return None;
    }
    let mut call = br#"{"name":""}"#.to_vec();
    set_str_go(&mut call, "name", &name);
    let mut call_id = field(item, "call_id");
    if call_id.is_empty() {
        call_id = fallback_id.to_vec();
    }
    if !call_id.is_empty() {
        set_str_go(&mut call, "id", &call_id);
    }
    let ok = if args.kind == Kind::String {
        let text = args.bytes();
        let parsed = gj::parse(&text);
        if parsed.exists() {
            gj::set_raw(&mut call, "args", parsed.raw())
        } else {
            gj::set_str(&mut call, "args", &text)
        }
    } else {
        gj::set_raw(&mut call, "args", args.raw())
    };
    if !ok {
        call.clear();
    }
    Some(call)
}

/// `restoreAntigravityNativeFunctionCallReplay`: one call back to its native identity
/// and, when `allow_signature`, its signature. A call whose ID is neither native nor
/// provenance (and not a schema-matched legacy ID) only gets the signature.
fn restore_native_call(
    payload: &[u8],
    content: usize,
    part: usize,
    item: &Res<'_>,
    allow_legacy: bool,
    allow_signature: bool,
) -> Option<Vec<u8>> {
    let part_path = format!("request.contents.{content}.parts.{part}");
    let signature_path = format!("{part_path}.thoughtSignature");
    let current = gj::get(payload, &format!("{part_path}.functionCall"));
    if !current.exists() {
        return None;
    }
    let current_id = field(&current, "id");
    let native_id = field(item, "call_id");
    let native_name = field(item, "name");
    let signature = field(item, "thoughtSignature");
    if !(current_id == native_id || ag::is_claude_provenance_id(&current_id) || allow_legacy) {
        if !allow_signature || signature.is_empty() || has_native_signature(&gj::get(payload, &signature_path).bytes())
        {
            return None;
        }
        let mut out = remove_signature_elsewhere(payload.to_vec(), content as i64, &signature, &part_path);
        return gj::set_str(&mut out, &signature_path, &signature).then_some(out);
    }
    if current_id != native_id && !responses_can_restore_id(payload, &current_id, &native_name) {
        return None;
    }
    let call = native_call(item, &current_id)?;
    let mut out = payload.to_vec();
    if !gj::set_raw(&mut out, &format!("{part_path}.functionCall"), &call) {
        return None;
    }
    for f in SIGNATURE_FIELDS {
        delete_go(&mut out, &format!("{part_path}.{f}"));
    }
    if allow_signature && !signature.is_empty() {
        out = remove_signature_elsewhere(out, content as i64, &signature, &part_path);
        set_str_go(&mut out, &signature_path, &signature);
    }
    if !current_id.is_empty() && !native_id.is_empty() && current_id != native_id {
        out = rewrite_response_ids(out, &current_id, &native_id, &native_name);
    }
    (out != payload).then_some(out)
}

/// `applyAntigravityReasoningReplayItems` (sequential): each eligible item applied to
/// the payload the previous one produced. `None` when nothing changed.
// ponytail: the history is indexed again after each applied item, so restoring n items
// walks the body n times (Go's sequential path does the same; its batch splicer, which
// Go's tests compare with it, does one pass). Port the splicer if replay of long
// histories shows up in request preparation time.
fn apply_items(payload: &[u8], items: &[Vec<u8>], schemas: &Schemas) -> Option<Vec<u8>> {
    let mut current = payload.to_vec();
    let mut changed = false;
    let mut next = 0;
    while next < items.len() {
        let index = Index::new(&current);
        let mut updated = None;
        while next < items.len() {
            let item = gj::parse(&items[next]);
            next += 1;
            if index.eligible(&item, schemas)
                && let Some(out) = index.insert(&current, &item, schemas)
            {
                updated = Some(out);
                break;
            }
        }
        drop(index);
        match updated {
            Some(out) => {
                current = out;
                changed = true;
            }
            None => break,
        }
    }
    changed.then_some(current)
}

// ---------------------------------------------------------------------------------------
// Request preparation

/// `prepareAntigravityGeminiReasoningReplayPayload` for Gemini-family models: the
/// ledger's signatures and native calls restored, then the cache-independent history
/// repairs and Go's pairing check. A pairing that only replay broke clears the ledger
/// and sends the original payload; any other broken pairing is a 400 (`Err`).
pub(crate) fn prepare(req: &ExecRequest, model: &str, payload: Vec<u8>) -> Result<(Vec<u8>, Scope), String> {
    if !ag::uses_reasoning_replay(model) {
        return Ok((payload, Scope::default()));
    }
    let mut scope = scope_from_request(req, model, &payload);
    let mut replayed = None;
    if scope.valid() {
        let (items, snapshot) = cache::replay_get_with_snapshot(&scope.model, &scope.session);
        scope.snapshot = snapshot;
        if let Some(items) = items.filter(|items| !items.is_empty()) {
            let schemas = if req.source_format == Format::Claude {
                tool_schemas(&[&req.original_body, &req.body])
            } else {
                Schemas::new()
            };
            replayed = apply_items(&payload, &items, &schemas);
        }
    }
    let (working, original) = match replayed {
        Some(updated) => (updated, Some(payload)),
        None => (payload, None),
    };
    let updated = ag::repair_history(working);
    let Err(error) = sig::validate_gemini_function_call_pairing(&updated) else {
        return Ok((updated, scope));
    };
    if let Some(original) = original
        && sig::validate_gemini_function_call_pairing(&original).is_ok()
        && scope.valid()
    {
        scope.clear();
        tracing::warn!(
            "antigravity executor: reasoning replay broke Gemini function call pairing ({error}); degrading to original payload"
        );
        return Ok((original, scope));
    }
    Err(format!(
        "antigravity executor: invalid Gemini function call history: {error}"
    ))
}

/// `clearAntigravityReasoningReplayOnInvalidSignature`: a 400 that mentions a signature
/// drops the ledger entry the request read.
pub(crate) fn clear_on_invalid_signature(scope: &Scope, status: u16, body: &[u8]) {
    // "thoughtsignature" and "thought_signature" both contain "signature".
    if scope.valid() && status == 400 && lower_bytes(body).contains("signature") {
        scope.clear();
    }
}

// ---------------------------------------------------------------------------------------
// Response accumulation

struct Pending {
    signature: Vec<u8>,
    kind: &'static str,
}

/// Collects one answer's signatures and native calls on top of the request's own
/// (`antigravityReasoningReplayAccumulator`); `commit` publishes them when the answer
/// finished.
pub(crate) struct Accumulator {
    scope: Scope,
    context_hash: String,
    items: Vec<Vec<u8>>,
    seen_calls: HashSet<Vec<u8>>,
    seen_signatures: HashSet<Vec<u8>>,
    segment_occurrences: HashMap<String, i64>,
    call_occurrences: HashMap<Vec<u8>, i64>,
    content_index: i64,
    next_part: i64,
    visible: Vec<u8>,
    thought: Vec<u8>,
    visible_part: i64,
    thought_part: i64,
    last_kind: &'static str,
    pending: Vec<Pending>,
    item_bytes: usize,
    overflow: bool,
    terminal: bool,
}

impl Accumulator {
    /// `newAntigravityReasoningReplayAccumulator` for the payload sent upstream.
    pub(crate) fn new(scope: &Scope, request: &[u8]) -> Option<Self> {
        if !scope.valid() {
            return None;
        }
        let index = Index::new(request);
        let (content_index, base_part) = index.pending_model_content();
        let items = index.items_from_request().unwrap_or_default();
        let seen_signatures = items
            .iter()
            .map(|item| field(&gj::parse(item), "thoughtSignature"))
            .filter(|signature| !signature.is_empty())
            .collect();
        let item_bytes = items.iter().map(Vec::len).sum();
        let mut segment_occurrences = HashMap::new();
        let mut call_occurrences = HashMap::new();
        if let Some(content) = usize::try_from(content_index)
            .ok()
            .and_then(|ci| index.contents.get(ci))
        {
            for part in &content.parts {
                let call = part.get("functionCall");
                if call.exists() {
                    let key = call_key(&call.get("name").bytes(), call.get("args").raw(), b"");
                    if !key.is_empty() {
                        *call_occurrences.entry(key).or_insert(0) += 1;
                    }
                    continue;
                }
                let (kind, hash) = fingerprint(part);
                if !hash.is_empty() {
                    *segment_occurrences.entry(format!("{kind}\x00{hash}")).or_insert(0) += 1;
                }
            }
        }
        Some(Accumulator {
            scope: scope.clone(),
            context_hash: index.context(content_index),
            overflow: items.len() > MAX_REPLAY_ITEMS || item_bytes > MAX_REPLAY_BYTES,
            items,
            seen_calls: HashSet::new(),
            seen_signatures,
            segment_occurrences,
            call_occurrences,
            content_index,
            next_part: base_part,
            visible: vec![],
            thought: vec![],
            visible_part: -1,
            thought_part: -1,
            last_kind: "",
            pending: vec![],
            item_bytes,
            terminal: false,
        })
    }

    fn append(&mut self, item: Vec<u8>) {
        if item.is_empty() || self.overflow {
            return;
        }
        if self.items.len() + 1 > MAX_REPLAY_ITEMS || self.item_bytes + item.len() > MAX_REPLAY_BYTES {
            self.overflow = true;
            return;
        }
        self.item_bytes += item.len();
        self.items.push(item);
    }

    /// `attachDetachedSignatureToLastFunctionCall`.
    fn attach_to_last_call(&mut self, signature: &[u8]) {
        if signature.is_empty() {
            return;
        }
        for i in (0..self.items.len()).rev() {
            let updated = {
                let item = gj::parse(&self.items[i]);
                if *item.get("type").bytes() != *b"function_call_part" {
                    continue;
                }
                if !field(&item, "thoughtSignature").is_empty() {
                    return;
                }
                let mut updated = self.items[i].clone();
                if !gj::set_str(&mut updated, "thoughtSignature", signature) {
                    return;
                }
                updated
            };
            let bytes = self.item_bytes - self.items[i].len() + updated.len();
            if bytes > MAX_REPLAY_BYTES {
                self.overflow = true;
                return;
            }
            self.items[i] = updated;
            self.item_bytes = bytes;
            return;
        }
    }

    /// `ObserveSSELine`.
    pub(crate) fn observe_sse_line(&mut self, line: &[u8]) {
        if let Some(payload) = crate::gemini_stream::json_payload(line) {
            self.observe(payload);
        }
    }

    /// `observeResponsePayload`: one generate answer or stream payload.
    pub(crate) fn observe(&mut self, payload: &[u8]) {
        if !trim_space(&gj::get(payload, "response.candidates.0.finishReason").bytes()).is_empty() {
            self.terminal = true;
        }
        let parts = gj::get(payload, "response.candidates.0.content.parts");
        if !parts.is_array() {
            return;
        }
        parts.each(|_, part| {
            self.observe_part(&part);
            true
        });
    }

    fn observe_part(&mut self, part: &Res<'_>) {
        let pi = self.next_part;
        self.next_part += 1;
        let mut signature = ag::native_part_signature(part);
        if !has_native_signature(&signature) {
            signature.clear();
        }
        let call = part.get("functionCall");
        if call.exists() {
            if matches!(self.last_kind, "text" | "thought") {
                self.flush(self.last_kind);
            }
            if signature.is_empty() {
                if let Some(i) = self.pending.iter().rposition(|p| p.kind.is_empty()) {
                    signature = self.pending.remove(i).signature;
                }
            } else {
                self.pending.retain(|p| !p.kind.is_empty());
            }
            if let Some(key) = tool_call_key(&call) {
                let dedupe = if signature.is_empty() {
                    [&key[..], format!("\x00part:{pi}").as_bytes()].concat()
                } else {
                    [&key[..], b"\x00", &signature].concat()
                };
                if !self.seen_calls.insert(dedupe) {
                    return;
                }
            }
            let key = call_key(&call.get("name").bytes(), call.get("args").raw(), b"");
            let occurrence = self.call_occurrences.get(&key).copied().unwrap_or(0);
            if !key.is_empty() {
                self.call_occurrences.insert(key, occurrence + 1);
            }
            if let Some(item) = call_item(self.content_index, pi, occurrence, &call, &signature) {
                let item = with_context_hash(item, &self.context_hash);
                self.append(item);
                if !signature.is_empty() {
                    self.seen_signatures.insert(signature);
                }
            }
            self.last_kind = "function_call";
            return;
        }

        let mut kind: &'static str = if part.get("thought").bool() { "thought" } else { "" };
        let text = part.get("text");
        let text_bytes = text.bytes();
        let semantic = text.exists() && !text_bytes.is_empty();
        let signature_only = !signature.is_empty() && !semantic;
        if signature_only && self.last_kind == "function_call" {
            if !self.seen_signatures.contains(&signature) {
                self.attach_to_last_call(&signature);
                self.seen_signatures.insert(signature);
            }
            return;
        }
        if semantic {
            if kind != "thought" {
                kind = "text";
            }
            if !signature.is_empty() {
                let (visible_empty, thought_empty) = (self.visible.is_empty(), self.thought.is_empty());
                let mut kept = vec![];
                for pending in std::mem::take(&mut self.pending) {
                    // A signature still waiting for text of this kind binds to this part.
                    let unbound = if pending.kind == kind {
                        (kind == "text" && visible_empty) || (kind == "thought" && thought_empty)
                    } else {
                        pending.kind.is_empty()
                    };
                    if unbound {
                        if pending.signature == signature {
                            self.seen_signatures.remove(&signature);
                        }
                        continue;
                    }
                    kept.push(pending);
                }
                self.pending = kept;
                if self.pending.iter().any(|p| p.kind == kind && p.signature != signature) {
                    self.flush(kind);
                }
            }
            if matches!(self.last_kind, "text" | "thought") && self.last_kind != kind {
                self.flush(self.last_kind);
            }
            if kind == "thought" {
                if self.thought.is_empty() {
                    self.thought_part = pi;
                }
                self.thought.extend_from_slice(&text_bytes);
            } else {
                if self.visible.is_empty() {
                    self.visible_part = pi;
                }
                self.visible.extend_from_slice(&text_bytes);
            }
            self.last_kind = kind;
        }
        let mut accepted = false;
        if !signature.is_empty() && !self.seen_signatures.contains(&signature) {
            if kind.is_empty() {
                kind = self.last_kind;
            }
            let unmatched_carrier = signature_only
                && self.last_kind == kind
                && ((kind == "text" && self.visible.is_empty()) || (kind == "thought" && self.thought.is_empty()));
            if !unmatched_carrier {
                if self.pending.len() + self.items.len() + 1 > MAX_REPLAY_ITEMS
                    || self.item_bytes + signature.len() > MAX_REPLAY_BYTES
                {
                    self.overflow = true;
                } else {
                    self.pending.push(Pending {
                        signature: signature.clone(),
                        kind,
                    });
                    accepted = true;
                }
            }
            self.seen_signatures.insert(signature);
        }
        if accepted && (signature_only || semantic) {
            match kind {
                "text" if !self.visible.is_empty() => self.flush("text"),
                "thought" if !self.thought.is_empty() => self.flush("thought"),
                _ => {}
            }
        }
    }

    /// `flushPendingThoughtSignaturesForKind`: pending signatures of `kind` become items
    /// pinned to the text segment collected so far, which then starts over.
    fn flush(&mut self, kind: &'static str) {
        if kind != "text" && kind != "thought" {
            return;
        }
        let (text, part) = if kind == "thought" {
            (std::mem::take(&mut self.thought), self.thought_part)
        } else {
            (std::mem::take(&mut self.visible), self.visible_part)
        };
        let mut hash = String::new();
        let mut occurrence = 0;
        if !text.is_empty() {
            hash = text_hash(kind, &text);
            let count = self.segment_occurrences.entry(format!("{kind}\x00{hash}")).or_insert(0);
            occurrence = *count;
            *count += 1;
        }
        for pending in std::mem::take(&mut self.pending) {
            if pending.kind != kind || hash.is_empty() {
                self.pending.push(pending);
                continue;
            }
            let mut item = signature_item(self.content_index, part, &pending.signature, kind, &hash);
            if !gj::set_int(&mut item, "targetOccurrence", occurrence) {
                item.clear();
            }
            let item = with_context_hash(item, &self.context_hash);
            self.append(item);
        }
        if kind == "thought" {
            self.thought_part = -1;
        } else {
            self.visible_part = -1;
        }
    }

    /// `appendPendingThoughtSignatures`: unbound signatures go to the last text segment.
    fn flush_all(&mut self) {
        let (last, visible, thought) = (self.last_kind, !self.visible.is_empty(), !self.thought.is_empty());
        for pending in self.pending.iter_mut().filter(|p| p.kind.is_empty()) {
            pending.kind = match () {
                _ if last == "text" && visible => "text",
                _ if last == "thought" && thought => "thought",
                _ if visible => "text",
                _ if thought => "thought",
                _ => "",
            };
        }
        self.flush("thought");
        self.flush("text");
        self.pending.clear();
    }

    /// `Commit`: a finished answer replaces the ledger entry (unless a newer request
    /// changed it); an unfinished one leaves it; an empty or oversized one clears it.
    pub(crate) fn commit(mut self) {
        if !self.scope.valid() || !self.terminal {
            return;
        }
        if !self.overflow {
            self.flush_all();
        }
        if self.overflow || self.items.is_empty() {
            self.scope.clear();
            return;
        }
        let scope = &self.scope;
        if cache::replay_replace_if_unchanged(&scope.model, &scope.session, &scope.snapshot, &self.items).is_err() {
            scope.clear();
        }
    }
}

/// A stream's upstream lines observed by `accumulator`, which the returned
/// [`StreamCommit`] commits.
pub(crate) fn observe_stream(lines: ExecStream, accumulator: Accumulator) -> (ExecStream, StreamCommit) {
    let shared = Arc::new(Mutex::new(Some(accumulator)));
    let observer = shared.clone();
    let lines = lines
        .inspect(move |item| {
            if let Ok(line) = item
                && let Some(accumulator) = observer.lock().unwrap_or_else(PoisonError::into_inner).as_mut()
            {
                accumulator.observe_sse_line(line);
            }
        })
        .boxed();
    (
        lines,
        StreamCommit {
            accumulator: shared,
            terminal: false,
            failed: false,
        },
    )
}

/// The commit half of `observe_stream`, Go's `commitReplay`: the client stream commits
/// at a clean end, before the synthetic tail goes out. Once a terminal chunk went out
/// (Go a4acc9f7's `terminalDelivered`), dropping it commits too, whether the client went
/// away or an error ended the stream. An apply_patch translation failure never commits.
pub(crate) struct StreamCommit {
    accumulator: Arc<Mutex<Option<Accumulator>>>,
    /// A terminal chunk went out.
    pub(crate) terminal: bool,
    /// The apply_patch translation failed (Go's `ApplyPatchTranslationError`).
    pub(crate) failed: bool,
}

impl StreamCommit {
    /// Commits once, unless the translation failed; `Accumulator::commit` itself skips a
    /// stream whose upstream sent no finish reason.
    pub(crate) fn commit(&mut self) {
        if self.failed {
            return;
        }
        if let Some(accumulator) = self.accumulator.lock().unwrap_or_else(PoisonError::into_inner).take() {
            accumulator.commit();
        }
    }

    /// Go c997cbb5: a Responses client's `response.completed` commits at once when the
    /// upstream already sent its finish reason, so later payloads stay out of the ledger.
    pub(crate) fn commit_if_terminal(&mut self) {
        if self.failed {
            return;
        }
        let mut slot = self.accumulator.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.as_ref().is_some_and(|a| a.terminal)
            && let Some(accumulator) = slot.take()
        {
            accumulator.commit();
        }
    }
}

impl Drop for StreamCommit {
    fn drop(&mut self) {
        if self.terminal {
            self.commit();
        }
    }
}

#[cfg(test)]
#[path = "antigravity_replay_tests.rs"]
mod tests;
