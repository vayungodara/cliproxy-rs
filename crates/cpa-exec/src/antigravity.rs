//! The Antigravity executor (internal/runtime/executor/antigravity_executor*.go).
//!
//! `antigravity` OAuth credentials call Google's Cloud Code `v1internal` API on the
//! daily endpoint (or the credential's `base_url`) over HTTP/1.1, wrapping Gemini
//! requests in the Antigravity envelope (`antigravity_request`). Gemini-family models
//! answer `generateContent`; Claude, Gemini 3 Pro and Gemini 3.1 Flash Image models are
//! always streamed and merged for non-stream clients. Responses compaction is served
//! locally from a summary turn (`antigravity_compaction`).
//!
//! Credential preparation (`antigravity_auth`) gives requests a project ID and an
//! access token valid for at least five more minutes, so the executor never refreshes
//! inline as Go's `ensureAccessToken` does.
//!
//! Gemini-family requests restore their thought signatures and native calls from the
//! reasoning replay ledger (`antigravity_replay`), and each finished answer updates it.
//! Registered credentials are probed for web-search models (`antigravity_models`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use cpa_common::json as gj;
use cpa_common::thinking::parse_suffix;
use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, Operation, ResponseBody};
use cpa_core::format::Format;
use cpa_translate::RequestCtx;
use futures_util::StreamExt;

use crate::antigravity_auth as auth;
use crate::antigravity_compaction as compaction;
use crate::antigravity_credits as credits;
use crate::antigravity_models as models;
use crate::antigravity_replay as replay;
use crate::antigravity_request as ag;
use crate::devin_wire::SensitiveWords;
use crate::gemini::{self, Output};
use crate::gemini_stream as sse;
use crate::payload_barrier::Barrier;
use crate::proxy::{self, GoClients, GoHeaders, Hooks, Proxy, Upstream};

pub const PROVIDER: &str = auth::PROVIDER;

/// `antigravityTransportCacheCapacity`: credential pools kept.
const POOL_CAPACITY: usize = 8192;
/// `antigravityDefaultIdleConnTimeout` and `antigravityMaxAllowedIdleConnTimeout`.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(210);
/// `antigravityDefaultMaxIdleConnsPerHost` and `antigravityMaxAllowedMaxIdleConnsPerHost`.
const DEFAULT_MAX_IDLE: usize = 2;
const MAX_MAX_IDLE: usize = 100;

/// `antigravityPoolSettings` from `oauth.providers.antigravity.connection-pool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PoolSettings {
    /// The default: each connection closes after its response.
    Short,
    /// Per-credential keep-alive pools.
    Pooled {
        idle_timeout: Duration,
        max_idle_per_host: usize,
    },
}

