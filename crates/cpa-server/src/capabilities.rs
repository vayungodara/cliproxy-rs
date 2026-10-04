//! Per-attempt model capability binding (Go sdk/cliproxy/auth/api_key_model_capabilities.go).
//!
//! Go's conductor binds the capabilities of the model an attempt actually sends upstream
//! to the execution request (`attachResolvedExecutionModelInfo`), so executors read
//! `ResolvedModelInfo(req)` instead of re-resolving config entries. In order:
//! - a configured API-key model (`lookupAPIKeyModelCapability`), routed by the client's
//!   model (alias or name, with and without a thinking suffix) and the selected upstream;
//! - any Codex API-key model the credential's entry does not list
//!   (`lookupUnlistedCodexAPIKeyModelCapability`);
//! - a Codex OAuth credential's plan catalog model (`lookupCodexOAuthModelCapability`).
//!
//! The result is [`cpa_core::exec::ExecRequest::resolved_model`].

use cpa_common::gostr::GoStr;
use cpa_common::thinking::parse_suffix;
use cpa_core::config::Config;
use cpa_core::config::credentials;
use cpa_core::credential::{Credential, Source};
use cpa_core::exec::{ResolvedModel, ResolvedSource};
use cpa_core::registry::dynamic::{self, ConfigModel};
use cpa_core::registry::{ModelInfo, ThinkingSupport, pinned};
use serde_json::{Map, Value};

/// Go `attachResolvedExecutionModelInfo`. `execution_model` is set when the attempt
/// executes the request's own model rather than the selection's (Go
/// `restoreExecutionModel`): then only Codex binds, routing by that model.
// ponytail: compiled per attempt from the config snapshot (Go precompiles a routing
// table on every config or credential change); entries are small. Cache it on
// `Config::derived` if profiling ever shows it.
pub fn resolve_attempt(
    cfg: &Config,
    c: &Credential,
    route_model: &str,
    upstream_model: &str,
    execution_model: Option<&str>,
) -> Option<ResolvedModel> {
    match execution_model {
        Some(_) if !c.provider.trim().go_eq_fold("codex") => None,
        Some(model) => resolve(cfg, c, model, model),
        None => resolve(cfg, c, route_model, upstream_model),
    }
}

/// Go `attachResolvedAPIKeyModelInfo` for a fresh attempt.
pub fn resolve(cfg: &Config, c: &Credential, route_model: &str, upstream_model: &str) -> Option<ResolvedModel> {
    let api_key = configured(cfg, c, route_model, upstream_model).or_else(|| unlisted_codex(cfg, c, upstream_model));
    if let Some(info) = api_key {
        return Some(ResolvedModel {
            info,
            source: ResolvedSource::ApiKey,
        });
    }
    codex_oauth(c, upstream_model).map(|info| ResolvedModel {
        info,
        source: ResolvedSource::CodexOAuth,
    })
}

/// Go `attachResolvedHomeModelInfo`, after the local binding of an attempt on a Home
/// credential that executes the selection's model: Home's definition of the dispatched
/// model ([`crate::remote::MODEL_INFO`]) is the attempt's model (Go's
/// `ResolvedModelInfo` reads it first). Its configuration-update support is Home's
/// explicit flag, else `local`'s when that names the same model; Go's stream path binds
/// Home's model before the local one, so callers pass no `local` there. Its is-compat
/// is the selected `credential_options` entry's, never `local`'s. `None` without a Home
/// definition: the local binding stays.
pub fn bind_home(c: &Credential, local: Option<&ResolvedModel>) -> Option<ResolvedModel> {
    let wire: Map<String, Value> = serde_json::from_str(c.attributes.get(crate::remote::MODEL_INFO)?).ok()?;
    let mut info = home_model_info(&wire)?;
    let base = |id: &str| parse_suffix(id.trim()).model_name.trim().to_owned();
    let flag = |info: &ModelInfo, key: &str| info.raw.get(key) == Some(&Value::Bool(true));
    let support = match wire.get("support_configuration_update").and_then(Value::as_bool) {
        Some(explicit) => explicit,
        None => local.is_some_and(|local| {
            base(&local.info.id).go_eq_fold(&base(&info.id)) && flag(&local.info, "support_configuration_update")
        }),
    };
    set_flag(&mut info, "support_configuration_update", support);
    if let Some(options) = c.attributes.get(credentials::HOME_MODEL_OPTIONS) {
        let options: Map<String, Value> = serde_json::from_str(options).unwrap_or_default();
        set_flag(
            &mut info,
            "is_compat",
            options.get("is-compat") == Some(&Value::Bool(true)),
        );
    }
    Some(ResolvedModel {
        info,
        source: ResolvedSource::Home,
    })
}

