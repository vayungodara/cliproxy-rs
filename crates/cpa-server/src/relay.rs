//! `/v1/ws`: the AI Studio relay route (sdk/cliproxy service_auth.go
//! `ensureWebsocketGateway`, internal/api `AttachWebsocketRoute`, internal/wsrelay).
//!
//! A browser running the AI Studio bridge connects here. Its session is registered with
//! the executors' [`Relay`] under a random `aistudio-…` name, and a runtime-only
//! `aistudio` credential of that name serves requests until the socket closes. Client
//! keys are required while `ws-auth` is on (Go's default).
//!
//! The transport follows gorilla and session.go: a ping every 30 seconds, a read
//! deadline of 60 seconds that only a pong extends, 10-second writes, frames up to
//! 64 MiB; text and binary frames are both read as JSON messages.

use std::sync::{Arc, Weak};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::wsrelay::{self, Relay};
use futures_util::{SinkExt, StreamExt};
use tokio::time::{Instant, sleep_until, timeout};

use crate::Runtime;

const READ_TIMEOUT: Duration = Duration::from_secs(60);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT: Duration = Duration::from_secs(30);
/// `maxInboundMessageLen`.
const MAX_MESSAGE: usize = 64 << 20;

/// `ws-auth` (`oauth.providers.aistudio.ws-auth`): on unless the config turns it off.
pub fn ws_auth(cfg: &Config) -> bool {
    cfg.document
        .get("oauth")
        .and_then(|o| o.get("providers"))
        .and_then(|p| p.get("aistudio"))
        .and_then(|a| a.get("ws-auth"))
        .and_then(serde_yaml_ng::Value::as_bool)
        .unwrap_or(true)
}

/// The route, with the relay's session events wired to the runtime's credentials.
pub(crate) fn routes(rt: &Arc<Runtime>) -> Router<Arc<Runtime>> {
    let weak = Arc::downgrade(rt);
    rt.executors
        .google
        .aistudio
        .relay
        .set_observer(Arc::new(move |provider, cause| observe(&weak, provider, cause)));
    Router::new()
        .route(wsrelay::PATH, get(upgrade))
        .route_layer(middleware::from_fn_with_state(rt.clone(), conditional_auth))
}

/// Go `wsOnConnected` / `wsOnDisconnected`.
fn observe(rt: &Weak<Runtime>, provider: &str, cause: Option<&str>) {
    let Some(rt) = rt.upgrade() else {
        return;
    };
    match cause {
        None => {
            if !provider.to_lowercase().starts_with("aistudio-") {
                return;
            }
            if rt.store().add_runtime(Credential::relay_session(provider)) {
                tracing::info!("websocket provider connected: {provider}");
            }
        }
        Some(cause) if cause.contains("replaced by new connection") => {
            tracing::info!("websocket provider replaced: {provider}");
        }
        Some(cause) => {
            tracing::warn!("websocket provider disconnected: {provider} ({cause})");
            rt.store().remove_runtime(provider);
        }
    }
}

/// Go's conditional auth: the client-key middleware only while `ws-auth` is on.
async fn conditional_auth(State(rt): State<Arc<Runtime>>, req: Request, next: Next) -> Response {
    if !ws_auth(&rt.config()) {
        return next.run(req).await;
    }
    crate::access::require_client_key(State(rt), req, next).await
}

async fn upgrade(State(rt): State<Arc<Runtime>>, ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>) -> Response {
    let Ok(ws) = ws else {
        return crate::websocket::upgrade_rejected();
    };
    let relay = rt.executors.google.aistudio.relay.clone();
    ws.max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| serve(relay, socket))
}

/// gorilla's `CloseError` text for a close frame.
fn close_error(frame: Option<&CloseFrame>) -> String {
    let (code, reason) = frame.map_or((1005, ""), |f| (f.code, f.reason.as_str()));
    let name = match code {
        1000 => "normal",
        1001 => "going away",
        1002 => "protocol error",
        1003 => "unsupported data",
        1005 => "no status",
        1006 => "abnormal closure",
        1007 => "invalid payload data",
        1008 => "policy violation",
        1009 => "message too big",
        1010 => "mandatory extension missing",
        1011 => "internal server error",
        1012 => "service restart",
        1013 => "try again later",
        1015 => "TLS handshake error",
        _ => "",
    };
    let mut text = format!("websocket: close {code}");
    if !name.is_empty() {
        text.push_str(&format!(" ({name})"));
    }
    if !reason.is_empty() {
        text.push_str(&format!(": {reason}"));
    }
    text
}

/// One connection: frames out from the session, frames in to it, the heartbeat and the
/// read deadline, until either side ends.
// ponytail: transport failures carry this module's wording, not Go's net errors.
async fn serve(relay: Arc<Relay>, socket: WebSocket) {
    let (session, mut frames) = relay.connect();
    let (mut sink, mut stream) = socket.split();
    let mut closed = session.closed();
    let mut heartbeat = tokio::time::interval_at(Instant::now() + HEARTBEAT, HEARTBEAT);
    let mut deadline = Instant::now() + READ_TIMEOUT;
    loop {
        tokio::select! {
            frame = frames.recv() => {
                let Some(frame) = frame else { break };
                match timeout(WRITE_TIMEOUT, sink.send(Message::Text(frame.into()))).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        session.close(&format!("write json: {e}"));
                        break;
                    }
                    Err(_) => {
                        session.close("write json: i/o timeout");
                        break;
                    }
                }
            }
            message = stream.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    if session.receive(text.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Binary(data))) => {
                    if session.receive(&data).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Pong(_))) => deadline = Instant::now() + READ_TIMEOUT,
                Some(Ok(Message::Ping(_))) => {}
                Some(Ok(Message::Close(frame))) => {
                    session.close(&close_error(frame.as_ref()));
                    break;
                }
                Some(Err(e)) => {
                    session.close(&e.to_string());
                    break;
                }
                None => {
                    session.close("websocket: close 1006 (abnormal closure): unexpected EOF");
                    break;
                }
            },
            _ = heartbeat.tick() => {
                let ping = sink.send(Message::Ping(axum::body::Bytes::from_static(b"ping")));
                if !matches!(timeout(WRITE_TIMEOUT, ping).await, Ok(Ok(()))) {
                    session.close("websocket: ping failed");
                    break;
                }
            }
            _ = sleep_until(deadline) => {
                session.close("websocket: read deadline exceeded");
                break;
            }
            _ = wsrelay::wait_closed(&mut closed) => break,
        }
    }
    session.close(wsrelay::CLOSED);
    let _ = sink.close().await;
}
