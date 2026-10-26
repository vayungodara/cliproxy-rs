//! Antigravity request and response shaping: antigravity_executor_request.go (envelope,
//! schema cleaning, endpoints, IDs), the payload helpers of antigravity_executor.go
//! (signature cleanup, function-response repair, web-search checks),
//! `convertStreamToNonStream`, the 429 rules of antigravity_executor_credits.go and the
//! cache-independent part of `prepareAntigravityGeminiReasoningReplayPayload`.
//!
//! Every edit is byte-level through `cpa_common::json`, so key order, escaping and
//! malformed input behave like Go's gjson/sjson.

use cpa_core::gostr::GoStr;
use std::collections::HashMap;
use std::time::Duration;

use cpa_common::gostr::trim_space;
use cpa_common::json::{self as gj, GoValue, Kind, Res};
use cpa_common::signature::{self as sig, GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR};
use cpa_core::credential::Credential;
use cpa_core::format::Format;
use sha2::{Digest, Sha256};

use crate::gemini_payload as payload;

pub(crate) const BASE_URL_DAILY: &str = "https://daily-cloudcode-pa.googleapis.com";
pub(crate) const GENERATE_PATH: &str = "/v1internal:generateContent";
pub(crate) const STREAM_PATH: &str = "/v1internal:streamGenerateContent";
pub(crate) const COUNT_TOKENS_PATH: &str = "/v1internal:countTokens";

/// `antigravityShortQuotaCooldownThreshold`.
const SHORT_COOLDOWN_THRESHOLD: Duration = Duration::from_secs(300);
/// `antigravityInstantRetryThreshold`.
const INSTANT_RETRY_THRESHOLD: Duration = Duration::from_secs(3);

fn lower_contains(model: &str, needle: &str) -> bool {
    model.go_lower().contains(needle)
}

/// `antigravityUsesReasoningReplayCache`: Gemini-family models (not Claude).
pub(crate) fn uses_reasoning_replay(model: &str) -> bool {
    let model = model.go_lower();
    !model.contains("claude") && (model.contains("gemini") || model.contains("flash") || model.contains("agent"))
}

/// Execute sends Claude, Gemini 3 Pro and Gemini 3.1 Flash Image models through the
/// stream endpoint and merges the answer (`executeClaudeNonStream`).
pub(crate) fn merges_stream(model: &str) -> bool {
    lower_contains(model, "claude") || model.contains("gemini-3-pro") || model.contains("gemini-3.1-flash-image")
}

// ---------------------------------------------------------------------------------------
// Credential fields

fn attr<'a>(credential: &'a Credential, key: &str) -> &'a str {
    credential.attributes.get(key).map(|v| v.trim()).unwrap_or_default()
}

fn meta<'a>(credential: &'a Credential, key: &str) -> &'a str {
    credential.str(key).map(str::trim).unwrap_or_default()
}

/// `resolveCustomAntigravityBaseURL`: the `base_url` attribute, else metadata.
fn custom_base_url(credential: &Credential) -> Option<String> {
    [attr(credential, "base_url"), meta(credential, "base_url")]
        .into_iter()
        .find(|v| !v.is_empty())
        .map(|v| v.trim_end_matches('/').to_owned())
}

/// `antigravityLoadCodeAssistBaseURL`: the custom base, else the production endpoint.
pub(crate) fn load_code_assist_base_url(credential: &Credential) -> String {
    custom_base_url(credential).unwrap_or_else(|| crate::antigravity_auth::API_ENDPOINT.to_owned())
}

/// `resolveAntigravityRequestBaseURL`: the custom base, else the daily endpoint.
pub(crate) fn base_url(credential: &Credential) -> String {
    custom_base_url(credential).unwrap_or_else(|| BASE_URL_DAILY.to_owned())
}

