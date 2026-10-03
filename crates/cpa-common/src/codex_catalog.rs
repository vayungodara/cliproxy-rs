//! The model catalog official Codex clients fetch with `GET /v1/models?client_version=`
//! (internal/client/codex/models/models.go and apply_patch.go), and the Codex client
//! catalog store it is built from (internal/registry/codex_client_models.go).
//!
//! Catalog models served by this proxy keep their template, adjusted to what the
//! routing providers support; every other model is synthesized from the `gpt-5.5`
//! template with compact instructions. Entries are Go maps, so the output has Go's
//! sorted keys and float64 numbers.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use cpa_core::registry::ThinkingSupport;

use crate::json::GoValue;

type Map = BTreeMap<String, GoValue>;

/// `registry.OpenAIImageModelType`.
const IMAGE_MODEL_TYPE: &str = "openai-image";
const FALLBACK_INSTRUCTIONS: &str = "You are Codex, a coding agent. You and the user share one workspace.";
const LEVELS: [&str; 8] = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];
const LEGACY_LEVELS: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];

// --------------------------------------------------------------------------- the store

struct Store {
    data: Arc<Vec<u8>>,
    revision: u64,
}

static STORE: RwLock<Option<Store>> = RwLock::new(None);

fn with_store<T>(f: impl FnOnce(&mut Option<Store>) -> T) -> T {
    let mut store = STORE.write().unwrap_or_else(std::sync::PoisonError::into_inner);
    if store.is_none() {
        // Go's init loads the embedded catalog at revision 1.
        *store = Some(Store {
            data: Arc::new(crate::codex_client::CLIENT_MODELS_JSON.as_bytes().to_vec()),
            revision: 1,
        });
    }
    f(&mut store)
}

/// `GetCodexClientModelsSnapshot`: the current catalog and its revision.
pub fn snapshot() -> (Arc<Vec<u8>>, u64) {
    {
        let store = STORE.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(store) = store.as_ref() {
            return (store.data.clone(), store.revision);
        }
    }
    with_store(|store| {
        let store = store.as_ref().expect("initialised");
        (store.data.clone(), store.revision)
    })
}

/// `loadCodexClientModelsFromBytes`: validates and installs a catalog. `Ok(false)` when
/// it equals the current one (the revision changes only with the content).
pub fn load(data: &[u8]) -> Result<bool, String> {
    validate(data)?;
    with_store(|store| {
        let store = store.as_mut().expect("initialised");
        if store.data.as_slice() == data {
            return Ok(false);
        }
        store.data = Arc::new(data.to_vec());
        store.revision += 1;
        Ok(true)
    })
}

/// `ValidateCodexClientModelsJSON`: the fields a complete Codex client catalog needs.
// ponytail: decode failures carry serde_json's wording, not encoding/json's; the text
// is only logged.
pub fn validate(data: &[u8]) -> Result<(), String> {
    let root: serde_json::Value =
        serde_json::from_slice(data).map_err(|e| format!("decode Codex client model catalog: {e}"))?;
    let models = match root.get("models") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(models)) => models.clone(),
        Some(_) => return Err("decode Codex client model catalog: models is not an array".into()),
    };
    if models.is_empty() {
        return Err("Codex client model catalog has no models".into());
    }
    let mut seen = HashSet::new();
    for (index, model) in models.iter().enumerate() {
        let empty = serde_json::Map::new();
        let model = model.as_object().unwrap_or(&empty);
        let slug =
            required_string(model, "slug").map_err(|e| format!("Codex client model catalog models[{index}]: {e}"))?;
        if !seen.insert(slug.clone()) {
            return Err(format!("Codex client model catalog contains duplicate slug {slug:?}"));
        }
        validate_model(model).map_err(|e| format!("Codex client model catalog model {slug:?}: {e}"))?;
    }
    if !seen.contains("gpt-5.5") {
        return Err(format!(
            "Codex client model catalog is missing default template {:?}",
            "gpt-5.5"
        ));
    }
    Ok(())
}

