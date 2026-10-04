//! Request shaping of the xAI executor (internal/runtime/executor/xai_executor_request.go
//! and the request-side helpers of xai_executor_response.go): translation to the Codex
//! Responses shape, thinking, payload rules, and the tool, tool_choice and input
//! rewrites xAI's Responses API needs.

use std::collections::{BTreeMap, HashMap, HashSet};

use cpa_common::gostr::GoStr;
use cpa_common::json::{self as gj, GoValue, Kind, Res};
use cpa_common::thinking::{self, ModelCaps, RequestThinking, parse_suffix};
use cpa_core::config::Config;
use cpa_core::exec::{ExecError, ExecRequest, FailureScope};
use cpa_core::format::Format;
use cpa_translate::RequestCtx;

use crate::openai_compat_payload::{self as compat, set_bool_if_different, set_str_if_different};
use crate::xai_replay::{self as replay, ReplayScope};
use crate::xai_response::{
    self as response, CUSTOM, ClientToolKey, FUNCTION, NAMESPACE, NamespaceRef, NamespaceRefs, WEB_SEARCH, text,
};
use cpa_translate::apply_patch_responses as apply_patch;

use crate::xai::PROVIDER;
const IMAGE_GENERATION: &str = "image_generation";
const TOOL_SEARCH: &str = "tool_search";
const X_SEARCH: &str = "x_search";
const CLIENT_WEB_SEARCH_ALIAS: &str = "clientfn_web_search";
pub(crate) const MAX_TOOLS: usize = 200;
const CODEX_APP_NAMESPACE: &str = "codex_app";
const AUTOMATION_UPDATE: &str = "automation_update";
const SAFE_FUNCTION_PARAMETERS: &str = r#"{"type":"object","properties":{},"additionalProperties":true}"#;
const X_SEARCH_TOOL: &str = r#"{"type":"x_search"}"#;
const COMPOSER_PREFIX: &str = "grok-composer-";

/// `xaiPreparedRequest`.
pub(crate) struct Prepared {
    pub base_model: String,
    pub original_payload: Vec<u8>,
    pub body: Vec<u8>,
    pub namespace_tools: NamespaceRefs,
    pub client_declared_tools: HashSet<ClientToolKey>,
    pub session_id: String,
    pub replay_scope: ReplayScope,
    pub filter_internal_x_search: bool,
    pub web_search_alias: String,
    /// `helps.ApplyPatchResponsesState`.
    pub apply_patch: apply_patch::State,
}

fn bad_request(message: impl Into<String>) -> ExecError {
    ExecError::local(400, FailureScope::Request, message)
}

/// `opts.OriginalRequest`, else the payload.
pub(crate) fn original(req: &ExecRequest) -> &[u8] {
    if req.original_body.is_empty() {
        &req.body
    } else {
        &req.original_body
    }
}

/// `helps.PayloadRequestedModel`.
fn requested_model(req: &ExecRequest) -> &str {
    if req.requested_model.trim().is_empty() {
        req.model.trim()
    } else {
        req.requested_model.trim()
    }
}

/// `oauth.providers.xai.inject-x-search` as this credential sees it (Go binds
/// `cfg.ForAPIKey()` for API-key credentials).
pub(crate) fn inject_x_search(cfg: &Config) -> bool {
    cfg.document
        .get("oauth")
        .and_then(|o| o.get("providers"))
        .and_then(|p| p.get("xai"))
        .and_then(|x| x.get("inject-x-search"))
        .and_then(serde_yaml_ng::Value::as_bool)
        .unwrap_or(false)
}

/// `TranslateRequestWithAPIKeyModelCompatibility(AndUpdateIntent)ForExecutor` with
/// target executor xai (Codex-client rewrites, then the pair or its compat variant).
// ponytail: the configuration-update intent is not exposed by crate::codex_client
// (owner: Codex), so thinking always sees `updates_changed: false`.
fn translate(
    req: &ExecRequest,
    client: &crate::codex_client::Client<'_>,
    to: Format,
    model: &str,
    stream: bool,
    body: &[u8],
) -> Result<Vec<u8>, ExecError> {
    crate::codex_client::translate_request(req.source_format, to, &RequestCtx { model, stream }, body, client)
        .map_err(|e| bad_request(e.to_string()))
}

/// `preserveXAIResponsesOutputControls`: output limits and sampling controls the
/// translator drops are copied from the client body.
fn preserve_output_controls(mut body: Vec<u8>, source: &[u8], from: Format) -> Vec<u8> {
    let usable = |r: &Res<'_>| r.exists() && r.kind != Kind::Null;
    let max = match from {
        Format::OpenAI => {
            let mct = gj::get(source, "max_completion_tokens");
            if usable(&mct) {
                mct
            } else {
                gj::get(source, "max_tokens")
            }
        }
        Format::OpenAIResponse => gj::get(source, "max_output_tokens"),
        _ => return body,
    };
    if usable(&max) {
        gj::set_raw(&mut body, "max_output_tokens", max.raw());
    }
    for field in ["temperature", "top_p", "top_k"] {
        let value = gj::get(source, field);
        if usable(&value) {
            gj::set_raw(&mut body, field, value.raw());
        }
    }
    body
}

