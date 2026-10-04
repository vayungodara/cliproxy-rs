//! Management-started logins (Go auth_files_provider_oauth.go, oauth_sessions.go,
//! oauth_callback.go, auth_files_oauth_callback.go, auth_files_v8.go):
//! `GET /oauth/auth-url`, `GET /oauth/status`, `DELETE /oauth/session`,
//! `GET|POST /oauth/callback` and `POST /oauth/import`.
//!
//! Go hands callbacks to the waiting login through `.oauth-<provider>-<state>.oauth`
//! files in auth-dir; cliproxy-rs keeps the callback with the pending session in
//! memory, so nothing extra appears in the watched directory.
//!
//! ponytail: the antigravity login and Vertex import answer `provider_not_found` (no
//! executor or auth module for them yet); no plugin logins.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use cpa_core::exec::ExecError;
use cpa_exec::proxy::Proxy;
use serde_json::{Value, json};

use super::auth_files::{Query, fail, reply};
use super::{Management, json as respond};

const SESSION_TTL: Duration = Duration::from_secs(30 * 60);
const COMPLETED_TTL: Duration = Duration::from_secs(60);
const CALLBACK_WAIT: Duration = Duration::from_secs(5 * 60);
const ANTHROPIC_CALLBACK_PORT: u16 = 54545;
const CODEX_CALLBACK_PORT: u16 = 1455;

#[derive(Clone)]
struct Session {
    provider: String,
    status: String,
    completed: bool,
    expires: Instant,
    callback: Option<Callback>,
    /// Go `oauthSessionSourcePlugin`: a plugin auth provider's login.
    plugin: bool,
    /// The plugin's polling context (Go `Session.Metadata`).
    metadata: cpa_plugin::gojson::Metadata,
}

#[derive(Clone)]
struct Callback {
    code: String,
    state: String,
    error: String,
}

/// Go `oauthSessionStore`.
#[derive(Default)]
pub(crate) struct Sessions(Mutex<HashMap<String, Session>>);

impl Sessions {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        map.retain(|_, s| s.expires > now);
        map
    }

    fn register(&self, state: &str, provider: &str) {
        let (state, provider) = (state.trim(), provider.trim().to_lowercase());
        if state.is_empty() || provider.is_empty() {
            return;
        }
        self.lock().insert(
            state.to_owned(),
            Session {
                provider,
                status: String::new(),
                completed: false,
                expires: Instant::now() + SESSION_TTL,
                callback: None,
                plugin: false,
                metadata: Default::default(),
            },
        );
    }

    /// Go `RegisterPlugin`: `false` when the state is unusable or already registered.
    pub(super) fn register_plugin(&self, state: &str, provider: &str, metadata: cpa_plugin::gojson::Metadata) -> bool {
        let (state, provider) = (state.trim(), provider.trim().to_lowercase());
        if state.is_empty() || provider.is_empty() || !valid_state(state) {
            return false;
        }
        let mut map = self.lock();
        if map.contains_key(state) {
            return false;
        }
        map.insert(
            state.to_owned(),
            Session {
                provider,
                status: String::new(),
                completed: false,
                expires: Instant::now() + SESSION_TTL,
                callback: None,
                plugin: true,
                metadata,
            },
        );
        true
    }

    /// Go `GetOAuthSessionDetails`: `(provider, status, plugin, metadata, completed)`.
    pub(super) fn details(&self, state: &str) -> Option<(String, String, bool, cpa_plugin::gojson::Metadata, bool)> {
        self.lock().get(state.trim()).map(|s| {
            (
                s.provider.clone(),
                s.status.clone(),
                s.plugin,
                s.metadata.clone(),
                s.completed,
            )
        })
    }

    pub(super) fn set_error(&self, state: &str, message: &str) {
        let message = match message.trim() {
            "" => "Authentication failed",
            m => m,
        };
        if let Some(s) = self.lock().get_mut(state.trim()).filter(|s| !s.completed) {
            s.status = message.to_owned();
            s.expires = Instant::now() + SESSION_TTL;
        }
    }

    pub(super) fn complete(&self, state: &str) {
        if let Some(s) = self.lock().get_mut(state.trim()).filter(|s| !s.completed) {
            s.status.clear();
            s.callback = None;
            s.metadata.clear();
            s.completed = true;
            s.expires = Instant::now() + COMPLETED_TTL;
        }
    }

    /// `(provider, status, completed)`.
    fn get(&self, state: &str) -> Option<(String, String, bool)> {
        self.lock()
            .get(state.trim())
            .map(|s| (s.provider.clone(), s.status.clone(), s.completed))
    }

    fn is_pending(&self, state: &str, provider: &str) -> bool {
        let provider = provider.trim().to_lowercase();
        self.lock().get(state.trim()).is_some_and(|s| {
            !s.completed && s.status.is_empty() && (provider.is_empty() || s.provider.eq_ignore_ascii_case(&provider))
        })
    }

    fn cancel(&self, state: &str) -> bool {
        let mut map = self.lock();
        let state = state.trim();
        if map.get(state).is_some_and(|s| !s.completed && s.status.is_empty()) {
            map.remove(state);
            return true;
        }
        false
    }

    /// Go `WriteOAuthCallbackFileForPendingSession`: keeps the callback for the waiter.
    /// `false` when the provider or state is unusable or the login is not pending.
    fn deliver(&self, provider: &str, state: &str, code: &str, error: &str) -> bool {
        let Some(provider) = normalize_callback_provider(provider) else {
            return false;
        };
        if !valid_state(state) || !self.is_pending(state, &provider) {
            return false;
        }
        if let Some(s) = self.lock().get_mut(state.trim()) {
            s.callback = Some(Callback {
                code: code.trim().to_owned(),
                state: state.trim().to_owned(),
                error: error.trim().to_owned(),
            });
        }
        true
    }

    fn take_callback(&self, state: &str) -> Option<Callback> {
        self.lock().get_mut(state).and_then(|s| s.callback.take())
    }
}

