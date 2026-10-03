//! Home mode (`-home-jwt`): config from Home, credentials dispatched per request,
//! concurrency leases released back (Go cmd/server/main.go's Home branch,
//! sdk/cliproxy/service_home.go and auth/conductor_home.go). The protocol lives in
//! `cpa-home`; this module wires it to the runtime through
//! [`cpa_server::remote::RemoteDispatch`].

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use cpa_core::config::Config;
use cpa_core::credential::{Credential, Source};
use cpa_core::exec::{ExecError, FailureScope};
use cpa_home::dispatch::{DispatchResponse, decode_concurrency, decode_error, verify_identity};
use cpa_home::registry::{Registry, ScopeSpec};
use cpa_home::release::ReleaseFlusher;
use cpa_home::{Client, CredentialConcurrency, DispatchRequest, HomeConfig};
use cpa_server::Runtime;
use cpa_server::remote::{
    EndLease, ModelsError, RemoteDispatch, RemoteError, RemoteErrorKind, RemoteGrant, RemoteRequest,
};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use serde_yaml_ng::{Mapping, Value as Yaml};
use tokio_util::sync::CancellationToken;

/// Go main's 30-second contexts around the Home bootstrap.
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(30);
/// Go `homeSubscriberPreAckRetryBackoff`.
const PRE_ACK_RETRY: Duration = Duration::from_millis(100);

fn set(doc: &mut Yaml, path: &[&str], value: Option<Yaml>) {
    let mut node = doc;
    for (i, key) in path.iter().enumerate() {
        if !node.is_mapping() {
            *node = Yaml::Mapping(Mapping::new());
        }
        let Yaml::Mapping(map) = node else { unreachable!() };
        let key = Yaml::String((*key).to_owned());
        if i + 1 == path.len() {
            match value {
                Some(value) => {
                    map.insert(key, value);
                }
                None => {
                    map.remove(&key);
                }
            }
            return;
        }
        node = map.entry(key).or_insert(Yaml::Null);
    }
}

fn get<'a>(doc: &'a Yaml, path: &[&str]) -> Option<&'a Yaml> {
    path.iter().try_fold(doc, |node, key| node.get(*key))
}

/// A Home config payload as this node runs it: Go `ParseConfigBytes`, the listener
/// kept from `base` (or `NormalizeHomePort` at startup), then `forceHomeRuntimeConfig`.
pub fn overlay(raw: &[u8], base: Option<&Config>) -> anyhow::Result<Config> {
    let text = std::str::from_utf8(raw).map_err(|e| anyhow::anyhow!("config payload is not UTF-8: {e}"))?;
    let parsed = Config::parse(text)?;
    let mut doc = parsed.document.clone();
    match base {
        Some(base) => {
            for key in ["host", "port", "tls"] {
                set(
                    &mut doc,
                    &["server", key],
                    get(&base.document, &["server", key]).cloned(),
                );
            }
        }
        None => set(
            &mut doc,
            &["server", "port"],
            Some(Yaml::Number(cpa_home::normalize_home_port(parsed.port).into())),
        ),
    }
    set(&mut doc, &["access", "api-keys"], None);
    set(
        &mut doc,
        &["observability", "usage", "usage-statistics-enabled"],
        Some(Yaml::Bool(true)),
    );
    set(
        &mut doc,
        &["routing", "cooldown", "disable-cooling"],
        Some(Yaml::Bool(true)),
    );
    set(
        &mut doc,
        &["routing", "cooldown", "save-cooldown-status"],
        Some(Yaml::Bool(false)),
    );
    set(
        &mut doc,
        &["oauth", "providers", "aistudio", "ws-auth"],
        Some(Yaml::Bool(false)),
    );
    set(&mut doc, &["management", "allow-remote"], Some(Yaml::Bool(false)));
    set(
        &mut doc,
        &["management", "disable-control-panel"],
        Some(Yaml::Bool(true)),
    );
    set(&mut doc, &["plugins", "store-auth"], None);
    Config::parse(&serde_yaml_ng::to_string(&doc)?)
}

/// Go main's Home bootstrap. The error is the line Go logs before exiting.
pub async fn bootstrap(jwt: &str, disable_discovery: bool) -> Result<(HomeConfig, Config), String> {
    let mut home = tokio::time::timeout(BOOTSTRAP_TIMEOUT, cpa_home::cert::config_from_jwt(jwt))
        .await
        .unwrap_or(Err(cpa_home::Error::Timeout))
        .map_err(|e| format!("invalid -home-jwt: {e}"))?;
    if disable_discovery {
        home.disable_cluster_discovery = true;
    }
    let client = Client::new(home.clone());
    let raw = tokio::time::timeout(BOOTSTRAP_TIMEOUT, client.get_config())
        .await
        .unwrap_or(Err(cpa_home::Error::Timeout));
    // Go closes the bootstrap client once startup is done; the service opens its own.
    client.close();
    let raw = raw.map_err(|e| format!("failed to fetch config from home: {e}"))?;
    // ponytail: Home plugin sync and its status report are not ported; plugins load
    // from the local configuration only.
    let config = overlay(&raw, None).map_err(|e| format!("failed to parse config payload from home: {e:#}"))?;
    Ok((home, config))
}

/// The client and registry of the live Home lifetime (Go `HomeDispatchBundle`).
#[derive(Clone)]
struct Bundle {
    client: Client,
    registry: Registry,
}

/// Go `pickHomeDispatchSelection` behind [`RemoteDispatch`].
pub struct Dispatcher {
    bundle: RwLock<Option<Bundle>>,
    /// For `Executors::supports` (Go `m.Executor(key)`); weak, the runtime owns this.
    rt: std::sync::Weak<Runtime>,
}

fn fail(status: u16, code: &str, message: impl std::fmt::Display) -> ExecError {
    // Go's handlers render `*auth.Error` text, `code: message`. Client errors stop the
    // request; the rest may move to another credential or round.
    let scope = if (400..500).contains(&status) && status != 429 && status != 408 {
        FailureScope::Request
    } else {
        FailureScope::Credential
    };
    ExecError::local(status, scope, format!("{code}: {message}"))
}

