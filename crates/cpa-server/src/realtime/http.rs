//! Call negotiation (`Handler.Handle`), hangup (`HandleHangup`) and the capability
//! stubs (capabilities.go).

use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use cpa_exec::codex_live::{self as live, BodyError, LiveTarget, ShapeError};
use futures_util::StreamExt;

use super::calls::Call;
use super::{Live, Principal, header_session, live_error, panic_response, realtime_error, select_oauth, with_trace};
use crate::runtime::Runtime;

pub(super) enum ReadError {
    TooLarge,
    Read(String),
}

/// `io.ReadAll(io.LimitReader(body, limit+1))`: more than `limit` bytes is too large.
pub(super) async fn read_limited(body: Body, limit: usize) -> Result<Vec<u8>, ReadError> {
    let mut stream = body.into_data_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ReadError::Read(e.to_string()))?;
        if out.len() + chunk.len() > limit {
            return Err(ReadError::TooLarge);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// `c.GetHeader`: the first value.
pub(super) fn header_text(headers: &HeaderMap, name: impl header::AsHeaderName) -> String {
    headers
        .get(name)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .unwrap_or_default()
}

/// `callResponseHeaders`: the upstream headers a live response may carry.
const CALL_RESPONSE_HEADERS: [&str; 5] = [
    "content-type",
    "location",
    "retry-after",
    "x-request-id",
    "openai-request-id",
];

/// `copyRealtimeHandshakeHeaders`.
pub(super) const HANDSHAKE_HEADERS: [&str; 3] = ["retry-after", "x-request-id", "openai-request-id"];

pub(super) fn copy_headers(to: &mut HeaderMap, from: &HeaderMap, names: &[&'static str]) {
    for name in names {
        for value in from.get_all(*name) {
            to.append(HeaderName::from_static(name), value.clone());
        }
    }
}

/// A handler's body written the way Go's server does: a missing Content-Type is
/// sniffed from a non-empty body (`http.DetectContentType`).
pub(super) fn written(status: u16, mut headers: HeaderMap, body: Vec<u8>) -> Response {
    if !headers.contains_key(header::CONTENT_TYPE) && !body.is_empty() {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(sniff(&body)));
    }
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = (status, body).into_response();
    *response.headers_mut() = headers;
    response
}

/// `http.DetectContentType` for what a live upstream returns.
// ponytail: HTML, XML, UTF BOMs and text-versus-binary only; Go also recognises images,
// audio, archives and fonts. SDP and JSON are text/plain in Go too.
fn sniff(body: &[u8]) -> &'static str {
    let data = &body[..body.len().min(512)];
    if data.starts_with(b"\xFE\xFF") {
        return "text/plain; charset=utf-16be";
    }
    if data.starts_with(b"\xFF\xFE") {
        return "text/plain; charset=utf-16le";
    }
    if data.starts_with(b"\xEF\xBB\xBF") {
        return "text/plain; charset=utf-8";
    }
    let trimmed = &data[data
        .iter()
        .take_while(|b| matches!(b, b'\t' | b'\n' | 0x0c | b'\r' | b' '))
        .count()..];
    const HTML: [&[u8]; 17] = [
        b"<!DOCTYPE HTML",
        b"<HTML",
        b"<HEAD",
        b"<SCRIPT",
        b"<IFRAME",
        b"<H1",
        b"<DIV",
        b"<FONT",
        b"<TABLE",
        b"<A",
        b"<STYLE",
        b"<TITLE",
        b"<B",
        b"<BODY",
        b"<BR",
        b"<P",
        b"<!--",
    ];
    for tag in HTML {
        if trimmed.len() > tag.len()
            && trimmed[..tag.len()].eq_ignore_ascii_case(tag)
            && matches!(trimmed[tag.len()], b' ' | b'>')
        {
            return "text/html; charset=utf-8";
        }
    }
    if trimmed.starts_with(b"<?xml") {
        return "text/xml; charset=utf-8";
    }
    if data.starts_with(b"%PDF-") {
        return "application/pdf";
    }
    let binary = |b: &u8| matches!(b, 0x00..=0x08 | 0x0B | 0x0E..=0x1A | 0x1C..=0x1F);
    if data.iter().any(binary) {
        "application/octet-stream"
    } else {
        "text/plain; charset=utf-8"
    }
}