/// `antigravityConfiguredUserAgent`: the `user_agent` attribute, else metadata.
fn configured_user_agent(credential: &Credential) -> &str {
    [attr(credential, "user_agent"), meta(credential, "user_agent")]
        .into_iter()
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// `resolveUserAgent`.
pub(crate) fn user_agent(credential: &Credential) -> String {
    crate::antigravity_version::request_user_agent(configured_user_agent(credential))
}

// ---------------------------------------------------------------------------------------
// Envelope and identifiers

/// `generateRequestID`.
fn request_id() -> String {
    format!("agent-{}", uuid::Uuid::new_v4())
}

/// `generateImageGenRequestID`.
fn image_request_id() -> String {
    format!(
        "image_gen/{}/{}/12",
        chrono::Utc::now().timestamp_millis(),
        uuid::Uuid::new_v4()
    )
}

/// `generateSessionID`: a random negative decimal below 9e18.
fn random_session_id() -> String {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    format!("-{}", u64::from_be_bytes(bytes) % 9_000_000_000_000_000_000)
}

/// The non-negative i64 of a SHA-256 prefix, as Go's session IDs encode it.
fn digest_i64(data: &[u8]) -> i64 {
    let sum = Sha256::digest(data);
    let value = u64::from_be_bytes(sum[..8].try_into().expect("digest prefix"));
    (value & 0x7FFF_FFFF_FFFF_FFFF) as i64
}

/// `generateStableSessionID`: from the first user turn's first text part, else random.
pub(crate) fn stable_session_id(payload: &[u8]) -> String {
    let contents = gj::get(payload, "request.contents");
    if !contents.is_array() {
        return random_session_id();
    }
    for content in contents.array() {
        if *content.get("role").bytes() != *b"user" {
            continue;
        }
        let text = content.get("parts.0.text").bytes();
        if text.is_empty() {
            continue;
        }
        return format!("-{}", digest_i64(&text));
    }
    random_session_id()
}

/// `helps.DerivedAntigravitySessionID` for Go's `derived_session_id` metadata.
pub(crate) fn derived_session_id(derived: Option<&str>) -> String {
    let derived = derived.map(str::trim).unwrap_or_default();
    if derived.is_empty() {
        return String::new();
    }
    let seed = [b"cli-proxy-api:antigravity:derived-session\x00", derived.as_bytes()].concat();
    format!("-{}", digest_i64(&seed))
}

/// `geminiToAntigravity`: model, user agent, request type, project and request and
/// session IDs on the envelope; `request.safetySettings` removed and a top-level
/// `toolConfig` moved into the request.
pub(crate) fn gemini_to_antigravity(model: &str, body: Vec<u8>, project: &str, derived_session: &str) -> Vec<u8> {
    let original = body.clone();
    let mut out = payload::set_str_if_different(body, "model", model);
    out = payload::set_str_if_different(out, "userAgent", "antigravity");
    let image = model.contains("image");
    let mut kind = String::from_utf8_lossy(trim_space(&gj::get(&out, "requestType").bytes())).into_owned();
    if kind.is_empty() {
        kind = if image { "image_gen" } else { "agent" }.to_owned();
        gj::set_str(&mut out, "requestType", &kind);
    }
    if project.is_empty() {
        gj::delete(&mut out, "project");
    } else {
        out = payload::set_str_if_different(out, "project", project);
    }
    if image {
        gj::set_str(&mut out, "requestId", image_request_id());
    } else if kind != "web_search" {
        gj::set_str(&mut out, "requestId", request_id());
        let mut session = String::from_utf8_lossy(trim_space(&gj::get(&out, "request.sessionId").bytes())).into_owned();
        if session.is_empty() {
            session = derived_session.trim().to_owned();
        }
        if session.is_empty() {
            session = stable_session_id(&original);
        }
        gj::set_str(&mut out, "request.sessionId", session);
    }
    gj::delete(&mut out, "request.safetySettings");
    let tool_config = gj::get(&out, "toolConfig");
    if tool_config.exists() && !gj::get(&out, "request.toolConfig").exists() {
        let raw = tool_config.raw().to_vec();
        gj::set_raw(&mut out, "request.toolConfig", raw);
        gj::delete(&mut out, "toolConfig");
    }
    out
}

/// The `buildRequest` body edits after the envelope: the registry's completion limit
/// caps `maxOutputTokens`, schemas are cleaned, then Claude models validate function
/// calls and every other model loses `maxOutputTokens`.
pub(crate) fn finish_request(model: &str, mut body: Vec<u8>) -> Vec<u8> {
    let max = gj::get(&body, "request.generationConfig.maxOutputTokens");
    if max.kind == Kind::Number
        && let Some(info) = cpa_core::registry::lookup_model(model, Some("antigravity"))
    {
        let limit = info
            .raw
            .get("max_completion_tokens")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        if limit > 0 && max.int() > limit {
            gj::set_int(&mut body, "request.generationConfig.maxOutputTokens", limit);
        }
    }
    let claude = model.contains("claude");
    if needs_schema_sanitization(&body) {
        let antigravity_schema = claude || model.contains("gemini-3-pro") || model.contains("gemini-3.1-pro");
        body = sanitize_request_schemas(body, antigravity_schema, claude);
    }
    if claude {
        gj::set_str(&mut body, "request.toolConfig.functionCallingConfig.mode", "VALIDATED");
    } else {
        gj::delete(&mut body, "request.generationConfig.maxOutputTokens");
    }
    body
}

// ---------------------------------------------------------------------------------------
// Schema cleaning

const DECLARATION_SCHEMA_KEYS: [&str; 6] = [
    "parameters",
    "parametersJsonSchema",
    "parameters_json_schema",
    "response",
    "responseJsonSchema",
    "response_json_schema",
];
const GENERATION_CONTAINERS: [&str; 2] = ["request.generationConfig", "request.generation_config"];
const GENERATION_SCHEMA_KEYS: [&str; 4] = [
    "responseSchema",
    "responseJsonSchema",
    "response_schema",
    "response_json_schema",
];

/// `antigravityRequestNeedsSchemaSanitization`.
fn needs_schema_sanitization(body: &[u8]) -> bool {
    gj::get(body, "request.tools.0").exists()
        || GENERATION_CONTAINERS.iter().any(|container| {
            GENERATION_SCHEMA_KEYS
                .iter()
                .any(|key| gj::get(body, &format!("{container}.{key}")).exists())
        })
}

/// `sanitizeAntigravityRequestSchemasForModel`: declaration schemas, then generation
/// schemas. For a Claude model, parameter schemas flatten multi-branch unions, which the
/// Antigravity Claude backend rejects.
pub(crate) fn sanitize_request_schemas(body: Vec<u8>, antigravity_schema: bool, claude: bool) -> Vec<u8> {
    let body = sanitize_tool_schemas(body, antigravity_schema, claude);
    sanitize_generation_schemas(body)
}

/// `sanitizeAntigravityToolSchemas`: the rewrites run on a document holding only
/// `request.tools`, which then replaces the request's tools once when changed.
fn sanitize_tool_schemas(body: Vec<u8>, antigravity_schema: bool, claude: bool) -> Vec<u8> {
    let tools = gj::get(&body, "request.tools");
    if !tools.is_array() {
        return body;
    }
    let original = tools.raw().to_vec();
    let document = [&b"{\"request\":{\"tools\":"[..], &original, b"}}"].concat();
    let document = sanitize_tool_schema_document(document, antigravity_schema, claude);
    let cleaned = gj::get(&document, "request.tools");
    if !cleaned.is_array() || cleaned.raw() == original.as_slice() {
        return body;
    }
    let cleaned = cleaned.raw().to_vec();
    gj::try_set_raw(&body, "request.tools", cleaned).unwrap_or(body)
}

/// `sanitizeAntigravityToolSchemaDocumentForModel` (Go b8d4e14a): in each declaration,
/// `parametersJsonSchema` renamed onto `parameters`, then every declaration schema
/// cleaned nested one level down, each declaration on its own bytes and the results
/// spliced back in one pass. A repeated declarations key is cleaned at its first
/// occurrence only, as Go's `Get` and `SetRawBytes` on the tool find it.
fn sanitize_tool_schema_document(document: Vec<u8>, antigravity_schema: bool, claude: bool) -> Vec<u8> {
    let tools = gj::get(&document, "request.tools");
    if !tools.is_array() {
        return document;
    }
    let mut edits = Vec::new();
    for (i, tool) in tools.array().iter().enumerate() {
        // A tool can carry both spellings; their edits are spliced in document order.
        let mut lists: Vec<_> = ["functionDeclarations", "function_declarations"]
            .into_iter()
            .map(|key| (key, tool.get(key)))
            .filter(|(_, declarations)| declarations.is_array())
            .collect();
        lists.sort_by_key(|(_, declarations)| declarations.index);
        for (key, declarations) in &lists {
            for (j, declaration) in declarations.array().iter().enumerate() {
                let cleaned = sanitize_declaration(declaration.raw(), antigravity_schema, claude);
                if cleaned != declaration.raw() {
                    edits.push(Replace::new(
                        &document,
                        declaration,
                        format!("request.tools.{i}.{key}.{j}"),
                        cleaned,
                    ));
                }
            }
        }
    }
    drop(tools);
    replace_values(document, edits)
}

/// The edits [`sanitize_tool_schema_document`] makes to one declaration, in Go's order.
fn sanitize_declaration(declaration: &[u8], antigravity_schema: bool, claude: bool) -> Vec<u8> {
    let mut declaration = declaration.to_vec();
    let value = gj::get(&declaration, "parametersJsonSchema");
    if value.exists() {
        // util.RenameKey: set the new key, then delete the old one.
        let raw = value.raw().to_vec();
        if let Ok(renamed) = gj::try_set_raw(&declaration, "parameters", raw)
            && let Ok(renamed) = gj::try_delete(&renamed, "parametersJsonSchema")
        {
            declaration = renamed;
        }
    }
    for key in DECLARATION_SCHEMA_KEYS {
        let schema = gj::get(&declaration, key);
        if !schema.is_object() {
            continue;
        }
        // Only parameter schemas become Claude's custom.input_schema upstream.
        let cleaned = nested_clean(schema.raw(), |s| {
            if claude && key.starts_with("parameters") {
                cpa_common::gemini_schema::for_antigravity_claude_tool(s, antigravity_schema)
            } else {
                cpa_common::gemini_schema::for_antigravity_tool(s, antigravity_schema)
            }
        });
        if cleaned == schema.raw() {
            continue;
        }
        if let Ok(updated) = gj::try_set_raw(&declaration, key, cleaned) {
            declaration = updated;
        }
    }
    declaration
}

/// `sanitizeAntigravityGenerationSchemas`: all schema edits of one generation config,
/// then one write of that config.
fn sanitize_generation_schemas(mut body: Vec<u8>) -> Vec<u8> {
    for container in GENERATION_CONTAINERS {
        let config = gj::get(&body, container);
        if !config.is_object() {
            continue;
        }
        let original = config.raw().to_vec();
        let mut cleaned = original.clone();
        for key in GENERATION_SCHEMA_KEYS {
            let schema = gj::get(&cleaned, key);
            if !schema.is_object() {
                continue;
            }
            let fixed = cpa_common::gemini_schema::for_antigravity_response(schema.raw());
            if fixed == schema.raw() {
                continue;
            }
            if let Ok(updated) = gj::try_set_raw(&cleaned, key, fixed) {
                cleaned = updated;
            }
        }
        if cleaned == original {
            continue;
        }
        if let Ok(updated) = gj::try_set_raw(&body, container, cleaned) {
            body = updated;
        }
    }
    body
}

/// `cleanNestedSchema`: the cleaner sees the schema under a `schema` key, as
/// whole-payload cleaning saw tool schemas, so its nested-only rules apply.
fn nested_clean(schema: &[u8], clean: impl Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    let Ok(wrapped) = gj::try_set_raw(b"{}", "schema", schema) else {
        return clean(schema);
    };
    let cleaned = clean(&wrapped);
    let unwrapped = gj::get(&cleaned, "schema");
    if unwrapped.exists() {
        unwrapped.raw().to_vec()
    } else {
        clean(schema)
    }
}

// ---------------------------------------------------------------------------------------
// Gemini signatures and function responses

/// `sanitizeAntigravityGeminiRequestSignatures`: Gemini-family models only.
pub(crate) fn sanitize_gemini_signatures(model: &str, body: Vec<u8>) -> Vec<u8> {
    if !uses_reasoning_replay(model) {
        return body;
    }
    let body = sig::sanitize_gemini_request_thought_signatures(&body, "request.contents");
    normalize_function_response_roles(body)
}

/// `ensureAntigravityGeminiLeadingUserContent`: not for Claude models, whose adapter
/// rejects empty text parts.
pub(crate) fn ensure_leading_user_content(model: &str, body: Vec<u8>) -> Vec<u8> {
    if lower_contains(model, "claude") {
        return body;
    }
    payload::ensure_leading_user_content(body, "request.contents")
}

/// `ensureAntigravityGeminiBoundaryUserContent`.
pub(crate) fn ensure_boundary_user_content(model: &str, body: Vec<u8>) -> Vec<u8> {
    if lower_contains(model, "claude") {
        return body;
    }
    let body = payload::ensure_leading_user_content(body, "request.contents");
    payload::ensure_trailing_user_content(body, "request.contents")
}

/// One call or response of a turn: its ID and name.
struct FunctionRef {
    id: Vec<u8>,
    name: Vec<u8>,
}

/// `normalizeAntigravityGeminiFunctionResponseRoles`: missing response names are
/// repaired; a response-only turn after a call turn lists responses in call order
/// (matching on ID, else on name; others after them), then other parts, and becomes
/// a `model` turn.
pub(crate) fn normalize_function_response_roles(body: Vec<u8>) -> Vec<u8> {
    let body = repair_function_response_names(body);
    let contents = gj::get(&body, "request.contents");
    if !contents.is_array() {
        return body;
    }
    let mut edits: Vec<Replace> = Vec::new();
    let mut pending: Vec<FunctionRef> = Vec::new();
    for (index, content) in contents.array().iter().enumerate() {
        let parts = content.get("parts");
        if !parts.is_array() {
            pending.clear();
            continue;
        }
        let (mut calls, mut responses) = (Vec::new(), Vec::new());
        let (mut response_parts, mut other_parts): (Vec<Vec<u8>>, Vec<Vec<u8>>) = (Vec::new(), Vec::new());
        let items = parts.array();
        for part in &items {
            if part.get("functionCall").exists() {
                calls.push(FunctionRef {
                    id: part.get("functionCall.id").bytes().into_owned(),
                    name: part.get("functionCall.name").bytes().into_owned(),
                });
            } else if part.get("functionResponse").exists() {
                responses.push(FunctionRef {
                    id: part.get("functionResponse.id").bytes().into_owned(),
                    name: part.get("functionResponse.name").bytes().into_owned(),
                });
                response_parts.push(part.raw().to_vec());
            } else {
                other_parts.push(part.raw().to_vec());
            }
        }
        if items.is_empty() {
            pending.clear();
            continue;
        }
        if !calls.is_empty() && responses.is_empty() {
            pending = calls;
            continue;
        }
        if responses.is_empty() {
            if !other_parts.is_empty() {
                pending.clear();
            }
            continue;
        }
        if !calls.is_empty() {
            pending.clear();
            continue;
        }
        let mut replacement: Option<Vec<u8>> = None;
        if !pending.is_empty() {
            let mut used = vec![false; responses.len()];
            let mut ordered: Vec<&[u8]> = Vec::with_capacity(items.len());
            for call in &pending {
                let matched = responses.iter().enumerate().position(|(i, response)| {
                    !used[i]
                        && ((!call.id.is_empty() && response.id == call.id)
                            || (call.id.is_empty() && !call.name.is_empty() && response.name == call.name))
                });
                if let Some(i) = matched {
                    used[i] = true;
                    ordered.push(&response_parts[i]);
                }
            }
            for (i, part) in response_parts.iter().enumerate() {
                if !used[i] {
                    ordered.push(part);
                }
            }
            ordered.extend(other_parts.iter().map(Vec::as_slice));
            // json.Marshal of []json.RawMessage: each part compacted, HTML escaped.
            let compacted: Vec<Vec<u8>> = ordered.iter().map(|p| gj::compact(p, true)).collect();
            let encoded = gj::join(&compacted);
            if encoded != parts.raw() {
                let mut item = content.raw().to_vec();
                if gj::set_raw(&mut item, "parts", &encoded) {
                    replacement = Some(item);
                }
            }
        }
        pending.clear();
        if other_parts.is_empty() && *content.get("role").bytes() != *b"model" {
            let mut item = replacement.unwrap_or_else(|| content.raw().to_vec());
            gj::set_str(&mut item, "role", "model");
            replacement = Some(item);
        }
        if let Some(item) = replacement {
            edits.push(Replace::new(&body, content, format!("request.contents.{index}"), item));
        }
    }
    drop(contents);
    replace_values(body, edits)
}

/// A value to replace in a body: where the walk found it (when it borrows the body),
/// its path for `sjson.SetRawBytes` otherwise, and its new raw bytes.
struct Replace {
    span: Option<(usize, usize)>,
    path: String,
    raw: Vec<u8>,
}

impl Replace {
    fn new(body: &[u8], value: &Res<'_>, path: String, raw: Vec<u8>) -> Self {
        let found = value.raw();
        let span = (found.as_ptr() as usize)
            .checked_sub(body.as_ptr() as usize)
            .map(|start| (start, start + found.len()))
            .filter(|(start, end)| !found.is_empty() && *end <= body.len() && &body[*start..*end] == found);
        Self { span, path, raw }
    }
}

/// `sjson.SetRawBytes` of each replacement in order, in one pass over `body`: setting an
/// existing value replaces its bytes in place, so values found by one walk are spliced
/// instead of searched for again from the start for each edit. The values must not
/// nest. When one does not borrow the body, every edit goes through its path instead.
fn replace_values(body: Vec<u8>, edits: Vec<Replace>) -> Vec<u8> {
    if edits.is_empty() {
        return body;
    }
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(edits.len());
    for edit in &edits {
        match edit.span {
            Some(span) if spans.last().is_none_or(|last| last.1 <= span.0) => spans.push(span),
            _ => {
                let mut body = body;
                for edit in edits {
                    gj::set_raw(&mut body, &edit.path, edit.raw);
                }
                return body;
            }
        }
    }
    let mut out = Vec::with_capacity(body.len() + edits.iter().map(|e| e.raw.len()).sum::<usize>());
    let mut at = 0;
    for (edit, (start, end)) in edits.iter().zip(spans) {
        out.extend_from_slice(&body[at..start]);
        out.extend_from_slice(&edit.raw);
        at = end;
    }
    out.extend_from_slice(&body[at..]);
    out
}

/// `repairAntigravityGeminiFunctionResponseNames`: a response with an ID and a missing
/// or `unknown` name takes the name of the call with that ID.
fn repair_function_response_names(body: Vec<u8>) -> Vec<u8> {
    let contents = gj::get(&body, "request.contents");
    if !contents.is_array() {
        return body;
    }
    let contents = contents.array();
    let mut names: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    for content in &contents {
        let parts = content.get("parts");
        if !parts.is_array() {
            continue;
        }
        for part in parts.array() {
            let call = part.get("functionCall");
            if !call.exists() {
                continue;
            }
            let id = trim_space(&call.get("id").bytes()).to_vec();
            let name = trim_space(&call.get("name").bytes()).to_vec();
            if !id.is_empty() && !name.is_empty() && name != b"unknown" {
                names.insert(id, name);
            }
        }
    }
    if names.is_empty() {
        return body;
    }
    let mut edits = Vec::new();
    for (i, content) in contents.iter().enumerate() {
        let parts = content.get("parts");
        if !parts.is_array() {
            continue;
        }
        for (j, part) in parts.array().iter().enumerate() {
            let response = part.get("functionResponse");
            if !response.exists() {
                continue;
            }
            let id = trim_space(&response.get("id").bytes()).to_vec();
            let name = trim_space(&response.get("name").bytes()).to_vec();
            if id.is_empty() || (!name.is_empty() && name != b"unknown") {
                continue;
            }
            let Some(real) = names.get(&id) else {
                continue;
            };
            let mut updated = part.raw().to_vec();
            if gj::set_str(&mut updated, "functionResponse.name", real) {
                edits.push(Replace::new(
                    &body,
                    part,
                    format!("request.contents.{i}.parts.{j}"),
                    updated,
                ));
            }
        }
    }
    drop(contents);
    replace_values(body, edits)
}

/// `util.IsGeminiClaudeToolUseID`: the reserved Claude-facing provenance namespace.
pub(crate) fn is_claude_provenance_id(id: &[u8]) -> bool {
    trim_space(id)
        .strip_prefix(b"cpa_gemini_")
        .is_some_and(|digest| digest.len() == 32 && digest.iter().all(u8::is_ascii_hexdigit))
}

/// `antigravityPayloadHasClaudeToolProvenanceID`.
fn has_provenance_id(body: &[u8]) -> bool {
    let contents = gj::get(body, "request.contents");
    if !contents.is_array() {
        return false;
    }
    let reserved = |part: &Res<'_>| {
        ["functionCall.id", "functionResponse.id"]
            .iter()
            .any(|path| is_claude_provenance_id(&part.get(path).bytes()))
    };
    contents.array().iter().any(|content| {
        let parts = content.get("parts");
        if parts.is_array() {
            parts.array().iter().any(reserved)
        } else {
            // Result.Array returns a non-array value as one item.
            parts.kind != Kind::Null && reserved(&parts)
        }
    })
}

