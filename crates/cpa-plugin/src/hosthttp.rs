//! `host.http.*` callbacks (internal/pluginhost/http_bridge.go, http_operation_bridge.go,
//! http_stream_bridge.go and their dispatch in host_callbacks.go): plugins send HTTP
//! requests through the host's Go-semantics client, optionally as cancellable
//! operations, with streamed responses read in chunks.
//!
//! An operation belongs to the calling plugin instance and, when opened under a
//! callback context, ends with that context (the request it serves) or when the
//! request's cancellation fires. A response stream belongs to its operation: cancelling
//! the operation closes the stream, and closing or draining the stream finishes the
//! operation.
//! ponytail: Go's `MarkUpstreamAttempt` (attempt accounting on the calling request) is
//! not recorded; it joins with the request-path wiring.
//! ponytail: Go's default transport bounds dialing (30 s) and the TLS handshake (10 s);
//! neither the shared Go-semantics clients (cpa_exec::proxy) nor the wire-profile
//! clients here set them.
//! ponytail: `RequestScope.cancel` carries the request's cancellation only; a request
//! deadline, which Go reports as `context deadline exceeded`, is not represented.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use cpa_common::gostr::quote;
use cpa_core::exec::{CaptureEvent, CaptureSink, UpstreamRequest};
use cpa_exec::proxy::{self, GoHeaders, Proxy, Route, SendError};
use cpa_exec::xai_url::{self as go_url, GoUrl};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::abi;
use crate::api::HttpWireProfile;
use crate::callbacks::{Caller, Cleanup, RequestScope};
use crate::client::{CallbackError, CallbackInstance};
use crate::go_struct;
use crate::gojson::{self, Header, NonNilBytes};
use crate::host::{Host, lock};

/// Go `DoStream`'s read buffer: no stream chunk is larger.
const STREAM_READ: usize = 32 * 1024;
/// The most `host.http.do` buffers. Go's `io.ReadAll` has no bound; a larger answer
/// needs `host.http.do_stream`.
const MAX_WHOLE_RESPONSE: usize = 64 << 20;

go_struct! {
    /// `httpRequest` (the nested form of a host HTTP request).
    pub struct RpcHttpRequest("pluginhost.httpRequest") {
        "method" omitempty => method: String,
        "url" omitempty => url: String,
        "headers" omitempty => headers: Header,
        "body" omitempty => body: Bytes,
        "wire_profile" omitempty => wire_profile: Option<HttpWireProfile>,
    }
}

go_struct! {
    /// `rpcHostHTTPRequest`.
    pub struct RpcHostHttpRequest("pluginhost.rpcHostHTTPRequest") {
        "http_client_id" omitempty => http_client_id: String,
        "host_callback_id" omitempty => host_callback_id: String,
        "operation_id" omitempty => operation_id: String,
        "method" omitempty => method: String,
        "url" omitempty => url: String,
        "headers" omitempty => headers: Header,
        "body" omitempty => body: Bytes,
        "wire_profile" omitempty => wire_profile: Option<HttpWireProfile>,
        "request" omitempty => request: Option<RpcHttpRequest>,
    }
}

go_struct! {
    /// `pluginapi.HTTPResponse` as `host.http.do` returns it (`io.ReadAll` never
    /// yields a nil body).
    pub struct HostHttpResponse("pluginapi.HTTPResponse") {
        "StatusCode" => status_code: i64,
        "Headers" => headers: Header,
        "Body" => body: NonNilBytes,
    }
}

go_struct! {
    /// `rpcHostHTTPStreamResponse`.
    pub struct HostHttpStreamResponse("pluginhost.rpcHostHTTPStreamResponse") {
        "status_code" => status_code: i64,
        "headers" omitempty => headers: Header,
        "stream_id" omitempty => stream_id: String,
    }
}

go_struct! {
    pub struct StreamReadRequest("pluginhost.rpcHostHTTPStreamReadRequest") {
        "stream_id" => stream_id: String,
    }
}

go_struct! {
    pub struct StreamCloseRequest("pluginhost.rpcHostHTTPStreamCloseRequest") {
        "stream_id" => stream_id: String,
    }
}

go_struct! {
    pub struct StreamReadResponse("pluginhost.rpcHostHTTPStreamReadResponse") {
        "payload" omitempty => payload: Bytes,
        "error" omitempty => error: String,
        "done" omitempty => done: bool,
    }
}

go_struct! {
    pub struct OperationOpenRequest("pluginhost.rpcHostHTTPOperationOpenRequest") {
        "host_callback_id" omitempty => host_callback_id: String,
    }
}

go_struct! {
    pub struct OperationOpenResponse("pluginhost.rpcHostHTTPOperationOpenResponse") {
        "operation_id" => operation_id: String,
    }
}

go_struct! {
    pub struct CancelRequest("pluginhost.rpcHostHTTPCancelRequest") {
        "host_callback_id" omitempty => host_callback_id: String,
        "operation_id" => operation_id: String,
    }
}

/// A decoded host HTTP request (Go `pluginapi.HTTPRequest`).
#[derive(Debug, Clone, Default)]
pub struct HostRequest {
    pub method: String,
    pub url: String,
    pub headers: Header,
    pub body: Bytes,
    pub wire_profile: Option<HttpWireProfile>,
}

/// Go `decodeHostHTTPRequestWithOperationID`: the nested `request` wins, with the
/// outer wire profile as its fallback.
fn decode_request(raw: &[u8]) -> Result<(HostRequest, String, String), CallbackError> {
    let req: RpcHostHttpRequest =
        gojson::from_slice(raw).map_err(|e| CallbackError::new(format!("decode host http request: {e}")))?;
    let callback_id = req.host_callback_id;
    let operation_id = req.operation_id.trim().to_owned();
    let request = match req.request {
        Some(inner) => HostRequest {
            method: inner.method,
            url: inner.url,
            headers: inner.headers,
            body: inner.body,
            wire_profile: inner.wire_profile.or(req.wire_profile),
        },
        None => HostRequest {
            method: req.method,
            url: req.url,
            headers: req.headers,
            body: req.body,
            wire_profile: req.wire_profile,
        },
    };
    Ok((request, callback_id, operation_id))
}

fn instance_key(instance: &Option<Arc<CallbackInstance>>) -> usize {
    instance.as_ref().map_or(0, |i| Arc::as_ptr(i) as usize)
}

/// One open operation (Go `hostHTTPOperation`).
struct Operation {
    token: CancellationToken,
    instance: Option<Arc<CallbackInstance>>,
    callback_id: String,
    started: bool,
    /// Closes the stream the operation produced.
    cleanup: Option<Cleanup>,
    /// The callback-context cleanup that cancels this operation.
    scope_cleanup: Option<(String, u64)>,
    scope: RequestScope,
}

