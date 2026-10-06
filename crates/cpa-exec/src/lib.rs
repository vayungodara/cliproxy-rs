//! Upstream executors. One module per provider; each owns its wire format, auth headers
//! and HTTP client profile. [`Executors`] dispatches on `Credential::provider`.
//!
//! Each call gets the config snapshot current when the request started, so a hot reload
//! never changes behaviour halfway through one execution.
//!
//! Credential preparation (token refresh, device identity creation) is a two-step
//! contract so the runtime can single-flight it: [`Executors::readiness`] is a cheap
//! check, and [`Executors::prepare`] does the work and returns a [`MetadataPatch`] that
//! the runtime commits. Executors never write credential files themselves.
//!
//! [`Readiness`] separates the two reasons Go prepares a credential. Requests block only
//! on [`Readiness::PrepareNow`] (Go `RequestAuthPreparer`: Claude identity, Meta API-key
//! mint, Antigravity project ID). Token refresh is [`Readiness::RefreshSoon`]: the
//! background loop refreshes inside the provider's refresh lead while requests keep
//! using the current token, and an upstream 401 triggers one refresh-and-retry (Go
//! `tryRefreshAfterUnauthorized`). An already expired token is still `RefreshSoon`:
//! Go sends it and recovers on 401.

pub mod aistudio;
pub mod catalog_etag;
pub mod claude;
pub mod claude_login;
pub mod codex;
mod codex_capture;
pub mod codex_catalog_updater;
mod codex_client;
mod codex_images;
mod codex_json;
pub mod codex_live;
pub mod codex_oauth;
pub mod codex_quota;
mod codex_replay;
mod codex_request;
mod codex_response;
#[cfg(test)]
mod codex_testkit;
mod codex_tls;
mod codex_tokens;
mod codex_ws;
pub mod devin;
pub mod devin_auth;
pub mod devin_models;
mod devin_request;
mod devin_wire;
pub mod gemini;
mod gemini_payload;
mod gemini_stream;
mod home_replay;
pub mod kimi;
pub mod kimi_auth;
#[cfg(test)]
mod kimi_fixture;
mod kimi_http;
mod kimi_replay;
pub mod meta;
pub mod meta_auth;
mod meta_codex;
mod meta_wire;
pub mod oauth;
pub mod openai_compat;
mod openai_compat_go;
mod openai_compat_http;
pub mod openai_compat_multipart;
mod openai_compat_payload;
#[cfg(test)]
mod openai_compat_usage;
pub mod proxy;
mod quota;
mod rawjson;
mod replay;
#[cfg(test)]
mod test_tls;
mod tls;
mod tokenizer;
mod tokens;
mod translate;
mod upstream;
pub mod vertex;
pub mod vertex_auth;
mod wire;
pub mod wsrelay;
pub mod xai;
pub mod xai_auth;
mod xai_replay;
mod xai_request;
mod xai_response;
pub mod xai_url;
mod xai_ws;

use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, ExecSession, FailureScope};

/// Go `helps.EnsureResponsesUsageDetails`, for paths outside the executors (plugin
/// executors' Responses payloads and stream chunks).
pub use openai_compat_payload::ensure_responses_usage_details;

/// What a credential needs before or around use. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// Usable as is; nothing to refresh.
    Ready,
    /// Usable now; the background refresh loop should prepare it.
    RefreshSoon,
    /// Unusable until prepared; requests wait on the single-flighted preparation.
    PrepareNow,
}

pub struct Executors {
    pub claude: claude::ClaudeExecutor,
    pub codex: codex::CodexExecutor,
    /// Device-login providers (Kimi, Meta, Devin). `Default` builds production clients.
    pub devices: DeviceExecutors,
    /// API-key upstreams speaking OpenAI wire formats (OpenAI-compatible providers, xAI).
    pub openai: OpenAIExecutors,
    /// Google-family providers (Gemini API keys, native Interactions keys).
    pub google: GoogleExecutors,
}

