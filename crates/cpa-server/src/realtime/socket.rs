//! The call sideband (sideband.go `HandleSideband`) and the standard Realtime WebSocket
//! (websocket.go `HandleDirectWebsocket`): both dial an upstream socket with a Codex
//! OAuth credential, then relay text and binary messages both ways until either side
//! closes. Pings are answered locally, never relayed (gorilla's default handlers).
//!
//! A sideband claims its call while it runs: a second join gets 409, the call does not
//! expire meanwhile, a hangup tears the relay down, and the call is forgotten when the
//! relay ends.

use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use axum::extract::rejection::PathRejection;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{self, WebSocket, WebSocketUpgrade};
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use cpa_exec::codex_live::{self as live, DialError, LiveSocket, LiveTarget, Sideband};
use futures_util::{SinkExt, StreamExt};
use wreq::ws::message as up;

use super::calls::{Claim, ClaimGuard};
use super::http::{HANDSHAKE_HEADERS, copy_headers, header_text};
use super::{Live, Principal, header_session, live_error, realtime_error, select_oauth, trace, with_trace};
use crate::runtime::Runtime;

/// Bound on the closing frames written after a relay ends.
const CLOSE_WRITE: Duration = Duration::from_secs(1);

/// gorilla `tokenListContainsValue`: `token` is one of the comma-separated values.
fn has_token(headers: &HeaderMap, name: header::HeaderName, token: &str) -> bool {
    headers.get_all(name).iter().any(|v| {
        String::from_utf8_lossy(v.as_bytes())
            .split(',')
            .any(|t| t.trim_matches([' ', '\t']).eq_ignore_ascii_case(token))
    })
}

/// gorilla `IsWebSocketUpgrade`: `Connection` lists `upgrade` and `Upgrade` lists
/// `websocket`.
fn is_upgrade(headers: &HeaderMap) -> bool {
    has_token(headers, header::CONNECTION, "upgrade") && has_token(headers, header::UPGRADE, "websocket")
}

/// The rest of gorilla's `Upgrader.Upgrade` checks, which run after the upstream dial:
/// version 13 among the offered versions and a challenge key of 16 base64 bytes.
fn acceptable_handshake(headers: &HeaderMap) -> bool {
    use base64::Engine;
    let key = header_text(headers, header::SEC_WEBSOCKET_KEY);
    has_token(headers, header::SEC_WEBSOCKET_VERSION, "13")
        && base64::engine::general_purpose::STANDARD
            .decode(key.as_bytes())
            .is_ok_and(|k| k.len() == 16)
}

/// Lets axum's upgrade extractor accept what gorilla accepts: it wants exactly
/// `Upgrade: websocket` and `Sec-WebSocket-Version: 13`, gorilla token lists. Neither
/// header is forwarded upstream.
pub(super) async fn normalize_upgrade(mut req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let headers = req.headers_mut();
    if is_upgrade(headers) && has_token(headers, header::SEC_WEBSOCKET_VERSION, "13") {
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(header::SEC_WEBSOCKET_VERSION, HeaderValue::from_static("13"));
    }
    next.run(req).await
}

/// gorilla `Subprotocols`: the first `Sec-WebSocket-Protocol` header, comma separated.
fn subprotocols(headers: &HeaderMap) -> Vec<String> {
    let value = header_text(headers, header::SEC_WEBSOCKET_PROTOCOL);
    let value = value.trim();
    if value.is_empty() {
        return Vec::new();
    }
    value.split(',').map(|p| p.trim().to_owned()).collect()
}

fn upgrade_required(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    response
}

/// gorilla `Upgrader.returnError` for a handshake it cannot complete.
fn bad_handshake() -> Response {
    let mut response = (StatusCode::BAD_REQUEST, "Bad Request\n").into_response();
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert(header::SEC_WEBSOCKET_VERSION, HeaderValue::from_static("13"));
    response
}

/// Everything a WebSocket route receives.
pub(super) struct Inbound {
    rt: Arc<Runtime>,
    live: Arc<Live>,
    principal: Principal,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
}