/// An acquired operation (Go `hostHTTPOperationHandle`).
struct Acquired {
    plugin_id: String,
    operation_id: String,
    instance: Option<Arc<CallbackInstance>>,
    token: CancellationToken,
    scope: RequestScope,
}

type StreamKey = (String, usize, String);

/// One chunk handed from the body pump to `host.http.stream_read`, with the receipt the
/// pump waits for (Go's unbuffered channel: the next read starts only after the
/// plugin has taken this chunk).
type Delivery = (Result<Bytes, String>, tokio::sync::oneshot::Sender<()>);

struct StreamEntry {
    chunks: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Delivery>>>,
    token: CancellationToken,
    on_close: Option<Box<dyn FnOnce() + Send>>,
}

/// Go `hostHTTPOperationBridge` plus `hostHTTPStreamBridge` and the clients.
#[derive(Default)]
pub(crate) struct HttpBridge {
    next_operation: AtomicU64,
    operations: Mutex<HashMap<(String, String), Operation>>,
    next_stream: AtomicU64,
    streams: Mutex<HashMap<StreamKey, StreamEntry>>,
    clients: Clients,
}

/// Go's standard-transport clients by proxy, plus the dedicated wire-profile
/// transports (keep-alives off, so nothing is pooled; HTTP/1.1 when forced).
struct Clients {
    go: proxy::GoClients,
    profiled: Mutex<Vec<((Proxy, bool), wreq::Client)>>,
}

impl Default for Clients {
    fn default() -> Self {
        Self {
            go: proxy::GoClients::new(proxy::Hooks::default()),
            profiled: Mutex::default(),
        }
    }
}

impl Clients {
    /// A wire-profile transport. Go builds one per request and closes its idle
    /// connections after use; an unpooled client is the same on the wire.
    fn profiled(&self, proxy: &Proxy, http1: bool) -> wreq::Result<wreq::Client> {
        let key = (proxy.clone(), http1);
        let mut cache = lock(&self.profiled);
        if let Some((_, client)) = cache.iter().find(|(k, _)| *k == key) {
            return Ok(client.clone());
        }
        let mut builder = wreq::Client::builder()
            .redirect(wreq::redirect::Policy::none())
            .pool_max_idle_per_host(0);
        if http1 {
            builder = builder.http1_only();
        }
        let client = proxy.apply(builder, true)?.build()?;
        cache.insert(0, (key, client.clone()));
        cache.truncate(16);
        Ok(client)
    }
}

fn cancel_operation(op: Operation, host: &Host) {
    op.token.cancel();
    if let Some((callback_id, handle)) = op.scope_cleanup {
        host.inner.callbacks.remove_cleanup(&callback_id, handle);
    }
    if let Some(cleanup) = op.cleanup {
        cleanup();
    }
}

impl HttpBridge {
    /// Go `hostHTTPOperationBridge.cancel`: only the owning instance may cancel.
    fn cancel(&self, host: &Host, plugin_id: &str, instance: &Option<Arc<CallbackInstance>>, operation_id: &str) {
        let key = (plugin_id.trim().to_owned(), operation_id.trim().to_owned());
        let op = {
            let mut ops = lock(&self.operations);
            match ops.get(&key) {
                Some(op) if instance_key(&op.instance) == instance_key(instance) => ops.remove(&key),
                _ => None,
            }
        };
        if let Some(op) = op {
            cancel_operation(op, host);
        }
    }

    /// Go `hostHTTPOperationBridge.finish`: the operation ends without its stream
    /// cleanup running (the stream is what finished it). Operation IDs come from one
    /// counter and are never reused, so the key names exactly this operation.
    fn finish(&self, host: &Host, plugin_id: &str, operation_id: &str, token: &CancellationToken) {
        let key = (plugin_id.trim().to_owned(), operation_id.trim().to_owned());
        let op = lock(&self.operations).remove(&key);
        if let Some(op) = op
            && let Some((callback_id, handle)) = op.scope_cleanup
        {
            host.inner.callbacks.remove_cleanup(&callback_id, handle);
        }
        token.cancel();
    }

    fn cancel_matching(&self, host: &Host, plugin_id: Option<&str>, instance: Option<&Arc<CallbackInstance>>) {
        let ops: Vec<Operation> = {
            let mut ops = lock(&self.operations);
            let keys: Vec<_> = ops
                .iter()
                .filter(|((plugin, _), op)| match plugin_id {
                    None => true,
                    Some(id) => {
                        plugin == id.trim()
                            && instance.is_none_or(|i| op.instance.as_ref().is_some_and(|o| Arc::ptr_eq(o, i)))
                    }
                })
                .map(|(k, _)| k.clone())
                .collect();
            let drained: Vec<Operation> = keys.into_iter().filter_map(|k| ops.remove(&k)).collect();
            // Go `cancelAll` closes the instances while it holds the registry, so no
            // operation can open for them after the drain.
            if plugin_id.is_none() {
                for instance in drained.iter().filter_map(|op| op.instance.as_ref()) {
                    instance.close();
                }
            }
            drained
        };
        for op in ops {
            cancel_operation(op, host);
        }
        let streams: Vec<StreamEntry> = {
            let mut streams = lock(&self.streams);
            let keys: Vec<_> = streams
                .keys()
                .filter(|(plugin, inst, _)| match plugin_id {
                    None => true,
                    Some(id) => plugin == id.trim() && instance.is_none_or(|i| *inst == Arc::as_ptr(i) as usize),
                })
                .cloned()
                .collect();
            keys.into_iter().filter_map(|k| streams.remove(&k)).collect()
        };
        for stream in streams {
            close_stream_entry(stream);
        }
    }

    /// Go `closeHostHTTPCallbackInstance`.
    pub(crate) fn close_instance(&self, host: &Host, plugin_id: &str, instance: &Arc<CallbackInstance>) {
        instance.close();
        self.cancel_matching(host, Some(plugin_id), Some(instance));
    }

    /// Go `closeHostHTTPPluginResources` (every instance of the plugin).
    pub(crate) fn close_plugin(&self, host: &Host, plugin_id: &str) {
        if !plugin_id.trim().is_empty() {
            self.cancel_matching(host, Some(plugin_id), None);
        }
    }

    pub(crate) fn close_all(&self, host: &Host) {
        self.cancel_matching(host, None, None);
    }

    fn close_stream(&self, key: &StreamKey) {
        let entry = lock(&self.streams).remove(key);
        if let Some(entry) = entry {
            close_stream_entry(entry);
        }
    }
}

