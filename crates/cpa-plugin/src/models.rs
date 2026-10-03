//! Model registration and executor ownership (internal/pluginhost/adapters.go
//! `RegisterModels`/`ModelsForAuth`, adapters_executors.go `RegisterExecutors`).
//!
//! The host decides which plugin models and executor providers exist; the server applies
//! the returned changes to its model registry and executor table.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::abi::method;
use crate::api::{
    AuthModelRequest, ModelInfo, ModelRegistrationRequest, ModelRegistrationResponse, ModelResponse, StaticModelRequest,
};
use crate::auth::{AuthView, PluginAuth};
use crate::callbacks::RequestScope;
use crate::host::{Host, normalize_provider};
use crate::rpc::CallError;

/// Go `pluginModelRegistration`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelRegistration {
    pub plugin_id: String,
    pub provider: String,
    pub priority: i64,
    pub models: Vec<ModelInfo>,
    pub has_executor: bool,
}

/// Registry changes: clients to (re)register with their provider and models, then
/// clients to remove.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClientChanges {
    pub register: Vec<(String, String, Vec<ModelInfo>)>,
    pub unregister: Vec<String>,
}

/// Executor table changes from [`Host::register_executors`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExecutorChanges {
    /// Provider to the plugin that executes it, in registration order.
    pub register: Vec<(String, String)>,
    /// Providers whose plugin executor went away. The caller removes each only if the
    /// executor registered for it is still a plugin executor (Go `ownsExecutor`).
    pub unregister: Vec<String>,
    pub clients: ClientChanges,
}

/// Go `AuthModelResult`.
#[derive(Debug, Clone, Default)]
pub struct AuthModelResult {
    pub provider: String,
    pub models: Vec<ModelInfo>,
    pub auth: Option<PluginAuth>,
    pub handled: bool,
    pub error: Option<CallError>,
}

/// Model IDs trimmed; empty IDs dropped.
fn clean_models(models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    models
        .into_iter()
        .filter_map(|mut m| {
            m.id = m.id.trim().to_owned();
            (!m.id.is_empty()).then_some(m)
        })
        .collect()
}

/// Go `appendModelsForProvider`: first occurrence of an ID wins.
fn append_models(out: &mut BTreeMap<String, Vec<ModelInfo>>, provider: &str, models: &[ModelInfo]) {
    let provider = normalize_provider(provider);
    if provider.is_empty() || models.is_empty() {
        return;
    }
    let list = out.entry(provider).or_default();
    let mut seen: HashSet<String> = list.iter().map(|m| m.id.trim().to_owned()).collect();
    for model in models {
        let id = model.id.trim();
        if !id.is_empty() && seen.insert(id.to_owned()) {
            list.push(model.clone());
        }
    }
}

/// Go `authDataHasValue`.
fn auth_data_has_value(d: &crate::api::AuthData) -> bool {
    let filled = |s: &str| !s.trim().is_empty();
    filled(&d.provider)
        || filled(&d.id)
        || filled(&d.file_name)
        || filled(&d.label)
        || filled(&d.prefix)
        || filled(&d.proxy_url)
        || d.disabled
        || !d.storage_json.is_empty()
        || !d.metadata.is_empty()
        || !d.attributes.is_empty()
        || !d.next_refresh_after.is_zero()
}

/// Go `authDataWithDefaults`.
fn auth_data_with_defaults(mut d: crate::api::AuthData, auth: &AuthView) -> crate::api::AuthData {
    let fill = |v: &mut String, from: &str| {
        if v.trim().is_empty() {
            *v = from.to_owned();
        }
    };
    fill(&mut d.provider, &auth.provider);
    fill(&mut d.id, &auth.id);
    fill(&mut d.file_name, &auth.file_name);
    fill(&mut d.label, &auth.label);
    fill(&mut d.prefix, &auth.prefix);
    fill(&mut d.proxy_url, &auth.proxy_url);
    for (k, v) in &auth.metadata {
        d.metadata.entry(k.clone()).or_insert_with(|| v.clone());
    }
    for (k, v) in &auth.attributes {
        d.attributes.entry(k.clone()).or_insert_with(|| v.clone());
    }
    if d.storage_json.is_empty() {
        d.storage_json = auth.storage_json.clone();
    }
    if d.next_refresh_after.is_zero() {
        d.next_refresh_after = auth.next_refresh_after;
    }
    d
}

/// Go `pluginExecutorModelClientID`.
pub fn executor_client_id(plugin_id: &str, provider: &str) -> String {
    format!("plugin:{plugin_id}:{provider}:executor")
}

