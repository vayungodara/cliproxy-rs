//! The `/v1/ws` relay (internal/wsrelay): an AI Studio browser bridge connects over a
//! websocket and performs HTTP requests on the proxy's behalf. Each connection is a
//! session named `aistudio-<16 random characters>`; requests and their responses are
//! JSON messages correlated by ID.
//!
//! The server route owns the transport (upgrade, ping heartbeat, read deadline, frame
//! limit) and feeds frames in and out of a [`Session`]; this module owns the sessions,
//! Go's message encoding and the request bookkeeping, shared with the AI Studio executor
//! through one [`Relay`].

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use cpa_common::json::{self as gj, GoValue};
use tokio::sync::{mpsc, watch};

use crate::meta_wire;

/// The route Go attaches the relay to (`wsrelay.Options.Path`).
pub const PATH: &str = "/v1/ws";

pub const HTTP_REQUEST: &str = "http_request";
pub const HTTP_RESPONSE: &str = "http_response";
pub const STREAM_START: &str = "stream_start";
pub const STREAM_CHUNK: &str = "stream_chunk";
pub const STREAM_END: &str = "stream_end";
pub const ERROR: &str = "error";
pub const PING: &str = "ping";
pub const PONG: &str = "pong";

/// `pendingChannelBuffer`: messages a request may hold before the reader waits.
const PENDING_BUFFER: usize = 64;

/// The cause sessions end with when the reader stops without an error of its own.
pub const CLOSED: &str = "websocket session closed";

/// `wsrelay.Message`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Message {
    pub id: String,
    pub kind: String,
    /// Go's `map[string]any`; `None` when absent or `null`.
    pub payload: Option<BTreeMap<String, GoValue>>,
}

impl Message {
    /// `conn.WriteJSON(msg)`: `id`, `type`, then a non-empty `payload` (keys sorted,
    /// HTML escaped), and the encoder's newline.
    pub fn encode(&self) -> String {
        let mut out = Vec::new();
        out.extend_from_slice(b"{\"id\":");
        gj::marshal_str(&mut out, self.id.as_bytes(), true);
        out.extend_from_slice(b",\"type\":");
        gj::marshal_str(&mut out, self.kind.as_bytes(), true);
        if let Some(payload) = self.payload.as_ref().filter(|p| !p.is_empty()) {
            out.extend_from_slice(b",\"payload\":");
            out.extend_from_slice(&GoValue::Object(payload.clone()).marshal());
        }
        out.extend_from_slice(b"}\n");
        String::from_utf8(out).unwrap_or_default()
    }

