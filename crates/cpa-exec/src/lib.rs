//! Upstream executors. One module per provider; each owns its wire format, auth headers
//! and HTTP client profile. [`Executors`] dispatches on `Credential::provider`.

pub mod claude;
mod upstream;

use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, FailureScope};

pub struct Executors {
    pub claude: claude::ClaudeExecutor,
}

impl Executors {
    pub async fn execute(
        &self,
        credential: &Credential,
        req: ExecRequest,
    ) -> Result<ExecResponse, ExecError> {
        match credential.provider.as_str() {
            "claude" => self.claude.execute(credential, req).await,
            other => Err(ExecError::local(
                500,
                FailureScope::Credential,
                format!("no executor for provider {other:?}"),
            )),
        }
    }
}