impl Host {
    /// Go `RegisterModels`: static models from every model provider/registrar whose
    /// executor scope allows static models. Plugins without an executor register their
    /// models under client `plugin:<id>:<provider>`; executor plugins' models wait for
    /// [`Self::register_executors`].
    pub async fn register_models(&self) -> ClientChanges {
        let snapshot = self.snapshot();
        let records: Vec<_> = snapshot
            .records
            .iter()
            .filter(|r| self.record_current(r))
            .cloned()
            .collect();
        let mut changes = ClientChanges::default();
        let mut next_clients = HashSet::new();
        let mut next_providers = HashMap::new();
        let mut next_registrations = HashMap::new();
        for record in records {
            let caps = &record.plugin.caps;
            if !(caps.model_provider || caps.model_registrar) || !record.plugin.allows_static_models() {
                continue;
            }
            if self.is_fused(&record.id) || !self.record_current(&record) {
                continue;
            }
            let result: Result<ModelRegistrationResponse, CallError> = if caps.model_provider {
                let req = StaticModelRequest {
                    plugin: record.plugin.metadata.clone(),
                    host: self.host_config_summary(),
                };
                self.call::<ModelResponse, _>(&record, method::MODEL_STATIC, &req)
                    .await
                    .map(|r| ModelRegistrationResponse {
                        provider: r.provider,
                        models: r.models,
                    })
            } else {
                let req = ModelRegistrationRequest {
                    plugin: record.plugin.metadata.clone(),
                };
                self.call(&record, method::MODEL_REGISTER, &req).await
            };
            let resp = match result {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!("pluginhost: model registrar {} failed: {e}", record.id);
                    continue;
                }
            };
            let provider = normalize_provider(&resp.provider);
            let models = clean_models(resp.models);
            if provider.is_empty() || models.is_empty() {
                continue;
            }
            next_registrations.insert(
                record.id.clone(),
                ModelRegistration {
                    plugin_id: record.id.clone(),
                    provider: provider.clone(),
                    priority: record.priority,
                    models: models.clone(),
                    has_executor: caps.executor,
                },
            );
            next_providers.insert(record.id.clone(), provider.clone());
            if !caps.executor {
                let client = format!("plugin:{}:{provider}", record.id);
                next_clients.insert(client.clone());
                changes.register.push((client, provider, models));
            }
        }
        let mut state = self.state();
        if !Arc::ptr_eq(&self.snapshot(), &snapshot) {
            return ClientChanges::default();
        }
        let mut stale: Vec<String> = state.model_client_ids.difference(&next_clients).cloned().collect();
        stale.sort();
        changes.unregister = stale;
        state.model_client_ids = next_clients;
        state.model_providers = next_providers;
        state.model_registrations = next_registrations;
        changes
    }

    /// Go `ModelsForAuth`: the first model provider for the credential's provider (its
    /// auth provider's identifier, else its registered or executor provider).
    pub async fn models_for_auth(&self, auth: &AuthView, scope: &RequestScope) -> AuthModelResult {
        let provider_key = normalize_provider(&auth.provider);
        if provider_key.is_empty() {
            return AuthModelResult::default();
        }
        for record in self.active_records() {
            if !record.plugin.caps.model_provider || self.is_fused(&record.id) || !record.plugin.allows_oauth_models() {
                continue;
            }
            if record.plugin.caps.auth_provider {
                if normalize_provider(&record.plugin.auth_identifier) != provider_key {
                    continue;
                }
            } else {
                let mut record_provider = normalize_provider(
                    &self
                        .state()
                        .model_providers
                        .get(&record.id)
                        .cloned()
                        .unwrap_or_default(),
                );
                if record_provider.is_empty() && record.plugin.caps.executor {
                    record_provider = self.executor_provider(&record).await.unwrap_or_default();
                }
                if record_provider != provider_key {
                    continue;
                }
            }
            if !self.record_current(&record) {
                continue;
            }
            let req = AuthModelRequest {
                plugin: record.plugin.metadata.clone(),
                auth_id: auth.id.clone(),
                auth_provider: auth.provider.clone(),
                storage_json: auth.storage_json.clone(),
                metadata: auth.metadata.clone(),
                attributes: auth.attributes.clone(),
                host: self.host_config_summary(),
            };
            let resp: ModelResponse = match self
                .call_with_callback(&record, method::MODEL_FOR_AUTH, &req, scope)
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!("pluginhost: models for auth {} failed: {e}", auth.id);
                    return AuthModelResult {
                        handled: true,
                        error: Some(e),
                        ..Default::default()
                    };
                }
            };
            let mut resp_provider = normalize_provider(&resp.provider);
            if !resp_provider.is_empty() && resp_provider != provider_key {
                continue;
            }
            if resp_provider.is_empty() {
                resp_provider = provider_key.clone();
            }
            let path = auth
                .attributes
                .get(crate::auth::ATTRIBUTE_PATH)
                .cloned()
                .unwrap_or_default();
            let updated = auth_data_has_value(&resp.auth_update)
                .then(|| auth_data_with_defaults(resp.auth_update, auth))
                .and_then(|data| {
                    PluginAuth::from_auth_data(data, &path, &auth.file_name, &self.host_config_summary().auth_dir)
                });
            return AuthModelResult {
                provider: resp_provider,
                models: clean_models(resp.models),
                auth: updated,
                handled: true,
                error: None,
            };
        }
        AuthModelResult::default()
    }

    fn sorted_registrations(&self) -> Vec<ModelRegistration> {
        let mut registrations: Vec<ModelRegistration> = self.state().model_registrations.values().cloned().collect();
        registrations.sort_by(|a, b| b.priority.cmp(&a.priority).then_with(|| a.plugin_id.cmp(&b.plugin_id)));
        registrations
    }

    /// Go `RegisterExecutors`. `native_provider` says whether a built-in executor serves
    /// a provider; `model_providers` lists the providers registered for a model ID. A
    /// plugin executor claims its provider only when no built-in executor does, and only
    /// models no built-in provider serves; the first plugin (by priority) wins a model
    /// and a provider.
    pub async fn register_executors(
        &self,
        native_provider: &dyn Fn(&str) -> bool,
        model_providers: &dyn Fn(&str) -> Vec<String>,
    ) -> ExecutorChanges {
        let snapshot = self.snapshot();
        let records: Vec<_> = snapshot
            .records
            .iter()
            .filter(|r| self.record_current(r))
            .cloned()
            .collect();
        let mut provider_models: BTreeMap<String, Vec<ModelInfo>> = BTreeMap::new();
        for registration in self.sorted_registrations() {
            if !registration.has_executor {
                append_models(&mut provider_models, &registration.provider, &registration.models);
            }
        }
        let registration_of = |id: &str| self.state().model_registrations.get(id).cloned().unwrap_or_default();
        let model_native = |model: &str| model_providers(model).iter().any(|p| native_provider(p));
        let mut selected: HashMap<String, Vec<ModelInfo>> = HashMap::new();
        let mut claimed_models: HashSet<String> = HashSet::new();
        let mut claimed_providers: HashMap<String, String> = HashMap::new();
        for record in &records {
            if !record.plugin.caps.executor || self.is_fused(&record.id) {
                continue;
            }
            let Some(provider) = self.executor_provider(record).await else {
                continue;
            };
            let registration = registration_of(&record.id);
            if native_provider(&provider) {
                append_models(&mut provider_models, &provider, &registration.models);
                continue;
            }
            if registration.models.is_empty() {
                continue;
            }
            if claimed_providers
                .get(&provider)
                .is_some_and(|owner| owner != &record.id)
            {
                continue;
            }
            for model in &registration.models {
                let id = model.id.trim();
                if id.is_empty() || claimed_models.contains(id) || model_native(id) {
                    continue;
                }
                claimed_models.insert(id.to_owned());
                claimed_providers.insert(provider.clone(), record.id.clone());
                selected.entry(record.id.clone()).or_default().push(model.clone());
            }
        }
        let mut changes = ExecutorChanges::default();
        let mut seen = HashSet::new();
        let mut next_providers = HashSet::new();
        let mut next_clients = HashSet::new();
        for record in &records {
            if !record.plugin.caps.executor || self.is_fused(&record.id) {
                continue;
            }
            let Some(provider) = self.executor_provider(record).await else {
                continue;
            };
            let registration = registration_of(&record.id);
            let mine = selected.get(&record.id).cloned().unwrap_or_default();
            if !registration.models.is_empty() && mine.is_empty() {
                continue;
            }
            if !seen.insert(provider.clone()) || native_provider(&provider) {
                continue;
            }
            next_providers.insert(provider.clone());
            changes.register.push((provider.clone(), record.id.clone()));
            append_models(&mut provider_models, &provider, &mine);
            if !mine.is_empty() {
                let client = executor_client_id(&record.id, &provider);
                next_clients.insert(client.clone());
                changes.clients.register.push((client, provider.clone(), mine));
            }
        }
        let mut state = self.state();
        if !Arc::ptr_eq(&self.snapshot(), &snapshot) {
            return ExecutorChanges::default();
        }
        state.provider_models = provider_models;
        let mut stale: Vec<String> = state.executor_providers.difference(&next_providers).cloned().collect();
        stale.sort();
        state.executor_providers = next_providers;
        let mut stale_clients: Vec<String> = state
            .executor_model_client_ids
            .difference(&next_clients)
            .cloned()
            .collect();
        stale_clients.sort();
        changes.clients.unregister = stale_clients;
        state.executor_model_client_ids = next_clients;
        drop(state);
        // Go removes a stale provider only while the manager still holds this host's
        // adapter; a built-in executor that replaced it stays.
        stale.retain(|provider| !native_provider(provider));
        changes.unregister = stale;
        changes
    }

    /// Go `ModelsForProvider`.
    pub fn models_for_provider(&self, provider: &str) -> Vec<ModelInfo> {
        self.state()
            .provider_models
            .get(&normalize_provider(provider))
            .cloned()
            .unwrap_or_default()
    }

    /// The plugin executing `provider` after the last [`Self::register_executors`].
    pub fn executor_providers(&self) -> HashSet<String> {
        self.state().executor_providers.clone()
    }

    /// Go `HasExecutorCandidateProvider`.
    pub async fn has_executor_candidate_provider(&self, provider: &str) -> bool {
        let provider = normalize_provider(provider);
        if provider.is_empty() {
            return false;
        }
        for record in self.active_records() {
            if record.plugin.caps.executor
                && !self.is_fused(&record.id)
                && self.executor_provider(&record).await.as_deref() == Some(provider.as_str())
            {
                return true;
            }
        }
        false
    }
}