/// Go `ValidateOAuthState`.
pub(super) fn valid_state(state: &str) -> bool {
    let s = state.trim();
    !s.is_empty()
        && s.len() <= 128
        && !s.contains("..")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Go `NormalizeOAuthCallbackProvider`: the built-in aliases, else a plugin-style name.
fn normalize_callback_provider(provider: &str) -> Option<String> {
    let p = provider.trim().to_lowercase();
    let builtin = match p.as_str() {
        "anthropic" | "claude" => Some("anthropic"),
        "codex" | "openai" => Some("codex"),
        "antigravity" | "anti-gravity" => Some("antigravity"),
        "xai" | "x-ai" | "x.ai" | "grok" => Some("xai"),
        "devin" | "cognition" => Some("devin"),
        "meta" | "muse" => Some("meta"),
        _ => None,
    };
    if let Some(b) = builtin {
        return Some(b.to_owned());
    }
    (!p.is_empty()
        && p.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
    .then_some(p)
}

/// Go `oauthSessionErrorWithCause`.
fn with_cause(message: &str, cause: &str) -> String {
    match redact_upstream(cause).trim() {
        "" => message.to_owned(),
        detail => format!("{message}: {detail}"),
    }
}

/// Cuts upstream-supplied text from a login failure cause: a response body after its
/// status, an OAuth error description after its error code, and a decoder diagnostic
/// (which quotes response values) after its `parse ...` context. Go keeps them, but
/// they can carry tokens and session errors are readable by anyone holding the state
/// (the key-less callback). The earliest boundary wins: upstream text follows it.
fn redact_upstream(cause: &str) -> String {
    let mut cut: Option<(usize, String)> = None;
    let mut consider = |at: usize, kept: String| {
        if cut.as_ref().is_none_or(|(i, _)| at < *i) {
            cut = Some((at, kept));
        }
    };
    for marker in ["with status ", "(HTTP "] {
        if let Some(i) = cause.find(marker) {
            let start = i + marker.len();
            let mut end = start + cause[start..].bytes().take_while(u8::is_ascii_digit).count();
            if marker == "(HTTP " && cause[end..].starts_with(')') {
                end += 1;
            }
            consider(i, cause[..end].to_owned());
        }
    }
    for marker in [
        "error from authorization server: ",
        "device token error: ",
        "OAuth error: ",
    ] {
        if let Some(i) = cause.find(marker) {
            let start = i + marker.len();
            let code: String = cause[start..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .take(64)
                .collect();
            consider(i, format!("{}{code}", &cause[..start]));
        }
    }
    if let Some(i) = cause.find("parse ")
        && let Some(j) = cause[i..].find(": ")
    {
        consider(i, cause[..i + j].to_owned());
    }
    cut.map_or_else(|| cause.to_owned(), |(_, kept)| kept)
}

fn exec_text(e: &ExecError) -> String {
    String::from_utf8_lossy(&e.body).into_owned()
}

/// Installs the main listener's provider-callback hook (Go's `/anthropic/callback`
/// etc. call `WriteOAuthCallbackFileForPendingSession`).
pub(super) fn install_callback_sink(state: &Arc<Management>) {
    let weak = Arc::downgrade(state);
    state
        .rt
        .set_oauth_callback_sink(Some(Arc::new(move |cb: &crate::runtime::OAuthCallback| {
            weak.upgrade()
                .is_some_and(|m| m.oauth.deliver(cb.provider, &cb.state, &cb.code, &cb.error))
        })));
}

fn status_error(status: StatusCode, message: &str) -> Response {
    respond(status, &json!({"status": "error", "error": message}))
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// Go `misc.GenerateRandomState`: 16 random bytes, hex.
fn random_state() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// `requests.proxy-url`, which Go's auth services use.
fn global_proxy(state: &Management) -> Proxy {
    let cfg = state.rt.config();
    let global = cfg
        .document
        .get("requests")
        .and_then(|r| r.get("proxy-url"))
        .and_then(serde_yaml_ng::Value::as_str)
        .unwrap_or_default();
    Proxy::parse(global)
}

/// The client for login traffic: the global proxy (else the environment's proxies);
/// local endpoints in tests.
fn login_client(state: &Management) -> wreq::Client {
    if state.login_base.is_some() {
        return state.clients.get(&Proxy::Direct);
    }
    state.clients.get(&global_proxy(state))
}

/// Go `StartOAuthV8`.
pub(crate) async fn auth_url(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery) -> Response {
    let raw_query = raw.clone();
    let q = Query::parse(raw);
    let webui = matches!(
        q.first("is_webui").trim().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    );
    match q.first("provider").trim().to_lowercase().as_str() {
        "" => fail(StatusCode::BAD_REQUEST, "provider is required"),
        "claude" => start_claude(state, webui).await,
        "codex" => start_codex(state, webui).await,
        "kimi" => {
            // Go `RequestKimiToken`: `domain`, else `channel`, else kimi.com.
            let domain = [q.first("domain"), q.first("channel")]
                .into_iter()
                .map(str::trim)
                .find(|v| !v.is_empty())
                .unwrap_or(cpa_exec::kimi_auth::DOMAIN_COM)
                .to_owned();
            start_kimi(state, domain).await
        }
        "kimi-ai" => start_kimi(state, cpa_exec::kimi_auth::DOMAIN_AI.to_owned()).await,
        "meta" => start_meta(state).await,
        "xai" => start_xai(state).await,
        "devin" => start_devin(state).await,
        // Go `ServePluginAuthURL`: a plugin auth provider's login, else not found.
        _ => {
            let raw = raw_query.as_deref().unwrap_or_default();
            match super::plugin_oauth::serve_auth_url(&state, "/v8/management/oauth/auth-url", raw).await {
                Some(response) => response,
                None => fail(StatusCode::NOT_FOUND, "provider_not_found"),
            }
        }
    }
}

/// Go `ImportOAuthV8`.
pub(crate) async fn import(RawQuery(raw): RawQuery) -> Response {
    match Query::parse(raw).first("provider").trim() {
        "" => fail(StatusCode::BAD_REQUEST, "provider is required"),
        _ => fail(StatusCode::NOT_FOUND, "provider_not_found"),
    }
}

/// How a login ended, for the session (Go's `SetOAuthSessionError` texts).
enum Outcome {
    Saved,
    /// The session stopped being pending: nothing is saved or reported.
    Cancelled,
    Failed(String),
}

/// Saves through `write` only while the session is still pending (Go
/// `guardOAuthSessionPendingForSave`), then resyncs credentials.
async fn save_if_pending<T: Send + 'static>(
    state: &Arc<Management>,
    sid: &str,
    provider: &str,
    value: T,
    write: impl FnOnce(PathBuf, T) -> Result<PathBuf, String> + Send + 'static,
    save_error: &str,
) -> Outcome {
    let auth_dir = state.rt.config().auth_dir.clone();
    let (guard, sid, provider) = (state.clone(), sid.to_owned(), provider.to_owned());
    let saved = tokio::task::spawn_blocking(move || {
        // Checked on the blocking thread itself, right before the write.
        if !guard.oauth.is_pending(&sid, &provider) {
            return Ok(None);
        }
        write(auth_dir, value).map(Some)
    })
    .await;
    match saved {
        Ok(Ok(Some(_))) => Outcome::Saved,
        Ok(Ok(None)) => Outcome::Cancelled,
        _ => Outcome::Failed(save_error.to_owned()),
    }
}

fn settle(state: &Management, sid: &str, outcome: Outcome) {
    match outcome {
        Outcome::Saved => {
            {
                let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
                state.publish((*state.rt.config()).clone(), None);
            }
            state.oauth.complete(sid);
        }
        Outcome::Cancelled => {}
        Outcome::Failed(message) => state.oauth.set_error(sid, &message),
    }
}

fn started(url: String, sid: String) -> Response {
    reply(
        StatusCode::OK,
        [("status", "ok".into()), ("url", url.into()), ("state", sid.into())],
    )
}

/// Go `RequestAnthropicToken`.
async fn start_claude(state: Arc<Management>, webui: bool) -> Response {
    use cpa_exec::claude_login::ManagedLogin;
    let login = match ManagedLogin::start() {
        Ok(l) => l,
        Err(_) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "failed to generate PKCE codes"),
    };
    state.oauth.register(&login.state, "anthropic");
    let forwarder = if webui {
        match start_forwarder(&state, ANTHROPIC_CALLBACK_PORT, "/anthropic/callback").await {
            Ok(id) => Some(id),
            Err(message) => return fail(StatusCode::INTERNAL_SERVER_ERROR, message),
        }
    } else {
        None
    };
    let (url, sid) = (login.url.clone(), login.state.clone());
    let worker = state.clone();
    tokio::spawn(async move {
        if let Some(code) = wait_for_callback(&worker, &login.state, "anthropic", "Bad request").await {
            let exchanged = match &worker.login_base {
                Some(base) => {
                    let oauth = cpa_exec::oauth::OAuth::with_endpoints(
                        login_client(&worker),
                        &format!("{base}/v1/oauth/token"),
                        &format!("{base}/api/oauth/profile"),
                        &format!("{base}/api/oauth/claude_cli/roles"),
                    );
                    login.exchange_with(&oauth, &code).await
                }
                None => login.exchange(&code, &global_proxy(&worker)).await,
            };
            let outcome = match exchanged {
                Ok(patch) => {
                    let write = |dir: PathBuf, patch| ManagedLogin::save(&dir, patch).map_err(|e| exec_text(&e));
                    save_if_pending(
                        &worker,
                        &login.state,
                        "anthropic",
                        patch,
                        write,
                        "Failed to save authentication tokens",
                    )
                    .await
                }
                Err(_) => Outcome::Failed("Failed to exchange authorization code for tokens".into()),
            };
            settle(&worker, &login.state, outcome);
        }
        stop_forwarder(&worker, ANTHROPIC_CALLBACK_PORT, forwarder);
    });
    started(url, sid)
}

/// Go `RequestCodexToken`.
async fn start_codex(state: Arc<Management>, webui: bool) -> Response {
    use cpa_exec::codex_oauth::{CodexOAuth, authorize_url, redirect_uri, write_login};
    let Ok((verifier, challenge)) = cpa_exec::oauth::pkce() else {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "failed to generate PKCE codes");
    };
    let Some(sid) = random_state() else {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "failed to generate state parameter");
    };
    let url = authorize_url(&sid, &challenge, CODEX_CALLBACK_PORT);
    state.oauth.register(&sid, "codex");
    let forwarder = if webui {
        match start_forwarder(&state, CODEX_CALLBACK_PORT, "/codex/callback").await {
            Ok(id) => Some(id),
            Err(message) => return fail(StatusCode::INTERNAL_SERVER_ERROR, message),
        }
    } else {
        None
    };
    let worker = state.clone();
    let flow = sid.clone();
    tokio::spawn(async move {
        if let Some(code) = wait_for_callback(&worker, &flow, "codex", "Bad Request").await {
            let client = login_client(&worker);
            let oauth = match &worker.login_base {
                Some(base) => CodexOAuth::with_endpoints(
                    client,
                    &format!("{base}/oauth/token"),
                    &format!("{base}/api/accounts/deviceauth/usercode"),
                    &format!("{base}/api/accounts/deviceauth/token"),
                ),
                None => CodexOAuth::new(client),
            };
            let outcome = match oauth
                .exchange(&code, &redirect_uri(CODEX_CALLBACK_PORT), &verifier)
                .await
            {
                Ok(tokens) => {
                    let write = |dir: PathBuf, tokens| write_login(&dir, &tokens).map_err(|e| exec_text(&e));
                    save_if_pending(
                        &worker,
                        &flow,
                        "codex",
                        tokens,
                        write,
                        "Failed to save authentication tokens",
                    )
                    .await
                }
                Err(e) => Outcome::Failed(with_cause(
                    "Failed to exchange authorization code for tokens",
                    &exec_text(&e),
                )),
            };
            settle(&worker, &flow, outcome);
        }
        stop_forwarder(&worker, CODEX_CALLBACK_PORT, forwarder);
    });
    started(url, sid)
}