/// A host dropped without `shutdown_all` still ends its operations: their watcher
/// tasks and body pumps wait on these tokens.
impl Drop for HttpBridge {
    fn drop(&mut self) {
        let ops = self
            .operations
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for op in ops.values() {
            op.token.cancel();
        }
        let streams = self
            .streams
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for stream in streams.values() {
            stream.token.cancel();
        }
    }
}

fn close_stream_entry(entry: StreamEntry) {
    entry.token.cancel();
    if let Some(on_close) = entry.on_close {
        on_close();
    }
}

impl Host {
    fn http(&self) -> &HttpBridge {
        &self.inner.callbacks.http
    }

    fn runtime(&self) -> Result<tokio::runtime::Handle, CallbackError> {
        self.inner
            .callbacks
            .runtime_handle()
            .ok_or_else(|| CallbackError::new("host http operation bridge is unavailable"))
    }

    /// The callback context a request names, checked against the caller (Go's checks in
    /// `openHostHTTPOperation` / `acquireHostHTTPOperation`).
    fn http_scope(&self, caller: &Caller, callback_id: &str) -> Result<RequestScope, CallbackError> {
        let callback_id = callback_id.trim();
        if callback_id.is_empty() {
            return Ok(RequestScope::default());
        }
        let Some((plugin_id, instance, scope)) = self.inner.callbacks.lookup(callback_id) else {
            return Err(CallbackError::new("host callback ID is not open"));
        };
        if plugin_id != caller.plugin_id.trim() {
            return Err(CallbackError::new(
                "host callback ID does not belong to the calling plugin",
            ));
        }
        if !instance.as_ref().is_some_and(|i| Arc::ptr_eq(i, &caller.instance)) {
            return Err(CallbackError::new(
                "host callback ID does not belong to the calling plugin instance",
            ));
        }
        Ok(scope)
    }

    /// Go `createHostHTTPOperation`. The operation's cancellation is a child of the
    /// request's (Go `context.WithCancel(parent)`), and the operation leaves the
    /// registry when either fires (Go's `AfterFunc`).
    fn create_operation(
        &self,
        caller: &Caller,
        callback_id: &str,
        scope: RequestScope,
        started: bool,
    ) -> Result<(String, CancellationToken), CallbackError> {
        let bridge = self.http();
        let plugin_id = caller.plugin_id.trim().to_owned();
        let instance = Some(caller.instance.clone());
        let runtime = self.runtime()?;
        let operation_id = (bridge.next_operation.fetch_add(1, Ordering::SeqCst) + 1).to_string();
        let token = scope.cancel.child_token();
        let callback_id = callback_id.trim().to_owned();
        {
            // Checked under the registry lock: closing an instance marks it closed
            // before draining, so no operation can slip in after the drain.
            let mut ops = lock(&bridge.operations);
            if caller.instance.is_closed() {
                return Err(CallbackError::new("host http operation bridge is unavailable"));
            }
            ops.insert(
                (plugin_id.clone(), operation_id.clone()),
                Operation {
                    token: token.clone(),
                    instance: instance.clone(),
                    callback_id: callback_id.clone(),
                    started,
                    cleanup: None,
                    scope_cleanup: None,
                    scope,
                },
            );
        }
        {
            let host = Arc::downgrade(&self.inner);
            let (plugin, op_id, inst, ended) =
                (plugin_id.clone(), operation_id.clone(), instance.clone(), token.clone());
            runtime.spawn(async move {
                ended.cancelled().await;
                if let Some(inner) = host.upgrade() {
                    let host = Host { inner };
                    host.http().cancel(&host, &plugin, &inst, &op_id);
                }
            });
        }
        if !callback_id.is_empty() {
            let host = Arc::downgrade(&self.inner);
            let (plugin, op_id, inst) = (plugin_id.clone(), operation_id.clone(), instance.clone());
            let attached = self.inner.callbacks.add_cleanup(
                &callback_id,
                Box::new(move || {
                    if let Some(inner) = host.upgrade() {
                        let host = Host { inner };
                        host.http().cancel(&host, &plugin, &inst, &op_id);
                    }
                }),
            );
            let stored = attached.is_some_and(|handle| {
                let mut ops = lock(&bridge.operations);
                match ops.get_mut(&(plugin_id.clone(), operation_id.clone())) {
                    Some(op) if op.scope_cleanup.is_none() => {
                        op.scope_cleanup = Some((callback_id.clone(), handle));
                        true
                    }
                    _ => false,
                }
            });
            if !stored {
                if let Some(handle) = attached {
                    self.inner.callbacks.remove_cleanup(&callback_id, handle);
                }
                bridge.cancel(self, &plugin_id, &instance, &operation_id);
                return Err(CallbackError::new(
                    "host callback context closed while opening HTTP operation",
                ));
            }
        }
        Ok((operation_id, token))
    }

    /// Go `acquireHostHTTPOperation`: a new claimed operation, or the named open one.
    fn acquire_operation(
        &self,
        caller: &Caller,
        callback_id: &str,
        operation_id: &str,
    ) -> Result<Acquired, CallbackError> {
        let plugin_id = caller.plugin_id.trim().to_owned();
        if operation_id.is_empty() {
            let scope = self.http_scope(caller, callback_id)?;
            let (operation_id, token) = self.create_operation(caller, callback_id, scope.clone(), true)?;
            return Ok(Acquired {
                plugin_id,
                operation_id,
                instance: Some(caller.instance.clone()),
                token,
                scope,
            });
        }
        let key = (plugin_id.clone(), operation_id.to_owned());
        let mut ops = lock(&self.http().operations);
        match ops.get_mut(&key) {
            Some(op)
                if !op.started
                    && op.instance.as_ref().is_some_and(|i| Arc::ptr_eq(i, &caller.instance))
                    && op.callback_id == callback_id.trim() =>
            {
                op.started = true;
                Ok(Acquired {
                    plugin_id,
                    operation_id: operation_id.to_owned(),
                    instance: op.instance.clone(),
                    token: op.token.clone(),
                    scope: op.scope.clone(),
                })
            }
            _ => Err(CallbackError::new(format!(
                "host http operation {operation_id:?} is not open"
            ))),
        }
    }

    fn finish_operation(&self, op: &Acquired) {
        self.http().finish(self, &op.plugin_id, &op.operation_id, &op.token);
    }

