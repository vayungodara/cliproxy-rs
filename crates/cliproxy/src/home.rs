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
    /// Go `Manager.homeSessionAliases`.
    sessions: cpa_home::session_alias::SessionAliases,
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
            sessions: Default::default(),
        }
    }

    /// Go `homeDispatchSessionIDs` once the session is extracted: a fallback that is the
    /// session's parent is sent as the parent; any other fallback names the same
    /// conversation, so both join one alias group whose canonical ID is sent.
    fn dispatch_sessions(&self, primary: &str, fallback: &str, ttl: Duration) -> (String, String) {
        use cpa_common::session::bound_session_identity as bound;
        let (primary, fallback) = (primary.trim(), fallback.trim());
        if primary.is_empty() {
            return (String::new(), String::new());
        }
        let (mut parent, mut alias) = ("", "");
        if !fallback.is_empty() && fallback != primary {
            if cpa_home::session_alias::is_hierarchy_parent(primary, fallback) {
                parent = fallback;
            } else {
                alias = fallback;
            }
        }
        let now = std::time::Instant::now();
        let canonical = self.sessions.canonical(primary, alias, ttl, now);
        let mut parent = if parent.is_empty() || parent == canonical || parent == primary || parent == alias {
            String::new()
        } else {
            let parent = self.sessions.canonical(parent, "", ttl, now);
            if parent == canonical { String::new() } else { parent }
        };
        let canonical = bound(&canonical);
        if !parent.is_empty() {
            parent = bound(&parent);
        }
        if canonical == parent {
            parent.clear();
        }
        (canonical, parent)
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
        // ponytail: no Merkle LCP session matching; Go consults it when no session
        // identifier was sent.
        let ttl = self.rt.upgrade().map_or(cpa_home::session_alias::DEFAULT_TTL, |rt| {
            rt.policy().session_affinity_ttl
        });
        let (session_id, parent_session_id) =
            self.dispatch_sessions(&request.session_id, &request.parent_session_id, ttl);
        let dispatch = DispatchRequest {
            model: model.clone(),
            session_id,
            parent_session_id,
            headers: request.headers.clone(),
            count: request.count,
            credential_policy: request.credential_policy.trim().to_owned(),
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
        // Go `selection.CanonicalSessionID` / `ParentSessionID`: what this pick sent.
        if !dispatch.session_id.is_empty() {
            credential
                .attributes
                .insert(cpa_server::remote::SESSION.into(), dispatch.session_id.clone());
            credential.attributes.insert(
                cpa_server::remote::PARENT_SESSION.into(),
                dispatch.parent_session_id.clone(),
            );
        }
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
        // Go `newHomeDispatchSelection`: the scope's bound resource cancels the
        // execution, so a draining registry stops in-flight work instead of waiting.
        let (cancel_tx, cancel) = tokio::sync::watch::channel(false);
        if scope
            .bind(move || {
                let _ = cancel_tx.send(true);
            })
            .is_err()
        {
            scope.end();
            return Err(reject(503, "home_unavailable", "home execution registry unavailable"));
        }
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
            cancel: Some(cancel),
            request_retry,
            user_api_key: response.user_api_key.trim().to_owned(),
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
    attributes.insert(cpa_server::remote::PROVIDER.into(), text("provider"));
    let prefix = cpa_core::config::credentials::normalize_prefix(&text("prefix"));
    if !prefix.is_empty() {
        attributes.insert("prefix".into(), prefix);
    }
    if !text("proxy_url").is_empty() {
        attributes.insert("proxy_url".into(), text("proxy_url"));
    }
    // Go `auth.Index = homeAuthIndex`.
    if !response.auth_index.trim().is_empty() {
        attributes.insert(
            cpa_core::config::credentials::HOME_AUTH_INDEX.into(),
            response.auth_index.trim().to_owned(),
        );
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
    let mut credential = Credential {
        id,
        provider: key,
        source,
        disabled: auth.get("disabled").and_then(Value::as_bool).unwrap_or(false),
        label,
        attributes,
        metadata,
        revision: 0,
    };
    // Go `attachResolvedHomeModelInfo` inputs, fixed for the lease: Home's definition of
    // the model (`registryModelInfo` is nil without an ID) and the `credential_options`
    // entry for the dispatched upstream model reached through the route model. Only the
    // dispatcher sets these attributes.
    for key in [
        cpa_server::remote::MODEL_INFO,
        cpa_core::config::credentials::HOME_MODEL_OPTIONS,
    ] {
        credential.attributes.remove(key);
    }
    let info = response.model_info.as_ref().filter(|info| {
        info.get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.trim().is_empty())
    });
    if let Some(info) = info {
        credential
            .attributes
            .insert(cpa_server::remote::MODEL_INFO.into(), info.to_string());
    }
    // ponytail: without a dispatched model or model info Go uses the attempt's model;
    // a Home pick without either is the route model, which this uses.
    let upstream = match (
        response.model.trim(),
        info.and_then(|i| i.get("id")).and_then(Value::as_str),
    ) {
        ("", Some(id)) => id.trim(),
        ("", None) => requested,
        (model, _) => model,
    };
    if let Some(options) = cpa_home::dispatch::credential_model_options(&credential, upstream, requested) {
        credential.attributes.insert(
            cpa_core::config::credentials::HOME_MODEL_OPTIONS.into(),
            Value::Object(options).to_string(),
        );
    }
    Ok(credential)
}

/// Go `startHomeUsageForwarder`: queued usage records go to Home while the heartbeat
/// holds; a failed push puts the rest back and waits a second.
async fn forward_usage(rt: Arc<Runtime>, client: Client, stop: CancellationToken) {
    let sleep = |wait: Duration| {
        let stop = stop.clone();
        async move {
            tokio::select! {
                _ = stop.cancelled() => false,
                _ = tokio::time::sleep(wait) => true,
            }
        }
    };
    while !stop.is_cancelled() {
        if !client.heartbeat_ok() {
            if !sleep(Duration::from_secs(1)).await {
                return;
            }
            continue;
        }
        let items = rt.usage_queue().pop_oldest(64);
        if items.is_empty() {
            if !sleep(Duration::from_millis(500)).await {
                return;
            }
            continue;
        }
        for (i, item) in items.iter().enumerate() {
            // Go passes the lifetime context to every push: once it is cancelled the
            // push fails, and the rest goes back to the queue.
            if stop.is_cancelled() || client.lpush_usage(item).await.is_err() {
                for rest in &items[i..] {
                    rt.usage_queue().enqueue(rest.clone());
                }
                if !sleep(Duration::from_secs(1)).await {
                    return;
                }
                break;
            }
        }
    }
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
        // Go `StartHomeAppLogForwarder` (process-wide logrus hook): bound to each
        // published lifetime, stopped with the subscriber.
        let app_log = cpa_home::applog::Forwarder::start(0);
        let hook = cpa_server::logging::add_hook({
            let app_log = app_log.clone();
            Arc::new(move |e: &cpa_server::logging::Entry<'_>| app_log.fire(e.line, e.level, e.time, e.request_id))
        });
        run_lifetimes(home, rt, dispatcher, shutdown, app_log.clone()).await;
        cpa_server::logging::remove_hook(hook);
        app_log.stop().await;
    })
}