    /// `conn.ReadJSON(&msg)`: a `json.Decoder` reads the frame's first value into
    /// `Message`. Anything after that value is never read; any decoding error ends the
    /// session. Names match case-insensitively and a later member wins; `null` leaves a
    /// string as it was but clears the payload, and payload objects merge into one map,
    /// as Go's decoder does.
    pub fn decode(frame: &[u8]) -> Result<Self, String> {
        let start = frame
            .iter()
            .position(|c| !matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
            .ok_or("EOF")?;
        let value = &frame[start..];
        if value.starts_with(b"null") {
            return Ok(Self::default());
        }
        if !value.starts_with(b"{") {
            meta_wire::check_valid(value)?;
            return Err("json: cannot unmarshal into Go value of type wsrelay.Message".into());
        }
        let value = &value[..meta_wire::object_end(value).ok_or("unexpected EOF")?];
        meta_wire::check_valid(value)?;
        let mut msg = Self::default();
        let mut error = None;
        gj::parse(value).each(|key, item| {
            let key = gj::go_unquote(key.raw()).unwrap_or_default();
            let field = ["id", "type", "payload"]
                .into_iter()
                .find(|name| *name == key)
                .or_else(|| {
                    ["id", "type", "payload"]
                        .into_iter()
                        .find(|name| meta_wire::fold(name) == meta_wire::fold(&key))
                });
            let result = match (field, item.kind) {
                // `null` resets a map to nil and leaves a string as it was.
                (Some("payload"), gj::Kind::Null) => {
                    msg.payload = None;
                    Ok(())
                }
                (None, _) | (Some(_), gj::Kind::Null) => Ok(()),
                (Some("id"), gj::Kind::String) => {
                    msg.id = gj::go_unquote(item.raw()).unwrap_or_default();
                    Ok(())
                }
                (Some("type"), gj::Kind::String) => {
                    msg.kind = gj::go_unquote(item.raw()).unwrap_or_default();
                    Ok(())
                }
                (Some("payload"), gj::Kind::Json) if item.is_object() => match GoValue::parse_f64(item.raw()) {
                    Some(GoValue::Object(map)) => {
                        msg.payload.get_or_insert_with(BTreeMap::new).extend(map);
                        Ok(())
                    }
                    _ => Err("json: cannot unmarshal number into Go value of type float64".to_owned()),
                },
                (Some(name), _) => Err(format!(
                    "json: cannot unmarshal {} into Go struct field Message.{name}",
                    meta_wire::kind_name(&item)
                )),
            };
            if let Err(e) = result {
                error.get_or_insert(e);
            }
            true
        });
        error.map_or(Ok(msg), Err)
    }

    fn payload_str(&self, key: &str) -> Option<&str> {
        match self.payload.as_ref()?.get(key)? {
            GoValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// A float64 payload field as Go's `int(v)`.
    fn payload_int(&self, key: &str) -> Option<i64> {
        match self.payload.as_ref()?.get(key)? {
            GoValue::Number(n) => n.parse::<f64>().ok().map(|f| f as i64),
            _ => None,
        }
    }
}

/// An HTTP request carried over the relay (`wsrelay.HTTPRequest`). Headers keep Go's
/// canonical names, each with its values.
#[derive(Debug, Clone, Default)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, Vec<String>)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// `encodeRequest`. `body` is a Go string, so invalid UTF-8 becomes U+FFFD per byte
    /// when marshaled; `sent_at` is the wall clock in RFC 3339 with nanoseconds.
    fn payload(&self) -> BTreeMap<String, GoValue> {
        let headers = self
            .headers
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    GoValue::Array(v.iter().cloned().map(GoValue::String).collect()),
                )
            })
            .collect();
        BTreeMap::from([
            ("method".to_owned(), GoValue::String(self.method.clone())),
            ("url".to_owned(), GoValue::String(self.url.clone())),
            ("headers".to_owned(), GoValue::Object(headers)),
            ("body".to_owned(), GoValue::String(go_lossy(&self.body))),
            ("sent_at".to_owned(), GoValue::String(rfc3339_nano(chrono::Utc::now()))),
        ])
    }
}

/// An HTTP response from the relay (`wsrelay.HTTPResponse`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HttpResponse {
    pub status: i64,
    /// Canonical names with their values, in name order (Go's `http.Header`).
    pub headers: BTreeMap<String, Vec<String>>,
    pub body: Vec<u8>,
}

/// One event of a streamed relay response (`wsrelay.StreamEvent`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamEvent {
    pub kind: String,
    pub payload: Vec<u8>,
    pub status: i64,
    pub headers: BTreeMap<String, Vec<String>>,
    pub error: Option<String>,
}

/// Each invalid UTF-8 byte becomes U+FFFD, as Go's `string` marshaling does.
fn go_lossy(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        out.extend(std::iter::repeat_n('\u{FFFD}', chunk.invalid().len()));
    }
    out
}

/// Go's `time.RFC3339Nano`: fractional seconds without trailing zeros.
fn rfc3339_nano(t: chrono::DateTime<chrono::Utc>) -> String {
    let nanos = format!("{:09}", t.timestamp_subsec_nanos());
    let nanos = nanos.trim_end_matches('0');
    let fraction = if nanos.is_empty() {
        String::new()
    } else {
        format!(".{nanos}")
    };
    format!("{}{fraction}Z", t.format("%Y-%m-%dT%H:%M:%S"))
}