fn validate_model(model: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    for field in [
        "display_name",
        "description",
        "base_instructions",
        "minimal_client_version",
        "visibility",
        "default_reasoning_level",
    ] {
        required_string(model, field)?;
    }
    let context = required_integer(model, "context_window", true)?;
    let max_context = required_integer(model, "max_context_window", true)?;
    if context > max_context {
        return Err(format!(
            "context_window {context} exceeds max_context_window {max_context}"
        ));
    }
    required_integer(model, "priority", false)?;
    let levels = match model.get("supported_reasoning_levels") {
        Some(serde_json::Value::Array(levels)) if !levels.is_empty() => levels,
        _ => {
            return Err(format!(
                "field {:?} must be a non-empty array",
                "supported_reasoning_levels"
            ));
        }
    };
    let mut seen = HashSet::new();
    for (index, level) in levels.iter().enumerate() {
        let Some(level) = level.as_object() else {
            return Err(format!(
                "field {:?} entry {index} must be an object",
                "supported_reasoning_levels"
            ));
        };
        let effort = required_string(level, "effort")
            .map_err(|e| format!("field {:?} entry {index}: {e}", "supported_reasoning_levels"))?;
        if !seen.insert(effort.clone()) {
            return Err(format!(
                "field {:?} contains duplicate effort {effort:?}",
                "supported_reasoning_levels"
            ));
        }
    }
    let default = required_string(model, "default_reasoning_level")?;
    if !seen.contains(&default) {
        return Err(format!(
            "default_reasoning_level {default:?} is not listed in supported_reasoning_levels"
        ));
    }
    Ok(())
}

fn required_string(model: &serde_json::Map<String, serde_json::Value>, field: &str) -> Result<String, String> {
    match model.get(field).and_then(serde_json::Value::as_str).map(str::trim) {
        Some(value) if !value.is_empty() => Ok(value.to_owned()),
        _ => Err(format!("field {field:?} must be a non-empty string")),
    }
}

fn required_integer(
    model: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    positive: bool,
) -> Result<i64, String> {
    let value = model.get(field).and_then(serde_json::Value::as_f64);
    let Some(value) = value.filter(|v| v.is_finite() && v.trunc() == *v && *v <= i64::MAX as f64) else {
        return Err(format!("field {field:?} must be an integer"));
    };
    if positive && value <= 0.0 {
        return Err(format!("field {field:?} must be positive"));
    }
    if !positive && value < 0.0 {
        return Err(format!("field {field:?} must not be negative"));
    }
    Ok(value as i64)
}

/// The catalog's models by slug and the `gpt-5.5` default template, parsed once per
/// revision (`loadCodexClientModelTemplates`).
struct Templates {
    by_slug: HashMap<String, Map>,
    default: Map,
}

fn templates() -> Option<Arc<Templates>> {
    static CACHE: Mutex<Option<(u64, Option<Arc<Templates>>)>> = Mutex::new(None);
    let (raw, revision) = snapshot();
    let mut cache = CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((cached, templates)) = cache.as_ref()
        && *cached == revision
    {
        return templates.clone();
    }
    let parsed = GoValue::parse_f64(&raw).and_then(|root| {
        let GoValue::Object(mut root) = root else {
            return None;
        };
        let GoValue::Array(models) = root.remove("models")? else {
            return None;
        };
        let mut by_slug = HashMap::new();
        for model in models {
            let GoValue::Object(model) = model else {
                continue;
            };
            let slug = string(&model, "slug");
            if !slug.is_empty() {
                by_slug.insert(slug, model);
            }
        }
        let default = by_slug.get("gpt-5.5")?.clone();
        Some(Arc::new(Templates { by_slug, default }))
    });
    *cache = Some((revision, parsed.clone()));
    parsed
}

// ------------------------------------------------------------------------ the builder

/// What the model registry knows about one model (Go `*registry.ModelInfo`).
#[derive(Debug, Clone, Default)]
pub struct ModelFacts {
    pub id: String,
    pub kind: String,
    pub owned_by: String,
    pub display_name: String,
    pub description: String,
    pub context_length: i64,
    pub metadata_model_id: String,
    pub thinking: Option<ThinkingSupport>,
    pub explicit_thinking: bool,
    pub input_modalities: Vec<String>,
    pub explicit_input_modalities: bool,
}

/// The registry queries the catalog makes.
pub trait Registry {
    /// `registry.LookupModelInfo(id[, provider])`.
    fn lookup(&self, id: &str, provider: Option<&str>) -> Option<ModelFacts>;
    /// `GetModelProviders`.
    fn providers(&self, id: &str) -> Vec<String>;
    /// `GetResponsesWebSearchCapability`: `None` when unknown.
    fn web_search(&self, id: &str) -> Option<bool>;
}

