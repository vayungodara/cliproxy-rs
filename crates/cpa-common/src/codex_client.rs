//! Codex-client request rewrites shared by every executor that serves Codex clients
//! (internal/client/codex/optimize-multi-agent-v2: optimize_multi_agent_v2.go and
//! orphan_delegation.go, and normalizeCodexInstructions).
//!
//! - Orphan delegation: with `oauth.providers.codex.orphan-delegation-compatibility` and
//!   `X-Openai-Subagent: collab_spawn`, `codex_app` delegation outputs without a matching
//!   call become user messages.
//! - Multi-agent v2 (`client.codex.optimize-multi-agent-v2`, official Codex clients only):
//!   `agent_message` input becomes portable messages for non-Codex targets, collaboration
//!   tools lose `message.encrypted`, `spawn_agent` descriptions list the models this
//!   proxy serves, and the `collaboration` namespace is renamed upstream and restored in
//!   responses.
//!
//! Edits go through [`crate::json`], so untouched bytes stay as the client sent them.

use std::collections::{HashMap, HashSet};

use cpa_core::config::Config;
use http::HeaderMap;

use crate::json::{self as gj, Kind, Res};

const DESCRIPTION_MARKER: &str = "Spawns an agent";
const MODELS_HEADING: &str = "Available model overrides (optional; inherited parent model is preferred):";
const NAMESPACE: &str = "collaboration";
const OPTIMIZED_NAMESPACE: &str = "collaboration-optimize";
const OPTIMIZED_NAME_PREFIX: &str = "collaboration-optimize__";
const OPTIMIZED_DOT_PREFIX: &str = "collaboration-optimize.";
const MESSAGE_TOOLS: [&str; 3] = ["spawn_agent", "send_message", "followup_task"];

/// The embedded Codex client model catalog (Go embeds `models/codex_client_models.json`);
/// [`crate::codex_catalog`] serves it until a remote refresh replaces it.
pub const CLIENT_MODELS_JSON: &str = include_str!("codex_client_models.json");

/// The two settings these rewrites read from one config snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Settings {
    /// `client.codex.optimize-multi-agent-v2`.
    pub optimize_multi_agent_v2: bool,
    /// `oauth.providers.codex.orphan-delegation-compatibility` (Go `cfg.Codex`). API-key
    /// credentials read it from `Config::for_api_key`.
    pub orphan_delegation: bool,
}

impl Settings {
    pub fn from_config(cfg: &Config) -> Self {
        let flag = |path: &[&str]| {
            path.iter()
                .try_fold(&cfg.document, |v, k| v.get(*k))
                .and_then(serde_yaml_ng::Value::as_bool)
                .unwrap_or(false)
        };
        Self {
            optimize_multi_agent_v2: flag(&["client", "codex", "optimize-multi-agent-v2"]),
            orphan_delegation: flag(&["oauth", "providers", "codex", ORPHAN_PATH]),
        }
    }

    /// The Responses API boundary's view (Go's handler `Cfg`): orphan delegation written in
    /// v8 form under `oauth.providers.codex` is OAuth-only and does not apply there.
    pub fn for_responses_handler(cfg: &Config) -> Self {
        let mut settings = Self::from_config(cfg);
        if cfg.oauth_only.contains(&format!("oauth.providers.codex.{ORPHAN_PATH}")) {
            settings.orphan_delegation = false;
        }
        settings
    }
}

const ORPHAN_PATH: &str = "orphan-delegation-compatibility";

/// Go's Responses handlers (`prepareCodexMultiAgentV2Tools`, then
/// `prepareCodexOrphanDelegation`) on a request body before dispatch.
pub fn prepare_responses_request(headers: &HeaderMap, payload: &[u8], settings: &Settings) -> Vec<u8> {
    let (prepared, _) = prepare_tools(
        headers,
        payload,
        settings.optimize_multi_agent_v2,
        served_spawn_agent_models,
    );
    rewrite_orphan_delegation_input(headers, &prepared, settings.orphan_delegation)
}

