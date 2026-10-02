//! Anthropic Messages routes (sdk/api/handlers/claude/code_handlers.go).

use std::sync::Arc;

use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::rejection::BytesRejection;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use cpa_core::exec::{Caller, ExecError, ExecRequest, Operation, ResponseBody};
use cpa_core::format::Format;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;

use crate::access::query_get;
use crate::runtime::{AcquireError, Completing, Outcome, Runtime, Selection};
use crate::scheduler::retry_status;

pub async fn messages(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    handle(
        rt,
        caller,
        uri.query().unwrap_or_default(),
        headers,
        body,
        Operation::Generate,
    )
    .await
}

pub async fn count_tokens(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    handle(
        rt,
        caller,
        uri.query().unwrap_or_default(),
        headers,
        body,
        Operation::CountTokens,
    )
    .await
}

/// The fields the handler reads, each independent of the others' types. Like gjson in
/// Go, a body that is not a JSON object is not rejected here; upstream decides. Only
/// these two values are materialized; the rest of the body is skipped.
#[derive(Deserialize, Default)]
struct Peek {
    #[serde(default)]
    model: Option<Value>,
    #[serde(default)]
    stream: Option<Value>,
}

async fn handle(
    rt: Arc<Runtime>,
    caller: Caller,
    query: &str,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
    operation: Operation,
) -> Response {
    let body = match body {
        Ok(body) => body,
        Err(rejection) => return claude_error(rejection.status().as_u16(), &rejection.body_text()),
    };
    let peek: Peek = serde_json::from_slice(&body).unwrap_or_default();
    let model = peek
        .model
        .as_ref()
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    let stream = peek.stream == Some(Value::Bool(true)) && operation == Operation::Generate;
    let alt = query_get(query, "alt")
        .or_else(|| query_get(query, "$alt"))
        .map(|v| String::from_utf8_lossy(&v).into_owned());
    let (cfg, policy) = rt.request_snapshot();
    // ponytail: explicit Claude header identity only. Full cross-client identity,
    // message hashes, parent/root relationships and alias groups remain M4-0021.
    let session = headers
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .map(str::to_owned);
    let mut selection = Selection {
        provider: "claude".into(),
        model: model.clone(),
        session: session.clone(),
        ..Selection::default()
    };
    let req = ExecRequest {
        operation,
        source_format: Format::Claude,
        response_format: Format::Claude,
        requested_model: model.clone(),
        model: model.clone(),
        original_body: body.clone(),
        body,
        stream,
        alt,
        session,
        headers,
        caller,
    };
    let mut last_error = None;
    loop {
        let mut attempted = 0;
        loop {
            if policy.max_retry_credentials > 0 && attempted >= policy.max_retry_credentials {
                break;
            }
            let lease = match rt.acquire_with_policy(selection.clone(), &cfg, policy.clone()).await {
                Ok(lease) => lease,
                Err(AcquireError::Prepare { id, error }) => {
                    attempted += 1;
                    selection.exclude.push(id.clone());
                    if rt
                        .store()
                        .get(&id)
                        .is_some_and(|c| policy.error_action(&c, &error).stop)
                    {
                        return exec_error(&error);
                    }
                    last_error = Some(error);
                    continue;
                }
                Err(AcquireError::Cooldown { wait }) => {
                    if last_error.is_none() {
                        return cooldown_error(&model, wait);
                    }
                    break;
                }
                Err(AcquireError::NoCredential) => {
                    if last_error.is_none() {
                        let model = if model.is_empty() { "unknown" } else { &model };
                        return claude_error(
                            503,
                            &format!(
                                "auth_not_found: no auth available (providers=claude, model={model}); \
                             check Claude auth/key session and cooldown state via /v0/management/auth-files"
                            ),
                        );
                    }
                    break;
                }
            };
            attempted += 1;
            selection.exclude.push(lease.credential.id.clone());
            let mut attempt_req = req.clone();
            attempt_req.model.clone_from(&lease.execution_model);
            // Cancellation while executing or bootstrapping drops this guard once.
            let response = match rt.executors.execute(&lease.credential, attempt_req, &cfg).await {
                Ok(response) => response,
                Err(e) => {
                    let action = policy.error_action(&lease.credential, &e);
                    lease.complete(Outcome::Failure(e.clone()));
                    if action.stop {
                        return exec_error(&e);
                    }
                    last_error = Some(e);
                    continue;
                }
            };
            // ponytail: upstream headers are never forwarded, CLIProxyAPI's default. The
            // `passthrough-headers` option and its filter (sdk/api/handlers/header_filter.go)
            // arrive with the config port.
            match response.body {
                ResponseBody::Buffered(bytes) => {
                    lease.complete(Outcome::Success);
                    let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::OK);
                    return (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response();
                }
                ResponseBody::Stream(stream) => {
                    let mut stream = stream;
                    // SSE headers go out only once the first event arrives, so an upstream that
                    // fails immediately still gets a proper status and JSON error.
                    let first = loop {
                        match stream.next().await {
                            Some(Ok(bytes)) if bytes.is_empty() => continue,
                            item => break item,
                        }
                    };
                    let first = match first {
                        Some(Err(e)) => {
                            let action = policy.error_action(&lease.credential, &e);
                            lease.complete(Outcome::Failure(e.clone()));
                            if action.stop {
                                return exec_error(&e);
                            }
                            last_error = Some(e);
                            continue;
                        }
                        first => first.map(Result::unwrap),
                    };
                    let stream = Completing::new(stream, lease);
                    // `Completing` yields nothing after an error, so the error event is last.
                    let rest = stream.map(|item| match item {
                        Ok(event) => event,
                        Err(e) => Bytes::from(format!("event: error\ndata: {}\n\n", error_json(e.status, &e.body))),
                    });
                    let events = futures_util::stream::iter(first).chain(rest);
                    let mut res = Body::from_stream(events.map(Ok::<_, std::convert::Infallible>)).into_response();
                    let h = res.headers_mut();
                    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
                    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
                    return res;
                }
            }
        }
        let error = last_error.as_ref().unwrap();
        // Only the Go retry-round status set (or transport faults) can replay a round.
        if !retry_status(error.status) && error.scope != cpa_core::exec::FailureScope::Transport {
            return exec_error(error);
        }
        let Some(wait) = rt.store().retry_wait(&selection, &policy, error) else {
            return exec_error(error);
        };
        if !wait.is_zero() {
            // ponytail: exact bounded waits; Go's capped anti-stampede jitter is
            // deferred until a controllable runtime clock/random source is wired.
            tokio::time::sleep(wait).await;
        }
        selection.retry_round += 1;
        selection.exclude.clear();
    }
}

