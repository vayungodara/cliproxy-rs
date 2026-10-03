//! Quota provider capability (internal/pluginhost/quota_provider.go).
//!
//! A quota plugin answers for its `quota.identifier`, its plugin ID, its auth provider's
//! identifier, and then any provider its `quota.describe` lists (cached per snapshot).

use std::collections::BTreeSet;

use crate::abi::method;
use crate::api::{
    QuotaDescribeRequest, QuotaDescribeResponse, QuotaFetchRequest, QuotaFetchResponse, QuotaResetRequest,
    QuotaResetResponse,
};
use crate::auth::AuthView;
use crate::callbacks::RequestScope;
use crate::gojson::{DecodeError, GoJson, Node};
use crate::host::{Host, Record, Snapshot, normalize_provider};
use crate::rpc::CallError;

/// Go `RegisteredQuotaProviderInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaProviderInfo {
    pub plugin_id: String,
    pub provider: String,
    pub display_name: String,
    pub supported_providers: Vec<String>,
    pub supports_reset: bool,
}

/// `quota.fetch` result with the snake_case fallbacks of its `UnmarshalJSON` methods.
#[derive(Debug, Clone, Default, PartialEq)]
struct FetchWire(QuotaFetchResponse);

impl GoJson for FetchWire {
    const GO_TYPE: &'static str = "pluginapi.QuotaFetchResponse";
    fn encode(&self, out: &mut Vec<u8>) {
        self.0.encode(out)
    }
    fn decode_value(v: &Node) -> Result<Self, DecodeError> {
        crate::api::quota_fetch_from_value(v).map(FetchWire)
    }
    fn is_empty(&self) -> bool {
        false
    }
}

impl Host {
    /// Current quota providers; callers that await between records recheck fusion.
    fn quota_records(&self) -> Vec<Record> {
        self.quota_records_in(&self.snapshot())
            .into_iter()
            .filter(|r| !self.is_fused(&r.id))
            .collect()
    }

    fn quota_records_in(&self, snapshot: &Snapshot) -> Vec<Record> {
        self.current_records(snapshot)
            .into_iter()
            .filter(|r| r.plugin.caps.quota_provider)
            .collect()
    }

    fn quota_identifier(record: &Record) -> String {
        normalize_provider(&record.plugin.quota_identifier)
    }