/// Google-family executors, grouped like [`DeviceExecutors`].
#[derive(Default)]
pub struct GoogleExecutors {
    /// `gemini` and `gemini-interactions` API keys.
    pub gemini: gemini::GeminiExecutor,
    /// `vertex` service accounts and API keys.
    pub vertex: vertex::VertexExecutor,
    /// `aistudio` sessions on the `/v1/ws` relay, which the server route shares.
    pub aistudio: aistudio::AiStudioExecutor,
}

/// OpenAI-wire executors, grouped like [`DeviceExecutors`].
#[derive(Default)]
pub struct OpenAIExecutors {
    pub compat: openai_compat::OpenAICompatExecutor,
    pub xai: xai::XaiExecutor,
}

/// Executors for the device-login providers, grouped so adding one does not touch every
/// [`Executors`] construction site.
#[derive(Default)]
pub struct DeviceExecutors {
    pub kimi: kimi::KimiExecutor,
    pub meta: meta::MetaExecutor,
    pub devin: devin::DevinExecutor,
}

impl Executors {
    pub async fn execute(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        match credential.provider.as_str() {
            "claude" => self.claude.execute(credential, req, cfg).await,
            "codex" => self.codex.execute(credential, req, cfg).await,
            p if kimi::PROVIDERS.contains(&p) => self.devices.kimi.execute(&self.claude, credential, req, cfg).await,
            meta::PROVIDER => self.devices.meta.execute(credential, req, cfg).await,
            devin::PROVIDER => self.devices.devin.execute(credential, req, cfg).await,
            p if openai_compat::handles(p) => self.openai.compat.execute(credential, req, cfg).await,
            p if gemini::handles(p) => self.google.gemini.execute(credential, req, cfg).await,
            p if vertex::handles(p) => self.google.vertex.execute(credential, req, cfg).await,
            p if aistudio::handles(p) => self.google.aistudio.execute(credential, req, cfg).await,
            xai::PROVIDER => self.openai.xai.execute(credential, req, cfg, false).await,
            other => Err(no_executor(other)),
        }
    }

    /// The Images API (`/v1/images/generations`, `/v1/images/edits`) for providers that
    /// serve it directly. `request_path` is the inbound route; `req.stream` asks for the
    /// upstream event stream passed through raw.
    pub async fn images(
        &self,
        credential: &Credential,
        req: ExecRequest,
        request_path: &str,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        match credential.provider.as_str() {
            "codex" => self.codex.images(credential, req, request_path, cfg).await,
            p if openai_compat::handles(p) => self.openai.compat.images(credential, req, request_path, cfg).await,
            xai::PROVIDER => self.openai.xai.images(credential, req, request_path, cfg).await,
            other => Err(no_executor(other)),
        }
    }

    /// The Videos API (`/v1/videos*`, `/openai/v1/videos*`) for providers that serve it.
    /// `request_path` is the inbound route; a route other than generations, edits or
    /// extensions polls the job named by the body's `request_id`.
    pub async fn videos(
        &self,
        credential: &Credential,
        req: ExecRequest,
        request_path: &str,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        match credential.provider.as_str() {
            xai::PROVIDER => self.openai.xai.videos(credential, req, request_path, cfg).await,
            other => Err(no_executor(other)),
        }
    }

    /// One turn of a downstream Responses WebSocket session. Codex and xAI credentials
    /// with `websockets` keep a pooled upstream socket per session; every other credential
    /// runs an ordinary execution and cannot continue upstream state, so a continuation
    /// turn fails with [`ExecError::replay_required`].
    pub async fn execute_in_session(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
        session: &ExecSession,
    ) -> Result<ExecResponse, ExecError> {
        match credential.provider.as_str() {
            "codex" => self.codex.execute_in_session(credential, req, cfg, session).await,
            xai::PROVIDER => self.openai.xai.execute_in_session(credential, req, cfg, session).await,
            _ if session.continuation => Err(ExecError::replay_required()),
            _ => self.execute(credential, req, cfg).await,
        }
    }