/// `decodeResponse`.
fn decode_response(msg: &Message) -> HttpResponse {
    let Some(payload) = &msg.payload else {
        return HttpResponse {
            status: 502,
            ..HttpResponse::default()
        };
    };
    let mut resp = HttpResponse {
        status: msg.payload_int("status").unwrap_or(200),
        ..HttpResponse::default()
    };
    if let Some(GoValue::Object(headers)) = payload.get("headers") {
        for (key, raw) in headers {
            let name = crate::proxy::canonical_header(key);
            match raw {
                GoValue::Array(items) => {
                    for item in items {
                        if let GoValue::String(s) = item {
                            resp.headers.entry(name.clone()).or_default().push(s.clone());
                        }
                    }
                }
                GoValue::String(s) => {
                    resp.headers.insert(name, vec![s.clone()]);
                }
                _ => {}
            }
        }
    }
    if let Some(body) = msg.payload_str("body") {
        resp.body = body.as_bytes().to_vec();
    }
    resp
}

/// `decodeChunk`.
fn decode_chunk(msg: &Message) -> Vec<u8> {
    msg.payload_str("data")
        .map(|d| d.as_bytes().to_vec())
        .unwrap_or_default()
}

/// `decodeError`.
fn decode_error(msg: &Message) -> String {
    if msg.payload.is_none() {
        return "wsrelay: unknown error".into();
    }
    let message = msg
        .payload_str("error")
        .filter(|m| !m.is_empty())
        .unwrap_or("wsrelay: upstream error");
    format!("{message} (status={})", msg.payload_int("status").unwrap_or(0))
}

fn terminal(kind: &str) -> bool {
    matches!(kind, HTTP_RESPONSE | ERROR | STREAM_END)
}

/// Called with `(provider, None)` when a browser connects and `(provider, Some(cause))`
/// when its session ends (Go `OnConnected` / `OnDisconnected`).
pub type Observer = Arc<dyn Fn(&str, Option<&str>) + Send + Sync>;

/// The relay's sessions by provider name (`wsrelay.Manager`).
#[derive(Default)]
pub struct Relay {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    observer: Mutex<Option<Observer>>,
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relay").finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `randomProviderName`: `aistudio-` and 16 characters of `[a-z0-9]`.
fn random_provider_name() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    let name: String = bytes
        .iter()
        .map(|b| char::from(ALPHABET[usize::from(*b) % ALPHABET.len()]))
        .collect();
    format!("aistudio-{name}")
}

impl Relay {
    pub fn set_observer(&self, observer: Observer) {
        *lock(&self.observer) = Some(observer);
    }

    fn observer(&self) -> Option<Observer> {
        lock(&self.observer).clone()
    }

    /// `handleWebsocket` after the upgrade: a session under a new provider name, which
    /// replaces any session of that name, then `OnConnected`. Returns the session and the
    /// frames the route must write.
    pub fn connect(self: &Arc<Self>) -> (Arc<Session>, mpsc::UnboundedReceiver<String>) {
        self.connect_as(random_provider_name())
    }

    /// [`Relay::connect`] under a given name (tests).
    pub fn connect_as(self: &Arc<Self>, provider: String) -> (Arc<Session>, mpsc::UnboundedReceiver<String>) {
        let (out, frames) = mpsc::unbounded_channel();
        let (closed, _) = watch::channel(false);
        let session = Arc::new(Session {
            provider: provider.to_lowercase(),
            relay: Arc::downgrade(self),
            out,
            closed,
            pending: Mutex::new(HashMap::new()),
        });
        let replaced = lock(&self.sessions).insert(session.provider.clone(), session.clone());
        if let Some(replaced) = replaced {
            replaced.close("replaced by new connection");
        }
        if let Some(observer) = self.observer() {
            observer(&session.provider, None);
        }
        (session, frames)
    }

    /// `Manager.Stop`: every session ends with "wsrelay: manager stopped".
    pub fn stop(&self) {
        let sessions: Vec<_> = lock(&self.sessions).drain().map(|(_, s)| s).collect();
        for session in sessions {
            session.close("wsrelay: manager stopped");
        }
    }

    /// The connected provider names.
    pub fn providers(&self) -> Vec<String> {
        lock(&self.sessions).keys().cloned().collect()
    }

    fn session(&self, provider: &str) -> Option<Arc<Session>> {
        lock(&self.sessions).get(&provider.trim().to_lowercase()).cloned()
    }