/// `BuildResponseForClientWithToolCapabilities`: `{"models": [...]}` for `available`
/// (Go's `GetAvailableModels("openai")` maps). `apply_patch` is set when the
/// `client.codex.enable-apply-patch` capability check applies.
pub fn build_response(
    available: &[Map],
    registry: &dyn Registry,
    apply_patch: Option<&dyn Fn(&str) -> bool>,
    optimize_multi_agent_v2: bool,
    client_version: &str,
) -> GoValue {
    let models = build_models(
        available,
        registry,
        apply_patch,
        optimize_multi_agent_v2,
        client_version,
    )
    .map_or(GoValue::Null, |models| {
        GoValue::Array(models.into_iter().map(GoValue::Object).collect())
    });
    GoValue::Object(BTreeMap::from([("models".to_owned(), models)]))
}

/// `MarshalCompact`: one JSON line without HTML escaping.
pub fn marshal_compact(payload: &GoValue) -> Vec<u8> {
    payload.marshal_no_html()
}

fn build_models(
    available: &[Map],
    registry: &dyn Registry,
    apply_patch: Option<&dyn Fn(&str) -> bool>,
    optimize: bool,
    version: &str,
) -> Option<Vec<Map>> {
    let templates = templates()?;
    let mut result = Vec::with_capacity(available.len());
    for model in available {
        let id = string(model, "id");
        if id.is_empty() {
            continue;
        }
        let metadata_id = metadata_model_id(&id, registry);
        if let Some(template) = templates.by_slug.get(&metadata_id) {
            let mut entry = template.clone();
            entry.insert("slug".into(), GoValue::String(id.clone()));
            let info = registry.lookup(&id, None);
            apply_capabilities(&mut entry, &id, &metadata_id, info.as_ref(), registry, version);
            for key in ["display_name", "description", "base_instructions"] {
                let value = string(model, key);
                if !value.is_empty() {
                    entry.insert(key.into(), GoValue::String(value));
                }
            }
            let max_context = int(model, "max_context_length");
            if max_context > 0 {
                entry.insert("context_window".into(), number(max_context));
                entry.insert("max_context_window".into(), number(max_context));
            }
            apply_max_tokens(&mut entry, model);
            if let Some(thinking) = thinking_support(model) {
                apply_thinking(&mut entry, &thinking, version);
            }
            apply_provider_capabilities(&mut entry, &id, true, registry);
            apply_web_search(&mut entry, &id, registry, version);
            sanitize_reasoning(&mut entry, version);
            apply_visibility(&mut entry, &id);
            if optimize {
                entry.insert("multi_agent_version".into(), GoValue::String("v2".into()));
            }
            apply_devin_display_name(&mut entry, &id, model, registry);
            apply_patch_capability(&mut entry, &id, apply_patch);
            result.push(entry);
            continue;
        }
        let mut entry = templates.default.clone();
        apply_model_metadata(&mut entry, &id, model, optimize, version, registry);
        apply_max_tokens(&mut entry, model);
        apply_provider_capabilities(&mut entry, &id, false, registry);
        apply_web_search(&mut entry, &id, registry, version);
        sanitize_reasoning(&mut entry, version);
        apply_visibility(&mut entry, &id);
        apply_devin_display_name(&mut entry, &id, model, registry);
        apply_patch_capability(&mut entry, &id, apply_patch);
        result.push(entry);
    }
    apply_non_template_priorities(&mut result, &templates, registry);
    result.sort_by_key(priority);
    Some(result)
}

/// `stringModelValue`: a string field, trimmed.
fn string(model: &Map, key: &str) -> String {
    match model.get(key) {
        Some(GoValue::String(s)) => s.trim().to_owned(),
        _ => String::new(),
    }
}

/// `intModelValue`: a numeric field as Go's `int`.
fn int(model: &Map, key: &str) -> i64 {
    match model.get(key) {
        Some(GoValue::Number(n)) => n.parse::<f64>().map_or(0, |f| f as i64),
        _ => 0,
    }
}

fn number(n: i64) -> GoValue {
    GoValue::Number(n.to_string())
}

