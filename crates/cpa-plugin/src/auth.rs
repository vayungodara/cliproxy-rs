//! Auth provider and frontend auth capabilities (internal/pluginhost/auth_provider.go,
//! adapters_auth.go).
//!
//! A plugin auth is a [`PluginAuth`]: Go's `coreauth.Auth` fields that plugins set, with
//! its storage (`pluginTokenStorage`) as raw JSON merged with metadata on save. The
//! server maps it onto its credential store.

use std::collections::BTreeMap;

use bytes::Bytes;
use serde_json::Value;

use crate::abi::method;
use crate::api::{
    AuthData, AuthLoginPollRequest, AuthLoginPollResponse, AuthLoginStartRequest, AuthLoginStartResponse,
    AuthParseRequest, AuthParseResponse, AuthRefreshRequest, AuthRefreshResponse, FrontendAuthRequest,
    FrontendAuthResponse, HostConfigSummary, ModelAlias,
};
use crate::callbacks::RequestScope;
use crate::gojson::{GoTime, Metadata, StringMap};
use crate::host::{Host, Record, normalize_provider};
use crate::rpc::CallError;

pub const ATTRIBUTE_PATH: &str = "path";
pub const ATTRIBUTE_SOURCE: &str = "source";
pub const ATTRIBUTE_SOURCE_BACKEND: &str = "source_backend";
pub const ATTRIBUTE_FILE_PRIORITY: &str = "file_priority";
pub const AUTH_SOURCE_FILE: &str = "file";

/// Go `coreauth.Auth` as a plugin produces it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginAuth {
    pub provider: String,
    pub id: String,
    pub file_name: String,
    pub label: String,
    pub prefix: String,
    pub proxy_url: String,
    pub disabled: bool,
    /// The plugin's raw storage JSON (`pluginTokenStorage.rawJSON`).
    pub storage_json: Bytes,
    /// Host-managed metadata; always carries `type`.
    pub metadata: Metadata,
    pub attributes: StringMap,
    pub next_refresh_after: GoTime,
}

/// Go `CanonicalCredentialMetadataKey`.
pub fn canonical_metadata_key(key: &str) -> &str {
    match key {
        "api-key" => "api_key",
        "base-url" => "base_url",
        "disable-cooling" => "disable_cooling",
        "excluded-models" => "excluded_models",
        "fingerprint-profile" => "fingerprint_profile",
        "model-aliases" => "model_aliases",
        "proxy-url" => "proxy_url",
        "request-retry" => "request_retry",
        "request-scoped-errors" => "request_scoped_errors",
        "tool-prefix-disabled" => "tool_prefix_disabled",
        other => other,
    }
}

/// Go `NormalizeCredentialMetadata`: legacy keys move to their canonical name unless
/// the canonical one is already present.
pub fn normalize_credential_metadata(metadata: &mut serde_json::Map<String, Value>) {
    let legacy: Vec<String> = metadata
        .keys()
        .filter(|k| canonical_metadata_key(k) != k.as_str())
        .cloned()
        .collect();
    for key in legacy {
        let value = metadata.remove(&key).expect("listed above");
        metadata.entry(canonical_metadata_key(&key).to_owned()).or_insert(value);
    }
}

/// Go `authIDForPath`: relative to the auth dir when inside it, slash-separated.
pub fn auth_id_for_path(path: &str, auth_dir: &str) -> String {
    let path = path.trim();
    if path.is_empty() {
        return String::new();
    }
    let mut id = std::path::PathBuf::from(path);
    let auth_dir = auth_dir.trim();
    if !auth_dir.is_empty()
        && let Ok(rel) = std::path::Path::new(path).strip_prefix(crate::platform::clean(std::path::Path::new(auth_dir)))
        && !rel.as_os_str().is_empty()
    {
        id = rel.to_owned();
    }
    let id = crate::platform::clean(&id).to_string_lossy().replace('\\', "/");
    if cfg!(windows) { id.to_lowercase() } else { id }
}