/// `prepareResponsesRequestTo`.
pub(crate) async fn prepare(
    req: &ExecRequest,
    cfg: &Config,
    stream: bool,
    to: Format,
    replay_store: &replay::Store,
    downstream_websocket: bool,
) -> Result<Prepared, ExecError> {
    let base_model = parse_suffix(&req.model).model_name;
    let original_payload = original(req).to_vec();
    // cliproxyauth.ResolvedModelInfo, bound by dispatch for configured xai-api-key models.
    let caps = req.resolved_model.as_ref().map(|r| ModelCaps::from(&r.info));
    let is_compat = req.resolved_model.as_ref().is_some_and(|r| r.is_compat());
    let client = crate::codex_client::Client::new(&req.headers, cfg, PROVIDER, is_compat);
    let original_translated = translate(req, &client, to, &base_model, stream, &original_payload)?;
    let original_translated = preserve_output_controls(original_translated, &original_payload, req.source_format);
    let body = translate(req, &client, to, &base_model, stream, &req.body)?;
    let body = preserve_output_controls(body, &req.body, req.source_format);
    let mut body = thinking::apply_request_thinking(&RequestThinking {
        body: &body,
        payload: &req.body,
        original: &original_payload,
        model: &req.model,
        from: req.source_format.as_str(),
        // Go passes the executor identifier as the target format.
        to: PROVIDER,
        provider: PROVIDER,
        resolved: caps.as_ref().map(Some),
        has_request_transformer: false,
        updates_changed: false,
    })
    .map_err(|e| ExecError::local(e.status(), FailureScope::Request, e.message))?;
    let rules = cpa_common::payload::Rules::of(cfg);
    body = cpa_common::payload::apply(
        &rules,
        &cpa_common::payload::Request {
            target_executor: PROVIDER,
            model: &base_model,
            requested_model: requested_model(req),
            protocol: to.as_str(),
            from_protocol: req.source_format.as_str(),
            root: "",
            original: &original_translated,
            request_path: &req.request_path,
            headers: Some(&req.headers),
        },
        body,
    );
    set_str_if_different(&mut body, "model", &base_model);
    set_bool_if_different(&mut body, "stream", stream);
    for key in [
        "previous_response_id",
        "prompt_cache_retention",
        "safety_identifier",
        "stream_options",
    ] {
        gj::delete(&mut body, key);
    }
    body = cpa_common::codex_client::rewrite_multi_agent_v2_input(&req.headers, &body, &client.settings, false);
    let mut apply_patch = apply_patch::State::new(req.source_format, &original_payload, &original_translated);
    // A plain Go error (translator/common NormalizeApplyPatchResponsesRequest).
    body = apply_patch::normalize_executor_request(&body, Some(&original_payload))
        .map_err(crate::openai_compat::plain_err)?;
    let will_inject = inject_x_search(cfg);
    let fold = should_fold_namespace_tools(&body, will_inject);
    let namespace_tools = collect_namespace_refs(&body, fold);
    for (name, r) in &namespace_tools {
        if r.dispatcher {
            apply_patch.add_dispatcher(name, &r.namespace);
        }
    }
    let client_declared_tools = response::client_declared_tool_keys(&body);
    body = normalize_tools(body, fold);
    body = promote_additional_tools(body);
    let mut web_search_alias = String::new();
    if has_client_web_search_function(&body, &namespace_tools) {
        web_search_alias = resolve_web_search_alias(&body);
        body = alias_web_search_function(body, &web_search_alias, &namespace_tools);
    }
    body = normalize_namespace_tool_choice(body, fold);
    body = prune_orphaned_tool_choice(body);
    body = normalize_forced_hosted_tool_choice(body, WEB_SEARCH);
    body = normalize_forced_hosted_tool_choice(body, IMAGE_GENERATION);
    body = normalize_tool_choice_for_tools(body);
    if will_inject && !requires_hosted_tool_only_any(&body) {
        body = ensure_native_x_search_tool(body);
    }
    body = clamp_tools_limit(body, MAX_TOOLS, &namespace_tools);
    let replay_scope = replay::scope(req, &body, downstream_websocket);
    body = replay::apply(replay_store, &replay_scope, body).await;
    body = normalize_input_custom_tool_calls(body);
    body = normalize_input_namespace_tool_calls(body, fold);
    if !web_search_alias.is_empty() {
        body = alias_web_search_input(body, &web_search_alias, &namespace_tools);
    }
    body = response::normalize_input_reasoning_items(body);
    body = response::sanitize_input_encrypted_content(body);
    cpa_common::codex_client::normalize_codex_instructions(&mut body);
    // stop is a Chat Completions field xAI's Responses API rejects.
    gj::delete(&mut body, "stop");
    body = normalize_image_refs(body);
    let session_id = composer_session_id(req, &base_model);
    if !session_id.is_empty() {
        set_str_if_different(&mut body, "prompt_cache_key", &session_id);
    }
    Ok(Prepared {
        filter_internal_x_search: response::request_has_native_x_search(&body),
        base_model,
        original_payload,
        body,
        namespace_tools,
        client_declared_tools,
        session_id,
        replay_scope,
        web_search_alias,
        apply_patch,
    })
}

/// `xaiExecutionSessionID`.
pub(crate) fn execution_session_id(req: &ExecRequest) -> String {
    if let Some(s) = req
        .execution_session
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return s.to_owned();
    }
    let key = text(&gj::get(&req.body, "prompt_cache_key"));
    if !key.is_empty() {
        return key;
    }
    // helps.DerivedSessionUUID("xai", ...).
    match req.derived_session.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(derived) => uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            format!("cli-proxy-api\0{PROVIDER}\0derived-session\0{derived}").as_bytes(),
        )
        .to_string(),
        None => String::new(),
    }
}

/// `xaiResolveComposerSessionID`: the conversation id Grok composer models need.
fn composer_session_id(req: &ExecRequest, base_model: &str) -> String {
    let session = execution_session_id(req);
    if !session.is_empty() {
        return session;
    }
    if !base_model.trim().go_lower().starts_with(COMPOSER_PREFIX) {
        return String::new();
    }
    if let Some(key) = compat::claude_code_prompt_cache(base_model, &req.body, &req.headers) {
        return key;
    }
    uuid::Uuid::new_v4().to_string()
}

/// Grok version prefix of a model name (`xaiParseGrokVersionPrefix`).
fn grok_version(rest: &str) -> Option<(i64, i64)> {
    let bytes = rest.as_bytes();
    let i = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if i == 0 {
        return None;
    }
    let major: i64 = rest[..i].parse().ok()?;
    if i == bytes.len() || bytes[i] != b'.' {
        return Some((major, -1));
    }
    let j = i + 1 + bytes[i + 1..].iter().take_while(|b| b.is_ascii_digit()).count();
    if j == i + 1 {
        return Some((major, -1));
    }
    Some((major, rest[i + 1..j].parse().ok()?))
}

/// `xaiSupportsNativeImageGeneration`: grok-4.6 and newer, but not the 4.20 line.
pub(crate) fn supports_native_image_generation(model: &str) -> bool {
    let name = parse_suffix(model).model_name.trim().go_lower();
    let name = name.rsplit_once('/').map_or(name.as_str(), |(_, n)| n);
    let Some(rest) = name.strip_prefix("grok-") else {
        return false;
    };
    if rest == "4.20" || rest.starts_with("4.20-") {
        return false;
    }
    let Some((major, minor)) = grok_version(rest) else {
        return false;
    };
    (major, minor.max(0)) >= (4, 6)
}