/// `Handler.Handle`: `POST /v1/live`, `/v1/realtime` and `/v1/realtime/calls`.
pub(super) async fn call(
    State(rt): State<Arc<Runtime>>,
    Extension(live_state): Extension<Arc<Live>>,
    Extension(principal): Extension<Principal>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let realtime = uri.path().starts_with("/v1/realtime");
    let fail = |status, message: &str| live_error(realtime, status, message);
    let body = match read_limited(body, live::MAX_BODY).await {
        Ok(body) => body,
        Err(ReadError::TooLarge) => return fail(413, "Codex live request body too large"),
        Err(ReadError::Read(e)) => return fail(400, &format!("failed to read Codex live request: {e}")),
    };
    let secret_session = principal
        .secret
        .as_ref()
        .map(|g| g.session.as_slice())
        .unwrap_or_default();
    let shaped = live::prepare(&body, &header_text(&headers, header::CONTENT_TYPE))
        .and_then(|call| live::apply_client_secret(call, secret_session))
        .and_then(live::rewrite_model);
    let call = match shaped {
        Ok(call) => call,
        Err(ShapeError::Invalid(message)) => return fail(400, &message),
        Err(ShapeError::NilMap) => return panic_response(),
    };
    // ponytail: the optional WebRTC media relay (codex.live-media-relay) is not wired yet;
    // calls always negotiate end to end with the upstream media servers.
    let cfg = rt.config();
    let selection_headers = principal.selection_headers(&headers);
    let lease = match select_oauth(&rt, &cfg, None, &selection_headers, &body, None).await {
        Ok(lease) => lease,
        Err(rejection) => return rejection.render(realtime),
    };
    let credential = lease.credential.clone();
    let session_id = super::session_id(&selection_headers, &body);
    let mut upstream_headers = live::protocol_headers(&headers);
    upstream_headers.push(("Content-Type".into(), call.content_type.clone()));
    let target = LiveTarget {
        credential: &credential,
        cfg: &cfg,
        client: &headers,
        session: header_session(&selection_headers, &body, None, ""),
    };
    let call_url = rt.executors.codex.live_endpoints().call_url.clone();
    let upstream = match rt
        .executors
        .codex
        .live_post(&target, &call_url, upstream_headers, Bytes::from(call.body))
        .await
    {
        Ok(upstream) => upstream,
        Err(e) => {
            let status = if e.status == 0 { 502 } else { e.status };
            return with_trace(fail(status, &String::from_utf8_lossy(&e.body)), &credential);
        }
    };
    let status = upstream.status;
    let mut response_headers = HeaderMap::new();
    copy_headers(&mut response_headers, &upstream.headers, &CALL_RESPONSE_HEADERS);
    let location = header_text(&upstream.headers, header::LOCATION);
    let (data, read_error) = live::read_limited(upstream).await;
    drop(lease);
    if let Some(error) = read_error {
        let message = match error {
            BodyError::TooLarge => "Codex live response body too large",
            BodyError::Read => "Failed to read Codex live response",
        };
        return with_trace(fail(502, message), &credential);
    }
    if (200..300).contains(&status) {
        let call_id = live::call_id_from_location(&location);
        if !call_id.is_empty() {
            if realtime && let Ok(value) = HeaderValue::from_str(&format!("/v1/realtime/calls/{call_id}")) {
                response_headers.insert(header::LOCATION, value);
            }
            live_state.calls.put(
                &call_id,
                Call {
                    auth_id: credential.id.clone(),
                    session_id,
                    owner_key: principal.key.clone(),
                    owner_provider: principal.provider.clone(),
                    secret_principal: principal
                        .secret
                        .as_ref()
                        .map(|g| g.principal.clone())
                        .unwrap_or_default(),
                    ..Call::default()
                },
            );
        }
    }
    // ponytail: Go completes the stored call when writing this body fails; axum gives no
    // write result, so such a call lives until its sideband, hangup or expiry.
    with_trace(written(status, response_headers, data), &credential)
}

/// The call ID path parameter, trimmed; `None` when it cannot be decoded.
fn path_call_id(path: Result<Path<String>, PathRejection>) -> Option<String> {
    path.ok()
        .map(|Path(id)| cpa_common::gostr::trim_space(id.as_bytes()).to_owned())
        .and_then(|id| String::from_utf8(id).ok())
}