    /// `Manager.Send`.
    fn send(&self, provider: &str, msg: Message) -> Result<Responses, String> {
        let session = self
            .session(provider)
            .ok_or_else(|| format!("wsrelay: provider {provider} not connected"))?;
        session.request(msg)
    }

    fn http_message(req: &HttpRequest) -> Message {
        Message {
            id: uuid::Uuid::new_v4().to_string(),
            kind: HTTP_REQUEST.into(),
            payload: Some(req.payload()),
        }
    }

    /// `Manager.NonStream`: an `http_response`, or the body of a streamed answer
    /// collected until `stream_end` (or the session closing).
    pub async fn non_stream(&self, provider: &str, req: &HttpRequest) -> Result<HttpResponse, String> {
        let mut responses = self.send(provider, Self::http_message(req))?;
        let mut stream: Option<HttpResponse> = None;
        let mut body = Vec::new();
        loop {
            let Some(msg) = responses.rx.recv().await else {
                return match stream {
                    Some(mut resp) => {
                        resp.body = body;
                        Ok(resp)
                    }
                    None => Err("wsrelay: connection closed during response".into()),
                };
            };
            match msg.kind.as_str() {
                HTTP_RESPONSE => {
                    let mut resp = decode_response(&msg);
                    if stream.is_some() && !body.is_empty() && resp.body.is_empty() {
                        resp.body = body;
                    }
                    return Ok(resp);
                }
                ERROR => return Err(decode_error(&msg)),
                STREAM_START => {
                    stream = Some(decode_response(&msg));
                    body.clear();
                }
                STREAM_CHUNK => {
                    stream.get_or_insert_with(|| HttpResponse {
                        status: 200,
                        ..HttpResponse::default()
                    });
                    body.extend_from_slice(&decode_chunk(&msg));
                }
                STREAM_END => {
                    let mut resp = stream.unwrap_or(HttpResponse {
                        status: 200,
                        ..HttpResponse::default()
                    });
                    resp.body = body;
                    return Ok(resp);
                }
                _ => {}
            }
        }
    }

    /// `Manager.Stream`: start, chunk, end, error and complete-response events; a
    /// closed session ends with an "wsrelay: stream closed" error event.
    pub async fn stream(&self, provider: &str, req: &HttpRequest) -> Result<mpsc::Receiver<StreamEvent>, String> {
        let mut responses = self.send(provider, Self::http_message(req))?;
        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(async move {
            loop {
                let Some(msg) = responses.rx.recv().await else {
                    let _ = tx
                        .send(StreamEvent {
                            error: Some("wsrelay: stream closed".into()),
                            ..StreamEvent::default()
                        })
                        .await;
                    return;
                };
                let event = match msg.kind.as_str() {
                    STREAM_START => {
                        let resp = decode_response(&msg);
                        StreamEvent {
                            kind: STREAM_START.into(),
                            status: resp.status,
                            headers: resp.headers,
                            ..StreamEvent::default()
                        }
                    }
                    STREAM_CHUNK => StreamEvent {
                        kind: STREAM_CHUNK.into(),
                        payload: decode_chunk(&msg),
                        ..StreamEvent::default()
                    },
                    STREAM_END => StreamEvent {
                        kind: STREAM_END.into(),
                        ..StreamEvent::default()
                    },
                    ERROR => StreamEvent {
                        kind: ERROR.into(),
                        error: Some(decode_error(&msg)),
                        ..StreamEvent::default()
                    },
                    HTTP_RESPONSE => {
                        let resp = decode_response(&msg);
                        StreamEvent {
                            kind: HTTP_RESPONSE.into(),
                            status: resp.status,
                            headers: resp.headers,
                            payload: resp.body,
                            error: None,
                        }
                    }
                    _ => continue,
                };
                let last = terminal(&event.kind);
                if tx.send(event).await.is_err() || last {
                    return;
                }
            }
        });
        Ok(rx)
    }
}

/// Resolves once `closed` reports the session ended.
pub async fn wait_closed(closed: &mut watch::Receiver<bool>) {
    let _ = closed.wait_for(|c| *c).await;
}