/// `ensureXAINativeXSearchTool`.
fn ensure_native_x_search_tool(mut body: Vec<u8>) -> Vec<u8> {
    if !gj::valid(&body) {
        return body;
    }
    if !response::request_has_native_x_search(&body) {
        if gj::get(&body, "tools").is_array() {
            gj::set_raw(&mut body, "tools.-1", X_SEARCH_TOOL);
        } else {
            gj::set_raw(&mut body, "tools", format!("[{X_SEARCH_TOOL}]"));
        }
    }
    let choice = gj::get(&body, "tool_choice");
    if !choice.is_object() || &*choice.get("type").bytes() != b"allowed_tools" {
        return body;
    }
    let allowed = choice.get("tools");
    if !allowed.is_array() {
        gj::set_raw(&mut body, "tool_choice.tools", format!("[{X_SEARCH_TOOL}]"));
        return body;
    }
    if allowed.array().iter().any(|t| text(&t.get("type")) == X_SEARCH) {
        return body;
    }
    gj::set_raw(&mut body, "tool_choice.tools.-1", X_SEARCH_TOOL);
    body
}

/// `xaiHasClientWebSearchFunction`.
fn has_client_web_search_function(body: &[u8], refs: &NamespaceRefs) -> bool {
    if !gj::valid(body) {
        return false;
    }
    gj::get(body, "tools").array().iter().any(|tool| {
        let t = text(&tool.get("type"));
        (t == FUNCTION || t == CUSTOM) && text(&tool.get("name")) == WEB_SEARCH && !refs.contains_key(WEB_SEARCH)
    }) && gj::get(body, "tools").is_array()
}

/// `xaiBodyHasToolNamed`.
fn body_has_tool_named(body: &[u8], name: &str) -> bool {
    let tools = gj::get(body, "tools");
    if tools.is_array() {
        for tool in tools.array() {
            if text(&tool.get("name")) == name {
                return true;
            }
            let nested = tool.get("tools");
            if nested.is_array() && nested.array().iter().any(|c| text(&c.get("name")) == name) {
                return true;
            }
        }
    }
    let input = gj::get(body, "input");
    input.is_array() && input.array().iter().any(|i| text(&i.get("name")) == name)
}

/// `xaiResolveClientWebSearchAlias`.
fn resolve_web_search_alias(body: &[u8]) -> String {
    if !body_has_tool_named(body, CLIENT_WEB_SEARCH_ALIAS) {
        return CLIENT_WEB_SEARCH_ALIAS.into();
    }
    (1..)
        .map(|i| format!("{CLIENT_WEB_SEARCH_ALIAS}_{i}"))
        .find(|candidate| !body_has_tool_named(body, candidate))
        .expect("an unused alias")
}

/// `aliasXAIClientWebSearchInput`.
fn alias_web_search_input(mut body: Vec<u8>, alias: &str, refs: &NamespaceRefs) -> Vec<u8> {
    if !gj::valid(&body) || alias.is_empty() || refs.contains_key(WEB_SEARCH) {
        return body;
    }
    let input = gj::get(&body, "input");
    if !input.is_array() {
        return body;
    }
    let targets: Vec<usize> = input
        .array()
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            matches!(
                text(&item.get("type")).as_str(),
                "function_call" | "custom_tool_call" | "function_call_output"
            ) && text(&item.get("name")) == WEB_SEARCH
                && text(&item.get("namespace")).is_empty()
        })
        .map(|(i, _)| i)
        .collect();
    for i in targets {
        gj::set_str(&mut body, &format!("input.{i}.name"), alias);
    }
    body
}

/// `aliasXAIClientWebSearchFunction`: a client function named `web_search` is renamed
/// so xAI does not run its hosted search instead.
fn alias_web_search_function(mut body: Vec<u8>, alias: &str, refs: &NamespaceRefs) -> Vec<u8> {
    if !gj::valid(&body) || alias.is_empty() {
        return body;
    }
    let namespace_web_search = refs.contains_key(WEB_SEARCH);
    let tools = gj::get(&body, "tools");
    if tools.is_array() {
        let targets: Vec<usize> = tools
            .array()
            .iter()
            .enumerate()
            .filter(|(_, tool)| {
                let t = text(&tool.get("type"));
                (t == FUNCTION || t == CUSTOM) && text(&tool.get("name")) == WEB_SEARCH && !namespace_web_search
            })
            .map(|(i, _)| i)
            .collect();
        for i in targets {
            gj::set_str(&mut body, &format!("tools.{i}.name"), alias);
        }
    }
    let choice = gj::get(&body, "tool_choice").into_owned();
    if choice.is_object() {
        let function_name = choice.get("function.name");
        if function_name.exists()
            && text(&function_name) == WEB_SEARCH
            && text(&choice.get("function.namespace")).is_empty()
            && !namespace_web_search
        {
            gj::set_str(&mut body, "tool_choice.function.name", alias);
        }
        let name = choice.get("name");
        if name.exists() && text(&name) == WEB_SEARCH && text(&choice.get("namespace")).is_empty() {
            let t = text(&choice.get("type"));
            if (t == FUNCTION || t == "tool") && !namespace_web_search {
                gj::set_str(&mut body, "tool_choice.name", alias);
            }
        }
        let allowed = choice.get("tools");
        if allowed.is_array() {
            for (i, tool) in allowed.array().iter().enumerate() {
                if !text(&tool.get("namespace")).is_empty() || namespace_web_search {
                    continue;
                }
                let (t, n) = (text(&tool.get("type")), text(&tool.get("name")));
                if n == WEB_SEARCH && ((t == FUNCTION || t == "tool") || t != WEB_SEARCH) {
                    gj::set_str(&mut body, &format!("tool_choice.tools.{i}.name"), alias);
                }
            }
        }
    }
    alias_web_search_input(body, alias, refs)
}