fn first_non_empty(values: &[&str]) -> String {
    values
        .iter()
        .map(|v| v.trim())
        .find(|v| !v.is_empty())
        .unwrap_or_default()
        .to_owned()
}

impl PluginAuth {
    /// Go `pluginAuthDataToCoreAuth`. `None` without a provider.
    pub fn from_auth_data(data: AuthData, path: &str, file_name: &str, auth_dir: &str) -> Option<Self> {
        let provider = normalize_provider(&data.provider);
        if provider.is_empty() {
            return None;
        }
        let mut metadata = data.metadata;
        metadata.insert("type".into(), provider.clone().into());
        let mut attributes = data.attributes;
        let path = path.trim();
        if !path.is_empty() {
            for (key, value) in [
                (ATTRIBUTE_PATH, path),
                (ATTRIBUTE_SOURCE, path),
                (ATTRIBUTE_SOURCE_BACKEND, AUTH_SOURCE_FILE),
            ] {
                if attributes.get(key).is_none_or(|v| v.is_empty()) {
                    attributes.insert(key.into(), value.into());
                }
            }
        }
        let file_name = first_non_empty(&[&data.file_name, file_name]);
        if !file_name.is_empty() && attributes.get(ATTRIBUTE_SOURCE).is_none_or(|v| v.is_empty()) {
            attributes.insert(ATTRIBUTE_SOURCE.into(), file_name.clone());
        }
        let mut id = data.id.trim().to_owned();
        if id.is_empty() {
            id = auth_id_for_path(&first_non_empty(&[path, &file_name]), auth_dir);
        }
        Some(Self {
            provider,
            id,
            file_name,
            label: data.label.trim().to_owned(),
            prefix: data.prefix.trim().to_owned(),
            proxy_url: data.proxy_url.trim().to_owned(),
            disabled: data.disabled,
            storage_json: data.storage_json,
            metadata,
            attributes,
            next_refresh_after: data.next_refresh_after,
        })
    }

    /// Go `mergedStorageJSON` (`pluginTokenStorage.RawJSON` / `SaveTokenToFile`): the raw
    /// storage object, overlaid with metadata, `type` set, legacy keys normalized,
    /// encoded with sorted keys.
    pub fn storage_payload(&self) -> Result<Bytes, String> {
        let mut out = serde_json::Map::new();
        if !self.storage_json.trim_ascii().is_empty() {
            match crate::gojson::parse(&self.storage_json).and_then(|n| n.to_value()) {
                Ok(Value::Object(map)) => out = map,
                Ok(Value::Null) => {}
                Ok(_) => return Err("decode plugin token storage: json: cannot unmarshal into map".into()),
                Err(e) => return Err(format!("decode plugin token storage: {e}")),
            }
        }
        for (key, value) in &self.metadata {
            out.insert(key.clone(), value.clone());
        }
        let provider = normalize_provider(&self.provider);
        if !provider.is_empty() {
            out.insert("type".into(), provider.into());
        }
        normalize_credential_metadata(&mut out);
        if out.is_empty() {
            return Err("plugin token storage payload is empty".into());
        }
        let mut payload = Vec::new();
        crate::gojson::encode_any(&Value::Object(out), &mut payload);
        Ok(Bytes::from(payload))
    }
}

impl PluginAuth {
    /// The credential as the host reads it back: Go `storageJSONFromAuth` takes the
    /// storage's `RawJSON`, which is [`PluginAuth::storage_payload`] (empty on error).
    pub fn view(&self) -> AuthView {
        AuthView {
            id: self.id.clone(),
            provider: self.provider.clone(),
            file_name: self.file_name.clone(),
            label: self.label.clone(),
            prefix: self.prefix.clone(),
            proxy_url: self.proxy_url.clone(),
            storage_json: self.storage_payload().unwrap_or_default(),
            metadata: self.metadata.clone(),
            attributes: self.attributes.clone(),
            next_refresh_after: self.next_refresh_after,
        }
    }
}

