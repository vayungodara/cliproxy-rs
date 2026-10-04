//! Credential refresh through Home (Go internal/runtime/executor/helps/home_refresh.go,
//! `RefreshAuthViaHome`). In Home mode an executor never refreshes a dispatched
//! credential with its provider: Home owns the stored credential, refreshes it
//! (`GET {"type":"refresh",...}`) and returns the new auth, so no node rotates a token
//! Home does not know about.

use base64::Engine;
use serde_json::{Map, Value};

use crate::client::Client;

/// The auth Home returned after a refresh, and the auth index the attempt keeps.
#[derive(Debug, Clone, PartialEq)]
pub struct Refreshed {
    pub auth: Map<String, Value>,
    pub auth_index: String,
}

/// A failed Home refresh as Go's `homeStatusErr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshError {
    pub status: u16,
    /// The client-facing text, or, when `direct`, the provider's body verbatim.
    pub body: Vec<u8>,
    /// Home relayed the provider's own response: status and body go out unchanged
    /// (Go `DirectResponse`).
    pub direct: bool,
    /// Text safe for logs (Go `LogDiagnostic`).
    pub log: String,
}

impl RefreshError {
    fn local(status: u16, message: &str, log: &str) -> Self {
        Self {
            status,
            body: message.as_bytes().to_vec(),
            direct: false,
            log: log.to_owned(),
        }
    }
}

/// Go `statusFromHomeErrorCode`.
fn status_from_code(code: &str) -> u16 {
    match code.trim().to_lowercase().as_str() {
        "authentication_error"
        | "unauthorized"
        | "invalid_grant"
        | "refresh_token_expired"
        | "refresh_token_revoked"
        | "refresh_token_reused" => 401,
        "model_not_found" => 404,
        _ => 503,
    }
}

/// Go `RefreshAuthViaHome` once Home mode is known to be on: `client` is the current
/// Home client (`None` without one), `auth_index` the dispatched auth's index and
/// `access_token_sha256` the hash of the token that failed (Go `AccessTokenSHA256`).
pub async fn refresh(
    client: Option<&Client>,
    auth_index: &str,
    access_token_sha256: &str,
) -> Result<Refreshed, RefreshError> {
    let Some(client) = client.filter(|c| c.heartbeat_ok()) else {
        return Err(RefreshError::local(
            503,
            "home control center unavailable",
            "home control center unavailable",
        ));
    };
    let auth_index = auth_index.trim();
    if auth_index.is_empty() {
        return Err(RefreshError::local(
            502,
            "home refresh: auth_index is empty",
            "home refresh: auth_index is empty",
        ));
    }
    // ponytail: Go logs `SafeErrorDiagnostic` (an allowlist of transport causes) and
    // passes Home's own diagnostic through `SafeDiagnosticForLog`; Rust logs only the
    // stage, type or status, never error or diagnostic text. Port the sanitizers if
    // operators need the cause in logs.
    let raw = client
        .get_refresh_auth(auth_index, access_token_sha256)
        .await
        .map_err(|_| {
            RefreshError::local(
                503,
                "home refresh temporarily unavailable",
                "Home refresh transport failed",
            )
        })?;
    decode(&raw, auth_index)
}

/// Go's reply handling in `RefreshAuthViaHome`: an error envelope, else the auth (bare
/// or as `{"auth":…, "auth_index":…}`).
pub fn decode(raw: &[u8], auth_index: &str) -> Result<Refreshed, RefreshError> {
    if let Some(error) = error_envelope(raw) {
        return Err(error);
    }
    let invalid = || {
        RefreshError::local(
            502,
            "home returned invalid auth payload",
            "Home refresh response decode failed",
        )
    };
    let object: Map<String, Value> = serde_json::from_slice(raw).map_err(|_| invalid())?;
    let (auth, returned_index) = match object.get("auth") {
        None => (object, None),
        // Go `homeRefreshAuthEnvelope`: `auth_index` is a string.
        Some(_) if object.get("auth_index").is_some_and(|v| !v.is_string() && !v.is_null()) => {
            return Err(invalid());
        }
        Some(Value::Object(auth)) => (auth.clone(), object.get("auth_index").and_then(Value::as_str)),
        Some(Value::Null) => (Map::new(), object.get("auth_index").and_then(Value::as_str)),
        Some(_) => return Err(invalid()),
    };
    let disabled = auth.get("disabled") == Some(&Value::Bool(true))
        || auth.get("status").and_then(Value::as_str) == Some("disabled");
    if disabled {
        return Err(RefreshError::local(
            401,
            "credential unauthorized",
            "Home refresh failed: credential disabled",
        ));
    }
    let auth_index = match returned_index.map(str::trim) {
        Some(index) if !index.is_empty() => index.to_owned(),
        _ => auth_index.trim().to_owned(),
    };
    Ok(Refreshed { auth, auth_index })
}