/// `normalizeXAIForcedHostedToolChoice`: a forced hosted tool (web_search or
/// image_generation) becomes `"required"` with only that tool left.
fn normalize_forced_hosted_tool_choice(body: Vec<u8>, tool_type: &str) -> Vec<u8> {
    let choice = gj::get(&body, "tool_choice").into_owned();
    if !choice.is_object() {
        return body;
    }
    let choice_type = text(&choice.get("type"));
    if choice_type == tool_type {
        let body = keep_only_hosted_tools(body, tool_type);
        return set_tool_choice_string(body, "required");
    }
    if choice_type != "allowed_tools" {
        return body;
    }
    let allowed = choice.get("tools");
    if !allowed.is_array() {
        return body;
    }
    let all = allowed.array();
    let filtered: Vec<Vec<u8>> = all
        .iter()
        .filter(|t| text(&t.get("type")) != tool_type)
        .map(|t| t.raw().to_vec())
        .collect();
    if filtered.len() == all.len() {
        return body;
    }
    if filtered.is_empty() {
        let mode = text(&choice.get("mode"));
        let mode = if mode == "auto" { "auto" } else { "required" };
        let body = keep_only_hosted_tools(body, tool_type);
        return set_tool_choice_string(body, mode);
    }
    gj::try_set_raw(&body, "tool_choice.tools", gj::join(&filtered)).unwrap_or(body)
}

fn keep_only_hosted_tools(body: Vec<u8>, tool_type: &str) -> Vec<u8> {
    let tools = gj::get(&body, "tools");
    if !tools.is_array() {
        return body;
    }
    let all = tools.array();
    let kept: Vec<Vec<u8>> = all
        .iter()
        .filter(|t| text(&t.get("type")) == tool_type)
        .map(|t| t.raw().to_vec())
        .collect();
    if kept.is_empty() || kept.len() == all.len() {
        return body;
    }
    gj::try_set_raw(&body, "tools", gj::join(&kept)).unwrap_or(body)
}

fn set_tool_choice_string(body: Vec<u8>, value: &str) -> Vec<u8> {
    gj::try_set_str(&body, "tool_choice", value).unwrap_or(body)
}

/// `xaiToolChoiceRequiresHostedToolOnly`.
fn requires_hosted_tool_only(body: &[u8], tool_type: &str) -> bool {
    let choice = gj::get(body, "tool_choice");
    if choice.kind != Kind::String || !matches!(&*choice.str(), "required" | "auto") {
        return false;
    }
    let tools = gj::get(body, "tools");
    let all = tools.array();
    tools.is_array() && !all.is_empty() && all.iter().all(|t| text(&t.get("type")) == tool_type)
}

fn requires_hosted_tool_only_any(body: &[u8]) -> bool {
    requires_hosted_tool_only(body, IMAGE_GENERATION) || requires_hosted_tool_only(body, WEB_SEARCH)
}

/// `xaiToolChoiceKey`: host tools by type, function tools by type and name.
type ChoiceKey = (String, String);

fn choice_key(r: &Res<'_>) -> Option<ChoiceKey> {
    let t = text(&r.get("type"));
    if t.is_empty() {
        return None;
    }
    let name = if t == FUNCTION || t == CUSTOM {
        let n = text(&r.get("name"));
        if n.is_empty() {
            return None;
        }
        n
    } else {
        String::new()
    };
    Some((t, name))
}

fn available_choice_keys(body: &[u8]) -> HashSet<ChoiceKey> {
    let mut keys = HashSet::new();
    let mut collect = |tools: &Res<'_>| {
        if tools.is_array() {
            keys.extend(tools.array().iter().filter_map(choice_key));
        }
    };
    collect(&gj::get(body, "tools"));
    let input = gj::get(body, "input");
    if input.is_array() {
        for item in input.array() {
            if &*item.get("type").bytes() == b"additional_tools" {
                collect(&item.get("tools"));
            }
        }
    }
    keys
}

/// `pruneXAIOrphanedToolChoice`: choices naming tools that normalization removed.
fn prune_orphaned_tool_choice(mut body: Vec<u8>) -> Vec<u8> {
    if !gj::valid(&body) {
        return body;
    }
    let choice = gj::get(&body, "tool_choice").into_owned();
    if !choice.exists() {
        return body;
    }
    let available = available_choice_keys(&body);
    if choice.kind == Kind::String || !choice.is_object() {
        return body;
    }
    match text(&choice.get("type")).as_str() {
        "allowed_tools" => {
            let allowed = gj::get(&body, "tool_choice.tools").into_owned();
            if !allowed.is_array() {
                gj::delete(&mut body, "tool_choice");
                return body;
            }
            let all = allowed.array();
            let kept: Vec<Vec<u8>> = all
                .iter()
                .filter(|t| choice_key(t).is_some_and(|k| available.contains(&k)))
                .map(|t| t.raw().to_vec())
                .collect();
            if kept.len() == all.len() {
                return body;
            }
            if kept.is_empty() {
                gj::delete(&mut body, "tool_choice");
            } else {
                gj::set_raw(&mut body, "tool_choice.tools", gj::join(&kept));
            }
            body
        }
        "" => body,
        _ => {
            if !choice_key(&choice).is_some_and(|k| available.contains(&k)) {
                gj::delete(&mut body, "tool_choice");
            }
            body
        }
    }
}

/// `xaiCountFlattenedTools`.
fn count_flattened(tools: &Res<'_>) -> usize {
    if !tools.is_array() {
        return 0;
    }
    tools
        .array()
        .iter()
        .map(|tool| match &*tool.get("type").bytes() {
            b"namespace" => {
                let nested = tool.get("tools");
                if nested.is_array() { nested.array().len() } else { 1 }
            }
            b"tool_search" => 0,
            _ => 1,
        })
        .sum()
}

/// `xaiShouldFoldNamespaceTools`: more than 200 tools once namespaces are flattened.
fn should_fold_namespace_tools(body: &[u8], will_inject: bool) -> bool {
    let mut count = count_flattened(&gj::get(body, "tools"));
    let input = gj::get(body, "input");
    if input.is_array() {
        for item in input.array() {
            if &*item.get("type").bytes() == b"additional_tools" {
                count += count_flattened(&item.get("tools"));
            }
        }
    }
    if will_inject && !response::request_has_native_x_search(body) && !requires_hosted_tool_only_any(body) {
        count += 1;
    }
    count > MAX_TOOLS
}

/// Go `json.Marshal` of a string.
fn json_str(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    gj::marshal_str(&mut out, s.as_bytes(), true);
    out
}

/// `util.InlineLocalRefs` with `$defs` and `definitions` dropped, as a string.
fn inlined_parameters(raw: &str) -> String {
    let inlined = cpa_common::gemini_schema::inline_local_refs(raw.as_bytes());
    if !gj::valid(&inlined) {
        return String::from_utf8_lossy(&inlined).into_owned();
    }
    let mut cleaned = inlined;
    for key in ["$defs", "definitions"] {
        if gj::get(&cleaned, key).exists() {
            gj::delete(&mut cleaned, key);
        }
    }
    String::from_utf8_lossy(&cleaned).into_owned()
}

