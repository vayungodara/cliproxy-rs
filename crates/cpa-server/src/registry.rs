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
    use super::{Registry, Suspension};
    use cpa_core::config::Config;
    use cpa_core::credential::Credential;
    use serde_json::Value;
    use std::path::Path;
    use std::sync::Arc;

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
