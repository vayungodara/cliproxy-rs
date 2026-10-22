//! Translator hooks, thinking appliers and usage plugins
//! (internal/pluginhost/adapters_usage_translation.go, sdk/translator/plugin_hooks.go).

use bytes::Bytes;
use cpa_common::gostr::GoStr;

use crate::abi::method;
use crate::api::{
    ModelInfo, PayloadResponse, RequestTransformRequest, ResponseTransformRequest, ThinkingApplyRequest,
    ThinkingConfig, UsageRecord,
};
use crate::callbacks::RequestScope;
use crate::host::{Host, Inner, Record};
use crate::rpc::Empty;
use std::sync::{Arc, Weak};

/// Providers with a built-in thinking applier; plugins cannot take them over
/// (internal/thinking/apply.go and the provider packages' `RegisterProvider` calls).
pub const NATIVE_THINKING_PROVIDERS: [&str; 11] = [
    "gemini",
    "claude",
    "openai",
    "codex",
    "antigravity",
    "kimi",
    "kimi-ai",
    "kimi.ai",
    "kimi.com",
    "xai",
    "interactions",
];

/// One response-side hook call's inputs.
#[derive(Debug, Clone, Default)]
pub struct ResponseTransform<'a> {
    pub from: &'a str,
    pub to: &'a str,
    pub model: &'a str,
    pub original_request: &'a [u8],
    pub translated_request: &'a [u8],
    pub stream: bool,
}

impl Host {
    /// Records with a hook; each call rechecks [`Host::live`].
    fn hook_records(&self, has: impl Fn(&Record) -> bool) -> Vec<Record> {
        self.active_records()
            .into_iter()
            .filter(|r| has(r) && !self.is_fused(&r.id))
            .collect()
    }

    /// A transform call: the plugin's body when it answered with one.
    fn transform<R: crate::gojson::GoStruct>(&self, record: &Record, method: &str, req: &R) -> Option<Bytes> {
        if !self.live(record) {
            return None;
        }
        match self.call_blocking::<PayloadResponse, _>(record, method, req) {
            Ok(resp) if !resp.body.is_empty() => Some(resp.body),
            _ => None,
        }
    }

    /// Go `NormalizeRequest`: every request normalizer, chained.
    pub fn normalize_request(&self, from: &str, to: &str, model: &str, body: Bytes, stream: bool) -> Bytes {
        let mut current = body;
        for record in self.hook_records(|r| r.plugin.caps.request_normalizer) {
            let req = RequestTransformRequest {
                from_format: from.into(),
                to_format: to.into(),
                model: model.into(),
                stream,
                body: current.clone(),
            };
            if let Some(out) = self.transform(&record, method::REQUEST_NORMALIZE, &req) {
                current = out;
            }
        }
        current
    }

    /// Go `TranslateRequest`: the first translator that answers wins.
    pub fn translate_request(&self, from: &str, to: &str, model: &str, body: &Bytes, stream: bool) -> Option<Bytes> {
        for record in self.hook_records(|r| r.plugin.caps.request_translator) {
            let req = RequestTransformRequest {
                from_format: from.into(),
                to_format: to.into(),
                model: model.into(),
                stream,
                body: body.clone(),
            };
            if let Some(out) = self.transform(&record, method::REQUEST_TRANSLATE, &req) {
                return Some(out);
            }
        }
        None
    }

    fn response_request(t: &ResponseTransform<'_>, body: Bytes) -> ResponseTransformRequest {
        ResponseTransformRequest {
            from_format: t.from.into(),
            to_format: t.to.into(),
            model: t.model.into(),
            stream: t.stream,
            original_request: Bytes::copy_from_slice(t.original_request),
            translated_request: Bytes::copy_from_slice(t.translated_request),
            body,
        }
    }

    /// Go `NormalizeResponseBefore`: chained, before native translation.
    pub fn normalize_response_before(&self, t: &ResponseTransform<'_>, body: Bytes) -> Bytes {
        let mut current = body;
        for record in self.hook_records(|r| r.plugin.caps.response_before_translator) {
            let req = Self::response_request(t, current.clone());
            if let Some(out) = self.transform(&record, method::RESPONSE_NORMALIZE_BEFORE, &req) {
                current = out;
            }
        }
        current
    }

    /// Go `TranslateResponse`: the first translator that answers wins.
    pub fn translate_response(&self, t: &ResponseTransform<'_>, body: &Bytes) -> Option<Bytes> {
        for record in self.hook_records(|r| r.plugin.caps.response_translator) {
            let req = Self::response_request(t, body.clone());
            if let Some(out) = self.transform(&record, method::RESPONSE_TRANSLATE, &req) {
                return Some(out);
            }
        }
        None
    }

