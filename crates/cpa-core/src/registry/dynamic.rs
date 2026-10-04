//! Dynamic model registry: which credential serves which public model ID, derived from
//! the credential set and config (sdk/cliproxy/service_models.go,
//! internal/registry/model_registry.go). The server builds it on demand and publishes
//! it through [`super::install_overlay`].
//!
//! The static catalog is `cpa_core::registry::pinned()`. Each enabled credential
//! registers its provider's channel (or its configured `models`), minus exclusions, plus
//! OAuth aliases and forks, plus `prefix/` copies. Selection admits a credential only
//! when it registered the route model (M4-0028), and route-time provider lookup uses
//! the registered providers, so a model nobody serves is Go's 400 `unknown provider`.
//!
//! Execution-model resolution (aliases back to upstream names, force mapping) lives
//! here too, so listing and routing read one definition.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Map, Value};

use super::{ModelInfo, ThinkingSupport, pinned};
use crate::config::Config;
use crate::credential::{Credential, Source};

/// Go `responsesWebSearchProviderPathSupport`.
fn provider_web_search_path(provider: &str) -> Option<bool> {
    match provider.trim().to_lowercase().as_str() {
        "codex" | "xai" | "claude" | "antigravity" => Some(true),
        "openai"
        | "openai-compatibility"
        | "gemini"
        | "aistudio"
        | "vertex"
        | "kimi"
        | "kimi-ai"
        | "kimi.ai"
        | "kimi.com"
        | "interactions"
        | "gemini-interactions" => Some(false),
        p if p.starts_with("openai-compatible-") => Some(false),
        _ => None,
    }
}

/// Go `canonicalModelKey`: the model without its terminal `(thinking)` suffix.
pub fn canonical_model(model: &str) -> &str {
    let model = model.trim();
    if model.ends_with(')') {
        model
            .rsplit_once('(')
            .map(|(base, _)| base.trim())
            .filter(|base| !base.is_empty())
            .unwrap_or(model)
    } else {
        model
    }
}

/// One model definition (Go `registry.ModelInfo`), typed for rendering.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Spec {
    pub id: String,
    #[serde(skip)]
    pub metadata_model_id: String,
    pub object: String,
    pub created: i64,
    pub owned_by: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub display_name: String,
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(rename = "inputTokenLimit")]
    pub input_token_limit: i64,
    #[serde(rename = "outputTokenLimit")]
    pub output_token_limit: i64,
    #[serde(rename = "supportedGenerationMethods")]
    pub supported_generation_methods: Vec<String>,
    pub context_length: i64,
    #[serde(skip)]
    pub max_context_length: i64,
    pub max_completion_tokens: i64,
    pub supported_parameters: Vec<String>,
    #[serde(rename = "supportedInputModalities")]
    pub supported_input_modalities: Vec<String>,
    #[serde(rename = "supportedOutputModalities")]
    pub supported_output_modalities: Vec<String>,
    pub thinking: Option<ThinkingSupport>,
    pub support_configuration_update: bool,
    /// Go `SupportsWebSearch` (Antigravity models whose upstream lists web search).
    pub supports_web_search: bool,
    #[serde(skip)]
    pub is_compat: bool,
    #[serde(skip)]
    pub user_defined: bool,
    /// Go `ExplicitThinking`: a config model that set `thinking` (the Codex client
    /// catalog lets it narrow the advertised reasoning levels).
    #[serde(skip)]
    pub explicit_thinking: bool,
    /// Go `ExplicitInputModalities`: an OpenAI-compatible model that set
    /// `input-modalities`.
    #[serde(skip)]
    pub explicit_input_modalities: bool,
    /// Go `NativeCapabilities.WebSearch` from the static catalog (`native_capabilities`),
    /// `None` when unknown.
    #[serde(skip)]
    pub native_web_search: Option<bool>,
}

impl Spec {
    fn from_static(info: &ModelInfo) -> Self {
        let mut spec = serde_json::from_value(Value::Object(info.raw.clone())).unwrap_or_else(|_| Self {
            id: info.id.clone(),
            kind: info.kind.clone(),
            ..Self::default()
        });
        spec.native_web_search = native_web_search(info);
        spec
    }

    /// The shared-contract view, for translators and executors.
    pub fn to_info(&self) -> ModelInfo {
        let mut raw = Map::new();
        raw.insert("id".into(), self.id.clone().into());
        raw.insert("object".into(), "model".into());
        raw.insert("created".into(), self.created.into());
        raw.insert("owned_by".into(), self.owned_by.clone().into());
        raw.insert("type".into(), self.kind.clone().into());
        let mut put = |k: &str, v: Value| {
            let empty = match &v {
                Value::String(s) => s.is_empty(),
                Value::Number(n) => n.as_i64() == Some(0),
                Value::Array(a) => a.is_empty(),
                Value::Bool(b) => !b,
                _ => false,
            };
            if !empty {
                raw.insert(k.into(), v);
            }
        };
        put("display_name", self.display_name.clone().into());
        put("name", self.name.clone().into());
        put("version", self.version.clone().into());
        put("description", self.description.clone().into());
        put("inputTokenLimit", self.input_token_limit.into());
        put("outputTokenLimit", self.output_token_limit.into());
        put(
            "supportedGenerationMethods",
            self.supported_generation_methods.clone().into(),
        );
        put("context_length", self.context_length.into());
        put("max_context_length", self.max_context_length.into());
        put("max_completion_tokens", self.max_completion_tokens.into());
        put("supported_parameters", self.supported_parameters.clone().into());
        put(
            "supportedInputModalities",
            self.supported_input_modalities.clone().into(),
        );
        put(
            "supportedOutputModalities",
            self.supported_output_modalities.clone().into(),
        );
        put("support_configuration_update", self.support_configuration_update.into());
        put("supports_web_search", self.supports_web_search.into());
        put("is_compat", self.is_compat.into());
        // Config `models[]` entries: thinking passes through unvalidated (thinking.IsUserDefinedModel).
        put("user_defined", self.user_defined.into());
        if let Some(t) = &self.thinking {
            raw.insert("thinking".into(), thinking_value(t));
        }
        ModelInfo::from_raw(raw).expect("spec renders a valid model")
    }
}

/// Go's JSON for a `ThinkingSupport` (`omitempty` on every field).
pub fn thinking_value(t: &ThinkingSupport) -> Value {
    let mut m = Map::new();
    for (k, v) in [("min", t.min), ("max", t.max)] {
        if v != 0 {
            m.insert(k.into(), v.into());
        }
    }
    if t.zero_allowed {
        m.insert("zero_allowed".into(), true.into());
    }
    if t.dynamic_allowed {
        m.insert("dynamic_allowed".into(), true.into());
    }
    if !t.levels.is_empty() {
        m.insert("levels".into(), t.levels.clone().into());
    }
    Value::Object(m)
}