/// `codexClientModelPriority`: 100 unless set.
fn priority(model: &Map) -> i64 {
    match model.get("priority") {
        Some(GoValue::Number(n)) => n.parse::<f64>().map_or(100, |f| f as i64),
        _ => 100,
    }
}

fn base_of(id: &str) -> Option<&str> {
    id.find('/').map(|i| id[i + 1..].trim())
}

/// `codexClientMetadataModelID`.
fn metadata_model_id(id: &str, registry: &dyn Registry) -> String {
    let id = id.trim();
    if let Some(metadata) = registry
        .lookup(id, None)
        .map(|i| i.metadata_model_id.trim().to_owned())
        .filter(|m| !m.is_empty())
    {
        return metadata;
    }
    if let Some(base) = base_of(id) {
        if let Some(metadata) = registry
            .lookup(base, None)
            .map(|i| i.metadata_model_id.trim().to_owned())
            .filter(|m| !m.is_empty())
        {
            return metadata;
        }
        return base.to_owned();
    }
    id.to_owned()
}

/// Providers for `id`, else for the part after its first `/`.
fn providers(id: &str, registry: &dyn Registry) -> Vec<String> {
    let found = registry.providers(id);
    if found.is_empty()
        && let Some(base) = base_of(id)
    {
        return registry.providers(base);
    }
    found
}

fn hide(entry: &mut Map) {
    entry.insert("visibility".into(), GoValue::String("hide".into()));
    entry.remove("input_modalities");
    entry.remove("supports_image_detail_original");
}

fn set_modalities(entry: &mut Map, modalities: Vec<GoValue>) {
    let image = modalities
        .iter()
        .any(|m| matches!(m, GoValue::String(s) if s == "image"));
    entry.insert("input_modalities".into(), GoValue::Array(modalities));
    if image {
        entry.insert("supports_image_detail_original".into(), GoValue::Bool(true));
    } else {
        entry.remove("supports_image_detail_original");
    }
}

/// `applyCodexClientModelCapabilities`: modalities and thinking narrowed to what every
/// routing provider declares (non-Codex aliases always constrain).
fn apply_capabilities(
    entry: &mut Map,
    id: &str,
    metadata_id: &str,
    info: Option<&ModelFacts>,
    registry: &dyn Registry,
    version: &str,
) {
    if info.is_some_and(|i| i.kind == IMAGE_MODEL_TYPE) {
        hide(entry);
        return;
    }
    let providers = providers(id, registry);
    let alias = !metadata_id.is_empty() && !id.eq_ignore_ascii_case(metadata_id);
    let provider_info = |provider: &str| {
        registry
            .lookup(id, Some(provider))
            .or_else(|| base_of(id).and_then(|base| registry.lookup(base, Some(provider))))
    };
    let mut modalities: Option<Vec<String>> = None;
    for provider in &providers {
        let Some(p_info) = provider_info(provider) else {
            continue;
        };
        let codex = provider.trim().eq_ignore_ascii_case("codex");
        if (!codex && alias) || p_info.explicit_input_modalities {
            modalities = Some(match modalities {
                None => p_info.input_modalities.clone(),
                Some(current) => intersect(&current, &p_info.input_modalities),
            });
        }
    }
    if modalities.is_none()
        && let Some(info) = info.filter(|i| i.explicit_input_modalities)
    {
        modalities = Some(info.input_modalities.clone());
    }
    if let Some(modalities) = modalities {
        set_modalities(entry, filter_modalities(&modalities));
    }
    let mut thinking: Option<ThinkingSupport> = None;
    let mut constrained = false;
    for provider in &providers {
        let Some(p_info) = provider_info(provider) else {
            continue;
        };
        let codex = provider.trim().eq_ignore_ascii_case("codex");
        if (!codex && alias) || p_info.explicit_thinking {
            constrained = true;
            let p_thinking = p_info.thinking.clone().unwrap_or_default();
            thinking = Some(match thinking {
                None => p_thinking,
                Some(current) => intersect_thinking(&current, &p_thinking),
            });
        }
    }
    if !constrained && let Some(info) = info.filter(|i| i.explicit_thinking) {
        thinking = Some(info.thinking.clone().unwrap_or_default());
        constrained = true;
    }
    if constrained && let Some(thinking) = thinking {
        apply_thinking(entry, &thinking, version);
    }
}