/// `buildXAINamespaceDispatcherTool`: one function standing for a whole namespace when
/// the flattened tool list would exceed xAI's limit. Built as Go's `json.Marshal` of a
/// map writes it: sorted keys, HTML-escaped strings.
fn namespace_dispatcher_tool(tool: &Res<'_>) -> Option<Vec<u8>> {
    let namespace = text(&tool.get("name"));
    if namespace.is_empty() {
        return None;
    }
    let description = text(&tool.get("description"));
    let mut names = Vec::new();
    let mut entries = Vec::new();
    let nested = tool.get("tools");
    if nested.is_array() {
        for child in nested.array() {
            let child_name = text(&child.get("name"));
            if child_name.is_empty() {
                continue;
            }
            names.push(child_name.clone());
            let child_description = text(&child.get("description"));
            let mut params = child.get("parameters");
            if !params.exists() {
                params = child.get("input_schema");
            }
            let mut param_str = String::new();
            if params.exists() && !params.raw().is_empty() {
                let raw = String::from_utf8_lossy(params.raw()).trim().to_owned();
                if !raw.is_empty() && raw != "{}" && raw != r#"{"type":"object","properties":{}}"# {
                    param_str = inlined_parameters(&raw);
                }
            }
            entries.push(match (child_description.is_empty(), param_str.is_empty()) {
                (false, false) => format!("- {child_name}: {child_description}\n  Parameters: {param_str}"),
                (false, true) => format!("- {child_name}: {child_description}"),
                (true, false) => format!("- {child_name}\n  Parameters: {param_str}"),
                (true, true) => format!("- {child_name}"),
            });
        }
    }
    let full = if !entries.is_empty() {
        let catalog = format!("Available tools in this namespace:\n{}", entries.join("\n"));
        if description.is_empty() {
            format!("Tools in namespace {namespace}.\n\n{catalog}")
        } else {
            format!("{description}\n\n{catalog}")
        }
    } else if description.is_empty() {
        format!("Tools in namespace {namespace}.")
    } else {
        description
    };
    let mut name_prop = BTreeMap::new();
    name_prop.insert(
        "description",
        json_str(&format!("Child tool name to execute in namespace {namespace}")),
    );
    if !names.is_empty() {
        let quoted: Vec<Vec<u8>> = names.iter().map(|n| json_str(n)).collect();
        name_prop.insert("enum", gj::join(&quoted));
    }
    name_prop.insert("type", json_str("string"));
    let object = |fields: BTreeMap<&str, Vec<u8>>| {
        let parts: Vec<Vec<u8>> = fields
            .into_iter()
            .map(|(k, v)| [json_str(k), b":".to_vec(), v].concat())
            .collect();
        [b"{".to_vec(), parts.join(&b","[..]), b"}".to_vec()].concat()
    };
    let arguments = object(BTreeMap::from([
        ("additionalProperties", b"true".to_vec()),
        (
            "description",
            json_str("Arguments object matching the parameter schema of the selected child tool"),
        ),
        ("type", json_str("object")),
    ]));
    let properties = object(BTreeMap::from([("arguments", arguments), ("name", object(name_prop))]));
    let parameters = object(BTreeMap::from([
        ("properties", properties),
        ("required", gj::join(&[json_str("name")])),
        ("type", json_str("object")),
    ]));
    Some(object(BTreeMap::from([
        ("description", json_str(&full)),
        ("name", json_str(&namespace)),
        ("parameters", parameters),
        ("type", json_str(FUNCTION)),
    ])))
}

/// `normalizeXAIToolsWithFold`: top-level tools and every `additional_tools` input item.
fn normalize_tools(body: Vec<u8>, fold: bool) -> Vec<u8> {
    if !gj::valid(&body) {
        return body;
    }
    let keep_image = supports_native_image_generation(&gj::get(&body, "model").str());
    let original = body.clone();
    let mut body = body;
    let at = |body: &mut Vec<u8>, path: &str| -> bool {
        let tools = gj::get(body, path);
        if !tools.is_array() {
            return true;
        }
        match normalize_tool_array(&tools, keep_image, fold) {
            None => false,
            Some(None) => true,
            Some(Some(filtered)) => gj::set_raw(body, path, filtered),
        }
    };
    if !at(&mut body, "tools") {
        return original;
    }
    let paths: Vec<String> = gj::get(&body, "input")
        .array()
        .iter()
        .enumerate()
        .filter(|(_, item)| &*item.get("type").bytes() == b"additional_tools")
        .map(|(i, _)| format!("input.{i}.tools"))
        .collect();
    if !gj::get(&body, "input").is_array() {
        return body;
    }
    for path in paths {
        if !at(&mut body, &path) {
            return original;
        }
    }
    body
}

/// `normalizeXAIToolArray`: `None` on failure, `Some(None)` when unchanged.
fn normalize_tool_array(tools: &Res<'_>, keep_image: bool, fold: bool) -> Option<Option<Vec<u8>>> {
    let mut filtered = Vec::new();
    let mut changed = false;
    for tool in tools.array() {
        if &*tool.get("type").bytes() == NAMESPACE.as_bytes() {
            changed = true;
            if fold {
                if let Some(dispatcher) = namespace_dispatcher_tool(&tool) {
                    filtered.push(dispatcher);
                }
                continue;
            }
            let namespace = tool.get("name").str().into_owned();
            let nested = tool.get("tools");
            if nested.is_array() {
                for child in nested.array() {
                    let (raw, child_changed) = normalize_tool(&child, &namespace, keep_image)?;
                    changed |= child_changed;
                    if !raw.is_empty() {
                        filtered.push(raw);
                    }
                }
            }
            continue;
        }
        let (raw, tool_changed) = normalize_tool(&tool, "", keep_image)?;
        changed |= tool_changed;
        if !raw.is_empty() {
            filtered.push(raw);
        }
    }
    Some(changed.then(|| gj::join(&filtered)))
}

/// `xaiSchemaTypeIsObjectOnly`.
fn schema_type_object_only(t: &Res<'_>) -> bool {
    if t.kind == Kind::String {
        return t.str().trim().go_eq_fold("object");
    }
    if !t.is_array() {
        return false;
    }
    let all = t.array();
    !all.is_empty()
        && all
            .iter()
            .all(|x| x.kind == Kind::String && x.str().trim().go_eq_fold("object"))
}