    /// Go `NormalizeResponseAfter`: chained, after translation.
    pub fn normalize_response_after(&self, t: &ResponseTransform<'_>, body: Bytes) -> Bytes {
        let mut current = body;
        for record in self.hook_records(|r| r.plugin.caps.response_after_translator) {
            let req = Self::response_request(t, current.clone());
            if let Some(out) = self.transform(&record, method::RESPONSE_NORMALIZE_AFTER, &req) {
                current = out;
            }
        }
        current
    }

    /// Go `hasResponseTranslator`.
    pub fn has_response_translator(&self) -> bool {
        !self.hook_records(|r| r.plugin.caps.response_translator).is_empty()
    }

    /// Go `refreshThinkingProviders` + `thinking.RegisterPluginProvider`: provider name to
    /// the owning plugin, highest priority then lowest ID, never shadowing a native
    /// provider. Identifiers are asked once per refresh.
    pub(crate) async fn refresh_thinking_providers(&self) {
        let mut providers: std::collections::BTreeMap<String, (String, i64)> = Default::default();
        for record in self.hook_records(|r| r.plugin.caps.thinking_applier) {
            let identifier = crate::rpc::identifier(&record.client, method::THINKING_IDENTIFIER)
                .await
                .go_lower();
            if identifier.is_empty() || NATIVE_THINKING_PROVIDERS.contains(&identifier.as_str()) {
                continue;
            }
            let replace = match providers.get(&identifier) {
                None => true,
                Some((owner, priority)) => {
                    record.priority > *priority || (record.priority == *priority && record.id < *owner)
                }
            };
            if replace {
                providers.insert(identifier, (record.id.clone(), record.priority));
            }
        }
        // Go `thinking.ClearPluginProviders` + `RegisterPluginProvider`: the process-wide
        // table thinking consults for providers without a built-in applier.
        cpa_common::thinking::clear_plugin_providers();
        for (provider, (owner, priority)) in &providers {
            let applier = Arc::new(ThinkingAdapter {
                host: Arc::downgrade(&self.inner),
                provider: provider.clone(),
            });
            cpa_common::thinking::register_plugin_provider(owner, provider, *priority, applier);
        }
        self.state().thinking_providers = providers.into_iter().map(|(k, (owner, _))| (k, owner)).collect();
    }

    /// The plugin that applies thinking for `provider`, if any.
    pub fn thinking_provider(&self, provider: &str) -> Option<String> {
        self.state()
            .thinking_providers
            .get(&provider.trim().go_lower())
            .cloned()
    }

    /// Go `thinkingAdapter.Apply`: the plugin's body, or `body` unchanged on any failure.
    pub fn apply_thinking(
        &self,
        provider: &str,
        model: ModelInfo,
        config: ThinkingConfig,
        body: Bytes,
        scope: &RequestScope,
    ) -> Bytes {
        let Some(owner) = self.thinking_provider(provider) else {
            return body;
        };
        let Some(record) = self.record(&owner) else {
            return body;
        };
        let req = ThinkingApplyRequest {
            provider: provider.trim().go_lower(),
            model,
            config,
            body: body.clone(),
        };
        match self.call_with_callback_blocking::<PayloadResponse, _>(&record, method::THINKING_APPLY, &req, scope) {
            Ok(resp) if !resp.body.is_empty() => resp.body,
            _ => body,
        }
    }

    /// Go `usageAdapter.HandleUsage` for every usage plugin, in the background. The
    /// caller resolves session fields the way the adapter does (see
    /// [`usage_session_ids`]); failures are logged at debug level.
    pub fn publish_usage(&self, record: UsageRecord) {
        for plugin in self.hook_records(|r| r.plugin.caps.usage_plugin) {
            let host = self.clone();
            let record = record.clone();
            tokio::spawn(async move {
                // Go `currentUsagePlugin`: the plugin as it is when the record arrives.
                let Some(plugin) = host.record(&plugin.id).filter(|r| r.plugin.caps.usage_plugin) else {
                    return;
                };
                if let Err(e) = host.call::<Empty, _>(&plugin, method::USAGE_HANDLE, &record).await {
                    tracing::debug!("pluginhost: usage.handle to {} failed: {e}", plugin.id);
                }
            });
        }
    }

    /// Whether any usage plugin is active.
    pub fn has_usage_plugins(&self) -> bool {
        !self.hook_records(|r| r.plugin.caps.usage_plugin).is_empty()
    }
}

