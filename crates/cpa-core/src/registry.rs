//! Pinned static model catalog: `internal/registry/models/models.json` from CLIProxyAPI
//! 6fecc6e, embedded verbatim.
//!
//! This is the static layer only. Go overlays a dynamic registry (per-credential
//! registrations, config model definitions, remote catalog refresh) and consults it before
//! these definitions (internal/registry/model_registry.go); that overlay belongs to the
//! server runtime.

use std::sync::LazyLock;

pub mod dynamic;

use serde::Deserialize;
use serde_json::{Map, Value};

const MODELS_JSON: &str = include_str!("registry/models.json");

/// Channels searched by [`Catalog::lookup`], in Go's `LookupStaticModelInfo` order
/// (internal/registry/model_definitions.go). `gemini-cli`, `codex-free`, `codex-team` and
/// `codex-plus` are not searched; Go reaches them only through per-channel getters.
// ponytail: Go also searches the Devin catalog (devin_models.json plus staticDevinModels)
// between xai and meta. Add it with the Devin provider port.
const LOOKUP_ORDER: [&str; 9] = [
    "claude",
    "gemini",
    "vertex",
    "aistudio",
    "codex-pro",
    "kimi",
    "antigravity",
    "xai",
    "meta",
];

/// Thinking budget and level support (`ThinkingSupport` in model_registry.go).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ThinkingSupport {
    pub min: i64,
    pub max: i64,
    pub zero_allowed: bool,
    pub dynamic_allowed: bool,
    /// Discrete effort levels; empty means budget-based thinking.
    pub levels: Vec<String>,
}

/// One model definition. Typed fields cover what routing and translation read; `raw` keeps
/// the full published object in file key order for handlers that render Go's shapes.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    /// Go's `type` field: the provider family that serves the model.
    pub kind: String,
    pub thinking: Option<ThinkingSupport>,
    pub raw: Map<String, Value>,
}

impl ModelInfo {
    /// Whether the resolved model is a config entry marked `is-compat` (Go
    /// `ModelInfo.IsCompat`). Only registry-resolved models carry it.
    pub fn is_compat(&self) -> bool {
        self.raw.get("is_compat").and_then(Value::as_bool).unwrap_or(false)
    }

    pub fn from_raw(raw: Map<String, Value>) -> Result<Self, serde_json::Error> {
        #[derive(Deserialize)]
        struct Typed {
            id: String,
            #[serde(default, rename = "type")]
            kind: String,
            #[serde(default)]
            thinking: Option<ThinkingSupport>,
        }
        let typed: Typed = serde_json::from_value(Value::Object(raw.clone()))?;
        Ok(Self {
            id: typed.id,
            kind: typed.kind,
            thinking: typed.thinking,
            raw,
        })
    }
}

pub struct Catalog {
    /// Channels in file order.
    channels: Vec<(String, Vec<ModelInfo>)>,
}

impl Catalog {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        let root: Map<String, Value> = serde_json::from_str(json)?;
        let mut channels = Vec::with_capacity(root.len());
        for (name, models) in root {
            let models: Vec<Map<String, Value>> = serde_json::from_value(models)?;
            let models = models.into_iter().map(ModelInfo::from_raw).collect::<Result<_, _>>()?;
            channels.push((name, models));
        }
        Ok(Self { channels })
    }

    /// Definitions for one channel (`claude`, `codex-plus`, ...), empty when unknown.
    pub fn channel(&self, name: &str) -> &[ModelInfo] {
        self.channels
            .iter()
            .find(|(n, _)| n == name)
            .map_or(&[], |(_, models)| models.as_slice())
    }

    /// First definition with this exact ID across channels, in Go's search order. The same
    /// ID can mean different things per channel (`claude-sonnet-4-6` is also an
    /// Antigravity model), so callers that know the provider should use [`Self::channel`].
    pub fn lookup(&self, id: &str) -> Option<&ModelInfo> {
        if id.is_empty() {
            return None;
        }
        LOOKUP_ORDER
            .iter()
            .flat_map(|channel| self.channel(channel))
            .find(|m| m.id == id)
    }
}