/// `antigravitySyntheticToolCallID`.
fn synthetic_call_id(reserved: &[u8]) -> String {
    let sum = Sha256::digest([&b"antigravity-degraded-tool-call\x00"[..], reserved].concat());
    let hex: String = sum[..6].iter().map(|b| format!("{b:02x}")).collect();
    format!("call_{hex}")
}

/// `degradeAntigravityClaudeToolProvenanceIDs`: unresolved reserved IDs become stable
/// synthetic ones; a degraded first call of a turn keeps a bypass signature, later
/// degraded calls lose theirs.
fn degrade_provenance_ids(body: &[u8]) -> (Vec<u8>, usize) {
    let contents = gj::get(body, "request.contents");
    if !contents.is_array() {
        return (body.to_vec(), 0);
    }
    let mut edits = Vec::new();
    for (i, content) in contents.array().iter().enumerate() {
        let parts = content.get("parts");
        if !parts.is_array() {
            continue;
        }
        let mut seen_call = false;
        for (j, part) in parts.array().iter().enumerate() {
            let call = part.get("functionCall");
            if call.exists() {
                let first = !seen_call;
                seen_call = true;
                let id = trim_space(&call.get("id").bytes()).to_vec();
                if !is_claude_provenance_id(&id) {
                    continue;
                }
                let mut updated = part.raw().to_vec();
                gj::set_str(&mut updated, "functionCall.id", synthetic_call_id(&id));
                let signature = part.get("thoughtSignature");
                if signature.exists() && !signature.bytes().is_empty() {
                    if first {
                        gj::set_str(
                            &mut updated,
                            "thoughtSignature",
                            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
                        );
                    } else {
                        gj::delete(&mut updated, "thoughtSignature");
                    }
                }
                edits.push(Replace::new(
                    body,
                    part,
                    format!("request.contents.{i}.parts.{j}"),
                    updated,
                ));
                continue;
            }
            let response = part.get("functionResponse");
            if response.exists() {
                let id = trim_space(&response.get("id").bytes()).to_vec();
                if !is_claude_provenance_id(&id) {
                    continue;
                }
                let mut updated = part.raw().to_vec();
                gj::set_str(&mut updated, "functionResponse.id", synthetic_call_id(&id));
                edits.push(Replace::new(
                    body,
                    part,
                    format!("request.contents.{i}.parts.{j}"),
                    updated,
                ));
            }
        }
    }
    let count = edits.len();
    (replace_values(body.to_vec(), edits), count)
}