    /// `host.http.operation_open`.
    pub(crate) fn host_http_operation_open(&self, caller: &Caller, raw: &[u8]) -> Result<Bytes, CallbackError> {
        let req: OperationOpenRequest = gojson::from_slice(raw)
            .map_err(|e| CallbackError::new(format!("decode host http operation open request: {e}")))?;
        let scope = self.http_scope(caller, &req.host_callback_id)?;
        let (operation_id, _) = self.create_operation(caller, &req.host_callback_id, scope, false)?;
        Ok(abi::ok_envelope(&OperationOpenResponse { operation_id }))
    }

    /// `host.http.cancel`.
    pub(crate) fn host_http_cancel(&self, caller: &Caller, raw: &[u8]) -> Result<Bytes, CallbackError> {
        let req: CancelRequest =
            gojson::from_slice(raw).map_err(|e| CallbackError::new(format!("decode host http cancel request: {e}")))?;
        let operation_id = req.operation_id.trim();
        if operation_id.is_empty() {
            return Err(CallbackError::new("host http operation id is required"));
        }
        self.http()
            .cancel(self, &caller.plugin_id, &Some(caller.instance.clone()), operation_id);
        Ok(abi::ok_envelope(&crate::rpc::Empty {}))
    }

    /// `host.http.do`: the whole response in one answer.
    pub(crate) fn host_http_do(&self, caller: &Caller, raw: &[u8]) -> Result<Bytes, CallbackError> {
        let (request, callback_id, operation_id) = decode_request(raw)?;
        let runtime = self.runtime()?;
        let op = self.acquire_operation(caller, &callback_id, &operation_id)?;
        let result = runtime.block_on(self.http_exchange(&request, &op.scope.capture, &op.token, true));
        self.finish_operation(&op);
        let (status, headers, body) = result?;
        let body = match body {
            Body::Whole(body) => body,
            Body::Stream(_) => unreachable!("whole body requested"),
        };
        Ok(abi::ok_envelope(&HostHttpResponse {
            status_code: i64::from(status),
            headers,
            body: NonNilBytes(body),
        }))
    }

    /// `host.http.do_stream`: headers now, the body through `host.http.stream_read`.
    pub(crate) fn host_http_do_stream(&self, caller: &Caller, raw: &[u8]) -> Result<Bytes, CallbackError> {
        let (request, callback_id, operation_id) = decode_request(raw)?;
        let runtime = self.runtime()?;
        let op = self.acquire_operation(caller, &callback_id, &operation_id)?;
        let result = runtime.block_on(self.http_exchange(&request, &op.scope.capture, &op.token, false));
        let (status, headers, body) = match result {
            Ok(r) => r,
            Err(e) => {
                self.finish_operation(&op);
                return Err(e);
            }
        };
        let Body::Stream(body) = body else {
            unreachable!("stream requested")
        };
        let (tx, rx) = tokio::sync::mpsc::channel::<Delivery>(1);
        runtime.spawn(pump(body, tx, op.token.clone(), op.scope.capture.clone()));
        let stream_id = (self.http().next_stream.fetch_add(1, Ordering::SeqCst) + 1).to_string();
        let key: StreamKey = (op.plugin_id.clone(), instance_key(&op.instance), stream_id.clone());
        // The registry holds these closures, so they hold the host weakly.
        let finish = {
            let host = Arc::downgrade(&self.inner);
            let (plugin, op_id, token) = (op.plugin_id.clone(), op.operation_id.clone(), op.token.clone());
            Box::new(move || {
                token.cancel();
                if let Some(inner) = host.upgrade() {
                    let host = Host { inner };
                    host.http().finish(&host, &plugin, &op_id, &token);
                }
            })
        };
        lock(&self.http().streams).insert(
            key.clone(),
            StreamEntry {
                chunks: Arc::new(tokio::sync::Mutex::new(rx)),
                token: op.token.clone(),
                on_close: Some(finish),
            },
        );
        // Cancelling the operation closes its stream.
        let attached = {
            let mut ops = lock(&self.http().operations);
            match ops.get_mut(&(op.plugin_id.clone(), op.operation_id.clone())) {
                Some(entry) if entry.cleanup.is_none() && !op.token.is_cancelled() => {
                    let host = Arc::downgrade(&self.inner);
                    let key = key.clone();
                    entry.cleanup = Some(Box::new(move || {
                        if let Some(inner) = host.upgrade() {
                            Host { inner }.http().close_stream(&key);
                        }
                    }));
                    true
                }
                _ => false,
            }
        };
        if !attached {
            self.http().close_stream(&key);
            return Err(CallbackError::new("context canceled"));
        }
        Ok(abi::ok_envelope(&HostHttpStreamResponse {
            status_code: i64::from(status),
            headers,
            stream_id,
        }))
    }

    /// `host.http.stream_read`: the next chunk, or `done` at the end.
    pub(crate) fn host_http_stream_read(&self, caller: &Caller, raw: &[u8]) -> Result<Bytes, CallbackError> {
        let req: StreamReadRequest = gojson::from_slice(raw)
            .map_err(|e| CallbackError::new(format!("decode host http stream read request: {e}")))?;
        // Go checks the ID as sent and looks it up trimmed.
        if req.stream_id.is_empty() {
            return Err(CallbackError::new("http stream id is required"));
        }
        let key: StreamKey = (
            caller.plugin_id.trim().to_owned(),
            Arc::as_ptr(&caller.instance) as usize,
            req.stream_id.trim().to_owned(),
        );
        let chunks = lock(&self.http().streams).get(&key).map(|e| e.chunks.clone());
        let Some(chunks) = chunks else {
            return Err(CallbackError::new(format!("http stream {} is not open", req.stream_id)));
        };
        let runtime = self.runtime()?;
        let next = runtime.block_on(async { chunks.lock().await.recv().await });
        let resp = match next {
            Some((Ok(payload), taken)) => {
                let _ = taken.send(());
                StreamReadResponse {
                    payload,
                    ..Default::default()
                }
            }
            Some((Err(error), taken)) => {
                let _ = taken.send(());
                self.http().close_stream(&key);
                StreamReadResponse {
                    error,
                    done: true,
                    ..Default::default()
                }
            }
            None => {
                self.http().close_stream(&key);
                StreamReadResponse {
                    done: true,
                    ..Default::default()
                }
            }
        };
        Ok(abi::ok_envelope(&resp))
    }

    /// `host.http.stream_close`.
    pub(crate) fn host_http_stream_close(&self, caller: &Caller, raw: &[u8]) -> Result<Bytes, CallbackError> {
        let req: StreamCloseRequest = gojson::from_slice(raw)
            .map_err(|e| CallbackError::new(format!("decode host http stream close request: {e}")))?;
        let key: StreamKey = (
            caller.plugin_id.trim().to_owned(),
            Arc::as_ptr(&caller.instance) as usize,
            req.stream_id.trim().to_owned(),
        );
        self.http().close_stream(&key);
        Ok(abi::ok_envelope(&crate::rpc::Empty {}))
    }

