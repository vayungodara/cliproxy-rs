//! Route error shapes: Go's `BuildErrorResponseBody` (OpenAI, Responses, Gemini and
//! Interactions routes) and `toClaudeError` (Claude routes).

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use serde_json::Value;

use crate::dispatch::Failure;
use crate::gojson::{self, Obj};

/// Go `WriteErrorResponse`'s text: the error text trimmed, else the status text.
fn text_for(status: u16, text: &str) -> String {
    let v = gojson::trim(text);
    if v.is_empty() {
        gojson::status_text(status).to_owned()
    } else {
        v.to_owned()
    }
}

/// Go `BuildErrorResponseBody`: valid JSON passes through trimmed, anything else is
/// wrapped in an OpenAI error envelope typed by status.
pub fn openai_body(status: u16, text: &str) -> String {
    let status = if status == 0 { 500 } else { status };
    let text = if gojson::trim(text).is_empty() {
        gojson::status_text(status)
    } else {
        text
    };
    let trimmed = gojson::trim(text);
    if !trimmed.is_empty() && serde_json::from_str::<Value>(trimmed).is_ok() {
        return trimmed.to_owned();
    }
    let (kind, code) = match status {
        401 => ("authentication_error", "invalid_api_key"),
        403 => ("permission_error", "insufficient_quota"),
        429 => ("rate_limit_error", "rate_limit_exceeded"),
        404 => ("invalid_request_error", "model_not_found"),
        408 => ("server_error", "request_timeout"),
        500.. => ("server_error", "internal_server_error"),
        _ => ("invalid_request_error", ""),
    };
    let mut detail = Obj::new().str("message", text).str("type", kind);
    if !code.is_empty() {
        detail = detail.str("code", code);
    }
    Obj::new().raw("error", &detail.finish()).finish()
}

/// Go `claudeErrorTypeFromStatus`.
fn claude_type(status: u16) -> &'static str {
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

/// Go `toClaudeError` + `claudeErrorDetailFromText`.
pub fn claude_body(status: u16, text: &str) -> String {
    let mut message = text_for(status, text);
    let mut kind = claude_type(status).to_owned();
    if let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(&message) {
        let field = |obj: &serde_json::Map<String, Value>, key: &str| {
            obj.get(key)
                .and_then(Value::as_str)
                .map(gojson::trim)
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
    let detail = Obj::new().str("type", &kind).str("message", &message).finish();
    Obj::new().str("type", "error").raw("error", &detail).finish()
}

/// Writes a failure with the route's body shape.
pub fn write(failure: &Failure, body: fn(u16, &str) -> String) -> Response {
    // Go `writeDirectErrorResponse`: the filtered headers as given, JSON by default.
    if let Some(e) = failure.direct() {
        let status = StatusCode::from_u16(e.status).unwrap_or(StatusCode::BAD_GATEWAY);
        let mut res = Response::new(axum::body::Body::from(e.body.clone()));
        *res.status_mut() = status;
        let headers = crate::respond::filter_upstream_headers(&e.headers);
        for name in headers.keys() {
            for value in headers.get_all(name) {
                res.headers_mut().append(name.clone(), value.clone());
            }
        }
        if !res.headers().contains_key(header::CONTENT_TYPE) {
            res.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        return res;
    }
    let status = failure.status();
    let text = text_for(status, &failure.text());
    let mut res = crate::respond::json(status, "application/json", body(status, &text));
    if let Some(seconds) = failure.retry_after() {
        res.headers_mut().insert(header::RETRY_AFTER, seconds.into());
    }
    res
}

pub fn openai(failure: &Failure) -> Response {
    write(failure, openai_body)
}

pub fn claude(failure: &Failure) -> Response {
    write(failure, claude_body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_envelope_matches_go() {
        assert_eq!(
            openai_body(500, "status 500"),
            r#"{"error":{"message":"status 500","type":"server_error","code":"internal_server_error"}}"#
        );
        assert_eq!(openai_body(400, " {\"a\":1} "), r#"{"a":1}"#);
        assert_eq!(
            openai_body(422, "bad <x>"),
            r#"{"error":{"message":"bad \u003cx\u003e","type":"invalid_request_error"}}"#
        );
        assert_eq!(
            openai_body(429, ""),
            r#"{"error":{"message":"Too Many Requests","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#
        );
    }

    #[test]
    fn claude_envelope_matches_go_detail_rules() {
        assert_eq!(
            claude_body(
                429,
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
            ),
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
        );
        assert_eq!(
            claude_body(400, r#"{"error":{"code":"bad_thing"}}"#),
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad_thing"}}"#
        );
        assert_eq!(
            claude_body(529, r#"{"type":"error","message":"busy"}"#),
            r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#
        );
        // Go: an empty upstream body becomes `status N` before formatting.
        assert_eq!(
            claude_body(500, "status 500"),
            r#"{"type":"error","error":{"type":"api_error","message":"status 500"}}"#
        );
        assert_eq!(
            claude_body(502, "  "),
            r#"{"type":"error","error":{"type":"api_error","message":"Bad Gateway"}}"#
        );
    }
}
