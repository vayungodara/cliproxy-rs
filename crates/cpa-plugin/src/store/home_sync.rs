//! Home plugin sync payloads (internal/pluginstore/home_sync.go).

use std::collections::BTreeMap;

use super::auth::{ResolvedAuthConfig, validate_resolved_auth_config};
use super::manifest::Manifest;
use super::registry::normalize_install_plan;
use super::url;
use crate::go_struct;
use crate::gojson::GoTime;

pub const PLUGIN_SYNC_SCHEMA_VERSION: isize = 1;

go_struct! {
    pub struct PluginSyncRequest("pluginstore.PluginSyncRequest") {
        "schema_version" => schema_version: isize,
        "goos" => goos: String,
        "goarch" => goarch: String,
        "installed_versions" omitempty => installed_versions: BTreeMap<String, String>,
    }
}

go_struct! {
    pub struct PluginSyncItem("pluginstore.PluginSyncItem") {
        "manifest" => manifest: Manifest,
        "auth" omitempty => auth: Vec<ResolvedAuthConfig>,
    }
}

go_struct! {
    pub struct PluginSyncResponse("pluginstore.PluginSyncResponse") {
        "schema_version" => schema_version: isize,
        "expires_at" => expires_at: GoTime,
        "items" => items: Vec<PluginSyncItem>,
    }
}

impl PluginSyncResponse {
    /// Go `PluginSyncResponse.Validate`.
    pub fn validate(&self, now: chrono::DateTime<chrono::FixedOffset>) -> Result<(), String> {
        if self.schema_version != PLUGIN_SYNC_SCHEMA_VERSION {
            return Err(format!(
                "unsupported plugin sync schema_version {}",
                self.schema_version
            ));
        }
        let Some(expires_at) = self.expires_at.0 else {
            return Err("plugin sync response missing expires_at".into());
        };
        if now >= expires_at {
            return Err("plugin sync response expired".into());
        }
        let mut seen: Vec<&str> = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            item.manifest
                .validate()
                .map_err(|e| format!("plugin sync item {index}: {e}"))?;
            validate_manifest_urls(&item.manifest).map_err(|e| format!("plugin sync item {index}: {e}"))?;
            let id = item.manifest.id.trim();
            if seen.contains(&id) {
                return Err(format!(
                    "plugin sync response contains duplicate plugin {}",
                    cpa_common::gostr::quote(id)
                ));
            }
            seen.push(id);
            for (auth_index, auth) in item.auth.iter().enumerate() {
                validate_resolved_auth_config(auth)
                    .map_err(|e| format!("plugin sync item {index} auth {auth_index}: {e}"))?;
            }
        }
        Ok(())
    }
}

/// Go `validatePluginSyncManifestURLs`: synced direct installs carry pinned HTTPS
/// artifacts.
fn validate_manifest_urls(manifest: &Manifest) -> Result<(), String> {
    if manifest.install_type() != super::registry::INSTALL_TYPE_DIRECT {
        return Ok(());
    }
    let plan = normalize_install_plan(&manifest.install);
    if plan.artifacts.is_empty() {
        return Err("direct plugin sync manifest requires pinned artifacts".into());
    }
    for (index, artifact) in plan.artifacts.iter().enumerate() {
        if !url::parse(artifact.url.trim()).is_some_and(|u| u.scheme.eq_ignore_ascii_case("https")) {
            return Err(format!("direct plugin sync artifact {index} must use https"));
        }
    }
    Ok(())
}