/// `GET /v1/live/{call_id}`.
pub(super) async fn live_sideband(
    State(rt): State<Arc<Runtime>>,
    Extension(live): Extension<Arc<Live>>,
    Extension(principal): Extension<Principal>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let inbound = Inbound {
        rt,
        live,
        principal,
        headers,
        ws,
    };
    sideband(inbound, false, Sideband::Frameless, path_param(path)).await
}

/// `GET /v1/realtime/calls/{call_id}`.
pub(super) async fn calls_sideband(
    State(rt): State<Arc<Runtime>>,
    Extension(live): Extension<Arc<Live>>,
    Extension(principal): Extension<Principal>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let inbound = Inbound {
        rt,
        live,
        principal,
        headers,
        ws,
    };
    sideband(inbound, true, Sideband::Calls, path_param(path)).await
}

/// `GET /v1/realtime` (`HandleRealtimeWebsocket`): a sideband when `call_id` is given,
/// else a standard Realtime session.
pub(super) async fn realtime(
    State(rt): State<Arc<Runtime>>,
    Extension(live): Extension<Arc<Live>>,
    Extension(principal): Extension<Principal>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let query = |name| {
        crate::access::query_get(uri.query().unwrap_or_default(), name)
            .map(|v| String::from_utf8_lossy(&v).trim().to_owned())
            .unwrap_or_default()
    };
    let inbound = Inbound {
        rt,
        live,
        principal,
        headers,
        ws,
    };
    let call_id = query("call_id");
    if !call_id.is_empty() {
        return sideband(inbound, true, Sideband::Query, call_id).await;
    }
    direct(inbound, query("model")).await
}

fn path_param(path: Result<Path<String>, PathRejection>) -> String {
    path.map(|Path(id)| id.trim().to_owned()).unwrap_or_default()
}

/// `HandleSideband`.
async fn sideband(inbound: Inbound, realtime: bool, style: Sideband, call_id: String) -> Response {
    let Inbound {
        rt,
        live,
        principal,
        headers,
        ws,
    } = inbound;
    let fail = |status, message: &str| live_error(realtime, status, message);
    if !is_upgrade(&headers) {
        return upgrade_required(fail(426, "WebSocket upgrade required"));
    }
    if !live::valid_call_id(&call_id) {
        return fail(400, "Invalid Codex live call ID");
    }
    let guard = match live.calls.claim(&call_id) {
        Claim::Acquired(call) => ClaimGuard::new(live.calls.clone(), call),
        Claim::Busy => return fail(409, "Codex live session already joining"),
        Claim::Missing => return fail(404, "Codex live session not found"),
    };
    let call = guard.call.clone();
    if let Some(secret) = &principal.secret {
        if call.secret_principal.is_empty() || secret.principal != call.secret_principal {
            return realtime_error(
                403,
                "Realtime client secret is not valid for this call",
                "invalid_request_error",
                "realtime_client_secret_scope_mismatch",
            );
        }
    } else if !call.owner_key.is_empty()
        && (principal.key != call.owner_key || principal.provider != call.owner_provider)
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
    // Go: the call's Home pick (a drained one ends the call), else a pick pinned to the
    // call's credential that lasts as long as this relay.
    let (credential, lease) = match &call.home {
        Some(hold) => match hold.active() {
            Some(credential) => (credential, None),
            None => {
                let mut guard = guard;
                guard.consume = true;
                return fail(503, "Codex live Home selection unavailable");
            }
        },
        None => {
            let lease = match select_oauth(
                &rt,
                &cfg,
                Some(&call.auth_id),
                &selection_headers,
                &[],
                Some(&call_id),
                &call.model,
                "websocket",
            )
            .await
            {
                Ok(lease) => lease,
                Err(rejection) => return rejection.render(realtime),
            };
            (lease.credential.clone(), Some(lease))
        }
    };
    let home = call.home.is_some() || lease.as_ref().is_some_and(|lease| lease.is_remote());
    let trace = trace(&credential);
    let target = LiveTarget {
        credential: &credential,
        cfg: &cfg,
        client: &headers,
        session: header_session(&selection_headers, &[], Some(&call_id), &call.session_id),
        capture: Default::default(),
    };
    let url = live::sideband_url(&rt.executors.codex.live_endpoints().api_base, style, &call_id);
    let drained = match &lease {
        Some(lease) => super::drained(Some(lease)),
        None => call
            .home
            .as_ref()
            .and_then(|hold| hold.drained())
            .unwrap_or_else(|| Box::pin(std::future::pending())),
    };
    let dial =
        rt.executors
            .codex
            .live_websocket(&target, &url, live::protocol_headers(&headers), subprotocols(&headers));
    let upstream = match until_drained_dial(dial, drained).await {
        Ok(upstream) => upstream,
        Err(error) => {
            if home {
                report_dial_unauthorized(
                    &rt,
                    &credential,
                    &call.model,
                    &error,
                    (call.session_id.clone(), String::new()),
                );
            }
            return with_trace(
                dial_failed(error, |status| fail(status, "Codex live sideband upstream unavailable")),
                &trace,
            );
        }
    };
    let Some(ws) = ws.ok().filter(|_| acceptable_handshake(&headers)) else {
        return with_trace(bad_handshake(), &trace);
    };
    upgrade(ws, upstream, move |relay| {
        let mut guard = guard;
        guard.consume = true;
        guard.call.resources.add(relay.abort_handle());
        // Go binds both sockets to the pick: Home draining it closes the relay.
        if let Some(hold) = &guard.call.home {
            hold.bind(relay.abort_handle());
        }
        let drained = super::drained(lease.as_ref());
        async move {
            let _ = until_drained(relay, drained).await;
            // Ending the call closes its media; a temporary pick ends once that close
            // finished, as a stored call's selection does.
            let media = guard.call.media.clone();
            drop(guard);
            match media {
                Some(media) => media.after_close(Box::new(move || drop(lease))),
                None => drop(lease),
            }
        }
    })
}