/// Go `Auth.AuthKind()` (sdk/cliproxy/auth/classification.go): a recognised
/// `auth_kind` attribute, then a recognised `auth_kind` metadata string, then a
/// non-empty `api_key` attribute (`apikey`), then OAuth token metadata (`oauth`).
/// Unrecognised kinds fall through to the next source.
pub fn auth_kind(c: &Credential) -> Option<&'static str> {
    let normalize = |s: &str| match s.trim().to_lowercase().as_str() {
        "apikey" | "api_key" | "api-key" => Some("apikey"),
        "oauth" | "oauth2" => Some("oauth"),
        _ => None,
    };
    if let Some(kind) = c.attributes.get("auth_kind").and_then(|s| normalize(s)) {
        return Some(kind);
    }
    if let Some(kind) = c.str("auth_kind").and_then(normalize) {
        return Some(kind);
    }
    if c.attributes.get("api_key").is_some_and(|k| !k.trim().is_empty()) {
        return Some("apikey");
    }
    let oauth = [
        "access_token",
        "refresh_token",
        "id_token",
        "email",
        "token_type",
        "expires_at",
        "expired",
    ]
    .iter()
    .any(|k| c.str(k).is_some_and(|v| !v.trim().is_empty()))
        || c.metadata
            .get("token")
            .and_then(Value::as_object)
            .is_some_and(|t| !t.is_empty());
    oauth.then_some("oauth")
}

/// Go `Auth.AccountInfo()`: the account kind and value request logs print. OAuth
/// reports the trimmed metadata `email`, an API key the trimmed `api_key` attribute;
/// an unclassified credential reports nothing.
pub fn account_info(c: &Credential) -> (&'static str, String) {
    match auth_kind(c) {
        Some("oauth") => (
            "oauth",
            c.metadata
                .get("email")
                .and_then(Value::as_str)
                .map(|e| e.trim().to_owned())
                .unwrap_or_default(),
        ),
        Some("apikey") => (
            "api_key",
            c.attributes
                .get("api_key")
                .map(|k| k.trim().to_owned())
                .unwrap_or_default(),
        ),
        _ => ("", String::new()),
    }
}

/// `auth.AuthKind() == AuthKindAPIKey`.
pub fn is_api_key(c: &Credential) -> bool {
    auth_kind(c) == Some("apikey")
}

fn openai_compat(c: &Credential) -> bool {
    ["compat_name", "provider_key"]
        .iter()
        .any(|k| c.attributes.get(*k).is_some_and(|v| !v.trim().is_empty()))
        || c.provider.eq_ignore_ascii_case("openai-compatibility")
}

/// The provider key a credential registers and is scheduled under.
pub fn provider_key(c: &Credential) -> String {
    if openai_compat(c)
        && let Some(key) = c
            .attributes
            .get("provider_key")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
    {
        return key.to_lowercase();
    }
    c.provider.trim().to_lowercase()
}

/// The authoritative `prefix` (attributes first, then file metadata).
pub fn credential_prefix(c: &Credential) -> &str {
    c.attributes
        .get("prefix")
        .map(String::as_str)
        .or_else(|| c.str("prefix"))
        .unwrap_or("")
        .trim()
}

/// One configured model entry (`models[]` of an API-key or OpenAI-compatible entry).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ConfigModel {
    pub name: String,
    pub alias: String,
    pub display_name: String,
    pub max_context_length: i64,
    pub force_mapping: bool,
    pub is_compat: bool,
    pub support_configuration_update: bool,
    pub image: bool,
    pub thinking: Option<ThinkingSupport>,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
}

/// Configured models carried on a config-backed credential: full entries under
/// metadata `models` when synthesis provides them, otherwise `model_aliases` pairs.
pub fn config_models(c: &Credential) -> Vec<ConfigModel> {
    if !matches!(c.source, Source::Config { .. }) {
        return Vec::new();
    }
    let raw = c.metadata.get("models").or_else(|| c.metadata.get("model_aliases"));
    let Some(Value::Array(items)) = raw else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|v| {
            let mut v = v.clone();
            // Accept snake_case spellings from JSON metadata too.
            if let Value::Object(m) = &mut v {
                for key in [
                    "display_name",
                    "max_context_length",
                    "force_mapping",
                    "is_compat",
                    "support_configuration_update",
                    "input_modalities",
                    "output_modalities",
                ] {
                    if let Some(x) = m.remove(key) {
                        m.entry(key.replace('_', "-")).or_insert(x);
                    }
                }
            }
            serde_json::from_value(v).ok()
        })
        .collect()
}

/// One OAuth alias (`oauth.model-alias.<channel>[]` or attributes `model_aliases`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct OAuthAlias {
    pub name: String,
    pub alias: String,
    pub fork: bool,
    pub display_name: String,
    pub force_mapping: bool,
}

/// Go `SanitizeOAuthModelAlias` for one channel.
fn sanitize_aliases(entries: Vec<OAuthAlias>) -> Vec<OAuthAlias> {
    let mut seen = std::collections::HashSet::new();
    entries
        .into_iter()
        .filter_map(|mut e| {
            e.name = e.name.trim().to_owned();
            e.alias = e.alias.trim().to_owned();
            e.display_name = e.display_name.trim().to_owned();
            (!e.name.is_empty()
                && !e.alias.is_empty()
                && !e.name.eq_ignore_ascii_case(&e.alias)
                && seen.insert(e.alias.to_lowercase()))
            .then_some(e)
        })
        .collect()
}

/// `oauth.model-alias`, sanitized, by lowercase channel.
pub fn global_aliases(cfg: &Config) -> HashMap<String, Vec<OAuthAlias>> {
    let mut out = HashMap::new();
    let Some(map) = cfg
        .document
        .get("oauth")
        .and_then(|o| o.get("model-alias"))
        .and_then(serde_yaml_ng::Value::as_mapping)
    else {
        return out;
    };
    for (channel, list) in map {
        let channel = channel.as_str().unwrap_or_default().trim().to_lowercase();
        let entries: Vec<OAuthAlias> = serde_yaml_ng::from_value(list.clone()).unwrap_or_default();
        let clean = sanitize_aliases(entries);
        if !channel.is_empty() && !clean.is_empty() {
            out.insert(channel, clean);
        }
    }
    out
}