/// A plain Home failure (Go `*auth.Error`).
fn reject(status: u16, code: &str, message: impl std::fmt::Display) -> RemoteError {
    RemoteError::plain(fail(status, code, message), code)
}

/// Go `invalidHomeConcurrencyResponse`.
fn invalid_concurrency(message: &str) -> RemoteError {
    reject(502, "invalid_home_concurrency", message)
}

impl Dispatcher {
    pub fn new(rt: &Arc<Runtime>) -> Self {
        Self {
            bundle: RwLock::default(),
            rt: Arc::downgrade(rt),
        }
    }

    fn install(&self, client: Client, registry: Registry) {
        *self.bundle.write().unwrap_or_else(PoisonError::into_inner) = Some(Bundle { client, registry });
    }

    /// Go `ClearHomeDispatchBundle`: only the lifetime that installed it.
    fn clear(&self, client: &Client) {
        let mut bundle = self.bundle.write().unwrap_or_else(PoisonError::into_inner);
        if bundle.as_ref().is_some_and(|b| b.client.ptr_eq(client)) {
            *bundle = None;
        }
    }

    fn current(&self) -> Option<Bundle> {
        self.bundle.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    async fn pick(&self, request: RemoteRequest) -> Result<RemoteGrant, RemoteError> {
        let Some(Bundle { client, registry }) = self.current() else {
            return Err(reject(503, "home_unavailable", "home dispatch bundle unavailable"));
        };
        if !client.heartbeat_ok() {
            return Err(reject(503, "home_unavailable", "home control center unavailable"));
        }
        let pinned = request.pinned.trim().to_owned();
        if !pinned.is_empty() && request.excluded.contains(&pinned) {
            return Err(reject(
                503,
                "auth_not_found",
                "pinned auth is unavailable in the current retry round",
            ));
        }
        let pending = registry
            .begin_dispatch()
            .map_err(|_| reject(503, "home_unavailable", "home execution registry unavailable"))?;
        let model = request.model.trim().to_owned();
        let mut excluded: Vec<String> = request
            .excluded
            .iter()
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
            .collect();
        excluded.sort();
        excluded.dedup();
        let dispatch = DispatchRequest {
            model: model.clone(),
            session_id: request.session_id.clone(),
            parent_session_id: request.parent_session_id.clone(),
            headers: request.headers.clone(),
            count: request.count,
            credential_policy: String::new(),
            retry_round: Some(request.retry_round.max(0)),
            // Go sends the list only once something was tried.
            excluded_auth_ids: (!excluded.is_empty()).then_some(excluded),
            pinned_auth_id: pinned.clone(),
        };
        let raw = match client.rpop_auth(&dispatch).await {
            Ok(raw) => raw,
            Err(error) => {
                drop(pending);
                return Err(match error {
                    cpa_home::Error::AuthNotFound => reject(503, "auth_not_found", &error),
                    _ => reject(503, "home_unavailable", &error),
                });
            }
        };
        let tuple = match decode_concurrency(&raw) {
            Ok(tuple) => tuple,
            Err((true, _)) => {
                client.abort_ambiguous_dispatch();
                return Err(invalid_concurrency("Home returned malformed concurrency tuple"));
            }
            Err((false, _)) => return Err(reject(502, "invalid_auth", "home returned invalid auth payload")),
        };
        let kind = request.kind.to_owned();
        let spec = |credential_id: String, model: String, accounted: bool| ScopeSpec {
            request_id: request.request_id.clone(),
            credential_id,
            model,
            kind: kind.clone(),
            started_at: SystemTime::now(),
            accounted,
        };
        // Go `homeConcurrencyInstallError` for a registry that refuses the scope.
        let install_failed = |error: cpa_home::registry::RegistryError| {
            client.abort_ambiguous_dispatch();
            reject(
                503,
                "home_unavailable",
                format!("home execution registry unavailable: {error}"),
            )
        };
        // An accounted lease exists in Home from here on; every failure below ends its
        // scope, which releases it.
        let mut pending = Some(pending);
        let scope = match &tuple {
            Some(tuple) => Some(
                registry
                    .install(
                        pending.take().expect("pending"),
                        spec(tuple.credential_id.clone(), tuple.model.clone(), true),
                    )
                    .map_err(install_failed)?,
            ),
            None => None,
        };
        let end_scope = |scope: &Option<cpa_home::registry::Scope>| {
            if let Some(scope) = scope {
                scope.end();
            }
        };
        if let Some(error) = decode_error(&raw) {
            if tuple.is_some() {
                client.abort_ambiguous_dispatch();
                end_scope(&scope);
                return Err(invalid_concurrency(
                    "Home returned both accounted concurrency and an error",
                ));
            }
            // Go's typed Home errors: cooldowns join retry rounds; busy is never
            // retried and exposes its safe `Retry-After`.
            let header = error.retry_after_header();
            let kind = match error.kind {
                cpa_home::dispatch::HomeErrorKind::Plain => RemoteErrorKind::Plain,
                cpa_home::dispatch::HomeErrorKind::Cooldown {
                    retry_after,
                    request_retry,
                } => RemoteErrorKind::Cooldown {
                    retry_after,
                    request_retry,
                },
                cpa_home::dispatch::HomeErrorKind::Busy { .. } => RemoteErrorKind::Busy { header },
            };
            let mut failure = fail(error.status, &error.code, &error.message);
            if let RemoteErrorKind::Cooldown { retry_after, .. } = &kind {
                failure.retry_after = *retry_after;
            }
            return Err(RemoteError {
                error: failure,
                code: error.code.clone(),
                kind,
            });
        }
        let response = match DispatchResponse::parse(&raw) {
            Ok(response) => response,
            Err(message) => {
                end_scope(&scope);
                return Err(reject(502, "invalid_auth", message));
            }
        };
        let observed = response.observed_model(&model).to_owned();
        if let Some(tuple) = &tuple
            && cpa_home::dispatch::valid_concurrency_model_key(&observed).as_deref() != Some(tuple.model.as_str())
        {
            client.abort_ambiguous_dispatch();
            end_scope(&scope);
            return Err(invalid_concurrency(
                "Home concurrency model does not match dispatched model",
            ));
        }
        let auth_id = response.auth_id().trim().to_owned();
        if auth_id.is_empty() {
            end_scope(&scope);
            return Err(reject(502, "invalid_auth", "home returned auth without id"));
        }
        if !pinned.is_empty() && auth_id != pinned {
            end_scope(&scope);
            return Err(reject(
                503,
                "auth_not_found",
                "home returned an auth that does not match the pinned credential",
            ));
        }
        if let Some(tuple) = &tuple
            && let Err(message) = verify_identity(tuple, response.auth_id(), response.auth_index.trim())
        {
            end_scope(&scope);
            return Err(invalid_concurrency(&message));
        }
        let mut credential = match credential(&response, &model) {
            Ok(credential) => credential,
            Err(error) => {
                end_scope(&scope);
                return Err(error);
            }
        };
        let supports = |provider: &str| self.rt.upgrade().is_some_and(|rt| rt.executors.supports(provider));
        // Go: an unregistered provider with a base URL runs on the generic
        // OpenAI-compatible executor.
        if !supports(&credential.provider)
            && credential
                .attributes
                .get("base_url")
                .is_some_and(|u| !u.trim().is_empty())
        {
            credential.provider = "openai-compatibility".into();
        }
        if !supports(&credential.provider) {
            end_scope(&scope);
            return Err(reject(502, "executor_not_found", "executor not registered"));
        }
        let scope = match scope {
            Some(scope) => scope,
            None => registry
                .install(
                    pending.take().expect("pending"),
                    spec(auth_id.clone(), observed.clone(), false),
                )
                .map_err(install_failed)?,
        };
        // ponytail: the scope has no bound resource, so a drain waits for in-flight
        // executions instead of cancelling them (Go binds the request context).
        let bound = client.limiter_config().cancel_bound();
        let end: EndLease = Box::new(move || {
            let ticket = scope.end_with_release()?;
            Some(Box::pin(
                async move { ticket.wait(bound).await.map_err(|e| e.to_string()) },
            ))
        });
        // Go: Home's request-retry limit applies unless the request is pinned.
        let request_retry = response.request_retry.filter(|r| *r >= 0 && pinned.is_empty());
        Ok(RemoteGrant {
            credential,
            end,
            request_retry,
        })
    }
}

impl RemoteDispatch for Dispatcher {
    fn available(&self) -> bool {
        self.current().is_some_and(|b| b.client.heartbeat_ok())
    }

