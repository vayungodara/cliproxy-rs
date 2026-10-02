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
}