/// Go's callback wait: every 500 ms for up to five minutes, until the callback
/// arrives, the session stops being pending, or the deadline passes (recorded as the
/// session's error).
async fn next_callback(state: &Management, sid: &str, provider: &str) -> Option<Callback> {
    let deadline = Instant::now() + CALLBACK_WAIT;
    loop {
        if !state.oauth.is_pending(sid, provider) {
            return None;
        }
        if Instant::now() > deadline {
            state.oauth.set_error(sid, "Timeout waiting for OAuth callback");
            return None;
        }
        if let Some(cb) = state.oauth.take_callback(sid) {
            return Some(cb);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// [`next_callback`] with the Claude and Codex checks: an error first, then the state.
/// Returns the code.
async fn wait_for_callback(state: &Management, sid: &str, provider: &str, bad_request: &str) -> Option<String> {
    let cb = next_callback(state, sid, provider).await?;
    if !cb.error.is_empty() {
        state.oauth.set_error(sid, bad_request);
        return None;
    }
    if cb.state != sid {
        state.oauth.set_error(sid, "State code error");
        return None;
    }
    Some(cb.code)
}

/// Go `RequestDevinToken`: PKCE login whose callback reaches the main listener's
/// `/callback` (Devin only accepts `http://127.0.0.1:<port>/callback`).
async fn start_devin(state: Arc<Management>) -> Response {
    use cpa_exec::devin_auth::{DevinAuth, generate_pkce, save_record};
    let port = state.rt.config().port;
    if port == 0 {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "callback server unavailable");
    }
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let Ok(pkce) = generate_pkce() else {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "failed to generate PKCE codes");
    };
    let Some(sid) = random_state() else {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "failed to generate state parameter");
    };
    let mut auth = DevinAuth::new(login_client(&state));
    if let Some(base) = &state.login_base {
        auth = auth.with_app_base(base).with_api_base(base).with_server_base(base);
    }
    let url = auth.build_authorization_url(&redirect, &pkce.challenge, &sid);
    state.oauth.register(&sid, "devin");
    let worker = state.clone();
    let flow = sid.clone();
    tokio::spawn(async move {
        // Go runs the whole flow under one five-minute context.
        let deadline = tokio::time::Instant::now() + CALLBACK_WAIT;
        let Some(cb) = next_callback(&worker, &flow, "devin").await else {
            return;
        };
        let fail = |message: &str| {
            if worker.oauth.is_pending(&flow, "devin") {
                settle(&worker, &flow, Outcome::Failed(message.into()));
            }
        };
        if cb.state != flow {
            return fail("State code error");
        }
        if !cb.error.is_empty() {
            return fail("Devin authorization denied");
        }
        if cb.code.trim().is_empty() {
            return fail("Missing authorization code");
        }
        // Upstream errors can carry tokens or codes: only Go's fixed messages surface.
        let exchanged = tokio::select! {
            r = auth.exchange_code(&cb.code, &pkce.verifier) => r.ok(),
            () = tokio::time::sleep_until(deadline) => None,
            () = cancelled(&worker, &flow, "devin") => return,
        };
        let token = match exchanged {
            Some(token) if !token.trim().is_empty() => token,
            _ => return fail("Failed to exchange authorization code for tokens"),
        };
        if !worker.oauth.is_pending(&flow, "devin") {
            return;
        }
        let created = tokio::select! {
            r = auth.create_auth_record(&token) => r.ok(),
            () = tokio::time::sleep_until(deadline) => None,
            () = cancelled(&worker, &flow, "devin") => return,
        };
        let Some(record) = created else {
            return fail("Failed to create Devin authentication record");
        };
        let write = |dir: PathBuf, record| save_record(&dir, &record);
        let outcome = save_if_pending(
            &worker,
            &flow,
            "devin",
            record,
            write,
            "Failed to save authentication tokens",
        )
        .await;
        settle(&worker, &flow, outcome);
    });
    started(url, sid)
}