/// Go `homeDispatchModelInfo.registryModelInfo`: nil without an ID; the context limits,
/// thinking, native capabilities and user-defined flag carried over; never is-compat.
fn home_model_info(wire: &Map<String, Value>) -> Option<ModelInfo> {
    let text = |key: &str| {
        wire.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let id = text("id");
    if id.is_empty() {
        return None;
    }
    let mut raw = Map::new();
    raw.insert("id".into(), id.into());
    let kind = text("type");
    if !kind.is_empty() {
        raw.insert("type".into(), kind.into());
    }
    for key in [
        "inputTokenLimit",
        "outputTokenLimit",
        "context_length",
        "max_completion_tokens",
    ] {
        if let Some(n) = wire.get(key).and_then(Value::as_i64).filter(|n| *n != 0) {
            raw.insert(key.into(), n.into());
        }
    }
    for key in ["thinking", "native_capabilities"] {
        if let Some(value) = wire.get(key).filter(|v| !v.is_null()) {
            raw.insert(key.into(), go_zero_nulls(value));
        }
    }
    if wire.get("user_defined") == Some(&Value::Bool(true)) {
        raw.insert("user_defined".into(), true.into());
    }
    ModelInfo::from_raw(raw).ok()
}

/// Go decodes a JSON null as the zero value: a null field of `ThinkingSupport` or
/// `NativeCapabilities` stays unset, and a null `levels` element (their only list, of
/// strings) is "". Rust's decoder rejects nulls, so drop or zero them first.
fn go_zero_nulls(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), go_zero_nulls(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| match v {
                    Value::Null => Value::String(String::new()),
                    v => go_zero_nulls(v),
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn attr<'a>(c: &'a Credential, key: &str) -> &'a str {
    c.attributes.get(key).map_or("", |v| v.trim())
}

/// `isConfiguredModelRoutingAuth`: API keys, or config-sourced OpenAI-compatible entries.
fn configured_routing(c: &Credential) -> bool {
    dynamic::is_api_key(c) || (matches!(c.source, Source::Config { .. }) && !attr(c, "compat_name").is_empty())
}

/// One configured model as Go compiles it (`addConfiguredModelCapability` inputs).
struct Configured {
    name: String,
    alias: String,
    thinking: Option<ThinkingSupport>,
    is_compat: bool,
    /// Codex only: `SupportConfigurationUpdate` overrides the static definition.
    support_configuration_update: Option<bool>,
}

/// `compileAPIKeyModelCapabilitiesForAuth`: the credential's config entry models, with
/// Go's model type per provider.
fn entry_models(cfg: &Config, c: &Credential) -> Option<(Vec<Configured>, &'static str)> {
    let provider = c.provider.trim().go_lower();
    let (family, kind) = match provider.as_str() {
        "gemini" => ("gemini", "gemini"),
        "gemini-interactions" => ("interactions", "interactions"),
        "claude" => ("claude", "claude"),
        "codex" => ("codex", "codex"),
        "xai" => ("xai", "xai"),
        "vertex" => ("vertex", "gemini"),
        "meta" => ("meta", "meta"),
        _ => return Some((compat_models(cfg, c)?, "openai-compatibility")),
    };
    let entry = credentials::resolve_api_key_entry(cfg, family, c)?;
    let models = entry
        .models
        .iter()
        .filter_map(|m| serde_json::from_value::<ConfigModel>(m.clone()).ok())
        .map(|m| Configured {
            name: m.name,
            alias: m.alias,
            thinking: m.thinking,
            // VertexCompatModel has no is-compat (Go's GetIsCompat is absent).
            is_compat: m.is_compat && family != "vertex",
            support_configuration_update: (family == "codex").then_some(m.support_configuration_update),
        })
        .collect();
    Some((models, kind))
}

/// `resolveOpenAICompatConfigForAuth` then `compileOpenAICompatibleModelCapabilities`:
/// the entry at `config_index` among entries with a base URL when enabled, else the
/// first enabled entry named by `compat_name`, `provider_key` or the provider. Models
/// without configured thinking get low/medium/high unless they are image models.
fn compat_models(cfg: &Config, c: &Credential) -> Option<Vec<Configured>> {
    let text = |v: Option<&serde_yaml_ng::Value>| match v {
        Some(serde_yaml_ng::Value::String(s)) => s.trim().to_owned(),
        Some(serde_yaml_ng::Value::Number(n)) => n.to_string(),
        Some(serde_yaml_ng::Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    };
    let entries: Vec<&serde_yaml_ng::Value> = cfg
        .document
        .get("api-keys")
        .and_then(|k| k.get("openai-compatibility"))
        .and_then(serde_yaml_ng::Value::as_sequence)
        .into_iter()
        .flatten()
        .filter(|g| !text(g.get("base-url")).is_empty())
        .collect();
    let disabled = |g: &serde_yaml_ng::Value| g.get("disabled").and_then(serde_yaml_ng::Value::as_bool) == Some(true);
    let by_index = matches!(c.source, Source::Config { .. })
        .then(|| attr(c, "config_index").parse::<usize>().ok())
        .flatten()
        .and_then(|i| entries.get(i).copied())
        .filter(|g| !disabled(g));
    let entry = by_index.or_else(|| {
        let candidates = [attr(c, "compat_name"), attr(c, "provider_key"), c.provider.trim()];
        entries.iter().copied().find(|g| {
            let name = text(g.get("name"));
            !disabled(g) && candidates.iter().any(|cand| !cand.is_empty() && cand.go_eq_fold(&name))
        })
    })?;
    let models = entry
        .get("models")
        .and_then(serde_yaml_ng::Value::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|m| serde_yaml_ng::from_value::<ConfigModel>(m.clone()).ok())
        .map(|m| Configured {
            thinking: m.thinking.or_else(|| {
                (!m.image).then(|| ThinkingSupport {
                    levels: vec!["low".into(), "medium".into(), "high".into()],
                    ..ThinkingSupport::default()
                })
            }),
            name: m.name,
            alias: m.alias,
            is_compat: m.is_compat,
            support_configuration_update: None,
        })
        .collect();
    Some(models)
}

/// `modelAliasLookupCandidates`: the model, then its suffix-free base when different.
fn candidates(model: &str) -> Vec<String> {
    let model = model.trim();
    if model.is_empty() {
        return Vec::new();
    }
    let base = parse_suffix(model).model_name;
    let base = if base.is_empty() { model.to_owned() } else { base };
    if base == model {
        vec![model.to_owned()]
    } else {
        vec![model.to_owned(), base]
    }
}

/// `configuredUpstreamFallbackMatches`: a suffix-free configured name matches the
/// selection's base.
fn fallback_matches(configured: &str, selected: &str) -> bool {
    let configured = parse_suffix(configured.trim());
    !configured.has_suffix
        && configured
            .model_name
            .trim()
            .go_eq_fold(parse_suffix(selected.trim()).model_name.trim())
}

/// `lookupAPIKeyModelCapability` over the routes `addConfiguredModelCapability` builds:
/// each model is keyed by its alias and name candidates (lowercased); a key keeps one
/// route per upstream name, first model first.
fn configured(cfg: &Config, c: &Credential, route_model: &str, upstream_model: &str) -> Option<ModelInfo> {
    if !configured_routing(c) {
        return None;
    }
    let (models, kind) = entry_models(cfg, c)?;
    // (key, upstream name, model index) in insertion order.
    let mut routes: Vec<(String, String, usize)> = Vec::new();
    for (index, m) in models.iter().enumerate() {
        let (mut name, mut alias) = (m.name.trim(), m.alias.trim());
        if name.is_empty() {
            name = alias;
        }
        if alias.is_empty() {
            alias = name;
        }
        if name.is_empty() {
            continue;
        }
        let mut seen: Vec<String> = Vec::new();
        for candidate in [alias, name].into_iter().flat_map(candidates) {
            let key = candidate.trim().go_lower();
            if key.is_empty() || seen.contains(&key) {
                continue;
            }
            seen.push(key.clone());
            if !routes.iter().any(|(k, up, _)| *k == key && up.go_eq_fold(name)) {
                routes.push((key, name.to_owned(), index));
            }
        }
    }
    let requested = dynamic::strip_prefix(route_model.trim(), c);
    let matched: Vec<(&str, usize)> = candidates(requested)
        .iter()
        .flat_map(|candidate| {
            let key = candidate.trim().go_lower();
            routes
                .iter()
                .filter(move |(k, ..)| *k == key)
                .map(|(_, up, i)| (up.as_str(), *i))
        })
        .collect();
    let selected = upstream_model.trim();
    let (name, index) = matched
        .iter()
        .find(|(up, _)| up.trim().go_eq_fold(selected))
        .or_else(|| matched.iter().find(|(up, _)| fallback_matches(up, selected)))?;
    let m = &models[*index];
    let mut info = resolve_model_info(name, kind, m.thinking.clone());
    set_flag(&mut info, "is_compat", m.is_compat);
    if let Some(support) = m.support_configuration_update {
        set_flag(&mut info, "support_configuration_update", support);
    }
    Some(info)
}

/// `lookupUnlistedCodexAPIKeyModelCapability`: every upstream model of a Codex API key
/// whose entry still matches its key and base URL binds; a configured name (exact or
/// suffix-free base) lends its thinking, configuration-update support and is-compat.
fn unlisted_codex(cfg: &Config, c: &Credential, upstream_model: &str) -> Option<ModelInfo> {
    let upstream = upstream_model.trim();
    if !dynamic::is_api_key(c) || !c.provider.trim().go_eq_fold("codex") || upstream.is_empty() {
        return None;
    }
    let entry = credentials::resolve_api_key_entry(cfg, "codex", c)?;
    let (key, base) = (attr(c, "api_key"), attr(c, "base_url"));
    let entry_base = entry.base_url.trim();
    if (key.is_empty() && base.is_empty())
        || !key.go_eq_fold(entry.api_key.trim())
        || (!entry_base.is_empty() && !base.go_eq_fold(entry_base))
    {
        return None;
    }
    let configured = entry
        .models
        .iter()
        .filter_map(|m| serde_json::from_value::<ConfigModel>(m.clone()).ok())
        .find(|m| m.name.trim().go_eq_fold(upstream) || fallback_matches(&m.name, upstream));
    let mut info = resolve_model_info(
        upstream_model,
        "codex",
        configured.as_ref().and_then(|m| m.thinking.clone()),
    );
    let (support, compat) = configured.map_or((false, false), |m| (m.support_configuration_update, m.is_compat));
    set_flag(&mut info, "support_configuration_update", support);
    set_flag(&mut info, "is_compat", compat);
    Some(info)
}

/// `lookupCodexOAuthModelCapability`: the plan catalog entry for the selection's
/// suffix-free model (`plan_type` plus, team/business/go, free; otherwise pro).
fn codex_oauth(c: &Credential, upstream_model: &str) -> Option<ModelInfo> {
    if dynamic::auth_kind(c) != Some("oauth") || !c.provider.trim().go_eq_fold("codex") {
        return None;
    }
    let channel = match attr(c, "plan_type").go_lower().as_str() {
        "plus" => "codex-plus",
        "team" | "business" | "go" => "codex-team",
        "free" => "codex-free",
        _ => "codex-pro",
    };
    let selected = parse_suffix(upstream_model.trim()).model_name;
    let selected = selected.trim();
    pinned()
        .channel(channel)
        .iter()
        .find(|m| m.id.go_eq_fold(selected))
        .cloned()
}

/// `modelconfig.ResolveModelInfo`: the static definition of the suffix-free name (Go
/// `LookupStaticModelInfo`), renamed to the configured name, typed for the provider,
/// configured thinking normalized, never user-defined.
fn resolve_model_info(name: &str, kind: &str, thinking: Option<ThinkingSupport>) -> ModelInfo {
    let name = name.trim();
    let base = parse_suffix(name).model_name;
    let mut info = pinned().lookup(base.trim()).cloned().unwrap_or_else(|| ModelInfo {
        id: String::new(),
        kind: String::new(),
        thinking: None,
        raw: Map::new(),
    });
    info.id = name.to_owned();
    info.raw.insert("id".into(), name.into());
    info.kind = kind.trim().to_owned();
    info.raw.insert("type".into(), info.kind.clone().into());
    if let Some(support) = thinking {
        let support = dynamic::normalize_thinking(support);
        info.raw.insert("thinking".into(), dynamic::thinking_value(&support));
        info.thinking = Some(support);
    }
    info.raw.remove("user_defined");
    info
}

/// Internal boolean metadata (`json:"-"` in Go) kept in `raw` only when true.
fn set_flag(info: &mut ModelInfo, key: &str, value: bool) {
    if value {
        info.raw.insert(key.into(), Value::Bool(true));
    } else {
        info.raw.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Go goldens (tests/reference/server/main.go `resolvedModels`, run through
    /// `GoldenResolvedModelInfo` in auth_export.go.overlay): Go's real
    /// `attachResolvedExecutionModelInfo` on config-synthesized auths, mutated
    /// `config_index` values and file-style auths.
    #[test]
    fn resolved_models_match_go() {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/server_go.json")).unwrap();
        let cfg = Config::parse(fixture["resolved_config"].as_str().unwrap()).unwrap();
        let synthesized = credentials::from_config(&cfg);
        let cases = fixture["resolved"].as_array().unwrap();
        assert_eq!(cases.len(), 32);
        let (mut bound, mut codex_oauth) = (0, 0);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let id = case["auth_id"].as_str().unwrap();
            let attributes: std::collections::BTreeMap<String, String> = case["attributes"]
                .as_object()
                .map(|a| {
                    a.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                        .collect()
                })
                .unwrap_or_default();
            let c = if case["synthetic"].as_bool().unwrap() {
                // Go and Rust synthesize the same IDs; the case's config_index mutation
                // is carried over from Go's attributes.
                let mut c = synthesized
                    .iter()
                    .find(|c| c.id == id)
                    .unwrap_or_else(|| panic!("{name}: no synthesized credential {id}"))
                    .clone();
                c.attributes
                    .insert("config_index".into(), attributes["config_index"].clone());
                c
            } else {
                let meta = case["metadata"].as_object().cloned().unwrap_or_default();
                let path = format!("/auth/{id}");
                let mut c = Credential::from_file(Path::new("/auth"), Path::new(&path), meta).unwrap();
                c.attributes = attributes;
                c
            };
            let restore = case["restore"].as_bool().unwrap();
            let got = resolve_attempt(
                &cfg,
                &c,
                case["route"].as_str().unwrap(),
                case["upstream"].as_str().unwrap(),
                restore.then(|| case["req_model"].as_str().unwrap()),
            );
            let source = match got.as_ref().map(|r| r.source) {
                None => "",
                Some(ResolvedSource::ApiKey) => "api_key",
                Some(ResolvedSource::CodexOAuth) => "codex_oauth",
                Some(ResolvedSource::Home) => "home",
            };
            assert_eq!(source, case["source"].as_str().unwrap(), "{name}: source");
            let Some(got) = got else { continue };
            bound += 1;
            codex_oauth += usize::from(got.source == ResolvedSource::CodexOAuth);
            // Go's JSON drops internal fields; compare everything else, ignoring zero
            // values Go emits for an unknown static model.
            let visible = |m: &Map<String, Value>| -> Map<String, Value> {
                m.iter()
                    .filter(|(k, v)| {
                        !matches!(
                            k.as_str(),
                            "support_configuration_update" | "is_compat" | "user_defined" | "native_capabilities"
                        ) && !matches!(v, Value::Null)
                            && *v != &Value::from("")
                            && *v != &Value::from(0)
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            };
            assert_eq!(
                Value::Object(visible(&got.info.raw)),
                Value::Object(visible(case["info"].as_object().unwrap())),
                "{name}: info"
            );
            assert_eq!(got.info.id, case["info"]["id"].as_str().unwrap(), "{name}: id");
            assert_eq!(got.info.kind, case["info"]["type"].as_str().unwrap(), "{name}: type");
            assert_eq!(
                got.is_compat(),
                case["is_compat"].as_bool().unwrap(),
                "{name}: is_compat"
            );
            let caps = cpa_common::thinking::ModelCaps::from(&got.info);
            assert_eq!(
                caps.support_configuration_update,
                case["support_configuration_update"].as_bool().unwrap(),
                "{name}: support_configuration_update"
            );
            assert_eq!(
                caps.user_defined,
                case["user_defined"].as_bool().unwrap(),
                "{name}: user_defined"
            );
            assert_eq!(
                got.info.thinking.as_ref().map(dynamic::thinking_value),
                case["info"].get("thinking").cloned(),
                "{name}: typed thinking"
            );
        }
        assert_eq!((bound, codex_oauth), (25, 4));
    }
}