/// An existing credential as the plugin sees it (Go `coreauth.Auth` read side).
#[derive(Debug, Clone, Default)]
pub struct AuthView {
    pub id: String,
    pub provider: String,
    pub file_name: String,
    pub label: String,
    pub prefix: String,
    pub proxy_url: String,
    /// Go `storageJSONFromAuth`.
    pub storage_json: Bytes,
    pub metadata: Metadata,
    pub attributes: StringMap,
    pub next_refresh_after: GoTime,
}

/// Go `preserveFileAuthPriority`.
fn preserve_file_auth_priority(data: &mut AuthData, auth: &AuthView) {
    if auth.attributes.get(ATTRIBUTE_SOURCE_BACKEND).map(String::as_str) != Some(AUTH_SOURCE_FILE) {
        return;
    }
    for key in [
        ATTRIBUTE_PATH,
        ATTRIBUTE_SOURCE,
        ATTRIBUTE_SOURCE_BACKEND,
        ATTRIBUTE_FILE_PRIORITY,
    ] {
        if let Some(value) = auth.attributes.get(key) {
            data.attributes.insert(key.into(), value.clone());
        }
    }
    if auth.attributes.get(ATTRIBUTE_FILE_PRIORITY).map(String::as_str) != Some("true") {
        data.attributes.remove(ATTRIBUTE_FILE_PRIORITY);
        return;
    }
    match auth.attributes.get("priority") {
        Some(p) => data.attributes.insert("priority".into(), p.clone()),
        None => data.attributes.remove("priority"),
    };
    match auth.metadata.get("priority") {
        Some(p) => data.metadata.insert("priority".into(), p.clone()),
        None => data.metadata.remove("priority"),
    };
}