    /// Whether `credential` keeps upstream conversation state on a session socket, so
    /// the next turn may be sent as an incremental continuation.
    pub fn session_upstream(&self, credential: &Credential) -> bool {
        match credential.provider.as_str() {
            "codex" => codex::CodexExecutor::upstream_websocket(credential),
            xai::PROVIDER => xai::XaiExecutor::session_upstream(credential),
            _ => false,
        }
    }

    /// Resolves when an upstream socket held for session `id` is lost; pending forever
    /// when no executor holds one.
    pub fn session_closed(&self, id: &str) -> impl std::future::Future<Output = ExecError> + Send + 'static {
        let codex = self.codex.session_closed(id);
        let xai = self.openai.xai.session_closed(id);
        async move {
            tokio::select! {
                error = codex => error,
                error = xai => error,
            }
        }
    }

    /// Releases everything executors hold for session `id`.
    pub fn close_session(&self, id: &str) {
        self.codex.close_session(id);
        self.openai.xai.close_session(id);
    }

    /// Whether an executor serves this provider. Credentials of other providers never
    /// enter selection (Go skips auths whose executor is not registered).
    pub fn supports(&self, provider: &str) -> bool {
        matches!(
            provider,
            "claude" | "codex" | meta::PROVIDER | xai::PROVIDER | devin::PROVIDER
        ) || kimi::PROVIDERS.contains(&provider)
            || openai_compat::handles(provider)
            || gemini::handles(provider)
            || vertex::handles(provider)
            || aistudio::handles(provider)
    }

    /// Go `authHasRefreshCredential`: whether an upstream 401 on `credential` should be
    /// followed by one forced [`Self::prepare`] and a retry (Go
    /// `tryRefreshAfterUnauthorized`). A refresh token qualifies for every provider;
    /// Meta re-mints its API key from its device token. Providers that recover from a
    /// 401 another way add an arm here.
    pub fn has_refresh_credential(&self, credential: &Credential) -> bool {
        let filled = |v: Option<&str>| v.is_some_and(|v| !v.trim().is_empty());
        if filled(credential.str("refresh_token")) || filled(credential.str("refreshToken")) {
            return true;
        }
        credential.provider.trim().eq_ignore_ascii_case("meta")
            && (filled(credential.str("dca_token"))
                || filled(credential.attributes.get("dca_token").map(String::as_str)))
    }

    /// Whether `credential` needs preparation, and whether requests must wait for it.
    /// Must be cheap and side-effect free. Providers without request-time preparation
    /// report `RefreshSoon` whenever `needs_prepare` holds.
    pub fn readiness(&self, credential: &Credential, cfg: &Config) -> Readiness {
        if !self.needs_prepare(credential, cfg) {
            return Readiness::Ready;
        }
        match credential.provider.as_str() {
            "claude" if oauth::needs_identity(credential) => Readiness::PrepareNow,
            meta::PROVIDER if self.devices.meta.must_mint(credential) => Readiness::PrepareNow,
            _ => Readiness::RefreshSoon,
        }
    }

    /// Whether `credential` needs any preparation (`readiness` is not `Ready`). Must be
    /// cheap and side-effect free. Providers implement this; the runtime reads
    /// [`Self::readiness`].
    pub fn needs_prepare(&self, credential: &Credential, cfg: &Config) -> bool {
        self.needs_prepare_at(credential, cfg, chrono::Utc::now())
    }

    /// [`Self::needs_prepare`] as of `now`. Once true it stays true as time passes until
    /// the credential is prepared: every provider compares an expiry or a last refresh
    /// with the clock.
    pub fn needs_prepare_at(&self, credential: &Credential, cfg: &Config, now: chrono::DateTime<chrono::Utc>) -> bool {
        match credential.provider.as_str() {
            "claude" => self.claude.needs_prepare_at(credential, cfg, now),
            "codex" => self.codex.needs_prepare_at(credential, cfg, now),
            p if kimi::PROVIDERS.contains(&p) => self.devices.kimi.needs_prepare_at(credential, cfg, now),
            meta::PROVIDER => self.devices.meta.needs_prepare_at(credential, cfg, now),
            xai::PROVIDER => self.openai.xai.needs_prepare_at(credential, cfg, now),
            devin::PROVIDER => self.devices.devin.needs_prepare_at(credential, cfg, now),
            _ => false,
        }
    }

    /// The earliest time within `horizon` of `now` at which `credential` needs
    /// preparation (`now` if it already does), to one second. Bisects
    /// [`Self::needs_prepare_at`], so every provider's own rule decides; about a dozen
    /// cheap evaluations for a credential that comes due inside the horizon, one for
    /// the rest.
    pub fn prepare_due(
        &self,
        credential: &Credential,
        cfg: &Config,
        now: chrono::DateTime<chrono::Utc>,
        horizon: chrono::Duration,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        if self.needs_prepare_at(credential, cfg, now) {
            return Some(now);
        }
        let (mut lo, mut hi) = (now, now + horizon);
        if !self.needs_prepare_at(credential, cfg, hi) {
            return None;
        }
        while hi - lo > chrono::Duration::seconds(1) {
            let mid = lo + (hi - lo) / 2;
            if self.needs_prepare_at(credential, cfg, mid) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        Some(hi)
    }

    /// Prepares `credential` and returns the metadata change to commit.
    pub async fn prepare(&self, credential: &Credential, cfg: &Config) -> Result<MetadataPatch, ExecError> {
        match credential.provider.as_str() {
            "claude" => self.claude.prepare(credential, cfg).await,
            "codex" => self.codex.prepare(credential, cfg).await,
            p if kimi::PROVIDERS.contains(&p) => self.devices.kimi.prepare(credential, cfg).await,
            meta::PROVIDER => self.devices.meta.prepare(credential, cfg).await,
            xai::PROVIDER => self.openai.xai.prepare(credential, cfg).await,
            devin::PROVIDER => self.devices.devin.prepare(credential, cfg).await,
            other => Err(no_executor(other)),
        }
    }
}