    fn request_log(&self, payload: Vec<u8>) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let Some(bundle) = self.current().filter(|b| b.client.heartbeat_ok()) else {
                return Ok(());
            };
            bundle
                .client
                .rpush_request_log(&payload)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn dispatch(&self, request: RemoteRequest) -> BoxFuture<'_, Result<RemoteGrant, RemoteError>> {
        Box::pin(self.pick(request))
    }

    /// Go `loadHomeModelEntries` with `GetModels` (headers and query lower-cased and
    /// joined as Go's `headersToLowerMap` / `queryToLowerMap`).
    fn models(
        &self,
        headers: Vec<(String, String)>,
        query: Vec<(String, String)>,
    ) -> BoxFuture<'_, Result<Vec<u8>, ModelsError>> {
        Box::pin(async move {
            let Some(bundle) = self.current() else {
                return Err(ModelsError::Unavailable);
            };
            let lower = |pairs: &[(String, String)]| {
                cpa_home::client::lower_map(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            };
            bundle
                .client
                .get_models(&lower(&headers), &lower(&query))
                .await
                .map_err(|e| ModelsError::Failed(e.to_string()))
        })
    }
}

/// Go `executorKeyFromAuth`: the provider key executors are registered under.
fn executor_key(provider: &str, label: &str, attributes: &BTreeMap<String, String>) -> String {
    let attr = |key: &str| attributes.get(key).map(|v| v.trim()).unwrap_or_default();
    if !attr("compat_name").is_empty() {
        let key = if attr("provider_key").is_empty() {
            attr("compat_name")
        } else {
            attr("provider_key")
        };
        return cpa_core::config::credentials::openai_compat_provider(key);
    }
    if provider.trim().eq_ignore_ascii_case("openai-compatibility") {
        return cpa_core::config::credentials::openai_compat_provider(label.trim());
    }
    match provider.trim().to_lowercase().as_str() {
        "kimi.com" => "kimi".into(),
        "kimi.ai" => "kimi-ai".into(),
        other => other.to_owned(),
    }
}