/// `isXAICodexAppAutomationUpdate`.
fn is_codex_app_automation_update(tool: &str, namespace: &str) -> bool {
    let fold = |a: &str, b: &str| a.go_eq_fold(b);
    let namespace = namespace.trim();
    let namespace = namespace.strip_prefix("mcp__").unwrap_or(namespace);
    let tool = tool.trim();
    let tool = tool.strip_prefix("mcp__").unwrap_or(tool);
    if fold(tool, AUTOMATION_UPDATE) && (fold(namespace, CODEX_APP_NAMESPACE) || fold(namespace, "codex_apps")) {
        return true;
    }
    fold(tool, &format!("{CODEX_APP_NAMESPACE}__{AUTOMATION_UPDATE}"))
        || fold(tool, &format!("codex_apps__{AUTOMATION_UPDATE}"))
}

/// `xaiFunctionParametersNeedSimplification`.
fn parameters_need_simplification(tool: &Res<'_>, namespace: &str) -> bool {
    let t = text(&tool.get("type"));
    let is_function = t.go_eq_fold(FUNCTION);
    if !is_function && !t.go_eq_fold(CUSTOM) {
        return false;
    }
    if is_function && is_codex_app_automation_update(&text(&tool.get("name")), namespace) {
        return true;
    }
    let parameters = tool.get("parameters");
    ["anyOf", "oneOf"].iter().any(|union| {
        let u = parameters.get(union);
        u.is_array()
            && u.array()
                .iter()
                .any(|branch| branch.get("$ref").exists() || !schema_type_object_only(&branch.get("type")))
    })
}

/// `normalizeXAIObjectRootUnionBranchTypes`: untyped branches of an object root union get
/// `"type":"object"`. `None` on failure, else the tool and whether it changed.
fn object_root_union_branch_types(tool: Vec<u8>) -> Option<(Vec<u8>, bool)> {
    let parameters = gj::get(&tool, "parameters").into_owned();
    let root_type = parameters.get("type");
    if root_type.kind != Kind::String || &*root_type.bytes() != b"object" {
        return Some((tool, false));
    }
    let mut tool = tool;
    let mut changed = false;
    for union in ["anyOf", "oneOf"] {
        let branches = parameters.get(union);
        if !branches.is_array() {
            continue;
        }
        for (i, branch) in branches.array().iter().enumerate() {
            if !branch.is_object() || branch.get("type").exists() || branch.get("$ref").exists() {
                continue;
            }
            tool = gj::try_set_str(&tool, &format!("parameters.{union}.{i}.type"), "object").ok()?;
            changed = true;
        }
    }
    Some((tool, changed))
}