fn no_executor(provider: &str) -> ExecError {
    ExecError::local(
        500,
        FailureScope::Credential,
        format!("no executor for provider {provider:?}"),
    )
}

#[cfg(test)]
mod readiness_tests {
    use super::{Executors, Readiness, claude};
    use cpa_core::config::Config;
    use cpa_core::credential::Credential;
    use std::path::Path;

    fn credential(metadata: serde_json::Value) -> Credential {
        Credential::from_file(
            Path::new("/auth"),
            Path::new("/auth/a.json"),
            metadata.as_object().unwrap().clone(),
        )
        .unwrap()
    }

    fn rfc3339(offset: chrono::Duration) -> String {
        (chrono::Utc::now() + offset).to_rfc3339()
    }

    /// Requests block only on Go `ShouldPrepareRequestAuth` (Claude: an OAuth token
    /// without a canonical device pool or account UUID). Expiry, even past expiry, is a
    /// background refresh.
    #[test]
    fn only_missing_identity_blocks_requests() {
        let executors = Executors {
            claude: claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        };
        let cfg = Config::default();
        let pool = serde_json::json!(["a".repeat(64)]);
        let identified = |expired: String| {
            credential(serde_json::json!({
                "type": "claude",
                "access_token": "sk-ant-oat01-fake",
                "refresh_token": "fake-refresh",
                "claude_device_ids": pool,
                "account_uuid": "fake-account",
                "expired": expired,
            }))
        };
        let cases = [
            (identified(rfc3339(chrono::Duration::days(2))), Readiness::Ready),
            (identified(rfc3339(chrono::Duration::hours(1))), Readiness::RefreshSoon),
            (identified(rfc3339(-chrono::Duration::hours(1))), Readiness::RefreshSoon),
            (
                credential(serde_json::json!({
                    "type": "claude",
                    "access_token": "sk-ant-oat01-fake",
                    "refresh_token": "fake-refresh",
                    "expired": rfc3339(-chrono::Duration::hours(1)),
                })),
                Readiness::PrepareNow,
            ),
            (
                credential(serde_json::json!({"type": "claude", "access_token": "sk-ant-api03-fake"})),
                Readiness::Ready,
            ),
            (
                credential(serde_json::json!({
                    "type": "kimi",
                    "access_token": "fake-access",
                    "refresh_token": "fake-refresh",
                    "expired": rfc3339(-chrono::Duration::hours(1)),
                })),
                Readiness::RefreshSoon,
            ),
        ];
        for (i, (c, expected)) in cases.iter().enumerate() {
            assert_eq!(executors.readiness(c, &cfg), *expected, "case {i}");
        }
    }