fn attribute_aliases(c: &Credential) -> Vec<OAuthAlias> {
    c.attributes
        .get("model_aliases")
        .and_then(|raw| serde_json::from_str::<Vec<Value>>(raw).ok())
        .map(|items| {
            sanitize_aliases(
                items
                    .into_iter()
                    .filter_map(|v| {
                        let mut v = v;
                        if let Value::Object(m) = &mut v {
                            for key in ["display_name", "force_mapping"] {
                                if let Some(x) = m.remove(key) {
                                    m.entry(key.replace('_', "-")).or_insert(x);
                                }
                            }
                        }
                        serde_json::from_value(v).ok()
                    })
                    .collect(),
            )
        })
        .unwrap_or_default()
}

/// `OAuthModelAliasChannel`: API keys and Gemini keys have none.
fn alias_channel(c: &Credential) -> Option<String> {
    if is_api_key(c) {
        return None;
    }
    match c.provider.trim().to_lowercase().as_str() {
        "gemini" | "" => None,
        other => Some(other.to_owned()),
    }
}

fn oauth_aliases_for(cfg_aliases: &HashMap<String, Vec<OAuthAlias>>, c: &Credential) -> Vec<OAuthAlias> {
    let Some(channel) = alias_channel(c) else {
        return Vec::new();
    };
    let mut out = attribute_aliases(c);
    let mut seen: std::collections::HashSet<String> = out.iter().map(|a| a.alias.to_lowercase()).collect();
    for entry in cfg_aliases.get(&channel).into_iter().flatten() {
        if seen.insert(entry.alias.to_lowercase()) {
            out.push(entry.clone());
        }
    }
    out
}