/// An upstream dial on the pick's attempt context: Home draining the pick fails it as a
/// dial without a response (Go cancels the handshake), dropping the half-open socket.
async fn until_drained_dial(
    dial: impl std::future::Future<Output = Result<LiveSocket, DialError>>,
    drained: impl std::future::Future<Output = ()> + Unpin,
) -> Result<LiveSocket, DialError> {
    tokio::select! {
        biased;
        _ = drained => Err(DialError {
            response: None,
            message: "context canceled".into(),
        }),
        dialed = dial => dialed,
    }
}

/// Waits for `relay`, aborting it once `drained` resolves (Go binds the sockets to the
/// Home selection).
async fn until_drained(
    relay: tokio::task::JoinHandle<()>,
    drained: futures_util::future::BoxFuture<'static, ()>,
) -> Result<(), tokio::task::JoinError> {
    let abort = relay.abort_handle();
    tokio::select! {
        result = relay => result,
        _ = drained => {
            abort.abort();
            Ok(())
        }
    }
}

/// Go `ReportHomeUnauthorized` for a rejected upstream handshake: the handshake body,
/// else the dial error's text.
fn report_dial_unauthorized(
    rt: &Runtime,
    credential: &cpa_core::credential::Credential,
    model: &str,
    error: &DialError,
    session: (String, String),
) {
    let Some(handshake) = error.response.as_ref().filter(|h| h.status == 401) else {
        return;
    };
    let body = if handshake.body.is_empty() {
        error.message.as_bytes()
    } else {
        &handshake.body
    };
    super::report_unauthorized(rt, credential, model, body, session);
}