/// `resolveAntigravityPoolSettings`: pooling only when `enabled: true`; a non-positive
/// timeout or a negative idle count falls back to short mode, larger values are capped.
pub(crate) fn pool_settings(cfg: &Config) -> PoolSettings {
    let pool = cfg
        .document
        .get("oauth")
        .and_then(|o| o.get("providers"))
        .and_then(|p| p.get("antigravity"))
        .and_then(|a| a.get("connection-pool"));
    let field = |key: &str| pool.and_then(|p| p.get(key));
    if field("enabled").and_then(serde_yaml_ng::Value::as_bool) != Some(true) {
        return PoolSettings::Short;
    }
    let mut idle_timeout = DEFAULT_IDLE_TIMEOUT;
    let raw = match field("idle-conn-timeout") {
        Some(serde_yaml_ng::Value::String(s)) => s.trim().to_owned(),
        Some(serde_yaml_ng::Value::Number(n)) => n.to_string(),
        Some(serde_yaml_ng::Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    };
    if !raw.is_empty() {
        match cpa_core::config::parse_duration(&raw) {
            None => tracing::warn!(
                "antigravity executor: invalid idle-conn-timeout {raw:?}, using default {}",
                go_duration(DEFAULT_IDLE_TIMEOUT)
            ),
            Some(nanos) if nanos <= 0 => return PoolSettings::Short,
            Some(nanos) => idle_timeout = Duration::from_nanos(nanos as u64).min(MAX_IDLE_TIMEOUT),
        }
    }
    let mut max_idle_per_host = DEFAULT_MAX_IDLE;
    if let Some(value) = field("max-idle-conns-per-host").and_then(serde_yaml_ng::Value::as_i64) {
        if value < 0 {
            return PoolSettings::Short;
        }
        // Go treats 0 as DefaultMaxIdleConnsPerHost (2).
        max_idle_per_host = match value as usize {
            0 => DEFAULT_MAX_IDLE,
            n => n.min(MAX_MAX_IDLE),
        };
    }
    PoolSettings::Pooled {
        idle_timeout,
        max_idle_per_host,
    }
}

/// Executor-side state Go keeps process-wide.
pub struct AntigravityExecutor {
    hooks: Hooks,
    /// HTTP/1.1-only clients by effective proxy that keep no idle connections (Go's
    /// default short mode, where no connection outlives its request).
    clients: Arc<GoClients>,
    /// Pooled mode: one HTTP/1.1 pool per credential and settings, most recently used
    /// first, so OAuth identities never share a connection (`antigravityHTTP11Transport`).
    pools: Mutex<Vec<(String, PoolSettings, Arc<GoClients>)>>,
    /// Go's standard clients for grounding redirects (`NewProxyAwareHTTPClient`).
    plain: GoClients,
    /// Short quota cooldowns by `auth|model|sc` (`antigravityShortCooldownByAuth`).
    cooldowns: Mutex<HashMap<String, Instant>>,
    /// The sensitive-word matcher for the configured word list.
    matcher: Mutex<Option<(String, Option<Arc<SensitiveWords>>)>>,
    /// Capability probes of registered credentials (`asyncProbeAntigravityCapabilities`).
    probes: Arc<models::Probes>,
}

impl Default for AntigravityExecutor {
    fn default() -> Self {
        Self::with_hooks(Hooks::default())
    }
}

/// Go `time.Duration.String`.
fn go_duration(d: Duration) -> String {
    let nanos = d.as_nanos();
    let trim = |whole: u128, frac: u128, width: usize| {
        let frac = format!("{frac:0width$}");
        let frac = frac.trim_end_matches('0');
        if frac.is_empty() {
            whole.to_string()
        } else {
            format!("{whole}.{frac}")
        }
    };
    match nanos {
        0 => "0s".into(),
        n if n < 1_000 => format!("{n}ns"),
        n if n < 1_000_000 => format!("{}µs", trim(n / 1_000, n % 1_000, 3)),
        n if n < 1_000_000_000 => format!("{}ms", trim(n / 1_000_000, n % 1_000_000, 6)),
        n => {
            let secs = n / 1_000_000_000;
            let seconds = trim(secs % 60, n % 1_000_000_000, 9);
            match (secs / 3600, secs / 60 % 60) {
                (0, 0) => format!("{seconds}s"),
                (0, m) => format!("{m}m{seconds}s"),
                (h, m) => format!("{h}h{m}m{seconds}s"),
            }
        }
    }
}

/// Go `parseBoolAny` on `disable_cooling` / `disable-cooling` metadata.
fn disable_cooling_override(credential: &Credential) -> Option<bool> {
    ["disable_cooling", "disable-cooling"].iter().find_map(|key| {
        let value = credential.metadata.get(*key)?;
        value
            .as_bool()
            .or_else(|| match value.as_str()?.trim() {
                "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
                "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
                _ => None,
            })
            .or_else(|| value.as_f64().map(|n| n != 0.0))
    })
}

/// `antigravityCoolingDisabled` (QuotaCooldownDisabledForAuthWithConfig): Home owns
/// cooldowns, then the credential's override, then `disable-cooling`.
fn cooling_disabled(credential: &Credential, cfg: &Config) -> bool {
    let home = cfg
        .document
        .get("home")
        .and_then(|h| h.get("enabled"))
        .and_then(serde_yaml_ng::Value::as_bool)
        .unwrap_or(false);
    home || disable_cooling_override(credential).unwrap_or(cfg.routing.cooldown.disable_cooling)
}

/// `antigravityShortCooldownKey`: empty when the credential or model has no name.
fn cooldown_key(credential: &Credential, model: &str) -> Option<String> {
    let (id, model) = (credential.id.trim(), model.trim());
    (!id.is_empty() && !model.is_empty()).then(|| format!("{id}|{model}|sc"))
}

/// Go `statusErr{code, msg}` with the retry hint of a 429 answer
/// (`newAntigravityStatusErr`).
pub(crate) fn upstream_error(status: u16, body: Bytes) -> ExecError {
    let retry = (status == 429).then(|| auth::parse_retry_delay(&body)).flatten();
    let mut error = gemini::status_err(status, body);
    error.retry_after = retry;
    error
}

/// What one attempt sends: the translated request Go hands its response translator,
/// and the body actually posted.
/// Go's Antigravity payload finalizer: rules relative to the `request` envelope.
fn barrier<'a>(model: &'a str, original: &'a [u8]) -> Barrier<'a> {
    Barrier {
        executor: PROVIDER,
        model,
        protocol: Format::Antigravity.as_str(),
        root: "request",
        original,
    }
}

struct Prepared {
    translated: Vec<u8>,
    body: Vec<u8>,
    /// The payload-rule baseline: the translated original request.
    rule_original: Vec<u8>,
    /// The replay ledger entry the request read (`antigravityReasoningReplayScope`).
    replay: replay::Scope,
}

impl AntigravityExecutor {
    /// Tests pin Google hosts to local mocks.
    pub fn with_hooks(hooks: Hooks) -> Self {
        Self {
            clients: Arc::new(GoClients::new(hooks.clone()).http1_only().pool(0, None)),
            pools: Mutex::default(),
            plain: GoClients::new(hooks.clone()),
            hooks,
            cooldowns: Mutex::default(),
            matcher: Mutex::default(),
            probes: Arc::default(),
        }
    }

    /// `asyncProbeAntigravityCapabilities`: a fresh model registration starts the
    /// web-search capability probe of each new Antigravity credential revision.
    pub fn models_registered(&self, credentials: &[Arc<Credential>], cfg: &Config) {
        self.probes.registered(&self.plain, credentials, cfg);
    }

    /// A project ID missing, or an access token inside the 30-minute refresh lead, as
    /// of `now`.
    pub fn needs_prepare_at(&self, credential: &Credential, _cfg: &Config, now: chrono::DateTime<chrono::Utc>) -> bool {
        auth::needs_prepare(credential, now)
    }

    /// Requests wait for a project ID and a token valid five more minutes (Go's
    /// `ShouldPrepareRequestAuth` and `ensureAccessToken`).
    pub fn must_prepare(&self, credential: &Credential) -> bool {
        auth::must_prepare(credential)
    }

    /// Credential preparation, then Go's post-refresh credits lookup
    /// (`queueAntigravityCreditsRefresh` after `refreshToken`).
    pub async fn prepare(&self, credential: &Credential, cfg: &Config) -> Result<MetadataPatch, ExecError> {
        let patch = auth::prepare(&self.clients_for(credential, cfg), credential, cfg).await?;
        if let Some(token) = patch.set.get("access_token").and_then(serde_json::Value::as_str) {
            let mut current = credential.clone();
            for (key, value) in &patch.set {
                current.metadata.insert(key.clone(), value.clone());
            }
            // Go bounds this lookup by the refreshing caller's context; preparation has
            // none here, so it gets the credential acquisition bound.
            self.queue_credits_refresh(&current, cfg, token, auth::ACQUISITION_TIMEOUT);
        }
        Ok(patch)
    }

    /// `maybeRefreshAntigravityCreditsHint`: with credits on and cooldowns active, a
    /// credential whose credits are not yet known gets a lookup (5 s budget).
    fn maybe_refresh_credits(&self, credential: &Credential, cfg: &Config) {
        if !credits::enabled(cfg) || cooling_disabled(credential, cfg) {
            return;
        }
        if credits::hint(&credential.id).is_some_and(|h| h.known) {
            return;
        }
        let token = credential.str("access_token").map(str::trim).unwrap_or_default();
        self.queue_credits_refresh(credential, cfg, token, credits::REQUEST_REFRESH_TIMEOUT);
    }

    /// `queueAntigravityCreditsRefresh` and `updateAntigravityCreditsBalanceForTask`: a
    /// background `loadCodeAssist` whose `paidTier` credits become the hint. Every lookup
    /// ends within `budget`, so a stalled one never blocks the credential's later ones.
    fn queue_credits_refresh(&self, credential: &Credential, cfg: &Config, token: &str, budget: Duration) {
        let id = credential.id.trim().to_owned();
        let token = token.trim().to_owned();
        if id.is_empty() || token.is_empty() || !credits::begin_refresh(&id) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            credits::end_refresh(&id);
            return;
        };
        let client = self.client(credential, cfg);
        let url = format!(
            "{}/v1internal:loadCodeAssist",
            ag::load_code_assist_base_url(credential)
        );
        let user_agent = ag::user_agent(credential);
        runtime.spawn(async move {
            let lookup = async {
                let mut headers = GoHeaders::new();
                headers.set("Authorization", format!("Bearer {token}"));
                headers.set("Accept", "*/*");
                headers.set("Content-Type", "application/json");
                headers.set("User-Agent", user_agent);
                let body = Bytes::from_static(br#"{"metadata":{"ideType":"ANTIGRAVITY"}}"#);
                let upstream = proxy::send(&client, &url, headers, body, None).await.ok()?;
                let ok = (200..300).contains(&upstream.status);
                let data = proxy::read_all(upstream.body, usize::MAX, false).await.ok()?;
                ok.then(|| credits::hint_from_load_code_assist(&data)).flatten()
            };
            let hint = tokio::time::timeout(budget, lookup).await.ok().flatten();
            if let Some(hint) = hint {
                credits::set_hint(&id, hint);
            }
            credits::end_refresh(&id);
        });
    }

    /// `newAntigravityHTTPClient`: the credential's HTTP/1.1 clients for the configured
    /// pool settings.
    fn clients_for(&self, credential: &Credential, cfg: &Config) -> Arc<GoClients> {
        let settings = pool_settings(cfg);
        let PoolSettings::Pooled {
            idle_timeout,
            max_idle_per_host,
        } = settings
        else {
            return self.clients.clone();
        };
        let scope = credential.id.trim().to_owned();
        let mut pools = self.pools.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(i) = pools.iter().position(|(s, p, _)| *s == scope && *p == settings) {
            let entry = pools.remove(i);
            let clients = entry.2.clone();
            pools.insert(0, entry);
            return clients;
        }
        let clients = Arc::new(
            GoClients::new(self.hooks.clone())
                .http1_only()
                .pool(max_idle_per_host, Some(idle_timeout)),
        );
        pools.insert(0, (scope, settings, clients.clone()));
        pools.truncate(POOL_CAPACITY);
        clients
    }

    /// `closeAntigravityAuthIdleTransports`: the credential's pools are dropped, so no
    /// idle connection of theirs is reused.
    fn close_idle(&self, credential: &Credential) {
        let scope = credential.id.trim();
        self.pools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(s, ..)| s != scope);
    }

    fn client(&self, credential: &Credential, cfg: &Config) -> wreq::Client {
        self.clients_for(credential, cfg)
            .get(&Proxy::effective(credential, cfg))
    }

    /// `getSensitiveWordMatcher` for `antigravity.sensitive-words` (v8
    /// `oauth.providers.antigravity.sensitive-words`), compiled once per word list.
    fn matcher(&self, cfg: &Config) -> Option<Arc<SensitiveWords>> {
        let words: Vec<String> = cfg
            .document
            .get("oauth")
            .and_then(|o| o.get("providers"))
            .and_then(|p| p.get("antigravity"))
            .and_then(|a| a.get("sensitive-words"))
            .and_then(|w| w.as_sequence())
            .map(|seq| seq.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        if words.is_empty() {
            return None;
        }
        let key = words.join("\0");
        let mut cached = self.matcher.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((k, m)) = cached.as_ref()
            && *k == key
        {
            return m.clone();
        }
        let built = SensitiveWords::new(&words).map(Arc::new);
        *cached = Some((key, built.clone()));
        built
    }

    /// `antigravityIsInShortCooldownRequired`: the cooldown left for this credential and
    /// model; an expired entry is removed.
    fn cooldown_remaining(&self, credential: &Credential, model: &str) -> Option<Duration> {
        let key = cooldown_key(credential, model)?;
        let mut cooldowns = self.cooldowns.lock().unwrap_or_else(PoisonError::into_inner);
        let until = *cooldowns.get(&key)?;
        let remaining = until.checked_duration_since(Instant::now()).filter(|d| !d.is_zero());
        if remaining.is_none() {
            cooldowns.remove(&key);
        }
        remaining
    }

    /// The short-cooldown gate in front of every generate attempt: a 429 that tells
    /// the scheduler to switch credentials.
    fn check_cooldown(&self, credential: &Credential, cfg: &Config, model: &str) -> Result<(), ExecError> {
        // A credits attempt ignores the short cooldown (`antigravityShouldBypassShortCooldown`).
        if cooling_disabled(credential, cfg) || credits::requested(credential, cfg) {
            return Ok(());
        }
        match self.cooldown_remaining(credential, model) {
            Some(remaining) => {
                let mut error = gemini::status_err(
                    429,
                    format!("auth in short cooldown, {} remaining", go_duration(remaining)),
                );
                error.retry_after = Some(remaining);
                Err(error)
            }
            None => Ok(()),
        }
    }

    /// The 429 rules after an upstream rejection: a short rate limit cools this
    /// credential down for the model; a credits attempt refused for an empty balance
    /// marks the credential out of credits.
    fn observe_rejection(&self, credential: &Credential, cfg: &Config, model: &str, status: u16, body: &[u8]) {
        if status != 429 {
            return;
        }
        let (decision, retry) = ag::decide_429(body);
        if matches!(
            decision,
            ag::RateLimit::ShortCooldownSwitchAuth | ag::RateLimit::FullQuotaExhausted
        ) {
            self.close_idle(credential);
        }
        if decision == ag::RateLimit::FullQuotaExhausted
            && credits::requested(credential, cfg)
            && credits::balance_exhausted(body)
            && !cooling_disabled(credential, cfg)
        {
            credits::mark_exhausted(&credential.id);
        }
        if decision == ag::RateLimit::ShortCooldownSwitchAuth
            && let Some(retry) = retry.filter(|d| !d.is_zero())
            && !cooling_disabled(credential, cfg)
            && let Some(key) = cooldown_key(credential, model)
        {
            self.cooldowns
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key, Instant::now() + retry);
        }
    }

    pub async fn execute(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        if req.operation == Operation::CountTokens {
            return self.count_tokens(credential, req, cfg).await;
        }
        // Go rejects both of these before it creates the usage reporter.
        if req.stream && req.alt.as_deref() == Some(gemini::COMPACT_ALT) {
            req.usage.discard();
            return Err(gemini::status_err(
                400,
                "streaming not supported for /responses/compact",
            ));
        }
        let mut req = req;
        if compaction::has_item(&req.body) {
            let expanded = match compaction::expand(&req.body) {
                Ok(expanded) => expanded,
                Err(e) => {
                    req.usage.discard();
                    return Err(gemini::status_err(400, e));
                }
            };
            if !req.original_body.is_empty() {
                req.original_body =
                    Bytes::from(compaction::expand(&req.original_body).unwrap_or_else(|_| expanded.clone()));
            }
            req.body = Bytes::from(expanded);
        }
        let triggered = compaction::has_trigger(&req.body) || compaction::has_trigger(&req.original_body);
        if triggered || (!req.stream && req.alt.as_deref() == Some(gemini::COMPACT_ALT)) {
            return self.compact(credential, req, cfg).await;
        }
        let model = parse_suffix(&req.model).model_name;
        self.check_cooldown(credential, cfg, &model)?;
        self.maybe_refresh_credits(credential, cfg);
        if req.stream {
            self.stream(credential, req, cfg, &model).await
        } else if ag::merges_stream(&model) {
            self.merged(credential, req, cfg, &model).await
        } else {
            self.generate(credential, req, cfg, &model).await
        }
    }

    /// The request side shared by every generate attempt: signature cleanup of the
    /// original request (which also becomes the working payload), translation with the
    /// attempt's model info, thinking, sensitive words, Gemini signature
    /// cleanup, the replay-history checks and the boundary user turns.
    fn prepare_request(
        &self,
        credential: &Credential,
        req: &mut ExecRequest,
        cfg: &Config,
        model: &str,
        envelope_stream: bool,
    ) -> Result<Prepared, ExecError> {
        let (from, to) = (req.source_format, Format::Antigravity);
        let validated = ag::validate_request_signatures(model, from, gemini::original_request(req));
        req.body = Bytes::from(validated);
        let resolved = gemini::resolved(req);
        // Go translates the original request once: it is both the payload-rule baseline
        // and, after thinking, the working body.
        let original_translated = self.translate(req, cfg, model, envelope_stream)?;
        let mut translated =
            gemini::apply_thinking(req, original_translated.clone(), from, to, PROVIDER, resolved.as_ref())?;
        if let Some(matcher) = self.matcher(cfg) {
            translated = ag::obfuscate_system_instruction(translated, |text| matcher.obfuscate(text));
        }
        translated = ag::sanitize_gemini_signatures(model, translated);
        if req.stream {
            gj::delete(&mut translated, "request.stream");
        }
        req.usage.request(to, &translated);
        let mut body = translated.clone();
        if credits::requested(credential, cfg)
            && let Some(with_credits) = credits::inject(&translated)
        {
            body = with_credits;
        }
        let (body, replay) = replay::prepare(req, model, body).map_err(|e| gemini::status_err(400, e))?;
        let body = ag::ensure_boundary_user_content(model, body);
        Ok(Prepared {
            translated,
            body,
            rule_original: original_translated,
            replay,
        })
    }

    /// `TranslateRequestEnvelopeWithCodexMultiAgentV2` of the working payload.
    fn translate(&self, req: &ExecRequest, cfg: &Config, model: &str, stream: bool) -> Result<Vec<u8>, ExecError> {
        let ctx = RequestCtx {
            model,
            stream,
            tool_search: false,
        };
        let client = crate::codex_client::Client::new(&req.headers, cfg, "", false);
        let info = req.resolved_model.as_ref().map(|r| &r.info);
        gemini::deep_stack(&req.body, || {
            crate::codex_client::translate_request_envelope(
                req.source_format,
                Format::Antigravity,
                &ctx,
                &req.body,
                &client,
                info,
            )
        })
        .map_err(|e| ExecError::local(400, cpa_core::exec::FailureScope::Request, e.to_string()))
    }

    /// `buildRequest` and the send: the envelope, the payload rules on the finished
    /// body (`rule_original` their baseline), the endpoint and Antigravity's headers,
    /// captured as one attempt.
    #[allow(clippy::too_many_arguments)]
    async fn send(
        &self,
        credential: &Credential,
        req: &ExecRequest,
        cfg: &Config,
        model: &str,
        body: Vec<u8>,
        rule_original: &[u8],
        stream: bool,
    ) -> Result<Upstream, ExecError> {
        let token = credential.str("access_token").map(str::trim).unwrap_or_default();
        if token.is_empty() {
            return Err(gemini::status_err(401, "missing access token"));
        }
        let base = ag::base_url(credential);
        let mut url = format!("{base}{}", if stream { ag::STREAM_PATH } else { ag::GENERATE_PATH });
        match gemini::alt(req) {
            Some(alt) => url.push_str(&format!("?$alt={}", cpa_common::gostr::query_escape(alt))),
            None if stream => url.push_str("?alt=sse"),
            None => {}
        }
        let project = auth::project_id(credential);
        if project.is_empty() {
            return Err(gemini::status_err(400, "antigravity auth missing project_id"));
        }
        let derived = ag::derived_session_id(req.derived_session.as_deref());
        let body = ag::gemini_to_antigravity(model, body, project, &derived);
        let body = ag::finish_request(model, body);
        let body = barrier(model, rule_original).apply(cfg, req, body);
        let mut headers = GoHeaders::new();
        headers.set("Content-Type", "application/json");
        headers.set("Authorization", format!("Bearer {token}"));
        headers.set("User-Agent", ag::user_agent(credential));
        gemini::set_custom_headers(&mut headers, credential, req);
        let client = self.client(credential, cfg);
        gemini::send_captured(
            req.capture(),
            credential,
            PROVIDER,
            &client,
            &url,
            headers,
            body.into(),
            stream,
        )
        .await
    }

    /// `Execute` for Gemini-family models: one `generateContent` answer.
    async fn generate(
        &self,
        credential: &Credential,
        mut req: ExecRequest,
        cfg: &Config,
        model: &str,
    ) -> Result<ExecResponse, ExecError> {
        let prepared = self.prepare_request(credential, &mut req, cfg, model, false)?;
        let accumulator = replay::Accumulator::new(&prepared.replay, &prepared.body);
        let upstream = self
            .send(
                credential,
                &req,
                cfg,
                model,
                prepared.body,
                &prepared.rule_original,
                false,
            )
            .await?;
        let (status, headers) = (upstream.status, upstream.headers.clone());
        let ok = (200..300).contains(&status);
        let limit = if ok { usize::MAX } else { proxy::MAX_ERROR_BODY };
        let data = gemini::read_captured(req.capture(), upstream, limit).await?;
        if !ok {
            self.observe_rejection(credential, cfg, model, status, &data);
            replay::clear_on_invalid_signature(&prepared.replay, status, &data);
            return Err(upstream_error(status, data));
        }
        // cacheAntigravityReasoningReplayFromResponse.
        if let Some(mut accumulator) = accumulator
            && !data.is_empty()
        {
            accumulator.observe(&data);
            accumulator.commit();
        }
        let data = self
            .resolve_grounding(
                credential,
                cfg,
                req.source_format,
                &req,
                &prepared.translated,
                data.to_vec(),
            )
            .await;
        req.usage.response_body(Format::Antigravity, &data);
        let out = gemini::translate_non_stream(&req, Format::Antigravity, &prepared.translated, &data)?;
        Ok(ExecResponse {
            status: 200,
            headers,
            body: ResponseBody::Buffered(out),
        })
    }

    /// `executeClaudeNonStream`: the stream endpoint read to its end, its JSON
    /// payloads merged into one answer (`convertStreamToNonStream`).
    async fn merged(
        &self,
        credential: &Credential,
        mut req: ExecRequest,
        cfg: &Config,
        model: &str,
    ) -> Result<ExecResponse, ExecError> {
        let prepared = self.prepare_request(credential, &mut req, cfg, model, true)?;
        let mut accumulator = replay::Accumulator::new(&prepared.replay, &prepared.body);
        let upstream = self
            .send(
                credential,
                &req,
                cfg,
                model,
                prepared.body,
                &prepared.rule_original,
                true,
            )
            .await?;
        let (status, headers) = (upstream.status, upstream.headers.clone());
        if !(200..300).contains(&status) {
            let data = gemini::read_captured(req.capture(), upstream, proxy::MAX_ERROR_BODY).await?;
            self.observe_rejection(credential, cfg, model, status, &data);
            replay::clear_on_invalid_signature(&prepared.replay, status, &data);
            return Err(upstream_error(status, data));
        }
        let mut lines = gemini::captured_lines(proxy::lines(upstream.body, gemini::MAX_LINE), req.capture().clone());
        let mut buffer = Vec::new();
        while let Some(line) = lines.next().await {
            let line = line?;
            if let Some(accumulator) = accumulator.as_mut() {
                accumulator.observe_sse_line(&line);
            }
            let filtered = sse::filter_sse_usage_metadata(&line);
            if let Some(payload) = sse::json_payload(&filtered) {
                buffer.extend_from_slice(payload);
                buffer.push(b'\n');
            }
        }
        if let Some(accumulator) = accumulator {
            accumulator.commit();
        }
        let merged = ag::merge_stream(&buffer);
        let merged = self
            .resolve_grounding(credential, cfg, req.source_format, &req, &prepared.translated, merged)
            .await;
        req.usage.response_body(Format::Antigravity, &merged);
        let out = gemini::translate_non_stream(&req, Format::Antigravity, &prepared.translated, &merged)?;
        Ok(ExecResponse {
            status: 200,
            headers,
            body: ResponseBody::Buffered(out),
        })
    }

    /// `ExecuteStream`: the stream endpoint's lines through the translator.
    async fn stream(
        &self,
        credential: &Credential,
        mut req: ExecRequest,
        cfg: &Config,
        model: &str,
    ) -> Result<ExecResponse, ExecError> {
        let prepared = self.prepare_request(credential, &mut req, cfg, model, true)?;
        let accumulator = replay::Accumulator::new(&prepared.replay, &prepared.body);
        let upstream = self
            .send(
                credential,
                &req,
                cfg,
                model,
                prepared.body,
                &prepared.rule_original,
                true,
            )
            .await?;
        let (status, headers) = (upstream.status, upstream.headers.clone());
        if !(200..300).contains(&status) {
            let data = gemini::read_captured(req.capture(), upstream, proxy::MAX_ERROR_BODY).await?;
            self.observe_rejection(credential, cfg, model, status, &data);
            replay::clear_on_invalid_signature(&prepared.replay, status, &data);
            return Err(upstream_error(status, data));
        }
        let mut lines = gemini::captured_lines(proxy::lines(upstream.body, gemini::MAX_LINE), req.capture().clone());
        let mut commit = None;
        if let Some(accumulator) = accumulator {
            let (observed, on_end) = replay::observe_stream(lines, accumulator);
            lines = observed;
            commit = Some(on_end);
        }
        // Usage filtering and split-payload joining come before grounding resolution, as
        // in Go, which resolves each joined payload.
        lines = gemini::antigravity_payloads(lines);
        if ag::resolves_grounding_urls(req.source_format, &req.body, &prepared.translated) {
            let resolver = Grounding {
                client: self.plain.get(&Proxy::effective(credential, cfg)),
            };
            lines = lines
                .then(move |item| {
                    let resolver = resolver.clone();
                    async move { Ok(Bytes::from(resolver.line(item?).await)) }
                })
                .boxed();
        }
        // The Claude input-token estimate reads the cleaned original request. Each
        // payload is one translator call, newlines included: Go v8.0.27 joins a payload
        // split over lines before translating it.
        let options = cpa_translate::stream::StreamOptions {
            whole_events: true,
            ..Default::default()
        };
        let output = Output::with_options(&req, Format::Antigravity, &prepared.translated, options)
            .with_claude_original(req.body.clone());
        let mut body = gemini::gemini_lines(lines, output, Format::Antigravity);
        if let Some(commit) = commit {
            body = commit.on_clean_end(body);
        }
        Ok(ExecResponse {
            status: 200,
            headers,
            body: ResponseBody::Stream(body),
        })
    }

    /// `resolveWebSearchGroundingURLs` on a whole answer.
    async fn resolve_grounding(
        &self,
        credential: &Credential,
        cfg: &Config,
        from: Format,
        req: &ExecRequest,
        translated: &[u8],
        body: Vec<u8>,
    ) -> Vec<u8> {
        if !ag::resolves_grounding_urls(from, &req.body, translated) {
            return body;
        }
        let resolver = Grounding {
            client: self.plain.get(&Proxy::effective(credential, cfg)),
        };
        resolver.payload(body).await
    }

    /// `executeCompaction` / `executeCompactionStream`: a non-stream Responses summary
    /// of the conversation, sealed into a compaction item.
    async fn compact(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        let model = parse_suffix(&req.model).model_name;
        let source = if req.body.is_empty() && !req.original_body.is_empty() {
            &req.original_body
        } else {
            &req.body
        };
        let mut summary = req.clone();
        summary.body = Bytes::from(compaction::summary_payload(source));
        summary.original_body = Bytes::new();
        summary.alt = None;
        summary.stream = false;
        summary.source_format = Format::OpenAIResponse;
        summary.response_format = Format::OpenAIResponse;
        let stream = req.stream;
        let answer = Box::pin(self.execute(credential, summary, cfg)).await?;
        let ResponseBody::Buffered(payload) = answer.body else {
            return Err(gemini::bad_gateway());
        };
        let text =
            compaction::summary_text(&payload).map_err(|e| compaction::statusless(format!("extract summary: {e}")))?;
        let capsule = compaction::seal(&text, &model)
            .map_err(|e| compaction::statusless(format!("seal compaction capsule: {e}")))?;
        let usage = compaction::summary_usage(&payload);
        let mut headers = answer.headers;
        if !stream {
            return Ok(ExecResponse {
                status: 200,
                headers,
                body: ResponseBody::Buffered(Bytes::from(compaction::response(&model, &capsule, usage))),
            });
        }
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/event-stream"),
        );
        let chunks: Vec<Result<Bytes, ExecError>> = compaction::stream_chunks(&model, &capsule, usage)
            .into_iter()
            .map(|c| Ok(Bytes::from(c)))
            .collect();
        Ok(ExecResponse {
            status: 200,
            headers,
            body: ResponseBody::Stream(futures_util::stream::iter(chunks).boxed()),
        })
    }

    /// `CountTokens`: the translated request without envelope fields Go strips, posted
    /// to `countTokens`; the total in the client's token-count shape.
    async fn count_tokens(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        self.maybe_refresh_credits(credential, cfg);
        let mut req = req;
        let model = parse_suffix(&req.model).model_name;
        let (from, to) = (req.source_format, Format::Antigravity);
        let validated = ag::validate_request_signatures(&model, from, gemini::original_request(&req));
        req.body = Bytes::from(validated);
        let token = credential.str("access_token").map(str::trim).unwrap_or_default();
        if token.is_empty() {
            return Err(gemini::status_err(401, "missing access token"));
        }
        let resolved = gemini::resolved(&req);
        let body = self.translate(&req, cfg, &model, false)?;
        let original = body.clone();
        let mut body = gemini::apply_thinking(&req, body, from, to, PROVIDER, resolved.as_ref())?;
        if let Some(matcher) = self.matcher(cfg) {
            body = ag::obfuscate_system_instruction(body, |text| matcher.obfuscate(text));
        }
        body = ag::sanitize_gemini_signatures(&model, body);
        body = replay::prepare(&req, &model, body)
            .map_err(|e| gemini::status_err(400, e))?
            .0;
        body = ag::ensure_leading_user_content(&model, body);
        for path in [
            "project",
            "model",
            "request.safetySettings",
            "request.toolConfig",
            "request.labels",
            "request.sessionId",
        ] {
            gj::delete(&mut body, path);
        }
        let body = barrier(&model, &original).apply(cfg, &req, body);
        let mut url = format!("{}{}", ag::base_url(credential), ag::COUNT_TOKENS_PATH);
        if let Some(alt) = gemini::alt(&req) {
            url.push_str(&format!("?$alt={}", cpa_common::gostr::query_escape(alt)));
        }
        let mut headers = GoHeaders::new();
        headers.set("Content-Type", "application/json");
        headers.set("Authorization", format!("Bearer {token}"));
        headers.set("User-Agent", ag::user_agent(credential));
        gemini::set_custom_headers(&mut headers, credential, &req);
        let client = self.client(credential, cfg);
        let upstream = gemini::send_captured(
            req.capture(),
            credential,
            PROVIDER,
            &client,
            &url,
            headers,
            body.into(),
            false,
        )
        .await?;
        let (status, response_headers) = (upstream.status, upstream.headers.clone());
        let ok = (200..300).contains(&status);
        let limit = if ok { usize::MAX } else { proxy::MAX_ERROR_BODY };
        let data = gemini::read_captured(req.capture(), upstream, limit).await?;
        if !ok {
            if status == 429 {
                self.close_idle(credential);
            }
            return Err(upstream_error(status, data));
        }
        let count = gj::get(&data, "totalTokens").int();
        let out = cpa_translate::translate_token_count(req.response_format, to, count, &data);
        Ok(ExecResponse {
            status: 200,
            headers: response_headers,
            body: ResponseBody::Buffered(Bytes::from(out)),
        })
    }
}