/// Go `homeErrorEnvelope`: a reply whose `error` decodes as Go's `homeErrorDetail`
/// object. Any other `error` value fails Go's typed decode, and the reply is then read
/// as an auth.
fn error_envelope(raw: &[u8]) -> Option<RefreshError> {
    let object: Map<String, Value> = serde_json::from_slice(raw).ok()?;
    let detail = object.get("error")?.as_object()?;
    let text = |key: &str| match detail.get(key) {
        None | Some(Value::Null) => Some(String::new()),
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => None,
    };
    let (kind, code, _message, _diagnostic) = (text("type")?, text("code")?, text("message")?, text("diagnostic")?);
    let code = match kind.trim() {
        "" => code.trim().to_owned(),
        kind => kind.to_owned(),
    };
    if let Some(upstream) = detail.get("upstream").filter(|v| !v.is_null()) {
        let upstream = upstream.as_object()?;
        let status = match upstream.get("status") {
            None | Some(Value::Null) => 0,
            Some(v) => v.as_i64().and_then(|s| u16::try_from(s).ok())?,
        };
        // Go `[]byte` fields travel as base64.
        let body = match upstream.get("body") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(s)) => base64::engine::general_purpose::STANDARD.decode(s).ok()?,
            Some(_) => return None,
        };
        return Some(RefreshError {
            status,
            body,
            direct: true,
            log: format!("Home refresh upstream response: status={status}"),
        });
    }
    let status = status_from_code(&code);
    let message = match status {
        401 => "credential unauthorized",
        404 => "credential refresh target not found",
        _ => "credential refresh temporarily unavailable",
    };
    let log = match code.trim().to_lowercase() {
        kind if kind.is_empty() => message.to_owned(),
        kind => format!("Home refresh failed: type={kind}"),
    };
    Some(RefreshError {
        status,
        body: message.as_bytes().to_vec(),
        direct: false,
        log,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(raw: &str) -> RefreshError {
        decode(raw.as_bytes(), "idx").unwrap_err()
    }

    /// Go `TestStatusFromHomeErrorCodeMapsAuthenticationErrorToUnauthorized`.
    #[test]
    fn error_codes_map_to_go_statuses() {
        for code in ["authentication_error", "unauthorized"] {
            assert_eq!(status_from_code(code), 401);
        }
        for code in [
            "auth_not_found",
            "auth_unavailable",
            "refresh_temporarily_unavailable",
            "refresh_unsupported",
        ] {
            assert_eq!(status_from_code(code), 503);
        }
        assert_eq!(status_from_code("model_not_found"), 404);
    }

    /// Go `TestRefreshAuthViaHomeUsesGenericMessageForLegacyErrorEnvelope`,
    /// `TestRefreshAuthViaHomeUsesDedicatedDiagnosticOnlyForLogs`,
    /// `TestRefreshAuthViaHomeUsesGenericMessageForLegacyProviderDiagnostics` and
    /// `TestRefreshAuthViaHomeRejectsUnmarkedRefreshMessage`: Home's messages and
    /// diagnostics never reach the client or the log.
    #[test]
    fn home_errors_become_generic_messages() {
        let cases = [
            (
                r#"{"error":{"type":"error","message":"provider response: refresh_token=provider-secret"}}"#,
                503,
                "credential refresh temporarily unavailable",
                "Home refresh failed: type=error",
            ),
            (
                r#"{"error":{"type":"refresh_temporarily_unavailable","message":"untrusted provider-secret","diagnostic":"antigravity refresh failed: stage=transport err=EOF"}}"#,
                503,
                "credential refresh temporarily unavailable",
                "Home refresh failed: type=refresh_temporarily_unavailable",
            ),
            (
                r#"{"error":{"type":"authentication_error","message":"codex refresh: invalid_grant refresh_token=provider-secret"}}"#,
                401,
                "credential unauthorized",
                "Home refresh failed: type=authentication_error",
            ),
            (
                r#"{"error":{"type":"refresh_temporarily_unavailable","message":"database unavailable: provider-secret"}}"#,
                503,
                "credential refresh temporarily unavailable",
                "Home refresh failed: type=refresh_temporarily_unavailable",
            ),
            (
                r#"{"error":{"code":"model_not_found"}}"#,
                404,
                "credential refresh target not found",
                "Home refresh failed: type=model_not_found",
            ),
        ];
        for (raw, status, message, log) in cases {
            let error = error(raw);
            assert_eq!((error.status, error.direct), (status, false), "{raw}");
            assert_eq!(String::from_utf8(error.body).unwrap(), message, "{raw}");
            assert_eq!(error.log, log, "{raw}");
            assert!(!error.log.contains("provider-secret"));
        }
    }

    /// Go `TestRefreshAuthViaHomePreservesUpstreamStatusAndBodyExactly` and
    /// `TestHomeStatusErrLogDiagnosticSanitizesUpstreamFallback`: a provider response
    /// Home relays goes out with its status and exact bytes; the log names the status.
    #[test]
    fn relayed_upstream_responses_keep_status_and_bytes() {
        let cases: [(u16, &[u8]); 5] = [
            (400, br#"{"error":"invalid_request"}"#),
            (401, br#"{"error":{"message":"access token expired"}}"#),
            (502, b"provider unavailable"),
            (429, b"first line\r\nsecond line\n"),
            (401, b""),
        ];
        for (status, body) in cases {
            let encoded = base64::engine::general_purpose::STANDARD.encode(body);
            let raw = format!(
                r#"{{"error":{{"type":"refresh_temporarily_unavailable","message":"credential refresh temporarily unavailable","diagnostic":"x","upstream":{{"status":{status},"body":"{encoded}"}}}}}}"#
            );
            let error = error(&raw);
            assert_eq!((error.status, error.direct), (status, true));
            assert_eq!(error.body, body);
            assert_eq!(error.log, format!("Home refresh upstream response: status={status}"));
        }
    }

    /// Go `TestRefreshAuthViaHomeAcceptsAuthEnvelope`: the envelope's auth index wins;
    /// a bare auth keeps the request's; a disabled auth is unauthorized; a malformed
    /// reply is Home's invalid payload.
    #[test]
    fn auths_decode_bare_or_enveloped() {
        let enveloped = decode(
            br#"{"auth":{"id":"home-auth-1","provider":"antigravity","metadata":{"access_token":"new-access-token"}},"auth_index":"home-index-1"}"#,
            "idx",
        )
        .unwrap();
        assert_eq!(enveloped.auth_index, "home-index-1");
        assert_eq!(enveloped.auth["metadata"]["access_token"], "new-access-token");
        let bare = decode(br#"{"id":"a","metadata":{"api_key":"k"}}"#, " idx ").unwrap();
        assert_eq!((bare.auth_index.as_str(), bare.auth["id"].as_str()), ("idx", Some("a")));
        for disabled in [
            r#"{"id":"a","disabled":true}"#,
            r#"{"auth":{"id":"a","status":"disabled"}}"#,
        ] {
            let error = error(disabled);
            assert_eq!(
                (error.status, error.body.as_slice()),
                (401, b"credential unauthorized".as_slice())
            );
        }
        for malformed in ["[1]", "not json", r#"{"auth":"x"}"#, r#"{"auth":{},"auth_index":5}"#] {
            let error = error(malformed);
            assert_eq!(
                (error.status, error.body.as_slice()),
                (502, b"home returned invalid auth payload".as_slice())
            );
        }
        // An `error` that is not Go's object is read as an auth, as Go's typed decode does.
        assert!(decode(br#"{"error":"text","id":"a"}"#, "idx").is_ok());
    }

    /// Go `TestRefreshAuthViaHomeMapsTransportFailureToGeneric503` and the unavailable
    /// and empty-index guards: no client or heartbeat is a 503, an empty index a 502,
    /// and a failed exchange a generic 503 that never carries the transport error.
    #[tokio::test]
    async fn unavailable_home_and_transport_failures_are_generic() {
        let error = refresh(None, "idx", "").await.unwrap_err();
        assert_eq!(
            (error.status, error.body.as_slice()),
            (503, b"home control center unavailable".as_slice())
        );
        let home =
            crate::fake::FakeHome::start(|_| crate::fake::raw("-ERR dial failed with provider-secret\r\n")).await;
        let client = Client::new(home.config());
        crate::fake::set_heartbeat(&client, true);
        let error = refresh(Some(&client), " ", "").await.unwrap_err();
        assert_eq!(
            (error.status, error.body.as_slice()),
            (502, b"home refresh: auth_index is empty".as_slice())
        );
        let error = refresh(Some(&client), "idx", "abc").await.unwrap_err();
        assert_eq!((error.status, error.direct), (503, false));
        assert_eq!(error.body, b"home refresh temporarily unavailable");
        assert!(!error.log.contains("provider-secret"));
        let sent = home.commands();
        let request: Value = serde_json::from_str(&sent.last().unwrap()[1]).unwrap();
        assert_eq!(
            request,
            serde_json::json!({"type": "refresh", "auth_index": "idx", "access_token_sha256": "abc"})
        );
    }
}