/// `headerValueCaseInsensitive`: the first non-blank value, trimmed.
fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get_all(name)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).trim().to_owned())
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// `IsCodexClientUserAgent`: an official Codex client identity.
pub fn is_codex_client_user_agent(user_agent: &str) -> bool {
    let ua = user_agent.trim();
    ua.starts_with("Codex Desktop/")
        || ua.starts_with("codex-tui/")
        || ua == "codex_cli_rs"
        || ua.starts_with("codex_cli_rs/")
        || ua.starts_with("codex_exec/")
}

/// Go `strings.EqualFold(value, ascii)` for an ASCII `ascii`: besides ASCII case, only
/// U+017F (long s) and U+212A (Kelvin sign) fold onto ASCII letters.
fn folds_to_ascii(value: &str, ascii: &str) -> bool {
    let fold = |c: char| match c {
        '\u{17f}' => 's',
        '\u{212a}' => 'k',
        c => c.to_ascii_lowercase(),
    };
    value
        .chars()
        .map(fold)
        .eq(ascii.chars().map(|c| c.to_ascii_lowercase()))
}

/// Whether `prepare_tools` rewrites this request: the setting is on and the caller is
/// an official Codex client.
pub fn multi_agent_client(headers: &HeaderMap, enabled: bool) -> bool {
    enabled && is_codex_client_user_agent(&header(headers, "user-agent"))
}

fn text(r: &Res<'_>) -> String {
    r.str().trim().to_owned()
}

fn edit(payload: &[u8], edits: impl FnOnce(&mut Vec<u8>) -> bool) -> Vec<u8> {
    let mut out = payload.to_vec();
    if edits(&mut out) { out } else { payload.to_vec() }
}

// ---------------------------------------------------------------------- orphan delegation

/// `RewriteCodexOrphanDelegationInput`.
pub fn rewrite_orphan_delegation_input(headers: &HeaderMap, payload: &[u8], enabled: bool) -> Vec<u8> {
    if !enabled || payload.is_empty() || !folds_to_ascii(&header(headers, "x-openai-subagent"), "collab_spawn") {
        return payload.to_vec();
    }
    let input = gj::get(payload, "input");
    if !input.is_array() {
        return payload.to_vec();
    }
    let items = input.array();
    let mut calls: HashMap<Vec<u8>, usize> = HashMap::new();
    for item in &items {
        if *item.get("type").bytes() == *b"function_call" {
            let id = item.get("call_id").bytes().into_owned();
            if !crate::gostr::trim_space(&id).is_empty() {
                *calls.entry(id).or_default() += 1;
            }
        }
    }
    edit(payload, |out| {
        for (index, item) in items.iter().enumerate() {
            if *item.get("type").bytes() != *b"function_call_output" {
                continue;
            }
            let id = item.get("call_id").bytes().into_owned();
            if !crate::gostr::trim_space(&id).is_empty()
                && let Some(count) = calls.get_mut(&id).filter(|c| **c > 0)
            {
                // Paired with a call in the same request: consumed and kept.
                *count -= 1;
                continue;
            }
            if *item.get("namespace").bytes() != *b"codex_app" {
                continue;
            }
            let label = match &*item.get("name").bytes() {
                b"create_thread" => "codex_app__create_thread",
                b"send_message_to_thread" => "codex_app__send_message_to_thread",
                _ => continue,
            };
            let output = item.get("output");
            let mut message = format!("Tool output from {label}:\n").into_bytes();
            if output.exists() {
                if output.kind == Kind::String {
                    message.extend_from_slice(&output.bytes());
                } else {
                    message.extend_from_slice(output.raw());
                }
            }
            let mut user = br#"{"type":"message","role":"user","content":[{"type":"input_text","text":""}]}"#.to_vec();
            gj::set_str(&mut user, "content.0.text", &message);
            if !gj::set_raw(out, &format!("input.{index}"), &user) {
                return false;
            }
        }
        true
    })
}

// ------------------------------------------------------------------ multi-agent v2 input

