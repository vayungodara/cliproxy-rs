//! The one place the OpenAI-compatible and xAI executors reach helpers other threads own.
//! Each function is an adapter: integration swaps its body for the shared module and the
//! Go-derived executor tests rerun unchanged.

use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::ExecError;
use http::HeaderMap;

use crate::kimi_http;
use crate::proxy::{GoClients, Hooks, Proxy};

pub(crate) use crate::proxy::{GoHeaders, Upstream};

/// Go standard-transport clients keyed by effective proxy (crate::proxy, M4-0029).
pub(crate) struct Clients(GoClients);

impl Clients {
    pub(crate) fn new(default: wreq::Client) -> Self {
        Self(GoClients::with_default(default))
    }

    pub(crate) fn for_credential(&self, credential: &Credential, cfg: &Config) -> wreq::Client {
        self.0.get(&Proxy::effective(credential, cfg))
    }
}

impl Default for Clients {
    fn default() -> Self {
        Self(GoClients::new(Hooks::default()))
    }
}

/// Go `http.Client.Do` for one POST, redirects included.
pub(crate) async fn send(
    client: &wreq::Client,
    url: &str,
    headers: GoHeaders,
    body: Bytes,
) -> Result<Upstream, ExecError> {
    crate::proxy::send(client, url, headers, body, None).await
}

/// Go `io.ReadAll` of a response body; read errors fail the call.
pub(crate) async fn read_all(upstream: Upstream) -> Result<Bytes, ExecError> {
    crate::proxy::read_all(upstream.body, usize::MAX, false).await
}

/// Go `io.ReadAll` of an error body, bounded, read errors ignored.
pub(crate) async fn read_error_body(upstream: Upstream) -> (u16, HeaderMap, Bytes) {
    let status = upstream.status;
    let headers = upstream.headers.clone();
    let body = crate::proxy::read_all(upstream.body, crate::proxy::MAX_ERROR_BODY, true)
        .await
        .unwrap_or_default();
    (status, headers, body)
}

/// Go `bufio.Scanner` lines with the executor's token limit.
pub(crate) fn lines(
    upstream: Upstream,
    max: usize,
) -> futures_util::stream::BoxStream<'static, Result<Bytes, ExecError>> {
    crate::proxy::lines(upstream.body, max)
}

// ponytail: adapter for cpa_common::headers (owner: server thread), interim
// kimi_http::custom_headers (Go util.ApplyCustomHeadersFromAttrs).
pub(crate) fn custom_headers(
    credential: &Credential,
    inbound: &HeaderMap,
    session: Option<&str>,
) -> Vec<(String, String)> {
    kimi_http::custom_headers(credential, inbound, session)
}

// ponytail: adapter for cpa_common::payload (owner: server thread; Go
// helps.ApplyPayloadConfigWithRequest, M4-0031). Identity until payload rules land.
pub(crate) fn apply_payload_rules(body: Vec<u8>, _cfg: &Config) -> Vec<u8> {
    body
}