/// The adapter's session fields: the record's own session wins; otherwise the client
/// request's; the parent is dropped when it equals the session or no session is known.
pub fn usage_session_ids(
    record_session: &str,
    record_parent: &str,
    client_session: &str,
    client_parent: &str,
) -> (String, String) {
    let mut session = record_session.trim().to_owned();
    let mut parent = record_parent.trim().to_owned();
    if session.is_empty() {
        session = client_session.trim().to_owned();
        parent = client_parent.trim().to_owned();
    } else if parent.is_empty() && session == client_session.trim() {
        parent = client_parent.trim().to_owned();
    }
    if session.is_empty() || session == parent {
        parent.clear();
    }
    (session, parent)
}

/// Any of the translator hooks (Go `PluginHooks`).
fn translator_hook(r: &Record) -> bool {
    let c = &r.plugin.caps;
    c.request_normalizer
        || c.request_translator
        || c.response_before_translator
        || c.response_translator
        || c.response_after_translator
}

impl Host {
    /// This host as Go's `translator.PluginHooks` (`sdktranslator.SetPluginHooks(host)`).
    /// It holds the host weakly, so installing it process-wide keeps nothing alive.
    /// From then on, this host keeps cpa-translate's hook flag current, so with no
    /// translator plugin the registry skips the hooks after one atomic load.
    pub fn translator_hooks(&self) -> Arc<dyn cpa_translate::hooks::Hooks> {
        self.inner
            .hooks_installed
            .store(true, std::sync::atomic::Ordering::Release);
        let state = self.state();
        self.publish_hook_activity(&state, &self.snapshot().records);
        drop(state);
        Arc::new(TranslatorHooks(Arc::downgrade(&self.inner)))
    }

    /// Publishes whether an unfused plugin among `records` offers a translator hook, and
    /// an auth provider, to whichever of the registry and credential loading hold this
    /// host. Callers hold the state lock, so updates follow the lifecycle's order.
    pub(crate) fn publish_hook_activity(&self, state: &crate::host::State, records: &[Record]) {
        use std::sync::atomic::Ordering;
        let offers = |has: fn(&Record) -> bool| records.iter().any(|r| has(r) && !state.fused.contains_key(&r.id));
        if self.inner.hooks_installed.load(Ordering::Acquire) {
            cpa_translate::hooks::set_plugin_hooks_active(offers(translator_hook));
        }
        if self.inner.auth_parser_installed.load(Ordering::Acquire) {
            // What auth parsing depends on: the plugins that parse, as loaded,
            // configured and identified, and the host config every parse request
            // carries. The same inputs publish the same signature, so an unchanged sync
            // keeps cached answers.
            use std::hash::{Hash, Hasher};
            let mut providers = records
                .iter()
                .filter(|r| r.plugin.caps.auth_provider && !state.fused.contains_key(&r.id))
                .peekable();
            let signature = providers.peek().is_some().then(|| {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                for r in providers {
                    r.id.hash(&mut hasher);
                    (Arc::as_ptr(&r.client) as usize).hash(&mut hasher);
                    state.loaded.get(&r.id).map(|lp| &lp.config_yaml).hash(&mut hasher);
                    crate::host::normalize_provider(&r.plugin.auth_identifier).hash(&mut hasher);
                }
                let summary = state.config.as_deref().map(crate::auth::host_config_summary_of);
                summary.map(|s| crate::gojson::to_vec(&s)).hash(&mut hasher);
                hasher.finish()
            });
            cpa_core::config::credentials::set_plugin_auth_providers(signature);
        }
    }

    /// Whether an active, unfused plugin offers a hook `has` selects, without copying
    /// the records.
    fn offers(&self, has: impl Fn(&Record) -> bool) -> bool {
        self.snapshot().records.iter().any(|r| has(r) && !self.is_fused(&r.id))
    }
}

struct TranslatorHooks(Weak<Inner>);

impl TranslatorHooks {
    fn host(&self, has: impl Fn(&Record) -> bool) -> Option<Host> {
        let host = Host {
            inner: self.0.upgrade()?,
        };
        host.offers(has).then_some(host)
    }
}

fn response_transform<'a>(t: &'a cpa_translate::hooks::Response<'a>) -> ResponseTransform<'a> {
    ResponseTransform {
        from: t.from,
        to: t.to,
        model: t.model,
        original_request: t.original_request,
        translated_request: t.translated_request,
        stream: t.stream,
    }
}

impl cpa_translate::hooks::Hooks for TranslatorHooks {
    fn normalize_request(&self, from: &str, to: &str, model: &str, body: &[u8], stream: bool) -> Option<Vec<u8>> {
        let host = self.host(|r| r.plugin.caps.request_normalizer)?;
        Some(
            host.normalize_request(from, to, model, Bytes::copy_from_slice(body), stream)
                .to_vec(),
        )
    }

