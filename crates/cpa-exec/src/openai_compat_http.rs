//! The one place the OpenAI-compatible and xAI executors reach helpers other threads own.
//! Each function is an adapter: integration swaps its body for the shared module and the
//! Go-derived executor tests rerun unchanged.

use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::ExecError;
use http::HeaderMap;

use crate::kimi_http;

pub(crate) use crate::kimi_http::{GoHeaders, Upstream};

// ponytail: adapter for crates/cpa-exec/src/proxy.rs (owner: Claude thread); the
// interim implementation is kimi_http's proxy-keyed client cache (credential proxy_url,
// then requests.proxy-url, `direct`/`none`, environment default).
pub(crate) struct Clients(kimi_http::Clients);

impl Clients {
    pub(crate) fn new(default: wreq::Client) -> Self {
        Self(kimi_http::Clients::new(default))
    }

    pub(crate) fn for_credential(&self, credential: &Credential, cfg: &Config) -> wreq::Client {
        self.0.get(&kimi_http::proxy_url(credential, cfg))
    }
}

impl Default for Clients {
    fn default() -> Self {
        Self::new(kimi_http::default_client())
    }
}

// ponytail: adapter for proxy.rs's Go `http.Client.Do` (owner: Claude thread), interim
// kimi_http::send. That takes a UTF-8 body; binary multipart uploads go out in one hop
// without Go's redirect handling until the shared sender accepts bytes.
pub(crate) async fn send(
    client: &wreq::Client,
    url: &str,
    headers: GoHeaders,
    body: Bytes,
) -> Result<Upstream, ExecError> {
    match String::from_utf8(body.to_vec()) {
        Ok(text) => kimi_http::send(client, url, headers, text, None).await,
        Err(_) => {
            use futures_util::StreamExt;
            let (builder, _) = headers.apply(client.post(url));
            let response = builder
                .body(body)
                .send()
                .await
                .map_err(crate::upstream::transport_error)?;
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            let body = response
                .bytes_stream()
                .map(|r| r.map_err(crate::upstream::transport_error))
                .boxed();
            Ok(Upstream { status, headers, body })
        }
    }
}

/// Go `io.ReadAll` of a response body; read errors fail the call.
pub(crate) async fn read_all(upstream: Upstream) -> Result<Bytes, ExecError> {
    kimi_http::read_all(upstream.body, usize::MAX, false).await
}

/// Go `io.ReadAll` of an error body, bounded, read errors ignored.
pub(crate) async fn read_error_body(upstream: Upstream) -> (u16, HeaderMap, Bytes) {
    let status = upstream.status;
    let headers = upstream.headers.clone();
    let body = kimi_http::read_all(upstream.body, kimi_http::MAX_ERROR_BODY, true)
        .await
        .unwrap_or_default();
    (status, headers, body)
}

/// Go `bufio.Scanner` lines with the executor's token limit.
pub(crate) fn lines(
    upstream: Upstream,
    max: usize,
) -> futures_util::stream::BoxStream<'static, Result<Bytes, ExecError>> {
    kimi_http::lines(upstream.body, max)
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

// ponytail: adapter for cpa_common::thinking::parse_suffix (owner: Google thread),
// interim kimi_thinking.
pub(crate) fn parse_suffix(model: &str) -> (&str, Option<&str>) {
    crate::kimi_thinking::parse_suffix(model)
}

/// What `helps.ApplyRequestThinking` needs to know about the attempt.
pub(crate) struct ThinkingInput<'a> {
    /// Translated target body.
    pub body: String,
    /// Model with any thinking suffix.
    pub model: &'a str,
    pub from: &'a str,
    pub to: &'a str,
    /// Executor identifier (the credential's provider key).
    pub provider: &'a str,
}

// ponytail: adapter for cpa_common::thinking ApplyThinking (owner: Google thread). Returns
// the body unchanged, which is Go's result whenever neither the body nor a model suffix
// carries thinking intent. Fixtures that exercise thinking are gated with
// `needs: thinking` until the shared module replaces this body; the error is
// `ThinkingError` (HTTP 400 with its message).
pub(crate) fn apply_thinking(input: ThinkingInput<'_>) -> Result<String, ExecError> {
    let _ = (input.model, input.from, input.to, input.provider);
    Ok(input.body)
}

// ponytail: adapter for cpa_common::payload (owner: server thread; Go
// helps.ApplyPayloadConfigWithRequest, M4-0031). Identity until payload rules land.
pub(crate) fn apply_payload_rules(body: String, _cfg: &Config) -> String {
    body
}

// ponytail: adapter for cpa_common::signature InspectGPTReasoningSignature (owner:
// Google thread). Accepts every signature; the `needs: signature` fixture covers the
// rejection path once the shared module lands.
pub(crate) fn inspect_gpt_reasoning_signature(_signature: &str) -> Result<(), String> {
    Ok(())
}

/// JSON edits with tidwall/sjson semantics.
// ponytail: adapter for cpa_common::json (owner: translator thread), interim kimi_json.
// sjson errors leave the body unchanged; Go call sites that would nil the body on an
// sjson error only see bodies that already parsed as JSON objects.
pub(crate) mod json {
    use crate::kimi_json;

    pub(crate) fn set_raw(json: &str, path: &str, raw: &str) -> String {
        kimi_json::set_raw(json, path, raw).unwrap_or_else(|_| json.to_owned())
    }

    pub(crate) fn set_str(json: &str, path: &str, value: &str) -> String {
        kimi_json::set_str(json, path, value).unwrap_or_else(|_| json.to_owned())
    }

    pub(crate) fn delete(json: &str, path: &str) -> String {
        kimi_json::delete(json, path)
    }

    /// gjson `Result.String()`.
    pub(crate) fn string(json: &str, path: &str) -> String {
        kimi_json::gstr(&gjson::get(json, path))
    }

    /// gjson `Result.Bool()`.
    pub(crate) fn boolean(value: &gjson::Value<'_>) -> bool {
        match value.kind() {
            gjson::Kind::True => true,
            gjson::Kind::String => matches!(value.str(), "1" | "t" | "T" | "TRUE" | "true" | "True"),
            gjson::Kind::Number => value.f64() != 0.0,
            _ => false,
        }
    }

    /// `helps.SetStringIfDifferent`.
    pub(crate) fn set_str_if_different(json: &str, path: &str, value: &str) -> String {
        let current = gjson::get(json, path);
        if current.kind() == gjson::Kind::String && current.str() == value {
            return json.to_owned();
        }
        set_str(json, path, value)
    }

    /// `helps.SetBoolIfDifferent`.
    pub(crate) fn set_bool_if_different(json: &str, path: &str, value: bool) -> String {
        let current = gjson::get(json, path).kind();
        if (value && current == gjson::Kind::True) || (!value && current == gjson::Kind::False) {
            return json.to_owned();
        }
        set_raw(json, path, if value { "true" } else { "false" })
    }
}