    /// Go `QuotaProviderIdentifiers`.
    pub fn quota_provider_identifiers(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for record in self.quota_records() {
            let id = Self::quota_identifier(&record);
            if !id.is_empty() && !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }

    /// Go `callQuotaDescribe`: `Ok(None)` when the plugin is not current.
    async fn describe_quota_record(&self, record: &Record) -> Result<Option<QuotaDescribeResponse>, CallError> {
        if self.is_fused(&record.id) || !self.record_current(record) {
            return Ok(None);
        }
        let req = QuotaDescribeRequest {
            plugin: record.plugin.metadata.clone(),
        };
        self.call(record, method::QUOTA_DESCRIBE, &req).await.map(Some)
    }

    /// Go `cachedQuotaSupportedProviders`: described once per snapshot. The caller passes
    /// the snapshot its records came from, so one lookup uses one cache throughout.
    async fn supported_providers(&self, snapshot: &Snapshot, record: &Record) -> Vec<String> {
        if let Some(cached) = crate::host::lock(&snapshot.quota_supported).get(&record.id) {
            return cached.clone();
        }
        let Ok(Some(desc)) = self.describe_quota_record(record).await else {
            return Vec::new();
        };
        crate::host::lock(&snapshot.quota_supported).insert(record.id.clone(), desc.supported_providers.clone());
        desc.supported_providers
    }

    /// Go `QuotaProviders`.
    pub async fn quota_providers(&self) -> Vec<QuotaProviderInfo> {
        let mut out = Vec::new();
        for record in self.quota_records() {
            if self.is_fused(&record.id) {
                continue;
            }
            let mut identifier = Self::quota_identifier(&record);
            if identifier.is_empty() {
                identifier = normalize_provider(&record.id);
            }
            let desc = self
                .describe_quota_record(&record)
                .await
                .ok()
                .flatten()
                .unwrap_or_default();
            let supported = if desc.supported_providers.is_empty() && !identifier.is_empty() {
                vec![identifier.clone()]
            } else {
                desc.supported_providers
            };
            let display_name = [
                desc.display_name.as_str(),
                record.plugin.metadata.name.as_str(),
                identifier.as_str(),
            ]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or_default()
            .to_owned();
            out.push(QuotaProviderInfo {
                plugin_id: record.id.clone(),
                provider: identifier,
                display_name,
                supported_providers: supported,
                supports_reset: desc.supports_reset,
            });
        }
        out
    }

    /// Go `QuotaSupportedProvidersSet`.
    pub async fn quota_supported_providers(&self) -> BTreeSet<String> {
        let snapshot = self.snapshot();
        let mut out = BTreeSet::new();
        for record in self.quota_records_in(&snapshot) {
            if self.is_fused(&record.id) {
                continue;
            }
            let id = Self::quota_identifier(&record);
            if !id.is_empty() {
                out.insert(id);
            }
            let plugin = normalize_provider(&record.id);
            if !plugin.is_empty() {
                out.insert(plugin);
            }
            if record.plugin.caps.auth_provider {
                let auth = normalize_provider(&record.plugin.auth_identifier);
                if !auth.is_empty() {
                    out.insert(auth);
                }
            }
            for p in self.supported_providers(&snapshot, &record).await {
                let p = normalize_provider(&p);
                if !p.is_empty() {
                    out.insert(p);
                }
            }
        }
        out
    }

    /// Go `quotaProviderRecord`.
    pub async fn quota_provider_record(&self, provider: &str) -> Option<Record> {
        let provider = normalize_provider(provider);
        if provider.is_empty() {
            return None;
        }
        let snapshot = self.snapshot();
        let records = self.quota_records_in(&snapshot);
        for record in &records {
            if self.is_fused(&record.id) {
                continue;
            }
            if Self::quota_identifier(record) == provider
                || normalize_provider(&record.id) == provider
                || (record.plugin.caps.auth_provider && normalize_provider(&record.plugin.auth_identifier) == provider)
            {
                return Some(record.clone());
            }
        }
        for record in &records {
            if self.is_fused(&record.id) {
                continue;
            }
            if self
                .supported_providers(&snapshot, record)
                .await
                .iter()
                .any(|p| normalize_provider(p) == provider)
            {
                return Some(record.clone());
            }
        }
        None
    }

    /// Go `quotaProviderRecordByPlugin`.
    pub fn quota_provider_record_by_plugin(&self, plugin_id: &str) -> Option<Record> {
        let plugin_id = plugin_id.trim();
        self.quota_records().into_iter().find(|r| r.id == plugin_id)
    }

    /// Go `HasQuotaProvider`.
    pub async fn has_quota_provider(&self, provider: &str) -> bool {
        self.quota_provider_record(provider).await.is_some()
    }

    /// Go `HasQuotaProviderForPlugin`.
    pub fn has_quota_provider_for_plugin(&self, plugin_id: &str) -> bool {
        self.quota_provider_record_by_plugin(plugin_id).is_some()
    }

    /// Go `DescribeQuota`: by plugin ID, else by provider.
    pub async fn describe_quota(&self, plugin_id: &str) -> Result<Option<QuotaDescribeResponse>, CallError> {
        let record = match self.quota_provider_record_by_plugin(plugin_id) {
            Some(record) => Some(record),
            None => self.quota_provider_record(plugin_id).await,
        };
        match record {
            Some(record) => self.describe_quota_record(&record).await,
            None => Ok(None),
        }
    }

    /// Go `FetchQuota`: by provider, else by plugin ID. `auth` is the credential behind
    /// `req.auth_index` when it resolves (Go `authPhysicalJSONByIndex`).
    pub async fn fetch_quota(
        &self,
        req: QuotaFetchRequest,
        auth: Option<&AuthView>,
        scope: &RequestScope,
    ) -> Result<Option<QuotaFetchResponse>, CallError> {
        let mut record = self.quota_provider_record(&req.provider).await;
        if record.is_none() && !req.provider.is_empty() {
            record = self.quota_provider_record_by_plugin(&req.provider);
        }
        match record {
            Some(record) => self.call_quota_fetch(&record, req, auth, scope).await,
            None => Ok(None),
        }
    }

    /// Go `FetchQuotaByPlugin`.
    pub async fn fetch_quota_by_plugin(
        &self,
        plugin_id: &str,
        req: QuotaFetchRequest,
        auth: Option<&AuthView>,
        scope: &RequestScope,
    ) -> Result<Option<QuotaFetchResponse>, CallError> {
        match self.quota_provider_record_by_plugin(plugin_id) {
            Some(record) => self.call_quota_fetch(&record, req, auth, scope).await,
            None => Ok(None),
        }
    }

    /// Go `ResetQuota`.
    pub async fn reset_quota(
        &self,
        req: QuotaResetRequest,
        auth: Option<&AuthView>,
        scope: &RequestScope,
    ) -> Result<Option<QuotaResetResponse>, CallError> {
        let mut record = self.quota_provider_record(&req.provider).await;
        if record.is_none() && !req.provider.is_empty() {
            record = self.quota_provider_record_by_plugin(&req.provider);
        }
        match record {
            Some(record) => self.call_quota_reset(&record, req, auth, scope).await,
            None => Ok(None),
        }
    }

    /// Go `ResetQuotaByPlugin`.
    pub async fn reset_quota_by_plugin(
        &self,
        plugin_id: &str,
        req: QuotaResetRequest,
        auth: Option<&AuthView>,
        scope: &RequestScope,
    ) -> Result<Option<QuotaResetResponse>, CallError> {
        match self.quota_provider_record_by_plugin(plugin_id) {
            Some(record) => self.call_quota_reset(&record, req, auth, scope).await,
            None => Ok(None),
        }
    }

    async fn call_quota_fetch(
        &self,
        record: &Record,
        mut req: QuotaFetchRequest,
        auth: Option<&AuthView>,
        scope: &RequestScope,
    ) -> Result<Option<QuotaFetchResponse>, CallError> {
        if self.is_fused(&record.id) || !self.record_current(record) {
            return Ok(None);
        }
        if !req.auth_index.is_empty()
            && let Some(auth) = auth
        {
            fill_from_auth(
                &mut req.auth_id,
                &mut req.provider,
                &mut req.storage_json,
                &mut req.metadata,
                &mut req.attributes,
                auth,
            );
        }
        req.host = self.host_config_summary();
        let resp: FetchWire = self
            .call_with_callback(record, method::QUOTA_FETCH, &req, scope)
            .await?;
        Ok(Some(resp.0))
    }

    async fn call_quota_reset(
        &self,
        record: &Record,
        mut req: QuotaResetRequest,
        auth: Option<&AuthView>,
        scope: &RequestScope,
    ) -> Result<Option<QuotaResetResponse>, CallError> {
        if self.is_fused(&record.id) || !self.record_current(record) {
            return Ok(None);
        }
        if !req.auth_index.is_empty()
            && let Some(auth) = auth
        {
            fill_from_auth(
                &mut req.auth_id,
                &mut req.provider,
                &mut req.storage_json,
                &mut req.metadata,
                &mut req.attributes,
                auth,
            );
        }
        req.host = self.host_config_summary();
        self.call_with_callback(record, method::QUOTA_RESET, &req, scope)
            .await
            .map(Some)
    }
}

/// Go fills a quota request's empty fields from the credential behind its auth index.
/// Metadata and attributes are replaced only when the request had none (nil in Go).
fn fill_from_auth(
    auth_id: &mut String,
    provider: &mut String,
    storage_json: &mut bytes::Bytes,
    metadata: &mut crate::gojson::Metadata,
    attributes: &mut crate::gojson::StringMap,
    auth: &AuthView,
) {
    if auth_id.is_empty() {
        *auth_id = auth.id.clone();
    }
    if provider.is_empty() {
        *provider = auth.provider.clone();
    }
    if storage_json.is_empty() {
        *storage_json = auth.storage_json.clone();
    }
    if metadata.is_empty() {
        *metadata = auth.metadata.clone();
    }
    if attributes.is_empty() {
        *attributes = auth.attributes.clone();
    }
}