/// Go `matchWildcard` (lowercased inputs).
pub fn wildcard(pattern: &str, value: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    if !pattern.contains('*') {
        return pattern == value;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut value = value;
    let first = parts[0];
    if !first.is_empty() {
        let Some(rest) = value.strip_prefix(first) else {
            return false;
        };
        value = rest;
    }
    let last = parts[parts.len() - 1];
    if !last.is_empty() {
        let Some(rest) = value.strip_suffix(last) else {
            return false;
        };
        value = rest;
    }
    for segment in &parts[1..parts.len() - 1] {
        if segment.is_empty() {
            continue;
        }
        match value.find(segment) {
            Some(i) => value = &value[i + segment.len()..],
            None => return false,
        }
    }
    true
}

fn excluded_for(cfg: &Config, c: &Credential) -> Vec<String> {
    if let Some(list) = c.attributes.get("excluded_models").filter(|v| !v.trim().is_empty()) {
        return list.split(',').map(str::to_owned).collect();
    }
    if is_api_key(c) {
        return Vec::new();
    }
    crate::config::credentials::oauth_excluded(cfg)
        .remove(&c.provider.trim().to_lowercase())
        .unwrap_or_default()
}

fn apply_excluded(models: Vec<Spec>, excluded: &[String]) -> Vec<Spec> {
    let patterns: Vec<String> = excluded
        .iter()
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    if patterns.is_empty() {
        return models;
    }
    models
        .into_iter()
        .filter(|m| {
            let id = m.id.trim().to_lowercase();
            !patterns.iter().any(|p| wildcard(p, &id))
        })
        .collect()
}

/// `native_capabilities.web_search` of a static catalog model.
fn native_web_search(info: &ModelInfo) -> Option<bool> {
    info.raw
        .get("native_capabilities")
        .and_then(|c| c.get("web_search"))
        .and_then(Value::as_bool)
}

/// Go `normalizeCompatConfigModalities`: lowercase, trimmed, unique, in order.
fn normalize_modalities(raw: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    raw.iter()
        .map(|m| m.trim().to_lowercase())
        .filter(|m| !m.is_empty() && seen.insert(m.clone()))
        .collect()
}

fn channel(name: &str) -> Vec<Spec> {
    pinned().channel(name).iter().map(Spec::from_static).collect()
}

/// The Images API models every Codex catalog carries (`codexBuiltinImage*ModelInfo`).
const CODEX_BUILTIN_IMAGES: [(&str, &str); 5] = [
    ("gpt-image-1.5", "GPT Image 1.5"),
    ("gpt-image-2", "GPT Image 2"),
    ("gpt-image-2.5-flare", "GPT Image 2.5 Flare"),
    ("gpt-image-2.5-sunburst", "GPT Image 2.5 Sunburst"),
    ("gpt-image-2.5", "GPT Image 2.5"),
];

/// Go `GetCodex*Models`: a plan catalog `WithCodexBuiltins` (`upsertModelInfos`), where
/// the built-ins replace same-ID models (case-insensitive) and come last.
fn codex_channel(name: &str) -> Vec<Spec> {
    let builtins = CODEX_BUILTIN_IMAGES.map(|(id, display_name)| Spec {
        id: id.into(),
        object: "model".into(),
        // 2024-01-01.
        created: 1_704_067_200,
        owned_by: "openai".into(),
        kind: "openai".into(),
        display_name: display_name.into(),
        version: id.into(),
        ..Spec::default()
    });
    let mut models: Vec<Spec> = channel(name)
        .into_iter()
        .filter(|m| {
            let id = m.id.trim().to_lowercase();
            !id.is_empty() && !builtins.iter().any(|b| b.id == id)
        })
        .collect();
    models.extend(builtins);
    models
}

/// Go `WithXAIBuiltins` (`upsertModelInfos`): the hard-coded image and video models,
/// appended after the channel's models and replacing any with the same ID.
fn with_xai_builtins(models: Vec<Spec>) -> Vec<Spec> {
    const BUILTINS: [(&str, i64, &str, &str); 6] = [
        (
            "grok-imagine-image",
            1735689600,
            "Grok Imagine Image",
            "xAI Grok image generation model.",
        ),
        (
            "grok-imagine-image-quality",
            1735689600,
            "Grok Imagine Image Quality",
            "xAI Grok higher-fidelity image generation model.",
        ),
        (
            "grok-imagine-image-2.0",
            1786060800,
            "Grok Imagine Image 2.0",
            "xAI Grok image generation model.",
        ),
        (
            "grok-imagine-video",
            1735689600,
            "Grok Imagine Video",
            "xAI Grok video generation model.",
        ),
        (
            "grok-imagine-video-1.5",
            1735689600,
            "Grok Imagine Video 1.5",
            "xAI Grok video generation model.",
        ),
        (
            "grok-imagine-video-1.5-preview",
            1735689600,
            "Grok Imagine Video 1.5 Preview",
            "Compatibility alias for the xAI Grok video generation model.",
        ),
    ];
    let mut out: Vec<Spec> = models
        .into_iter()
        .filter(|m| {
            let id = m.id.trim();
            !id.is_empty() && !BUILTINS.iter().any(|(b, ..)| b.eq_ignore_ascii_case(id))
        })
        .collect();
    out.extend(BUILTINS.iter().map(|(id, created, display, description)| Spec {
        id: (*id).into(),
        object: "model".into(),
        created: *created,
        owned_by: "xai".into(),
        kind: "xai".into(),
        display_name: (*display).into(),
        name: (*id).into(),
        description: (*description).into(),
        ..Spec::default()
    }));
    out
}

/// Go `buildConfigModels`; `metadata_channel` is the static channel whose native
/// capabilities a configured upstream name inherits.
fn build_config_models(
    models: &[ConfigModel],
    owned_by: &str,
    kind: &str,
    metadata_channel: &str,
    now: i64,
) -> Vec<Spec> {
    let mut seen = std::collections::HashSet::new();
    models
        .iter()
        .filter_map(|m| {
            let name = m.name.trim();
            let alias = if m.alias.trim().is_empty() {
                name
            } else {
                m.alias.trim()
            };
            if alias.is_empty() || !seen.insert(alias.to_lowercase()) {
                return None;
            }
            let display = [m.display_name.trim(), name, alias]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or(alias);
            let base = canonical_model(name);
            let thinking = m
                .thinking
                .clone()
                .map(normalize_thinking)
                .or_else(|| pinned().lookup(base).and_then(|i| i.thinking.clone()));
            Some(Spec {
                id: alias.to_owned(),
                metadata_model_id: if name.is_empty() {
                    alias.to_owned()
                } else {
                    name.to_owned()
                },
                object: "model".into(),
                created: now,
                owned_by: owned_by.into(),
                kind: kind.into(),
                display_name: display.to_owned(),
                context_length: m.max_context_length.max(0),
                max_context_length: m.max_context_length.max(0),
                thinking,
                is_compat: m.is_compat,
                support_configuration_update: m.support_configuration_update,
                user_defined: true,
                explicit_thinking: m.thinking.is_some(),
                native_web_search: pinned()
                    .channel(metadata_channel)
                    .iter()
                    .find(|i| i.id == name)
                    .and_then(native_web_search),
                ..Spec::default()
            })
        })
        .collect()
}

/// `NormalizeThinkingSupport`: lowercase unique levels; `none` and `auto` flags.
pub fn normalize_thinking(mut t: ThinkingSupport) -> ThinkingSupport {
    let mut seen = std::collections::HashSet::new();
    let levels = std::mem::take(&mut t.levels);
    for level in levels {
        let level = level.trim().to_lowercase();
        if level.is_empty() {
            continue;
        }
        match level.as_str() {
            "none" => t.zero_allowed = true,
            "auto" => t.dynamic_allowed = true,
            _ => {}
        }
        if seen.insert(level.clone()) {
            t.levels.push(level);
        }
    }
    t
}

fn compat_models(models: &[ConfigModel], owned_by: &str, now: i64) -> Vec<Spec> {
    models
        .iter()
        .filter_map(|m| {
            let name = m.name.trim();
            let alias = if m.alias.trim().is_empty() {
                name
            } else {
                m.alias.trim()
            };
            if alias.is_empty() {
                return None;
            }
            let display = [m.display_name.trim(), m.alias.trim(), alias]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or(alias);
            let thinking = match &m.thinking {
                Some(t) => Some(normalize_thinking(t.clone())),
                None if !m.image => Some(ThinkingSupport {
                    levels: vec!["low".into(), "medium".into(), "high".into()],
                    ..ThinkingSupport::default()
                }),
                None => None,
            };
            Some(Spec {
                id: alias.to_owned(),
                metadata_model_id: if name.is_empty() {
                    alias.to_owned()
                } else {
                    name.to_owned()
                },
                object: "model".into(),
                created: now,
                owned_by: owned_by.into(),
                kind: if m.image {
                    "openai-image".into()
                } else {
                    "openai-compatibility".into()
                },
                display_name: display.to_owned(),
                context_length: m.max_context_length.max(0),
                max_context_length: m.max_context_length.max(0),
                thinking,
                is_compat: m.is_compat,
                explicit_thinking: m.thinking.is_some(),
                explicit_input_modalities: !m.input_modalities.is_empty(),
                supported_input_modalities: normalize_modalities(&m.input_modalities),
                supported_output_modalities: normalize_modalities(&m.output_modalities),
                ..Spec::default()
            })
        })
        .collect()
}

/// Go `applyOAuthModelAliasEntries`: aliases replace their source model unless an
/// entry is a fork, which keeps both.
fn apply_aliases(aliases: &[OAuthAlias], models: Vec<Spec>) -> Vec<Spec> {
    let mut forward: HashMap<String, Vec<&OAuthAlias>> = HashMap::new();
    for a in aliases {
        forward.entry(a.name.to_lowercase()).or_default().push(a);
    }
    if forward.is_empty() {
        return models;
    }
    let mut out = Vec::with_capacity(models.len());
    let mut seen = std::collections::HashSet::new();
    for model in models {
        let id = model.id.trim().to_owned();
        if id.is_empty() {
            continue;
        }
        let key = id.to_lowercase();
        let Some(entries) = forward.get(&key) else {
            if seen.insert(key) {
                out.push(model);
            }
            continue;
        };
        let keep_original = entries.iter().any(|e| e.fork);
        if keep_original && seen.insert(key.clone()) {
            out.push(model.clone());
        }
        let mut added = false;
        for entry in entries {
            let mapped = entry.alias.trim();
            if mapped.is_empty() || mapped.eq_ignore_ascii_case(&id) || !seen.insert(mapped.to_lowercase()) {
                continue;
            }
            let mut clone = model.clone();
            clone.id = mapped.to_owned();
            if clone.metadata_model_id.is_empty() {
                clone.metadata_model_id = id.clone();
            }
            if !entry.display_name.is_empty() {
                clone.display_name = entry.display_name.clone();
            }
            if !clone.name.is_empty() {
                clone.name = rewrite_name(&clone.name, &id, mapped);
            }
            out.push(clone);
            added = true;
        }
        if !keep_original && !added && seen.insert(key) {
            out.push(model);
        }
    }
    out
}

fn rewrite_name(name: &str, old: &str, new: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() || old.eq_ignore_ascii_case(new) {
        return name.to_owned();
    }
    if trimmed.eq_ignore_ascii_case(old) {
        return new.to_owned();
    }
    if let Some(prefix) = trimmed.strip_suffix(old).filter(|p| p.ends_with('/')) {
        return format!("{prefix}{new}");
    }
    if trimmed == format!("models/{old}") {
        return format!("models/{new}");
    }
    name.to_owned()
}

/// Go `applyModelPrefixes`.
fn apply_prefix(models: Vec<Spec>, prefix: &str, force: bool) -> Vec<Spec> {
    if prefix.is_empty() {
        return models;
    }
    let mut out = Vec::with_capacity(models.len() * 2);
    let mut seen = std::collections::HashSet::new();
    for model in models {
        let base = model.id.trim().to_owned();
        if base.is_empty() {
            continue;
        }
        let mut clone = model.clone();
        if (!force || prefix == base) && seen.insert(base.clone()) {
            out.push(model);
        }
        clone.id = format!("{prefix}/{base}");
        if clone.metadata_model_id.is_empty() {
            clone.metadata_model_id = base;
        }
        if seen.insert(clone.id.clone()) {
            out.push(clone);
        }
    }
    out
}

/// The models one credential registers (Go `registerModelsForAuth`), or none.
pub fn models_for(cfg: &Config, aliases: &HashMap<String, Vec<OAuthAlias>>, c: &Credential, now: i64) -> Vec<Spec> {
    if c.disabled {
        return Vec::new();
    }
    let excluded = excluded_for(cfg, c);
    let configured = config_models(c);
    let api_key = is_api_key(c);
    let provider = c.provider.trim().to_lowercase();
    if openai_compat(c) {
        let owned_by = c.attributes.get("compat_name").cloned().unwrap_or_default();
        let models = compat_models(&configured, &owned_by, now);
        return apply_prefix(models, credential_prefix(c), cfg.routing.force_model_prefix);
    }
    let with_config = |channel_models: Vec<Spec>, owned_by: &str, kind: &str, metadata: &str| {
        if configured.is_empty() {
            channel_models
        } else {
            build_config_models(&configured, owned_by, kind, metadata, now)
        }
    };
    let models = match provider.as_str() {
        "gemini" | "gemini-interactions" => with_config(channel("gemini"), "google", "gemini", "gemini"),
        "vertex" => with_config(channel("vertex"), "google", "vertex", "vertex"),
        "aistudio" => channel("aistudio"),
        "antigravity" => channel("antigravity"),
        "claude" => with_config(channel("claude"), "anthropic", "claude", "claude"),
        "codex" if api_key => {
            if configured.is_empty() {
                let mut models = codex_channel("codex-pro");
                for m in &mut models {
                    m.support_configuration_update = false;
                }
                models
            } else {
                // Go's metadata channel "codex" is the Pro catalog.
                build_config_models(&configured, "openai", "openai", "codex-pro", now)
            }
        }
        "codex" => {
            let plan = c
                .attributes
                .get("plan_type")
                .map(|p| p.trim().to_lowercase())
                .unwrap_or_default();
            codex_channel(match plan.as_str() {
                "plus" => "codex-plus",
                "team" | "business" | "go" => "codex-team",
                "free" => "codex-free",
                _ => "codex-pro",
            })
        }
        "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com" => channel("kimi"),
        "xai" => with_config(with_xai_builtins(channel("xai")), "xai", "xai", "xai"),
        "meta" => with_config(channel("meta"), "meta", "meta", "meta"),
        // Go `registry.GetDevinModels()`: the live Devin catalog, never config models.
        "devin" => super::devin::models().iter().map(Spec::from_static).collect(),
        _ => Vec::new(),
    };
    let models = apply_excluded(models, &excluded);
    let models = apply_aliases(&oauth_aliases_for(aliases, c), models);
    // ponytail: oauth.settings max-context-length overrides are not applied yet.
    apply_prefix(models, credential_prefix(c), cfg.routing.force_model_prefix)
}

struct Registration {
    id: String,
    info: Arc<Spec>,
    /// Provider registration counts, in first-registration order.
    providers: Vec<(String, usize)>,
    by_provider: HashMap<String, Arc<Spec>>,
    /// Credentials that registered this model.
    clients: Vec<String>,
}

/// A credential's current state for one model, as Go projects it into the registry
/// (`ClientModelProjection`): quota cooling keeps a model listed, other suspensions
/// (auth failures, unsupported models, credential-wide quota) hide it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suspension {
    None,
    /// Suspended with reason `quota` (a 429 on this model).
    Quota,
    /// Suspended for any other reason; `quota_exceeded` when the state is also marked
    /// quota-exceeded (Cloudflare challenges).
    Other {
        quota_exceeded: bool,
    },
}

