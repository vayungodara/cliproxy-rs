//! Claude executor: Anthropic Messages with a Claude OAuth token.

use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, FailureScope, Operation, ResponseBody};
use cpa_core::format::Format;

use crate::oauth::{self, OAuth};
use crate::upstream::{into_response, transport_error};
use crate::{quota, tokens, translate, wire};

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
            .attributes
            .get("api_key")
            .map(String::as_str)
            .filter(|t| !t.is_empty())
            .or_else(|| credential.str("access_token"))
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
    oauth: OAuth,
}

impl ClaudeExecutor {
    pub fn new(base_url: impl Into<String>) -> wreq::Result<Self> {
        let base_url = base_url.into();
        let client = if tokens::first_party(&base_url) {
            crate::tls::client(false)?
        } else {
            wreq::Client::builder()
                .http1_only()
                .redirect(wreq::redirect::Policy::none())
                .build()?
        };
        Ok(Self::with_client(client, base_url).with_oauth(OAuth::new(crate::tls::client(true)?)))
    }

    /// Uses a caller-built client. The differential harness passes one with test trust
    /// roots and dial overrides so the logical URL, Host and SNI stay first-party.
    pub fn with_client(client: wreq::Client, base_url: impl Into<String>) -> Self {
        Self {
            oauth: OAuth::new(client.clone()),
            client,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    /// A separate OAuth transport hook prevents local harnesses from reaching real endpoints.
    pub fn with_oauth(mut self, oauth: OAuth) -> Self {
        self.oauth = oauth;
        self
    }

    pub fn needs_prepare(&self, credential: &Credential, _cfg: &Config) -> bool {
        oauth::needs_prepare(credential)
    }

    pub async fn prepare(&self, credential: &Credential, _cfg: &Config) -> Result<MetadataPatch, ExecError> {
        self.oauth.prepare(credential).await
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
        if req.response_format != Format::Claude && cpa_translate::pair(req.response_format, Format::Claude).is_none() {
            return Err(ExecError::local(
                501,
                FailureScope::Request,
                "Claude response translation pair is not registered",
            ));
        }
        let body = translate::request(&req)?;
        let base_url = credential
            .attributes
            .get("base_url")
            .filter(|s| !s.is_empty())
            .map(String::as_str)
            .unwrap_or(&self.base_url)
            .trim_end_matches('/');
        if req.operation == Operation::CountTokens && !tokens::first_party(base_url) {
            let response = ExecResponse {
                status: 200,
                headers: Default::default(),
                body: ResponseBody::Buffered(tokens::count(&body)?),
            };
            return translate::response(req, body, response).await;
        }
        let path = match req.operation {
            Operation::Generate => "/v1/messages",
            Operation::CountTokens => "/v1/messages/count_tokens",
        };
        let res = self
            .client
            .post(format!("{base_url}{path}?beta=true"))
            .redirect(wreq::redirect::Policy::none())
            .orig_headers(wire::order(wire::MESSAGES))
            .header("authorization", format!("Bearer {}", view.access_token))
            .header("content-type", "application/json")
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("anthropic-beta", BASELINE_BETAS)
            .header("accept-encoding", "identity")
            .body(body.clone())
            .send()
            .await
            .map_err(transport_error)?;
        let response = into_response(res).await.map_err(quota::classify)?;
        translate::response(req, body, response).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use cpa_core::exec::Caller;
    use std::path::Path;
    use std::time::Duration;

    #[tokio::test]
    async fn custom_origin_counts_locally_without_sending_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        // A first-party default must not override the credential's custom origin.
        let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
        let mut credential = Credential::from_file(
            Path::new("/fake"),
            Path::new("/fake/claude.json"),
            serde_json::json!({"type":"claude","access_token":"sk-ant-oat-fake"})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        credential.attributes.insert("base_url".into(), base);
        let body = Bytes::from_static(br#"{"model":"claude","messages":[{"role":"user","content":"Hello."}]}"#);
        let req = ExecRequest {
            operation: Operation::CountTokens,
            source_format: Format::Claude,
            response_format: Format::Claude,
            requested_model: "claude".into(),
            model: "claude".into(),
            original_body: body.clone(),
            body,
            stream: false,
            alt: None,
            session: None,
            execution_session: None,
            headers: Default::default(),
            caller: Caller {
                principal: "fake-client".into(),
                source: "x-api-key",
            },
        };
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            executor.execute(&credential, req, &Config::parse("").unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status, 200);
        let ResponseBody::Buffered(body) = response.body else {
            panic!("count is buffered")
        };
        assert_eq!(body, br#"{"input_tokens":4}"#.as_slice());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }
}