    /// The configured `requests.proxy-url`, trimmed (Go `cfg.ProxyURL`).
    fn proxy_url(&self) -> String {
        self.config()
            .as_ref()
            .and_then(|c| {
                c.document
                    .get("requests")
                    .and_then(|r| r.get("proxy-url"))
                    .map(crate::config::yaml_string)
            })
            .unwrap_or_default()
            .trim()
            .to_owned()
    }

    /// Go `hostHTTPClient.doHTTP` (+ `Do`'s `io.ReadAll` when `whole`): the request is
    /// captured once it is built, the response metadata after the headers, and every
    /// failure after the request with Go's error text. `token` is the operation's
    /// context: cancelling it fails whichever phase is running.
    async fn http_exchange(
        &self,
        req: &HostRequest,
        capture: &CaptureSink,
        token: &CancellationToken,
        whole: bool,
    ) -> Result<(u16, Header, Body), CallbackError> {
        // http.NewRequestWithContext: the method, then the URL.
        let method_text = if req.method.is_empty() {
            "GET"
        } else {
            req.method.as_str()
        };
        let method = wreq::Method::from_bytes(method_text.as_bytes()).map_err(|_| {
            CallbackError::new(format!(
                "create host http request: net/http: invalid method {}",
                quote(method_text)
            ))
        })?;
        let parsed =
            go_url::parse(&req.url).map_err(|e| CallbackError::new(format!("create host http request: {e}")))?;
        let mut wire: Vec<(String, String)> = Vec::new();
        let mut headers = GoHeaders::new();
        for (name, values) in &req.headers {
            for value in values {
                wire.push((name.clone(), value.clone()));
                // Go's request writer takes Host from the URL and frames the body
                // itself; those header keys are never written.
                let canonical = proxy::canonical_header(name);
                if !matches!(canonical.as_str(), "Host" | "Transfer-Encoding" | "Trailer") {
                    headers.add_raw(name, value.clone());
                }
            }
        }
        capture.record(CaptureEvent::Request(UpstreamRequest {
            url: &req.url,
            method: method_text,
            headers: &wire,
            body: &req.body,
            ..Default::default()
        }));
        // newHTTPClientForRequest: any wire profile gets a dedicated transport with
        // keep-alives off (so the request asks to close), HTTP/1.1 when forced or
        // ordered, and the profile's header order on the written request
        // (internal/httpwire). Its proxy setting must parse, or nothing is sent.
        let profile = req.wire_profile.clone().unwrap_or_default();
        let profiled = profile.http1_only || profile.disable_auto_compression || !profile.header_profile.is_empty();
        let http1 = profile.http1_only || !profile.header_profile.is_empty();
        let proxy_url = self.proxy_url();
        let client = if profiled {
            let proxy = profile_proxy(&proxy_url)?;
            self.http().clients.profiled(&proxy, http1).map_err(|e| {
                CallbackError::new(format!(
                    "pluginhost: build proxy transport for {}: {e}",
                    go_redact(&proxy_url)
                ))
            })?
        } else {
            self.http().clients.go.get(&Proxy::parse(&proxy_url))
        };
        if profile.disable_auto_compression {
            headers.disable_compression();
        }
        let order = profiled.then(|| profile_order(method_text, &req.headers, &req.body, &profile));
        if profiled && !wants_close(&req.headers) {
            headers.add_raw("Connection", "close");
        }
        // client.Do: failures are `*url.Error`s naming the first request's method and
        // the URL of the hop that failed.
        let fail_at = |url: &str, cause: &str| {
            let text = format!("{} {}: {cause}", url_error_op(method_text), quote(strip_password(url)));
            capture.record(CaptureEvent::ResponseError(&text));
            CallbackError::new(format!("execute host http request: {text}"))
        };
        let fail = |cause: &str| fail_at(&req.url, cause);
        go_preflight(&req.url, &parsed, &req.headers).map_err(|cause| fail(&cause))?;
        let route = move |_: &url::Url| {
            Ok(Route {
                client: client.clone(),
                order: order.clone(),
            })
        };
        let sent = tokio::select! {
            biased;
            () = token.cancelled() => return Err(fail("context canceled")),
            sent = proxy::send_request_raw(&route, method, &req.url, headers, Some(req.body.clone()), None) => sent,
        };
        let upstream = sent.map_err(|e| {
            let (url, cause) = send_error_parts(e);
            fail_at(&url, &cause)
        })?;
        let mut header = Header::new();
        for (name, value) in &upstream.headers {
            header
                .entry(proxy::canonical_header(name.as_str()))
                .or_default()
                .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
        }
        go_response_headers(upstream.version, method_text, upstream.status, &mut header);
        let metadata: Vec<(String, String)> = header
            .iter()
            .flat_map(|(name, values)| values.iter().map(move |v| (name.clone(), v.clone())))
            .collect();
        capture.record(CaptureEvent::ResponseMetadata(upstream.status, &metadata));
        if !whole {
            return Ok((upstream.status, header, Body::Stream(upstream.body)));
        }
        let mut body = bytes::BytesMut::new();
        let mut stream = upstream.body;
        let mut error = None;
        loop {
            let next = tokio::select! {
                biased;
                () = token.cancelled() => {
                    error = Some("context canceled".to_owned());
                    break;
                }
                next = stream.next() => next,
            };
            match next {
                None => break,
                Some(Ok(chunk)) if body.len() + chunk.len() > MAX_WHOLE_RESPONSE => {
                    error = Some(format!("response body exceeds {MAX_WHOLE_RESPONSE} bytes"));
                    break;
                }
                Some(Ok(chunk)) => body.extend_from_slice(&chunk),
                Some(Err(e)) => {
                    error = Some(go_read_error(&e));
                    break;
                }
            }
        }
        if !body.is_empty() {
            capture.record(CaptureEvent::ResponseChunk(&body));
        }
        if let Some(error) = error {
            capture.record(CaptureEvent::ResponseError(&error));
            return Err(CallbackError::new(format!("read host http response: {error}")));
        }
        Ok((upstream.status, header, Body::Whole(body.freeze())))
    }
}

