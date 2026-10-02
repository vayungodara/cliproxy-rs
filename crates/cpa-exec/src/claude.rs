//! Claude executor: sends Anthropic Messages requests with a Claude OAuth token.
//!
//! The request body is forwarded byte for byte. Response bodies are returned unread so
//! callers can stream them.

use bytes::Bytes;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

// ponytail: fixed baseline headers. The Claude Code wire profile (per-request beta
// policy, device profile, Node TLS fingerprint, system/tool cloak) is the M1 port of
// claude_executor_request.go and claude_executor_cloaking.go. Do not point this at a
// real subscription until that lands.
const BASELINE_BETAS: &str = "claude-code-20250219,oauth-2025-04-20";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    Messages,
    CountTokens,
}

impl Endpoint {
    fn path(self) -> &'static str {
        match self {
            Endpoint::Messages => "/v1/messages",
            Endpoint::CountTokens => "/v1/messages/count_tokens",
        }
    }
}

pub struct ClaudeExecutor {
    client: wreq::Client,
    base_url: String,
}

impl ClaudeExecutor {
    pub fn new(base_url: impl Into<String>) -> wreq::Result<Self> {
        Ok(Self {
            client: wreq::Client::builder().build()?,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        })
    }

    pub async fn send(&self, access_token: &str, endpoint: Endpoint, body: Bytes) -> wreq::Result<wreq::Response> {
        self.client
            .post(format!("{}{}?beta=true", self.base_url, endpoint.path()))
            .header("authorization", format!("Bearer {access_token}"))
            .header("content-type", "application/json")
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("anthropic-beta", BASELINE_BETAS)
            .body(body)
            .send()
            .await
    }
}