/// `intersectThinkingSupport`.
fn intersect_thinking(a: &ThinkingSupport, b: &ThinkingSupport) -> ThinkingSupport {
    let mut max = a.max;
    if b.max > 0 && (max == 0 || b.max < max) {
        max = b.max;
    }
    ThinkingSupport {
        min: a.min.max(b.min),
        max,
        zero_allowed: a.zero_allowed && b.zero_allowed,
        dynamic_allowed: a.dynamic_allowed && b.dynamic_allowed,
        levels: intersect(&a.levels, &b.levels),
    }
}

/// `intersectStringSlices`: `a`'s items also in `b` (case-insensitive), deduplicated.
fn intersect(a: &[String], b: &[String]) -> Vec<String> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let key = |s: &String| s.trim().to_lowercase();
    let b: HashSet<String> = b.iter().map(key).collect();
    let mut seen = HashSet::new();
    a.iter()
        .filter(|item| {
            let k = key(item);
            b.contains(&k) && seen.insert(k)
        })
        .cloned()
        .collect()
}

fn apply_max_tokens(entry: &mut Map, model: &Map) {
    let max = int(model, "max_completion_tokens");
    if max > 0 {
        entry.insert("max_tokens".into(), number(max));
    }
}

/// `applyCPAWebSearchCapability`: only CPA's own clients see `cpa_capabilities`.
fn apply_web_search(entry: &mut Map, id: &str, registry: &dyn Registry, version: &str) {
    entry.remove("cpa_capabilities");
    if version != "cpa" {
        return;
    }
    if let Some(supported) = registry.web_search(id.trim()) {
        entry.insert(
            "cpa_capabilities".into(),
            GoValue::Object(BTreeMap::from([("web_search".to_owned(), GoValue::Bool(supported))])),
        );
    }
}

/// `applyCodexClientProviderCapabilities`.
fn apply_provider_capabilities(entry: &mut Map, id: &str, template: bool, registry: &dyn Registry) {
    if !template {
        apply_search_tool(entry, id, false, registry);
        return;
    }
    if !pure_codex(id, registry) {
        entry.insert("supports_search_tool".into(), GoValue::Bool(false));
        entry.insert("prefer_websockets".into(), GoValue::Bool(false));
        entry.insert("service_tiers".into(), GoValue::Array(Vec::new()));
        null_required_options(entry);
        return;
    }
    apply_search_tool(entry, id, true, registry);
}

/// `nullCodexClientRequiredOptions`: keys Codex requires, cleared for non-Codex models.
fn null_required_options(entry: &mut Map) {
    for key in ["apply_patch_tool_type", "upgrade", "availability_nux"] {
        entry.insert(key.into(), GoValue::Null);
    }
}

/// `useCompactCodexClientInstructions`.
fn compact_instructions(entry: &mut Map) {
    entry.insert(
        "base_instructions".into(),
        GoValue::String(FALLBACK_INSTRUCTIONS.into()),
    );
    let mut messages = Map::new();
    messages.insert(
        "instructions_template".into(),
        GoValue::String(FALLBACK_INSTRUCTIONS.into()),
    );
    for key in [
        "instructions_variables",
        "approvals",
        "collaboration_modes",
        "auto_review",
        "permissions",
        "multi_agent",
    ] {
        messages.insert(key.into(), GoValue::Null);
    }
    entry.insert("model_messages".into(), GoValue::Object(messages));
}

/// `isPureCodexProvider`: served, and only by Codex.
fn pure_codex(id: &str, registry: &dyn Registry) -> bool {
    let providers = providers(id, registry);
    !providers.is_empty() && providers.iter().all(|p| p.trim().eq_ignore_ascii_case("codex"))
}

/// `applyCodexClientSearchToolSupport`.
fn apply_search_tool(entry: &mut Map, id: &str, template: bool, registry: &dyn Registry) {
    if !matches!(entry.get("supports_search_tool"), Some(GoValue::Bool(true))) {
        return;
    }
    if !template || !pure_codex(id, registry) {
        entry.insert("supports_search_tool".into(), GoValue::Bool(false));
    }
}