/// Go `DoStream`'s reader goroutine: reads of at most 32 KiB, each captured and then
/// handed over before the next read starts. A read failure is captured and delivered
/// as the stream's last chunk; cancellation during a read is captured as Go's
/// `context canceled`, during a hand-over it just stops.
async fn pump(
    mut body: futures_util::stream::BoxStream<'static, std::io::Result<Bytes>>,
    tx: tokio::sync::mpsc::Sender<Delivery>,
    token: CancellationToken,
    capture: CaptureSink,
) {
    loop {
        let next = tokio::select! {
            biased;
            () = token.cancelled() => {
                capture.record(CaptureEvent::ResponseError("context canceled"));
                return;
            }
            next = body.next() => next,
        };
        match next {
            None => return,
            Some(Ok(chunk)) => {
                let mut at = 0;
                while at < chunk.len() {
                    let piece = chunk.slice(at..chunk.len().min(at + STREAM_READ));
                    at += piece.len();
                    capture.record(CaptureEvent::ResponseChunk(&piece));
                    if !hand_over(&tx, Ok(piece), &token).await {
                        return;
                    }
                }
            }
            Some(Err(e)) => {
                let text = go_read_error(&e);
                capture.record(CaptureEvent::ResponseError(&text));
                hand_over(&tx, Err(text), &token).await;
                return;
            }
        }
    }
}

/// Sends one chunk and waits until the reader has taken it; false when the stream or
/// the operation ended first.
async fn hand_over(
    tx: &tokio::sync::mpsc::Sender<Delivery>,
    item: Result<Bytes, String>,
    token: &CancellationToken,
) -> bool {
    let (taken, receipt) = tokio::sync::oneshot::channel();
    tokio::select! {
        () = token.cancelled() => return false,
        sent = tx.send((item, taken)) => if sent.is_err() { return false },
    }
    tokio::select! {
        () = token.cancelled() => false,
        r = receipt => r.is_ok(),
    }
}

/// The proxy a wire-profile transport uses (`proxyutil.Parse` on `cfg.ProxyURL`): an
/// unusable setting is an error here, where Go's ordinary client falls back.
fn profile_proxy(raw: &str) -> Result<Proxy, CallbackError> {
    if raw.is_empty() {
        return Ok(Proxy::Inherit);
    }
    if raw.eq_ignore_ascii_case("direct") || raw.eq_ignore_ascii_case("none") {
        return Ok(Proxy::Direct);
    }
    let reason = match go_url::parse(raw) {
        Err(_) => "parse proxy URL failed".to_owned(),
        Ok(u) if u.scheme.is_empty() || (u.hostname.is_empty() && u.port.is_empty()) => {
            "proxy URL missing scheme/host".to_owned()
        }
        Ok(u) if matches!(u.scheme.as_str(), "socks5" | "socks5h" | "http" | "https") => {
            return Ok(Proxy::Url(raw.to_owned()));
        }
        Ok(u) => format!("unsupported proxy scheme: {}", u.scheme),
    };
    Err(CallbackError::new(format!(
        "pluginhost: parse proxy {}: {reason}",
        go_redact(raw)
    )))
}

/// `proxyutil.Redact`: scheme and host only, user info as `redacted`.
fn go_redact(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    let invalid = || "<invalid proxy URL>".to_owned();
    let Ok(parsed) = go_url::parse(raw) else {
        return invalid();
    };
    if parsed.scheme.is_empty() || (parsed.hostname.is_empty() && parsed.port.is_empty()) {
        return invalid();
    }
    // The authority as written: after `scheme://`, up to the path, query or fragment.
    let authority = raw
        .split_once("//")
        .map_or("", |(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default());
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{}://redacted@{host}", parsed.scheme),
        None => format!("{}://{authority}", parsed.scheme),
    }
}

/// Go `Transport.roundTrip`'s checks before anything is dialed, in its order: header
/// names and values, the scheme, a host. The error is the `*url.Error` cause.
// ponytail: a URL whose host Go and the WHATWG parser read differently is refused
// instead of being sent to a host Go would not contact.
pub fn go_preflight(url: &str, parsed: &GoUrl, headers: &Header) -> Result<(), String> {
    let is_http = matches!(parsed.scheme.as_str(), "http" | "https");
    if is_http && let Some(invalid) = invalid_header(headers) {
        return Err(format!("net/http: invalid header {invalid}"));
    }
    if !is_http {
        return Err(format!("unsupported protocol scheme {}", quote(&parsed.scheme)));
    }
    if parsed.hostname.is_empty() && parsed.port.is_empty() {
        return Err("http: no Host in request URL".into());
    }
    if !go_url::same_authority(url) {
        return Err("unsupported URL: hosts differ between parsers".into());
    }
    Ok(())
}

/// The `*url.Error` text for a request Go's client refused: `Get "url": cause`.
pub fn go_url_error_text(method: &str, url: &str, cause: &str) -> String {
    format!("{} {}: {cause}", url_error_op(method), quote(strip_password(url)))
}

/// The error Go's response body reader reports for `err`.
pub fn go_read_error_text(err: &std::io::Error) -> String {
    go_read_error(err)
}

/// The `*url.Error` text Go's `http.Client.Do` returns for a failed send
/// (`Get "url": cause`), password masked.
pub fn go_url_error(method: &str, err: SendError) -> String {
    let (url, cause) = send_error_parts(err);
    format!("{} {}: {cause}", url_error_op(method), quote(strip_password(&url)))
}

/// Go `urlErrorOp`: the method with only its first letter upper case.
fn url_error_op(method: &str) -> String {
    if method.is_empty() {
        return "Get".into();
    }
    if method.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        let lower = method.to_ascii_lowercase();
        return format!("{}{}", &method[..1], &lower[1..]);
    }
    method.to_owned()
}

/// Go `stripPassword`: a password in the URL's authority (after `scheme://`, or a
/// leading `//`) prints as `***`.
fn strip_password(raw: &str) -> String {
    let start = match raw.find("://") {
        Some(i) => i + 3,
        None if raw.starts_with("//") => 2,
        None => return raw.to_owned(),
    };
    let rest = &raw[start..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let Some((userinfo, host)) = rest[..end].rsplit_once('@') else {
        return raw.to_owned();
    };
    match userinfo.split_once(':') {
        Some((user, _)) => format!("{}{user}:***@{host}{}", &raw[..start], &rest[end..]),
        None => raw.to_owned(),
    }
}

/// Go `validateHeaders` on the request: the first invalid name (or value, named by
/// its key) in key order.
fn invalid_header(headers: &Header) -> Option<String> {
    let token = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
    headers.iter().find_map(|(name, values)| {
        if name.is_empty() || !name.bytes().all(token) {
            return Some(format!("field name {}", quote(name)));
        }
        values
            .iter()
            .any(|v| v.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f))
            .then(|| format!("field value for {}", quote(name)))
    })
}

