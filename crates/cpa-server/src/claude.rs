//! Anthropic Messages routes (sdk/api/handlers/claude/code_handlers.go).

use std::sync::Arc;

use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::extract::rejection::BytesRejection;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use cpa_core::exec::{Caller, ExecError, ExecRequest, Operation, ResponseBody};
use cpa_core::format::Format;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;

use crate::runtime::{Completing, Outcome, Runtime};

pub async fn messages(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    handle(rt, caller, headers, body, Operation::Generate).await
}

pub async fn count_tokens(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    handle(rt, caller, headers, body, Operation::CountTokens).await
}

/// The two fields the handler reads. Like gjson in Go, a body that is not JSON is not
/// rejected here; upstream decides.
#[derive(Deserialize, Default)]
struct Peek {
    #[serde(default)]
    model: String,
    #[serde(default)]
    stream: bool,
}

async fn handle(
    rt: Arc<Runtime>,
    caller: Caller,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
    operation: Operation,
) -> Response {
    let body = match body {
        Ok(body) => body,
        Err(rejection) => {
            return claude_error(rejection.status().as_u16(), &rejection.body_text(), None);
        }
    };
    let peek: Peek = serde_json::from_slice(&body).unwrap_or_default();
    let Some(lease) = rt.store().select("claude") else {
        let model = if peek.model.is_empty() {
            "unknown"
        } else {
            &peek.model
        };
        let message = format!(
            "auth_not_found: no auth available (providers=claude, model={model}); \
             check Claude auth/key session and cooldown state via /v0/management/auth-files"
        );
        return claude_error(503, &message, None);
    };
    let req = ExecRequest {
        operation,
        source_format: Format::Claude,
        response_format: Format::Claude,
        requested_model: peek.model.clone(),
        model: peek.model,
        original_body: body.clone(),
        body,
        stream: peek.stream && operation == Operation::Generate,
        headers,
        caller,
    };
    // ponytail: one attempt. Retry rounds and failover across credentials arrive with
    // the scheduler port.
    let response = match rt.executors.execute(&lease.credential, req).await {
        Ok(response) => response,
        Err(e) => {
            rt.store().complete(&lease, Outcome::from_error(&e));
            return exec_error(&e);
        }
    };
    // ponytail: upstream headers are never forwarded, CLIProxyAPI's default. The
    // `passthrough-headers` option and its filter (sdk/api/handlers/header_filter.go)
    // arrive with the config port.
    match response.body {
        ResponseBody::Buffered(bytes) => {
            rt.store().complete(&lease, Outcome::Success);
            let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::OK);
            (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response()
        }
        ResponseBody::Stream(stream) => {
            let mut stream = Completing::new(stream, rt.clone(), lease);
            // SSE headers go out only once the first event arrives, so an upstream that
            // fails immediately still gets a proper status and JSON error.
            let first = match stream.next().await {
                Some(Err(e)) => return exec_error(&e),
                first => first,
            };
            let rest = stream.scan(false, |failed, item| {
                let out = match item {
                    _ if *failed => None,
                    Ok(event) => Some(event),
                    Err(e) => {
                        *failed = true;
                        Some(Bytes::from(format!(
                            "event: error\ndata: {}\n\n",
                            error_json(e.status, &e.body)
                        )))
                    }
                };
                std::future::ready(out)
            });
            let events = futures_util::stream::iter(first.into_iter().flatten()).chain(rest);
            let mut res =
                Body::from_stream(events.map(Ok::<_, std::convert::Infallible>)).into_response();
            let h = res.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            h.insert(
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_static("*"),
            );
            res
        }
    }
}

fn exec_error(e: &ExecError) -> Response {
    claude_error(
        e.status,
        &String::from_utf8_lossy(&e.body),
        e.retry_after.map(|d| d.as_secs()),
    )
}

fn claude_error(status: u16, text: &str, retry_after_secs: Option<u64>) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut res = (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        error_json(status.as_u16(), text.as_bytes()),
    )
        .into_response();
    if let Some(secs) = retry_after_secs {
        res.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(secs));
    }
    res
}

/// `toClaudeError` + `claudeErrorDetailFromText`: the type comes from the status unless
/// the upstream body names one; the message is the upstream message or the raw text.
fn error_json(status: u16, text: &[u8]) -> String {
    let text = String::from_utf8_lossy(text);
    let mut message = text.trim().to_owned();
    if message.is_empty() {
        message = StatusCode::from_u16(status)
            .ok()
            .and_then(|s| s.canonical_reason())
            .unwrap_or_default()
            .to_owned();
    }
    let mut kind = type_for_status(status).to_owned();
    if let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(&message) {
        let field = |obj: &serde_json::Map<String, Value>, key: &str| {
            obj.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        if let Some(Value::Object(err)) = payload.get("error") {
            kind = field(err, "type").unwrap_or(kind);
            if let Some(m) = field(err, "message").or_else(|| field(err, "code")) {
                message = m;
            }
        } else {
            kind = field(&payload, "type")
                .filter(|t| t != "error")
                .unwrap_or(kind);
            if let Some(m) = field(&payload, "message") {
                message = m;
            }
        }
    }
    serde_json::json!({ "type": "error", "error": { "type": kind, "message": message } })
        .to_string()
}

fn type_for_status(status: u16) -> &'static str {
    match status {
        401 => "authentication_error",
        402 => "billing_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        504 => "timeout_error",
        529 => "overloaded_error",
        500.. => "api_error",
        _ => "invalid_request_error",
    }
}

// ponytail: static list. The model registry port (internal/registry: embedded catalog,
// remote refresh, per-credential exclusions, aliases, unified OpenAI/Claude shapes)
// replaces this.
const CLAUDE_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-fable-5",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-sonnet-5-5",
    "claude-sonnet-5",
    "claude-haiku-4-5-20251001",
];

pub async fn models(State(rt): State<Arc<Runtime>>) -> axum::Json<Value> {
    let any_claude = rt
        .store()
        .snapshot()
        .iter()
        .any(|c| c.provider == "claude" && !c.disabled);
    let data: Vec<Value> = CLAUDE_MODELS
        .iter()
        .filter(|_| any_claude)
        .map(|id| serde_json::json!({ "id": id, "object": "model", "owned_by": "anthropic" }))
        .collect();
    axum::Json(serde_json::json!({ "object": "list", "data": data }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_json_matches_go_detail_rules() {
        let j = |s, t: &str| error_json(s, t.as_bytes());
        assert_eq!(
            j(
                429,
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
            ),
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
        );
        assert_eq!(
            j(400, r#"{"error":{"code":"bad_thing"}}"#),
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad_thing"}}"#
        );
        assert_eq!(
            j(529, r#"{"type":"error","message":"busy"}"#),
            r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#
        );
        assert_eq!(
            j(502, "  "),
            r#"{"type":"error","error":{"type":"api_error","message":"Bad Gateway"}}"#
        );
        assert_eq!(
            j(503, "[1,2]"),
            r#"{"type":"error","error":{"type":"api_error","message":"[1,2]"}}"#
        );
    }
}