/// Go's listing rule over the registering credentials' states
/// (`modelRegistrationAvailability`, `GetAvailableModelsByProvider`): listed while any
/// credential is usable, or when every unusable one is only quota-cooling.
fn listable(states: impl Iterator<Item = Suspension>) -> bool {
    let (mut count, mut expired, mut cooling, mut other, mut quota_and_other) = (0i64, 0i64, 0i64, 0i64, 0i64);
    for state in states {
        count += 1;
        match state {
            Suspension::None => {}
            Suspension::Quota => {
                expired += 1;
                cooling += 1;
            }
            Suspension::Other { quota_exceeded } => {
                other += 1;
                if quota_exceeded {
                    expired += 1;
                    quota_and_other += 1;
                }
            }
        }
    }
    let effective = count - expired - other + quota_and_other;
    effective > 0 || (count > 0 && (expired > 0 || cooling > 0) && other == 0)
}

struct Client {
    provider: String,
    models: HashMap<String, Arc<Spec>>,
}

/// A snapshot of every credential's registrations.
#[derive(Default)]
pub struct Registry {
    models: Vec<Registration>,
    index: HashMap<String, usize>,
    clients: HashMap<String, Client>,
}

impl Registry {
    pub fn build(cfg: &Config, credentials: &[Arc<Credential>]) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let aliases = global_aliases(cfg);
        let mut registry = Self::default();
        for c in credentials {
            let models = models_for(cfg, &aliases, c, now);
            if !models.is_empty() {
                registry.register(&c.id, &provider_key(c), models);
            }
        }
        registry
    }

    /// Go `RegisterClient` for a client not yet registered: duplicate IDs keep the
    /// first definition. `provider` is the lowercased provider key.
    pub fn register(&mut self, client: &str, provider: &str, models: Vec<Spec>) {
        let mut infos = HashMap::new();
        for model in models {
            if model.id.is_empty() || infos.contains_key(&model.id) {
                continue;
            }
            let info = Arc::new(model);
            infos.insert(info.id.clone(), info.clone());
            match self.index.get(&info.id) {
                Some(&i) => {
                    let reg = &mut self.models[i];
                    // Go keeps the latest registration's info, with `SupportsWebSearch`
                    // set when any registering credential (of that provider) has it.
                    let with_search = |seen: bool| {
                        if !seen || info.supports_web_search {
                            return info.clone();
                        }
                        let mut spec = (*info).clone();
                        spec.supports_web_search = true;
                        Arc::new(spec)
                    };
                    reg.info = with_search(reg.info.supports_web_search);
                    match reg.providers.iter_mut().find(|(p, _)| p == provider) {
                        Some((_, n)) => *n += 1,
                        None => reg.providers.push((provider.to_owned(), 1)),
                    }
                    let seen = reg.by_provider.get(provider).is_some_and(|s| s.supports_web_search);
                    reg.by_provider.insert(provider.to_owned(), with_search(seen));
                    reg.clients.push(client.to_owned());
                }
                None => {
                    self.index.insert(info.id.clone(), self.models.len());
                    self.models.push(Registration {
                        id: info.id.clone(),
                        info: info.clone(),
                        providers: vec![(provider.to_owned(), 1)],
                        by_provider: HashMap::from([(provider.to_owned(), info)]),
                        clients: vec![client.to_owned()],
                    });
                }
            }
        }
        self.clients.insert(
            client.to_owned(),
            Client {
                provider: provider.to_owned(),
                models: infos,
            },
        );
    }

    /// Go `GetModelProviders`: most registrations first, then by name.
    pub fn model_providers(&self, model: &str) -> Vec<String> {
        let Some(&i) = self.index.get(model) else {
            return Vec::new();
        };
        let mut providers = self.models[i].providers.clone();
        providers.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        providers.into_iter().map(|(p, _)| p).collect()
    }

    /// Go `util.GetProviderName`: exact ID, then the lowercased ID.
    pub fn providers(&self, model: &str) -> Vec<String> {
        if model.is_empty() {
            return Vec::new();
        }
        let exact = self.model_providers(model);
        if exact.is_empty() && model.to_lowercase() != model {
            return self.model_providers(&model.to_lowercase());
        }
        exact
    }

    /// Go `ClientSupportsModel`: case-insensitive membership.
    pub fn client_supports(&self, client: &str, model: &str) -> bool {
        let model = model.trim();
        !model.is_empty()
            && self
                .clients
                .get(client)
                .is_some_and(|c| c.models.keys().any(|id| id.eq_ignore_ascii_case(model)))
    }

    /// Whether `client` registered anything (unregistered credentials never route).
    pub fn has_client(&self, client: &str) -> bool {
        self.clients.contains_key(client)
    }

    /// The model registered by one credential under `model`.
    pub fn client_model(&self, client: &str, model: &str) -> Option<Arc<Spec>> {
        let c = self.clients.get(client)?;
        c.models
            .get(model)
            .or_else(|| {
                c.models
                    .iter()
                    .find(|(id, _)| id.eq_ignore_ascii_case(model))
                    .map(|(_, m)| m)
            })
            .cloned()
    }

    pub fn client_provider(&self, client: &str) -> Option<&str> {
        self.clients.get(client).map(|c| c.provider.as_str())
    }

    /// Go `GetModelInfo(id, provider)`.
    pub fn info(&self, id: &str, provider: Option<&str>) -> Option<Arc<Spec>> {
        let reg = &self.models[*self.index.get(id)?];
        provider
            .and_then(|p| reg.by_provider.get(p))
            .or(Some(&reg.info))
            .cloned()
    }

    /// Go `GetResponsesWebSearchCapability`: native web search across every route that
    /// registered exactly `model`. A route known not to support it wins; any unknown
    /// route makes the answer unknown.
    pub fn responses_web_search(&self, model: &str) -> Option<bool> {
        let model = model.trim();
        let routes: Vec<(&str, Option<bool>)> = self
            .clients
            .values()
            .filter_map(|c| c.models.get(model).map(|m| (c.provider.as_str(), m.native_web_search)))
            .collect();
        if routes.is_empty() {
            return None;
        }
        let mut unknown = false;
        for (provider, native) in routes {
            if native == Some(false) {
                return Some(false);
            }
            match provider_web_search_path(provider) {
                None => unknown = true,
                Some(false) => return Some(false),
                Some(true) => unknown |= native.is_none(),
            }
        }
        (!unknown).then_some(true)
    }

    /// Registered models in first-registration order, ignoring cooldown state.
    pub fn available(&self) -> impl Iterator<Item = &Spec> {
        self.models.iter().map(|r| r.info.as_ref())
    }

    /// Registered models a client may list or `auto` may pick (Go
    /// `modelRegistrationAvailability`), given each credential's state per model.
    // ponytail: Go recomputes projections only when a result or refresh is recorded, so
    // a model can stay hidden after its cooldown lapses; this reads live state instead.
    pub fn available_with<'a>(
        &'a self,
        state: impl Fn(&str, &str) -> Suspension + 'a,
    ) -> impl Iterator<Item = &'a Spec> {
        self.models.iter().filter_map(move |r| {
            let states = r.clients.iter().map(|client| state(client, &r.id));
            listable(states).then_some(r.info.as_ref())
        })
    }

    /// Go `GetAvailableModelsByProvider`: the models `provider`'s credentials
    /// registered that are still listable counting only that provider's credentials,
    /// each with the first such credential's model info. Registration order (Go's is
    /// map order).
    pub fn available_by_provider(&self, provider: &str, state: impl Fn(&str, &str) -> Suspension) -> Vec<Arc<Spec>> {
        let provider = provider.trim().to_lowercase();
        if provider.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for r in &self.models {
            let mut clients = r
                .clients
                .iter()
                .filter_map(|id| Some((id, self.clients.get(id).filter(|c| c.provider == provider)?)))
                .peekable();
            let Some(info) = clients.peek().and_then(|(_, c)| c.models.get(&r.id)).cloned() else {
                continue;
            };
            if listable(clients.map(|(id, _)| state(id, &r.id))) {
                out.push(info);
            }
        }
        out
    }

    /// Go `ResolveAutoModel`: the newest available model.
    pub fn resolve_auto(&self, state: impl Fn(&str, &str) -> Suspension) -> Option<String> {
        let mut best: Option<&Spec> = None;
        for m in self.available_with(state) {
            if best.is_none_or(|b| m.created > b.created) {
                best = Some(m);
            }
        }
        best.map(|m| m.id.clone())
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.models.iter().map(|r| r.id.as_str())
    }
}

