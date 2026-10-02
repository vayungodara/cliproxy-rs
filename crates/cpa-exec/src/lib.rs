//! Upstream executors. One module per provider; each owns its wire format, auth headers
//! and HTTP client profile. [`Executors`] dispatches on `Credential::provider`.
//!
//! Each call gets the config snapshot current when the request started, so a hot reload
//! never changes behaviour halfway through one execution.
//!
//! Credential preparation (token refresh, device identity creation) is a two-step
//! contract so the runtime can single-flight it: [`Executors::needs_prepare`] is a cheap
//! check, and [`Executors::prepare`] does the work and returns a [`MetadataPatch`] that
//! the runtime commits. Executors never write credential files themselves.

pub mod claude;
pub mod codex;
mod codex_json;
pub mod codex_oauth;
pub mod codex_quota;
mod codex_request;
mod codex_response;
mod codex_ws;
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
mod quota;
mod tls;
mod tokens;
mod translate;
mod upstream;
mod wire;

use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch};
/// `clienterror.IsRequestFault`: the caller's request is wrong and no credential can fix
/// it. Handlers use it to decide which upstream errors reach the client.
pub use codex_response::request_fault;
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, ExecSession, FailureScope};

pub struct Executors {
    pub claude: claude::ClaudeExecutor,
    pub codex: codex::CodexExecutor,
    /// Device-login providers (Kimi, Meta, Devin). `Default` builds production clients.
    pub devices: DeviceExecutors,
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
            other => Err(no_executor(other)),
        }
    }

    /// One turn of a downstream Responses WebSocket session. Codex credentials with
    /// `websockets` keep a pooled upstream socket per session; every other credential runs
    /// an ordinary execution and cannot continue upstream state, so a continuation turn
    /// fails with [`ExecError::replay_required`].
    pub async fn execute_in_session(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
        session: &ExecSession,
    ) -> Result<ExecResponse, ExecError> {
        match credential.provider.as_str() {
            "codex" => self.codex.execute_in_session(credential, req, cfg, session).await,
            _ if session.continuation => Err(ExecError::replay_required()),
            _ => self.execute(credential, req, cfg).await,
        }
    }

    /// Whether `credential` keeps upstream conversation state on a session socket, so
    /// the next turn may be sent as an incremental continuation.
    pub fn session_upstream(&self, credential: &Credential) -> bool {
        credential.provider == "codex" && codex::CodexExecutor::upstream_websocket(credential)
    }

    /// Resolves when an upstream socket held for session `id` is lost; pending forever
    /// when no executor holds one.
    pub fn session_closed(&self, id: &str) -> impl std::future::Future<Output = ExecError> + Send + 'static {
        self.codex.session_closed(id)
    }

    /// Releases everything executors hold for session `id`.
    pub fn close_session(&self, id: &str) {
        self.codex.close_session(id);
    }

    /// Whether `credential` must be prepared before use. Must be cheap and side-effect free.
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