/// `helps.ResolveAntigravityGroundingURLs`: Vertex Search redirect URIs in grounding
/// chunks replaced by their targets, one HEAD request per distinct URI of a payload.
#[derive(Clone)]
struct Grounding {
    client: wreq::Client,
}

impl Grounding {
    /// `resolveAntigravityGroundingURL`: the `Location` of a 3xx answer when it is an
    /// absolute `https` URL; the URI itself otherwise.
    async fn target(&self, raw: &str) -> String {
        if !ag::is_search_redirect(raw) {
            return raw.to_owned();
        }
        let Ok(answer) = proxy::request_once(&self.client, wreq::Method::HEAD, raw, GoHeaders::new(), None, None).await
        else {
            return raw.to_owned();
        };
        if !(300..400).contains(&answer.status) {
            return raw.to_owned();
        }
        let location = answer
            .headers
            .get(http::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .unwrap_or_default();
        match url::Url::parse(location) {
            Ok(parsed) if parsed.scheme() == "https" && parsed.host_str().is_some_and(|h| !h.is_empty()) => {
                location.to_owned()
            }
            _ => raw.to_owned(),
        }
    }

    async fn payload(&self, body: Vec<u8>) -> Vec<u8> {
        if body.is_empty() {
            return body;
        }
        let Some((base, uris)) = ag::grounding_uris(&body) else {
            return body;
        };
        let mut out = body;
        let mut resolved: HashMap<String, String> = HashMap::new();
        for (i, uri) in uris.iter().enumerate() {
            if uri.is_empty() {
                continue;
            }
            let target = match resolved.get(uri) {
                Some(target) => target.clone(),
                None => {
                    let target = self.target(uri).await;
                    resolved.insert(uri.clone(), target.clone());
                    target
                }
            };
            if target != *uri {
                gj::set_str(&mut out, &format!("{base}.{i}.web.uri"), target);
            }
        }
        out
    }

    /// A stream line with its JSON payload resolved in place. Only `web.uri` values
    /// change, so filtering usage before or after resolution gives the same payload.
    async fn line(&self, line: Bytes) -> Vec<u8> {
        let Some(payload) = sse::json_payload(&line) else {
            return line.to_vec();
        };
        let start = payload.as_ptr() as usize - line.as_ptr() as usize;
        let end = start + payload.len();
        let resolved = self.payload(payload.to_vec()).await;
        [&line[..start], &resolved, &line[end..]].concat()
    }
}

pub fn handles(provider: &str) -> bool {
    provider == PROVIDER
}

#[cfg(test)]
#[path = "antigravity_tests.rs"]
mod tests;
