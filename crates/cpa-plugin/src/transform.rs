//! Translator hooks, thinking appliers and usage plugins
//! (internal/pluginhost/adapters_usage_translation.go, sdk/translator/plugin_hooks.go).

use bytes::Bytes;

use crate::abi::method;
use crate::api::{
    ModelInfo, PayloadResponse, RequestTransformRequest, ResponseTransformRequest, ThinkingApplyRequest,
    ThinkingConfig, UsageRecord,
};
use crate::callbacks::RequestScope;
use crate::host::{Host, Record};
use crate::rpc::Empty;

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
    async fn transform<R: crate::gojson::GoStruct>(&self, record: &Record, method: &str, req: &R) -> Option<Bytes> {
        if !self.live(record) {
            return None;
        }
        match self.call::<PayloadResponse, _>(record, method, req).await {
            Ok(resp) if !resp.body.is_empty() => Some(resp.body),
            _ => None,
        }
    }

    /// Go `NormalizeRequest`: every request normalizer, chained.
    pub async fn normalize_request(&self, from: &str, to: &str, model: &str, body: Bytes, stream: bool) -> Bytes {
        let mut current = body;
        for record in self.hook_records(|r| r.plugin.caps.request_normalizer) {
            let req = RequestTransformRequest {
                from_format: from.into(),
                to_format: to.into(),
                model: model.into(),
                stream,
                body: current.clone(),
            };
            if let Some(out) = self.transform(&record, method::REQUEST_NORMALIZE, &req).await {
                current = out;
            }
        }
        current
    }

    /// Go `TranslateRequest`: the first translator that answers wins.
    pub async fn translate_request(
        &self,
        from: &str,
        to: &str,
        model: &str,
        body: &Bytes,
        stream: bool,
    ) -> Option<Bytes> {
        for record in self.hook_records(|r| r.plugin.caps.request_translator) {
            let req = RequestTransformRequest {
                from_format: from.into(),
                to_format: to.into(),
                model: model.into(),
                stream,
                body: body.clone(),
            };
            if let Some(out) = self.transform(&record, method::REQUEST_TRANSLATE, &req).await {
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
    pub async fn normalize_response_before(&self, t: &ResponseTransform<'_>, body: Bytes) -> Bytes {
        let mut current = body;
        for record in self.hook_records(|r| r.plugin.caps.response_before_translator) {
            let req = Self::response_request(t, current.clone());
            if let Some(out) = self.transform(&record, method::RESPONSE_NORMALIZE_BEFORE, &req).await {
                current = out;
            }
        }
        current
    }

    /// Go `TranslateResponse`: the first translator that answers wins.
    pub async fn translate_response(&self, t: &ResponseTransform<'_>, body: &Bytes) -> Option<Bytes> {
        for record in self.hook_records(|r| r.plugin.caps.response_translator) {
            let req = Self::response_request(t, body.clone());
            if let Some(out) = self.transform(&record, method::RESPONSE_TRANSLATE, &req).await {
                return Some(out);
            }
        }
        None
    }

    /// Go `NormalizeResponseAfter`: chained, after translation.
    pub async fn normalize_response_after(&self, t: &ResponseTransform<'_>, body: Bytes) -> Bytes {
        let mut current = body;
        for record in self.hook_records(|r| r.plugin.caps.response_after_translator) {
            let req = Self::response_request(t, current.clone());
            if let Some(out) = self.transform(&record, method::RESPONSE_NORMALIZE_AFTER, &req).await {
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
                .to_lowercase();
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
        self.state().thinking_providers = providers.into_iter().map(|(k, (owner, _))| (k, owner)).collect();
    }

    /// The plugin that applies thinking for `provider`, if any.
    pub fn thinking_provider(&self, provider: &str) -> Option<String> {
        self.state()
            .thinking_providers
            .get(&provider.trim().to_lowercase())
            .cloned()
    }

    /// Go `thinkingAdapter.Apply`: the plugin's body, or `body` unchanged on any failure.
    pub async fn apply_thinking(
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
            provider: provider.trim().to_lowercase(),
            model,
            config,
            body: body.clone(),
        };
        match self
            .call_with_callback::<PayloadResponse, _>(&record, method::THINKING_APPLY, &req, scope)
            .await
        {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_sessions_follow_the_go_adapter() {
        assert_eq!(usage_session_ids("", "", " c ", "p"), ("c".into(), "p".into()));
        assert_eq!(usage_session_ids("s", "", "s", "p"), ("s".into(), "p".into()));
        assert_eq!(usage_session_ids("s", "", "other", "p"), ("s".into(), "".into()));
        assert_eq!(usage_session_ids("s", "s", "", ""), ("s".into(), "".into()));
        assert_eq!(usage_session_ids("", "x", "", ""), ("".into(), "".into()));
    }
}
