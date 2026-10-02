//! Claude executor: Anthropic Messages with a Claude OAuth token.

use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, FailureScope, Operation};
use cpa_core::format::Format;

use crate::upstream::{into_response, transport_error};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

// ponytail: fixed baseline headers. The Claude Code wire profile (per-request beta
// policy, device profile, Node TLS fingerprint, system/tool cloak) is the M1 port of
// claude_executor_request.go and claude_executor_cloaking.go. Do not point this at a
// real subscription until that lands.
const BASELINE_BETAS: &str = "claude-code-20250219,oauth-2025-04-20";

/// The fields of a `"type": "claude"` credential this executor needs.
pub struct ClaudeView<'a> {
    pub access_token: &'a str,
    pub email: &'a str,
}

impl<'a> ClaudeView<'a> {
    pub fn new(credential: &'a Credential) -> Result<Self, ExecError> {
        let access_token = credential
            .str("access_token")
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ExecError::local(401, FailureScope::Credential, "claude credential has no access_token"))?;
        Ok(Self {
            access_token,
            email: credential.str("email").unwrap_or_default(),
        })
    }
}

pub struct ClaudeExecutor {
    client: wreq::Client,
    base_url: String,
}

impl ClaudeExecutor {
    pub fn new(base_url: impl Into<String>) -> wreq::Result<Self> {
        Ok(Self::with_client(wreq::Client::builder().build()?, base_url))
    }

    /// Uses a caller-built client. The differential harness passes one with test trust
    /// roots and dial overrides so the logical URL, Host and SNI stay first-party.
    pub fn with_client(client: wreq::Client, base_url: impl Into<String>) -> Self {
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    // ponytail: never prepares. The OAuth port refreshes near `expired` and creates the
    // device identity pool (anthropic_auth.go, claude_executor_auth.go) here.
    pub fn needs_prepare(&self, _credential: &Credential, _cfg: &Config) -> bool {
        false
    }

    pub async fn prepare(&self, _credential: &Credential, _cfg: &Config) -> Result<MetadataPatch, ExecError> {
        Ok(MetadataPatch::default())
    }

    pub async fn execute(
        &self,
        credential: &Credential,
        req: ExecRequest,
        _cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        let view = ClaudeView::new(credential)?;
        if req.alt.as_deref() == Some("responses/compact") {
            return Err(ExecError::local(
                501,
                FailureScope::Request,
                "/responses/compact not supported",
            ));
        }
        if req.source_format != Format::Claude || req.response_format != Format::Claude {
            // Translated traffic arrives with the translator port (cpa-translate pairs).
            return Err(ExecError::local(
                501,
                FailureScope::Request,
                "only Claude-format requests are supported yet",
            ));
        }
        // ponytail: count_tokens always goes upstream. CLIProxyAPI estimates locally
        // for non-first-party origins (claude_executor_tokens.go).
        let path = match req.operation {
            Operation::Generate => "/v1/messages",
            Operation::CountTokens => "/v1/messages/count_tokens",
        };
        let res = self
            .client
            .post(format!("{}{path}?beta=true", self.base_url))
            .header("authorization", format!("Bearer {}", view.access_token))
            .header("content-type", "application/json")
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("anthropic-beta", BASELINE_BETAS)
            .header("accept-encoding", "identity")
            .body(req.body)
            .send()
            .await
            .map_err(transport_error)?;
        into_response(res).await
    }
}