/// The dispatched Go `coreauth.Auth` as a runtime credential. It lives for one lease:
/// nothing persists it and the local scheduler never sees it.
fn credential(response: &DispatchResponse, requested: &str) -> Result<Credential, RemoteError> {
    let auth = &response.auth;
    let text = |key: &str| {
        auth.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let mut attributes: BTreeMap<String, String> = auth
        .get("attributes")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    let metadata: Map<String, Value> = auth
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let provider = text("provider").to_lowercase();
    let label = text("label");
    let key = executor_key(&provider, &label, &attributes);
    if provider.is_empty() || key.is_empty() {
        return Err(reject(502, "invalid_auth", "home returned auth without provider"));
    }
    let prefix = cpa_core::config::credentials::normalize_prefix(&text("prefix"));
    if !prefix.is_empty() {
        attributes.insert("prefix".into(), prefix);
    }
    if !text("proxy_url").is_empty() {
        attributes.insert("proxy_url".into(), text("proxy_url"));
    }
    if !response.model.trim().is_empty() {
        attributes.insert(
            cpa_server::remote::UPSTREAM_MODEL.into(),
            response.model.trim().to_owned(),
        );
    }
    // Go `homeForceMappingAliasResult`: the mapping applies when the alias Home mapped
    // from is the requested model (prefix removed, recognized suffixes ignored).
    let canonical = cpa_home::dispatch::canonical_concurrency_model_key;
    let raw_prefix = text("prefix");
    let unprefixed = match requested.strip_prefix(&format!("{raw_prefix}/")) {
        Some(rest) if !raw_prefix.is_empty() => rest,
        _ => requested,
    };
    let original = canonical(&response.original_alias);
    if response.force_mapping && !original.is_empty() && original == canonical(unprefixed) {
        attributes.insert(cpa_server::remote::FORCE_MAPPING.into(), "true".into());
        attributes.insert(
            cpa_server::remote::ORIGINAL_ALIAS.into(),
            response.original_alias.trim().to_owned(),
        );
    }
    let api_key = attributes.get("api_key").is_some_and(|k| !k.trim().is_empty());
    let id = text("id");
    let source = if api_key {
        Source::Config {
            section: "home".into(),
            index: 0,
        }
    } else {
        Source::File(PathBuf::from(
            attributes.get("path").cloned().unwrap_or_else(|| id.clone()),
        ))
    };
    let label = if label.is_empty() {
        metadata
            .get("email")
            .and_then(Value::as_str)
            .filter(|e| !e.is_empty())
            .unwrap_or(&provider)
            .to_owned()
    } else {
        label
    };
    Ok(Credential {
        id,
        provider: key,
        source,
        disabled: auth.get("disabled").and_then(Value::as_bool).unwrap_or(false),
        label,
        attributes,
        metadata,
        revision: 0,
    })
}

/// Go `startHomeSubscriber` / `runHomeSubscriber`: one Home lifetime after another until
/// `shutdown`, each subscribed to config, publishing dispatch once its first config
/// applied, and releasing leases through one flusher.
pub fn spawn_subscriber(
    home: HomeConfig,
    rt: Arc<Runtime>,
    dispatcher: Arc<Dispatcher>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut registry = Registry::new();
        let mut flusher = ReleaseFlusher::new();
        registry.set_release_sink(Some(flusher.sink()));
        let mut previous: Option<Client> = None;
        let bound = || CredentialConcurrency::default().with_defaults().cancel_bound();
        while !shutdown.is_cancelled() {
            let client = match &previous {
                Some(client) => client.new_lifetime(),
                None => Client::new(home.clone()),
            };
            client.set_managed_lifetime(true);
            flusher.set_sender(Some(Arc::new(client.clone())));
            let release_stop = CancellationToken::new();
            let release_task = tokio::spawn({
                let flusher = flusher.clone();
                let stop = release_stop.clone();
                async move { flusher.run(stop).await }
            });
            let lifetime = shutdown.child_token();
            let (configs, mut latest) = tokio::sync::watch::channel::<Option<Vec<u8>>>(None);
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
            let published = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let worker = tokio::spawn({
                let (rt, dispatcher, client, registry) =
                    (rt.clone(), dispatcher.clone(), client.clone(), registry.clone());
                let (lifetime, published) = (lifetime.clone(), published.clone());
                async move {
                    tokio::select! {
                        _ = lifetime.cancelled() => return,
                        ready = ready_rx => if ready.is_err() { return },
                    }
                    loop {
                        let raw = latest.borrow_and_update().clone();
                        if let Some(raw) = raw {
                            let base = rt.config();
                            match overlay(&raw, Some(base.as_ref())) {
                                Ok(cfg) => {
                                    rt.publish_config(cfg);
                                    if !published.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                        dispatcher.install(client.clone(), registry.clone());
                                        cpa_home::set_current(Some(client.clone()));
                                    }
                                }
                                Err(error) => tracing::warn!("failed to stage home config; retrying: {error:#}"),
                            }
                        }
                        tokio::select! {
                            _ = lifetime.cancelled() => return,
                            changed = latest.changed() => if changed.is_err() { return },
                        }
                    }
                }
            });
            let run = client
                .run_config_subscriber_lifetime(
                    &lifetime,
                    |raw| {
                        let parsed = Config::parse(&String::from_utf8_lossy(raw)).map_err(|e| {
                            tracing::warn!("failed to parse home config payload: {e:#}");
                            e.to_string()
                        })?;
                        CredentialConcurrency::from_document(&parsed.document)
                            .map_err(|e| e.to_string())
                            .and_then(|c| client.set_lifecycle_config(c).map_err(|e| e.to_string()))
                            .inspect_err(|e| tracing::warn!("failed to apply Home lifecycle config: {e}"))?;
                        registry.observe_barrier(client.limiter_config().observation_barrier_revision);
                        configs.send_replace(Some(raw.to_vec()));
                        Ok(())
                    },
                    move || {
                        let _ = ready_tx.send(());
                    },
                )
                .await;
            lifetime.cancel();
            let _ = worker.await;
            dispatcher.clear(&client);
            cpa_home::clear_current_if(&client);
            let bound = client.limiter_config().cancel_bound().max(bound());
            let retry = run.is_err() && !shutdown.is_cancelled();
            if retry {
                release_stop.cancel();
                let _ = release_task.await;
                client.close();
                if let Err(error) = registry.wait_pending(bound).await {
                    tracing::error!("failed to settle pending Home dispatches before subscriber replacement: {error}");
                    return;
                }
                let error = run.as_ref().err();
                let legacy = error.is_some_and(|e| e.is_legacy_membership_protocol());
                if legacy {
                    client.enable_legacy_membership();
                }
                if client.ambiguous_dispatch()
                    || error.is_some_and(|e| e.is_membership_takeover_unavailable())
                    || legacy
                    || client.legacy_membership()
                {
                    registry.set_release_sink(None);
                    if let Err(error) = registry.drain(bound).await {
                        tracing::error!("failed to drain Home executions after unsafe subscriber replacement: {error}");
                        return;
                    }
                    client.suppress_takeover();
                    registry = Registry::new();
                    flusher = ReleaseFlusher::new();
                    registry.set_release_sink(Some(flusher.sink()));
                }
                if let Some(error) = error {
                    tracing::warn!("home config subscription lifetime ended: {error}");
                }
                if !published.load(std::sync::atomic::Ordering::SeqCst) {
                    tokio::select! {
                        _ = shutdown.cancelled() => return,
                        _ = tokio::time::sleep(PRE_ACK_RETRY) => {}
                    }
                }
                previous = Some(client);
                continue;
            }
            let drained = registry.drain(bound).await;
            let flushed = match &drained {
                Ok(()) => flusher.flush_all(bound).await,
                Err(_) => Ok(()),
            };
            release_stop.cancel();
            let _ = release_task.await;
            client.close();
            if let Err(error) = drained {
                tracing::error!("failed to drain Home execution registry: {error}");
            } else if let Err(error) = flushed {
                tracing::error!("failed to flush Home concurrency releases: {error}");
            }
            return;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_home::fake::{self, FakeHome, Reply};
    use std::sync::Mutex;

    const ACK: &str = "*3\r\n$9\r\nsubscribe\r\n$6\r\nconfig\r\n:1\r\n";

    /// A Claude upstream on loopback that records what reached it.
    async fn upstream(seen: Arc<Mutex<Vec<(String, String)>>>) -> String {
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move |headers: axum::http::HeaderMap, body: String| {
                let seen = seen.clone();
                async move {
                    // Off Anthropic's host the key goes as a bearer token, as in Go.
                    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default();
                    let key = match header("x-api-key") {
                        "" => header("authorization").trim_start_matches("Bearer ").to_owned(),
                        key => key.to_owned(),
                    };
                    let model = serde_json::from_str::<Value>(&body).unwrap()["model"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    let fail = key == "sk-bad";
                    seen.lock().unwrap().push((key, model));
                    if fail {
                        return (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            axum::Json(
                                serde_json::json!({"type": "error", "error": {"type": "api_error", "message": "boom"}}),
                            ),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({
                            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-upstream",
                            "content": [{"type": "text", "text": "hi"}],
                            "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}
                        })),
                    )
                }
            }),
        );
        let app = app.route(
            "/chat/completions",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({
                    "id": "c1", "object": "chat.completion", "created": 1, "model": "compat-up",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    /// A Home whose RPOP replies come from `replies` in order (nil once empty).
    async fn scripted(config: &'static str, replies: Vec<String>) -> FakeHome {
        let replies = Mutex::new(std::collections::VecDeque::from(replies));
        FakeHome::start(move |args| match args[0].to_lowercase().as_str() {
            "get" => fake::bulk(config),
            "subscribe" => fake::raw(ACK),
            "ping" => fake::raw("+PONG\r\n"),
            "rpop" => match replies.lock().unwrap().pop_front() {
                Some(reply) => fake::bulk(reply),
                None => fake::raw("$-1\r\n"),
            },
            "lpush" => fake::raw(":1\r\n"),
            _ => fake::raw("-ERR unexpected\r\n"),
        })
        .await
    }

    fn accounted(id: &str, key: &str, upstream: &str) -> String {
        format!(
            r#"{{"model":"claude-up","auth_index":"{id}","concurrency":{{"accounted":true,"credential_id":"{id}","model":"claude-up"}},"auth":{{"id":"{id}","provider":"claude","attributes":{{"api_key":"{key}","base_url":"{upstream}"}}}}}}"#
        )
    }

    fn releases(home: &FakeHome) -> Vec<Value> {
        home.commands()
            .into_iter()
            .filter(|c| c[0].eq_ignore_ascii_case("lpush") && c[1] == "concurrency-release")
            .map(|c| serde_json::from_str(&c[2]).unwrap())
            .collect()
    }

    fn rpops(home: &FakeHome) -> Vec<Value> {
        home.commands()
            .into_iter()
            .filter(|c| c[0] == "rpop")
            .map(|c| serde_json::from_str(&c[1]).unwrap())
            .collect()
    }

    async fn ask_with_headers(base: &str, path: &str, body: Value) -> (u16, Option<String>, String) {
        let response = wreq::Client::new()
            .post(format!("{base}{path}"))
            .header("authorization", "Bearer client-key")
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .map(|v| v.to_str().unwrap().to_owned());
        (status, retry_after, response.text().await.unwrap())
    }

    /// The node under test: a runtime routed through the dispatcher, the Home
    /// subscriber, and the public router with the heartbeat gate.
    async fn node(home: &FakeHome) -> (String, Arc<Runtime>, CancellationToken, tokio::task::JoinHandle<()>) {
        let executors = cpa_exec::Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        };
        let config = overlay(b"port: 0\n", None).unwrap();
        let rt = Arc::new(cpa_server::testing::runtime(config, vec![], executors));
        let dispatcher = Arc::new(Dispatcher::new(&rt));
        rt.set_remote_dispatch(Some(dispatcher.clone()));
        let shutdown = CancellationToken::new();
        let task = spawn_subscriber(home.config(), rt.clone(), dispatcher, shutdown.clone());
        let app = cpa_server::router(rt.clone()).layer(axum::middleware::from_fn_with_state(
            rt.clone(),
            cpa_server::remote::gate,
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        (base, rt, shutdown, task)
    }

    async fn ask(base: &str, model: &str) -> (u16, String) {
        let response = wreq::Client::new()
            .post(format!("{base}/v1/messages"))
            .header("authorization", "Bearer client-key")
            .json(
                &serde_json::json!({"model": model, "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]}),
            )
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    async fn eventually(what: &str, check: impl Fn() -> bool) {
        let waited = tokio::time::timeout(Duration::from_secs(10), async {
            while !check() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(waited.is_ok(), "timed out waiting for {what}");
    }

    #[test]
    fn overlay_forces_go_home_runtime_settings() {
        let raw = b"port: 0\napi-keys: [k1]\ndisable-cooling: false\nsave-cooldown-status: true\nrequest-retry: 4\nremote-management: {allow-remote: true}\n";
        let first = overlay(raw, None).unwrap();
        assert_eq!(first.port, 8317, "NormalizeHomePort");
        assert!(first.api_keys.is_empty());
        assert!(first.routing.cooldown.disable_cooling);
        assert!(!first.routing.cooldown.save_cooldown_status);
        assert!(!first.management.allow_remote && first.management.disable_control_panel);
        assert_eq!(first.routing.retry.request_retry, 4, "everything else comes from Home");
        let next = overlay(b"port: 9999\nhost: 10.0.0.1\nrequest-retry: 2\n", Some(&first)).unwrap();
        assert_eq!((next.port, next.host.as_str()), (8317, ""), "the listener stays");
        assert_eq!(next.routing.retry.request_retry, 2);
    }

    #[test]
    fn dispatched_auths_become_runtime_credentials() {
        let raw = br#"{"model":"gpt-up","auth_index":"i1","force_mapping":true,"original_alias":"pretty","auth":{"id":"c1","provider":"openai-compatibility","label":"Acme","prefix":"team","proxy_url":"socks5://p","attributes":{"api_key":"k","base_url":"http://127.0.0.1:1","compat_name":"acme"},"metadata":{"x":1}}}"#;
        let response = DispatchResponse::parse(raw).unwrap();
        let c = credential(&response, "team/pretty(high)").unwrap();
        assert_eq!(c.provider, "openai-compatible-acme", "Go executorKeyFromAuth");
        assert_eq!(c.attributes["prefix"], "team");
        assert_eq!(c.attributes["proxy_url"], "socks5://p");
        assert_eq!(c.attributes[cpa_server::remote::UPSTREAM_MODEL], "gpt-up");
        assert_eq!(c.attributes[cpa_server::remote::ORIGINAL_ALIAS], "pretty");
        assert!(matches!(c.source, Source::Config { .. }));
        assert_eq!(c.label, "Acme");
        // Go `homeForceMappingAliasResult`: only recognized suffixes are ignored.
        let c = credential(&response, "pretty(custom)").unwrap();
        assert!(!c.attributes.contains_key(cpa_server::remote::FORCE_MAPPING));
        assert_eq!(executor_key("kimi.com", "", &BTreeMap::new()), "kimi");
        let oauth =
            DispatchResponse::parse(br#"{"id":"a.json","provider":"Claude","metadata":{"email":"e@x"}}"#).unwrap();
        let c = credential(&oauth, "m").unwrap();
        assert_eq!(
            (c.id.as_str(), c.provider.as_str(), c.label.as_str()),
            ("a.json", "claude", "e@x")
        );
        assert!(credential(&DispatchResponse::parse(br#"{"id":"x"}"#).unwrap(), "m").is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn requests_run_on_home_credentials_and_release_their_leases() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let upstream = upstream(seen.clone()).await;
        let reply = format!(
            r#"{{"model":"claude-upstream","auth_index":"cred-1","concurrency":{{"accounted":true,"credential_id":"cred-1","model":"claude-upstream"}},"auth":{{"id":"cred-1","provider":"claude","attributes":{{"api_key":"sk-home-fake","base_url":"{upstream}"}}}}}}"#
        );
        let home = FakeHome::start(move |args| match args[0].to_lowercase().as_str() {
            "get" => fake::bulk("port: 0\ncredentials:\n  concurrency:\n    cpa-flush-interval: 20ms\n"),
            "subscribe" => fake::raw(ACK),
            "ping" => fake::raw("+PONG\r\n"),
            "rpop" => fake::bulk(&reply),
            "lpush" => fake::raw(":1\r\n"),
            _ => fake::raw("-ERR unexpected\r\n"),
        })
        .await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;

        let (status, body) = ask(&base, "claude-alias").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![("sk-home-fake".to_owned(), "claude-upstream".to_owned())],
            "Home's credential and upstream model reach the provider"
        );
        let rpop = home.commands().into_iter().find(|c| c[0] == "rpop").unwrap();
        let request: Value = serde_json::from_str(&rpop[1]).unwrap();
        assert_eq!(request["model"], "claude-alias");
        assert_eq!(
            (request["count"].as_i64(), request["retry_round"].as_i64()),
            (Some(1), Some(0))
        );
        assert_eq!(request["headers"]["authorization"], "Bearer client-key");
        assert!(request.get("excluded_auth_ids").is_none());
        // The accounted lease is released once, with sequence 1.
        eventually("release frame", || {
            home.commands()
                .iter()
                .any(|c| c[0].eq_ignore_ascii_case("lpush") && c[1] == "concurrency-release")
        })
        .await;
        let frame = home
            .commands()
            .into_iter()
            .find(|c| c[0].eq_ignore_ascii_case("lpush") && c[1] == "concurrency-release")
            .unwrap();
        let frame: Value = serde_json::from_str(&frame[2]).unwrap();
        assert_eq!(
            (
                frame["credential_id"].as_str(),
                frame["model"].as_str(),
                frame["release_seq"].as_i64()
            ),
            (Some("cred-1"), Some("claude-upstream"), Some(1))
        );
        shutdown.cancel();
        task.await.unwrap();
        assert!(
            !rt.remote_dispatch().unwrap().available(),
            "the bundle is cleared at shutdown"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn home_errors_reach_the_client_and_the_gate_closes_without_home() {
        let home = FakeHome::start(|args| match args[0].to_lowercase().as_str() {
            "get" => fake::bulk("port: 0\n"),
            "subscribe" => fake::raw(ACK),
            "ping" => fake::raw("+PONG\r\n"),
            "rpop" => fake::bulk(r#"{"error":{"type":"user_credits_insufficient","message":"no credits left"}}"#),
            _ => fake::raw("-ERR unexpected\r\n"),
        })
        .await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let (status, body) = ask(&base, "claude-x").await;
        assert_eq!(status, 402, "{body}");
        assert!(body.contains("user_credits_insufficient: no credits left"), "{body}");
        shutdown.cancel();
        task.await.unwrap();
        // Go `homeHeartbeatMiddleware`: no Home, no service.
        let (status, body) = ask(&base, "claude-x").await;
        assert_eq!((status, body.as_str()), (503, ""));
        let _ = Reply::Hang;
    }

    /// Go `executeHomeOnce` after a failed attempt: the lease is released and Home
    /// acknowledges it before the next RPOP, which excludes the tried credential and
    /// counts as the round's second pick.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_credential_is_released_before_the_next_pick() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let upstream = upstream(seen.clone()).await;
        let auth = |id: &str, key: &str| {
            format!(
                r#"{{"model":"claude-up","auth_index":"{id}","concurrency":{{"accounted":true,"credential_id":"{id}","model":"claude-up"}},"auth":{{"id":"{id}","provider":"claude","attributes":{{"api_key":"{key}","base_url":"{upstream}"}}}}}}"#
            )
        };
        let replies = Mutex::new(vec![auth("cred-2", "sk-good"), auth("cred-1", "sk-bad")]);
        let home = FakeHome::start(move |args| match args[0].to_lowercase().as_str() {
            "get" => fake::bulk("port: 0\n"),
            "subscribe" => fake::raw(ACK),
            "ping" => fake::raw("+PONG\r\n"),
            "rpop" => match replies.lock().unwrap().pop() {
                Some(reply) => fake::bulk(reply),
                None => fake::raw("$-1\r\n"),
            },
            "lpush" => fake::raw(":1\r\n"),
            _ => fake::raw("-ERR unexpected\r\n"),
        })
        .await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let (status, body) = ask(&base, "claude-x").await;
        assert_eq!(status, 200, "{body}");
        let keys: Vec<String> = seen.lock().unwrap().iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, vec!["sk-bad", "sk-good"]);
        let commands = home.commands();
        let position = |pred: &dyn Fn(&Vec<String>) -> bool| commands.iter().position(pred).unwrap();
        let rpops: Vec<&Vec<String>> = commands.iter().filter(|c| c[0] == "rpop").collect();
        let second: Value = serde_json::from_str(&rpops[1][1]).unwrap();
        assert_eq!(second["excluded_auth_ids"], serde_json::json!(["cred-1"]));
        assert_eq!(
            (second["count"].as_i64(), second["retry_round"].as_i64()),
            (Some(1), Some(0)),
            "excluded IDs keep count at 1"
        );
        let release_1 = position(&|c| c[0].eq_ignore_ascii_case("lpush") && c[2].contains("\"cred-1\""));
        let second_rpop = commands
            .iter()
            .enumerate()
            .filter(|(_, c)| c[0] == "rpop")
            .nth(1)
            .map(|(i, _)| i)
            .unwrap();
        assert!(
            release_1 < second_rpop,
            "release acknowledged before redispatch: {commands:?}"
        );
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go `executeHome`: a Home cooldown with a request-retry budget is waited out and
    /// the next round asks again with `retry_round: 1`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn home_cooldowns_start_a_new_round_after_the_wait() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let upstream = upstream(seen.clone()).await;
        let cooldown =
            r#"{"error":{"type":"model_cooldown","message":"cooling","retry_after_ms":300,"request_retry":1}}"#;
        let home = scripted(
            "port: 0\nrouting: {retry: {request-retry: 0, max-retry-interval: 5}}\n",
            vec![cooldown.into(), accounted("cred-1", "sk-good", &upstream)],
        )
        .await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let started = std::time::Instant::now();
        let (status, body) = ask(&base, "claude-x").await;
        assert_eq!(status, 200, "{body}");
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "waited for the cooldown"
        );
        let rounds: Vec<i64> = rpops(&home)
            .iter()
            .map(|r| r["retry_round"].as_i64().unwrap())
            .collect();
        assert_eq!(rounds, vec![0, 1]);
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go `shouldReturnLastErrorOnPickFailure`: a Home rejection after an upstream
    /// failure is what the client sees, not the older upstream error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_home_rejection_is_not_masked_by_an_earlier_upstream_failure() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let upstream = upstream(seen.clone()).await;
        let home = scripted(
            "port: 0\n",
            vec![
                accounted("cred-1", "sk-bad", &upstream),
                r#"{"error":{"type":"user_credits_insufficient","message":"no credits left"}}"#.into(),
            ],
        )
        .await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let (status, body) = ask(&base, "claude-x").await;
        assert_eq!(status, 402, "{body}");
        assert!(body.contains("user_credits_insufficient: no credits left"), "{body}");
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go `executeHomeOnce`: a credential Home hands out again is released unused and
    /// the round ends with the earlier upstream failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_repeated_credential_is_released_and_ends_the_round() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let upstream = upstream(seen.clone()).await;
        let home = scripted(
            "port: 0\nrouting: {retry: {request-retry: 0}}\n",
            vec![
                accounted("cred-1", "sk-bad", &upstream),
                accounted("cred-1", "sk-bad", &upstream),
            ],
        )
        .await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let (status, body) = ask(&base, "claude-x").await;
        assert_eq!(status, 500, "{body}");
        assert_eq!(seen.lock().unwrap().len(), 1, "the duplicate never executes");
        assert_eq!(rpops(&home).len(), 2);
        eventually("both releases", || releases(&home).len() == 2).await;
        let sequences: Vec<i64> = releases(&home)
            .iter()
            .map(|f| f["release_seq"].as_i64().unwrap())
            .collect();
        assert_eq!(sequences, vec![1, 2]);
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go `HomeConcurrencyBusyError`: never retried, and its safe `Retry-After`
    /// reaches the client; concurrency validation failures use their own code and
    /// still release the lease.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn busy_and_invalid_concurrency_replies_follow_go() {
        let busy = r#"{"error":{"type":"credential_concurrency_exceeded","message":"busy","retry_after_ms":1500}}"#;
        let mismatch = r#"{"model":"claude-up","auth_index":"other","concurrency":{"accounted":true,"credential_id":"cred-9","model":"claude-up"},"auth":{"id":"cred-9","provider":"claude"}}"#;
        let home = scripted("port: 0\n", vec![busy.into(), mismatch.into()]).await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let body =
            serde_json::json!({"model": "claude-x", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]});
        let (status, retry_after, text) = ask_with_headers(&base, "/v1/messages", body.clone()).await;
        assert_eq!((status, retry_after.as_deref()), (429, Some("2")), "{text}");
        assert!(text.contains("credential_concurrency_exceeded: busy"), "{text}");
        assert_eq!(rpops(&home).len(), 1, "busy is not retried");
        let (status, _, text) = ask_with_headers(&base, "/v1/messages", body).await;
        assert_eq!(status, 502, "{text}");
        assert!(
            text.contains("invalid_home_concurrency: Home concurrency identity does not match dispatched auth"),
            "{text}"
        );
        eventually("release of the rejected lease", || {
            releases(&home).iter().any(|f| f["credential_id"] == "cred-9")
        })
        .await;
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go: an unregistered provider with a base URL runs on the OpenAI-compatible
    /// executor.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_providers_with_a_base_url_run_openai_compatible() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let upstream = upstream(seen.clone()).await;
        let reply = format!(
            r#"{{"model":"compat-up","auth":{{"id":"m1","provider":"mystery","attributes":{{"api_key":"k","base_url":"{upstream}"}}}}}}"#
        );
        let home = scripted("port: 0\n", vec![reply]).await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let body = serde_json::json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
        let (status, _, text) = ask_with_headers(&base, "/v1/chat/completions", body).await;
        assert_eq!(status, 200, "{text}");
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go `handleHomeModels`, `handleGrokModels` and `handleHomeGeminiModels`: every
    /// catalog route serves what Home answers for this client.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn model_routes_serve_the_home_catalog() {
        let catalog = r#"{"xai":[{"id":"grok-4","display_name":"Grok 4","context_length":256000,"owned_by":"xai"}],"claude":[{"id":"claude-x","owned_by":"anthropic","created":1700000000}]}"#;
        let home = FakeHome::start(move |args| match args[0].to_lowercase().as_str() {
            "get" if args[1] == "config" => fake::bulk("port: 0\n"),
            "get" if args[1].contains("x-bad") => {
                fake::bulk(r#"{"error":{"type":"no_credentials","message":"who are you"}}"#)
            }
            "get" => fake::bulk(catalog),
            "subscribe" => fake::raw(ACK),
            "ping" => fake::raw("+PONG\r\n"),
            _ => fake::raw("-ERR unexpected\r\n"),
        })
        .await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let get = |path: &str, headers: &[(&str, &str)]| {
            let mut request = wreq::Client::new().get(format!("{base}{path}"));
            for (k, v) in headers {
                request = request.header(*k, *v);
            }
            async move {
                let response = request.send().await.unwrap();
                (response.status().as_u16(), response.text().await.unwrap())
            }
        };
        let (status, body) = get("/v1/models?key=k1", &[]).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"data":[{"created":1700000000,"id":"claude-x","object":"model","owned_by":"anthropic"},{"id":"grok-4","object":"model","owned_by":"xai"}],"object":"list"}"#
        );
        let key = home
            .commands()
            .into_iter()
            .find(|c| c[0] == "get" && c[1].starts_with('{'))
            .unwrap();
        let key: Value = serde_json::from_str(&key[1]).unwrap();
        assert_eq!(
            (key["type"].as_str(), key["query"]["key"].as_str()),
            (Some("models"), Some("k1"))
        );
        let (_, body) = get("/v1/models", &[("user-agent", "grok-shell/1.2")]).await;
        assert!(
            body.starts_with(r#"{"object":"list","data":[{"id":"claude-x","model":"claude-x","name":"claude-x","#),
            "{body}"
        );
        assert!(body.contains(r#""name":"Grok 4","context_window":256000"#), "{body}");
        let (_, body) = get("/v1/models", &[("anthropic-version", "2023-06-01")]).await;
        let claude: Value = serde_json::from_str(&body).unwrap();
        // Go `claudemodels.BuildResponse` sorts by display name: "Grok 4" < "claude-x".
        assert_eq!(claude["data"][0]["display_name"], "Grok 4");
        assert_eq!(claude["data"][1]["created_at"], "2023-11-14T22:13:20Z");
        assert_eq!(claude["data"][1]["max_input_tokens"], 200000);
        let (status, body) = get("/v1beta/models/grok-4", &[]).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body,
            r#"{"description":"Grok 4","displayName":"Grok 4","name":"models/grok-4","supportedGenerationMethods":["generateContent"]}"#
        );
        let (status, body) = get("/v1beta/models", &[]).await;
        assert_eq!(status, 200);
        assert!(body.contains("models/claude-x"), "{body}");
        let (status, body) = get("/v1/models", &[("x-bad", "1")]).await;
        assert_eq!(
            (status, body.as_str()),
            (
                401,
                r#"{"error":{"message":"who are you","type":"authentication_error"}}"#
            )
        );
        shutdown.cancel();
        task.await.unwrap();
        let (status, _) = get("/v1/models", &[]).await;
        assert_eq!(status, 503, "the heartbeat gate closes without Home");
    }

    /// Go `SafeResponseHeaders`: a Home cooldown that ends the request exposes its
    /// rounded-up delay.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_final_home_cooldown_sends_its_retry_after() {
        let cooldown =
            r#"{"error":{"type":"model_cooldown","message":"cooling","retry_after_ms":1500,"request_retry":0}}"#;
        let home = scripted("port: 0\n", vec![cooldown.into()]).await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let body =
            serde_json::json!({"model": "claude-x", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]});
        let (status, retry_after, text) = ask_with_headers(&base, "/v1/messages", body).await;
        assert_eq!((status, retry_after.as_deref()), (429, Some("2")), "{text}");
        assert!(text.contains("model_cooldown: cooling"), "{text}");
        assert_eq!(rpops(&home).len(), 1);
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go sets `maxBootstrapRetries = 0` in Home mode: a stream that fails before its
    /// first byte is not re-run by the handler; the Home round decides alone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn home_streams_get_no_handler_bootstrap_retries() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let upstream = upstream(seen.clone()).await;
        let home = scripted(
            "port: 0\nrouting: {retry: {request-retry: 0}}\nstreaming: {bootstrap-retries: 2}\n",
            vec![accounted("cred-1", "sk-bad", &upstream)],
        )
        .await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let body = serde_json::json!({"model": "claude-x", "max_tokens": 8, "stream": true, "messages": [{"role": "user", "content": "hi"}]});
        let (status, _, text) = ask_with_headers(&base, "/v1/messages", body).await;
        assert_eq!(status, 500, "{text}");
        assert_eq!(seen.lock().unwrap().len(), 1, "one execution");
        // The failed credential, then Home's empty answer that ends the round.
        assert_eq!(rpops(&home).len(), 2);
        shutdown.cancel();
        task.await.unwrap();
    }
}