/// `RewriteCodexMultiAgentV2Input`: `agent_message` input as standard messages when the
/// optimization is on for an official client; `compat` also strips non-standard fields.
pub fn rewrite_multi_agent_v2_input(headers: &HeaderMap, payload: &[u8], settings: &Settings, compat: bool) -> Vec<u8> {
    let optimize = compat || multi_agent_client(headers, settings.optimize_multi_agent_v2);
    if !compat && !optimize {
        return payload.to_vec();
    }
    let input = gj::get(payload, "input");
    if !input.is_array() {
        return payload.to_vec();
    }
    let start = if optimize {
        rewrite_agent_message_content(payload)
    } else {
        payload.to_vec()
    };
    edit(&start, |out| {
        for (index, item) in input.array().iter().enumerate() {
            let path = format!("input.{index}");
            if text(&item.get("type")) == "agent_message"
                && optimize
                && !(gj::set_str(out, &format!("{path}.role"), "user")
                    && gj::set_str(out, &format!("{path}.type"), "message"))
            {
                return false;
            }
            if compat {
                for field in ["author", "recipient", "internal_chat_message_metadata_passthrough"] {
                    if item.get(field).exists() && !gj::delete(out, &format!("{path}.{field}")) {
                        return false;
                    }
                }
            }
        }
        true
    })
}

/// `rewriteCodexAgentMessageContent`: `encrypted_content` parts of `agent_message` input
/// become `input_text`.
fn rewrite_agent_message_content(payload: &[u8]) -> Vec<u8> {
    let input = gj::get(payload, "input");
    if !input.is_array() {
        return payload.to_vec();
    }
    edit(payload, |out| {
        for (index, item) in input.array().iter().enumerate() {
            if text(&item.get("type")) != "agent_message" {
                continue;
            }
            let content = item.get("content");
            if !content.is_array() {
                continue;
            }
            for (part_index, part) in content.array().iter().enumerate() {
                if text(&part.get("type")) != "encrypted_content" {
                    continue;
                }
                let encrypted = part.get("encrypted_content");
                if encrypted.kind != Kind::String {
                    continue;
                }
                let path = format!("input.{index}.content.{part_index}");
                if !(gj::set_str(out, &format!("{path}.type"), "input_text")
                    && gj::set_str(out, &format!("{path}.text"), encrypted.bytes())
                    && gj::delete(out, &format!("{path}.encrypted_content")))
                {
                    return false;
                }
            }
        }
        true
    })
}

// ------------------------------------------------------------------- collaboration tools