/// Go `watchOAuthSessionCancel`: resolves once the session stops being pending.
async fn cancelled(state: &Management, sid: &str, provider: &str) {
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if !state.oauth.is_pending(sid, provider) {
            return;
        }
    }
}

/// The JSON answer for a started device login.
fn device_started(url: String, sid: String, user_code: &str, expires_in: Option<i64>) -> Response {
    let mut out = vec![
        ("status", Value::from("ok")),
        ("url", url.into()),
        ("state", sid.into()),
        ("flow", "device".into()),
    ];
    if !user_code.trim().is_empty() {
        out.push(("user_code", user_code.trim().into()));
    }
    if let Some(e) = expires_in {
        out.push(("expires_in", e.into()));
    }
    reply(StatusCode::OK, out)
}

/// Go `requestKimiTokenWithDomain`.
async fn start_kimi(state: Arc<Management>, domain: String) -> Response {
    use cpa_exec::kimi_auth::{DeviceFlow, is_ai_domain, login_record, write_login};
    let (provider, prefix) = if is_ai_domain(&domain) {
        ("kimi-ai", "kmi-ai")
    } else {
        ("kimi", "kmi")
    };
    let sid = format!("{prefix}-{}", unix_nanos());
    let mut flow = DeviceFlow::new(login_client(&state), &domain, "");
    if let Some(base) = &state.login_base {
        flow = flow.with_oauth_host(base);
    }
    let Ok(code) = flow.request_device_code().await else {
        return fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to generate authorization url",
        );
    };
    let url = if code.verification_uri_complete.is_empty() {
        code.verification_uri.clone()
    } else {
        code.verification_uri_complete.clone()
    };
    state.oauth.register(&sid, provider);
    let response = device_started(
        url,
        sid.clone(),
        &code.user_code,
        (code.expires_in > 0).then_some(code.expires_in),
    );
    let worker = state.clone();
    tokio::spawn(async move {
        let polled = tokio::select! {
            t = flow.poll(&code) => t,
            () = cancelled(&worker, &sid, provider) => return,
        };
        let outcome = match polled {
            Ok(tokens) => {
                let mut record = login_record(
                    provider,
                    &tokens,
                    flow.device_id(),
                    chrono::Utc::now().timestamp_millis(),
                );
                // Go records the requested domain as given.
                record.metadata.insert("domain".into(), domain.clone().into());
                let write = |dir: PathBuf, record| write_login(&dir, &record).map_err(|e| exec_text(&e));
                save_if_pending(
                    &worker,
                    &sid,
                    provider,
                    record,
                    write,
                    "Failed to save authentication tokens",
                )
                .await
            }
            Err(_) if !worker.oauth.is_pending(&sid, provider) => Outcome::Cancelled,
            Err(e) => Outcome::Failed(with_cause("Authentication failed", &exec_text(&e))),
        };
        settle(&worker, &sid, outcome);
    });
    response
}

