//! `GET /v1/models?client_version=…`: the catalog official Codex clients load
//! (OpenAIAPIHandler.OpenAIModels with `client_version`, sdk/api/handlers/openai/
//! codex_client_models.go), built by `cpa_common::codex_catalog` from the models this
//! proxy serves now.

use std::collections::BTreeMap;

use axum::response::Response;
use cpa_common::codex_catalog::{self, ModelFacts};
use cpa_common::json::GoValue;
use cpa_core::config::Config;
use cpa_core::registry::ModelInfo;

use crate::registry::{Registry, Spec};
use crate::{Runtime, respond};

/// Providers whose Go executor implements `SupportsApplyPatch` (AI Studio and Gemini CLI
/// do not).
const APPLY_PATCH_PROVIDERS: [&str; 10] = [
    "codex",
    "claude",
    "gemini",
    "vertex",
    "antigravity",
    "openai-compatibility",
    "kimi",
    "meta",
    "devin",
    "xai",
];

fn flag(cfg: &Config, path: &[&str]) -> bool {
    path.iter()
        .try_fold(&cfg.document, |v, k| v.get(*k))
        .and_then(serde_yaml_ng::Value::as_bool)
        .unwrap_or(false)
}

/// The catalog response, written like Go's `WriteModelListResponse`.
pub fn response(rt: &Runtime, client_version: &str) -> Response {
    crate::model_updater::codex_client_catalog_wanted(rt);
    let cfg = rt.config();
    let registry = rt.registry();
    let available: Vec<BTreeMap<String, GoValue>> = registry
        .available_with(|c, m| rt.suspension(c, m))
        .map(openai_map)
        .collect();
    let facts = Facts(&registry);
    let apply_patch = |model: &str| {
        let providers = registry.providers(model);
        !providers.is_empty()
            && providers.iter().all(|p| {
                rt.executors.supports(p)
                    && (APPLY_PATCH_PROVIDERS.contains(&p.as_str()) || cpa_exec::openai_compat::handles(p))
            })
    };
    let capability: Option<&dyn Fn(&str) -> bool> = if flag(&cfg, &["client", "codex", "enable-apply-patch"]) {
        Some(&apply_patch)
    } else {
        None
    };
    let body = codex_catalog::build_response(
        &available,
        &facts,
        capability,
        flag(&cfg, &["client", "codex", "optimize-multi-agent-v2"]),
        client_version,
    );
    respond::gin_json(
        200,
        String::from_utf8_lossy(&codex_catalog::marshal_compact(&body)).into_owned(),
    )
}

/// `convertModelToMap(model, "openai")`.
fn openai_map(m: &Spec) -> BTreeMap<String, GoValue> {
    let mut out = BTreeMap::new();
    let mut text = |k: &str, v: &str, always: bool| {
        if always || !v.is_empty() {
            out.insert(k.to_owned(), GoValue::String(v.to_owned()));
        }
    };
    text("id", &m.id, true);
    text("object", "model", true);
    text("owned_by", &m.owned_by, true);
    text("type", &m.kind, false);
    text("display_name", &m.display_name, false);
    text("version", &m.version, false);
    text("description", &m.description, false);
    for (key, value) in [
        ("created", m.created),
        ("context_length", m.context_length),
        ("max_context_length", m.max_context_length),
        ("max_completion_tokens", m.max_completion_tokens),
    ] {
        if value > 0 {
            out.insert(key.to_owned(), GoValue::Number(value.to_string()));
        }
    }
    if !m.supported_parameters.is_empty() {
        out.insert(
            "supported_parameters".to_owned(),
            GoValue::Array(
                m.supported_parameters
                    .iter()
                    .map(|p| GoValue::String(p.clone()))
                    .collect(),
            ),
        );
    }
    out
}

struct Facts<'a>(&'a Registry);

fn from_spec(s: &Spec) -> ModelFacts {
    ModelFacts {
        id: s.id.clone(),
        kind: s.kind.clone(),
        owned_by: s.owned_by.clone(),
        display_name: s.display_name.clone(),
        description: s.description.clone(),
        context_length: s.context_length,
        metadata_model_id: s.metadata_model_id.clone(),
        thinking: s.thinking.clone(),
        explicit_thinking: s.explicit_thinking,
        input_modalities: s.supported_input_modalities.clone(),
        explicit_input_modalities: s.explicit_input_modalities,
    }
}

fn from_static(info: &ModelInfo) -> ModelFacts {
    let text = |k: &str| {
        info.raw
            .get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    ModelFacts {
        id: info.id.clone(),
        kind: info.kind.clone(),
        owned_by: text("owned_by"),
        display_name: text("display_name"),
        description: text("description"),
        context_length: info
            .raw
            .get("context_length")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or_default(),
        thinking: info.thinking.clone(),
        input_modalities: info
            .raw
            .get("supportedInputModalities")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.iter().filter_map(|m| m.as_str().map(str::to_owned)).collect())
            .unwrap_or_default(),
        ..ModelFacts::default()
    }
}

impl codex_catalog::Registry for Facts<'_> {
    /// Go `LookupModelInfo`: registered models, then the static catalog.
    fn lookup(&self, id: &str, provider: Option<&str>) -> Option<ModelFacts> {
        let id = id.trim();
        if id.is_empty() {
            return None;
        }
        self.0
            .info(id, provider)
            .map(|s| from_spec(&s))
            .or_else(|| cpa_core::registry::pinned().lookup(id).map(from_static))
    }

    fn providers(&self, id: &str) -> Vec<String> {
        self.0.providers(id)
    }

    fn web_search(&self, id: &str) -> Option<bool> {
        self.0.responses_web_search(id)
    }
}