/// `HandleDirectWebsocket`.
async fn direct(inbound: Inbound, model: String) -> Response {
    let Inbound {
        rt,
        principal,
        headers,
        ws,
        ..
    } = inbound;
    if !is_upgrade(&headers) {
        return upgrade_required(realtime_error(
            426,
            "WebSocket upgrade required",
            "invalid_request_error",
            "websocket_upgrade_required",
        ));
    }
    let requested = if model.is_empty() {
        live::DEFAULT_STANDARD_MODEL.to_owned()
    } else {
        model
    };
    let selection_model = live::codex_model(&requested);
    let secret_session = principal.secret.as_ref().map(|g| g.session.clone()).unwrap_or_default();
    if !secret_session.is_empty() && live::codex_model(&live::model_from_json(&secret_session)) != selection_model {
        return realtime_error(
            403,
            "Realtime client secret is not valid for the requested model",
            "invalid_request_error",
            "realtime_client_secret_scope_mismatch",
        );
    }
    let cfg = rt.config();
    let selection_headers = principal.selection_headers(&headers);
    let lease = match select_oauth(
        &rt,
        &cfg,
        None,
        &selection_headers,
        &[],
        None,
        &selection_model,
        "websocket",
    )
    .await
    {
        Ok(lease) => lease,
        Err(rejection) => return rejection.render(true),
    };
    let credential = lease.credential.clone();
    let trace = trace(&credential);
    let traced = |response| with_trace(response, &trace);
    let target = LiveTarget {
        credential: &credential,
        cfg: &cfg,
        client: &headers,
        session: header_session(&selection_headers, &[], None, ""),
        capture: Default::default(),
    };
    let url = live::direct_url(&rt.executors.codex.live_endpoints().api_base, &requested);
    let mut drained = super::drained(Some(&lease));
    let dial = rt
        .executors
        .codex
        .live_websocket(&target, &url, live::direct_headers(&headers), subprotocols(&headers));
    let mut upstream = match until_drained_dial(dial, &mut drained).await {
        Ok(upstream) => upstream,
        Err(error) => {
            if lease.is_remote() {
                report_dial_unauthorized(
                    &rt,
                    &credential,
                    &selection_model,
                    &error,
                    super::session_ids(&selection_headers, &[]),
                );
            }
            return traced(dial_failed(error, |status| {
                let (status, message, kind, code) = match status {
                    404 | 501 => (
                        501,
                        "Direct Realtime WebSocket is not supported by the Codex OAuth upstream",
                        "not_supported_error",
                        "realtime_capability_not_supported",
                    ),
                    401 => (
                        401,
                        "Codex Realtime WebSocket upstream unavailable",
                        "authentication_error",
                        "realtime_upstream_unauthorized",
                    ),
                    _ => (
                        status,
                        "Codex Realtime WebSocket upstream unavailable",
                        "api_error",
                        "realtime_websocket_upstream_unavailable",
                    ),
                };
                realtime_error(status, message, kind, code)
            }));
        }
    };
    if !secret_session.is_empty() {
        let Ok(session) = live::session_update(&secret_session) else {
            return traced(realtime_error(
                500,
                "Failed to apply Realtime client secret session",
                "server_error",
                "realtime_session_failed",
            ));
        };
        let mut update = br#"{"type":"session.update","session":"#.to_vec();
        update.extend_from_slice(&cpa_common::json::compact(&session, true));
        update.push(b'}');
        let update = String::from_utf8_lossy(&update).into_owned();
        // The write runs on the pick's attempt context too.
        let sent = tokio::select! {
            biased;
            _ = &mut drained => false,
            sent = upstream.socket.send(up::Message::text(update)) => sent.is_ok(),
        };
        if !sent {
            return traced(realtime_error(
                502,
                "Failed to apply Realtime client secret session",
                "api_error",
                "realtime_upstream_unavailable",
            ));
        }
    }
    let Some(ws) = ws.ok().filter(|_| acceptable_handshake(&headers)) else {
        return traced(bad_handshake());
    };
    // Go `selection.Retain()` until the session closes; Home draining the pick closes
    // the sockets bound to it.
    upgrade(ws, upstream, move |relay| {
        let drained = super::drained(Some(&lease));
        async move {
            let _ = until_drained(relay, drained).await;
            drop(lease);
        }
    })
}