/// Go `RequestMetaToken`.
async fn start_meta(state: Arc<Management>) -> Response {
    use cpa_exec::meta_auth::{MetaAuth, create_token_storage, save_login};
    const MAX_POLL_SECONDS: i64 = 15 * 60;
    let sid = format!("meta-{}", unix_nanos());
    let mut auth = MetaAuth::new(login_client(&state));
    if let Some(base) = &state.login_base {
        auth = auth
            .with_auth_host(base)
            .with_mint_url(&format!("{base}/muse-code/key"));
    }
    let Ok(code) = auth.start_device_flow().await else {
        return fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to start device authorization flow",
        );
    };
    let url = match code.verification_uri_complete.trim() {
        "" => code.verification_uri.trim().to_owned(),
        u => u.to_owned(),
    };
    state.oauth.register(&sid, "meta");
    let expires = if code.expires_in > 0 {
        code.expires_in
    } else {
        MAX_POLL_SECONDS
    };
    let response = device_started(url, sid.clone(), &code.user_code, Some(expires));
    let worker = state.clone();
    tokio::spawn(async move {
        let waited = tokio::select! {
            b = auth.wait_for_authorization(&code) => b,
            () = cancelled(&worker, &sid, "meta") => return,
        };
        let now = chrono::Utc::now();
        let outcome = match waited {
            Err(_) if !worker.oauth.is_pending(&sid, "meta") => Outcome::Cancelled,
            Err(e) => Outcome::Failed(with_cause("Authentication failed", &e)),
            Ok(_) if !worker.oauth.is_pending(&sid, "meta") => Outcome::Cancelled,
            Ok(bundle) if create_token_storage(&bundle, now).access_token.trim().is_empty() => {
                Outcome::Failed("Failed to exchange token".into())
            }
            Ok(bundle) => {
                let write = move |dir: PathBuf, bundle| save_login(&dir, &bundle, now).map(|o| o.path);
                save_if_pending(&worker, &sid, "meta", bundle, write, "Failed to save token to file").await
            }
        };
        settle(&worker, &sid, outcome);
    });
    response
}

/// Go `RequestXAIToken`: device flow; the poller saves the credential file.
async fn start_xai(state: Arc<Management>) -> Response {
    use cpa_exec::xai_auth::{LoginRecord, MAX_POLL_DURATION, XaiAuth, login_record, saved_document};
    let sid = format!("xai-{}", unix_nanos());
    let mut auth = XaiAuth::new(login_client(&state));
    if let Some(base) = &state.login_base {
        auth = auth
            .with_issuer_origin(base)
            .with_min_poll_interval(std::time::Duration::from_millis(10));
    }
    let Ok(code) = auth.start_device_flow().await else {
        return fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to start device authorization flow",
        );
    };
    let url = match code.verification_uri_complete.trim() {
        "" => code.verification_uri.trim().to_owned(),
        u => u.to_owned(),
    };
    state.oauth.register(&sid, "xai");
    let expires = if code.expires_in > 0 {
        code.expires_in
    } else {
        MAX_POLL_DURATION.as_secs() as i64
    };
    let response = device_started(url, sid.clone(), &code.user_code, Some(expires));
    // Go `saveTokenRecord` through the file token store: merged with an existing file.
    fn save_login(dir: &std::path::Path, record: LoginRecord) -> Result<PathBuf, String> {
        let path = dir.join(&record.file_name);
        let existing = std::fs::read(&path).ok().filter(|raw| !raw.is_empty());
        let document = saved_document(record.metadata, existing.as_deref())?;
        let text = cpa_common::json::GoValue::Object(document).encode_indented();
        std::fs::create_dir_all(dir)
            .and_then(|()| super::auth_files::write_file(&path, &text))
            .map_err(|e| e.to_string())?;
        Ok(path)
    }
    let worker = state.clone();
    tokio::spawn(async move {
        let waited = tokio::select! {
            t = auth.poll(&code) => t,
            () = cancelled(&worker, &sid, "xai") => return,
        };
        let now = chrono::Utc::now();
        let outcome = match waited {
            _ if !worker.oauth.is_pending(&sid, "xai") => Outcome::Cancelled,
            Err(e) => Outcome::Failed(with_cause("Authentication failed", &exec_text(&e))),
            Ok(tokens) if tokens.access_token.trim().is_empty() => Outcome::Failed("Failed to exchange token".into()),
            Ok(tokens) => {
                // Go `WaitForAuthorization`: RFC 3339 seconds in UTC.
                let last_refresh = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
                let record = login_record(&tokens, &last_refresh, &code.token_endpoint, now.timestamp_millis());
                let write = |dir: PathBuf, record| save_login(&dir, record);
                save_if_pending(&worker, &sid, "xai", record, write, "Failed to save token to file").await
            }
        };
        settle(&worker, &sid, outcome);
    });
    response
}