/// `antigravityNativePartThoughtSignature`.
pub(crate) fn native_part_signature(part: &Res<'_>) -> Vec<u8> {
    [
        "thoughtSignature",
        "thought_signature",
        "extra_content.google.thought_signature",
    ]
    .iter()
    .map(|path| trim_space(&part.get(path).bytes()).to_vec())
    .find(|s| !s.is_empty())
    .unwrap_or_default()
}

/// `antigravityRepairUnsignedFirstFunctionCalls`: the first function call of a model
/// turn without any signature gets the bypass sentinel.
fn repair_unsigned_first_calls(body: Vec<u8>) -> Vec<u8> {
    let contents = gj::get(&body, "request.contents");
    if !contents.is_array() {
        return body;
    }
    let mut edits = Vec::new();
    for (i, content) in contents.array().iter().enumerate() {
        if !trim_space(&content.get("role").bytes()).eq_ignore_ascii_case(b"model") {
            continue;
        }
        let parts = content.get("parts");
        if !parts.is_array() {
            continue;
        }
        if let Some((j, part)) = parts
            .array()
            .iter()
            .enumerate()
            .find(|(_, p)| p.get("functionCall").exists())
            && native_part_signature(part).is_empty()
        {
            // sjson sets a key inside the part the same way on the part alone.
            let mut updated = part.raw().to_vec();
            gj::set_str(
                &mut updated,
                "thoughtSignature",
                GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
            );
            edits.push(Replace::new(
                &body,
                part,
                format!("request.contents.{i}.parts.{j}"),
                updated,
            ));
        }
    }
    drop(contents);
    replace_values(body, edits)
}