/// `normalizeXAITool`: `None` on failure; an empty raw drops the tool.
fn normalize_tool(tool: &Res<'_>, namespace: &str, keep_image: bool) -> Option<(Vec<u8>, bool)> {
    let mut tool_type = tool.get("type").str().into_owned();
    if tool_type == TOOL_SEARCH || (tool_type == IMAGE_GENERATION && !keep_image) {
        return Some((Vec::new(), true));
    }
    let mut changed = false;
    let mut raw = tool.raw().to_vec();
    // The schema view the later checks read (Go re-parses only after schema edits).
    let mut schema = raw.clone();
    if tool_type == FUNCTION || tool_type == CUSTOM {
        let params = tool.get("parameters");
        if params.exists() {
            let inlined = cpa_common::gemini_schema::inline_local_refs(params.raw());
            if inlined != params.raw()
                && let Ok(mut updated) = gj::try_set_raw(&raw, "parameters", &inlined)
            {
                for key in ["parameters.$defs", "parameters.definitions"] {
                    if gj::get(&updated, key).exists() {
                        gj::delete(&mut updated, key);
                    }
                }
                raw = updated;
                schema = raw.clone();
                changed = true;
            }
        }
        let (updated, schema_changed) = object_root_union_branch_types(raw)?;
        raw = updated;
        if schema_changed {
            schema = raw.clone();
            changed = true;
        }
    }
    if tool_type == CUSTOM {
        raw = gj::try_set_str(&raw, "type", FUNCTION).ok()?;
        tool_type = FUNCTION.into();
        changed = true;
    }
    if tool_type == WEB_SEARCH && tool.get("external_web_access").exists() {
        raw = gj::try_delete(&raw, "external_web_access").ok()?;
        changed = true;
    }
    let schema = gj::parse(&schema);
    if tool_type == FUNCTION && !schema.get("parameters").exists() {
        raw = gj::try_set_raw(&raw, "parameters", r#"{"type":"object","properties":{}}"#).ok()?;
        changed = true;
    }
    if tool_type == FUNCTION && parameters_need_simplification(&schema, namespace) {
        raw = gj::try_set_raw(&raw, "parameters", SAFE_FUNCTION_PARAMETERS).ok()?;
        let strict = tool.get("strict");
        if strict.exists() && strict.bool() {
            raw = gj::try_set_raw(&raw, "strict", "false").ok()?;
        }
        changed = true;
    }
    if tool_type == FUNCTION && !namespace.trim().is_empty() {
        let qualified = response::qualify(namespace, &tool.get("name").str());
        if qualified.is_empty() {
            return None;
        }
        raw = gj::try_set_str(&raw, "name", qualified).ok()?;
        changed = true;
    }
    Some((raw, changed))
}

/// `xaiHasFunctionToolNamed`.
fn has_function_tool_named(body: &[u8], name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let is = |t: &Res<'_>| &*t.get("type").bytes() == FUNCTION.as_bytes() && &*t.get("name").bytes() == name.as_bytes();
    let tools = gj::get(body, "tools");
    if tools.is_array() && tools.array().iter().any(is) {
        return true;
    }
    let input = gj::get(body, "input");
    input.is_array()
        && input
            .array()
            .iter()
            .any(|item| &*item.get("type").bytes() == b"additional_tools" && item.get("tools").array().iter().any(is))
}

/// `clampXAIToolsLimit`: at most `max` tools, namespace dispatchers first.
fn clamp_tools_limit(body: Vec<u8>, max: usize, refs: &NamespaceRefs) -> Vec<u8> {
    let tools = gj::get(&body, "tools");
    if !tools.is_array() || tools.array().len() <= max {
        return body;
    }
    let (mut dispatchers, mut regular) = (Vec::new(), Vec::new());
    for tool in tools.array() {
        if refs.get(&text(&tool.get("name"))).is_some_and(|r| r.dispatcher) {
            dispatchers.push(tool.raw().to_vec());
        } else {
            regular.push(tool.raw().to_vec());
        }
    }
    dispatchers.truncate(max);
    let room = max - dispatchers.len();
    regular.truncate(room);
    dispatchers.extend(regular);
    let Ok(updated) = gj::try_set_raw(&body, "tools", response::marshal_raw(&dispatchers)) else {
        return body;
    };
    normalize_tool_choice_for_tools(prune_orphaned_tool_choice(updated))
}

/// `promoteXAIAdditionalTools`: Responses Lite `additional_tools` input items move into
/// the top-level tools; xAI rejects the item type.
fn promote_additional_tools(body: Vec<u8>) -> Vec<u8> {
    if !gj::valid(&body) {
        return body;
    }
    let input = gj::get(&body, "input");
    if !input.is_array() {
        return body;
    }
    let items = input.array();
    let mut remaining = Vec::new();
    let mut promoted = Vec::new();
    for item in &items {
        if &*item.get("type").bytes() != b"additional_tools" {
            remaining.push(item.raw().to_vec());
            continue;
        }
        promoted.extend(item.get("tools").array().iter().map(|t| t.raw().to_vec()));
    }
    if remaining.len() == items.len() {
        return body;
    }
    let Ok(updated) = gj::try_set_raw(&body, "input", response::marshal_raw(&remaining)) else {
        return body;
    };
    if promoted.is_empty() {
        return updated;
    }
    let top = gj::get(&updated, "tools");
    let mut tools: Vec<Vec<u8>> = if top.is_array() {
        top.array().iter().map(|t| t.raw().to_vec()).collect()
    } else {
        Vec::new()
    };
    tools.extend(promoted);
    gj::try_set_raw(&updated, "tools", response::marshal_raw(&tools)).unwrap_or(body)
}

/// `normalizeXAIToolChoiceForTools`: without tools, `tool_choice` and
/// `parallel_tool_calls` go too.
pub(crate) fn normalize_tool_choice_for_tools(mut body: Vec<u8>) -> Vec<u8> {
    let tools = gj::get(&body, "tools");
    let mut has_tools = tools.is_array() && !tools.array().is_empty();
    if !has_tools {
        let input = gj::get(&body, "input");
        has_tools = input.is_array()
            && input.array().iter().any(|item| {
                let extra = item.get("tools");
                &*item.get("type").bytes() == b"additional_tools" && extra.is_array() && !extra.array().is_empty()
            });
    }
    if has_tools {
        return body;
    }
    for key in ["tools", "tool_choice", "parallel_tool_calls"] {
        if gj::get(&body, key).exists() {
            gj::delete(&mut body, key);
        }
    }
    body
}

/// `normalizeXAINamespaceToolChoiceWithFold`: namespaced function choices name the tool
/// actually sent (the dispatcher or the qualified name).
fn normalize_namespace_tool_choice(body: Vec<u8>, fold: bool) -> Vec<u8> {
    if !gj::valid(&body) {
        return body;
    }
    let original = body.clone();
    let mut body = body;
    let at = |body: &mut Vec<u8>, path: &str| -> bool {
        let choice = gj::get(body, path).into_owned();
        if !choice.is_object() || &*choice.get("type").bytes() != FUNCTION.as_bytes() {
            return true;
        }
        let namespace = text(&choice.get("namespace"));
        if namespace.is_empty() {
            return true;
        }
        let qualified = response::qualify(&namespace, &text(&choice.get("name")));
        let target = if has_function_tool_named(body, &namespace) {
            namespace
        } else if has_function_tool_named(body, &qualified) {
            qualified
        } else if fold {
            namespace
        } else {
            qualified
        };
        if target.is_empty() {
            return true;
        }
        gj::set_str(body, &format!("{path}.name"), target) && gj::delete(body, &format!("{path}.namespace"))
    };
    if !at(&mut body, "tool_choice") {
        return original;
    }
    let count = gj::get(&body, "tool_choice.tools").array().len();
    if gj::get(&body, "tool_choice.tools").is_array() {
        for i in 0..count {
            if !at(&mut body, &format!("tool_choice.tools.{i}")) {
                return original;
            }
        }
    }
    body
}

/// `collectXAINamespaceToolRefsWithFold`.
fn collect_namespace_refs(body: &[u8], fold: bool) -> NamespaceRefs {
    let mut refs = HashMap::new();
    let mut collect = |tools: &Res<'_>| {
        if !tools.is_array() {
            return;
        }
        for tool in tools.array() {
            if &*tool.get("type").bytes() != NAMESPACE.as_bytes() {
                continue;
            }
            let namespace = text(&tool.get("name"));
            if namespace.is_empty() {
                continue;
            }
            if fold {
                refs.insert(
                    namespace.clone(),
                    NamespaceRef {
                        namespace: namespace.clone(),
                        name: String::new(),
                        dispatcher: true,
                    },
                );
            }
            for nested in tool.get("tools").array() {
                let name = text(&nested.get("name"));
                let qualified = response::qualify(&namespace, &name);
                if qualified.is_empty() {
                    continue;
                }
                refs.insert(
                    qualified,
                    NamespaceRef {
                        namespace: namespace.clone(),
                        name,
                        dispatcher: false,
                    },
                );
            }
        }
    };
    collect(&gj::get(body, "tools"));
    let input = gj::get(body, "input");
    if input.is_array() {
        for item in input.array() {
            if &*item.get("type").bytes() == b"additional_tools" {
                collect(&item.get("tools"));
            }
        }
    }
    refs
}