/// A running callback forwarder (Go `callbackForwarder`).
pub(crate) struct Forwarder {
    id: u64,
    task: tokio::task::JoinHandle<()>,
}

/// Go `startCallbackForwarder`: `0.0.0.0:<port>` redirects every request to the
/// main listener's provider callback. A forwarder already on that port is stopped
/// (and its listener released) first. Errors carry Go's message for the 500 answer.
async fn start_forwarder(state: &Management, port: u16, path: &str) -> Result<u64, &'static str> {
    let cfg = state.rt.config();
    if cfg.port == 0 {
        return Err("callback server unavailable");
    }
    let tls = cfg
        .document
        .get("server")
        .and_then(|s| s.get("tls"))
        .and_then(|t| t.get("enable"))
        .and_then(serde_yaml_ng::Value::as_bool)
        .unwrap_or(false);
    let target = format!("{}://127.0.0.1:{}{path}", if tls { "https" } else { "http" }, cfg.port);
    let previous = state
        .forwarders
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&port);
    if let Some(previous) = previous {
        previous.task.abort();
        let _ = previous.task.await;
    }
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .map_err(|_| "failed to start callback server")?;
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(forward(stream, target.clone()));
        }
    });
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    state
        .forwarders
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(port, Forwarder { id, task });
    Ok(id)
}

const FORWARD_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_HEAD: usize = 16 << 10;

/// One forwarder connection with Go's server deadlines (`ReadHeaderTimeout` and
/// `WriteTimeout`, 5 s each): read the request head, answer with the redirect, close.
async fn forward(mut stream: tokio::net::TcpStream, target: String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut head = Vec::new();
    let read = tokio::time::timeout(FORWARD_TIMEOUT, async {
        let mut buf = [0u8; 2048];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < MAX_REQUEST_HEAD {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            head.extend_from_slice(&buf[..n]);
        }
        Ok::<_, std::io::Error>(())
    })
    .await;
    if !matches!(read, Ok(Ok(()))) || !head.windows(4).any(|w| w == b"\r\n\r\n") {
        return;
    }
    let line = head.split(|b| *b == b'\n').next().unwrap_or_default();
    let line = String::from_utf8_lossy(line);
    let mut parts = line.trim_end_matches('\r').split(' ');
    let response = match (parts.next(), parts.next(), parts.next()) {
        (Some(method), Some(uri), Some(version)) if version.starts_with("HTTP/") => {
            redirect_bytes(method, &target, uri.split_once('?').map(|(_, q)| q))
        }
        _ => b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
    };
    let _ = tokio::time::timeout(FORWARD_TIMEOUT, async {
        stream.write_all(&response).await?;
        stream.shutdown().await
    })
    .await;
}

/// The forwarder's answer: Go `http.Redirect` (302; an HTML link body for GET and
/// HEAD) plus `Cache-Control: no-store`.
fn redirect_bytes(method: &str, target: &str, query: Option<&str>) -> Vec<u8> {
    let location = match query.filter(|q| !q.is_empty()) {
        Some(q) if target.contains('?') => format!("{target}&{q}"),
        Some(q) => format!("{target}?{q}"),
        None => target.to_owned(),
    };
    let mut out = format!("HTTP/1.1 302 Found\r\nCache-Control: no-store\r\nLocation: {location}\r\n");
    let body = if matches!(method, "GET" | "HEAD") {
        let escaped = location
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&#34;")
            .replace('\'', "&#39;");
        format!("<a href=\"{escaped}\">Found</a>.\n\n")
    } else {
        String::new()
    };
    if !body.is_empty() {
        out.push_str("Content-Type: text/html; charset=utf-8\r\n");
    }
    out.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()));
    if method != "HEAD" {
        out.push_str(&body);
    }
    out.into_bytes()
}

/// Go `stopCallbackForwarderInstance`: only the forwarder this login started.
fn stop_forwarder(state: &Management, port: u16, id: Option<u64>) {
    let Some(id) = id else { return };
    let mut forwarders = state.forwarders.lock().unwrap_or_else(PoisonError::into_inner);
    if forwarders.get(&port).is_some_and(|f| f.id == id)
        && let Some(f) = forwarders.remove(&port)
    {
        f.task.abort();
    }
}

/// Go `GetAuthStatus`; a pending plugin login is polled through its plugin.
pub(crate) async fn status(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery) -> Response {
    let q = Query::parse(raw);
    let sid = q.first("state").trim().to_owned();
    if sid.is_empty() {
        return reply(StatusCode::OK, [("status", "ok".into())]);
    }
    if !valid_state(&sid) {
        return status_error(StatusCode::BAD_REQUEST, "invalid state");
    }
    match state.oauth.details(&sid) {
        None => status_error(StatusCode::OK, "unknown or expired state"),
        Some((_, _, _, _, true)) => reply(StatusCode::OK, [("status", "ok".into())]),
        Some((_, status, _, _, _)) if !status.is_empty() => status_error(StatusCode::OK, &status),
        Some((provider, _, true, metadata, _)) if state.rt.plugins().has_auth_provider(&provider) => {
            match super::plugin_oauth::poll(&state, &sid, &provider, metadata).await {
                Some(response) => response,
                None => reply(StatusCode::OK, [("status", "wait".into())]),
            }
        }
        Some(_) => reply(StatusCode::OK, [("status", "wait".into())]),
    }
}