/// Result of resolving a route model for one credential (Go `OAuthModelAliasResult`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AliasResult {
    pub upstream: String,
    pub force_mapping: bool,
    pub original_alias: String,
}

fn suffix_of(model: &str) -> Option<&str> {
    let open = model.rfind('(')?;
    model.ends_with(')').then(|| &model[open + 1..model.len() - 1])
}

fn preserve_suffix(resolved: &str, requested: &str) -> String {
    let resolved = resolved.trim();
    if resolved.is_empty() || suffix_of(resolved).is_some() {
        return resolved.to_owned();
    }
    match suffix_of(requested.trim()) {
        Some(s) if !s.is_empty() => format!("{resolved}({s})"),
        _ => resolved.to_owned(),
    }
}

fn candidates(requested: &str) -> Vec<String> {
    let requested = requested.trim();
    if requested.is_empty() {
        return Vec::new();
    }
    let base = match suffix_of(requested) {
        Some(_) => &requested[..requested.rfind('(').unwrap()],
        None => requested,
    };
    let mut out = vec![requested.to_owned()];
    if !base.is_empty() && base != requested {
        out.push(base.to_owned());
    }
    out
}

fn resolve_with<'a, I>(requested: &str, entries: I) -> AliasResult
where
    I: Iterator<Item = (&'a str, &'a str, bool)> + Clone,
{
    let base = canonical_model(requested);
    for candidate in candidates(requested) {
        for (name, alias, force) in entries.clone() {
            let (name, alias) = (name.trim(), alias.trim());
            if name.is_empty() || alias.is_empty() || !alias.eq_ignore_ascii_case(&candidate) {
                continue;
            }
            if name.eq_ignore_ascii_case(base) {
                if !force {
                    return AliasResult::default();
                }
                return AliasResult {
                    upstream: preserve_suffix(name, requested),
                    force_mapping: true,
                    original_alias: alias.to_owned(),
                };
            }
            return AliasResult {
                upstream: preserve_suffix(name, requested),
                force_mapping: force,
                original_alias: if force { alias.to_owned() } else { requested.to_owned() },
            };
        }
    }
    AliasResult::default()
}