/// `applyCodexClientModelMetadata`: a synthesized entry for a model outside the catalog.
fn apply_model_metadata(
    entry: &mut Map,
    id: &str,
    model: &Map,
    optimize: bool,
    version: &str,
    registry: &dyn Registry,
) {
    let info = registry.lookup(id, None);
    let mut display_name = string(model, "display_name");
    let mut description = string(model, "description");
    let mut context = int(model, "context_length");
    let mut thinking = thinking_support(model);
    if let Some(info) = &info {
        if !info.display_name.is_empty() {
            display_name.clone_from(&info.display_name);
        }
        if !info.description.is_empty() {
            description.clone_from(&info.description);
        }
        if context <= 0 && info.context_length > 0 {
            context = info.context_length;
        }
        if info.kind == IMAGE_MODEL_TYPE {
            hide(entry);
        } else {
            let modalities = filter_modalities(&info.input_modalities);
            if !modalities.is_empty() {
                set_modalities(entry, modalities);
            }
        }
        if thinking.is_none() {
            thinking.clone_from(&info.thinking);
        }
    }
    if let Some(thinking) = &thinking {
        apply_thinking(entry, thinking, version);
    }
    let max_context = int(model, "max_context_length");
    if max_context > 0 {
        context = max_context;
    }
    if display_name.is_empty() {
        display_name = id.to_owned();
    }
    if description.is_empty() {
        description = id.to_owned();
    }
    entry.insert("slug".into(), GoValue::String(id.to_owned()));
    entry.insert("display_name".into(), GoValue::String(display_name));
    entry.insert("description".into(), GoValue::String(description));
    entry.insert("prefer_websockets".into(), GoValue::Bool(false));
    if optimize {
        entry.insert("multi_agent_version".into(), GoValue::String("v2".into()));
    }
    entry.insert("service_tiers".into(), GoValue::Array(Vec::new()));
    null_required_options(entry);
    if context > 0 {
        entry.insert("context_window".into(), number(context));
        entry.insert("max_context_window".into(), number(context));
    }
    if let Some(plans) = model.get("available_in_plans") {
        entry.insert("available_in_plans".into(), plans.clone());
    }
    // Codex 0.156+ caps a model_catalog_url body at 1 MiB: synthesized entries carry
    // short instructions instead of the template's.
    compact_instructions(entry);
}

/// `codexClientThinkingSupport`: the model's own `thinking` object.
fn thinking_support(model: &Map) -> Option<ThinkingSupport> {
    match model.get("thinking") {
        None | Some(GoValue::Null) => None,
        Some(value) => serde_json::from_slice(&value.marshal()).ok(),
    }
}

/// `codexClientImageOrVideoModel` IDs, always hidden.
fn image_or_video(id: &str) -> bool {
    let target = base_of(id).unwrap_or(id.trim());
    matches!(
        target,
        "grok-imagine-image-quality"
            | "gpt-image-1.5"
            | "gpt-image-2"
            | "gpt-image-2.5-flare"
            | "gpt-image-2.5-sunburst"
            | "gpt-image-2.5"
            | "grok-imagine-image"
            | "grok-imagine-image-2.0"
            | "grok-imagine-video"
            | "grok-imagine-video-1.5"
            | "grok-imagine-video-1.5-preview"
    )
}

fn apply_visibility(entry: &mut Map, id: &str) {
    if image_or_video(id) {
        entry.insert("visibility".into(), GoValue::String("hide".into()));
    }
}

/// `filterCodexInputModalities`: text and image, in order, once each.
fn filter_modalities(modalities: &[String]) -> Vec<GoValue> {
    let mut seen = HashSet::new();
    modalities
        .iter()
        .map(|m| m.trim().to_lowercase())
        .filter(|m| (m == "text" || m == "image") && seen.insert(m.clone()))
        .map(GoValue::String)
        .collect()
}

fn reasoning_description(level: &str) -> &str {
    match level {
        "none" => "No reasoning",
        "minimal" => "Fastest responses with minimal reasoning",
        "low" => "Fast responses with lighter reasoning",
        "medium" => "Balances speed and reasoning depth for everyday tasks",
        "high" => "Greater reasoning depth for complex problems",
        "xhigh" => "Extra high reasoning depth for complex problems",
        "max" => "Maximum available reasoning depth for complex problems",
        other => other,
    }
}