/// The cache-independent steps of `prepareAntigravityGeminiReasoningReplayPayload`
/// after replay (`antigravity_replay::prepare`): response roles, degradation of
/// provenance IDs the ledger could not resolve, and the bypass signature on unsigned
/// first calls.
pub(crate) fn repair_history(body: Vec<u8>) -> Vec<u8> {
    let mut updated = normalize_function_response_roles(body);
    if has_provenance_id(&updated) {
        let (degraded, count) = degrade_provenance_ids(&updated);
        tracing::warn!(
            "antigravity executor: replay state missing for {count} tool ID(s); rewriting them to synthetic IDs and continuing without reasoning replay for those calls"
        );
        updated = normalize_function_response_roles(degraded);
    }
    repair_unsigned_first_calls(updated)
}

/// `validateAntigravityRequestSignatures` for Claude clients: Gemini-family models keep
/// only replayable Gemini carriers; Claude models drop thinking blocks without a Claude
/// signature and, in strict bypass mode without the signature cache, invalid ones.
pub(crate) fn validate_request_signatures(model: &str, from: Format, body: &[u8]) -> Vec<u8> {
    validate_signatures_with(
        model,
        from,
        body,
        cpa_common::cache::signature_cache_enabled(),
        cpa_common::cache::signature_bypass_strict(),
    )
}

/// [`validate_request_signatures`] under the given signature-cache settings.
fn validate_signatures_with(
    model: &str,
    from: Format,
    body: &[u8],
    cache_enabled: bool,
    bypass_strict: bool,
) -> Vec<u8> {
    if from != Format::Claude {
        return body.to_vec();
    }
    if uses_reasoning_replay(model) {
        return cpa_translate::strip_invalid_gemini_signature_thinking_blocks(body);
    }
    let body = sig::strip_invalid_claude_thinking_blocks(
        body,
        sig::ClaudeValidation {
            prefix_only: true,
            ..Default::default()
        },
    );
    if cache_enabled || !bypass_strict {
        return body;
    }
    sig::strip_invalid_claude_thinking_blocks(
        &body,
        sig::ClaudeValidation {
            strict: true,
            ..Default::default()
        },
    )
}