/// Go `rewriteModelForAuth`: drop the credential's `prefix/`.
pub fn strip_prefix<'a>(model: &'a str, c: &Credential) -> &'a str {
    let prefix = credential_prefix(c);
    if prefix.is_empty() || model.is_empty() {
        return model;
    }
    model.strip_prefix(&format!("{prefix}/")).unwrap_or(model)
}

/// Upstream model candidates for one credential (Go `executionModelCandidatesWithAlias`).
/// More than one candidate is an OpenAI-compatible alias pool, tried in order.
pub fn execution_models(
    cfg_aliases: &HashMap<String, Vec<OAuthAlias>>,
    c: &Credential,
    route_model: &str,
) -> (Vec<String>, AliasResult) {
    let requested = strip_prefix(route_model, c);
    let configured = is_api_key(c) || openai_compat(c);
    let models = config_models(c);
    let alias = if configured {
        if models.is_empty() {
            AliasResult {
                upstream: requested.to_owned(),
                ..AliasResult::default()
            }
        } else {
            let r = resolve_with(
                requested,
                models
                    .iter()
                    .map(|m| (m.name.as_str(), m.alias.as_str(), m.force_mapping)),
            );
            if r.upstream.is_empty() {
                AliasResult {
                    upstream: requested.to_owned(),
                    ..AliasResult::default()
                }
            } else {
                r
            }
        }
    } else {
        let aliases = oauth_aliases_for(cfg_aliases, c);
        resolve_with(
            requested,
            aliases
                .iter()
                .map(|a| (a.name.as_str(), a.alias.as_str(), a.force_mapping)),
        )
    };
    let upstream = if configured || alias.upstream.is_empty() {
        requested.to_owned()
    } else {
        alias.upstream.clone()
    };
    if configured && !models.is_empty() {
        let pool = alias_pool(&upstream, &models);
        if openai_compat(c) && pool.len() > 1 {
            return (pool, alias);
        }
        if let Some(first) = pool.into_iter().next() {
            return (vec![first], alias);
        }
    }
    (vec![upstream], alias)
}