async fn run_lifetimes(
    home: HomeConfig,
    rt: Arc<Runtime>,
    dispatcher: Arc<Dispatcher>,
    shutdown: CancellationToken,
    app_log: Arc<cpa_home::applog::Forwarder>,
) {
    let mut registry = Registry::new();
    let mut flusher = ReleaseFlusher::new();
    registry.set_release_sink(Some(flusher.sink()));
    let mut previous: Option<Client> = None;
    let bound = || CredentialConcurrency::default().with_defaults().cancel_bound();
    // Go keeps the publisher settings on the auth manager, across lifetimes.
    let publisher = cpa_home::inflight::PublisherSettings::default();
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
            let (rt, dispatcher, client, registry, app_log) = (
                rt.clone(),
                dispatcher.clone(),
                client.clone(),
                registry.clone(),
                app_log.clone(),
            );
            let (lifetime, published, publisher) = (lifetime.clone(), published.clone(), publisher.clone());
            async move {
                // Go `runHomeConfigWorkerWithSupervisor`: the lifetime's in-flight
                // publisher and usage forwarder, awaited before it is torn down.
                let mut helpers = Vec::new();
                tokio::select! {
                    _ = lifetime.cancelled() => return helpers,
                    ready = ready_rx => if ready.is_err() { return helpers },
                }
                loop {
                    let raw = latest.borrow_and_update().clone();
                    if let Some(raw) = raw {
                        let base = rt.config();
                        match overlay(&raw, Some(base.as_ref())) {
                            Ok(cfg) => {
                                // Go `redisqueue.SetEnabled(... || cfg.Home.Enabled)`.
                                rt.usage_queue().configure(true, &cfg);
                                rt.publish_config(cfg);
                                if !published.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                    // The KV client goes first: an attempt that
                                    // receives a Home credential must also see
                                    // Home KV for its identities and caches.
                                    cpa_home::set_current(Some(client.clone()));
                                    dispatcher.install(client.clone(), registry.clone());
                                    app_log.bind(&client);
                                    helpers.push(tokio::spawn(cpa_home::inflight::run_publisher(
                                        client.clone(),
                                        registry.clone(),
                                        publisher.clone(),
                                        lifetime.clone(),
                                    )));
                                    helpers.push(tokio::spawn(forward_usage(
                                        rt.clone(),
                                        client.clone(),
                                        lifetime.clone(),
                                    )));
                                }
                            }
                            Err(error) => tracing::warn!("failed to stage home config; retrying: {error:#}"),
                        }
                    }
                    tokio::select! {
                        _ = lifetime.cancelled() => return helpers,
                        changed = latest.changed() => if changed.is_err() { return helpers },
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
                    let in_flight = cpa_home::CredentialInFlight::from_document(&parsed.document);
                    let settings = cpa_home::inflight::PublisherConfig::from_config(&in_flight)
                        .map_err(|e| e.to_string())
                        .inspect_err(|e| tracing::warn!("failed to apply Home in-flight publisher config: {e}"))?;
                    publisher.apply(settings);
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
        if let Ok(helpers) = worker.await {
            for helper in helpers {
                let _ = helper.await;
            }
        }
        dispatcher.clear(&client);
        cpa_home::clear_current_if(&client);
        app_log.deactivate(&client);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_home::fake::{self, FakeHome, Reply};
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

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
                    use axum::response::IntoResponse;
                    let (fail, empty, hang) = (key == "sk-bad", key == "sk-empty", key == "sk-hang");
                    let slow = key == "sk-slow-bad";
                    seen.lock().unwrap().push((key, model));
                    if hang {
                        std::future::pending::<()>().await;
                    }
                    if slow {
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                    }
                    if fail || slow {
                        return (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            axum::Json(
                                serde_json::json!({"type": "error", "error": {"type": "api_error", "message": "boom"}}),
                            ),
                        )
                            .into_response();
                    }
                    if empty {
                        // A 200 stream that ends before its first event: bootstrap-eligible.
                        return ([("content-type", "text/event-stream")], "").into_response();
                    }
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({
                            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-upstream",
                            "content": [{"type": "text", "text": "hi"}],
                            "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}
                        })),
                    )
                        .into_response()
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
        scripted_with_kv(config, replies, Default::default()).await
    }

    /// A Home serving `config`, the dispatch `replies` in order, and an in-memory KV
    /// seeded with `kv`.
    async fn scripted_with_kv(
        config: &'static str,
        replies: Vec<String>,
        kv: std::collections::HashMap<String, String>,
    ) -> FakeHome {
        sequenced(config, replies.into_iter().map(fake::bulk).collect(), kv).await
    }

    /// [`scripted_with_kv`] with raw RPOP replies (errors as well as payloads).
    async fn sequenced(
        config: &'static str,
        replies: Vec<Reply>,
        kv: std::collections::HashMap<String, String>,
    ) -> FakeHome {
        let replies = Mutex::new(std::collections::VecDeque::from(replies));
        FakeHome::start(with_kv(kv, move |args| match args[0].to_lowercase().as_str() {
            "get" => fake::bulk(config),
            "subscribe" => fake::raw(ACK),
            "ping" => fake::raw("+PONG\r\n"),
            "rpop" => replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| fake::raw("$-1\r\n")),
            "lpush" => fake::raw(":1\r\n"),
            _ => fake::raw("-ERR unexpected\r\n"),
        }))
        .await
    }

    /// Home KV in front of `handler`: GET of `cpa:*` keys, SET (NX/XX) and EXPIRE are
    /// served from an in-memory map seeded with `kv`; everything else goes to
    /// `handler`.
    fn with_kv(
        kv: std::collections::HashMap<String, String>,
        handler: impl Fn(&[String]) -> Reply + Send + Sync + 'static,
    ) -> impl Fn(&[String]) -> Reply + Send + Sync + 'static {
        let kv = Mutex::new(kv);
        move |args| match args[0].to_lowercase().as_str() {
            "get" if args[1].starts_with("cpa:") => kv
                .lock()
                .unwrap()
                .get(&args[1])
                .map_or_else(|| fake::raw("$-1\r\n"), fake::bulk),
            "set" => {
                let mut kv = kv.lock().unwrap();
                let exists = kv.contains_key(&args[1]);
                let flag = |f: &str| args.iter().any(|a| a == f);
                if (flag("NX") && exists) || (flag("XX") && !exists) {
                    return fake::raw("$-1\r\n");
                }
                kv.insert(args[1].clone(), args[2].clone());
                fake::raw("+OK\r\n")
            }
            "expire" => fake::raw(if kv.lock().unwrap().contains_key(&args[1]) {
                ":1\r\n"
            } else {
                ":0\r\n"
            }),
            _ => handler(args),
        }
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
        node_with(home, Default::default()).await
    }

    /// [`node`] with this Codex executor.
    async fn node_with(
        home: &FakeHome,
        codex: cpa_exec::codex::CodexExecutor,
    ) -> (String, Arc<Runtime>, CancellationToken, tokio::task::JoinHandle<()>) {
        let executors = cpa_exec::Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
            codex,
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        };
        node_full(home, executors, vec![]).await
    }

    /// The published Home client is process-global (Go `home.SetCurrent`): one node at a
    /// time, held until its subscriber ends or the test's runtime drops it.
    static ONE_NODE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// [`node`] with these executors and local credentials in the runtime's store.
    async fn node_full(
        home: &FakeHome,
        executors: cpa_exec::Executors,
        local: Vec<Credential>,
    ) -> (String, Arc<Runtime>, CancellationToken, tokio::task::JoinHandle<()>) {
        let guard = ONE_NODE.lock().await;
        let config = overlay(b"port: 0\n", None).unwrap();
        let rt = Arc::new(cpa_server::testing::runtime(config, local, executors));
        let dispatcher = Arc::new(Dispatcher::new(&rt));
        rt.set_remote_dispatch(Some(dispatcher.clone()));
        let shutdown = CancellationToken::new();
        let subscriber = spawn_subscriber(home.config(), rt.clone(), dispatcher, shutdown.clone());
        let task = tokio::spawn(async move {
            let _guard = guard;
            subscriber.await.unwrap();
        });
        let app = cpa_server::router(rt.clone()).layer(axum::middleware::from_fn_with_state(
            rt.clone(),
            cpa_server::remote::gate,
        ));
        // main.rs: the request identity middleware wraps the served app.
        let app = cpa_server::observability::router(&rt, app);
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

    /// Go `TestLoadConfigOptionalMissingFallbackAppliesCredentialInFlightDefaults`,
    /// `…EmptyFallback…`, `…WhitespaceFallback…`, `…InvalidFallback…` and
    /// `TestCredentialInFlightConfigContractFixture`: an optional config that is
    /// missing, empty, blank or undecodable still carries the in-flight defaults, which
    /// equal the shared contract fixture's config and validate, and no credential
    /// concurrency settings.
    #[test]
    fn optional_config_fallbacks_carry_the_in_flight_defaults() {
        use cpa_home::config::{CredentialConcurrency, CredentialInFlight};
        let fixture: Value = serde_json::from_str(include_str!(
            "../../cpa-home/tests/fixtures/credential_in_flight_contract.json"
        ))
        .unwrap();
        let fixture =
            serde_yaml_ng::to_value(serde_json::json!({"credentials": {"in-flight": fixture["config"]}})).unwrap();
        assert_eq!(
            CredentialInFlight::from_document(&fixture),
            CredentialInFlight::default()
        );
        let dir = std::env::temp_dir().join(format!("cpa-home-optional-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, contents) in [
            ("missing.yaml", None),
            ("empty.yaml", Some("")),
            ("blank.yaml", Some(" \t\n\r ")),
            ("invalid.yaml", Some(":")),
        ] {
            let path = dir.join(name);
            if let Some(contents) = contents {
                std::fs::write(&path, contents).unwrap();
            }
            let cfg = crate::load_config(&path, true).unwrap();
            let in_flight = CredentialInFlight::from_document(&cfg.document);
            assert_eq!(in_flight, CredentialInFlight::default(), "{name}");
            cpa_home::inflight::PublisherConfig::from_config(&in_flight).unwrap();
            let concurrency = CredentialConcurrency::from_document(&cfg.document).unwrap();
            assert_eq!(concurrency, CredentialConcurrency::default(), "{name}");
        }
        let _ = std::fs::rename(
            &dir,
            std::env::temp_dir().join(format!("cpa-trash-optional-{}", std::process::id())),
        );
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

    /// Go `TestParseConfigBytesIgnoresHomeConfig`: Home is runtime-only (`-home-jwt`);
    /// a `home:` block in a config document (local or Home's own) parses and changes
    /// nothing: the config has no Home section, and the overlay keeps the node's
    /// listener.
    #[test]
    fn a_home_block_in_config_yaml_is_ignored() {
        let raw = b"port: 9999\nhome:\n  enabled: true\n  host: home.example.com\n  port: 444\n  disable-cluster-discovery: true\n  tls:\n    enable: true\n    server-name: home.example.com\n    ca-cert: C:/certs/ca.pem\n    insecure-skip-verify: true\n";
        let local = Config::parse(std::str::from_utf8(raw).unwrap()).unwrap();
        let plain = Config::parse("port: 9999\n").unwrap();
        assert_eq!(local.port, plain.port);
        assert_eq!(local.api_keys, plain.api_keys);
        let first = overlay(b"port: 0\n", None).unwrap();
        let next = overlay(raw, Some(&first)).unwrap();
        assert_eq!((next.port, next.host.as_str()), (8317, ""));
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
        assert_eq!(c.attributes[cpa_server::remote::PROVIDER], "openai-compatibility");
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

    /// Go `TestHomeForceMappingAliasResult`,
    /// `TestHomeForceMappingAliasResultRequiresSameOriginalAlias` and
    /// `TestHomeForceMappingAliasResultRequiresExplicitFlag`: Home's force mapping applies
    /// when the requested model is the alias Home mapped from (case and spacing aside,
    /// recognized reasoning suffixes ignored) and Home set the flag.
    #[test]
    fn force_mapping_follows_the_original_alias_and_the_flag() {
        let reply = |force: bool| {
            DispatchResponse::parse(
                serde_json::json!({"model": "grok-4.5", "force_mapping": force, "original_alias": "grok-latest",
                    "auth": {"id": "c1", "provider": "xai", "attributes": {"api_key": "k"}}})
                .to_string()
                .as_bytes(),
            )
            .unwrap()
        };
        let mapped = |force: bool, requested: &str| {
            let c = credential(&reply(force), requested).unwrap();
            assert_eq!(c.attributes[cpa_server::remote::UPSTREAM_MODEL], "grok-4.5");
            let flag = c.attributes.get(cpa_server::remote::FORCE_MAPPING).map(String::as_str);
            let alias = c.attributes.get(cpa_server::remote::ORIGINAL_ALIAS).map(String::as_str);
            assert_eq!(flag.is_some(), alias.is_some(), "{requested}");
            alias.map(str::to_owned)
        };
        assert_eq!(mapped(true, "grok-latest").as_deref(), Some("grok-latest"));
        assert!(mapped(true, " GROK-LATEST ").is_some());
        assert!(mapped(true, "grok-latest(high)").is_some());
        assert!(mapped(true, "grok-latest(custom)").is_none());
        assert!(mapped(true, "grok-other").is_none());
        assert!(mapped(false, "grok-latest").is_none(), "the flag is required");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn requests_run_on_home_credentials_and_release_their_leases() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let upstream = upstream(seen.clone()).await;
        let reply = format!(
            r#"{{"model":"claude-upstream","auth_index":"cred-1","concurrency":{{"accounted":true,"credential_id":"cred-1","model":"claude-upstream"}},"auth":{{"id":"cred-1","provider":"claude","attributes":{{"api_key":"sk-home-fake","base_url":"{upstream}"}}}}}}"#
        );
        let home = FakeHome::start(with_kv(Default::default(), move |args| {
            match args[0].to_lowercase().as_str() {
                "get" => fake::bulk("port: 0\ncredentials:\n  concurrency:\n    cpa-flush-interval: 20ms\n"),
                "subscribe" => fake::raw(ACK),
                "ping" => fake::raw("+PONG\r\n"),
                "rpop" => fake::bulk(&reply),
                "lpush" => fake::raw(":1\r\n"),
                _ => fake::raw("-ERR unexpected\r\n"),
            }
        }))
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
        let home = FakeHome::start(with_kv(Default::default(), move |args| {
            match args[0].to_lowercase().as_str() {
                "get" => fake::bulk("port: 0\n"),
                "subscribe" => fake::raw(ACK),
                "ping" => fake::raw("+PONG\r\n"),
                "rpop" => match replies.lock().unwrap().pop() {
                    Some(reply) => fake::bulk(reply),
                    None => fake::raw("$-1\r\n"),
                },
                "lpush" => fake::raw(":1\r\n"),
                _ => fake::raw("-ERR unexpected\r\n"),
            }
        }))
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
    /// the round ends with the earlier upstream failure (Go
    /// `TestManagerExecuteHomeStopsWhenDispatchRepeatsTriedAuth`: two dispatches, one
    /// execution, the first failure's status).
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
            vec![accounted("cred-1", "sk-empty", &upstream)],
        )
        .await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let body = serde_json::json!({"model": "claude-x", "max_tokens": 8, "stream": true, "messages": [{"role": "user", "content": "hi"}]});
        let (status, _, text) = ask_with_headers(&base, "/v1/messages", body).await;
        assert_ne!(status, 200, "{text}");
        assert_eq!(seen.lock().unwrap().len(), 1, "one execution");
        // The empty stream, then Home's empty answer that ends the round. A handler
        // bootstrap retry would run the request again and ask Home a third time.
        assert_eq!(rpops(&home).len(), 2);
        shutdown.cancel();
        task.await.unwrap();
    }

    /// A loopback server that answers every request with a 200 stream whose body is
    /// cut off before its first byte: the client sees Go's `unexpected EOF`.
    async fn truncating_upstream(hits: Arc<std::sync::atomic::AtomicUsize>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let hits = hits.clone();
                tokio::spawn(async move {
                    let mut seen = Vec::new();
                    let mut buf = [0u8; 4096];
                    // Read the request head and its declared body.
                    loop {
                        let n = socket.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        seen.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&seen).to_lowercase();
                        if let Some(end) = text.find("\r\n\r\n") {
                            let length = text
                                .lines()
                                .find_map(|l| {
                                    l.strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if seen.len() >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    hits.fetch_add(1, Ordering::SeqCst);
                    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 4096\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        base
    }

    /// Go's streaming Home loop: a connection-lifecycle failure (the body ended early)
    /// lets Home hand the same credential out once more, unexcluded; a second failure
    /// excludes it. `count` advances after each failed execution; on the wire Go's
    /// `newAuthDispatchRequest` pins it to 1 once an exclusion list is sent. Go
    /// `TestHomeStreamLifecycleRecoveryFailureRotatesWithoutExtraDispatch`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lifecycle_failure_retries_the_same_credential_once() {
        use std::sync::atomic::AtomicUsize;
        let truncated = Arc::new(AtomicUsize::new(0));
        let flaky = truncating_upstream(truncated.clone()).await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let good = upstream(seen.clone()).await;
        let home = scripted(
            "port: 0\nrouting: {retry: {request-retry: 0}}\n",
            vec![
                accounted("cred-1", "sk-flaky", &flaky),
                accounted("cred-1", "sk-flaky", &flaky),
                accounted("cred-2", "sk-good", &good),
            ],
        )
        .await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let body = serde_json::json!({"model": "claude-x", "max_tokens": 8, "stream": true, "messages": [{"role": "user", "content": "hi"}]});
        let (status, _, text) = ask_with_headers(&base, "/v1/messages", body).await;
        assert_eq!(status, 200, "{text}");
        assert_eq!(truncated.load(Ordering::SeqCst), 2, "cred-1 ran twice");
        assert_eq!(seen.lock().unwrap().len(), 1, "then cred-2");
        let requests = rpops(&home);
        let counts: Vec<i64> = requests.iter().map(|r| r["count"].as_i64().unwrap()).collect();
        assert_eq!(counts, vec![1, 2, 1]);
        let excluded: Vec<Value> = requests
            .iter()
            .map(|r| r.get("excluded_auth_ids").cloned().unwrap_or(Value::Null))
            .collect();
        assert_eq!(excluded, vec![Value::Null, Value::Null, serde_json::json!(["cred-1"])]);
        shutdown.cancel();
        task.await.unwrap();
    }

    fn pushed(home: &FakeHome, key: &str) -> Vec<String> {
        home.commands()
            .into_iter()
            .filter(|c| c[0].eq_ignore_ascii_case("lpush") && c[1] == key)
            .map(|c| c[2].clone())
            .collect()
    }

    /// Go's Home lifetime helpers and drain: usage records go to Home, running work is
    /// reported in in-flight snapshots, and shutting down cancels it (the scope's bound
    /// resource) instead of waiting out the cancel bound, releasing its lease.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usage_and_in_flight_reach_home_and_drains_cancel_running_work() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let upstream = upstream(seen.clone()).await;
        let home = scripted(
            "port: 0\ncredentials:\n  concurrency:\n    lifecycle-config-revision: 1\n    cpa-cancel-bound: 20s\n  in-flight:\n    snapshot-interval: 100ms\n    stale-after: 1s\n",
            vec![accounted("cred-1", "sk-good", &upstream), accounted("cred-2", "sk-hang", &upstream)],
        )
        .await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let (status, body) = ask(&base, "claude-x").await;
        assert_eq!(status, 200, "{body}");
        eventually("usage pushed", || !pushed(&home, "usage").is_empty()).await;
        let usage: Value = serde_json::from_str(&pushed(&home, "usage")[0]).unwrap();
        assert!(usage.is_object(), "{usage}");

        let hanging = tokio::spawn({
            let base = base.clone();
            async move { ask(&base, "claude-x").await }
        });
        eventually("hanging request reached upstream", || seen.lock().unwrap().len() == 2).await;
        eventually("in-flight snapshot names it", || {
            pushed(&home, "in-flight-snapshot").iter().any(|f| f.contains("cred-2"))
        })
        .await;

        let started = std::time::Instant::now();
        shutdown.cancel();
        task.await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "drain cancelled the execution"
        );
        let (status, _) = tokio::time::timeout(Duration::from_secs(5), hanging)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(status, 200);
        assert!(
            releases(&home).iter().any(|f| f["credential_id"] == "cred-2"),
            "the cancelled lease was released"
        );
    }

    /// Go `startHomeUsageForwarder` passes the lifetime context to every push: a
    /// lifetime cancelled while a push is in flight sends nothing more, and the
    /// records it had not sent go back to the queue.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_usage_forwarder_requeues_the_rest_of_its_batch() {
        let first = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The first push is answered (successfully) only after the lifetime was
        // cancelled.
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Mutex::new(held);
        let home = FakeHome::start({
            let first = first.clone();
            move |args| match args[0].to_ascii_lowercase().as_str() {
                "lpush" => {
                    first.store(true, Ordering::SeqCst);
                    // The handler runs on a runtime worker: hand its queued tasks
                    // (the test's among them) to another thread while it waits.
                    tokio::task::block_in_place(|| {
                        let _ = held.lock().unwrap().recv_timeout(Duration::from_secs(5));
                    });
                    fake::raw(":1\r\n")
                }
                _ => fake::raw("+PONG\r\n"),
            }
        })
        .await;
        let client = home.client();
        fake::set_heartbeat(&client, true);
        let config = overlay(b"port: 0\n", None).unwrap();
        let rt = Arc::new(cpa_server::testing::runtime(
            config.clone(),
            vec![],
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        rt.usage_queue().configure(true, &config);
        for record in [b"{\"n\":1}", b"{\"n\":2}", b"{\"n\":3}"] {
            rt.usage_queue().enqueue(record.to_vec());
        }
        let stop = CancellationToken::new();
        let task = tokio::spawn(forward_usage(rt.clone(), client, stop.clone()));
        eventually("first push in flight", || first.load(Ordering::SeqCst)).await;
        stop.cancel();
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pushed(&home, "usage"), [r#"{"n":1}"#]);
        assert_eq!(
            rt.usage_queue().pop_oldest(64),
            [b"{\"n\":2}".to_vec(), b"{\"n\":3}".to_vec()]
        );
    }

    /// Go `prepareHomeRequestAuth` with the Home KV identity caches: a dispatched OAuth
    /// credential without a device pool gets the pool Home KV holds for Home's auth
    /// index (never a local random one), and a cloaked API key's session ID comes from
    /// Home KV (an API key keeping the caller's own headers never reads it).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatched_credentials_take_their_identity_from_home_kv() {
        type Seen = Arc<Mutex<Vec<(axum::http::HeaderMap, Value)>>>;
        let seen: Seen = Arc::default();
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post({
                let seen = seen.clone();
                move |headers: axum::http::HeaderMap, body: String| {
                    let seen = seen.clone();
                    async move {
                        seen.lock()
                            .unwrap()
                            .push((headers, serde_json::from_str(&body).unwrap()));
                        axum::Json(serde_json::json!({
                            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-up",
                            "content": [{"type": "text", "text": "hi"}],
                            "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}
                        }))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let device = "e".repeat(64);
        let pool_key = format!(
            "cpa:claude:credential-device-pool:{}",
            cpa_home::kv::hash_key_part("idx-oauth")
        );
        let oauth = format!(
            r#"{{"model":"claude-up","auth_index":"idx-oauth","auth":{{"id":"cred-oauth","provider":"claude","attributes":{{"base_url":"{upstream}"}},"metadata":{{"type":"claude","access_token":"sk-ant-oat01-FAKE","account_uuid":"8f14e45f-ceea-467f-a8a1-0c3b9e1f3a77","email":"fake@example.invalid"}}}}}}"#
        );
        let api_key = format!(
            r#"{{"model":"claude-up","auth_index":"idx-key","auth":{{"id":"cred-key","provider":"claude","attributes":{{"api_key":"sk-fake-key","base_url":"{upstream}","cloak_mode":"always"}}}}}}"#
        );
        let plain = api_key
            .replace("sk-fake-key", "sk-fake-plain")
            .replace(r#","cloak_mode":"always""#, "");
        let home = scripted_with_kv(
            "port: 0\n",
            vec![oauth, api_key, plain],
            [(pool_key.clone(), format!(r#"["{device}"]"#))].into_iter().collect(),
        )
        .await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;

        let (status, body) = ask(&base, "claude-x").await;
        assert_eq!(status, 200, "{body}");
        let (_, sent) = seen.lock().unwrap()[0].clone();
        let user_id: Value = serde_json::from_str(sent["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(user_id["device_id"], device.as_str(), "{sent}");
        assert_eq!(user_id["account_uuid"], "8f14e45f-ceea-467f-a8a1-0c3b9e1f3a77");
        assert!(
            home.commands().iter().any(|c| c[0] == "get" && c[1] == pool_key),
            "the pool was read from Home KV"
        );

        let (status, body) = ask(&base, "claude-x").await;
        assert_eq!(status, 200, "{body}");
        // Go `CachedSessionIDRequired` twice: the cloaked body's fake user ID misses
        // (GET, SETNX for an hour, GET), then the headers hit and renew (GET, EXPIRE).
        let session_key = format!("cpa:claude:session-id:{}", cpa_home::kv::hash_key_part("sk-fake-key"));
        let calls: Vec<Vec<String>> = home
            .commands()
            .into_iter()
            .filter(|c| c.get(1) == Some(&session_key))
            .map(|mut c| {
                if c[0].eq_ignore_ascii_case("set") {
                    c[2] = "<uuid>".into();
                }
                c
            })
            .collect();
        let want = |c: &[&str]| c.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            calls,
            [
                want(&["get", &session_key]),
                want(&["SET", &session_key, "<uuid>", "EX", "3600", "NX"]),
                want(&["get", &session_key]),
                want(&["get", &session_key]),
                want(&["expire", &session_key, "3600"]),
            ]
        );

        // Go `applyClaudeHeaders` returns before the cached session for a caller that
        // keeps its own headers: no Home KV lookup at all.
        let (status, body) = ask(&base, "claude-x").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(rpops(&home).len(), 3);
        let plain_key = format!("cpa:claude:session-id:{}", cpa_home::kv::hash_key_part("sk-fake-plain"));
        assert!(!home.commands().iter().any(|c| c.get(1) == Some(&plain_key)));
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go `homeDispatchSessionIDs` after extraction: a parent fallback is sent as the
    /// parent (canonicalized itself); any other fallback joins the session's alias group,
    /// whose canonical ID later requests send.
    #[test]
    fn dispatch_sessions_follow_go() {
        let config = overlay(b"port: 0\n", None).unwrap();
        let rt = Arc::new(cpa_server::testing::runtime(
            config,
            vec![],
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        let dispatcher = Dispatcher::new(&rt);
        let ttl = Duration::from_secs(3600);
        let ids = |primary: &str, fallback: &str| {
            let (a, b) = dispatcher.dispatch_sessions(primary, fallback, ttl);
            (a, b)
        };
        let pair = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        assert_eq!(ids("", "x"), pair("", ""));
        // A hierarchy parent goes to Home as the parent.
        assert_eq!(ids("claude:child", "claude:root"), pair("claude:child", "claude:root"));
        // A prompt cache key is another name of the session.
        assert_eq!(ids("sess-a", "pck:k"), pair("sess-a", ""));
        assert_eq!(ids("pck:k", ""), pair("sess-a", ""));
        // A parent that is an alias is sent as its group's canonical ID.
        assert_eq!(ids("pck:w", "pck:k"), pair("pck:w", "sess-a"));
        // An agent session's fallback is its parent.
        assert_eq!(ids("agent:x:agent:1", "sess-a"), pair("agent:x:agent:1", "sess-a"));
        // A parent in the session's own alias group is dropped.
        assert_eq!(ids("a:5", "c:6"), pair("a:5", ""));
        assert_eq!(ids("a:5", "c:1"), pair("a:5", ""));
        assert_eq!(ids("c:6", "c:1"), pair("a:5", ""));
        assert_eq!(ids("sess-b", "pck:b"), pair("sess-b", ""));
        assert_eq!(ids("pck:b", "sess-b"), pair("sess-b", ""));
        // Long identifiers are bounded after canonicalization.
        let long = "s".repeat(300);
        let (bounded, _) = ids(&long, "");
        assert_eq!(bounded, cpa_common::session::bound_session_identity(&long));
        assert!(bounded.len() < 256);
    }

    /// Go `homeDispatchSessionIDs`: the request's explicit identity (else the derived
    /// one) through the dispatcher's alias groups, as a pick sends them.
    fn home_session_ids(
        dispatcher: &Dispatcher,
        headers: &[(&str, &str)],
        body: &str,
        ttl: Duration,
    ) -> (String, String) {
        let mut map = axum::http::HeaderMap::new();
        for (name, value) in headers {
            map.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        let meta = cpa_common::session::Meta {
            execution_session: None,
            derived: None,
        };
        let (mut primary, mut fallback, _) = cpa_common::session::explicit_session_ids(&map, body.as_bytes(), &meta);
        if primary.is_empty() {
            (primary, fallback) = cpa_common::session::session_ids(&map, body.as_bytes(), &meta);
        }
        dispatcher.dispatch_sessions(&primary, &fallback, ttl)
    }

    fn test_runtime() -> Arc<Runtime> {
        Arc::new(cpa_server::testing::runtime(
            overlay(b"port: 0\n", None).unwrap(),
            vec![],
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ))
    }

    fn test_dispatcher() -> Dispatcher {
        Dispatcher::new(&test_runtime())
    }

    /// Go `TestHomeDispatchSessionIDsExtractsParentSessionID`,
    /// `TestHomeDispatchSessionIDsExtractsParentFromHeaderPlusBody`,
    /// `TestHomeDispatchSessionIDsNestedRequestSubagent` and the hierarchies
    /// `TestPickNextViaHomePassesParentSessionIDToHierarchyDispatcher` and
    /// `TestPickNextViaHomeNestedRequestSubagentHierarchy` send: Go's expected session and
    /// parent for each request shape. (Their LCP cases need the Merkle LCP matcher.)
    #[test]
    fn home_session_hierarchies_follow_go() {
        let hour = Duration::from_secs(3600);
        let pair = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        type Case<'a> = (&'a [(&'a str, &'a str)], &'a str, (String, String));
        let cases: Vec<Case<'_>> = vec![
            (
                &[("X-Claude-Code-Session-Id", "claude-root-1")],
                "",
                pair("claude:claude-root-1", ""),
            ),
            (
                &[
                    ("X-Claude-Code-Session-Id", "claude-root-1"),
                    ("X-Claude-Code-Agent-Id", "sub-checker"),
                ],
                "",
                pair("claude:claude-root-1:agent:sub-checker", "claude:claude-root-1"),
            ),
            (
                &[
                    ("X-Slot-Session-Id", "pi-slot-worker-1"),
                    ("X-Parent-Session-ID", "pi-slot-main-0"),
                ],
                "",
                pair("slot:pi-slot-worker-1", "slot:pi-slot-main-0"),
            ),
            (
                &[("X-Claude-Code-Session-Id", "child-session-001")],
                r#"{"parent_session_id":"parent-session-999"}"#,
                pair("claude:child-session-001", "claude:parent-session-999"),
            ),
            (
                &[],
                r#"{"metadata":{"user_id":"{\"session_id\":\"child-session-002\",\"parent_session_id\":\"parent-session-888\",\"agent_id\":\"sub-agent-1\"}"}}"#,
                pair(
                    "claude:child-session-002:agent:sub-agent-1",
                    "claude:parent-session-888",
                ),
            ),
            (
                &[
                    ("X-Http-Session-Id", "agy-child-101"),
                    ("X-Parent-Session-ID", "agy-parent-100"),
                ],
                "",
                pair("agy:agy-child-101", "agy:agy-parent-100"),
            ),
            (
                &[],
                r#"{"cachedContent":"cache-child-201","parent_session_id":"cache-parent-200"}"#,
                pair("geminicache:cache-child-201", "geminicache:cache-parent-200"),
            ),
            (
                &[],
                r#"{"request":{"sessionId":"root","metadata":{"agent_id":"worker"}}}"#,
                pair("session:root:agent:worker", "session:root"),
            ),
            (
                &[],
                r#"{"request":{"sessionId":"root","metadata":{"subagent_id":"worker-sub"}}}"#,
                pair("session:root:agent:worker-sub", "session:root"),
            ),
            (
                &[
                    ("X-Claude-Code-Session-Id", "tree-parent-1"),
                    ("X-Claude-Code-Agent-Id", "worker-agent"),
                ],
                r#"{"messages":[{"role":"user","content":"test"}]}"#,
                pair("claude:tree-parent-1:agent:worker-agent", "claude:tree-parent-1"),
            ),
        ];
        for (headers, body, want) in cases {
            let dispatcher = test_dispatcher();
            assert_eq!(
                home_session_ids(&dispatcher, headers, body, hour),
                want,
                "{headers:?} {body}"
            );
        }
    }

    /// Go `TestHomeDispatchCanonicalizesPromptCacheAndConversationAliases` and
    /// `TestHomeSessionAliasCacheClearsWhenConfiguredTTLChanges`: every request of one
    /// conversation reaches Home under the identity it was first seen with, until the
    /// session-affinity TTL changes.
    #[test]
    fn prompt_cache_and_conversation_aliases_share_one_home_session() {
        let hour = Duration::from_secs(3600);
        let (conversation, combined, prompt) = (
            r#"{"conversation":{"id":"conversation-session"}}"#,
            r#"{"conversation":{"id":"conversation-session"},"prompt_cache_key":"shared-cache-bucket"}"#,
            r#"{"prompt_cache_key":"shared-cache-bucket"}"#,
        );
        for (payloads, want) in [
            ([conversation, combined, prompt], "conv:conversation-session"),
            ([prompt, combined, conversation], "pck:shared-cache-bucket"),
            ([combined, conversation, prompt], "pck:shared-cache-bucket"),
        ] {
            let dispatcher = test_dispatcher();
            for payload in payloads {
                assert_eq!(
                    home_session_ids(&dispatcher, &[], payload, hour).0,
                    want,
                    "{payloads:?}"
                );
            }
        }

        let dispatcher = test_dispatcher();
        let combined = r#"{"conversation":{"id":"ttl-conversation"},"prompt_cache_key":"ttl-prompt"}"#;
        let only = r#"{"conversation":{"id":"ttl-conversation"}}"#;
        assert_eq!(home_session_ids(&dispatcher, &[], combined, hour).0, "pck:ttl-prompt");
        assert_eq!(home_session_ids(&dispatcher, &[], only, hour).0, "pck:ttl-prompt");
        let minute = Duration::from_secs(60);
        assert_eq!(
            home_session_ids(&dispatcher, &[], only, minute).0,
            "conv:ttl-conversation"
        );
    }

    /// Go `SelectHomeAuthWithCredentialPolicy` behind Codex Alpha Search: Home picks under
    /// `codex_alpha_search_v1`; a credential the policy does not allow is released and
    /// excluded before the next pick, and the allowed one serves the search.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn alpha_search_dispatches_under_its_credential_policy() {
        let searched: Arc<Mutex<Vec<String>>> = Arc::default();
        let app = axum::Router::new().route(
            "/alpha/search",
            axum::routing::post({
                let searched = searched.clone();
                move |headers: axum::http::HeaderMap, body: String| {
                    let searched = searched.clone();
                    async move {
                        let auth = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_owned();
                        let model = serde_json::from_str::<Value>(&body).unwrap()["model"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned();
                        searched.lock().unwrap().push(format!("{auth} {model}"));
                        axum::Json(serde_json::json!({"results": []}))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let codex = |id: &str, key: &str, alpha: bool| {
            let alpha = if alpha { r#","codex_alpha_search":"true""# } else { "" };
            format!(
                r#"{{"model":"gpt-home-upstream","auth_index":"{id}","concurrency":{{"accounted":true,"credential_id":"{id}","model":"gpt-home-upstream"}},"auth":{{"id":"{id}","provider":"codex","attributes":{{"api_key":"{key}","base_url":"{upstream}"{alpha}}}}}}}"#
            )
        };
        let home = scripted(
            "port: 0\n",
            vec![
                codex("cred-plain", "sk-fake-plain", false),
                codex("cred-alpha", "sk-fake-alpha", true),
            ],
        )
        .await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let response = wreq::Client::new()
            .post(format!("{base}/v1/alpha/search"))
            .header("authorization", "Bearer client-key")
            .json(&serde_json::json!({"id": "search-1", "model": "gpt-up", "query": "q"}))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = response.text().await.unwrap();
        assert_eq!(status, 200, "{body}");
        // Home's upstream model reaches the upstream (Go `ResolveExecutionModel`).
        assert_eq!(*searched.lock().unwrap(), ["Bearer sk-fake-alpha gpt-home-upstream"]);
        let picks = rpops(&home);
        assert_eq!(picks.len(), 2);
        assert_eq!(picks[0]["credential_policy"], "codex_alpha_search_v1");
        assert!(picks[0]["excluded_auth_ids"].is_null());
        assert_eq!(picks[1]["excluded_auth_ids"], serde_json::json!(["cred-plain"]));
        // Go pins the wire count to 1 once exclusions are sent.
        assert_eq!(picks[1]["count"], 1);
        let commands = home.commands();
        let release = commands
            .iter()
            .position(|c| {
                c[0].eq_ignore_ascii_case("lpush") && c[1] == "concurrency-release" && c[2].contains("cred-plain")
            })
            .expect("the ineligible pick is released");
        let second = commands
            .iter()
            .enumerate()
            .filter(|(_, c)| c[0] == "rpop")
            .nth(1)
            .map(|(i, _)| i)
            .unwrap();
        assert!(release < second, "the ineligible pick is released before the next one");
        shutdown.cancel();
        task.await.unwrap();
    }

    /// A draining dispatcher cancels an Alpha Search waiting on its upstream (Go binds
    /// the search to the selection's attempt context), releasing its lease.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_drain_cancels_a_stalled_alpha_search() {
        let started = Arc::new(tokio::sync::Notify::new());
        let app = axum::Router::new().route(
            "/alpha/search",
            axum::routing::post({
                let started = started.clone();
                move || {
                    let started = started.clone();
                    async move {
                        started.notify_one();
                        std::future::pending::<()>().await;
                        "never"
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let reply = format!(
            r#"{{"model":"gpt-up","auth_index":"cred-alpha","concurrency":{{"accounted":true,"credential_id":"cred-alpha","model":"gpt-up"}},"auth":{{"id":"cred-alpha","provider":"codex","attributes":{{"api_key":"sk-fake-alpha","base_url":"{upstream}","codex_alpha_search":"true"}}}}}}"#
        );
        let home = scripted(
            "port: 0\ncredentials:\n  concurrency:\n    lifecycle-config-revision: 1\n    cpa-cancel-bound: 20s\n",
            vec![reply],
        )
        .await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let search = tokio::spawn(async move {
            let response = wreq::Client::new()
                .post(format!("{base}/v1/alpha/search"))
                .json(&serde_json::json!({"id": "search-1", "model": "gpt-up"}))
                .send()
                .await
                .unwrap();
            (response.status().as_u16(), response.text().await.unwrap())
        });
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .expect("the search reached the upstream");
        let begun = std::time::Instant::now();
        shutdown.cancel();
        task.await.unwrap();
        assert!(
            begun.elapsed() < Duration::from_secs(5),
            "the drain did not wait out the bound"
        );
        let (status, body) = tokio::time::timeout(Duration::from_secs(5), search)
            .await
            .unwrap()
            .unwrap();
        // Go: `HTTPStatusFromErrorOr` maps the cancelled context to 499, and the error
        // is the client's `*url.Error`.
        assert_eq!(
            (status, body),
            (
                499,
                format!(r#"{{"error":"Post \"{upstream}/alpha/search\": context canceled"}}"#)
            )
        );
        assert!(
            releases(&home).iter().any(|f| f["credential_id"] == "cred-alpha"),
            "the cancelled lease was released"
        );
    }

    /// The usage records pushed to Home so far.
    fn usage_records(home: &FakeHome) -> Vec<Value> {
        pushed(home, "usage")
            .iter()
            .map(|r| serde_json::from_str(r).unwrap())
            .collect()
    }

    /// Go `reportHomeUnauthorized`: a Home count-tokens attempt the upstream answers 401
    /// becomes a zero-token `home-result` failure (no executor reporter records count
    /// tokens) under the selection's provider, Home's upstream model and Home's session;
    /// the next credential's success records nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn count_tokens_401s_reach_home_as_home_results() {
        use axum::response::IntoResponse;
        const DENIED: &str = r#"{"type":"error","error":{"type":"authentication_error","message":"bad token"}}"#;
        let app = axum::Router::new().route(
            "/v1/messages/count_tokens",
            axum::routing::post(|headers: axum::http::HeaderMap| async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default();
                if auth.ends_with("fake-A") {
                    (axum::http::StatusCode::UNAUTHORIZED, DENIED).into_response()
                } else {
                    axum::Json(serde_json::json!({"input_tokens": 5})).into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        // Kimi counts tokens upstream through the Claude executor (Go
        // `countTokensUpstream`); its auth's provider differs from its executor key.
        let pick = |id: &str, token: &str| {
            format!(
                r#"{{"model":"kimi-up","auth_index":"idx-{id}","auth":{{"id":"{id}","provider":"Kimi.com","attributes":{{"base_url":"{upstream}"}},"metadata":{{"type":"kimi","access_token":"{token}"}}}}}}"#
            )
        };
        let home = scripted_with_kv(
            "port: 0\n",
            vec![pick("cred-a", "kimi-oat-fake-A"), pick("cred-b", "kimi-oat-fake-B")],
            Default::default(),
        )
        .await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let response = wreq::Client::new()
            .post(format!("{base}/v1/messages/count_tokens"))
            .header("authorization", "Bearer client-key")
            .header("x-session-id", "conv-count-1")
            .json(&serde_json::json!({"model": "kimi-x", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = response.text().await.unwrap();
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("5"), "{body}");
        // Go `TestHomeCountTokensTriesFreshCredentialWhenRequestRetryIsZero`: the second
        // pick excludes the failed credential.
        assert_eq!(exclusions(&home), vec![vec![], strings(&["cred-a"])]);
        eventually("the home result reached Home", || !usage_records(&home).is_empty()).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let records = usage_records(&home);
        assert_eq!(records.len(), 1, "only the 401 is recorded: {records:?}");
        let record = &records[0];
        let sent_session = rpops(&home)[0]["session_id"].as_str().unwrap().to_owned();
        assert_eq!(sent_session, "header:conv-count-1");
        let expect = [
            ("executor_type", "home-result"),
            ("provider", "kimi.com"),
            ("model", "kimi-up"),
            ("alias", "kimi-x"),
            ("auth_index", "idx-cred-a"),
            (
                "access_token_sha256",
                "893a8052149a5afa166437049902edae776705705842626aefff0a5fea14041d",
            ),
            ("auth_type", "oauth"),
            ("source", ""),
            ("api_key", ""),
            ("endpoint", "POST /v1/messages/count_tokens"),
        ];
        for (key, want) in expect {
            assert_eq!(record[key], want, "{key}: {record}");
        }
        assert_eq!(
            record["session_id"],
            cpa_common::session::normalize_to_canonical_uuid(&sent_session)
        );
        assert_eq!(record["failed"], true);
        assert_eq!(record["generate"], false);
        assert_eq!(record["fail"], serde_json::json!({"status_code": 401, "body": DENIED}));
        assert_eq!(record["tokens"]["total_tokens"], 0);
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go's Alpha Search reports an upstream 401 on a Home pick (`ReportHomeUnauthorized`
    /// with provider `codex` and the selection model) from a context holding only the
    /// request ID and the session: no client address, agent or endpoint.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn alpha_search_401s_reach_home_as_home_results() {
        let app = axum::Router::new().route(
            "/alpha/search",
            axum::routing::post(|| async { (axum::http::StatusCode::UNAUTHORIZED, "denied upstream") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        // An API key carrying an access token: Go fingerprints the metadata token
        // whatever the auth kind.
        let reply = format!(
            r#"{{"model":"gpt-home-upstream","auth_index":"idx-alpha","auth":{{"id":"cred-alpha","provider":"codex","attributes":{{"api_key":"sk-fake-alpha","base_url":"{upstream}","codex_alpha_search":"true","runtime_only":"true"}},"metadata":{{"access_token":"oat-fake-codex"}}}}}}"#
        );
        let home = scripted("port: 0\n", vec![reply]).await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let response = wreq::Client::new()
            .post(format!("{base}/v1/alpha/search"))
            .header("authorization", "Bearer client-key")
            .header("user-agent", "codex_cli_rs/0.50")
            .json(&serde_json::json!({"id": "search-1", "model": "gpt-up", "query": "q"}))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = response.text().await.unwrap();
        assert_eq!((status, body.as_str()), (401, "denied upstream"));
        eventually("the home result reached Home", || !usage_records(&home).is_empty()).await;
        let record = &usage_records(&home)[0];
        let expect = [
            ("executor_type", "home-result"),
            ("provider", "codex"),
            ("model", "gpt-up"),
            ("alias", "gpt-up"),
            ("auth_index", "idx-alpha"),
            (
                "access_token_sha256",
                "c8e8a68cecad5aa7e768b220e6ae624d3ab8d4ecf5d1474620db553e06e0bfd0",
            ),
            ("auth_type", "apikey"),
            ("source", "memory"),
            ("api_key", ""),
            ("client_ip", ""),
            ("user_agent", ""),
            ("endpoint", ""),
            ("reasoning_effort", ""),
            ("service_tier", "default"),
        ];
        for (key, want) in expect {
            assert_eq!(record[key], want, "{key}: {record}");
        }
        assert_eq!(
            record["session_id"],
            cpa_common::session::normalize_to_canonical_uuid("search-1")
        );
        assert!(!record["request_id"].as_str().unwrap().is_empty(), "{record}");
        assert_eq!(
            record["fail"],
            serde_json::json!({"status_code": 401, "body": "denied upstream"})
        );
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go `TestHomeExecutionPropagatesRequestMetadata` and
    /// `TestHomeExecutionFailureResultPreservesRequestMetadata` (Go issue #4791): the
    /// usage record of a Home attempt carries the client's requested model, reasoning
    /// effort, service tier (`auto` when omitted) and generate flag, and a failed attempt
    /// keeps the requested tier.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn home_usage_records_carry_the_client_request_metadata() {
        let upstream = ContractUpstream::start(vec![(
            "sk-bad",
            Answer::Fail(400, None, r#"{"error":{"message":"invalid request"}}"#),
        )])
        .await;
        let bad = accounted_compat("cred-3", &upstream.base).replace("sk-home-fake", "sk-bad");
        let home = scripted(
            "port: 0\nrequest-retry: 0\n",
            vec![
                accounted_compat("cred-1", &upstream.base),
                accounted_compat("cred-2", &upstream.base),
                bad,
            ],
        )
        .await;
        let (base, _rt, shutdown, task) = contract_node(&home).await;
        let request = |tier: Option<&str>| {
            let mut body = serde_json::json!({
                "model": "client-model", "reasoning_effort": "high", "generate": false,
                "messages": [{"role": "user", "content": "hi"}],
            });
            if let Some(tier) = tier {
                body["service_tier"] = tier.into();
            }
            body
        };
        for (tier, want) in [(Some("priority"), 200), (None, 200), (Some("priority"), 400)] {
            let (status, _, body) = ask_with_headers(&base, "/v1/chat/completions", request(tier)).await;
            assert_eq!(status, want, "{body}");
        }
        eventually("three usage records", || usage_records(&home).len() >= 3).await;
        let records = usage_records(&home);
        let seen: Vec<(&str, &str, &str, bool, bool)> = records
            .iter()
            .map(|r| {
                (
                    r["alias"].as_str().unwrap(),
                    r["reasoning_effort"].as_str().unwrap(),
                    r["service_tier"].as_str().unwrap(),
                    r["generate"].as_bool().unwrap(),
                    r["failed"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("client-model", "high", "priority", false, false),
                ("client-model", "high", "auto", false, false),
                ("client-model", "high", "priority", false, true),
            ],
            "{records:?}"
        );
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go `setHomeUserAPIKeyOnGinContext`: in Home mode the client key Home
    /// authenticated is the request's caller. Usage records carry it, and a later pick
    /// that sends none keeps it for the rest of the request.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn home_user_api_key_is_the_attempt_caller() {
        let upstream = ContractUpstream::start(vec![("sk-bad", bad_gateway())]).await;
        let mut first: Value =
            serde_json::from_str(&accounted_compat("cred-1", &upstream.base).replace("sk-home-fake", "sk-bad"))
                .unwrap();
        first["user_api_key"] = " client-key-home ".into();
        let home = scripted(
            "port: 0\n",
            vec![first.to_string(), accounted_compat("cred-2", &upstream.base)],
        )
        .await;
        let (base, _rt, shutdown, task) = contract_node(&home).await;
        let (status, _, body) = ask_with_headers(
            &base,
            "/v1/chat/completions",
            serde_json::json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(rpops(&home).len(), 2, "the first credential failed over");
        eventually("two usage records", || usage_records(&home).len() >= 2).await;
        let records = usage_records(&home);
        let keys: Vec<(&str, bool)> = records
            .iter()
            .map(|r| (r["api_key"].as_str().unwrap(), r["failed"].as_bool().unwrap()))
            .collect();
        assert_eq!(
            keys,
            [("client-key-home", true), ("client-key-home", false)],
            "{records:?}"
        );
        shutdown.cancel();
        task.await.unwrap();
    }

    /// A Codex live upstream on loopback: the calls endpoint answers 201 with a call ID
    /// (401 for the token ending `denied`), hangups 200, and the direct Realtime
    /// WebSocket's handshake 401. Records `METHOD path authorization`.
    async fn live_upstream(seen: Arc<Mutex<Vec<String>>>) -> cpa_exec::codex::CodexExecutor {
        use axum::response::IntoResponse;
        let app = axum::Router::new().fallback(
            move |method: axum::http::Method, uri: axum::http::Uri, headers: axum::http::HeaderMap| {
                let seen = seen.clone();
                async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    seen.lock().unwrap().push(format!("{method} {} {auth}", uri.path()));
                    let path = uri.path();
                    if path.ends_with("/hangup") {
                        return (axum::http::StatusCode::OK, "{}").into_response();
                    }
                    if path == "/v1/realtime" {
                        return (axum::http::StatusCode::UNAUTHORIZED, "ws denied").into_response();
                    }
                    if auth.ends_with("denied") {
                        return (axum::http::StatusCode::UNAUTHORIZED, "live denied").into_response();
                    }
                    (
                        axum::http::StatusCode::CREATED,
                        [
                            ("location", "/v1/realtime/calls/rtc_live1"),
                            ("content-type", "application/sdp"),
                        ],
                        "v=0 answer",
                    )
                        .into_response()
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        cpa_exec::codex::CodexExecutor::with_client(
            wreq::Client::new(),
            cpa_exec::codex_oauth::CodexOAuth::new(wreq::Client::new()),
        )
        .with_live_endpoints(
            format!("{base}/backend-api/codex/realtime/calls"),
            format!("ws{}/v1", base.trim_start_matches("http")),
        )
    }

    /// A dispatched Codex credential: OAuth with `token`, or an API key.
    fn codex_pick(id: &str, oauth_token: Option<&str>) -> String {
        let auth = match oauth_token {
            Some(token) => format!(
                r#"{{"id":"{id}","provider":"codex","metadata":{{"type":"codex","access_token":"{token}","account_id":"acct-{id}"}}}}"#
            ),
            None => format!(r#"{{"id":"{id}","provider":"codex","attributes":{{"api_key":"sk-fake-{id}"}}}}"#),
        };
        format!(
            r#"{{"model":"gpt-live-1-codex","auth_index":"{id}","concurrency":{{"accounted":true,"credential_id":"{id}","model":"gpt-live-1-codex"}},"auth":{auth}}}"#
        )
    }

    async fn live_call(base: &str) -> (u16, String) {
        let response = wreq::Client::new()
            .post(format!("{base}/v1/realtime/calls"))
            .header("authorization", "Bearer client-key")
            .json(&serde_json::json!({"model": "gpt-realtime", "sdp": "v=0 offer"}))
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    /// Go `SelectHomeAuthByKind(codex, model, oauth)` for live calls: an API key is
    /// released and excluded before the next pick; the call keeps its OAuth pick
    /// (`selection.Retain()`) until the hangup ends it, and the hangup runs on it
    /// without another pick.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_calls_hold_their_pick_until_hangup() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let codex = live_upstream(seen.clone()).await;
        let home = scripted(
            "port: 0\n",
            vec![codex_pick("cred-key", None), codex_pick("cred-live", Some("oat-live"))],
        )
        .await;
        let (base, rt, shutdown, task) = node_with(&home, codex).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let (status, body) = live_call(&base).await;
        assert_eq!((status, body.as_str()), (201, "v=0 answer"));
        let picks = rpops(&home);
        assert_eq!(picks.len(), 2, "{picks:?}");
        assert_eq!(picks[0]["model"], "gpt-live-1-codex");
        assert!(picks[0].get("credential_policy").is_none());
        assert_eq!(picks[1]["excluded_auth_ids"], serde_json::json!(["cred-key"]));
        let released = |id: &str| releases(&home).iter().any(|f| f["credential_id"] == id);
        assert!(released("cred-key"), "the API key was released before the next pick");
        assert_eq!(
            *seen.lock().unwrap(),
            ["POST /backend-api/codex/realtime/calls Bearer oat-live"]
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!released("cred-live"), "the stored call keeps its pick");

        let response = wreq::Client::new()
            .post(format!("{base}/v1/realtime/calls/rtc_live1/hangup"))
            .header("authorization", "Bearer client-key")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(rpops(&home).len(), 2, "the hangup ran on the call's pick");
        assert_eq!(
            seen.lock().unwrap().last().unwrap(),
            "POST /v1/realtime/calls/rtc_live1/hangup Bearer oat-live"
        );
        eventually("the hangup ended the pick", || released("cred-live")).await;
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Home draining a retained live pick ends it at once (Go binds the session's end to
    /// the selection) instead of holding the drain for the cancel bound.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_drain_ends_a_retained_live_pick() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let codex = live_upstream(seen.clone()).await;
        let home = scripted(
            "port: 0\ncredentials:\n  concurrency:\n    lifecycle-config-revision: 1\n    cpa-cancel-bound: 20s\n",
            vec![codex_pick("cred-live", Some("oat-live"))],
        )
        .await;
        let (base, rt, shutdown, task) = node_with(&home, codex).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        assert_eq!(live_call(&base).await.0, 201);
        let begun = std::time::Instant::now();
        shutdown.cancel();
        task.await.unwrap();
        assert!(
            begun.elapsed() < Duration::from_secs(5),
            "the drain did not wait out the bound"
        );
        assert!(releases(&home).iter().any(|f| f["credential_id"] == "cred-live"));
    }

    /// A Codex Responses WebSocket upstream on loopback: every request frame gets
    /// `response.created` and `response.completed`. Counts dials and closed sockets.
    async fn responses_ws_upstream() -> (String, Arc<Mutex<(usize, usize)>>) {
        use axum::extract::ws::{Message, WebSocketUpgrade};
        let counts: Arc<Mutex<(usize, usize)>> = Arc::default();
        let app = axum::Router::new().fallback({
            let counts = counts.clone();
            move |ws: WebSocketUpgrade| {
                let counts = counts.clone();
                async move {
                    counts.lock().unwrap().0 += 1;
                    ws.on_upgrade(move |mut socket| async move {
                        while let Some(Ok(message)) = socket.recv().await {
                            if !matches!(message, Message::Text(_)) {
                                continue;
                            }
                            for event in [
                                r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#,
                                r#"{"type":"response.completed","response":{"id":"r1","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
                            ] {
                                let _ = socket.send(Message::Text(event.into())).await;
                            }
                        }
                        counts.lock().unwrap().1 += 1;
                    })
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, counts)
    }

    /// Go `bindExecutionLifecycle` and `retainedHomeSessionSelection`: a Responses
    /// WebSocket turn on a Home Codex key with upstream WebSockets leaves its pick with
    /// the pooled socket. The pick outlives the response, the next turn runs on it
    /// without another pick, and Home draining it closes the socket before the release.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pooled_codex_socket_keeps_its_home_pick() {
        use futures_util::StreamExt;
        use wreq::ws::message::Message;
        let (upstream, counts) = responses_ws_upstream().await;
        let pick = |id: &str| {
            format!(
                r#"{{"model":"gpt-5-codex","auth_index":"{id}","concurrency":{{"accounted":true,"credential_id":"{id}","model":"gpt-5-codex"}},"auth":{{"id":"{id}","provider":"codex","attributes":{{"api_key":"sk-fake-{id}","base_url":"{upstream}","websockets":"true"}}}}}}"#
            )
        };
        let home = scripted(
            "port: 0\ncredentials:\n  concurrency:\n    lifecycle-config-revision: 1\n    cpa-cancel-bound: 20s\n",
            vec![pick("cred-ws"), pick("cred-spare")],
        )
        .await;
        let (base, rt, shutdown, task) = node_with(&home, cpa_exec::codex::CodexExecutor::new().unwrap()).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let mut socket = wreq::Client::new()
            .websocket(format!("ws{}/v1/responses", base.trim_start_matches("http")))
            .header("authorization", "Bearer client-key")
            .send()
            .await
            .unwrap()
            .into_websocket()
            .await
            .unwrap();
        let turn = r#"{"type":"response.create","model":"gpt-5-codex","input":[{"type":"message","role":"user","content":"hi"}]}"#;
        for round in 0..2 {
            socket.send(Message::text(turn)).await.unwrap();
            loop {
                let next = tokio::time::timeout(Duration::from_secs(5), socket.next())
                    .await
                    .expect("turn completes");
                let Some(Ok(Message::Text(text))) = next else {
                    panic!("turn {round}: {next:?}");
                };
                if text.as_str().contains("response.completed") {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(releases(&home).is_empty(), "turn {round}: the socket keeps the pick");
        }
        assert_eq!(rpops(&home).len(), 1, "the second turn ran on the kept pick");
        assert_eq!(*counts.lock().unwrap(), (1, 0), "one pooled socket, still open");

        let begun = std::time::Instant::now();
        shutdown.cancel();
        task.await.unwrap();
        assert!(
            begun.elapsed() < Duration::from_secs(5),
            "the drain did not wait out the bound"
        );
        assert!(releases(&home).iter().any(|f| f["credential_id"] == "cred-ws"));
        eventually("the drained socket closed", || counts.lock().unwrap().1 == 1).await;
    }

    /// A Codex live upstream that stalls: the calls endpoint sends its headers and then
    /// never finishes the body, and the WebSocket endpoint accepts TCP but never answers
    /// the handshake. Counts requests that reached it.
    async fn stalled_live_upstream() -> (cpa_exec::codex::CodexExecutor, Arc<std::sync::atomic::AtomicUsize>) {
        let reached: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
        let app = axum::Router::new().fallback({
            let reached = reached.clone();
            move || {
                reached.fetch_add(1, Ordering::SeqCst);
                async move {
                    let body = futures_util::StreamExt::chain(
                        futures_util::stream::once(async { Ok::<_, std::convert::Infallible>("v=0") }),
                        futures_util::stream::pending(),
                    );
                    (
                        axum::http::StatusCode::CREATED,
                        [("location", "/v1/realtime/calls/rtc_live1")],
                        axum::body::Body::from_stream(body),
                    )
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_base = format!("ws://{}/v1", silent.local_addr().unwrap());
        tokio::spawn({
            let reached = reached.clone();
            async move {
                let mut held = Vec::new();
                while let Ok((socket, _)) = silent.accept().await {
                    reached.fetch_add(1, Ordering::SeqCst);
                    held.push(socket);
                }
            }
        });
        let codex = cpa_exec::codex::CodexExecutor::with_client(
            wreq::Client::new(),
            cpa_exec::codex_oauth::CodexOAuth::new(wreq::Client::new()),
        )
        .with_live_endpoints(format!("{base}/backend-api/codex/realtime/calls"), ws_base);
        (codex, reached)
    }

    /// Home draining a live pick cancels every upstream read and handshake, not only
    /// the wait for response headers: a stalled call body and a stalled Realtime
    /// WebSocket handshake both end at once with a read or dial failure, instead of
    /// holding the drain for the cancel bound.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_drain_cancels_stalled_live_reads_and_handshakes() {
        let (codex, reached) = stalled_live_upstream().await;
        let home = scripted(
            "port: 0\ncredentials:\n  concurrency:\n    lifecycle-config-revision: 1\n    cpa-cancel-bound: 20s\n",
            vec![
                codex_pick("cred-call", Some("oat-call")),
                codex_pick("cred-ws", Some("oat-ws")),
            ],
        )
        .await;
        let (base, rt, shutdown, task) = node_with(&home, codex).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let call = tokio::spawn({
            let base = base.clone();
            async move { live_call(&base).await }
        });
        let socket = tokio::spawn({
            let base = base.clone();
            async move {
                let response = wreq::Client::new()
                    .get(format!("{base}/v1/realtime?model=gpt-realtime"))
                    .header("authorization", "Bearer client-key")
                    .header("connection", "Upgrade")
                    .header("upgrade", "websocket")
                    .header("sec-websocket-version", "13")
                    .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                    .send()
                    .await
                    .unwrap();
                (response.status().as_u16(), response.text().await.unwrap())
            }
        });
        eventually("both requests reached the upstream", || {
            reached.load(Ordering::SeqCst) == 2
        })
        .await;
        let begun = std::time::Instant::now();
        shutdown.cancel();
        task.await.unwrap();
        assert!(
            begun.elapsed() < Duration::from_secs(5),
            "the drain did not wait out the bound"
        );
        let (status, body) = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status, 502, "{body}");
        assert!(body.contains("Failed to read Codex live response"), "{body}");
        let (status, body) = tokio::time::timeout(Duration::from_secs(5), socket)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status, 502, "{body}");
        assert!(body.contains("realtime_websocket_upstream_unavailable"), "{body}");
        let released = releases(&home);
        for id in ["cred-call", "cred-ws"] {
            assert!(released.iter().any(|f| f["credential_id"] == id), "{id} released");
        }
    }

    /// Go `ReportHomeUnauthorized` from the live handlers: a call the upstream rejects,
    /// and a direct Realtime WebSocket whose handshake it rejects, each become a
    /// `home-result` record for the selection model.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_401s_reach_home_as_home_results() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let codex = live_upstream(seen.clone()).await;
        let home = scripted(
            "port: 0\n",
            vec![
                codex_pick("cred-a", Some("oat-denied")),
                codex_pick("cred-b", Some("oat-ws")),
            ],
        )
        .await;
        let (base, rt, shutdown, task) = node_with(&home, codex).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let (status, body) = live_call(&base).await;
        assert_eq!((status, body.as_str()), (401, "live denied"));
        let response = wreq::Client::new()
            .get(format!("{base}/v1/realtime?model=gpt-realtime"))
            .header("authorization", "Bearer client-key")
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 401);
        assert_eq!(rpops(&home)[1]["model"], "gpt-live-1-codex");
        eventually("both results reached Home", || usage_records(&home).len() == 2).await;
        let records = usage_records(&home);
        let summary: Vec<(String, String, String, Value)> = records
            .iter()
            .map(|r| {
                (
                    r["executor_type"].as_str().unwrap().to_owned(),
                    r["auth_index"].as_str().unwrap().to_owned(),
                    r["model"].as_str().unwrap().to_owned(),
                    r["fail"].clone(),
                )
            })
            .collect();
        let fail = |body: &str| serde_json::json!({"status_code": 401, "body": body});
        assert_eq!(
            summary,
            [
                (
                    "home-result".into(),
                    "cred-a".into(),
                    "gpt-live-1-codex".into(),
                    fail("live denied")
                ),
                (
                    "home-result".into(),
                    "cred-b".into(),
                    "gpt-live-1-codex".into(),
                    fail("ws denied")
                ),
            ]
        );
        eventually("the direct socket's pick ended", || {
            releases(&home).iter().any(|f| f["credential_id"] == "cred-b")
        })
        .await;
        shutdown.cancel();
        task.await.unwrap();
    }

    // ---- Go home_retry_contract_test.go ---------------------------------------------
    //
    // `ContractHome` answers like Go's `retryContractHomeDispatcher` and the upstream
    // like `retryContractHomeExecutor` (by credential: an OpenAI-compatible key named
    // after the credential). Go's 1 ms retry waits become Home's `retry_after_ms` or
    // whole-second upstream `Retry-After` values.

    /// How the contract upstream answers one credential.
    #[derive(Clone)]
    enum Answer {
        Ok,
        /// Status, `Retry-After` seconds, body.
        Fail(u16, Option<u64>, &'static str),
    }

    /// Go `retryContractRateLimitError`: 429, retry after `seconds`.
    fn rate_limited(seconds: u64) -> Answer {
        Answer::Fail(429, Some(seconds), r#"{"error":{"message":"credential rate limited"}}"#)
    }

    fn bad_gateway() -> Answer {
        Answer::Fail(502, None, r#"{"error":{"message":"upstream unavailable"}}"#)
    }

    /// An OpenAI-compatible upstream answering each credential (its bearer key) from
    /// `answers`; any other succeeds with its own ID as the content.
    struct ContractUpstream {
        base: String,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl ContractUpstream {
        async fn start(answers: Vec<(&'static str, Answer)>) -> Self {
            use axum::response::IntoResponse;
            let calls: Arc<Mutex<Vec<String>>> = Arc::default();
            let answers: std::collections::HashMap<&'static str, Answer> = answers.into_iter().collect();
            let app = axum::Router::new().route(
                "/chat/completions",
                axum::routing::post({
                    let calls = calls.clone();
                    move |headers: axum::http::HeaderMap, body: String| {
                        let (calls, answers) = (calls.clone(), answers.clone());
                        async move {
                            let key = headers
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or_default()
                                .trim_start_matches("Bearer ")
                                .to_owned();
                            calls.lock().unwrap().push(key.clone());
                            let stream = serde_json::from_str::<Value>(&body).unwrap()["stream"] == true;
                            match answers.get(key.as_str()).cloned().unwrap_or(Answer::Ok) {
                                Answer::Fail(status, retry_after, body) => {
                                    let mut response = (
                                        axum::http::StatusCode::from_u16(status).unwrap(),
                                        [("content-type", "application/json")],
                                        body,
                                    )
                                        .into_response();
                                    if let Some(seconds) = retry_after {
                                        response
                                            .headers_mut()
                                            .insert("retry-after", seconds.to_string().parse().unwrap());
                                    }
                                    response
                                }
                                Answer::Ok if stream => (
                                    [("content-type", "text/event-stream")],
                                    format!(
                                        "data: {}\n\ndata: [DONE]\n\n",
                                        serde_json::json!({"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "gpt",
                                            "choices": [{"index": 0, "delta": {"content": key}, "finish_reason": "stop"}]})
                                    ),
                                )
                                    .into_response(),
                                Answer::Ok => axum::Json(serde_json::json!({
                                    "id": "c1", "object": "chat.completion", "created": 1, "model": "gpt",
                                    "choices": [{"index": 0, "message": {"role": "assistant", "content": key}, "finish_reason": "stop"}],
                                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                                }))
                                .into_response(),
                            }
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            Self { base, calls }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    /// One dispatched `home-retry-contract` credential on the contract upstream.
    fn contract_pick(id: &str, upstream: &str, request_retry: Option<i64>, metadata: &Value) -> String {
        let mut reply = serde_json::json!({
            "model": "gpt",
            "auth": {"id": id, "provider": "home-retry-contract",
                     "attributes": {"api_key": id, "base_url": upstream}, "metadata": metadata},
        });
        if let Some(retry) = request_retry {
            reply["request_retry"] = retry.into();
        }
        reply.to_string()
    }

    /// Go `retryContractHomeDispatcher`: the first listed credential neither excluded nor
    /// (when pinned) another; with none left, `exhausted` (RESP bytes; nil by default).
    async fn contract_home(
        config: &'static str,
        auths: Vec<&'static str>,
        upstream: &str,
        request_retry: Option<i64>,
        metadata: Value,
        exhausted: Option<Reply>,
    ) -> FakeHome {
        let upstream = upstream.to_owned();
        let exhausted = Mutex::new(exhausted.map(|r| match r {
            Reply::Bytes(bytes) => bytes,
            _ => unreachable!(),
        }));
        FakeHome::start(move |args| match args[0].to_lowercase().as_str() {
            "get" => fake::bulk(config),
            "subscribe" => fake::raw(ACK),
            "ping" => fake::raw("+PONG\r\n"),
            "rpop" => {
                let request: Value = serde_json::from_str(&args[1]).unwrap();
                let excluded: Vec<String> = request["excluded_auth_ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|v| v.as_str().unwrap().to_owned())
                    .collect();
                let pinned = request["pinned_auth_id"].as_str().unwrap_or_default();
                match auths
                    .iter()
                    .find(|id| (pinned.is_empty() || **id == pinned) && !excluded.iter().any(|e| e == *id))
                {
                    Some(id) => fake::bulk(contract_pick(id, &upstream, request_retry, &metadata)),
                    None => match exhausted.lock().unwrap().clone() {
                        Some(bytes) => Reply::Bytes(bytes),
                        None => fake::raw("$-1\r\n"),
                    },
                }
            }
            "lpush" => fake::raw(":1\r\n"),
            _ => fake::raw("-ERR unexpected\r\n"),
        })
        .await
    }

    /// The exclusion list each pick sent (empty when none).
    fn exclusions(home: &FakeHome) -> Vec<Vec<String>> {
        rpops(home)
            .iter()
            .map(|r| {
                r["excluded_auth_ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|v| v.as_str().unwrap().to_owned())
                    .collect()
            })
            .collect()
    }

    /// A chat completion through the node: status, `Retry-After`, body.
    async fn chat(base: &str, stream: bool) -> (u16, Option<String>, String) {
        let body =
            serde_json::json!({"model": "gpt", "stream": stream, "messages": [{"role": "user", "content": "hi"}]});
        ask_with_headers(base, "/v1/chat/completions", body).await
    }

    async fn contract_node(home: &FakeHome) -> (String, Arc<Runtime>, CancellationToken, tokio::task::JoinHandle<()>) {
        let (base, rt, shutdown, task) = node(home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        (base, rt, shutdown, task)
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Go `TestHomeRetryRoundTriesFreshCredentialWhenRequestRetryIsZero`,
    /// `TestHomeStreamOAuthUnauthorizedRotatesWithoutRefreshRetry` and
    /// `TestHomeStreamAPIKeyUnauthorizedRotatesImmediately`: within one round a failed
    /// credential is excluded and the next one serves, streamed or not; a 401 rotates
    /// without a refresh-and-retry, whatever the credential's kind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_round_rotates_to_a_fresh_credential() {
        let unauthorized = || Answer::Fail(401, None, r#"{"error":{"message":"expired"}}"#);
        let cases = [
            (rate_limited(1), false, Value::Null),
            (rate_limited(1), true, Value::Null),
            (unauthorized(), true, serde_json::json!({"auth_kind": "oauth"})),
            (unauthorized(), true, serde_json::json!({"auth_kind": "apikey"})),
        ];
        for (answer, stream, metadata) in cases {
            let upstream = ContractUpstream::start(vec![("home-retry-a", answer)]).await;
            let home = contract_home(
                "port: 0\nrequest-retry: 0\nmax-retry-interval: 1\nmax-retry-credentials: 2\n",
                vec!["home-retry-a", "home-retry-b"],
                &upstream.base,
                None,
                metadata,
                None,
            )
            .await;
            let (base, _rt, shutdown, task) = contract_node(&home).await;
            let (status, _, body) = chat(&base, stream).await;
            assert_eq!(status, 200, "{body}");
            assert!(body.contains("home-retry-b"), "{body}");
            assert_eq!(upstream.calls(), strings(&["home-retry-a", "home-retry-b"]));
            assert_eq!(exclusions(&home), vec![vec![], strings(&["home-retry-a"])]);
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    /// Go `TestHomeRequestRetryCountsAdditionalCredentialRounds` and
    /// `TestHomeRequestRetryRoundDoesNotRequireRetryAfter`: request-retry 1 runs a second
    /// round through both credentials, after a rate limit or a plain 502 alike.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_retry_counts_additional_credential_rounds() {
        for (fail, stream) in [
            (rate_limited(1), false),
            (rate_limited(1), true),
            (bad_gateway(), false),
            (bad_gateway(), true),
        ] {
            let upstream = ContractUpstream::start(vec![("home-retry-a", fail.clone()), ("home-retry-b", fail)]).await;
            let home = contract_home(
                "port: 0\nrequest-retry: 1\nmax-retry-interval: 1\nmax-retry-credentials: 2\n",
                vec!["home-retry-a", "home-retry-b"],
                &upstream.base,
                None,
                Value::Null,
                None,
            )
            .await;
            let (base, _rt, shutdown, task) = contract_node(&home).await;
            let (status, _, body) = chat(&base, stream).await;
            assert_ne!(status, 200, "{body}");
            assert_eq!(upstream.calls().len(), 4, "{:?}", upstream.calls());
            let excluded = exclusions(&home);
            assert_eq!(
                excluded[..4],
                [vec![], strings(&["home-retry-a"]), vec![], strings(&["home-retry-a"])]
            );
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    /// Go `TestHomeRetryRoundHonorsCredentialRequestRetryOverride` and
    /// `TestHomeRetryRoundUsesAuthoritativeZeroAggregate`: a credential's own
    /// `request_retry` replaces the configured one, and Home's aggregate `request_retry`
    /// of 0 wins over both.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn credential_and_aggregate_request_retry_overrides() {
        // Config, Home's aggregate, the credential's own, the upstream's answer, calls.
        let cases: [(&'static str, Option<i64>, i64, Answer, usize); 3] = [
            (
                "port: 0\nrequest-retry: 3\nmax-retry-interval: 1\nmax-retry-credentials: 2\n",
                None,
                0,
                rate_limited(1),
                2,
            ),
            (
                "port: 0\nrequest-retry: 0\nmax-retry-interval: 1\nmax-retry-credentials: 2\n",
                None,
                1,
                rate_limited(1),
                4,
            ),
            (
                "port: 0\nrequest-retry: 3\nmax-retry-interval: 1\nmax-retry-credentials: 2\n",
                Some(0),
                3,
                bad_gateway(),
                2,
            ),
        ];
        for (config, aggregate, own, fail, want) in cases {
            let upstream = ContractUpstream::start(vec![("home-retry-a", fail.clone()), ("home-retry-b", fail)]).await;
            let home = contract_home(
                config,
                vec!["home-retry-a", "home-retry-b"],
                &upstream.base,
                aggregate,
                serde_json::json!({"request_retry": own}),
                None,
            )
            .await;
            let (base, _rt, shutdown, task) = contract_node(&home).await;
            let (status, _, body) = chat(&base, false).await;
            assert_ne!(status, 200, "{body}");
            assert_eq!(upstream.calls().len(), want, "aggregate {aggregate:?} own {own}");
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    /// Go `TestHomeRetryRoundUsesAuthoritativeRemoteCooldown` and
    /// `TestHomeRetryRoundUsesRemoteCooldownWhenAttemptedErrorHasNoTiming`: when Home
    /// answers a round's last pick with a cooldown, its retry-after replaces the
    /// upstream's own, longer or shorter (Go `markHomeRetryRoundExhausted(…,
    /// homeCooldown.RetryAfter(), …)`), and the client sees it rounded up.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_remote_cooldown_times_the_round() {
        let cooldown = |ms: u64| {
            fake::bulk(format!(
                r#"{{"error":{{"type":"model_cooldown","message":"remaining Home credentials are cooling down","retryable":true,"retry_after_ms":{ms}}}}}"#
            ))
        };
        for (answer, ms, want) in [
            (rate_limited(2), 5000, "5"),
            (rate_limited(5), 1500, "2"),
            (bad_gateway(), 1500, "2"),
        ] {
            for stream in [false, true] {
                let upstream = ContractUpstream::start(vec![("home-retry-a", answer.clone())]).await;
                let home = contract_home(
                    "port: 0\nrequest-retry: 0\nmax-retry-interval: 1\nmax-retry-credentials: 2\n",
                    vec!["home-retry-a"],
                    &upstream.base,
                    None,
                    Value::Null,
                    Some(cooldown(ms)),
                )
                .await;
                let (base, _rt, shutdown, task) = contract_node(&home).await;
                let (status, retry_after, body) = chat(&base, stream).await;
                assert_ne!(status, 200, "{body}");
                assert_eq!(retry_after.as_deref(), Some(want), "{ms} ms, stream {stream}: {body}");
                shutdown.cancel();
                task.await.unwrap();
            }
        }
    }

    /// Go `TestHomeRetryRoundUsesEarliestCredentialRetryAfter` and
    /// `TestHomeStreamBootstrapErrorPreservesAggregatedRetryAfter`: a round whose
    /// credentials all rate-limited ends with the earliest retry-after, not the last
    /// upstream's, streamed or not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_round_takes_the_earliest_credential_retry_after() {
        for stream in [false, true] {
            let upstream = ContractUpstream::start(vec![
                ("home-retry-a", rate_limited(2)),
                ("home-retry-b", rate_limited(5)),
            ])
            .await;
            let home = contract_home(
                "port: 0\nrequest-retry: 0\nmax-retry-interval: 1\nmax-retry-credentials: 2\n",
                vec!["home-retry-a", "home-retry-b"],
                &upstream.base,
                None,
                Value::Null,
                None,
            )
            .await;
            let (base, _rt, shutdown, task) = contract_node(&home).await;
            let (status, retry_after, body) = chat(&base, stream).await;
            assert_eq!(status, 429, "{body}");
            assert_eq!(retry_after.as_deref(), Some("2"), "stream {stream}: {body}");
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    /// Go `TestHomeCooldownClassificationPreservesNonRetryableRoundStatus`: a Home
    /// cooldown after a 401 keeps the 401 and starts no further round, even though the
    /// cooldown named a request-retry of 2.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cooldown_after_a_non_retryable_failure_keeps_its_status() {
        for stream in [false, true] {
            let upstream = ContractUpstream::start(vec![(
                "home-retry-a",
                Answer::Fail(401, None, r#"{"error":{"message":"invalid credential"}}"#),
            )])
            .await;
            let home = contract_home(
                "port: 0\nrequest-retry: 3\nmax-retry-interval: 1\nmax-retry-credentials: 2\n",
                vec!["home-retry-a"],
                &upstream.base,
                None,
                Value::Null,
                Some(fake::bulk(
                    r#"{"error":{"type":"model_cooldown","message":"another credential is cooling down","retryable":true,"retry_after_ms":5,"request_retry":2}}"#,
                )),
            )
            .await;
            let (base, _rt, shutdown, task) = contract_node(&home).await;
            let (status, _, body) = chat(&base, stream).await;
            assert_eq!(status, 401, "stream {stream}: {body}");
            assert_eq!(upstream.calls().len(), 1, "no further round");
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    /// Go `TestHomeRetryRoundStartsImmediatelyWhenHomeReportsAvailableNextRound`: Home's
    /// `auth_unavailable` ends the round with the next one starting at once, despite the
    /// credentials' long retry-after.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_available_next_round_starts_immediately() {
        for stream in [false, true] {
            let upstream =
                ContractUpstream::start(vec![("home-retry-a", rate_limited(5)), ("home-retry-b", bad_gateway())]).await;
            let home = contract_home(
                "port: 0\nrequest-retry: 1\nmax-retry-interval: 10\n",
                vec!["home-retry-a", "home-retry-b"],
                &upstream.base,
                None,
                Value::Null,
                Some(fake::bulk(
                    r#"{"error":{"type":"auth_unavailable","message":"a credential is immediately available next round"}}"#,
                )),
            )
            .await;
            let (base, _rt, shutdown, task) = contract_node(&home).await;
            let started = std::time::Instant::now();
            let (status, _, body) = chat(&base, stream).await;
            assert_ne!(status, 200, "{body}");
            assert!(
                started.elapsed() < Duration::from_secs(4),
                "no wait before the next round"
            );
            assert_eq!(upstream.calls().len(), 4, "a second round ran: {:?}", upstream.calls());
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    /// Go `TestHomePreferredErrorIgnoresLaterInternalExecutorFailure`: a later credential
    /// that failed locally (no base URL: nothing reached upstream) does not replace the
    /// earlier upstream failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_later_local_failure_keeps_the_upstream_error() {
        let upstream = ContractUpstream::start(vec![("home-retry-a", bad_gateway())]).await;
        let local = serde_json::json!({
            "model": "gpt",
            "auth": {"id": "home-retry-b", "provider": "openai-compatibility", "label": "contract",
                     "attributes": {"api_key": "home-retry-b", "compat_name": "contract"}},
        })
        .to_string();
        let home = scripted(
            "port: 0\nrequest-retry: 0\nmax-retry-credentials: 2\n",
            vec![contract_pick("home-retry-a", &upstream.base, None, &Value::Null), local],
        )
        .await;
        let (base, _rt, shutdown, task) = contract_node(&home).await;
        let (status, _, body) = chat(&base, false).await;
        assert_eq!(status, 502, "{body}");
        assert!(body.contains("upstream unavailable"), "{body}");
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go `TestHomeSelectionFailureIsNotHiddenByEarlierUpstreamAttempt`: a pick that fails
    /// (Home unreachable, an invalid auth payload, a malformed concurrency tuple) is the
    /// request's error, not the earlier upstream failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_selection_failure_is_not_hidden_by_an_earlier_upstream_failure() {
        let cases = [
            (fake::raw("-ERR Home transport failed\r\n"), "home_unavailable"),
            (fake::bulk("{}"), "invalid_auth"),
            (
                fake::bulk(
                    r#"{"concurrency":{"accounted":true,"credential_id":"home-retry-a","model":"gpt"},"error":"busy","auth":{"id":"home-retry-a","provider":"home-retry-contract"}}"#,
                ),
                "invalid_home_concurrency",
            ),
        ];
        for (exhausted, code) in cases {
            let upstream = ContractUpstream::start(vec![("home-retry-a", bad_gateway())]).await;
            let home = contract_home(
                "port: 0\nrequest-retry: 0\n",
                vec!["home-retry-a"],
                &upstream.base,
                None,
                Value::Null,
                Some(exhausted),
            )
            .await;
            let (base, _rt, shutdown, task) = contract_node(&home).await;
            let (status, _, body) = chat(&base, false).await;
            assert!(body.contains(code), "{status} {body}");
            assert!(!body.contains("upstream unavailable"), "{body}");
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    /// Go `TestHomeModelCooldownErrorPreservesRetryContract`: Home's `model_cooldown`
    /// is a retryable 429 whose `Retry-After` is its `retry_after_ms` rounded up.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_model_cooldown_is_a_retryable_429() {
        let home = scripted(
            "port: 0\nrequest-retry: 0\n",
            vec![
                r#"{"error":{"type":"model_cooldown","message":"all credentials are cooling down","retryable":true,"retry_after_ms":1500,"request_retry":2}}"#
                    .into(),
            ],
        )
        .await;
        let (base, _rt, shutdown, task) = contract_node(&home).await;
        let (status, retry_after, body) = chat(&base, false).await;
        assert_eq!((status, retry_after.as_deref()), (429, Some("2")), "{body}");
        assert!(body.contains("model_cooldown"), "{body}");
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go `TestHomeRetryRoundUsesSuccessfulDispatchAggregate`: the aggregate request-retry
    /// of a successful pick (2) applies over the failed credential's own (0), so the next
    /// credential is tried even with max-retry-credentials 1.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_successful_pick_aggregate_starts_the_next_round() {
        for stream in [false, true] {
            use std::sync::atomic::AtomicUsize;
            let upstream = ContractUpstream::start(vec![("home-retry-a", rate_limited(1))]).await;
            let calls = Arc::new(AtomicUsize::new(0));
            let base_url = upstream.base.clone();
            let home = FakeHome::start({
                let calls = calls.clone();
                move |args| match args[0].to_lowercase().as_str() {
                    "get" => fake::bulk("port: 0\nrequest-retry: 0\nmax-retry-interval: 1\nmax-retry-credentials: 1\n"),
                    "subscribe" => fake::raw(ACK),
                    "ping" => fake::raw("+PONG\r\n"),
                    "rpop" => {
                        let (id, own) = match calls.fetch_add(1, Ordering::SeqCst) {
                            0 => ("home-retry-a", 0),
                            _ => ("home-retry-b", 2),
                        };
                        fake::bulk(contract_pick(
                            id,
                            &base_url,
                            Some(2),
                            &serde_json::json!({"request_retry": own}),
                        ))
                    }
                    "lpush" => fake::raw(":1\r\n"),
                    _ => fake::raw("-ERR unexpected\r\n"),
                }
            })
            .await;
            let (base, _rt, shutdown, task) = contract_node(&home).await;
            let (status, _, body) = chat(&base, stream).await;
            assert_eq!(status, 200, "{body}");
            assert_eq!(upstream.calls(), strings(&["home-retry-a", "home-retry-b"]));
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    /// A Home `model_cooldown` dispatch reply of 1 ms naming `request_retry`.
    fn cooldown_reply(request_retry: i64) -> Reply {
        fake::bulk(format!(
            r#"{{"error":{{"type":"model_cooldown","message":"credential is cooling down","retryable":true,"retry_after_ms":1,"request_retry":{request_retry}}}}}"#
        ))
    }

    /// Go `TestHomeCredentialLimitWaitsBeforeConsumingAdditionalRound`,
    /// `TestHomePendingRetryRoundStopsWhenRemoteLimitDrops` and
    /// `TestHomePendingRetryRoundStopsAfterRepeatedCooldown`: a cooldown answering the
    /// next round's first pick is waited out once, within Home's request-retry, and
    /// does not consume the round; a second cooldown, or one lowering request-retry to
    /// 0, ends the request with the cooldown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pending_round_waits_out_one_home_cooldown() {
        const CONFIG: &str = "port: 0\nrequest-retry: 1\nmax-retry-interval: 1\nmax-retry-credentials: 1\n";
        // The picks after the first, the upstream calls and picks Go expects, and
        // whether the request ends with Home's cooldown.
        type Case = (fn(&str) -> Vec<Reply>, usize, usize, bool);
        let cases: [Case; 3] = [
            (
                |up| {
                    vec![
                        cooldown_reply(1),
                        fake::bulk(contract_pick("home-retry-a", up, None, &Value::Null)),
                    ]
                },
                2,
                3,
                false,
            ),
            (
                |up| {
                    vec![
                        cooldown_reply(0),
                        fake::bulk(contract_pick("home-retry-b", up, None, &Value::Null)),
                    ]
                },
                1,
                2,
                true,
            ),
            (|_| vec![cooldown_reply(1), cooldown_reply(1)], 1, 3, true),
        ];
        for (index, (rest, calls, picks, cooled)) in cases.into_iter().enumerate() {
            for stream in [false, true] {
                let upstream = ContractUpstream::start(vec![("home-retry-a", rate_limited(1))]).await;
                // The first pick carries an aggregate request-retry of 1, as in Go's
                // `retryRoundLimitDownshiftDispatcher`.
                let mut replies = vec![fake::bulk(contract_pick(
                    "home-retry-a",
                    &upstream.base,
                    Some(1),
                    &Value::Null,
                ))];
                replies.extend(rest(&upstream.base));
                let home = sequenced(CONFIG, replies, Default::default()).await;
                let (base, _rt, shutdown, task) = contract_node(&home).await;
                let (status, _, body) = chat(&base, stream).await;
                let case = format!("case {index}, stream {stream}: {status} {body}");
                assert_eq!(upstream.calls().len(), calls, "{case}");
                assert_eq!(rpops(&home).len(), picks, "{case}");
                assert_eq!(status, 429, "{case}");
                assert_eq!(body.contains("model_cooldown"), cooled, "{case}");
                shutdown.cancel();
                task.await.unwrap();
            }
        }
    }

    /// Go `TestHomeNextRoundSelectionFailureSupersedesEarlierUpstreamAttempt`: a pick
    /// failure opening the next round is the request's error, not the previous round's
    /// upstream failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_next_round_selection_failure_supersedes_the_upstream_error() {
        // The RESP reply opening round 1 (a Home error, an empty auth) and Go's code.
        let failures = [
            ("-ERR Home transport failed\r\n", "home_unavailable"),
            ("$2\r\n{}\r\n", "invalid_auth"),
        ];
        for (failure, code) in failures {
            for stream in [false, true] {
                let upstream = ContractUpstream::start(vec![("home-retry-a", bad_gateway())]).await;
                let replies = vec![
                    fake::bulk(contract_pick("home-retry-a", &upstream.base, None, &Value::Null)),
                    fake::raw(failure),
                ];
                let home = sequenced(
                    "port: 0\nrequest-retry: 1\nmax-retry-interval: 1\nmax-retry-credentials: 1\n",
                    replies,
                    Default::default(),
                )
                .await;
                let (base, _rt, shutdown, task) = contract_node(&home).await;
                let (status, _, body) = chat(&base, stream).await;
                assert!(body.contains(code), "stream {stream}: {status} {body}");
                assert!(!body.contains("upstream unavailable"), "{body}");
                assert_eq!(rpops(&home)[1]["retry_round"], 1, "the failure opened round 1");
                shutdown.cancel();
                task.await.unwrap();
            }
        }
    }

    /// Go `TestHomeStreamLegacyDispatcherDoesNotSpinOnIgnoredExclusions` and
    /// `TestHomeNonStreamLegacyDispatcherCompletesAdditionalRetryRound`: a Home that
    /// ignores exclusions and repeats the failed credential ends each round on the
    /// repeat, so two rounds make two upstream calls and four picks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_home_ignoring_exclusions_ends_each_round_on_the_repeat() {
        for (stream, config) in [
            (
                true,
                "port: 0\nrequest-retry: 1\nmax-retry-interval: 1\nmax-retry-credentials: 2\n",
            ),
            (
                false,
                "port: 0\nrequest-retry: 1\nmax-retry-interval: 1\nmax-retry-credentials: 0\n",
            ),
        ] {
            let upstream = ContractUpstream::start(vec![("home-retry-a", bad_gateway())]).await;
            let pick = contract_pick("home-retry-a", &upstream.base, None, &Value::Null);
            let home = FakeHome::start(move |args| match args[0].to_lowercase().as_str() {
                "get" => fake::bulk(config),
                "subscribe" => fake::raw(ACK),
                "ping" => fake::raw("+PONG\r\n"),
                "rpop" => fake::bulk(&pick),
                "lpush" => fake::raw(":1\r\n"),
                _ => fake::raw("-ERR unexpected\r\n"),
            })
            .await;
            let (base, _rt, shutdown, task) = contract_node(&home).await;
            let (status, _, body) = chat(&base, stream).await;
            assert_eq!(status, 502, "stream {stream}: {body}");
            assert_eq!(upstream.calls().len(), 2, "stream {stream}");
            assert_eq!(rpops(&home).len(), 4, "stream {stream}");
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    /// Go `TestHomeLocalSelectionRejectionWaitsForReleaseAcknowledgement`: a pick the
    /// node rejects (a repeated credential, buffered or streamed, or one beyond
    /// max-retry-credentials) is released and its release acknowledged before anything
    /// else; without the acknowledgement within the cancel bound the request fails with
    /// `home_unavailable`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rejected_pick_waits_for_its_release_acknowledgement() {
        use std::sync::atomic::AtomicUsize;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let good = upstream(seen.clone()).await;
        let flaky = truncating_upstream(Arc::new(AtomicUsize::new(0))).await;
        const BASE: &str = "port: 0\nrequest-retry: 0\ncredentials:\n  concurrency:\n    cpa-cancel-bound: 200ms\n";
        const CAPPED: &str = "port: 0\nrequest-retry: 0\nmax-retry-credentials: 1\ncredentials:\n  concurrency:\n    cpa-cancel-bound: 200ms\n";
        // Stream, config, picks, and the (credential, release_seq) Home never acknowledges.
        let cases = [
            (
                false,
                BASE,
                vec![
                    accounted("cred-a", "sk-bad", &good),
                    accounted("cred-a", "sk-bad", &good),
                ],
                ("cred-a", 2),
            ),
            (
                true,
                BASE,
                vec![
                    accounted("cred-a", "sk-bad", &good),
                    accounted("cred-a", "sk-bad", &good),
                ],
                ("cred-a", 2),
            ),
            (
                true,
                CAPPED,
                vec![
                    accounted("cred-a", "sk-flaky", &flaky),
                    accounted("cred-b", "sk-good", &good),
                ],
                ("cred-b", 1),
            ),
        ];
        for (stream, config, picks, (blocked_id, blocked_seq)) in cases {
            let replies = Mutex::new(std::collections::VecDeque::from(picks));
            let blocked_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let blocked_seen_by_home = blocked_seen.clone();
            let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let done_by_home = done.clone();
            // The Claude executor keeps its session IDs in Home KV.
            let home = FakeHome::start(with_kv(Default::default(), move |args| {
                match args[0].to_lowercase().as_str() {
                    "get" => fake::bulk(config),
                    "subscribe" => fake::raw(ACK),
                    "ping" => fake::raw("+PONG\r\n"),
                    "rpop" => replies
                        .lock()
                        .unwrap()
                        .pop_front()
                        .map_or_else(|| fake::raw("$-1\r\n"), fake::bulk),
                    "lpush" if args[1] == "concurrency-release" => {
                        let blocked = args[2..].iter().any(|frame| {
                            let frame: Value = serde_json::from_str(frame).unwrap();
                            frame["credential_id"] == blocked_id && frame["release_seq"] == blocked_seq
                        });
                        // Once the test has its answer, flushes are acknowledged so the
                        // drain does not wait out the default cancel bound.
                        if blocked && !done_by_home.load(Ordering::SeqCst) {
                            blocked_seen_by_home.store(true, Ordering::SeqCst);
                            fake::raw("-ERR release not acknowledged\r\n")
                        } else {
                            fake::raw(":1\r\n")
                        }
                    }
                    "lpush" => fake::raw(":1\r\n"),
                    _ => fake::raw("-ERR unexpected\r\n"),
                }
            }))
            .await;
            let (base, _rt, shutdown, task) = contract_node(&home).await;
            let body = serde_json::json!({"model": "claude-x", "max_tokens": 8, "stream": stream,
                "messages": [{"role": "user", "content": "hi"}]});
            let (status, _, text) = ask_with_headers(&base, "/v1/messages", body).await;
            assert!(
                blocked_seen.load(Ordering::SeqCst),
                "the release of {blocked_id} #{blocked_seq} was attempted"
            );
            assert!(
                text.contains("home_unavailable: Home did not acknowledge credential release"),
                "stream {stream}, {blocked_id}: {status} {text}"
            );
            done.store(true, Ordering::SeqCst);
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    /// Go `TestHomePinnedAuthRetriesOnlyPinnedCredential` and
    /// `TestHomePinnedAuthRejectsMismatchedDispatch`, on Home mode's pinned path: polling
    /// a video pins the credential that created it (Go `contextWithVideoAuthBinding`).
    /// Each round asks Home for the pinned credential with no exclusions, and the
    /// credential's own request-retry (1) applies, not Home's aggregate (3). A Home that
    /// answers the pin with another credential is refused before anything executes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pinned_video_poll_uses_only_the_pinned_credential() {
        use axum::response::IntoResponse;
        let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        // xAI: creation fails for `home-retry-a` and creates `vid-pin` for anyone else;
        // polling always fails.
        let app = axum::Router::new()
            .route(
                "/videos/generations",
                axum::routing::post({
                    let calls = calls.clone();
                    move |headers: axum::http::HeaderMap| {
                        let calls = calls.clone();
                        async move {
                            let key = headers["authorization"]
                                .to_str()
                                .unwrap()
                                .trim_start_matches("Bearer ")
                                .to_owned();
                            calls.lock().unwrap().push(("create".into(), key.clone()));
                            if key == "home-retry-a" {
                                return (axum::http::StatusCode::BAD_GATEWAY, r#"{"error":"busy"}"#).into_response();
                            }
                            axum::Json(serde_json::json!({"request_id": "vid-pin"})).into_response()
                        }
                    }
                }),
            )
            .route(
                "/videos/{id}",
                axum::routing::get({
                    let calls = calls.clone();
                    move |headers: axum::http::HeaderMap| {
                        let calls = calls.clone();
                        async move {
                            let key = headers["authorization"]
                                .to_str()
                                .unwrap()
                                .trim_start_matches("Bearer ")
                                .to_owned();
                            calls.lock().unwrap().push(("poll".into(), key));
                            (
                                axum::http::StatusCode::BAD_GATEWAY,
                                r#"{"error":"upstream unavailable"}"#,
                            )
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for honours_pin in [true, false] {
            calls.lock().unwrap().clear();
            let pick = {
                let upstream = upstream.clone();
                move |id: &str| {
                    serde_json::json!({
                        "model": "grok-imagine-video", "request_retry": 3,
                        "auth": {"id": id, "provider": "xai",
                                 "attributes": {"api_key": id, "base_url": upstream},
                                 "metadata": {"request_retry": 1}},
                    })
                    .to_string()
                }
            };
            let home = FakeHome::start(move |args| match args[0].to_lowercase().as_str() {
                "get" => fake::bulk("port: 0\nrequest-retry: 0\nmax-retry-interval: 1\n"),
                "subscribe" => fake::raw(ACK),
                "ping" => fake::raw("+PONG\r\n"),
                "rpop" => {
                    let request: Value = serde_json::from_str(&args[1]).unwrap();
                    let excluded = request["excluded_auth_ids"].as_array().cloned().unwrap_or_default();
                    match request["pinned_auth_id"].as_str().unwrap_or_default() {
                        "" => match ["home-retry-a", "home-retry-b"]
                            .into_iter()
                            .find(|id| !excluded.contains(&(*id).into()))
                        {
                            Some(id) => fake::bulk(pick(id)),
                            None => fake::raw("$-1\r\n"),
                        },
                        pinned if honours_pin => fake::bulk(pick(pinned)),
                        _ => fake::bulk(pick("home-retry-a")),
                    }
                }
                "lpush" => fake::raw(":1\r\n"),
                _ => fake::raw("-ERR unexpected\r\n"),
            })
            .await;
            let (base, _rt, shutdown, task) = contract_node(&home).await;
            // Creation rotates past `home-retry-a` and binds the video to `home-retry-b`.
            let (status, _, body) = ask_with_headers(
                &base,
                "/v1/videos/generations",
                serde_json::json!({"model": "grok-imagine-video", "prompt": "a cat"}),
            )
            .await;
            assert_eq!(status, 200, "{body}");
            let created = rpops(&home).len();
            let response = wreq::Client::new()
                .get(format!("{base}/v1/videos/vid-pin"))
                .header("authorization", "Bearer client-key")
                .send()
                .await
                .unwrap();
            let status = response.status().as_u16();
            let body = response.text().await.unwrap();
            let polls: Vec<Value> = rpops(&home)[created..].to_vec();
            assert!(polls.iter().all(|r| r["pinned_auth_id"] == "home-retry-b"), "{polls:?}");
            let polled: Vec<String> = calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(kind, _)| kind == "poll")
                .map(|(_, key)| key.clone())
                .collect();
            if honours_pin {
                assert_eq!(status, 502, "{body}");
                assert_eq!(
                    polled,
                    strings(&["home-retry-b", "home-retry-b"]),
                    "one poll in each of two rounds"
                );
                assert_eq!(polls.len(), 2, "one pick per round: {polls:?}");
                assert!(polls.iter().all(|r| r.get("excluded_auth_ids").is_none()), "{polls:?}");
            } else {
                assert!(
                    body.contains("home returned an auth that does not match the pinned credential"),
                    "{status} {body}"
                );
                assert!(
                    polled.is_empty(),
                    "the mismatched credential never executes: {polled:?}"
                );
            }
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    /// Go `TestHomeUnauthorizedReturnsOriginalErrorWithoutRefresh`,
    /// `TestHomeUnauthorizedDoesNotRefreshOrReplay` and
    /// `TestHomeUnauthorizedIgnoresExecutorRefreshFailure`: a Home credential's 401 is
    /// returned as is, with no refresh-and-retry, even when Home's auth carries a refresh
    /// token and the node holds a refreshable local credential of the same ID (local
    /// mode would refresh that one and retry with its token).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_home_401_never_refreshes_a_local_credential() {
        use axum::response::IntoResponse;
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let app = axum::Router::new().fallback({
            let calls = calls.clone();
            move |uri: axum::http::Uri, headers: axum::http::HeaderMap| {
                let calls = calls.clone();
                async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    calls.lock().unwrap().push(format!("{} {auth}", uri.path()));
                    match uri.path() {
                        "/token" => axum::Json(serde_json::json!({
                            "access_token": "sk-ant-oat-new-fake", "refresh_token": "fake-rotated", "expires_in": 3600,
                            "account": {"uuid": "acct-fake", "email_address": "a@example.invalid"}}))
                        .into_response(),
                        _ => (
                            axum::http::StatusCode::UNAUTHORIZED,
                            [("content-type", "application/json")],
                            r#"{"type":"error","error":{"type":"authentication_error","message":"token expired"}}"#,
                        )
                            .into_response(),
                    }
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let oauth = cpa_exec::oauth::OAuth::with_endpoints(
            wreq::Client::new(),
            &format!("{upstream}/token"),
            &format!("{upstream}/profile"),
            &format!("{upstream}/roles"),
        );
        let executors = cpa_exec::Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new(&upstream)
                .unwrap()
                .with_oauth(oauth),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        };
        // A local credential that shares the Home pick's ID and can refresh.
        let dir = std::env::temp_dir().join(format!("cpa-home-401-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cred-oauth");
        let metadata = serde_json::json!({"type": "claude", "access_token": "sk-ant-oat-local-fake",
            "refresh_token": "fake-refresh", "expired": "2000-01-01T00:00:00Z",
            "account_uuid": "acct-fake", "email": "a@example.invalid"});
        std::fs::write(&path, metadata.to_string()).unwrap();
        let mut local = Credential::from_file(&dir, &path, metadata.as_object().unwrap().clone()).unwrap();
        local.id = "cred-oauth".into();
        local.attributes.insert("base_url".into(), upstream.clone());
        let pick = format!(
            r#"{{"model":"claude-up","auth_index":"idx-oauth","auth":{{"id":"cred-oauth","provider":"claude","attributes":{{"base_url":"{upstream}"}},"metadata":{{"type":"claude","access_token":"sk-ant-oat01-FAKE","refresh_token":"fake-home-refresh","account_uuid":"8f14e45f-ceea-467f-a8a1-0c3b9e1f3a77"}}}}}}"#
        );
        let home = scripted_with_kv("port: 0\nrequest-retry: 0\n", vec![pick], Default::default()).await;
        let (base, rt, shutdown, task) = node_full(&home, executors, vec![local]).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let (status, body) = ask(&base, "claude-x").await;
        assert_eq!(status, 401, "{body}");
        assert!(body.contains("token expired"), "{body}");
        let calls = calls.lock().unwrap().clone();
        assert!(calls.iter().all(|c| !c.starts_with("/token")), "no refresh: {calls:?}");
        assert_eq!(calls.len(), 1, "one execution, with the Home token: {calls:?}");
        assert!(calls[0].ends_with("sk-ant-oat01-FAKE"), "{calls:?}");
        shutdown.cancel();
        task.await.unwrap();
        let _ = std::fs::rename(
            &dir,
            std::env::temp_dir().join(format!("cpa-trash-home-401-{}", std::process::id())),
        );
    }

    /// Go `helps.RefreshAuthViaHome` behind Meta's request-time preparation: a Home pick
    /// holding only a DCA token is minted by Home (`GET {"type":"refresh",...}` with its
    /// auth index and token hash), never by this node at Meta, and the attempt runs on
    /// the key Home returns. A Home refresh failure is the attempt's error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn meta_mints_go_through_home() {
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let app = axum::Router::new().fallback({
            let calls = calls.clone();
            move |uri: axum::http::Uri, headers: axum::http::HeaderMap| {
                let calls = calls.clone();
                async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    calls.lock().unwrap().push(format!("{} {auth}", uri.path()));
                    let completed = serde_json::json!({"type": "response.completed", "response": {
                        "id": "r1", "object": "response", "model": "muse", "status": "completed",
                        "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hi"}]}],
                        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}});
                    ([("content-type", "text/event-stream")], format!("data: {completed}\n\n"))
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let executors = |upstream: &str| cpa_exec::Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
            codex: Default::default(),
            devices: cpa_exec::DeviceExecutors {
                meta: cpa_exec::meta::MetaExecutor::default().with_mint_url(&format!("{upstream}/mint")),
                ..Default::default()
            },
            openai: Default::default(),
            google: Default::default(),
        };
        let pick = serde_json::json!({"model": "muse-up", "auth_index": "idx-meta",
            "auth": {"id": "meta-1", "provider": "meta", "attributes": {"base_url": upstream},
                     "metadata": {"type": "meta", "dca_token": "dca:fake-dca"}}})
        .to_string();
        for refreshed in [
            serde_json::json!({"auth": {"id": "meta-1", "provider": "meta",
                "metadata": {"type": "meta", "api_key": "sk-meta-home-fake", "dca_token": "dca:fake-dca"}},
                "auth_index": "idx-meta"}),
            serde_json::json!({"error": {"type": "authentication_error", "message": "dca revoked"}}),
        ] {
            calls.lock().unwrap().clear();
            let pick = pick.clone();
            let minted = refreshed.to_string().contains("sk-meta-home-fake");
            let refreshed = refreshed.to_string();
            let home = FakeHome::start(move |args| match args[0].to_lowercase().as_str() {
                "get" if args[1].starts_with('{') => fake::bulk(&refreshed),
                "get" => fake::bulk("port: 0\nrequest-retry: 0\n"),
                "subscribe" => fake::raw(ACK),
                "ping" => fake::raw("+PONG\r\n"),
                "rpop" => fake::bulk(&pick),
                "lpush" => fake::raw(":1\r\n"),
                _ => fake::raw("-ERR unexpected\r\n"),
            })
            .await;
            let (base, rt, shutdown, task) = node_full(&home, executors(&upstream), vec![]).await;
            eventually("dispatch published", || {
                rt.remote_dispatch().is_some_and(|d| d.available())
            })
            .await;
            let (status, _, body) = chat(&base, false).await;
            let refreshes: Vec<Value> = home
                .commands()
                .iter()
                .filter(|c| c[0].eq_ignore_ascii_case("get") && c[1].starts_with('{'))
                .map(|c| serde_json::from_str(&c[1]).unwrap())
                .collect();
            assert!(!refreshes.is_empty(), "Home was asked to refresh: {status} {body}");
            assert_eq!(
                (refreshes[0]["type"].as_str(), refreshes[0]["auth_index"].as_str()),
                (Some("refresh"), Some("idx-meta"))
            );
            // The DCA token is not an access token: Go sends no token hash.
            assert!(refreshes[0].get("access_token_sha256").is_none(), "{}", refreshes[0]);
            let calls = calls.lock().unwrap().clone();
            assert!(
                calls.iter().all(|c| !c.starts_with("/mint")),
                "no local mint: {calls:?}"
            );
            if minted {
                assert_eq!(status, 200, "{body}");
                assert_eq!(calls, vec!["/responses Bearer sk-meta-home-fake".to_owned()]);
            } else {
                assert_eq!(status, 401, "{body}");
                assert!(body.contains("credential unauthorized"), "{body}");
                assert!(!body.contains("dca revoked"), "Home's message stays private: {body}");
                assert!(calls.is_empty(), "nothing ran upstream: {calls:?}");
            }
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    // ---- Go home_v8_model_capabilities_test.go and friends: Home's model definition --

    /// The credential a Home dispatch reply makes for `requested` (the route model).
    fn home_credential(reply: Value, requested: &str) -> Credential {
        let response = DispatchResponse::parse(reply.to_string().as_bytes()).unwrap();
        credential(&response, requested).unwrap()
    }

    /// Go `TestHomeV8ModelCapabilitiesAndLegacyPayload` and
    /// `TestHomeDispatchWebSearchCapability`: Home's definition becomes the attempt's
    /// model, keeping its context length, thinking, user-defined flag and native web
    /// search (present or not), dropping fields Go's struct lacks; is-compat comes only
    /// from the credential's options.
    #[test]
    fn home_model_info_becomes_the_resolved_model() {
        use cpa_core::exec::ResolvedSource;
        let thinking = serde_json::json!({"levels": ["high", "none"], "zero_allowed": true});
        let cases = [
            (
                serde_json::json!({"id": "upstream", "context_length": 32768, "thinking": thinking, "user_defined": false}),
                32768,
                true,
            ),
            (
                serde_json::json!({"id": "upstream", "context_length": 32768, "max_context_length": 32768, "is_compat": false, "thinking": thinking, "user_defined": false}),
                32768,
                true,
            ),
            (
                serde_json::json!({"id": "upstream", "context_length": 16384, "user_defined": true}),
                16384,
                false,
            ),
        ];
        for (info, context, compat) in cases {
            let mut auth = serde_json::json!({"id": "c1", "provider": "codex"});
            if compat {
                auth["metadata"] = serde_json::json!({"credential_options": {"models": [{"name": "upstream", "alias": "alias", "is-compat": true}]}});
            }
            let c = home_credential(serde_json::json!({"model_info": info, "auth": auth}), "alias");
            let bound = cpa_server::capabilities::bind_home(&c, None).unwrap();
            assert_eq!(
                (bound.source, bound.info.id.as_str()),
                (ResolvedSource::Home, "upstream")
            );
            assert!(!bound.info.raw.contains_key("max_context_length"), "{info}");
            assert_eq!(bound.is_compat(), compat, "{info}");
            assert_eq!(bound.info.raw["context_length"], context);
            let user_defined = bound.info.raw.get("user_defined") == Some(&Value::Bool(true));
            assert_eq!(user_defined, info["user_defined"] == true);
            if compat {
                let thinking = bound.info.thinking.as_ref().unwrap();
                assert!(thinking.zero_allowed && thinking.levels.len() == 2, "{thinking:?}");
            }
        }
        for (info, present, enabled) in [
            (
                serde_json::json!({"id": "upstream", "native_capabilities": {"web_search": true}}),
                true,
                true,
            ),
            (
                serde_json::json!({"id": "upstream", "native_capabilities": {"web_search": false}}),
                true,
                false,
            ),
            (serde_json::json!({"id": "upstream"}), false, false),
        ] {
            let c = home_credential(
                serde_json::json!({"model_info": info, "auth": {"id": "c1", "provider": "codex"}}),
                "alias",
            );
            let bound = cpa_server::capabilities::bind_home(&c, None).unwrap();
            let web_search = bound
                .info
                .raw
                .get("native_capabilities")
                .and_then(|n| n.get("web_search"));
            assert_eq!(web_search.is_some(), present, "{info}");
            if present {
                assert_eq!(web_search, Some(&Value::Bool(enabled)));
            }
        }
        // Go decodes nulls as zero values and keeps the definition: unset fields, an
        // empty level, and Home's own configuration-update and is-compat flags.
        let info = serde_json::json!({
            "id": "upstream", "support_configuration_update": true,
            "thinking": {"levels": ["high", null], "max": null, "zero_allowed": true},
            "native_capabilities": {"web_search": null},
        });
        let auth = serde_json::json!({"id": "c1", "provider": "codex", "metadata": {"credential_options": {"models": [{"name": "upstream", "alias": "alias", "is-compat": true}]}}});
        let c = home_credential(serde_json::json!({"model_info": info, "auth": auth}), "alias");
        let bound = cpa_server::capabilities::bind_home(&c, None).expect("Home's definition binds");
        let thinking = bound.info.thinking.as_ref().unwrap();
        assert_eq!(
            (thinking.levels.clone(), thinking.max, thinking.zero_allowed),
            (strings(&["high", ""]), 0, true)
        );
        assert!(bound.is_compat());
        assert_eq!(bound.info.raw["support_configuration_update"], true);
        assert!(bound.info.raw["native_capabilities"].get("web_search").is_none());
        // No usable definition: the local binding stays.
        for info in [serde_json::json!({"id": " "}), Value::Null] {
            let c = home_credential(
                serde_json::json!({"model_info": info, "auth": {"id": "c1", "provider": "codex"}}),
                "alias",
            );
            assert!(cpa_server::capabilities::bind_home(&c, None).is_none(), "{info}");
        }
        // Go decodes `model_info` into its typed struct: a mistyped field fails the reply.
        for info in [
            serde_json::json!({"id": 5}),
            serde_json::json!({"id": "m", "context_length": 1.5}),
            serde_json::json!({"id": "m", "native_capabilities": {"web_search": "yes"}}),
            serde_json::json!("m"),
        ] {
            let reply = serde_json::json!({"model_info": info, "auth": {"id": "c1", "provider": "codex"}});
            assert!(DispatchResponse::parse(reply.to_string().as_bytes()).is_err(), "{info}");
        }
    }

    /// Go `TestHomeCompatUsesCredentialModelOptions`: is-compat comes from the
    /// `credential_options` entry for the dispatched upstream model reached through the
    /// route model (upstream names before aliases, exact before suffix-free), never from
    /// the local binding, whatever the options hold.
    #[test]
    fn home_is_compat_comes_from_the_credential_options() {
        // Options, dispatched upstream, local model, route, want.
        let cases: [(&str, &str, &str, &str, bool); 16] = [
            (r#"{"models":[{"name":"model","is-compat":true}]}"#, "", "", "", true),
            (r#"{"models":[{"name":"model","is-compat":false}]}"#, "", "", "", false),
            (r#"{"models":[{"name":"model"}]}"#, "", "", "", false),
            (r#"{"models":null}"#, "", "", "", false),
            (r#"{"models":[]}"#, "", "", "", false),
            (r#"{"models":[{"name":"other","is-compat":true}]}"#, "", "", "", false),
            ("", "", "", "", false),
            (r#"{"weight":2}"#, "", "", "", false),
            ("", "", "other", "", false),
            (r#"{"models":"invalid"}"#, "", "", "", false),
            (
                r#"{"models":[{"name":"other","alias":"model","is-compat":true},{"name":"model","is-compat":false}]}"#,
                "",
                "",
                "",
                false,
            ),
            (
                r#"{"models":[{"name":"upstream","alias":"model","is-compat":true}]}"#,
                "",
                "",
                "",
                true,
            ),
            (
                r#"{"models":[{"name":"model","is-compat":true}]}"#,
                "model(high)",
                "",
                "",
                true,
            ),
            (
                r#"{"models":[{"name":"model","is-compat":true},{"name":"model(high)","is-compat":false}]}"#,
                "model(high)",
                "",
                "",
                false,
            ),
            (
                r#"{"models":[{"name":"model","alias":"other","is-compat":false},{"name":"model","alias":"chosen","is-compat":true}]}"#,
                "model(high)",
                "",
                "tenant/chosen(high)",
                true,
            ),
            (
                r#"{"models":[{"name":"other","alias":"chosen","is-compat":false},{"name":"model","alias":"chosen","is-compat":true}]}"#,
                "",
                "",
                "tenant/chosen",
                true,
            ),
        ];
        for (options, upstream, local_model, route, want) in cases {
            let mut auth = serde_json::json!({"id": "c1", "provider": "codex", "prefix": "tenant"});
            if !options.is_empty() {
                auth["metadata"] =
                    serde_json::json!({"credential_options": serde_json::from_str::<Value>(options).unwrap()});
            }
            let reply = serde_json::json!({"model": upstream, "model_info": {"id": "model"}, "auth": auth});
            let c = home_credential(reply, route);
            let mut local = cpa_core::registry::ModelInfo {
                id: if local_model.is_empty() { "model" } else { local_model }.into(),
                kind: String::new(),
                thinking: None,
                raw: Map::new(),
            };
            local.raw.insert("is_compat".into(), true.into());
            let local = cpa_core::exec::ResolvedModel {
                info: local,
                source: cpa_core::exec::ResolvedSource::ApiKey,
            };
            for local in [Some(&local), None] {
                let bound = cpa_server::capabilities::bind_home(&c, local).unwrap();
                assert_eq!(
                    bound.is_compat(),
                    want,
                    "options {options}, upstream {upstream:?}, route {route:?}"
                );
            }
        }
    }

    /// Go `TestHomeDispatchConfigurationUpdateCapabilityAndLegacyFallback`, with the local
    /// binding the dispatch loop computes first (Go's fixture sets an unsanitized config;
    /// a loaded Codex key needs a base URL, so this one has one): Home's explicit flag wins either way;
    /// without one, a local binding of the same model lends its support (API key or
    /// Codex OAuth plan catalog), and an unknown or unbound model has none. On Go's
    /// stream path no local binding exists yet (probed against Go: execute and count
    /// keep the legacy fallback, stream does not).
    #[test]
    fn configuration_update_support_follows_home_then_the_same_local_model() {
        const MODEL: &str = "gpt-6-luna";
        let local_config = format!(
            "codex-api-key:\n  - api-key: home-update-key\n    base-url: http://127.0.0.1:9\n    prefix: tenant\n    models:\n      - name: {MODEL}\n        support-configuration-update: true\n"
        );
        // Home's flag, local support configured, OAuth credential, Home's model ID, want.
        let cases = [
            (r#","support_configuration_update":true"#, false, false, MODEL, true),
            (r#","support_configuration_update":false"#, true, false, MODEL, false),
            ("", true, false, MODEL, true),
            ("", false, true, MODEL, true),
            ("", true, false, "unknown-home-model", false),
            ("", false, false, MODEL, false),
        ];
        for (flag, local_support, oauth, info_id, want) in cases {
            let cfg = cpa_core::config::Config::parse(if local_support { &local_config } else { "" }).unwrap();
            let auth = if oauth {
                serde_json::json!({"id": "c1", "provider": "codex", "prefix": "tenant",
                    "attributes": {"auth_kind": "oauth", "plan_type": "free"}, "metadata": {"access_token": "fake-token"}})
            } else {
                serde_json::json!({"id": "c1", "provider": "codex", "prefix": "tenant",
                    "attributes": {"auth_kind": "apikey", "api_key": "home-update-key", "source": "config:codex[0]"}})
            };
            let info: Value = serde_json::from_str(&format!(r#"{{"id":"{info_id}"{flag}}}"#)).unwrap();
            let route = if local_support {
                format!("tenant/{MODEL}")
            } else {
                MODEL.to_owned()
            };
            let c = home_credential(
                serde_json::json!({"model": MODEL, "model_info": info, "auth": auth}),
                &route,
            );
            let local = cpa_server::capabilities::resolve(&cfg, &c, &route, MODEL);
            let support = |bound: cpa_core::exec::ResolvedModel| {
                cpa_common::thinking::ModelCaps::from(&bound.info).support_configuration_update
            };
            let execute = support(cpa_server::capabilities::bind_home(&c, local.as_ref()).unwrap());
            assert_eq!(
                execute, want,
                "flag {flag:?}, local {local_support}, oauth {oauth}, id {info_id}"
            );
            let stream = support(cpa_server::capabilities::bind_home(&c, None).unwrap());
            assert_eq!(stream, want && !flag.is_empty(), "stream: flag {flag:?}");
        }
    }

    /// Go `TestHomeCompatCredentialOptionsExecutionPaths` and
    /// `TestHomeCompatDoesNotInheritLocalModelExecutionPaths`: whatever the provider, the
    /// options entry for the route's alias decides is-compat; options that list no
    /// models leave it false even when the local binding says compat.
    #[test]
    fn home_is_compat_ignores_the_local_binding_on_every_provider() {
        for provider in ["codex", "claude", "gemini", "custom-compat"] {
            for enabled in [true, false] {
                let options = serde_json::json!({"models": [
                    {"name": "upstream", "alias": "other", "is-compat": !enabled},
                    {"name": "upstream", "alias": "alias", "is-compat": enabled},
                ]});
                let reply = serde_json::json!({"model": "upstream(high)", "model_info": {"id": "upstream"},
                    "auth": {"id": "c1", "provider": provider, "prefix": "tenant",
                             "attributes": {"api_key": "fixture-key", "base_url": "http://127.0.0.1:9"},
                             "metadata": {"credential_options": options}}});
                let c = home_credential(reply, "tenant/alias(high)");
                let bound = cpa_server::capabilities::bind_home(&c, None).unwrap();
                assert_eq!(bound.is_compat(), enabled, "{provider}");
            }
        }
        let cfg = cpa_core::config::Config::parse(
            "codex-api-key:\n  - api-key: fixture-key\n    base-url: http://127.0.0.1:9\n    prefix: tenant\n    models:\n      - name: upstream\n        alias: alias\n        is-compat: true\n",
        )
        .unwrap();
        for options in [
            None,
            Some(serde_json::json!({"weight": 2})),
            Some(serde_json::json!({"models": "invalid"})),
        ] {
            let mut auth = serde_json::json!({"id": "c1", "provider": "codex", "prefix": "tenant",
                "attributes": {"auth_kind": "apikey", "api_key": "fixture-key", "source": "config:codex[0]"}});
            if let Some(options) = &options {
                auth["metadata"] = serde_json::json!({"credential_options": options});
            }
            let reply = serde_json::json!({"model": "upstream(high)", "model_info": {"id": "upstream"}, "auth": auth});
            let c = home_credential(reply, "tenant/alias(high)");
            let local = cpa_server::capabilities::resolve(&cfg, &c, "tenant/alias(high)", "upstream(high)");
            assert!(
                local.as_ref().is_some_and(|l| l.is_compat()),
                "the fixture binds local is-compat"
            );
            let bound = cpa_server::capabilities::bind_home(&c, local.as_ref()).unwrap();
            assert!(!bound.is_compat(), "{options:?}");
        }
    }

    /// End to end through a fake Home: the `credential_options` entry Home mode selects
    /// for the dispatched model reaches the OpenAI-compatible executor (Go
    /// `ResolvedHomeModelOptions` in `resolveCompatConfig`), whose
    /// `use-max-completion-tokens` rewrites the upstream body, and Home's model
    /// definition binds the attempt; another alias's entry does not leak in.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn credential_options_reach_the_executor_through_a_home_pick() {
        let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
        let app = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post({
                let bodies = bodies.clone();
                move |body: String| {
                    let bodies = bodies.clone();
                    async move {
                        bodies.lock().unwrap().push(serde_json::from_str(&body).unwrap());
                        axum::Json(serde_json::json!({
                            "id": "c1", "object": "chat.completion", "created": 1, "model": "upstream",
                            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                        }))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for (route, want) in [("alias", true), ("other", false)] {
            let reply = serde_json::json!({
                "model": "upstream", "model_info": {"id": "upstream", "user_defined": true},
                "auth": {"id": "c1", "provider": "openai-compatibility", "label": "contract",
                    "attributes": {"api_key": "sk-home-fake", "base_url": upstream, "compat_name": "contract"},
                    "metadata": {"credential_options": {"models": [
                        {"name": "upstream", "alias": "other", "use-max-completion-tokens": false},
                        {"name": "upstream", "alias": "alias", "use-max-completion-tokens": true}
                    ]}}},
            });
            let home = scripted("port: 0\nrequest-retry: 0\n", vec![reply.to_string()]).await;
            let (base, _rt, shutdown, task) = contract_node(&home).await;
            let body =
                serde_json::json!({"model": route, "max_tokens": 7, "messages": [{"role": "user", "content": "hi"}]});
            let (status, _, text) = ask_with_headers(&base, "/v1/chat/completions", body).await;
            assert_eq!(status, 200, "{text}");
            let sent = bodies.lock().unwrap().pop().unwrap();
            assert_eq!(sent.get("max_completion_tokens").is_some(), want, "{route}: {sent}");
            assert_eq!(sent.get("max_tokens").is_none(), want, "{route}: {sent}");
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    // ---- Go home_concurrency_test.go: single picks against a fixture Home ----------

    /// Go `newHomeSelectionTestManager`: a dispatcher with its own Home client and
    /// registry, outside any subscriber lifetime, recording each release as
    /// `credential/model` and its sequence.
    struct Picker {
        _rt: Arc<Runtime>,
        dispatcher: Dispatcher,
        client: Client,
        registry: Registry,
        releases: Arc<Mutex<Vec<(String, i64)>>>,
    }

    impl Picker {
        async fn new(home: &FakeHome) -> Self {
            let executors = cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            };
            let config = overlay(b"port: 0\n", None).unwrap();
            let rt = Arc::new(cpa_server::testing::runtime(config, vec![], executors));
            let dispatcher = Dispatcher::new(&rt);
            let client = Client::new(home.config());
            fake::set_heartbeat(&client, true);
            let registry = Registry::new();
            let releases: Arc<Mutex<Vec<(String, i64)>>> = Arc::default();
            let sink = releases.clone();
            registry.set_release_sink(Some(Arc::new(
                move |group: cpa_home::registry::ReleaseGroup, sequence| {
                    let group = format!("{}/{}", group.credential_id, group.model);
                    sink.lock().unwrap().push((group, sequence));
                    None
                },
            )));
            dispatcher.install(client.clone(), registry.clone());
            Self {
                _rt: rt,
                dispatcher,
                client,
                registry,
                releases,
            }
        }

        async fn pick(&self) -> Result<RemoteGrant, RemoteError> {
            let request = RemoteRequest {
                model: "gpt".into(),
                count: 1,
                kind: "http",
                ..Default::default()
            };
            self.dispatcher.pick(request).await
        }

        fn fenced(&self) -> bool {
            self.client.ambiguous_dispatch()
        }

        fn in_flight(&self) -> usize {
            self.registry.freeze().executions.len()
        }

        fn releases(&self) -> Vec<(String, i64)> {
            self.releases.lock().unwrap().clone()
        }
    }

    const TUPLE: &str = r#""concurrency":{"accounted":true,"credential_id":"cred-1","model":"gpt"}"#;

    /// Go `TestPickHomeDispatchSelectionReleasesAccountedScopeAfterAuthValidationFailure`,
    /// `…AfterPayloadDecodeFailure`, `…AfterAuthDecodeFailure`,
    /// `TestPickHomeDispatchSelectionRejectsMalformedErrorPresence`,
    /// `TestPickHomeDispatchSelectionFencesAccountedBusyError`,
    /// `TestMalformedAccountedTupleClosesHomeClient`,
    /// `TestHomeConcurrencyTupleAuthMismatchEndsScope` and
    /// `TestPickHomeDispatchSelectionFencesInvalidExplicitConcurrency`: every refused
    /// pick ends its scope; only a reply that leaves Home's accounting ambiguous fences
    /// the client.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refused_picks_end_their_scope_and_fence_only_ambiguous_replies() {
        let tupled = |rest: &str| format!("{{{TUPLE},{rest}}}");
        // The reply, Go's error code, and whether Home is fenced.
        let cases = [
            (tupled(r#""auth":{"id":"","provider":"codex"}"#), "invalid_auth", false),
            (tupled(r#""model":123,"auth":{"id":"cred-1","provider":"codex"}"#), "invalid_auth", false),
            (tupled(r#""auth":"invalid""#), "invalid_auth", false),
            (
                tupled(r#""auth_index":"other","auth":{"id":"cred-1","provider":"codex"}"#),
                "invalid_home_concurrency",
                false,
            ),
            (r#"{"error":"busy","auth":{"id":"cred-1","provider":"codex"}}"#.into(), "invalid_auth", false),
            (r#"{"error":{},"auth":{"id":"cred-1","provider":"codex"}}"#.into(), "invalid_auth", false),
            (r#"{"error":null,"auth":{"id":"cred-1","provider":"codex"}}"#.into(), "invalid_auth", false),
            (
                r#"{"error":{"type":" ","code":""},"auth":{"id":"cred-1","provider":"codex"}}"#.into(),
                "invalid_auth",
                false,
            ),
            (tupled(r#""error":"busy","auth":{"id":"cred-1","provider":"codex"}"#), "invalid_home_concurrency", true),
            (tupled(r#""error":{},"auth":{"id":"cred-1","provider":"codex"}"#), "invalid_home_concurrency", true),
            (tupled(r#""error":null,"auth":{"id":"cred-1","provider":"codex"}"#), "invalid_home_concurrency", true),
            (
                tupled(r#""error":{"type":"credential_concurrency_exceeded","message":"busy","retry_after_ms":750}"#),
                "invalid_home_concurrency",
                true,
            ),
            (
                r#"{"concurrency":{"accounted":true,"credential_id":"cred-1","model":""},"auth":{"id":"cred-1","provider":"codex"}}"#.into(),
                "invalid_home_concurrency",
                true,
            ),
            (
                r#"{"concurrency":{"accounted":false,"credential_id":"cred-1","model":"gpt"},"auth":{"id":"cred-1","provider":"codex"}}"#.into(),
                "invalid_home_concurrency",
                true,
            ),
            (
                r#"{"concurrency":{"accounted":true,"credential_id":" cred-1","model":"gpt"},"auth":{"id":"cred-1","provider":"codex"}}"#.into(),
                "invalid_home_concurrency",
                true,
            ),
            (
                r#"{"concurrency":{"accounted":true,"credential_id":"cred-1","model":"other"},"model":"gpt","auth":{"id":"cred-1","provider":"codex"}}"#.into(),
                "invalid_home_concurrency",
                true,
            ),
        ];
        for (reply, code, fenced) in cases {
            let home = sequenced("port: 0\n", vec![fake::bulk(&reply)], Default::default()).await;
            let picker = Picker::new(&home).await;
            let error = match picker.pick().await {
                Ok(_) => panic!("{reply} was accepted"),
                Err(error) => error,
            };
            assert_eq!(error.code, code, "{reply}");
            assert!(!matches!(error.kind, RemoteErrorKind::Busy { .. }), "{reply}");
            assert_eq!(picker.fenced(), fenced, "{reply}");
            assert_eq!(picker.in_flight(), 0, "the scope ended: {reply}");
        }
    }

    /// Go `TestPickHomeDispatchSelectionValidAccountedLocalValidationReleasesAndKeepsHomeHealthy`:
    /// an accounted pick the node refuses (an invalid auth, payload or identity) releases
    /// sequence 1 of its credential and model without fencing, and the next, valid pick
    /// releases sequence 2.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_accounted_pick_releases_and_keeps_home_healthy() {
        let valid = format!(r#"{{{TUPLE},"auth_index":"cred-1","auth":{{"id":"cred-1","provider":"codex"}}}}"#);
        for invalid in [
            r#""auth":{"id":"","provider":"codex"}"#,
            r#""model":123,"auth":{"id":"cred-1","provider":"codex"}"#,
            r#""auth":"invalid""#,
            r#""auth_index":"other","auth":{"id":"cred-1","provider":"codex"}"#,
        ] {
            let invalid = format!("{{{TUPLE},{invalid}}}");
            let home = sequenced(
                "port: 0\n",
                vec![fake::bulk(&invalid), fake::bulk(&valid)],
                Default::default(),
            )
            .await;
            let picker = Picker::new(&home).await;
            assert!(picker.pick().await.is_err(), "{invalid}");
            assert!(!picker.fenced(), "{invalid}");
            assert_eq!(picker.releases(), vec![("cred-1/gpt".to_owned(), 1)], "{invalid}");
            let grant = picker.pick().await.unwrap_or_else(|e| panic!("{invalid}: {e:?}"));
            assert!(!picker.fenced());
            (grant.end)();
            assert_eq!(
                picker.releases(),
                vec![("cred-1/gpt".to_owned(), 1), ("cred-1/gpt".to_owned(), 2)],
                "{invalid}"
            );
        }
    }

    /// Go `TestConcurrencyDispatchFixture` (accounted), `TestOldHomeDispatchIsUnaccounted`
    /// and `TestHomeInFlightObservationUsesFinalDispatchModel`: the shared fixture
    /// installs one accounted scope for its credential and model; a reply without a
    /// tuple installs an unaccounted one, observed under Home's upstream model rather
    /// than the requested one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn accounted_and_legacy_picks_install_their_scopes() {
        let fixture = include_str!("../../cpa-home/tests/fixtures/concurrency_dispatch_accounted.json");
        let legacy = r#"{"model":"final-upstream-model","auth":{"id":"cred-1","provider":"codex"}}"#;
        for (reply, accounted) in [(fixture, true), (legacy, false)] {
            let home = sequenced("port: 0\n", vec![fake::bulk(reply)], Default::default()).await;
            let picker = Picker::new(&home).await;
            let grant = picker.pick().await.unwrap_or_else(|e| panic!("{reply}: {e:?}"));
            assert_eq!(
                (grant.credential.id.as_str(), grant.credential.provider.as_str()),
                ("cred-1", "codex")
            );
            if accounted {
                let index = &grant.credential.attributes[cpa_core::config::credentials::HOME_AUTH_INDEX];
                assert_eq!(index, "cred-1");
            }
            let executions = picker.registry.freeze().executions;
            assert_eq!(executions.len(), 1, "{reply}");
            let scope = &executions[0];
            assert_eq!((scope.credential_id.as_str(), scope.accounted), ("cred-1", accounted));
            let model = if accounted { "gpt" } else { "final-upstream-model" };
            assert_eq!(scope.model, model, "the requested model was gpt");
            (grant.end)();
            assert_eq!(picker.in_flight(), 0);
        }
    }

    /// Go `TestManagerHomeDispatchBundleCompareAndClearDoesNotRemoveReplacement` and
    /// `TestPickHomeDispatchSelectionDoesNotMixDetachedBundleWithReplacement`: clearing
    /// removes only the lifetime that installed the bundle, and a pick whose registry
    /// already drained fails as `home_unavailable` without asking Home.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bundles_clear_only_their_own_lifetime_and_drained_picks_never_dispatch() {
        let home = sequenced("port: 0\n", vec![], Default::default()).await;
        let picker = Picker::new(&home).await;
        let replacement = Client::new(home.config());
        picker.dispatcher.clear(&replacement);
        assert!(
            picker.dispatcher.current().is_some(),
            "another lifetime's clear is a no-op"
        );
        picker.dispatcher.clear(&picker.client);
        assert!(picker.dispatcher.current().is_none());

        picker
            .dispatcher
            .install(picker.client.clone(), picker.registry.clone());
        picker.registry.drain(Duration::from_secs(1)).await.unwrap();
        let error = match picker.pick().await {
            Ok(_) => panic!("a drained registry dispatched"),
            Err(error) => error,
        };
        assert_eq!(error.code, "home_unavailable");
        assert!(rpops(&home).is_empty(), "Home was never asked");
    }

    /// Go `TestHomeBusySkipsNormalAndStreamOuterRetries`: Home's busy answer ends the
    /// request at once, streamed or not, even with request-retry left and a retry hint
    /// within max-retry-interval.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn home_busy_skips_outer_retries() {
        for stream in [false, true] {
            let busy = r#"{"error":{"type":"credential_concurrency_exceeded","message":"busy","retryable":true,"retry_after_ms":20000}}"#;
            let home = FakeHome::start(move |args| match args[0].to_lowercase().as_str() {
                "get" => fake::bulk("port: 0\nrequest-retry: 1\nmax-retry-interval: 30\n"),
                "subscribe" => fake::raw(ACK),
                "ping" => fake::raw("+PONG\r\n"),
                "rpop" => fake::bulk(busy),
                "lpush" => fake::raw(":1\r\n"),
                _ => fake::raw("-ERR unexpected\r\n"),
            })
            .await;
            let (base, _rt, shutdown, task) = contract_node(&home).await;
            let started = std::time::Instant::now();
            let (status, retry_after, body) = chat(&base, stream).await;
            assert!(started.elapsed() < Duration::from_secs(5), "busy waited for its hint");
            assert_eq!(
                (status, retry_after.as_deref()),
                (429, Some("20")),
                "stream {stream}: {body}"
            );
            assert!(body.contains("credential_concurrency_exceeded"), "{body}");
            assert_eq!(rpops(&home).len(), 1, "stream {stream}");
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    // ---- Go home_execution_paths_test.go: lease lifetimes on the execution paths ------

    /// An OpenAI-compatible upstream whose streams send one chunk and then wait for
    /// `close` (a `Notify` permit per stream); buffered requests answer at once.
    async fn held_stream_upstream(close: Arc<tokio::sync::Notify>) -> String {
        let app = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(move |body: String| {
                let close = close.clone();
                async move {
                    use axum::response::IntoResponse;
                    use futures_util::StreamExt;
                    let stream = serde_json::from_str::<Value>(&body).unwrap()["stream"] == true;
                    if !stream {
                        return axum::Json(serde_json::json!({
                            "id": "c1", "object": "chat.completion", "created": 1, "model": "m",
                            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                        }))
                        .into_response();
                    }
                    let chunk = |content: &str| {
                        format!(
                            "data: {}\n\n",
                            serde_json::json!({"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "m",
                                "choices": [{"index": 0, "delta": {"content": content}}]})
                        )
                    };
                    let first = chunk("initial");
                    let body = futures_util::stream::once(async move { Ok::<_, std::io::Error>(first) }).chain(
                        futures_util::stream::once(async move {
                            close.notified().await;
                            Ok("data: [DONE]\n\n".to_owned())
                        }),
                    );
                    ([("content-type", "text/event-stream")], axum::body::Body::from_stream(body)).into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    /// An accounted pick of `id` on an OpenAI-compatible credential at `upstream`.
    fn accounted_compat(id: &str, upstream: &str) -> String {
        serde_json::json!({
            "model": "m", "auth_index": id,
            "concurrency": {"accounted": true, "credential_id": id, "model": "m"},
            "auth": {"id": id, "provider": "openai-compatibility", "label": "pool",
                     "attributes": {"api_key": "sk-home-fake", "base_url": upstream, "compat_name": "pool"}},
        })
        .to_string()
    }

    /// Go `TestAccountedHomeStreamEndsOnlyAfterSourceTerminates`,
    /// `TestHomeStreamEndsOnTerminalChunk` and
    /// `TestAccountedHomeStreamConsumerCancellationEndsSelection`
    /// (`TestHomeStreamConsumerCancelEndsSelection`): a streamed lease is released only
    /// once its upstream stream ends, or once the client goes away, never at the first
    /// chunk.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streamed_leases_release_when_the_stream_ends_or_the_client_leaves() {
        use futures_util::StreamExt;
        for client_leaves in [false, true] {
            let close = Arc::new(tokio::sync::Notify::new());
            let upstream = held_stream_upstream(close.clone()).await;
            let home = scripted(
                "port: 0\nrequest-retry: 0\n",
                vec![accounted_compat("cred-1", &upstream)],
            )
            .await;
            let (base, _rt, shutdown, task) = contract_node(&home).await;
            let response = wreq::Client::new()
                .post(format!("{base}/v1/chat/completions"))
                .header("authorization", "Bearer client-key")
                .json(
                    &serde_json::json!({"model": "m", "stream": true, "messages": [{"role": "user", "content": "hi"}]}),
                )
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            let mut body = response.bytes_stream();
            let first = body.next().await.unwrap().unwrap();
            assert!(String::from_utf8_lossy(&first).contains("initial"));
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(releases(&home).is_empty(), "no release while the stream is open");
            if client_leaves {
                drop(body);
            } else {
                close.notify_one();
                while body.next().await.is_some() {}
            }
            eventually("the release", || releases(&home).len() == 1).await;
            assert_eq!(releases(&home)[0]["credential_id"], "cred-1");
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    /// Go `TestHomeSelectionEndsOnMissingExecutor`, `TestAccountedHomeExecuteAndCountReleaseOnce`
    /// and `TestAccountedHomeRetrySelectsAndReleasesEveryAttempt`: a pick no executor
    /// serves is refused and its accounted lease released; a buffered request that fails
    /// over releases each credential exactly once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_pick_releases_exactly_once() {
        let missing = serde_json::json!({
            "model": "m", "auth_index": "cred-x",
            "concurrency": {"accounted": true, "credential_id": "cred-x", "model": "m"},
            "auth": {"id": "cred-x", "provider": "mystery"},
        })
        .to_string();
        let home = scripted("port: 0\nrequest-retry: 0\n", vec![missing]).await;
        let (base, _rt, shutdown, task) = contract_node(&home).await;
        let (status, _, body) = chat(&base, false).await;
        assert!(body.contains("executor_not_found"), "{status} {body}");
        eventually("the refused pick's release", || releases(&home).len() == 1).await;
        shutdown.cancel();
        task.await.unwrap();

        let upstream = ContractUpstream::start(vec![("sk-bad", bad_gateway())]).await;
        // The first credential's key fails upstream; the second one's succeeds.
        let first = accounted_compat("cred-1", &upstream.base).replace("sk-home-fake", "sk-bad");
        let home = scripted(
            "port: 0\nrequest-retry: 0\nmax-retry-credentials: 2\n",
            vec![first, accounted_compat("cred-2", &upstream.base)],
        )
        .await;
        let (base, _rt, shutdown, task) = contract_node(&home).await;
        let (status, _, body) = chat(&base, false).await;
        assert_eq!(status, 200, "{body}");
        eventually("both releases", || releases(&home).len() == 2).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let released: Vec<(String, i64)> = releases(&home)
            .iter()
            .map(|f| {
                (
                    f["credential_id"].as_str().unwrap().to_owned(),
                    f["release_seq"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(released, vec![("cred-1".to_owned(), 1), ("cred-2".to_owned(), 1)]);
        assert_eq!(upstream.calls(), strings(&["sk-bad", "sk-home-fake"]));
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go keeps the last upstream error over a later credential's failed preparation
    /// (`prepareHomeRequestAuth` only updates lastErr): the client sees the upstream's
    /// answer, not the preparation error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_preparation_does_not_replace_the_upstream_error() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let upstream = upstream(seen.clone()).await;
        let oauth = format!(
            r#"{{"model":"claude-up","auth_index":"idx-broken","auth":{{"id":"cred-oauth","provider":"claude","attributes":{{"base_url":"{upstream}"}},"metadata":{{"type":"claude","access_token":"sk-ant-oat01-FAKE","account_uuid":"8f14e45f-ceea-467f-a8a1-0c3b9e1f3a77"}}}}}}"#
        );
        // An undecodable stored pool: Go's preparation fails reading it back.
        let pool_key = format!(
            "cpa:claude:credential-device-pool:{}",
            cpa_home::kv::hash_key_part("idx-broken")
        );
        let home = scripted_with_kv(
            "port: 0\n",
            vec![accounted("cred-bad", "sk-bad", &upstream), oauth],
            [(pool_key, r#"{"not":"a list"}"#.to_owned())].into_iter().collect(),
        )
        .await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let (status, body) = ask(&base, "claude-x").await;
        assert_eq!(rpops(&home).len(), 3, "both credentials were tried");
        assert_eq!(status, 500, "{body}");
        assert!(
            body.contains("boom"),
            "the upstream's error, not the preparation's: {body}"
        );
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go reads the query credential from the request for every Home pick: after a
    /// keep-alive committed the response, the redispatch still identifies the client.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn query_key_clients_keep_their_identity_past_a_keepalive() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let upstream = upstream(seen.clone()).await;
        let home = scripted(
            "port: 0\nrequests:\n  nonstream-keepalive-interval: 1\n",
            vec![
                accounted("cred-slow", "sk-slow-bad", &upstream),
                accounted("cred-good", "sk-good", &upstream),
            ],
        )
        .await;
        let (base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        let response = wreq::Client::new()
            .post(format!("{base}/v1/messages?key=query-client-key"))
            .json(&serde_json::json!({"model": "claude-x", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap();
        let body = response.text().await.unwrap();
        assert!(body.starts_with('\n'), "a keep-alive committed the response: {body:?}");
        let picks = rpops(&home);
        assert_eq!(picks.len(), 2, "{body}");
        for pick in &picks {
            assert_eq!(pick["headers"]["x-goog-api-key"], "query-client-key", "{pick}");
        }
        shutdown.cancel();
        task.await.unwrap();
    }

    fn app_logs(home: &FakeHome, marker: &str) -> Vec<Value> {
        home.commands()
            .into_iter()
            .filter(|c| c[0].eq_ignore_ascii_case("rpush") && c[1] == "app-log" && c[2].contains(marker))
            .map(|c| serde_json::from_str(&c[2]).unwrap())
            .collect()
    }

    /// Go `HomeAppLogForwarder`: process-log lines reach Home once a lifetime is
    /// published, and stop when the subscriber shuts down, which removes its one
    /// process-logger hook (Go `TestHomeAppLogForwarder_StopUnregistersMuxTarget`; the
    /// subscriber registers a single hook, Go
    /// `TestHomeAppLogForwardersUseOneProcessWideMuxHook`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn process_log_lines_reach_home_while_published() {
        // The real process logger (Go's format on stdout); idempotent across tests.
        cpa_server::logging::init();
        let home = scripted("port: 0\n", vec![]).await;
        let (_base, rt, shutdown, task) = node(&home).await;
        eventually("dispatch published", || {
            rt.remote_dispatch().is_some_and(|d| d.available())
        })
        .await;
        tracing::warn!(request_id = "req-applog-123456", "applog-marker-published");
        eventually("app log pushed", || {
            !app_logs(&home, "applog-marker-published").is_empty()
        })
        .await;
        let pushed = app_logs(&home, "applog-marker-published").remove(0);
        assert_eq!(
            (pushed["level"].as_str(), pushed["request_id"].as_str()),
            (Some("warning"), Some("req-applog-123456"))
        );
        let line = pushed["line"].as_str().unwrap();
        // Go `ShortRequestID`: the last 8 bytes.
        assert!(line.contains("] [g-123456] [warn ] ["), "{line}");
        assert!(line.ends_with("applog-marker-published\n"), "{line}");
        let timestamp = pushed["timestamp"].as_str().unwrap();
        assert!(timestamp.len() >= 20 && timestamp.as_bytes()[10] == b'T', "{pushed}");

        shutdown.cancel();
        task.await.unwrap();
        tracing::warn!("applog-marker-stopped");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(app_logs(&home, "applog-marker-stopped").is_empty());
    }

    // ---- Go service_executionregistry_test.go: the registry across lifetimes ----------

    /// Go `serveRegistryTestHomeConnection`'s Home: lifecycle revision 1 with a short CPA
    /// heartbeat and cancel bound. The first subscription is ACKed at once; later ones
    /// hang (the subscriber retries them) until `allow_second` is set. Pongs keep every
    /// subscription alive while `beating` is set, so clearing it loses the heartbeat.
    struct LifetimeHome {
        home: Arc<FakeHome>,
        subscriptions: Arc<std::sync::atomic::AtomicUsize>,
        allow_second: Arc<std::sync::atomic::AtomicBool>,
        beating: Arc<std::sync::atomic::AtomicBool>,
    }

    impl LifetimeHome {
        // ponytail: Go's heartbeat is 100ms; 300ms leaves the 50ms pongs room on a loaded
        // test machine without changing what is asserted.
        const CONFIG: &str = "credential-concurrency:\n  lifecycle-config-revision: 1\n  cpa-heartbeat-timeout: 300ms\n  cpa-cancel-bound: 100ms\n";

        async fn start() -> Self {
            let subscriptions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let allow_second = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let beating = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let home = Arc::new(
                FakeHome::start({
                    let (subscriptions, allow_second) = (subscriptions.clone(), allow_second.clone());
                    move |args| match args[0].to_lowercase().as_str() {
                        "get" if args[1] == "config" => fake::bulk(Self::CONFIG),
                        "subscribe" => {
                            let n = subscriptions.fetch_add(1, Ordering::SeqCst) + 1;
                            if n == 1 || allow_second.load(Ordering::SeqCst) {
                                fake::subscribe_ack()
                            } else {
                                Reply::Hang
                            }
                        }
                        "ping" => fake::raw("+PONG\r\n"),
                        "lpush" | "rpush" => fake::raw(":1\r\n"),
                        _ => fake::raw("+OK\r\n"),
                    }
                })
                .await,
            );
            tokio::spawn({
                let (home, beating) = (Arc::downgrade(&home), beating.clone());
                async move {
                    while let Some(home) = home.upgrade() {
                        if beating.load(Ordering::SeqCst) {
                            home.push(fake::pong());
                        }
                        drop(home);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            });
            Self {
                home,
                subscriptions,
                allow_second,
                beating,
            }
        }

        fn subscriptions(&self) -> usize {
            self.subscriptions.load(Ordering::SeqCst)
        }
    }

    /// Go `startHomeSubscriber` on a bare service: the subscriber alone, with the
    /// dispatcher it publishes each lifetime to.
    struct Bare {
        _rt: Arc<Runtime>,
        dispatcher: Arc<Dispatcher>,
        shutdown: CancellationToken,
        task: tokio::task::JoinHandle<()>,
        _guard: tokio::sync::MutexGuard<'static, ()>,
    }

    impl Bare {
        async fn start(home: &FakeHome) -> Self {
            let guard = ONE_NODE.lock().await;
            let rt = test_runtime();
            let dispatcher = Arc::new(Dispatcher::new(&rt));
            let shutdown = CancellationToken::new();
            let task = spawn_subscriber(home.config(), rt.clone(), dispatcher.clone(), shutdown.clone());
            Self {
                _rt: rt,
                dispatcher,
                shutdown,
                task,
                _guard: guard,
            }
        }

        /// The bundle the published lifetime exposes, once there is one.
        async fn published(&self) -> Bundle {
            eventually("a published Home lifetime", || self.dispatcher.current().is_some()).await;
            self.dispatcher.current().unwrap()
        }

        async fn stop(self) {
            self.shutdown.cancel();
            self.task.await.unwrap();
        }
    }

    /// Go `TestServiceKeepsRegistryAcrossHeartbeatFailoverAndExposesOnlyAfterNewACK`: a
    /// heartbeat loss unpublishes the lifetime (no bundle, no current client) until the
    /// replacement subscription is ACKed, and the replacement keeps the registry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_heartbeat_failover_keeps_the_registry_and_publishes_only_after_the_new_ack() {
        let home = LifetimeHome::start().await;
        let node = Bare::start(&home.home).await;
        let first = node.published().await;
        assert!(cpa_home::current().is_some_and(|c| c.ptr_eq(&first.client)));

        home.beating.store(false, Ordering::SeqCst);
        eventually("the replacement subscription", || home.subscriptions() >= 2).await;
        assert!(
            node.dispatcher.current().is_none() && cpa_home::current().is_none(),
            "the old lifetime stayed published before the replacement ACK"
        );

        home.allow_second.store(true, Ordering::SeqCst);
        home.beating.store(true, Ordering::SeqCst);
        let second = node.published().await;
        assert!(
            second.registry.ptr_eq(&first.registry),
            "heartbeat failover replaced the execution registry"
        );
        assert!(!second.client.ptr_eq(&first.client));
        assert!(cpa_home::current().is_some_and(|c| c.ptr_eq(&second.client)));
        node.stop().await;
    }

    /// Go `TestServiceAmbiguousDispatchDrainsRegistryBeforeRetry`: an ambiguous dispatch
    /// ends the lifetime, drains the registry (closing every bound resource) before the
    /// retry, publishes nothing until the new ACK, and then a fresh registry, while the
    /// subscriber keeps running.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_ambiguous_dispatch_drains_the_registry_before_the_retry() {
        let home = LifetimeHome::start().await;
        let node = Bare::start(&home.home).await;
        let first = node.published().await;
        let pending = first.registry.begin_dispatch().unwrap();
        let scope = first
            .registry
            .install(
                pending,
                // Go's `ScopeSpec{}`.
                cpa_home::registry::ScopeSpec {
                    request_id: String::new(),
                    credential_id: String::new(),
                    model: String::new(),
                    kind: String::new(),
                    started_at: std::time::SystemTime::UNIX_EPOCH,
                    accounted: false,
                },
            )
            .unwrap();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        scope
            .bind({
                let scope = scope.clone();
                move || {
                    let _ = closed_tx.send(());
                    // Go: `go scope.End("ambiguous dispatch")`.
                    std::thread::spawn(move || scope.end());
                }
            })
            .unwrap();
        drop(scope);

        first.client.abort_ambiguous_dispatch();
        tokio::time::timeout(Duration::from_secs(5), closed_rx)
            .await
            .expect("ambiguous dispatch did not drain the active registry")
            .unwrap();
        eventually("the retried subscription", || home.subscriptions() >= 2).await;
        assert!(
            node.dispatcher.current().is_none(),
            "the replacement registry was exposed before its ACK"
        );

        home.allow_second.store(true, Ordering::SeqCst);
        let next = node.published().await;
        assert!(
            !next.registry.ptr_eq(&first.registry),
            "ambiguous dispatch reused the drained execution registry"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!node.task.is_finished(), "recovery ended the subscriber");
        node.stop().await;
    }

    /// Go `TestServiceBacksOffAfterRepeatedPreAckFailures`: a lifetime that fails before
    /// its ACK is retried no sooner than Go's 100ms backoff (Go asserts at least 75ms),
    /// and not at all once the service is cancelled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pre_ack_failures_back_off_and_stop_on_shutdown() {
        let (attempt_tx, mut attempts) = tokio::sync::mpsc::unbounded_channel();
        let home = FakeHome::start(move |args| match args[0].to_lowercase().as_str() {
            "get" if args[1] == "config" => {
                let _ = attempt_tx.send(std::time::Instant::now());
                fake::raw("-ERR unavailable\r\n")
            }
            _ => fake::raw("+OK\r\n"),
        })
        .await;
        let node = Bare::start(&home).await;

        let first = attempts.recv().await.unwrap();
        let second = attempts.recv().await.unwrap();
        let delay = second - first;
        assert!(
            delay >= Duration::from_millis(75),
            "pre-ACK retry delay = {delay:?}, want at least 75ms"
        );
        node.shutdown.cancel();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            attempts.try_recv().is_err(),
            "pre-ACK retry continued after cancellation"
        );
        node.stop().await;
    }
}
