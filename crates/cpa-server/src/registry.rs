//! The dynamic model registry lives in `cpa_core::registry::dynamic` so translators and
//! shared helpers can read capabilities; this module publishes the runtime's copy.

use cpa_core::registry::ModelInfo;
pub use cpa_core::registry::dynamic::*;

/// Exposes the runtime's registry through `cpa_core::registry::lookup_model`.
pub struct Overlay(pub std::sync::Weak<crate::Runtime>);

impl cpa_core::registry::Overlay for Overlay {
    fn lookup(&self, id: &str, provider: Option<&str>) -> Option<ModelInfo> {
        self.0.upgrade()?.registry().info(id, provider).map(|s| s.to_info())
    }

    fn for_credential(&self, credential_id: &str, model: &str) -> Option<ModelInfo> {
        self.0
            .upgrade()?
            .registry()
            .client_model(credential_id, model)
            .map(|s| s.to_info())
    }

    fn available_by_provider(&self, provider: &str) -> Vec<ModelInfo> {
        let Some(rt) = self.0.upgrade() else {
            return Vec::new();
        };
        rt.registry()
            .available_by_provider(provider, |client, model| rt.suspension(client, model))
            .iter()
            .map(|s| s.to_info())
            .collect()
    }

    fn model_providers(&self, model: &str) -> Vec<String> {
        self.0
            .upgrade()
            .map(|rt| rt.registry().model_providers(model))
            .unwrap_or_default()
    }

    fn available(&self) -> Vec<ModelInfo> {
        let Some(rt) = self.0.upgrade() else {
            return Vec::new();
        };
        let registry = rt.registry();
        registry
            .available_with(|c, m| rt.suspension(c, m))
            .map(|s| s.to_info())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{Registry, Spec, Suspension};
    use cpa_core::config::Config;
    use cpa_core::credential::Credential;
    use serde_json::Value;
    use std::path::Path;
    use std::sync::Arc;

    fn golden() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/server_go.json")).unwrap()
    }

    /// Go `Auth.AuthKind` goldens (`authKinds`).
    #[test]
    fn auth_kind_matches_go() {
        let fixture = golden();
        let cases = fixture["auth_kind"].as_array().unwrap();
        assert_eq!(cases.len(), 16);
        for case in cases {
            let meta = serde_json::json!({"type": "x"}).as_object().unwrap().clone();
            let mut c = Credential::from_file(Path::new("/a"), Path::new("/a/x.json"), meta).unwrap();
            c.metadata.clear();
            if let Some(attrs) = case["attributes"].as_object() {
                c.attributes = attrs
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                    .collect();
            }
            if let Some(meta) = case["metadata"].as_object() {
                c.metadata = meta.clone();
            }
            let want = case["kind"].as_str().unwrap();
            assert_eq!(super::auth_kind(&c).unwrap_or(""), want, "case {case}");
            assert_eq!(super::is_api_key(&c), want == "apikey");
        }
    }

    /// Go `GetAvailableModelsByProvider` and the `SupportsWebSearch` aggregation in
    /// `GetModelInfo` (`byProvider`): only the provider's credentials count, the
    /// listed info is the credential's own, lookups OR every registration's flag.
    #[test]
    fn available_by_provider_matches_go() {
        let fixture = golden();
        let cases = fixture["by_provider"].as_array().unwrap();
        assert_eq!(cases.len(), 9);
        for (i, case) in cases.iter().enumerate() {
            let clients = case["clients"].as_array().unwrap();
            let mut registry = Registry::default();
            for (j, cl) in clients.iter().enumerate() {
                let spec = Spec {
                    id: format!("bp-model-{i}"),
                    supports_web_search: cl["search"].as_bool().unwrap(),
                    ..Spec::default()
                };
                registry.register(&format!("c{i}-{j}"), cl["provider"].as_str().unwrap(), vec![spec]);
            }
            let state = |client: &str, _: &str| {
                let j: usize = client.rsplit('-').next().unwrap().parse().unwrap();
                match clients[j]["state"].as_str().unwrap() {
                    "none" => Suspension::None,
                    "quota" => Suspension::Quota,
                    "other" => Suspension::Other { quota_exceeded: false },
                    "other_qe" => Suspension::Other { quota_exceeded: true },
                    s => panic!("unknown state {s}"),
                }
            };
            let id = format!("bp-model-{i}");
            let listed = registry.available_by_provider(" GOLDEN-AG ", state);
            assert_eq!(listed.len(), usize::from(case["listed"].as_bool().unwrap()), "case {i}");
            if let (Some(spec), Some(want)) = (listed.first(), case["listed_search"].as_bool()) {
                assert_eq!(spec.supports_web_search, want, "case {i} listed flag");
            }
            let info = registry.info(&id, None).unwrap();
            assert_eq!(
                info.supports_web_search,
                case["info_search"].as_bool().unwrap(),
                "case {i}"
            );
            let ag = registry.info(&id, Some("golden-ag")).unwrap();
            assert_eq!(ag.supports_web_search, case["ag_search"].as_bool().unwrap(), "case {i}");
        }
    }

    /// Go `modelRegistrationAvailability` through the real registry: goldens from
    /// tests/reference/server/main.go (`ApplyClientModelProjections` + `GetAvailableModels`).
    #[test]
    fn availability_matches_go_projections() {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/server_go.json")).unwrap();
        let cases = fixture["availability"].as_array().unwrap();
        assert_eq!(cases.len(), 15);
        let credentials: Vec<Arc<Credential>> = (0..3)
            .map(|i| {
                let metadata = serde_json::json!({"type": "claude"}).as_object().unwrap().clone();
                let path = format!("/auth/{i}.json");
                Arc::new(Credential::from_file(Path::new("/auth"), Path::new(&path), metadata).unwrap())
            })
            .collect();
        let model = "claude-sonnet-4-6";
        for case in cases {
            let states: Vec<Suspension> = case["states"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| match s.as_str().unwrap() {
                    "none" => Suspension::None,
                    "quota" => Suspension::Quota,
                    "other" => Suspension::Other { quota_exceeded: false },
                    "other_qe" => Suspension::Other { quota_exceeded: true },
                    other => panic!("unknown state {other}"),
                })
                .collect();
            let registry = Registry::build(&Config::default(), &credentials[..states.len()]);
            let ids: Vec<String> = credentials[..states.len()].iter().map(|c| c.id.clone()).collect();
            let listed = registry
                .available_with(|client, m| match ids.iter().position(|id| id == client) {
                    Some(i) if m == model => states[i],
                    _ => Suspension::None,
                })
                .any(|spec| spec.id == model);
            assert_eq!(
                listed,
                case["available"].as_bool().unwrap(),
                "states {}",
                case["states"]
            );
        }
    }
}