/// The catalog embedded at build time.
pub fn pinned() -> &'static Catalog {
    static PINNED: LazyLock<Catalog> =
        LazyLock::new(|| Catalog::parse(MODELS_JSON).expect("embedded models.json is valid"));
    &PINNED
}

/// The server's dynamic registry, consulted before the pinned catalog.
pub trait Overlay: Send + Sync {
    /// A registered model by public ID, preferring `provider`'s registration.
    fn lookup(&self, id: &str, provider: Option<&str>) -> Option<ModelInfo>;
    /// The model one credential registered under `model` (config model entries carry
    /// display names, context limits, thinking and `is_compat`).
    fn for_credential(&self, credential_id: &str, model: &str) -> Option<ModelInfo>;
    /// Go `GetAvailableModelsByProvider`: the models `provider`'s credentials currently
    /// serve (translators read Antigravity's `supports_web_search` from them). Defaults
    /// to none.
    fn available_by_provider(&self, _provider: &str) -> Vec<ModelInfo> {
        Vec::new()
    }
}

static OVERLAY: std::sync::RwLock<Option<std::sync::Arc<dyn Overlay>>> = std::sync::RwLock::new(None);

/// Installs (or removes) the dynamic registry. Wired once at startup.
pub fn install_overlay(overlay: Option<std::sync::Arc<dyn Overlay>>) {
    *OVERLAY.write().unwrap_or_else(std::sync::PoisonError::into_inner) = overlay;
}

fn overlay() -> Option<std::sync::Arc<dyn Overlay>> {
    OVERLAY
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Go `registry.LookupModelInfo`: registered models first, then the pinned catalog.
pub fn lookup_model(id: &str, provider: Option<&str>) -> Option<ModelInfo> {
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    overlay()
        .and_then(|o| o.lookup(id, provider))
        .or_else(|| pinned().lookup(id).cloned())
}

/// Go `GetGlobalRegistry().GetAvailableModelsByProvider(provider)`; empty without an
/// installed registry.
pub fn available_models_by_provider(provider: &str) -> Vec<ModelInfo> {
    overlay().map(|o| o.available_by_provider(provider)).unwrap_or_default()
}

/// The model info resolved for one credential and model (Go attaches this to the
/// execution request), when the registry is installed and the credential registered it.
pub fn credential_model(credential_id: &str, model: &str) -> Option<ModelInfo> {
    overlay().and_then(|o| o.for_credential(credential_id, model))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_matches_go_channel_sizes() {
        let catalog = pinned();
        let sizes: Vec<(&str, usize)> = catalog.channels.iter().map(|(n, m)| (n.as_str(), m.len())).collect();
        assert_eq!(
            sizes,
            [
                ("claude", 18),
                ("gemini", 14),
                ("vertex", 21),
                ("gemini-cli", 7),
                ("aistudio", 16),
                ("codex-free", 5),
                ("codex-team", 9),
                ("codex-plus", 9),
                ("codex-pro", 9),
                ("kimi", 10),
                ("antigravity", 12),
                ("xai", 12),
                ("meta", 5),
            ]
        );
    }

    #[test]
    fn lookup_follows_go_search_order_across_providers() {
        let catalog = pinned();
        // Present in claude and antigravity: the claude definition wins.
        assert_eq!(catalog.lookup("claude-sonnet-4-6").unwrap().kind, "claude");
        // Only in vertex and antigravity: vertex is searched first.
        assert_eq!(catalog.lookup("gemini-3-flash").unwrap().kind, "gemini");
        // Non-Claude capabilities are visible to Claude-bound translation (Go does the same).
        let kimi = catalog.lookup("kimi-k2.5").unwrap().thinking.as_ref().unwrap();
        assert_eq!(kimi.levels, ["low", "high"]);
        assert!(kimi.zero_allowed);
        assert!(catalog.lookup("").is_none());
        assert!(catalog.lookup("no-such-model").is_none());
    }

    #[test]
    fn raw_definition_keeps_published_key_order() {
        let haiku = &pinned().channel("claude")[0];
        let keys: Vec<&str> = haiku.raw.keys().map(String::as_str).collect();
        assert_eq!(&keys[..4], ["id", "object", "created", "owned_by"]);
        assert_eq!(haiku.thinking.as_ref().unwrap().min, 1024);
    }
}