/// Go `CancelAuthSession`.
pub(crate) async fn cancel(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery) -> Response {
    let q = Query::parse(raw);
    let sid = q.first("state").trim().to_owned();
    if sid.is_empty() {
        return status_error(StatusCode::BAD_REQUEST, "missing state");
    }
    if !valid_state(&sid) {
        return status_error(StatusCode::BAD_REQUEST, "invalid state");
    }
    let cancelled = state.oauth.cancel(&sid);
    reply(
        StatusCode::OK,
        [("status", "ok".into()), ("cancelled", cancelled.into())],
    )
}

#[derive(Default)]
struct CallbackRequest {
    provider: String,
    redirect_url: String,
    code: String,
    state: String,
    error: String,
}

/// Go `GetOAuthCallback`.
pub(crate) async fn callback_get(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery) -> Response {
    let q = Query::parse(raw);
    let error = [q.first("error"), q.first("error_description")]
        .into_iter()
        .map(str::trim)
        .find(|v| !v.is_empty())
        .unwrap_or_default()
        .to_owned();
    let req = CallbackRequest {
        provider: q.first("provider").trim().to_owned(),
        code: q.first("code").trim().to_owned(),
        state: q.first("state").trim().to_owned(),
        error,
        ..CallbackRequest::default()
    };
    handle_callback(&state, req)
}

/// Go `PostOAuthCallback`: gin's `ShouldBindJSON` into `oauthCallbackRequest`
/// (first JSON value, exact-then-case-insensitive names, type mismatches rejected).
pub(crate) async fn callback_post(State(state): State<Arc<Management>>, body: Bytes) -> Response {
    const FIELDS: [&str; 5] = ["provider", "redirect_url", "code", "state", "error"];
    let invalid = || status_error(StatusCode::BAD_REQUEST, "invalid body");
    let members = serde_json::Deserializer::from_slice(&body)
        .into_iter::<super::api_call::Members>()
        .next();
    let Some(Ok(super::api_call::Members(members))) = members else {
        return invalid();
    };
    let mut req = CallbackRequest::default();
    for (key, v) in members.unwrap_or_default() {
        let Some(i) = FIELDS
            .iter()
            .position(|f| *f == key)
            .or_else(|| FIELDS.iter().position(|f| f.eq_ignore_ascii_case(&key)))
        else {
            continue;
        };
        let slot = match i {
            0 => &mut req.provider,
            1 => &mut req.redirect_url,
            2 => &mut req.code,
            3 => &mut req.state,
            _ => &mut req.error,
        };
        match v {
            Value::String(s) => *slot = s,
            Value::Null => {}
            _ => return invalid(),
        }
    }
    handle_callback(&state, req)
}

/// Go `handleOAuthCallback`.
fn handle_callback(state: &Management, req: CallbackRequest) -> Response {
    let (mut sid, mut code, mut error) = (
        req.state.trim().to_owned(),
        req.code.trim().to_owned(),
        req.error.trim().to_owned(),
    );
    let redirect = req.redirect_url.trim();
    if !redirect.is_empty() {
        let Some(query) = redirect_query(redirect) else {
            return status_error(StatusCode::BAD_REQUEST, "invalid redirect_url");
        };
        let q = Query::parse(Some(query));
        let get = |k: &str| q.first(k).trim().to_owned();
        if sid.is_empty() {
            sid = get("state");
        }
        if code.is_empty() {
            code = get("code");
        }
        if error.is_empty() {
            error = Some(get("error"))
                .filter(|e| !e.is_empty())
                .unwrap_or_else(|| get("error_description"));
        }
    }
    if sid.is_empty() {
        return status_error(StatusCode::BAD_REQUEST, "state is required");
    }
    if !valid_state(&sid) {
        return status_error(StatusCode::BAD_REQUEST, "invalid state");
    }
    if code.is_empty() && error.is_empty() {
        return status_error(StatusCode::BAD_REQUEST, "code or error is required");
    }
    let Some((session_provider, status, plugin, _, completed)) = state.oauth.details(&sid) else {
        return status_error(StatusCode::NOT_FOUND, "unknown or expired state");
    };
    if completed {
        return status_error(StatusCode::CONFLICT, "oauth flow is already completed");
    }
    let provider = match req.provider.trim() {
        "" => session_provider.clone(),
        p => p.to_owned(),
    };
    let canonical = if plugin {
        super::plugin_oauth::normalize_provider(&provider)
    } else {
        normalize_callback_provider(&provider)
    };
    let Some(canonical) = canonical else {
        return status_error(StatusCode::BAD_REQUEST, "unsupported provider");
    };
    if !status.is_empty() {
        return status_error(StatusCode::CONFLICT, &status);
    }
    if !session_provider.eq_ignore_ascii_case(&canonical) {
        return status_error(StatusCode::BAD_REQUEST, "provider does not match state");
    }
    // A plugin reads its callback from the auth directory, where Go writes every one.
    if plugin && state.oauth.is_pending(&sid, &canonical) {
        let auth_dir = state.rt.config().auth_dir.clone();
        return match super::plugin_oauth::write_callback_file(&auth_dir, &canonical, &sid, &code, &error) {
            Ok(()) => reply(StatusCode::OK, [("status", "ok".into())]),
            Err(e) => {
                tracing::error!(error = %e, "failed to persist oauth callback");
                status_error(StatusCode::INTERNAL_SERVER_ERROR, "failed to persist oauth callback")
            }
        };
    }
    if !plugin && state.oauth.deliver(&canonical, &sid, &code, &error) {
        return reply(StatusCode::OK, [("status", "ok".into())]);
    }
    match state.oauth.get(&sid) {
        Some((_, status, false)) if !status.is_empty() => status_error(StatusCode::CONFLICT, &status),
        _ => status_error(StatusCode::CONFLICT, "oauth flow is not pending"),
    }
}