/// `HandleHangup`: forwards to the upstream with the call's credential and forgets the
/// call when the upstream accepts.
pub(super) async fn hangup(
    State(rt): State<Arc<Runtime>>,
    Extension(live_state): Extension<Arc<Live>>,
    Extension(principal): Extension<Principal>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(call_id) = path_call_id(path).filter(|id| live::valid_call_id(id)) else {
        return realtime_error(
            400,
            "Invalid Realtime call ID",
            "invalid_request_error",
            "invalid_call_id",
        );
    };
    let Some(stored) = live_state.calls.peek(&call_id) else {
        return realtime_error(
            404,
            "Realtime call not found",
            "invalid_request_error",
            "realtime_call_not_found",
        );
    };
    if !stored.owner_key.is_empty()
        && (principal.key != stored.owner_key || principal.provider != stored.owner_provider)
    {
        return realtime_error(
            403,
            "Realtime call belongs to another API principal",
            "invalid_request_error",
            "realtime_call_scope_mismatch",
        );
    }
    let cfg = rt.config();
    let selection_headers = principal.selection_headers(&headers);
    let lease = match select_oauth(
        &rt,
        &cfg,
        Some(&stored.auth_id),
        &selection_headers,
        &[],
        Some(&call_id),
    )
    .await
    {
        Ok(lease) => lease,
        Err(rejection) => return rejection.render(true),
    };
    let credential = lease.credential.clone();
    let traced = |response| with_trace(response, &credential);
    let body = match read_limited(body, live::MAX_BODY).await {
        Ok(body) => body,
        Err(ReadError::TooLarge) => {
            return traced(realtime_error(
                400,
                "Codex live request body too large",
                "invalid_request_error",
                "invalid_request",
            ));
        }
        Err(ReadError::Read(e)) => {
            return traced(realtime_error(
                400,
                &format!("failed to read Codex live request: {e}"),
                "invalid_request_error",
                "invalid_request",
            ));
        }
    };
    let mut upstream_headers = live::protocol_headers(&headers);
    let content_type = header_text(&headers, header::CONTENT_TYPE);
    let content_type = String::from_utf8_lossy(cpa_common::gostr::trim_space(content_type.as_bytes())).into_owned();
    if !content_type.is_empty() {
        upstream_headers.push(("Content-Type".into(), content_type));
    }
    let target = LiveTarget {
        credential: &credential,
        cfg: &cfg,
        client: &headers,
        session: header_session(&selection_headers, &[], Some(&call_id), &stored.session_id),
    };
    let url = format!(
        "{}/realtime/calls/{call_id}/hangup",
        live::http_base(&rt.executors.codex.live_endpoints().api_base)
    );
    let upstream = match rt
        .executors
        .codex
        .live_post(&target, &url, upstream_headers, Bytes::from(body))
        .await
    {
        Ok(upstream) => upstream,
        Err(e) => {
            let status = if e.status == 0 { 502 } else { e.status };
            return traced(realtime_error(
                status,
                &String::from_utf8_lossy(&e.body),
                "api_error",
                "realtime_upstream_unavailable",
            ));
        }
    };
    let status = upstream.status;
    let mut response_headers = HeaderMap::new();
    if let Some(content_type) = upstream.headers.get(header::CONTENT_TYPE).filter(|v| !v.is_empty()) {
        response_headers.insert(header::CONTENT_TYPE, content_type.clone());
    }
    copy_headers(&mut response_headers, &upstream.headers, &HANDSHAKE_HEADERS);
    let (data, read_error) = live::read_limited(upstream).await;
    drop(lease);
    if read_error.is_some() {
        return traced(realtime_error(
            502,
            "Failed to read Realtime hangup response",
            "api_error",
            "realtime_upstream_unavailable",
        ));
    }
    if (200..300).contains(&status) {
        live_state.calls.complete(&stored, "client_hangup");
    }
    traced(written(status, response_headers, data))
}

/// `writeCapabilityNotSupported`.
fn not_supported(capability: &str) -> Response {
    realtime_error(
        501,
        &format!("{capability} are not supported by the ChatGPT/Codex OAuth upstream"),
        "not_supported_error",
        "realtime_capability_not_supported",
    )
}

pub(super) async fn translation() -> Response {
    not_supported("Realtime translation sessions")
}

pub(super) async fn transcription() -> Response {
    not_supported("Realtime transcription-only sessions")
}

/// `HandleSIPControl`: the action is the last path segment.
pub(super) async fn sip(OriginalUri(uri): OriginalUri) -> Response {
    let action = uri
        .path()
        .trim_matches('/')
        .rsplit('/')
        .next()
        .filter(|a| !a.trim().is_empty())
        .unwrap_or("control");
    not_supported(&format!("Realtime SIP {action}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values from Go 1.26 `http.DetectContentType`.
    #[test]
    fn sniff_matches_go_for_live_bodies() {
        assert_eq!(
            sniff(b"v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\n"),
            "text/plain; charset=utf-8"
        );
        assert_eq!(sniff(br#"{"sdp":"v=0"}"#), "text/plain; charset=utf-8");
        assert_eq!(sniff(b"  <html><body>x"), "text/html; charset=utf-8");
        assert_eq!(sniff(b"<htmlx"), "text/plain; charset=utf-8");
        assert_eq!(sniff(b"<?xml version"), "text/xml; charset=utf-8");
        assert_eq!(sniff(b"a\x00b"), "application/octet-stream");
        assert_eq!(sniff(b"\xEF\xBB\xBFx"), "text/plain; charset=utf-8");
    }
}