/// `obfuscateSensitiveWords` (helps.ObfuscateSensitiveWordsInSystemInstruction): text
/// of `request.systemInstruction` / `request.system_instruction`, as a string or parts.
pub(crate) fn obfuscate_system_instruction(body: Vec<u8>, obfuscate: impl Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    let mut out = body;
    for path in ["request.systemInstruction", "request.system_instruction"] {
        let instruction = gj::get(&out, path).into_owned();
        if !instruction.exists() {
            continue;
        }
        if instruction.kind == Kind::String {
            let text = instruction.bytes();
            let changed = obfuscate(&text);
            if changed != *text {
                gj::set_str(&mut out, path, changed);
            }
            continue;
        }
        let parts = instruction.get("parts");
        if !parts.is_array() {
            continue;
        }
        for (i, part) in parts.array().iter().enumerate() {
            let text = part.get("text");
            if text.kind != Kind::String {
                continue;
            }
            let text = text.bytes();
            let changed = obfuscate(&text);
            if changed != *text {
                gj::set_str(&mut out, &format!("{path}.parts.{i}.text"), changed);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// Web search

fn has_tool_of_type(body: &[u8], path: &str, kinds: &[&[u8]]) -> bool {
    let tools = gj::get(body, path);
    tools.is_array() && tools.array().iter().any(|t| kinds.contains(&&*t.get("type").bytes()))
}

/// `shouldResolveAntigravityWebSearchGroundingURLs`: a Google Search request made for
/// a Claude typed web-search tool or a Responses web-search tool.
pub(crate) fn resolves_grounding_urls(from: Format, original: &[u8], translated: &[u8]) -> bool {
    let tools = gj::get(translated, "request.tools");
    let google_search = tools.is_array() && tools.array().iter().any(|t| t.get("googleSearch").exists());
    if !google_search {
        return false;
    }
    match from {
        Format::Claude => has_tool_of_type(original, "tools", &[b"web_search_20250305", b"web_search_20260209"]),
        Format::OpenAIResponse => has_tool_of_type(
            original,
            "tools",
            &[
                b"web_search",
                b"web_search_2025_08_26",
                b"web_search_preview",
                b"web_search_preview_2025_03_11",
            ],
        ),
        _ => false,
    }
}

/// `isAntigravityVertexSearchRedirect` with Go's `url.Parse`: an `https` URL whose host
/// is exactly `vertexaisearch.cloud.google.com` (case and any port count) and whose
/// unescaped path starts with `/grounding-api-redirect/`. URLs Go rejects (control
/// characters, bad escapes) are not redirects.
pub(crate) fn is_search_redirect(raw: &str) -> bool {
    if raw.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return false;
    }
    let Some((scheme, rest)) = raw.split_once(':') else {
        return false;
    };
    let Some(rest) = rest.strip_prefix("//").filter(|_| scheme.eq_ignore_ascii_case("https")) else {
        return false;
    };
    let (rest, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let rest = rest.split_once('?').map_or(rest, |(before, _)| before);
    let end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(end);
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    host == "vertexaisearch.cloud.google.com"
        && unescape(fragment).is_some()
        && unescape(path).is_some_and(|p| p.starts_with(b"/grounding-api-redirect/"))
}

/// Go `url.unescape`: `None` for a malformed `%` escape.
fn unescape(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            out.push(value);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

/// The grounding chunks of a response: their array path and each chunk's trimmed URI.
pub(crate) fn grounding_uris(body: &[u8]) -> Option<(&'static str, Vec<String>)> {
    let mut base = "response.candidates.0.groundingMetadata.groundingChunks";
    let mut chunks = gj::get(body, base);
    if !chunks.is_array() {
        base = "candidates.0.groundingMetadata.groundingChunks";
        chunks = gj::get(body, base);
    }
    if !chunks.is_array() {
        return None;
    }
    let uris = chunks
        .array()
        .iter()
        .map(|c| String::from_utf8_lossy(trim_space(&c.get("web.uri").bytes())).into_owned())
        .collect();
    Some((base, uris))
}

// ---------------------------------------------------------------------------------------
// Merged streams (convertStreamToNonStream)

/// A run of text or thought parts merged into one part.
#[derive(Default)]
struct Pending {
    kind: Option<bool>,
    text: Vec<u8>,
    signature: Vec<u8>,
}

impl Pending {
    /// `flushPending`: blank text runs vanish; a blank thought survives with a signature.
    fn flush(&mut self, parts: &mut Vec<Vec<u8>>) {
        let Some(thought) = self.kind.take() else {
            return;
        };
        let text = std::mem::take(&mut self.text);
        let signature = std::mem::take(&mut self.signature);
        let blank = trim_space(&text).is_empty();
        if !thought {
            if !blank {
                let mut part = b"{\"text\":".to_vec();
                gj::marshal_str(&mut part, &text, true);
                part.push(b'}');
                parts.push(part);
            }
            return;
        }
        if blank && signature.is_empty() {
            return;
        }
        // map[string]interface{} marshals with sorted keys.
        let mut part = b"{\"text\":".to_vec();
        gj::marshal_str(&mut part, &text, true);
        part.extend_from_slice(b",\"thought\":true");
        if !signature.is_empty() {
            part.extend_from_slice(b",\"thoughtSignature\":");
            gj::marshal_str(&mut part, &signature, true);
        }
        part.push(b'}');
        parts.push(part);
    }
}

/// A part kept as is, through Go's map round trip: sorted keys, float64 numbers, the
/// signature as `thoughtSignature` and `inline_data` as `inlineData`.
fn normalized_part(part: &Res<'_>) -> Vec<u8> {
    let mut map = GoValue::parse_f64(part.raw())
        .and_then(|v| v.into_object().ok())
        .unwrap_or_default();
    let mut signature = part.get("thoughtSignature").bytes().into_owned();
    if signature.is_empty() {
        signature = part.get("thought_signature").bytes().into_owned();
    }
    if !signature.is_empty() {
        map.insert(
            "thoughtSignature".into(),
            GoValue::String(String::from_utf8_lossy(&signature).into_owned()),
        );
        map.remove("thought_signature");
    }
    if let Some(inline) = map.remove("inline_data") {
        map.insert("inlineData".into(), inline);
    }
    GoValue::Object(map).marshal()
}

/// `convertStreamToNonStream`: the stream's `response` objects (or bare Gemini answers)
/// merged into one, text and thought runs joined, the last trace ID, role, finish
/// reason, model version, response ID and usage kept (zero usage when none).
pub(crate) fn merge_stream(stream: &[u8]) -> Vec<u8> {
    let mut template: Vec<u8> = Vec::new();
    let (mut trace, mut finish, mut version, mut response_id, mut role) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), None::<Vec<u8>>);
    let mut usage: Option<Vec<u8>> = None;
    let mut parts: Vec<Vec<u8>> = Vec::new();
    let mut pending = Pending::default();
    for line in stream.split(|b| *b == b'\n') {
        let line = trim_space(line);
        if line.is_empty() || !gj::valid(line) {
            continue;
        }
        let root = gj::parse(line);
        let mut response = root.get("response");
        if !response.exists() {
            if !root.get("candidates").exists() {
                continue;
            }
            response = root.clone();
        }
        template = response.raw().to_vec();
        let text_of = |r: Res<'_>| (r.exists() && !r.bytes().is_empty()).then(|| r.bytes().into_owned());
        if let Some(t) = text_of(root.get("traceId")) {
            trace = t;
        }
        let r = response.get("candidates.0.content.role");
        if r.exists() {
            role = Some(r.bytes().into_owned());
        }
        if let Some(f) = text_of(response.get("candidates.0.finishReason")) {
            finish = f;
        }
        if let Some(v) = text_of(response.get("modelVersion")) {
            version = v;
        }
        if let Some(id) = text_of(response.get("responseId")) {
            response_id = id;
        }
        let u = response.get("usageMetadata");
        if u.exists() {
            usage = Some(u.raw().to_vec());
        } else if root.get("usageMetadata").exists() {
            usage = Some(root.get("usageMetadata").raw().to_vec());
        }
        let items = response.get("candidates.0.content.parts");
        if !items.is_array() {
            continue;
        }
        for part in items.array() {
            let call = part.get("functionCall").exists();
            let inline = part.get("inlineData").exists() || part.get("inline_data").exists();
            if call || inline {
                pending.flush(&mut parts);
                parts.push(normalized_part(&part));
                continue;
            }
            let thought = part.get("thought").bool();
            if thought || part.get("text").exists() {
                if pending.kind.is_some_and(|k| k != thought) {
                    pending.flush(&mut parts);
                }
                pending.kind = Some(thought);
                pending.text.extend_from_slice(&part.get("text").bytes());
                let mut signature = part.get("thoughtSignature").bytes().into_owned();
                if signature.is_empty() {
                    signature = part.get("thought_signature").bytes().into_owned();
                }
                if thought && !signature.is_empty() {
                    pending.signature = signature;
                }
                continue;
            }
            pending.flush(&mut parts);
            parts.push(normalized_part(&part));
        }
    }
    pending.flush(&mut parts);
    if template.is_empty() {
        template = br#"{"candidates":[{"content":{"role":"model","parts":[]}}]}"#.to_vec();
    }
    gj::set_raw(&mut template, "candidates.0.content.parts", gj::join(&parts));
    if let Some(role) = role.filter(|r| !r.is_empty()) {
        gj::set_str(&mut template, "candidates.0.content.role", role);
    }
    if !finish.is_empty() {
        gj::set_str(&mut template, "candidates.0.finishReason", finish);
    }
    if !version.is_empty() {
        gj::set_str(&mut template, "modelVersion", version);
    }
    if !response_id.is_empty() {
        gj::set_str(&mut template, "responseId", response_id);
    }
    match usage {
        Some(usage) => {
            gj::set_raw(&mut template, "usageMetadata", usage);
        }
        None if !gj::get(&template, "usageMetadata").exists() => {
            for key in ["promptTokenCount", "candidatesTokenCount", "totalTokenCount"] {
                gj::set_int(&mut template, &format!("usageMetadata.{key}"), 0);
            }
        }
        None => {}
    }
    let mut out = br#"{"response":{},"traceId":""}"#.to_vec();
    gj::set_raw(&mut out, "response", template);
    if !trace.is_empty() {
        gj::set_str(&mut out, "traceId", trace);
    }
    out
}

// ---------------------------------------------------------------------------------------
// 429 rules

/// What a 429 answer means for the credential (`antigravity429DecisionKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RateLimit {
    SoftRetry,
    InstantRetrySameAuth,
    ShortCooldownSwitchAuth,
    FullQuotaExhausted,
}

/// `decideAntigravity429`: the decision and the answer's retry delay.
pub(crate) fn decide_429(body: &[u8]) -> (RateLimit, Option<Duration>) {
    if body.is_empty() {
        return (RateLimit::SoftRetry, None);
    }
    let retry = crate::antigravity_auth::parse_retry_delay(body);
    let status = gj::get(body, "error.status").bytes().into_owned();
    if !trim_space(&status).eq_ignore_ascii_case(b"RESOURCE_EXHAUSTED") {
        return (RateLimit::SoftRetry, retry);
    }
    let details = gj::get(body, "error.details");
    if details.is_array() {
        for detail in details.array() {
            if *detail.get("@type").bytes() != *b"type.googleapis.com/google.rpc.ErrorInfo" {
                continue;
            }
            let reason = trim_space(&detail.get("reason").bytes()).to_vec();
            if reason.eq_ignore_ascii_case(b"QUOTA_EXHAUSTED") {
                return (RateLimit::FullQuotaExhausted, retry);
            }
            if reason.eq_ignore_ascii_case(b"RATE_LIMIT_EXCEEDED") {
                let kind = match retry {
                    None => RateLimit::SoftRetry,
                    Some(d) if d < INSTANT_RETRY_THRESHOLD => RateLimit::InstantRetrySameAuth,
                    Some(d) if d < SHORT_COOLDOWN_THRESHOLD => RateLimit::ShortCooldownSwitchAuth,
                    Some(_) => RateLimit::FullQuotaExhausted,
                };
                return (kind, retry);
            }
        }
    }
    let lower = String::from_utf8_lossy(body).go_lower();
    if ["quota_exhausted", "quota exhausted"].iter().any(|k| lower.contains(k)) {
        return (RateLimit::FullQuotaExhausted, retry);
    }
    (RateLimit::SoftRetry, retry)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every call Go's request-helper tests make (and a sample of the executor tests'
    /// calls), recorded with Go's results (tests/reference/antigravity_helpers).
    #[test]
    fn helpers_match_go() {
        let records: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../tests/fixtures/antigravity_helpers_go.json")).unwrap();
        let mut ran = std::collections::BTreeMap::<String, usize>::new();
        for r in &records {
            let s = |k: &str| r[k].as_str().unwrap_or_default();
            let (fun, test) = (s("fn"), s("test"));
            let input = s("in").as_bytes().to_vec();
            let from = || Format::parse(s("from")).unwrap();
            let got = match fun {
                "validate" => validate_signatures_with(
                    s("model"),
                    from(),
                    &input,
                    r["cache_on"].as_bool().unwrap(),
                    r["strict"].as_bool().unwrap(),
                ),
                "sanitize_signatures" => sanitize_gemini_signatures(s("model"), input),
                "normalize_roles" => normalize_function_response_roles(input),
                "repair_names" => repair_function_response_names(input),
                "sanitize_schemas" => sanitize_request_schemas(
                    input,
                    r["flag"].as_bool().unwrap_or(false),
                    r["claude"].as_bool().unwrap_or(false),
                ),
                "resolves_grounding" => {
                    let got = resolves_grounding_urls(from(), s("original").as_bytes(), &input);
                    assert_eq!(got, r["bool"].as_bool().unwrap_or(false), "{test}: {fun}");
                    *ran.entry(fun.into()).or_default() += 1;
                    continue;
                }
                "base_url" => {
                    // Go's test auths carry the provider outside their metadata.
                    let mut metadata = r["metadata"].as_object().cloned().unwrap_or_default();
                    metadata.entry("type").or_insert("antigravity".into());
                    let dir = std::path::Path::new("/fixture");
                    let mut c = Credential::from_file(dir, &dir.join("ag.json"), metadata).unwrap();
                    for (k, v) in r["attributes"].as_object().into_iter().flatten() {
                        c.attributes.insert(k.clone(), v.as_str().unwrap().to_owned());
                    }
                    base_url(&c).into_bytes()
                }
                other => panic!("unknown helper {other}"),
            };
            assert_eq!(String::from_utf8_lossy(&got), s("out"), "{test}: {fun}({})", s("in"));
            *ran.entry(fun.into()).or_default() += 1;
        }
        assert_eq!(ran.len(), 7, "fixture lost a helper: {ran:?}");
        assert!(
            records.iter().any(|r| r["claude"] == true),
            "fixture lost the Claude-model schema cleaning"
        );
        assert!(records.len() >= 600, "fixture lost cases: {}", records.len());
    }

    /// Go `url.Parse` semantics: scheme case-insensitive, host exact, escapes decoded.
    #[test]
    fn search_redirects_follow_go_url_parse() {
        let base = "https://vertexaisearch.cloud.google.com/grounding-api-redirect/";
        assert!(is_search_redirect(&format!("{base}AAA")));
        assert!(is_search_redirect(
            "HTTPS://vertexaisearch.cloud.google.com/grounding-api-redirect/x?y#z"
        ));
        assert!(is_search_redirect(
            "https://u@vertexaisearch.cloud.google.com/grounding-api-redirect%2Fx"
        ));
        assert!(!is_search_redirect(
            "https://VERTEXAISEARCH.cloud.google.com/grounding-api-redirect/x"
        ));
        assert!(!is_search_redirect(
            "https://vertexaisearch.cloud.google.com:443/grounding-api-redirect/x"
        ));
        assert!(!is_search_redirect(
            "http://vertexaisearch.cloud.google.com/grounding-api-redirect/x"
        ));
        assert!(!is_search_redirect(&format!("{base}%zz")));
        assert!(!is_search_redirect(&format!("{base}x#%g")));
        assert!(!is_search_redirect(&format!("{base}x\t")));
        assert!(!is_search_redirect(
            "https://vertexaisearch.cloud.google.com/grounding-api-redirect"
        ));
    }

    #[test]
    fn provenance_ids_need_32_hex_digits() {
        assert!(is_claude_provenance_id(
            b" cpa_gemini_0123456789ABCDEF0123456789abcdef "
        ));
        assert!(!is_claude_provenance_id(b"cpa_gemini_0123456789abcdef0123456789abcde"));
        assert!(!is_claude_provenance_id(b"cpa_gemini_0123456789abcdef0123456789abcdeg"));
    }

    #[test]
    fn rate_limit_thresholds() {
        let body = |delay: &str| {
            format!(
                r#"{{"error":{{"status":"RESOURCE_EXHAUSTED","details":[{{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"RATE_LIMIT_EXCEEDED"}},{{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"{delay}"}}]}}}}"#
            )
        };
        assert_eq!(decide_429(body("2.9s").as_bytes()).0, RateLimit::InstantRetrySameAuth);
        assert_eq!(decide_429(body("3s").as_bytes()).0, RateLimit::ShortCooldownSwitchAuth);
        assert_eq!(
            decide_429(body("299s").as_bytes()).0,
            RateLimit::ShortCooldownSwitchAuth
        );
        assert_eq!(decide_429(body("300s").as_bytes()).0, RateLimit::FullQuotaExhausted);
        assert_eq!(
            decide_429(br#"{"error":{"status":"resource_exhausted","message":"Quota Exhausted now"}}"#).0,
            RateLimit::FullQuotaExhausted
        );
        assert_eq!(
            decide_429(br#"{"error":{"status":"UNAVAILABLE","message":"quota exhausted"}}"#).0,
            RateLimit::SoftRetry
        );
    }

    /// Cleaning the schemas of many tool declarations is one pass over the tools: a
    /// search from the start of the tools for each declaration took minutes here. Both
    /// spellings of the declarations key appear, in both orders.
    #[test]
    fn tool_schemas_are_cleaned_in_one_pass() {
        let declaration = r#"{"name":"t","parametersJsonSchema":{"type":"object","$schema":"x","properties":{"a":{"type":["string","null"]}},"additionalProperties":false}}"#;
        let one = format!(r#"{{"request":{{"tools":[{{"functionDeclarations":[{declaration}]}}]}}}}"#);
        let one = sanitize_request_schemas(one.into_bytes(), true, false);
        let cleaned = gj::get(&one, "request.tools.0.functionDeclarations.0").raw().to_vec();
        let cleaned = String::from_utf8(cleaned).unwrap();
        assert!(cleaned.contains(r#""parameters":"#) && !cleaned.contains("parametersJsonSchema"));
        let body = |declaration: &str| {
            let list = vec![declaration; 1_000].join(",");
            format!(
                r#"{{"request":{{"tools":[{{"functionDeclarations":[{list}],"function_declarations":[{list}]}},{{"function_declarations":[{list}],"functionDeclarations":[{list}]}}]}}}}"#
            )
        };
        let started = std::time::Instant::now();
        let many = sanitize_request_schemas(body(declaration).into_bytes(), true, false);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(String::from_utf8(many).unwrap(), body(&cleaned));
    }

    /// Go b8d4e14a's TestSanitizeAntigravityRequestSchemasScalesLinearlyWithDeclarations:
    /// 256 declarations, each renamed and cleaned on its own bytes, with Go's expected
    /// bytes. (The fixture skips this call: Go's recorder drops records over 32 KiB.)
    #[test]
    fn many_declarations_match_go() {
        let pad = "x".repeat(4096);
        let (mut input, mut want) = (Vec::new(), Vec::new());
        for i in 0..256 {
            input.push(format!(
                r#"{{"name":"tool_{i}","description":"{pad}","parametersJsonSchema":{{"type":"object","$id":"drop","properties":{{"q":{{"type":"string"}}}}}}}}"#
            ));
            want.push(format!(
                r#"{{"name":"tool_{i}","description":"{pad}","parameters":{{"type":"object","properties":{{"q":{{"type":"string"}},"_":{{"type":"boolean"}}}},"required":["_"]}}}}"#
            ));
        }
        let wrap = |list: &[String]| {
            format!(
                r#"{{"request":{{"tools":[{{"functionDeclarations":[{}]}}]}}}}"#,
                list.join(",")
            )
        };
        let got = sanitize_request_schemas(wrap(&input).into_bytes(), true, false);
        assert!(
            String::from_utf8(got).unwrap() == wrap(&want),
            "sanitized tools diverged from Go's bytes"
        );
    }
}