/// `applyCodexClientThinkingMetadata`: the levels the client may pick, medium by
/// default when offered.
fn apply_thinking(entry: &mut Map, thinking: &ThinkingSupport, version: &str) {
    let mut levels = Vec::new();
    let (mut default, mut first) = (String::new(), String::new());
    for raw in &thinking.levels {
        let level = normalize_level(raw, version);
        if level.is_empty() {
            continue;
        }
        if first.is_empty() {
            first.clone_from(&level);
        }
        if (default.is_empty() && level != "none") || level == "medium" {
            default.clone_from(&level);
        }
        let mut item = Map::new();
        item.insert(
            "description".into(),
            GoValue::String(reasoning_description(&level).to_owned()),
        );
        item.insert("effort".into(), GoValue::String(level));
        levels.push(GoValue::Object(item));
    }
    if levels.is_empty() {
        entry.insert("supported_reasoning_levels".into(), GoValue::Array(levels));
        entry.remove("default_reasoning_level");
        return;
    }
    if default.is_empty() {
        default = first;
    }
    entry.insert("supported_reasoning_levels".into(), GoValue::Array(levels));
    entry.insert("default_reasoning_level".into(), GoValue::String(default));
}

/// `sanitizeCodexClientReasoningMetadata`: only levels this client version knows.
fn sanitize_reasoning(entry: &mut Map, version: &str) {
    let Some(GoValue::Array(raw)) = entry.get("supported_reasoning_levels") else {
        return;
    };
    let mut levels = Vec::new();
    let mut allowed = HashSet::new();
    for item in raw {
        let GoValue::Object(item) = item else {
            continue;
        };
        let level = normalize_level(&string(item, "effort"), version);
        if level.is_empty() {
            continue;
        }
        let mut item = item.clone();
        item.insert("effort".into(), GoValue::String(level.clone()));
        levels.push(item);
        allowed.insert(level);
    }
    if levels.is_empty() {
        entry.insert("supported_reasoning_levels".into(), GoValue::Array(Vec::new()));
        entry.remove("default_reasoning_level");
        return;
    }
    let mut default = normalize_level(&string(entry, "default_reasoning_level"), version);
    if !allowed.contains(&default) {
        default = string(&levels[0], "effort");
    }
    entry.insert(
        "supported_reasoning_levels".into(),
        GoValue::Array(levels.into_iter().map(GoValue::Object).collect()),
    );
    entry.insert("default_reasoning_level".into(), GoValue::String(default));
}

fn normalize_level(raw: &str, version: &str) -> String {
    let level = raw.trim().to_lowercase();
    let allowed: &[&str] = if extended_levels(version) {
        &LEVELS
    } else {
        &LEGACY_LEVELS
    };
    if allowed.contains(&level.as_str()) {
        level
    } else {
        String::new()
    }
}

/// `supportsExtendedReasoningLevels`: `max` and `ultra` need Codex 0.144.0; an empty or
/// unparseable version keeps them.
fn extended_levels(version: &str) -> bool {
    let version = version.trim();
    version.is_empty() || compare_versions(version, "0.144.0").is_none_or(|c| c >= 0)
}

/// `parseDottedVersion`: `v` prefix and `-`/`+` suffixes dropped; `None` when a part is
/// not a non-negative integer.
fn parse_version(version: &str) -> Vec<i64> {
    let mut version = version.trim();
    if let Some(rest) = version.strip_prefix(['v', 'V']) {
        version = rest;
    }
    if let Some(cut) = version.find(['-', '+']) {
        version = &version[..cut];
    }
    let mut parts = Vec::new();
    for part in version.split('.').map(str::trim).filter(|p| !p.is_empty()) {
        match part.parse::<i64>() {
            Ok(n) if n >= 0 => parts.push(n),
            _ => return Vec::new(),
        }
    }
    parts
}

fn compare_versions(a: &str, b: &str) -> Option<i32> {
    let (a, b) = (parse_version(a), parse_version(b));
    if a.is_empty() || b.is_empty() {
        return None;
    }
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return Some(if x < y { -1 } else { 1 });
        }
    }
    Some(0)
}