fn cooldown_error(model: &str, wait: std::time::Duration) -> Response {
    let model = if model.is_empty() { "requested model" } else { model };
    let mut response = claude_error(
        429,
        &format!("All credentials for model {model} are cooling down via provider claude"),
    );
    let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, seconds.to_string().parse().unwrap());
    response
}

/// `Retry-After` and other upstream headers are not sent: Go emits `Retry-After` only for
/// its own scheduler/cooldown errors (SafeResponseHeaders), which arrive with that port.
fn exec_error(e: &ExecError) -> Response {
    if e.direct {
        let status = StatusCode::from_u16(e.status).unwrap_or(StatusCode::BAD_GATEWAY);
        let content_type = e
            .headers
            .get(header::CONTENT_TYPE)
            .cloned()
            .unwrap_or(HeaderValue::from_static("application/json"));
        return (status, [(header::CONTENT_TYPE, content_type)], e.body.clone()).into_response();
    }
    claude_error(e.status, &String::from_utf8_lossy(&e.body))
}

fn claude_error(status: u16, text: &str) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = error_json(status.as_u16(), text.as_bytes());
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
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
            kind = field(&payload, "type").filter(|t| t != "error").unwrap_or(kind);
            if let Some(m) = field(&payload, "message") {
                message = m;
            }
        }
    }
    serde_json::json!({ "type": "error", "error": { "type": kind, "message": message } }).to_string()
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

    #[test]
    fn peek_fields_are_independent() {
        let peek: Peek = serde_json::from_slice(br#"{"model":" claude-opus-5 ","stream":"yes","x":[1]}"#).unwrap();
        assert_eq!(
            peek.model.as_ref().and_then(Value::as_str).map(str::trim),
            Some("claude-opus-5")
        );
        assert_ne!(
            peek.stream,
            Some(Value::Bool(true)),
            "a non-bool stream is not streaming"
        );
    }
}