/// Go `resolveModelAliasPoolFromConfigModels`.
fn alias_pool(requested: &str, models: &[ConfigModel]) -> Vec<String> {
    for candidate in candidates(requested) {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for m in models {
            let (name, alias) = (m.name.trim(), m.alias.trim());
            if alias.is_empty() || !alias.eq_ignore_ascii_case(&candidate) {
                continue;
            }
            let resolved = preserve_suffix(if name.is_empty() { &candidate } else { name }, requested);
            if !resolved.is_empty() && seen.insert(resolved.to_lowercase()) {
                out.push(resolved);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    for candidate in candidates(requested) {
        if let Some(m) = models
            .iter()
            .find(|m| !m.name.trim().is_empty() && m.name.trim().eq_ignore_ascii_case(&candidate))
        {
            return vec![preserve_suffix(m.name.trim(), requested)];
        }
    }
    Vec::new()
}

/// Go `selectionModelForAuth`: prefix stripped, OAuth alias applied. Cooldown state is
/// keyed by this when it equals the upstream model, else by the route model.
pub fn selection_model(cfg_aliases: &HashMap<String, Vec<OAuthAlias>>, c: &Credential, route_model: &str) -> String {
    let requested = strip_prefix(route_model, c);
    if is_api_key(c) || openai_compat(c) {
        return requested.to_owned();
    }
    let aliases = oauth_aliases_for(cfg_aliases, c);
    let r = resolve_with(
        requested,
        aliases
            .iter()
            .map(|a| (a.name.as_str(), a.alias.as_str(), a.force_mapping)),
    );
    if r.upstream.is_empty() {
        requested.to_owned()
    } else {
        r.upstream
    }
}

/// Go `stateModelForExecution`.
pub fn state_model(selection: &str, route_model: &str, upstream: &str, pooled: bool) -> String {
    if !selection.trim().is_empty() && canonical_model(selection) == canonical_model(upstream) {
        return upstream.trim().to_owned();
    }
    if pooled && !upstream.trim().is_empty() {
        return upstream.trim().to_owned();
    }
    if !route_model.trim().is_empty() {
        route_model.trim().to_owned()
    } else {
        upstream.trim().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn file(id: &str, meta: Value, attrs: &[(&str, &str)]) -> Arc<Credential> {
        let mut c = Credential::from_file(
            Path::new("/a"),
            &Path::new("/a").join(id),
            meta.as_object().unwrap().clone(),
        )
        .unwrap();
        for (k, v) in attrs {
            c.attributes.insert((*k).into(), (*v).into());
        }
        Arc::new(c)
    }

    /// Go `Auth.AccountInfo` (sdk/cliproxy/auth/types.go): kind first, then the
    /// trimmed email or API key; a non-string email or an unclassified auth is empty.
    #[test]
    fn account_info_matches_go() {
        let oauth = file(
            "o.json",
            serde_json::json!({"type":"claude","email":" a@b.test ","access_token":"t"}),
            &[],
        );
        assert_eq!(account_info(&oauth), ("oauth", "a@b.test".into()));
        let odd = file(
            "x.json",
            serde_json::json!({"type":"claude","email":7,"access_token":"t"}),
            &[],
        );
        assert_eq!(account_info(&odd), ("oauth", String::new()));
        let key = file(
            "k.json",
            serde_json::json!({"type":"claude","email":"ignored@b.test"}),
            &[("api_key", " sk-1 ")],
        );
        assert_eq!(account_info(&key), ("api_key", "sk-1".into()));
        let declared = file(
            "d.json",
            serde_json::json!({"type":"claude"}),
            &[("auth_kind", "API-KEY")],
        );
        assert_eq!(account_info(&declared), ("api_key", String::new()));
        let bare = file("b.json", serde_json::json!({"type":"claude"}), &[]);
        assert_eq!(account_info(&bare), ("", String::new()));
    }

    fn config(provider: &str, id: &str, meta: Value, attrs: &[(&str, &str)]) -> Arc<Credential> {
        let mut c = Credential {
            id: id.into(),
            provider: provider.into(),
            source: Source::Config {
                section: format!("{provider}-api-key"),
                index: 0,
            },
            disabled: false,
            label: String::new(),
            attributes: attrs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect(),
            metadata: meta.as_object().unwrap().clone(),
            revision: 0,
        };
        c.attributes.insert("auth_kind".into(), "apikey".into());
        Arc::new(c)
    }

    #[test]
    fn oauth_claude_registers_go_catalog_minus_disabled() {
        let cfg = Config::default();
        let live = file("a.json", serde_json::json!({"type":"claude"}), &[]);
        let off = file("b.json", serde_json::json!({"type":"claude","disabled":true}), &[]);
        let r = Registry::build(&cfg, &[live, off]);
        assert_eq!(r.available().count(), 18, "Go registers the 18-model Claude catalog");
        assert_eq!(r.providers("claude-sonnet-4-6"), ["claude"]);
        assert!(r.has_client("a.json") && !r.has_client("b.json"));
        assert!(r.providers("gpt-5.5").is_empty());
        let only_off = Registry::build(
            &cfg,
            &[file(
                "b.json",
                serde_json::json!({"type":"claude","disabled":true}),
                &[],
            )],
        );
        assert!(
            only_off.providers("claude-sonnet-4-6").is_empty(),
            "disabled credentials register nothing"
        );
    }

    #[test]
    fn exclusions_aliases_forks_and_prefixes_follow_go() {
        let cfg = Config::parse(
            "oauth:\n  model-alias:\n    claude:\n      - {name: claude-opus-5, alias: big, fork: true}\n      - {name: claude-sonnet-5, alias: mid, display-name: Mid}\n",
        )
        .unwrap();
        let c = file(
            "a.json",
            serde_json::json!({"type":"claude"}),
            &[("excluded_models", "*haiku*,*-2025*"), ("prefix", "team")],
        );
        let r = Registry::build(&cfg, std::slice::from_ref(&c));
        let ids: Vec<&str> = r.ids().collect();
        assert!(
            !ids.iter().any(|id| id.contains("haiku") || id.contains("-2025")),
            "{ids:?}"
        );
        // Fork keeps the source model; a plain alias replaces it.
        assert!(ids.contains(&"claude-opus-5") && ids.contains(&"big"));
        assert!(!ids.contains(&"claude-sonnet-5") && ids.contains(&"mid"));
        assert_eq!(r.info("mid", None).unwrap().display_name, "Mid");
        assert!(ids.contains(&"team/mid") && ids.contains(&"team/claude-opus-5"));
        assert!(r.client_supports("a.json", "TEAM/MID"));
        // Forced prefixes drop the bare IDs.
        let forced = Config::parse("routing:\n  force-model-prefix: true\n").unwrap();
        let r = Registry::build(&forced, std::slice::from_ref(&c));
        assert!(r.ids().all(|id| id.starts_with("team/")));
        // Execution resolves the alias back to its upstream name and keeps the suffix.
        let aliases = global_aliases(&cfg);
        let (models, alias) = execution_models(&aliases, &c, "team/mid(high)");
        assert_eq!(models, ["claude-sonnet-5(high)"]);
        assert!(!alias.force_mapping);
        assert_eq!(
            state_model(
                &selection_model(&aliases, &c, "team/mid"),
                "team/mid",
                "claude-sonnet-5",
                false
            ),
            "claude-sonnet-5"
        );
    }

    #[test]
    fn config_models_and_force_mapping() {
        let cfg = Config::default();
        let c = config(
            "claude",
            "claude:apikey:1",
            serde_json::json!({"models":[
                {"name":"claude-opus-5","alias":"opus","force-mapping":true,"is-compat":true},
                {"name":"claude-sonnet-5","alias":"opus"}]}),
            &[],
        );
        let r = Registry::build(&cfg, std::slice::from_ref(&c));
        assert_eq!(
            r.ids().collect::<Vec<_>>(),
            ["opus"],
            "first alias wins, duplicates dropped"
        );
        let spec = r.client_model("claude:apikey:1", "opus").unwrap();
        assert!(spec.is_compat && spec.user_defined);
        assert_eq!(spec.owned_by, "anthropic");
        assert_eq!(
            spec.thinking,
            pinned().lookup("claude-opus-5").unwrap().thinking,
            "static thinking inherited"
        );
        let (models, alias) = execution_models(&HashMap::new(), &c, "opus");
        assert_eq!(models, ["claude-opus-5"], "non-compat API keys never pool");
        assert_eq!(
            alias,
            AliasResult {
                upstream: "claude-opus-5".into(),
                force_mapping: true,
                original_alias: "opus".into()
            }
        );
        // Unknown models pass through unchanged.
        assert_eq!(execution_models(&HashMap::new(), &c, "other").0, ["other"]);
    }

    #[test]
    fn compat_pools_rotate_through_every_name() {
        let c = config(
            "openai-compatible-pool",
            "openai-compatibility:pool:1",
            serde_json::json!({"model_aliases":[{"name":"a-1","alias":"fast"},{"name":"a-2","alias":"fast"}]}),
            &[("compat_name", "pool"), ("provider_key", "openai-compatible-pool")],
        );
        let r = Registry::build(&Config::default(), std::slice::from_ref(&c));
        assert_eq!(r.providers("fast"), ["openai-compatible-pool"]);
        assert_eq!(
            r.info("fast", None).unwrap().thinking.as_ref().unwrap().levels,
            ["low", "medium", "high"]
        );
        assert_eq!(execution_models(&HashMap::new(), &c, "fast").0, ["a-1", "a-2"]);
    }

    #[test]
    fn wildcard_matches_go() {
        for (p, v, ok) in [
            ("a*c", "abc", true),
            ("a*c", "ab", false),
            ("*", "x", true),
            ("a**b", "ab", true),
            ("x", "x", true),
            ("*mid*", "a-mid-b", true),
            ("a*b*c", "acb", false),
        ] {
            assert_eq!(wildcard(p, v), ok, "{p} {v}");
        }
    }
}