    /// The refresh loop sleeps until `prepare_due`: each provider's own lead before
    /// expiry (Claude 4 h, Kimi 5 min), to the second, `now` once due, and nothing past
    /// the horizon.
    #[test]
    fn prepare_due_finds_each_providers_deadline() {
        let executors = Executors {
            claude: claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        };
        let cfg = Config::default();
        let now = chrono::Utc::now();
        let horizon = chrono::Duration::hours(1);
        let with_expiry = |provider: &str, expiry: chrono::DateTime<chrono::Utc>| {
            credential(serde_json::json!({
                "type": provider,
                "access_token": "fake-access",
                "refresh_token": "fake-refresh",
                "expired": expiry.to_rfc3339(),
            }))
        };
        let near = |got: Option<chrono::DateTime<chrono::Utc>>, want: chrono::DateTime<chrono::Utc>| {
            let got = got.expect("due inside the horizon");
            assert!((got - want).num_milliseconds().abs() <= 1000, "{got} vs {want}");
        };
        let offset = chrono::Duration::seconds(1337);
        let claude = with_expiry("claude", now + chrono::Duration::hours(4) + offset);
        near(executors.prepare_due(&claude, &cfg, now, horizon), now + offset);
        let kimi = with_expiry("kimi", now + chrono::Duration::minutes(5) + offset);
        near(executors.prepare_due(&kimi, &cfg, now, horizon), now + offset);
        let due = with_expiry("claude", now - chrono::Duration::hours(1));
        assert_eq!(executors.prepare_due(&due, &cfg, now, horizon), Some(now));
        let later = with_expiry("claude", now + chrono::Duration::hours(6));
        assert_eq!(executors.prepare_due(&later, &cfg, now, horizon), None);
        let api_key = credential(serde_json::json!({"type": "claude", "access_token": "sk-ant-api03-fake"}));
        assert_eq!(executors.prepare_due(&api_key, &cfg, now, horizon), None);
    }

    /// Go `authHasRefreshCredential`.
    #[test]
    fn refresh_credentials_follow_go() {
        let executors = Executors {
            claude: claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            openai: Default::default(),
            google: Default::default(),
            devices: Default::default(),
        };
        let has = |m: serde_json::Value| executors.has_refresh_credential(&credential(m));
        assert!(has(serde_json::json!({"type": "claude", "refresh_token": "fake"})));
        assert!(has(serde_json::json!({"type": "kimi", "refreshToken": "fake"})));
        assert!(!has(serde_json::json!({"type": "claude", "refresh_token": "  "})));
        assert!(has(serde_json::json!({"type": "meta", "dca_token": "fake"})));
        assert!(
            !has(serde_json::json!({"type": "claude", "dca_token": "fake"})),
            "dca_token is Meta's"
        );
        let mut meta = credential(serde_json::json!({"type": "meta"}));
        assert!(!executors.has_refresh_credential(&meta));
        meta.attributes.insert("dca_token".into(), "fake".into());
        assert!(executors.has_refresh_credential(&meta));
    }
}