/// `url.Parse(raw).RawQuery`, or `None` where Go's parser fails.
fn redirect_query(raw: &str) -> Option<String> {
    cpa_core::config::go_url::parse(raw)?;
    let without_fragment = raw.split('#').next().unwrap_or_default();
    Some(
        without_fragment
            .split_once('?')
            .map(|(_, q)| q.to_owned())
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_causes_drop_upstream_bodies_and_descriptions() {
        for (cause, want) in [
            (
                r#"xai device token request failed with status 500: {"access_token":"sentinel"}"#,
                "xai device token request failed with status 500",
            ),
            (
                "xai device token error: invalid_grant: sentinel-description",
                "xai device token error: invalid_grant",
            ),
            ("xai device token error: weird", "xai device token error: weird"),
            (
                "meta device flow failed (HTTP 401): sentinel-body",
                "meta device flow failed (HTTP 401)",
            ),
            (
                "meta auth: error from authorization server: server_error: sentinel",
                "meta auth: error from authorization server: server_error",
            ),
            ("kimi: OAuth error: bad_code - sentinel", "kimi: OAuth error: bad_code"),
            ("kimi: OAuth error: sentinel$token!", "kimi: OAuth error: sentinel"),
            ("xai device code expired", "xai device code expired"),
            // A later marker inside the description must not decide the cut.
            (
                "kimi: OAuth error: invalid_grant - SENTINEL with status 500",
                "kimi: OAuth error: invalid_grant",
            ),
            (
                "xai device token error: x: SENTINEL (HTTP 500)",
                "xai device token error: x",
            ),
            // Decoder diagnostics quote response values.
            (
                r#"failed to parse token response: invalid type: string "SENTINEL", expected i64"#,
                "failed to parse token response",
            ),
            (
                "xai device token: parse response: json: cannot unmarshal number 123456789012345678901234567890",
                "xai device token: parse response",
            ),
            (
                "meta auth: parse mint response: number 99999999999999999999",
                "meta auth: parse mint response",
            ),
        ] {
            assert_eq!(redact_upstream(cause), want);
        }
        assert_eq!(
            with_cause(
                "Authentication failed",
                "kimi: device code request failed with status 400: body"
            ),
            "Authentication failed: kimi: device code request failed with status 400"
        );
    }

    #[test]
    fn states_and_providers_follow_go() {
        assert!(valid_state("kmi-ai-1759.x_y"));
        for bad in ["", "a/b", "a\\b", "a..b", "sp ace", &"x".repeat(129)] {
            assert!(!valid_state(bad), "{bad}");
        }
        assert_eq!(normalize_callback_provider(" Claude ").as_deref(), Some("anthropic"));
        assert_eq!(normalize_callback_provider("grok").as_deref(), Some("xai"));
        assert_eq!(normalize_callback_provider("kimi").as_deref(), Some("kimi"));
        assert_eq!(normalize_callback_provider("kimi_ai"), None);
    }

    #[test]
    fn sessions_follow_go_lifecycle() {
        let s = Sessions::default();
        s.register("st", "codex");
        assert!(s.is_pending("st", "CODEX"));
        assert!(s.deliver("openai", "st", "c", ""));
        assert_eq!(s.take_callback("st").map(|c| c.code).as_deref(), Some("c"));
        s.set_error("st", " ");
        assert_eq!(
            s.get("st"),
            Some(("codex".into(), "Authentication failed".into(), false))
        );
        assert!(!s.cancel("st"), "an errored session is not cancellable");
        assert!(!s.deliver("codex", "st", "c", ""));
        s.register("done", "meta");
        s.complete("done");
        assert_eq!(s.get("done"), Some(("meta".into(), String::new(), true)));
        assert!(!s.cancel("done"));
        s.register("live", "kimi");
        assert!(s.cancel("live"));
        assert!(s.get("live").is_none());
    }

    #[test]
    fn forwarder_redirect_keeps_the_query() {
        let r = String::from_utf8(redirect_bytes(
            "GET",
            "http://127.0.0.1:8317/codex/callback",
            Some("code=a&state=b"),
        ))
        .unwrap();
        assert!(r.starts_with("HTTP/1.1 302 Found\r\n"), "{r}");
        assert!(
            r.contains("\r\nLocation: http://127.0.0.1:8317/codex/callback?code=a&state=b\r\n"),
            "{r}"
        );
        assert!(r.contains("\r\nCache-Control: no-store\r\n"), "{r}");
        assert!(r.ends_with("\">Found</a>.\n\n"), "{r}");
        let post = String::from_utf8(redirect_bytes("POST", "http://h/cb", None)).unwrap();
        assert!(
            post.ends_with("Content-Length: 0\r\nConnection: close\r\n\r\n"),
            "{post}"
        );
    }

    /// A client that never finishes its request head is dropped after Go's 5 s
    /// header deadline; a complete one gets the redirect.
    #[tokio::test]
    async fn forwarder_connections_have_go_deadlines() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((s, _)) = listener.accept().await {
                tokio::spawn(forward(s, "http://127.0.0.1:1/x".into()));
            }
        });
        let mut slow = tokio::net::TcpStream::connect(addr).await.unwrap();
        slow.write_all(b"GET /?a=1 HTTP/1.1\r\nHost: x\r\n").await.unwrap();
        let mut buf = Vec::new();
        let closed = tokio::time::timeout(Duration::from_secs(8), slow.read_to_end(&mut buf)).await;
        assert!(matches!(closed, Ok(Ok(0))), "dropped without an answer");
        let mut ok = tokio::net::TcpStream::connect(addr).await.unwrap();
        ok.write_all(b"GET /cb?code=c HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut out = String::new();
        ok.read_to_string(&mut out).await.unwrap();
        assert!(out.contains("Location: http://127.0.0.1:1/x?code=c\r\n"), "{out}");
    }
}