/// `normalizeXAIInputCustomToolCalls`: custom tool history as function calls.
fn normalize_input_custom_tool_calls(body: Vec<u8>) -> Vec<u8> {
    let input = gj::get(&body, "input");
    if !input.is_array() {
        return body;
    }
    let mut changed = false;
    let mut items = Vec::new();
    for item in input.array() {
        let normalized = match &*item.get("type").bytes() {
            b"custom_tool_call" => {
                let (call_id, name) = (text(&item.get("call_id")), text(&item.get("name")));
                if call_id.is_empty() || name.is_empty() {
                    changed = true;
                    continue;
                }
                let mut n = br#"{"type":"function_call"}"#.to_vec();
                gj::set_str(&mut n, "call_id", call_id);
                gj::set_str(&mut n, "name", name);
                gj::set_str(&mut n, "arguments", custom_tool_call_arguments(&item.get("input")));
                n
            }
            b"custom_tool_call_output" => {
                let call_id = text(&item.get("call_id"));
                if call_id.is_empty() {
                    changed = true;
                    continue;
                }
                let mut n = br#"{"type":"function_call_output"}"#.to_vec();
                gj::set_str(&mut n, "call_id", call_id);
                let output = item.get("output");
                let out = if !output.exists() {
                    Vec::new()
                } else if output.kind == Kind::String {
                    output.bytes().into_owned()
                } else {
                    output.raw().to_vec()
                };
                gj::set_str(&mut n, "output", out);
                n
            }
            _ => {
                items.push(item.raw().to_vec());
                continue;
            }
        };
        items.push(normalized);
        changed = true;
    }
    if !changed {
        return body;
    }
    gj::try_set_raw(&body, "input", response::marshal_raw(&items)).unwrap_or(body)
}

/// `xaiCustomToolCallArguments`.
fn custom_tool_call_arguments(input: &Res<'_>) -> Vec<u8> {
    if !input.exists() {
        return b"{}".to_vec();
    }
    if input.kind == Kind::String {
        let value = input.bytes();
        let trimmed = crate::meta_codex::go_trim_space(&value);
        if gj::valid(trimmed) {
            let parsed = gj::parse(trimmed);
            if parsed.is_object() {
                return parsed.raw().to_vec();
            }
        }
        let mut quoted = Vec::new();
        gj::marshal_str(&mut quoted, &value, true);
        return [b"{\"input\":".to_vec(), quoted, b"}".to_vec()].concat();
    }
    if input.is_object() {
        return input.raw().to_vec();
    }
    if !input.raw().is_empty() {
        return [b"{\"input\":", input.raw(), b"}"].concat();
    }
    b"{}".to_vec()
}

/// `normalizeXAIInputNamespaceToolCallsWithFold`: namespaced function-call history in
/// the form xAI saw it (a dispatcher call or the qualified name).
fn normalize_input_namespace_tool_calls(mut body: Vec<u8>, fold: bool) -> Vec<u8> {
    if !gj::valid(&body) {
        return body;
    }
    let input = gj::get(&body, "input").into_owned();
    if !input.is_array() {
        return body;
    }
    for (index, item) in input.array().iter().enumerate() {
        if &*item.get("type").bytes() != b"function_call" {
            continue;
        }
        let namespace = text(&item.get("namespace"));
        let name = text(&item.get("name"));
        if namespace.is_empty() {
            continue;
        }
        let qualified = response::qualify(&namespace, &name);
        let folded = if has_function_tool_named(&body, &namespace) {
            true
        } else if has_function_tool_named(&body, &qualified) {
            false
        } else {
            fold
        };
        let (name_path, namespace_path) = (format!("input.{index}.name"), format!("input.{index}.namespace"));
        if folded {
            // json.Marshal(map{"name", "arguments"}): sorted keys; raw JSON arguments are
            // compacted, anything else is a string.
            let raw_args = item.get("arguments").bytes().into_owned();
            let mut encoded = b"{".to_vec();
            if !raw_args.is_empty() {
                encoded.extend_from_slice(b"\"arguments\":");
                if gj::valid(&raw_args) {
                    encoded.extend(gj::compact(&raw_args, true));
                } else {
                    gj::marshal_str(&mut encoded, &raw_args, true);
                }
                encoded.push(b',');
            }
            encoded.extend_from_slice(b"\"name\":");
            gj::marshal_str(&mut encoded, name.as_bytes(), true);
            encoded.push(b'}');
            let mut updated = body.clone();
            if gj::set_str(&mut updated, &name_path, &namespace)
                && gj::set_str(&mut updated, &format!("input.{index}.arguments"), &encoded)
                && gj::delete(&mut updated, &namespace_path)
            {
                body = updated;
            }
            continue;
        }
        if qualified.is_empty() {
            continue;
        }
        let mut updated = body.clone();
        if gj::set_str(&mut updated, &name_path, &qualified) && gj::delete(&mut updated, &namespace_path) {
            body = updated;
        }
    }
    body
}

/// `normalizeXAIImageRefs`: `image`, `images` and `reference_images` entries anywhere
/// in the payload use xAI's `url` field instead of OpenAI's `image_url`. Go re-encodes
/// the whole document when it changes anything.
pub(crate) fn normalize_image_refs(body: Vec<u8>) -> Vec<u8> {
    if !gj::valid(&body) {
        return body;
    }
    let Some(mut value) = GoValue::parse(&body) else {
        return body;
    };
    if !image_refs_value(&mut value) {
        return body;
    }
    value.marshal()
}

fn image_refs_value(value: &mut GoValue) -> bool {
    let mut changed = false;
    match value {
        GoValue::Object(map) => {
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    "image" => changed |= image_ref(child),
                    "images" | "reference_images" => {
                        if let GoValue::Array(refs) = child {
                            for r in refs.iter_mut() {
                                changed |= image_ref(r);
                            }
                        }
                    }
                    _ => {}
                }
                changed |= image_refs_value(child);
            }
        }
        GoValue::Array(items) => {
            for item in items.iter_mut() {
                changed |= image_refs_value(item);
            }
        }
        _ => {}
    }
    changed
}

/// `normalizeXAIImageRef`.
fn image_ref(value: &mut GoValue) -> bool {
    let GoValue::Object(r) = value else {
        return false;
    };
    let original_url = match r.get("url") {
        Some(GoValue::String(s)) => s.clone(),
        _ => String::new(),
    };
    let mut url = original_url.trim().to_owned();
    let has_image_url = r.contains_key("image_url");
    if url.is_empty() {
        url = match r.get("image_url") {
            Some(GoValue::String(s)) => s.trim().to_owned(),
            Some(GoValue::Object(m)) => match m.get("url") {
                Some(GoValue::String(s)) => s.trim().to_owned(),
                _ => String::new(),
            },
            _ => String::new(),
        };
    }
    if url.is_empty() || (url == original_url && !has_image_url) {
        return false;
    }
    r.insert("url".into(), GoValue::String(url));
    r.remove("image_url");
    true
}