    fn translate_request(&self, from: &str, to: &str, model: &str, body: &[u8], stream: bool) -> Option<Vec<u8>> {
        let host = self.host(|r| r.plugin.caps.request_translator)?;
        host.translate_request(from, to, model, &Bytes::copy_from_slice(body), stream)
            .map(|b| b.to_vec())
    }

    fn normalize_response_before(&self, t: &cpa_translate::hooks::Response<'_>, body: &[u8]) -> Option<Vec<u8>> {
        let host = self.host(|r| r.plugin.caps.response_before_translator)?;
        Some(
            host.normalize_response_before(&response_transform(t), Bytes::copy_from_slice(body))
                .to_vec(),
        )
    }

    fn translate_response(&self, t: &cpa_translate::hooks::Response<'_>, body: &[u8]) -> Option<Vec<u8>> {
        let host = self.host(|r| r.plugin.caps.response_translator)?;
        host.translate_response(&response_transform(t), &Bytes::copy_from_slice(body))
            .map(|b| b.to_vec())
    }

    fn normalize_response_after(&self, t: &cpa_translate::hooks::Response<'_>, body: &[u8]) -> Option<Vec<u8>> {
        let host = self.host(|r| r.plugin.caps.response_after_translator)?;
        Some(
            host.normalize_response_after(&response_transform(t), Bytes::copy_from_slice(body))
                .to_vec(),
        )
    }
}

/// Go `thinkingAdapter`: a plugin's thinking applier for one provider. Any failure
/// leaves the body unchanged.
struct ThinkingAdapter {
    host: Weak<Inner>,
    provider: String,
}

impl cpa_common::thinking::PluginApplier for ThinkingAdapter {
    fn apply(
        &self,
        body: &[u8],
        config: &cpa_common::thinking::Config,
        info: Option<&cpa_common::thinking::ModelCaps>,
    ) -> Result<Vec<u8>, cpa_common::thinking::Error> {
        let Some(inner) = self.host.upgrade() else {
            return Ok(body.to_vec());
        };
        let config = ThinkingConfig {
            mode: config.mode.as_str().to_owned(),
            budget: config.budget,
            level: config.level.clone(),
        };
        let out = Host { inner }.apply_thinking(
            &self.provider,
            plugin_model_info(info),
            config,
            Bytes::copy_from_slice(body),
            &RequestScope::default(),
        );
        Ok(out.to_vec())
    }
}

/// Go `registryModelInfoToPluginModelInfo` from the fields thinking resolves.
// ponytail: thinking sees only `ModelCaps` (ID, type, completion limit, thinking support,
// user-defined); the descriptive registry fields reach the plugin empty. Pass the full
// `ModelInfo` through `ModelCaps` if a plugin needs them.
fn plugin_model_info(info: Option<&cpa_common::thinking::ModelCaps>) -> ModelInfo {
    let Some(info) = info else {
        return ModelInfo::default();
    };
    ModelInfo {
        id: info.id.clone(),
        model_type: info.kind.clone(),
        max_completion_tokens: info.max_completion_tokens,
        thinking: info.thinking.as_ref().map(|t| crate::api::ThinkingSupport {
            min: t.min,
            max: t.max,
            zero_allowed: t.zero_allowed,
            dynamic_allowed: t.dynamic_allowed,
            disable_type: t.disable_type.clone(),
            levels: t.levels.clone(),
        }),
        user_defined: info.user_defined,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go TestPluginThinkingSupportCarriesDisableTypeBothWays, registry to plugin: a
    /// plugin thinking applier sees the model's `DisableType`.
    #[test]
    fn plugin_model_info_keeps_the_disable_type() {
        let caps = cpa_common::thinking::ModelCaps {
            id: "m".into(),
            thinking: Some(cpa_core::registry::ThinkingSupport {
                levels: vec!["low".into()],
                disable_type: "between_tools".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let got = plugin_model_info(Some(&caps)).thinking.unwrap();
        assert_eq!(got.disable_type, "between_tools");
    }

    #[test]
    fn usage_sessions_follow_the_go_adapter() {
        assert_eq!(usage_session_ids("", "", " c ", "p"), ("c".into(), "p".into()));
        assert_eq!(usage_session_ids("s", "", "s", "p"), ("s".into(), "p".into()));
        assert_eq!(usage_session_ids("s", "", "other", "p"), ("s".into(), "".into()));
        assert_eq!(usage_session_ids("s", "s", "", ""), ("s".into(), "".into()));
        assert_eq!(usage_session_ids("", "x", "", ""), ("".into(), "".into()));
    }
}