/// `codexToolPathsByNames`: function tools with these names in `tools`, nested
/// namespaces and `input[].additional_tools`.
fn tool_paths(payload: &[u8], names: &[&str]) -> Vec<String> {
    fn collect(tools: &Res<'_>, path: &str, names: &[&str], out: &mut Vec<String>) {
        if !tools.is_array() {
            return;
        }
        for (index, tool) in tools.array().iter().enumerate() {
            let tool_path = format!("{path}.{index}");
            match text(&tool.get("type")).as_str() {
                "function" if names.contains(&text(&tool.get("name")).as_str()) => out.push(tool_path),
                "namespace" => collect(&tool.get("tools"), &format!("{tool_path}.tools"), names, out),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    collect(&gj::get(payload, "tools"), "tools", names, &mut out);
    let input = gj::get(payload, "input");
    if input.is_array() {
        for (index, item) in input.array().iter().enumerate() {
            if text(&item.get("type")) == "additional_tools" {
                collect(&item.get("tools"), &format!("input.{index}.tools"), names, &mut out);
            }
        }
    }
    out
}

/// `HasCodexMultiAgentV2NamespaceConflict`: the request already uses the reserved
/// optimized namespace, which must stay untouched.
pub fn has_namespace_conflict(payload: &[u8]) -> bool {
    fn conflict(tools: &Res<'_>) -> bool {
        tools.is_array()
            && tools.array().iter().any(|tool| {
                let name = text(&tool.get("name"));
                name == OPTIMIZED_NAMESPACE
                    || name.starts_with(OPTIMIZED_NAME_PREFIX)
                    || name.starts_with(OPTIMIZED_DOT_PREFIX)
                    || (text(&tool.get("type")) == "namespace" && conflict(&tool.get("tools")))
            })
    }
    if conflict(&gj::get(payload, "tools")) {
        return true;
    }
    let input = gj::get(payload, "input");
    input.is_array()
        && input
            .array()
            .iter()
            .any(|item| text(&item.get("type")) == "additional_tools" && conflict(&item.get("tools")))
}

/// `removeCodexCollaborationMessageEncryption`.
fn remove_message_encryption(payload: &[u8], paths: &[String]) -> Vec<u8> {
    edit(payload, |out| {
        for path in paths {
            let encrypted = format!("{path}.parameters.properties.message.encrypted");
            if gj::get(out, encrypted.as_str()).exists() && !gj::delete(out, &encrypted) {
                return false;
            }
        }
        true
    })
}

/// `PrepareCodexMultiAgentV2Tools`: strips `message.encrypted` from collaboration tools
/// and lists the served models in `spawn_agent` descriptions. `models` builds that list
/// (Markdown) only when a `spawn_agent` tool needs it. Returns whether the client is
/// eligible.
pub fn prepare_tools(
    headers: &HeaderMap,
    payload: &[u8],
    enabled: bool,
    models: impl FnOnce() -> String,
) -> (Vec<u8>, bool) {
    if !multi_agent_client(headers, enabled) {
        return (payload.to_vec(), false);
    }
    let spawn = tool_paths(payload, &["spawn_agent"]);
    let messages = tool_paths(payload, &MESSAGE_TOOLS);
    if spawn.is_empty() && messages.is_empty() {
        return (payload.to_vec(), true);
    }
    if has_namespace_conflict(payload) {
        return (remove_message_encryption(payload, &messages), true);
    }
    let list = if spawn.is_empty() { String::new() } else { models() };
    let updated = edit(payload, |out| {
        for path in &spawn {
            let description_path = format!("{path}.description");
            let description = gj::get(out, description_path.as_str());
            if description.kind == Kind::String && !list.is_empty() {
                let current = description.str().into_owned();
                let rewritten = replace_spawn_agent_models(&current, &list);
                if rewritten != current && !gj::set_str(out, &description_path, &rewritten) {
                    return false;
                }
            }
        }
        true
    });
    (remove_message_encryption(&updated, &messages), true)
}

/// `OptimizeCodexMultiAgentV2Request`: prepares the tools (unless the Responses boundary
/// already did) and renames the `collaboration` namespace that holds `spawn_agent`.
/// Returns whether the namespace was renamed, so responses must be restored.
pub fn optimize_request(
    headers: &HeaderMap,
    payload: &[u8],
    settings: &Settings,
    tools_prepared: bool,
    models: impl FnOnce() -> String,
) -> (Vec<u8>, bool) {
    if !multi_agent_client(headers, settings.optimize_multi_agent_v2) {
        return (payload.to_vec(), false);
    }
    let mut updated = rewrite_agent_message_content(payload);
    if tools_prepared {
        updated = remove_message_encryption(&updated, &tool_paths(&updated, &MESSAGE_TOOLS));
    } else {
        updated = prepare_tools(headers, &updated, settings.optimize_multi_agent_v2, models).0;
    }
    let spawn = tool_paths(&updated, &["spawn_agent"]);
    if spawn.is_empty() || has_namespace_conflict(&updated) {
        return (updated, false);
    }
    // optimizeCodexCollaborationNamespace
    let mut out = updated.clone();
    let mut renamed = false;
    for path in &spawn {
        let Some(separator) = path.rfind(".tools.") else {
            continue;
        };
        let namespace_path = &path[..separator];
        let namespace = gj::get(&out, namespace_path);
        if text(&namespace.get("type")) != "namespace" || text(&namespace.get("name")) != NAMESPACE {
            continue;
        }
        if !gj::set_str(&mut out, &format!("{namespace_path}.name"), OPTIMIZED_NAMESPACE) {
            return (updated, false);
        }
        renamed = true;
    }
    (out, renamed)
}

/// `RestoreCodexMultiAgentV2Response`: the client's `collaboration` names back in an
/// upstream event or body (Go decodes with `UseNumber` and re-marshals).
pub fn restore_response(payload: &[u8], optimized: bool) -> Vec<u8> {
    use crate::json::GoValue;
    if !optimized || payload.is_empty() || !gj::valid(payload) {
        return payload.to_vec();
    }
    let Some(mut value) = GoValue::parse(payload) else {
        return payload.to_vec();
    };
    if !restore_value(&mut value) {
        return payload.to_vec();
    }
    value.marshal()
}

fn restore_value(value: &mut crate::json::GoValue) -> bool {
    use crate::json::GoValue;
    let mut changed = false;
    match value {
        GoValue::Array(items) => {
            for item in items {
                changed |= restore_value(item);
            }
        }
        GoValue::Object(map) => {
            let item_type = match map.get("type") {
                Some(GoValue::String(s)) => s.trim().to_owned(),
                _ => String::new(),
            };
            let tool_call = item_type == "function_call" || item_type == "custom_tool_call";
            if tool_call && matches!(map.get("namespace"), Some(GoValue::String(s)) if s == OPTIMIZED_NAMESPACE) {
                map.insert("namespace".into(), GoValue::String(NAMESPACE.into()));
                changed = true;
            }
            if let Some(GoValue::String(name)) = map.get("name").cloned() {
                if name == OPTIMIZED_NAMESPACE && item_type == "namespace" {
                    map.insert("name".into(), GoValue::String(NAMESPACE.into()));
                    changed = true;
                } else if tool_call && let Some(tool) = name.strip_prefix(OPTIMIZED_DOT_PREFIX) {
                    if !tool.is_empty() {
                        map.insert("namespace".into(), GoValue::String(NAMESPACE.into()));
                        map.insert("name".into(), GoValue::String(tool.into()));
                        changed = true;
                    }
                } else if tool_call && let Some(rest) = name.strip_prefix(OPTIMIZED_NAME_PREFIX) {
                    map.insert("name".into(), GoValue::String(format!("{NAMESPACE}__{rest}")));
                    changed = true;
                }
            }
            let output_item = item_type == "function_call_output" || item_type == "custom_tool_call_output";
            for (key, child) in map.iter_mut() {
                if key == "arguments" || key == "input" || (key == "output" && output_item) {
                    continue;
                }
                changed |= restore_value(child);
            }
        }
        _ => {}
    }
    changed
}

// ------------------------------------------------------------------- spawn-agent models

/// One model the proxy serves, as Go's `GetAvailableModels("openai")` lists it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AvailableModel {
    pub id: String,
    pub display_name: String,
    pub description: String,
}

/// What the model registry knows about a model (Go `LookupModelInfo`).
#[derive(Debug, Clone, Default)]
pub struct ModelFacts {
    pub description: String,
    pub thinking_levels: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct SpawnModel {
    id: String,
    description: String,
    efforts: Vec<String>,
    default_effort: String,
    tiers: Vec<String>,
    priority: i64,
    display_name: String,
}

fn catalog_string(model: &Res<'_>, key: &str) -> String {
    let v = model.get(key);
    if v.kind == Kind::String {
        text(&v)
    } else {
        String::new()
    }
}

/// `mapInt`: a JSON number truncated to an integer, else 0.
fn catalog_int(model: &Res<'_>, key: &str) -> i64 {
    let v = model.get(key);
    if v.kind == Kind::Number { v.num as i64 } else { 0 }
}

fn normalize_effort(effort: &str) -> String {
    let effort = effort.trim().to_lowercase();
    match effort.as_str() {
        "none" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra" => effort,
        _ => String::new(),
    }
}

/// `codexSpawnAgentModelFromMetadata`.
fn from_template(id: &str, template: &Res<'_>) -> SpawnModel {
    let mut efforts = Vec::new();
    let levels = template.get("supported_reasoning_levels");
    if levels.is_array() {
        for level in levels.array() {
            let effort = if level.is_object() {
                normalize_effort(&catalog_string(&level, "effort"))
            } else {
                String::new()
            };
            if !effort.is_empty() {
                efforts.push(effort);
            }
        }
    }
    let mut default_effort = String::new();
    if !efforts.is_empty() {
        default_effort = normalize_effort(&catalog_string(template, "default_reasoning_level"));
        if !efforts.contains(&default_effort) {
            default_effort = efforts[0].clone();
        }
    }
    let mut tiers: Vec<String> = Vec::new();
    let raw_tiers = template.get("service_tiers");
    if raw_tiers.is_array() {
        for tier in raw_tiers.array() {
            let id = if tier.is_object() {
                catalog_string(&tier, "id")
            } else {
                String::new()
            };
            if !id.is_empty() && !tiers.contains(&id) {
                tiers.push(id);
            }
        }
    }
    SpawnModel {
        id: id.to_owned(),
        description: catalog_string(template, "description"),
        efforts,
        default_effort,
        tiers,
        priority: catalog_int(template, "priority"),
        display_name: catalog_string(template, "display_name"),
    }
}

/// `codexSpawnAgentModelsFromTemplates` over the current catalog: catalog models by
/// priority, then other served models by display name with the `gpt-5.5` template's
/// reasoning efforts (or the registry's thinking levels) and no service tiers.
fn spawn_models(available: &[AvailableModel], lookup: &dyn Fn(&str) -> Option<ModelFacts>) -> Vec<SpawnModel> {
    let (raw, _) = crate::codex_catalog::snapshot();
    let catalog = gj::get(raw.as_slice(), "models");
    let mut templates: HashMap<String, Res<'_>> = HashMap::new();
    if catalog.is_array() {
        for model in catalog.array() {
            if !model.is_object() {
                continue;
            }
            let slug = catalog_string(&model, "slug");
            if !slug.is_empty() {
                templates.insert(slug, model);
            }
        }
    }
    let Some(default_template) = templates.get("gpt-5.5") else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let (mut from_catalog, mut synthesized) = (Vec::new(), Vec::new());
    for model in available {
        let id = model.id.trim();
        if id.is_empty() || !seen.insert(id.to_owned()) {
            continue;
        }
        if let Some(template) = templates.get(id) {
            from_catalog.push(from_template(id, template));
            continue;
        }
        let mut profile = from_template(id, default_template);
        profile.description = model.description.trim().to_owned();
        profile.display_name = model.display_name.trim().to_owned();
        if profile.display_name.is_empty() {
            profile.display_name = id.to_owned();
        }
        if let Some(facts) = lookup(id) {
            if !facts.description.trim().is_empty() {
                profile.description = facts.description.trim().to_owned();
            }
            apply_thinking(&mut profile, &facts.thinking_levels);
        }
        if profile.description.is_empty() {
            profile.description = id.to_owned();
        }
        profile.tiers.clear();
        synthesized.push(profile);
    }
    from_catalog.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.id.cmp(&b.id)));
    synthesized.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    from_catalog.extend(synthesized);
    from_catalog
}

/// `applyCodexSpawnAgentThinking`: the registry's discrete levels, defaulting to
/// medium, else the first non-`none` level.
fn apply_thinking(profile: &mut SpawnModel, levels: &[String]) {
    let mut efforts = Vec::new();
    let (mut default_effort, mut first) = (String::new(), String::new());
    for raw in levels {
        let effort = normalize_effort(raw);
        if effort.is_empty() {
            continue;
        }
        if first.is_empty() {
            first.clone_from(&effort);
        }
        if (default_effort.is_empty() && effort != "none") || effort == "medium" {
            default_effort.clone_from(&effort);
        }
        efforts.push(effort);
    }
    if efforts.is_empty() {
        return;
    }
    if default_effort.is_empty() {
        default_effort = first;
    }
    profile.efforts = efforts;
    profile.default_effort = default_effort;
}

fn fields(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `formatCodexSpawnAgentModels`.
fn format_models(models: &[SpawnModel]) -> String {
    let mut list = String::new();
    for model in models {
        let id = fields(&model.id);
        if id.is_empty() {
            continue;
        }
        list.push_str("- ");
        if id.contains('`') {
            list.push_str(&format!("`` {id} ``"));
        } else {
            list.push_str(&format!("`{id}`"));
        }
        list.push_str(": ");
        let mut details = false;
        let description = fields(&model.description);
        if !description.is_empty() {
            list.push_str(&description);
            if !matches!(description.as_bytes().last(), Some(b'.' | b'!' | b'?')) {
                list.push('.');
            }
            details = true;
        }
        if !model.efforts.is_empty() {
            if details {
                list.push(' ');
            }
            list.push_str("Reasoning efforts: ");
            for (index, effort) in model.efforts.iter().enumerate() {
                if index > 0 {
                    list.push_str(", ");
                }
                list.push_str(effort);
                if *effort == model.default_effort {
                    list.push_str(" (default)");
                }
            }
            list.push('.');
            details = true;
        }
        if !model.tiers.is_empty() {
            if details {
                list.push(' ');
            }
            list.push_str("Service tiers: ");
            list.push_str(&model.tiers.join(", "));
            list.push('.');
        }
        list.push('\n');
    }
    list.strip_suffix('\n').map(str::to_owned).unwrap_or(list)
}

/// The Markdown model list for `spawn_agent` descriptions, from `available` and the
/// registry `lookup`.
pub fn spawn_agent_models(available: &[AvailableModel], lookup: &dyn Fn(&str) -> Option<ModelFacts>) -> String {
    format_models(&spawn_models(available, lookup))
}

/// [`spawn_agent_models`] for the models the installed registry serves now.
// ponytail: Go caches the list per catalog revision and registry generation; it is
// rebuilt per request here, only when the optimization is on and a spawn_agent tool is
// present.
pub fn served_spawn_agent_models() -> String {
    let available: Vec<AvailableModel> = cpa_core::registry::available_models()
        .into_iter()
        .map(|m| {
            let field = |k: &str| {
                m.raw
                    .get(k)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            AvailableModel {
                display_name: field("display_name"),
                description: field("description"),
                id: m.id,
            }
        })
        .collect();
    spawn_agent_models(&available, &registry_facts)
}

/// `registry.LookupModelInfo` as the facts the model list reads.
pub fn registry_facts(id: &str) -> Option<ModelFacts> {
    cpa_core::registry::lookup_model(id, None).map(|m| ModelFacts {
        description: m
            .raw
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        thinking_levels: m.thinking.map(|t| t.levels).unwrap_or_default(),
    })
}

/// `replaceCodexSpawnAgentModels`: one model section, placed on the line that holds
/// "Spawns an agent" (or appended), replacing any earlier sections.
fn replace_spawn_agent_models(description: &str, list: &str) -> String {
    if list.is_empty() {
        return description.to_owned();
    }
    let (cleaned, indent) = remove_model_sections(description);
    let section = format!("{indent}{MODELS_HEADING}\n{list}\n");
    if let Some(marker) = cleaned.find(DESCRIPTION_MARKER) {
        let line_start = cleaned[..marker].rfind('\n').map_or(0, |i| i + 1);
        return format!("{}{section}{}", &cleaned[..line_start], &cleaned[line_start..]);
    }
    let separator = if !cleaned.is_empty() && !cleaned.ends_with('\n') {
        "\n\n"
    } else {
        ""
    };
    format!("{cleaned}{separator}{}", section.strip_suffix('\n').unwrap_or(&section))
}

/// `removeCodexSpawnAgentModelSections`: drops every heading and its `- ` lines, and
/// returns the first heading's indentation.
fn remove_model_sections(description: &str) -> (String, String) {
    if !description.contains(MODELS_HEADING) {
        return (description.to_owned(), String::new());
    }
    let lines: Vec<&str> = description.split_inclusive('\n').collect();
    let (mut cleaned, mut indent) = (String::new(), String::new());
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.trim() != MODELS_HEADING {
            cleaned.push_str(line);
            index += 1;
            continue;
        }
        if indent.is_empty()
            && let Some(at) = line.find(MODELS_HEADING).filter(|at| *at > 0)
        {
            indent = line[..at].to_owned();
        }
        index += 1;
        while index < lines.len() && lines[index].trim().starts_with("- ") {
            index += 1;
        }
    }
    (cleaned, indent)
}

// ------------------------------------------------------------------- Codex request body

/// `normalizeCodexInstructions` for non-native requests: a missing or null
/// `instructions` becomes "".
pub fn normalize_codex_instructions(body: &mut Vec<u8>) {
    let instructions = gj::get(body, "instructions");
    if !instructions.exists() || instructions.kind == Kind::Null {
        gj::set_str(body, "instructions", "");
    }
}

#[cfg(test)]
#[path = "codex_client_tests.rs"]
mod tests;
