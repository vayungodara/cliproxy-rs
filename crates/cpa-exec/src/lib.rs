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

pub mod claude;
pub mod codex;
mod codex_json;
pub mod codex_oauth;
pub mod codex_quota;
mod codex_request;
mod codex_response;
#[cfg(test)]
mod codex_testkit;
pub mod kimi;
pub mod kimi_auth;
#[cfg(test)]
mod kimi_fixture;
mod kimi_http;
mod kimi_json;
mod kimi_replay;
mod kimi_thinking;
pub mod oauth;
pub mod openai_compat;
mod openai_compat_go;
mod openai_compat_http;
pub mod openai_compat_multipart;
mod openai_compat_payload;
pub mod proxy;
mod quota;
mod rawjson;
mod tls;
mod tokens;
mod translate;
mod upstream;
mod wire;

use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, FailureScope};

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
}

/// OpenAI-wire executors, grouped like [`DeviceExecutors`].
#[derive(Default)]
pub struct OpenAIExecutors {
    pub compat: openai_compat::OpenAICompatExecutor,
}

/// Executors for the device-login providers, grouped so adding one does not touch every
/// [`Executors`] construction site.
#[derive(Default)]
pub struct DeviceExecutors {
    pub kimi: kimi::KimiExecutor,
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
            p if openai_compat::handles(p) => self.openai.compat.execute(credential, req, cfg).await,
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
            p if openai_compat::handles(p) => self.openai.compat.images(credential, req, request_path, cfg).await,
            other => Err(no_executor(other)),
        }
    }

    /// Whether an executor serves this provider. Credentials of other providers never
    /// enter selection (Go skips auths whose executor is not registered).
    pub fn supports(&self, provider: &str) -> bool {
        matches!(provider, "claude" | "codex")
            || kimi::PROVIDERS.contains(&provider)
            || openai_compat::handles(provider)
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
            _ => Readiness::RefreshSoon,
        }
    }

    /// Whether `credential` needs any preparation (`readiness` is not `Ready`). Must be
    /// cheap and side-effect free. Providers implement this; the runtime reads
    /// [`Self::readiness`].
    pub fn needs_prepare(&self, credential: &Credential, cfg: &Config) -> bool {
        match credential.provider.as_str() {
            "claude" => self.claude.needs_prepare(credential, cfg),
            "codex" => self.codex.needs_prepare(credential, cfg),
            p if kimi::PROVIDERS.contains(&p) => self.devices.kimi.needs_prepare(credential, cfg),
            _ => false,
        }
    }

    /// Prepares `credential` and returns the metadata change to commit.
    pub async fn prepare(&self, credential: &Credential, cfg: &Config) -> Result<MetadataPatch, ExecError> {
        match credential.provider.as_str() {
            "claude" => self.claude.prepare(credential, cfg).await,
            "codex" => self.codex.prepare(credential, cfg).await,
            p if kimi::PROVIDERS.contains(&p) => self.devices.kimi.prepare(credential, cfg).await,
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
}