/// Go `RefreshAuth`'s defaults: the refreshed record inherits what the plugin left out.
pub fn refreshed_auth_data(resp: AuthRefreshResponse, auth: &AuthView) -> AuthData {
    let mut data = resp.auth;
    let fill = |v: &mut String, from: &str| {
        if v.trim().is_empty() {
            *v = from.to_owned();
        }
    };
    fill(&mut data.provider, &auth.provider);
    fill(&mut data.id, &auth.id);
    fill(&mut data.file_name, &auth.file_name);
    fill(&mut data.label, &auth.label);
    fill(&mut data.prefix, &auth.prefix);
    fill(&mut data.proxy_url, &auth.proxy_url);
    if data.metadata.is_empty() {
        data.metadata = auth.metadata.clone();
    }
    if data.attributes.is_empty() {
        data.attributes = auth.attributes.clone();
    } else {
        for (k, v) in &auth.attributes {
            data.attributes.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    preserve_file_auth_priority(&mut data, auth);
    if data.storage_json.is_empty() {
        data.storage_json = auth.storage_json.clone();
    }
    data.next_refresh_after = if resp.next_refresh_after.is_zero() {
        auth.next_refresh_after
    } else {
        resp.next_refresh_after
    };
    data
}

/// Go `pluginOAuthModelAliases`.
fn plugin_oauth_aliases(cfg: &cpa_core::config::Config) -> BTreeMap<String, Vec<ModelAlias>> {
    let mut out: BTreeMap<String, Vec<ModelAlias>> = BTreeMap::new();
    for (provider, aliases) in cpa_core::registry::dynamic::global_aliases(cfg) {
        let key = normalize_provider(&provider);
        if key.is_empty() {
            continue;
        }
        for alias in aliases {
            let (name, value) = (alias.name.trim(), alias.alias.trim());
            if !name.is_empty() && !value.is_empty() {
                out.entry(key.clone()).or_default().push(ModelAlias {
                    name: name.into(),
                    alias: value.into(),
                });
            }
        }
    }
    out
}

impl Host {
    /// Go `hostConfigSummary`.
    pub fn host_config_summary(&self) -> HostConfigSummary {
        let Some(cfg) = self.config() else {
            return HostConfigSummary::default();
        };
        let proxy_url = cfg
            .document
            .get("requests")
            .and_then(|r| r.get("proxy-url"))
            .map(crate::config::yaml_string)
            .unwrap_or_default();
        HostConfigSummary {
            auth_dir: cfg.auth_dir.to_string_lossy().trim().to_owned(),
            proxy_url: proxy_url.trim().to_owned(),
            force_model_prefix: cfg.routing.force_model_prefix,
            oauth_model_alias: plugin_oauth_aliases(&cfg),
            excluded_models: cpa_core::config::credentials::oauth_excluded(&cfg)
                .into_iter()
                .filter_map(|(k, v)| {
                    let k = normalize_provider(&k);
                    (!k.is_empty()).then_some((k, v))
                })
                .collect(),
        }
    }

    /// Go `AuthProviderIdentifiers`.
    pub fn auth_provider_identifiers(&self) -> Vec<String> {
        self.active_records()
            .into_iter()
            .filter(|r| r.plugin.caps.auth_provider && !self.is_fused(&r.id))
            .map(|r| normalize_provider(&r.plugin.auth_identifier))
            .filter(|id| !id.is_empty())
            .collect()
    }

    /// Go `authProviderRecord`: the first active auth provider for `provider`.
    pub fn auth_provider_record(&self, provider: &str) -> Option<Record> {
        let provider = normalize_provider(provider);
        if provider.is_empty() {
            return None;
        }
        self.active_records().into_iter().find(|r| {
            r.plugin.caps.auth_provider
                && !self.is_fused(&r.id)
                && normalize_provider(&r.plugin.auth_identifier) == provider
        })
    }

    /// Go `HasAuthProvider`.
    pub fn has_auth_provider(&self, provider: &str) -> bool {
        self.auth_provider_record(provider).is_some()
    }

    /// Go `ParseAuths`: `Ok(None)` when no plugin handled the material. With a provider,
    /// only that provider's plugin is asked; otherwise every auth provider in order until
    /// one handles it or fails.
    pub async fn parse_auths(&self, req: AuthParseRequest) -> Result<Option<Vec<PluginAuth>>, CallError> {
        if !req.provider.trim().is_empty() {
            let Some(record) = self.auth_provider_record(&req.provider) else {
                return Ok(None);
            };
            return self.call_parse_auths(&record, req).await;
        }
        for record in self.active_records() {
            if !record.plugin.caps.auth_provider || self.is_fused(&record.id) {
                continue;
            }
            match self.call_parse_auths(&record, req.clone()).await {
                Ok(None) => continue,
                other => return other,
            }
        }
        Ok(None)
    }

    async fn call_parse_auths(
        &self,
        record: &Record,
        mut req: AuthParseRequest,
    ) -> Result<Option<Vec<PluginAuth>>, CallError> {
        if !self.record_current(record) {
            return Ok(None);
        }
        if req.host.auth_dir.is_empty() {
            req.host = self.host_config_summary();
        }
        req.provider = normalize_provider(&req.provider);
        let identifier = normalize_provider(&record.plugin.auth_identifier);
        if req.provider.is_empty() {
            req.provider = identifier.clone();
        }
        let resp: AuthParseResponse = self.call(record, method::AUTH_PARSE, &req).await?;
        if !resp.handled {
            return Ok(None);
        }
        let datas = if resp.auths.is_empty() {
            vec![resp.auth]
        } else {
            resp.auths
        };
        let auth_dir = self.host_config_summary().auth_dir;
        let mut auths = Vec::with_capacity(datas.len());
        for mut data in datas {
            if data.provider.trim().is_empty() {
                data.provider = req.provider.clone();
            }
            if data.provider.trim().is_empty() {
                data.provider = identifier.clone();
            }
            if normalize_provider(&data.provider).is_empty() {
                return Err(CallError::Other(format!(
                    "auth provider {} returned auth without provider",
                    record.id
                )));
            }
            let parsed = PluginAuth::from_auth_data(data, &req.path, &req.file_name, &auth_dir)
                .ok_or_else(|| CallError::Other(format!("auth provider {} returned invalid auth data", record.id)))?;
            auths.push(parsed);
        }
        Ok(Some(auths))
    }

    /// Go `StartLogin`: `Ok(None)` when no plugin owns `provider`.
    pub async fn start_login(
        &self,
        provider: &str,
        base_url: &str,
        metadata: Metadata,
        scope: &RequestScope,
    ) -> Result<Option<AuthLoginStartResponse>, CallError> {
        let Some(record) = self.auth_provider_record(provider) else {
            return Ok(None);
        };
        if !self.record_current(&record) {
            return Ok(None);
        }
        let req = AuthLoginStartRequest {
            provider: normalize_provider(provider),
            base_url: base_url.trim().to_owned(),
            host: self.host_config_summary(),
            metadata,
        };
        self.call_with_callback(&record, method::AUTH_LOGIN_START, &req, scope)
            .await
            .map(Some)
    }

    /// Go `PollLogin`.
    pub async fn poll_login(
        &self,
        provider: &str,
        state: &str,
        metadata: Metadata,
        scope: &RequestScope,
    ) -> Result<Option<AuthLoginPollResponse>, CallError> {
        let Some(record) = self.auth_provider_record(provider) else {
            return Ok(None);
        };
        if !self.record_current(&record) {
            return Ok(None);
        }
        let req = AuthLoginPollRequest {
            provider: normalize_provider(provider),
            state: state.trim().to_owned(),
            host: self.host_config_summary(),
            metadata,
        };
        self.call_with_callback(&record, method::AUTH_LOGIN_POLL, &req, scope)
            .await
            .map(Some)
    }

    /// Go `RefreshAuth`: `Ok(None)` when no plugin owns the credential's provider.
    pub async fn refresh_auth(&self, auth: &AuthView, scope: &RequestScope) -> Result<Option<PluginAuth>, CallError> {
        let Some(record) = self.auth_provider_record(&auth.provider) else {
            return Ok(None);
        };
        if !self.record_current(&record) {
            return Ok(None);
        }
        let req = AuthRefreshRequest {
            auth_id: auth.id.clone(),
            auth_provider: auth.provider.clone(),
            storage_json: auth.storage_json.clone(),
            metadata: auth.metadata.clone(),
            attributes: auth.attributes.clone(),
            host: self.host_config_summary(),
        };
        let resp: AuthRefreshResponse = self
            .call_with_callback(&record, method::AUTH_REFRESH, &req, scope)
            .await?;
        let data = refreshed_auth_data(resp, auth);
        let path = auth.attributes.get(ATTRIBUTE_PATH).cloned().unwrap_or_default();
        let file_name = data.file_name.clone();
        PluginAuth::from_auth_data(data, &path, &file_name, &self.host_config_summary().auth_dir)
            .map(Some)
            .ok_or_else(|| CallError::Other("auth provider refresh returned invalid auth data".into()))
    }

    /// Go `RegisterFrontendAuthProviders`: `(access provider key, plugin ID)` for each
    /// active frontend auth provider in order, and the exclusive key (highest priority,
    /// then lowest ID) if any. Keys are `plugin:<id>:<identifier>`; identifiers are asked
    /// on every call, as Go does.
    pub async fn frontend_auth_providers(&self) -> (Vec<(String, String)>, Option<String>) {
        let mut providers = Vec::new();
        let mut exclusive: Option<(String, String, i64)> = None;
        for record in self.active_records() {
            if !record.plugin.caps.frontend_auth_provider || self.is_fused(&record.id) {
                continue;
            }
            let Some(key) = self.frontend_auth_key(&record).await else {
                continue;
            };
            if record.plugin.caps.frontend_auth_provider_exclusive {
                let better = match &exclusive {
                    None => true,
                    Some((_, id, priority)) => {
                        record.priority > *priority || (record.priority == *priority && record.id < *id)
                    }
                };
                if better {
                    exclusive = Some((key.clone(), record.id.clone(), record.priority));
                }
            }
            providers.push((key, record.id.clone()));
        }
        (providers, exclusive.map(|(key, _, _)| key))
    }

    /// Go `accessAdapter.Identifier` for a plugin: `plugin:<id>:<identifier>`, asking the
    /// plugin each time. `None` when the plugin is gone or answers empty.
    pub async fn frontend_auth_identifier(&self, plugin_id: &str) -> Option<String> {
        let record = self.record(plugin_id)?;
        if !record.plugin.caps.frontend_auth_provider {
            return None;
        }
        self.frontend_auth_key(&record).await
    }

    /// Go `accessAdapter.Identifier`.
    async fn frontend_auth_key(&self, record: &Record) -> Option<String> {
        let identifier = crate::rpc::identifier(&record.client, method::FRONTEND_AUTH_IDENTIFIER).await;
        let id = record.id.trim();
        (!id.is_empty() && !identifier.is_empty()).then(|| format!("plugin:{id}:{identifier}"))
    }

    /// Go `accessAdapter.Authenticate`: `None` is "not handled" (unavailable plugin, RPC
    /// failure, or not authenticated); the caller tries the next provider.
    pub async fn frontend_authenticate(
        &self,
        plugin_id: &str,
        req: &FrontendAuthRequest,
    ) -> Option<FrontendAuthOutcome> {
        let record = self.record(plugin_id)?;
        if !self.record_current(&record) {
            return None;
        }
        let resp: FrontendAuthResponse = self.call(&record, method::FRONTEND_AUTH_AUTHENTICATE, req).await.ok()?;
        if !resp.authenticated {
            return None;
        }
        let provider = self.frontend_auth_key(&record).await?;
        Some(FrontendAuthOutcome {
            provider,
            principal: resp.principal,
            metadata: resp.metadata,
        })
    }
}

/// A frontend request a plugin accepted (Go `sdkaccess.Result`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontendAuthOutcome {
    pub provider: String,
    pub principal: String,
    pub metadata: StringMap,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go `pluginAuthDataToCoreAuth` and `mergedStorageJSON`.
    #[test]
    fn auth_data_conversion_and_storage_follow_go() {
        let data = AuthData {
            provider: " Plugin-X ".into(),
            storage_json: Bytes::from_static(br#"{"token":"t","priority":1,"base-url":"old"}"#),
            metadata: [
                ("base-url".to_owned(), Value::from("u")),
                ("email".to_owned(), Value::from("e")),
            ]
            .into(),
            ..Default::default()
        };
        let auth = PluginAuth::from_auth_data(data, "/auth/sub/a.json", "", "/auth").unwrap();
        assert_eq!(auth.provider, "plugin-x");
        assert_eq!(auth.id, "sub/a.json");
        assert_eq!(auth.attributes["source_backend"], "file");
        assert_eq!(auth.attributes["source"], "/auth/sub/a.json");
        assert_eq!(auth.metadata["type"], "plugin-x");
        let payload = auth.storage_payload().unwrap();
        assert_eq!(
            &payload[..],
            br#"{"base_url":"u","email":"e","priority":1,"token":"t","type":"plugin-x"}"#
        );
        assert!(PluginAuth::from_auth_data(AuthData::default(), "", "", "").is_none());
        assert_eq!(auth_id_for_path("/elsewhere/x.json", "/auth"), "/elsewhere/x.json");
    }
}