/// `applyCodexClientDevinDisplayName`: Devin models end in " (Devin)".
fn apply_devin_display_name(entry: &mut Map, id: &str, model: &Map, registry: &dyn Registry) {
    if !is_devin(id, model, entry, registry) {
        return;
    }
    let mut display = string(entry, "display_name");
    if display.is_empty() {
        display = id.to_owned();
    }
    let trimmed = display.trim();
    if trimmed.ends_with(" (Devin)") {
        return;
    }
    let lower = trimmed.to_lowercase();
    let renamed = if lower.ends_with(" (devin)") {
        format!("{} (Devin)", &trimmed[..trimmed.len() - " (devin)".len()])
    } else if lower.ends_with("(devin)") {
        format!("{} (Devin)", trimmed[..trimmed.len() - "(devin)".len()].trim())
    } else {
        format!("{trimmed} (Devin)")
    };
    entry.insert("display_name".into(), GoValue::String(renamed));
}

/// `isCodexClientDevinModel`.
fn is_devin(id: &str, model: &Map, entry: &Map, registry: &dyn Registry) -> bool {
    let devin_path = |s: &str| {
        let s = s.trim().to_lowercase();
        s.starts_with("devin/") || s.find('/').is_some_and(|i| s[i + 1..].starts_with("devin/"))
    };
    let marked = |m: &Map| {
        string(m, "type").eq_ignore_ascii_case("devin") || string(m, "owned_by").eq_ignore_ascii_case("cognition")
    };
    if devin_path(id) || devin_path(&string(entry, "slug")) || marked(entry) || marked(model) {
        return true;
    }
    let devin_info = |info: &ModelFacts| {
        info.kind.eq_ignore_ascii_case("devin")
            || info.owned_by.eq_ignore_ascii_case("cognition")
            || info.id.to_lowercase().starts_with("devin/")
    };
    match registry.lookup(id, None) {
        Some(info) => {
            if devin_info(&info) {
                return true;
            }
        }
        None => {
            if let Some(info) = base_of(id).and_then(|base| registry.lookup(base, None))
                && devin_info(&info)
            {
                return true;
            }
        }
    }
    providers(id, registry)
        .iter()
        .any(|p| p.trim().eq_ignore_ascii_case("devin"))
}

/// `applyCodexClientApplyPatchCapability`: `freeform` only for text models every route
/// supports.
fn apply_patch_capability(entry: &mut Map, id: &str, capability: Option<&dyn Fn(&str) -> bool>) {
    entry.insert("apply_patch_tool_type".into(), GoValue::Null);
    let Some(capability) = capability else {
        return;
    };
    let lower = id.trim().to_lowercase();
    let base = lower.rfind('/').map_or(lower.as_str(), |i| lower[i + 1..].trim());
    if image_or_video(base) {
        return;
    }
    let (mut text, mut declared) = (false, false);
    if let Some(GoValue::Array(modalities)) = entry.get("input_modalities") {
        declared = !modalities.is_empty();
        text = modalities
            .iter()
            .any(|m| matches!(m, GoValue::String(s) if s == "text"));
    }
    let hidden = matches!(entry.get("visibility"), Some(GoValue::String(s)) if s == "hide");
    if !text && (declared || hidden) {
        return;
    }
    if capability(id.trim()) {
        entry.insert("apply_patch_tool_type".into(), GoValue::String("freeform".into()));
    }
}

/// `applyCodexClientNonTemplatePriorities`: synthesized entries after every template,
/// ordered by display name.
fn apply_non_template_priorities(result: &mut [Map], templates: &Templates, registry: &dyn Registry) {
    if result.is_empty() {
        return;
    }
    let base = templates.by_slug.values().map(priority).fold(0, i64::max);
    let mut pending: Vec<(usize, String, String)> = result
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let slug = string(entry, "slug");
            if templates.by_slug.contains_key(&metadata_model_id(&slug, registry)) {
                return None;
            }
            let mut display = string(entry, "display_name");
            if display.is_empty() {
                display.clone_from(&slug);
            }
            Some((index, display, slug))
        })
        .collect();
    pending.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()).then_with(|| a.2.cmp(&b.2)));
    for (rank, (index, _, _)) in pending.into_iter().enumerate() {
        result[index].insert("priority".into(), number(base + 100 * (rank as i64 + 1)));
    }
}

#[cfg(test)]
#[path = "codex_catalog_tests.rs"]
mod tests;
