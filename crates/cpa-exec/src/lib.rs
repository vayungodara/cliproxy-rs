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
pub mod oauth;
mod quota;
mod tls;
mod tokens;
mod translate;
mod upstream;
mod wire;

use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, FailureScope};

pub struct Executors {
    pub claude: claude::ClaudeExecutor,
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
            other => Err(no_executor(other)),
        }
    }

    /// Whether `credential` must be prepared before use. Must be cheap and side-effect free.
    pub fn needs_prepare(&self, credential: &Credential, cfg: &Config) -> bool {
        match credential.provider.as_str() {
            "claude" => self.claude.needs_prepare(credential, cfg),
            _ => false,
        }
    }

    /// Prepares `credential` and returns the metadata change to commit.
    pub async fn prepare(&self, credential: &Credential, cfg: &Config) -> Result<MetadataPatch, ExecError> {
        match credential.provider.as_str() {
            "claude" => self.claude.prepare(credential, cfg).await,
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