/// One connected browser (`wsrelay.session`).
pub struct Session {
    provider: String,
    relay: Weak<Relay>,
    out: mpsc::UnboundedSender<String>,
    closed: watch::Sender<bool>,
    pending: Mutex<HashMap<String, mpsc::Sender<Message>>>,
}

/// The responses to one request; dropping it forgets the request (Go's context
/// cancellation).
struct Responses {
    rx: mpsc::Receiver<Message>,
    session: Arc<Session>,
    id: String,
}

impl Drop for Responses {
    fn drop(&mut self) {
        lock(&self.session.pending).remove(&self.id);
    }
}

impl Session {
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Resolves when the session has ended; the route then stops its transport.
    pub fn closed(&self) -> watch::Receiver<bool> {
        self.closed.subscribe()
    }

    pub fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    /// `session.send`.
    fn send(&self, msg: &Message) -> Result<(), String> {
        if self.is_closed() {
            return Err(CLOSED.into());
        }
        self.out.send(msg.encode()).map_err(|_| CLOSED.to_owned())
    }

    /// `session.request`.
    fn request(self: &Arc<Self>, msg: Message) -> Result<Responses, String> {
        if msg.id.is_empty() {
            return Err("wsrelay: message id is required".into());
        }
        let (tx, rx) = mpsc::channel(PENDING_BUFFER);
        {
            let mut pending = lock(&self.pending);
            if pending.contains_key(&msg.id) {
                return Err(format!("wsrelay: duplicate message id {}", msg.id));
            }
            pending.insert(msg.id.clone(), tx);
        }
        let responses = Responses {
            rx,
            session: self.clone(),
            id: msg.id.clone(),
        };
        self.send(&msg)?;
        Ok(responses)
    }

    /// One frame read from the socket (`session.run` and `dispatch`). An error ends the
    /// session with that cause. A full request buffer makes the reader wait, as Go's does.
    pub async fn receive(&self, frame: &[u8]) -> Result<(), String> {
        let msg = match Message::decode(frame) {
            Ok(msg) => msg,
            Err(e) => {
                self.close(&e);
                return Err(e);
            }
        };
        if msg.kind == PING {
            let _ = self.send(&Message {
                id: msg.id,
                kind: PONG.into(),
                payload: None,
            });
            return Ok(());
        }
        let Some(tx) = lock(&self.pending).get(&msg.id).cloned() else {
            if terminal(&msg.kind) {
                tracing::debug!(id = %msg.id, provider = %self.provider, "wsrelay: received terminal message for unknown id");
            }
            return Ok(());
        };
        let (id, last) = (msg.id.clone(), terminal(&msg.kind));
        let mut closed = self.closed();
        tokio::select! {
            _ = tx.send(msg) => {}
            _ = wait_closed(&mut closed) => {}
        }
        // A terminal message closes the request: its receiver sees the end after it.
        if last {
            lock(&self.pending).remove(&id);
        }
        Ok(())
    }

    /// `session.cleanup`: once, every waiting request gets an error message carrying
    /// `cause`, the session leaves the relay and the observer hears of it.
    // ponytail: Go makes room for the error in a full request buffer by dropping its
    // oldest message; here a full buffer just ends without the error message.
    pub fn close(&self, cause: &str) {
        if self.closed.send_replace(true) {
            return;
        }
        let pending: Vec<_> = lock(&self.pending).drain().map(|(_, tx)| tx).collect();
        for tx in pending {
            let _ = tx.try_send(Message {
                id: String::new(),
                kind: ERROR.into(),
                payload: Some(BTreeMap::from([(
                    "error".to_owned(),
                    GoValue::String(cause.to_owned()),
                )])),
            });
        }
        let Some(relay) = self.relay.upgrade() else {
            return;
        };
        {
            let mut sessions = lock(&relay.sessions);
            if sessions
                .get(&self.provider)
                .is_some_and(|s| std::ptr::eq(s.as_ref(), self))
            {
                sessions.remove(&self.provider);
            }
        }
        if let Some(observer) = relay.observer() {
            observer(&self.provider, Some(cause));
        }
    }
}

#[cfg(test)]
#[path = "wsrelay_tests.rs"]
mod tests;