/// A rejected or failed upstream dial (`handleSidebandDialError` and its direct twin): the
/// upstream's own 401 passes through with its body; anything else becomes `other(status)`.
fn dial_failed(error: DialError, other: impl FnOnce(u16) -> Response) -> Response {
    let Some(handshake) = error.response else {
        tracing::debug!(error = %error.message, "codex live upstream dial failed");
        return other(502);
    };
    let mut response = if handshake.status == 401 {
        let mut headers = HeaderMap::new();
        if let Some(content_type) = handshake.headers.get(header::CONTENT_TYPE).filter(|v| !v.is_empty()) {
            headers.insert(header::CONTENT_TYPE, content_type.clone());
        }
        super::http::written(401, headers, handshake.body)
    } else {
        other(handshake.status)
    };
    copy_headers(response.headers_mut(), &handshake.headers, &HANDSHAKE_HEADERS);
    response
}

/// Completes the downstream upgrade with the upstream's subprotocol, then runs the
/// relay as its own task (so a hangup can abort it) inside `run`.
fn upgrade<F, Fut>(ws: WebSocketUpgrade, upstream: LiveSocket, run: F) -> Response
where
    F: FnOnce(tokio::task::JoinHandle<()>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let mut ws = ws
        .max_message_size(live::MAX_WS_MESSAGE)
        .max_frame_size(live::MAX_WS_MESSAGE);
    if let Some(protocol) = upstream.protocol.as_deref().and_then(|p| HeaderValue::from_str(p).ok()) {
        ws.set_selected_protocol(protocol);
    }
    let upstream = upstream.socket;
    ws.on_upgrade(move |downstream| async move {
        run(tokio::spawn(relay(downstream, upstream))).await;
    })
}

/// How one relay direction ended.
#[derive(Debug)]
enum End {
    /// A close frame: its code and reason, if any.
    Closed(Option<(u16, String)>),
    /// The peer went away: the stream ended, or the connection closed without a close
    /// frame (gorilla's 1006, `io.EOF`, `net.ErrClosed`).
    Gone,
    /// Reading failed for another reason: a protocol violation or a socket error.
    Failed,
    /// Writing to the other side failed.
    WriteFailed,
}

/// A read error as gorilla reports it: an abnormal closure (connection closed without a
/// close frame) counts as the peer going away; anything else is a relay failure.
fn read_end(error: &(dyn std::error::Error + 'static)) -> End {
    let mut current = Some(error);
    while let Some(e) = current {
        // tungstenite's `ResetWithoutClosingHandshake`, `ConnectionClosed`, `AlreadyClosed`;
        // the crate is not a direct dependency, so they are recognised by their text.
        let text = e.to_string();
        if text.contains("Connection reset without closing handshake")
            || text.contains("Connection closed normally")
            || text.contains("Trying to work with closed connection")
        {
            return End::Gone;
        }
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            return if io.kind() == std::io::ErrorKind::UnexpectedEof {
                End::Gone
            } else {
                End::Failed
            };
        }
        current = e.source();
    }
    End::Failed
}

/// `websocketCloseDetails`: what both sides are told when one direction stops.
fn close_details(end: End) -> (u16, String) {
    match end {
        End::Closed(Some((code, reason))) if !matches!(code, 1005 | 1006 | 1015) => (code, reason),
        End::Closed(_) | End::Gone => (1000, String::new()),
        End::Failed | End::WriteFailed => (1011, "relay closed".into()),
    }
}