/// The error sources below `err`, looking through `io::Error` wrappers.
fn error_chain<'a>(err: &'a (dyn std::error::Error + 'static)) -> Vec<&'a (dyn std::error::Error + 'static)> {
    let mut out = Vec::new();
    let mut next = Some(err);
    while let Some(e) = next {
        out.push(e);
        next = match e.downcast_ref::<std::io::Error>().and_then(std::io::Error::get_ref) {
            Some(inner) => Some(inner as &(dyn std::error::Error + 'static)),
            None => e.source(),
        };
    }
    out
}

/// The URL Go's client names for a failed exchange, and the cause it reports.
// ponytail: the common transport failures are spelled as Go spells them (a refused
// dial, a connection closed before the response); any other reads as the transport's
// innermost error. A refused dial names the hop's host, which for a host name is not
// the address Go dialed.
fn send_error_parts(err: SendError) -> (String, String) {
    let (err, hop) = match err {
        SendError::Local { error, url } => return (url, String::from_utf8_lossy(&error.body).into_owned()),
        // What `transport_cause` reads in the transport's own timeout (wreq's text).
        SendError::Timeout { url } => return (url, "operation timed out".into()),
        SendError::Transport { error, url } => (error, url),
    };
    let cause = transport_cause(&err, go_url::parse(&hop).ok().as_ref());
    (hop, cause)
}

pub(crate) fn transport_cause(err: &wreq::Error, url: Option<&GoUrl>) -> String {
    let chain = error_chain(err);
    for e in &chain {
        if let Some(url) = url
            && e.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::ConnectionRefused)
        {
            let port = if url.port.is_empty() {
                if url.scheme == "https" { "443" } else { "80" }
            } else {
                url.port.as_str()
            };
            let host = if url.hostname.contains(':') {
                format!("[{}]", url.hostname)
            } else {
                url.hostname.clone()
            };
            return format!("dial tcp {host}:{port}: connect: connection refused");
        }
        if e.to_string().contains("connection closed before message completed") {
            return "EOF".into();
        }
    }
    chain.last().map_or_else(|| err.to_string(), ToString::to_string)
}

/// The error Go's body reader reports.
// ponytail: a truncated body (`unexpected EOF`) and a bad gzip header are spelled as Go
// spells them; any other failure reads as the innermost error.
pub(crate) fn go_read_error(err: &std::io::Error) -> String {
    let chain = error_chain(err);
    for e in &chain {
        if e.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof)
        {
            return "unexpected EOF".into();
        }
        if e.to_string().contains("invalid gzip header") {
            return "gzip: invalid header".into();
        }
    }
    chain.last().map_or_else(|| err.to_string(), ToString::to_string)
}

/// What Go's response reader leaves of the header (net/http transfer.go `readTransfer`
/// for HTTP/1.x, x/net/http2 for HTTP/2): framing headers it consumed are gone.
// ponytail: only the header normalisation is ported. Malformed framing Go refuses
// (a non-chunked Transfer-Encoding on HTTP/1.1, a forbidden trailer key, an invalid
// Content-Length beside chunked) is accepted here, and an HTTP/1.0 GET answer with
// Transfer-Encoding, which Go reads, is refused by the transport.
fn go_response_headers(version: http::Version, method: &str, status: u16, header: &mut Header) {
    if version >= http::Version::HTTP_2 {
        header.remove("Trailer");
        return;
    }
    let http11 = version >= http::Version::HTTP_11;
    // shouldClose: from HTTP/1.1 on, a closing Connection header is consumed.
    if http11 && has_token(header.get("Connection"), "close") {
        header.remove("Connection");
    }
    // parseTransferEncoding: always consumed; chunked only from HTTP/1.1 on.
    let chunked = header
        .remove("Transfer-Encoding")
        .is_some_and(|te| http11 && te.len() == 1 && te[0].eq_ignore_ascii_case("chunked"));
    // fixLength: identical repeated lengths collapse; chunked framing drops the length
    // unless the response cannot have a body.
    if let Some(lengths) = header.get_mut("Content-Length")
        && lengths.len() > 1
    {
        let first = lengths[0].trim_matches([' ', '\t']).to_owned();
        if lengths.iter().all(|l| l.trim_matches([' ', '\t']) == first) {
            *lengths = vec![first];
        }
    }
    let no_body = method == "HEAD" || status / 100 == 1 || status == 204 || status == 304;
    if chunked && !no_body {
        header.remove("Content-Length");
    }
    // fixTrailer: a Trailer header is consumed only with chunked framing.
    if chunked {
        header.remove("Trailer");
    }
}

/// `httpguts.HeaderValuesContainsToken`.
fn has_token(values: Option<&Vec<String>>, token: &str) -> bool {
    values.is_some_and(|values| {
        values
            .iter()
            .flat_map(|v| v.split(','))
            .any(|t| t.trim_matches([' ', '\t']).eq_ignore_ascii_case(token))
    })
}

/// The request's `Connection` header asks to close (Go `Request.wantsClose`).
fn wants_close(headers: &Header) -> bool {
    has_token(headers.get("Connection"), "close")
}

/// The header lines a wire-profile request writes, in order: Go's request writer
/// (Host, User-Agent, Content-Length, the request's headers sorted by key, then the
/// transport's own Accept-Encoding and `Connection: close`), reordered by the profile
/// as `httpwire.orderRequestHeader` does (profile names first, in the profile's
/// spelling, then the rest in place).
fn profile_order(method: &str, headers: &Header, body: &[u8], profile: &HttpWireProfile) -> Vec<String> {
    const EXCLUDED: [&str; 5] = ["Host", "User-Agent", "Content-Length", "Transfer-Encoding", "Trailer"];
    let mut base: Vec<String> = vec!["Host".into()];
    if headers
        .get("User-Agent")
        .is_none_or(|v| v.first().is_none_or(|ua| !ua.is_empty()))
    {
        base.push("User-Agent".into());
    }
    if !body.is_empty() || matches!(method, "POST" | "PUT" | "PATCH") {
        base.push("Content-Length".into());
    }
    let mut rest: Vec<String> = headers
        .keys()
        .filter(|k| !EXCLUDED.contains(&k.as_str()))
        .cloned()
        .collect();
    rest.sort();
    rest.dedup();
    base.extend(rest);
    let auto_gzip = !profile.disable_auto_compression
        && !headers.contains_key("Accept-Encoding")
        && !headers.contains_key("Range")
        && method != "HEAD";
    if auto_gzip {
        base.push("Accept-Encoding".into());
    }
    if !wants_close(headers) {
        base.push("Connection".into());
    }
    let mut used = vec![false; base.len()];
    let mut out = Vec::with_capacity(base.len());
    for name in &profile.header_profile {
        for (i, line) in base.iter().enumerate() {
            if !used[i] && !name.is_empty() && line.eq_ignore_ascii_case(name) {
                out.push(name.clone());
                used[i] = true;
            }
        }
    }
    out.extend(base.iter().zip(&used).filter(|(_, u)| !**u).map(|(n, _)| n.clone()));
    out
}

enum Body {
    Whole(Bytes),
    Stream(futures_util::stream::BoxStream<'static, std::io::Result<Bytes>>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(pairs: &[(&str, &str)]) -> Header {
        let mut h = Header::new();
        for (k, v) in pairs {
            h.entry((*k).to_owned()).or_default().push((*v).to_owned());
        }
        h
    }

    /// net/http transfer.go on HTTP/1.0, HTTP/1.1 and HTTP/2 responses.
    #[test]
    fn response_framing_headers_follow_go() {
        let raw = [
            ("Connection", "close"),
            ("Trailer", "X-T"),
            ("Transfer-Encoding", "chunked"),
            ("Content-Length", "5"),
        ];
        let mut h11 = header(&raw);
        go_response_headers(http::Version::HTTP_11, "GET", 200, &mut h11);
        assert_eq!(h11, Header::new());
        // HTTP/1.0 keeps Connection, ignores chunked, so keeps Trailer and the length.
        let mut h10 = header(&raw);
        go_response_headers(http::Version::HTTP_10, "GET", 200, &mut h10);
        assert_eq!(
            h10,
            header(&[("Connection", "close"), ("Trailer", "X-T"), ("Content-Length", "5")])
        );
        // A HEAD answer keeps its length even when chunked; a non-chunked Trailer stays.
        let mut head = header(&raw);
        go_response_headers(http::Version::HTTP_11, "HEAD", 200, &mut head);
        assert_eq!(head, header(&[("Content-Length", "5")]));
        let mut plain = header(&[
            ("Trailer", "X-T"),
            ("Connection", "keep-alive"),
            ("Content-Length", "1"),
        ]);
        go_response_headers(http::Version::HTTP_11, "GET", 200, &mut plain);
        assert_eq!(
            plain,
            header(&[
                ("Trailer", "X-T"),
                ("Connection", "keep-alive"),
                ("Content-Length", "1")
            ])
        );
        let mut dup = header(&[("Content-Length", "3"), ("Content-Length", " 3")]);
        go_response_headers(http::Version::HTTP_11, "GET", 200, &mut dup);
        assert_eq!(dup, header(&[("Content-Length", "3")]));
        let mut h2 = header(&[("Trailer", "X-T"), ("Connection", "close")]);
        go_response_headers(http::Version::HTTP_2, "GET", 200, &mut h2);
        assert_eq!(h2, header(&[("Connection", "close")]));
    }

    /// Go's 32 KiB `Body.Read` buffer: one large transport frame is handed over in
    /// reads of at most 32 KiB, each waiting for the reader.
    #[tokio::test]
    async fn pump_reads_at_most_32_kib() {
        let body = futures_util::stream::iter([Ok(Bytes::from(vec![b'x'; 70_000]))]).boxed();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(pump(body, tx, CancellationToken::new(), CaptureSink::default()));
        let mut sizes = Vec::new();
        while let Some((chunk, taken)) = rx.recv().await {
            sizes.push(chunk.unwrap().len());
            let _ = taken.send(());
        }
        assert_eq!(sizes, [32_768, 32_768, 4_464]);
    }

    /// A hop skipped because the deadline passed reads as the transport's own timeout.
    #[tokio::test]
    async fn a_skipped_hop_reads_as_a_transport_timeout() {
        // Connections complete in the backlog and are never answered.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = format!("http://{}/x", listener.local_addr().unwrap());
        let client = proxy::default_client();
        let route = |_: &url::Url| {
            Ok(Route {
                client: client.clone(),
                order: None,
            })
        };
        let timeout = Some(std::time::Duration::from_millis(50));
        let sent = proxy::send_request_raw(&route, wreq::Method::GET, &target, GoHeaders::new(), None, timeout);
        let Err(timed_out) = sent.await else {
            panic!("an unanswered request succeeded");
        };
        assert!(
            matches!(&timed_out, SendError::Transport { error, .. } if error.is_timeout()),
            "{timed_out:?}"
        );
        assert_eq!(
            send_error_parts(SendError::Timeout { url: target.clone() }),
            send_error_parts(timed_out)
        );
    }

    #[test]
    fn url_error_parts_follow_go() {
        assert_eq!(url_error_op("POST"), "Post");
        assert_eq!(url_error_op("get"), "get");
        assert_eq!(url_error_op("PROPFIND"), "Propfind");
        assert_eq!(strip_password("http://u:secret@h:1/p?q=1"), "http://u:***@h:1/p?q=1");
        assert_eq!(strip_password("http://u@h/p"), "http://u@h/p");
        assert_eq!(strip_password("//u:secret@example.com/p@x"), "//u:***@example.com/p@x");
        assert_eq!(strip_password("/u:secret@x"), "/u:secret@x");
        assert_eq!(
            invalid_header(&header(&[("A", "ok\tfine"), ("B c", "x")])).as_deref(),
            Some("field name \"B c\"")
        );
        assert_eq!(
            invalid_header(&header(&[("A", "bad\r\nvalue")])).as_deref(),
            Some("field value for \"A\"")
        );
        assert_eq!(invalid_header(&header(&[("A", "caf\u{e9}")])), None);
    }

    #[test]
    fn profile_proxy_errors_follow_go() {
        let err = |raw: &str| profile_proxy(raw).unwrap_err().message;
        assert_eq!(
            err("ftp://user:pw@proxy.invalid:1"),
            "pluginhost: parse proxy ftp://redacted@proxy.invalid:1: unsupported proxy scheme: ftp"
        );
        assert_eq!(
            err("proxy.invalid:8080"),
            "pluginhost: parse proxy <invalid proxy URL>: proxy URL missing scheme/host"
        );
        assert_eq!(
            err("http://bad host"),
            "pluginhost: parse proxy <invalid proxy URL>: parse proxy URL failed"
        );
        assert_eq!(profile_proxy("DIRECT").unwrap(), Proxy::Direct);
        assert_eq!(profile_proxy("").unwrap(), Proxy::Inherit);
        assert_eq!(
            profile_proxy("socks5://p:1").unwrap(),
            Proxy::Url("socks5://p:1".into())
        );
    }
}