/// `relayWebsockets`.
async fn relay(downstream: WebSocket, upstream: wreq::ws::WebSocket) {
    let (mut down_tx, mut down_rx) = downstream.split();
    let (mut up_tx, mut up_rx) = upstream.split();
    let end = {
        let to_upstream = async {
            loop {
                let message = match down_rx.next().await {
                    Some(Ok(message)) => message,
                    Some(Err(e)) => return read_end(&e),
                    None => return End::Gone,
                };
                let message = match message {
                    ws::Message::Text(text) => up::Message::text(text.as_str()),
                    ws::Message::Binary(data) => up::Message::binary(data),
                    ws::Message::Ping(_) | ws::Message::Pong(_) => continue,
                    ws::Message::Close(frame) => {
                        return End::Closed(frame.map(|f| (f.code, f.reason.as_str().to_owned())));
                    }
                };
                if up_tx.send(message).await.is_err() {
                    return End::WriteFailed;
                }
            }
        };
        let to_downstream = async {
            loop {
                let message = match up_rx.next().await {
                    Some(Ok(message)) => message,
                    Some(Err(e)) => return read_end(&e),
                    None => return End::Gone,
                };
                let message = match message {
                    up::Message::Text(text) => ws::Message::Text(text.as_str().into()),
                    up::Message::Binary(data) => ws::Message::Binary(data),
                    up::Message::Ping(_) | up::Message::Pong(_) => continue,
                    up::Message::Close(frame) => {
                        return End::Closed(frame.map(|f| (u16::from(f.code), f.reason.as_str().to_owned())));
                    }
                };
                if down_tx.send(message).await.is_err() {
                    return End::WriteFailed;
                }
            }
        };
        tokio::select! {
            end = to_upstream => end,
            end = to_downstream => end,
        }
    };
    let (code, reason) = close_details(end);
    let down_close = ws::Message::Close(Some(ws::CloseFrame {
        code,
        reason: reason.as_str().into(),
    }));
    let up_close = up::Message::Close(Some(up::CloseFrame {
        code: code.into(),
        reason: reason.as_str().into(),
    }));
    // Both sides get the close; a side that closed first gets its close echoed instead.
    // Closing the sinks flushes those frames before the sockets drop.
    let close_down = async {
        let _ = down_tx.send(down_close).await;
        let _ = down_tx.close().await;
    };
    let close_up = async {
        let _ = up_tx.send(up_close).await;
        let _ = up_tx.close().await;
    };
    let _ = tokio::time::timeout(CLOSE_WRITE, futures_util::future::join(close_down, close_up)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_detection_and_subprotocols_follow_gorilla() {
        let mut headers = HeaderMap::new();
        assert!(!is_upgrade(&headers));
        headers.insert(header::CONNECTION, "keep-alive, Upgrade".parse().unwrap());
        assert!(!is_upgrade(&headers));
        headers.insert(header::UPGRADE, "WebSocket".parse().unwrap());
        assert!(is_upgrade(&headers));
        headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            " realtime , openai-insecure-api-key.x ".parse().unwrap(),
        );
        headers.append(header::SEC_WEBSOCKET_PROTOCOL, "ignored".parse().unwrap());
        assert_eq!(subprotocols(&headers), ["realtime", "openai-insecure-api-key.x"]);
    }

    #[test]
    fn close_codes_follow_go() {
        assert_eq!(
            close_details(End::Closed(Some((4001, "bye".into())))),
            (4001, "bye".into())
        );
        assert_eq!(
            close_details(End::Closed(Some((1006, "x".into())))),
            (1000, String::new())
        );
        assert_eq!(close_details(End::Closed(None)), (1000, String::new()));
        assert_eq!(close_details(End::Gone), (1000, String::new()));
        assert_eq!(close_details(End::WriteFailed), (1011, "relay closed".into()));
        assert_eq!(close_details(End::Failed), (1011, "relay closed".into()));
    }

    #[test]
    fn read_errors_classify_like_gorilla() {
        let io = |kind| std::io::Error::new(kind, "x");
        assert!(matches!(read_end(&io(std::io::ErrorKind::UnexpectedEof)), End::Gone));
        assert!(matches!(
            read_end(&io(std::io::ErrorKind::ConnectionReset)),
            End::Failed
        ));
        let abrupt = std::io::Error::other("WebSocket protocol error: Connection reset without closing handshake");
        assert!(matches!(read_end(&abrupt), End::Gone));
        let rsv = std::io::Error::other("WebSocket protocol error: Reserved bits are non-zero");
        assert!(matches!(read_end(&rsv), End::Failed));
    }
}
