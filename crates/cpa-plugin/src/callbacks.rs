//! Host callbacks (`host.*` methods) and callback contexts
//! (internal/pluginhost/host_callbacks.go, callback_contexts.go).
//!
//! A capability call that lets the plugin call back opens a callback context and sends
//! its ID as `host_callback_id`. The context ties later callbacks to the calling plugin
//! instance and the request they serve (request ID for logs, cancellation) and owns
//! resources opened through it, which close with the context.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use bytes::Bytes;

use crate::abi::{self, method};
use crate::client::{CallbackError, CallbackHandler, CallbackInstance};
use crate::go_struct;
use crate::gojson::{self, Metadata};
use crate::host::{Host, Inner, lock};
use crate::streams::{Chunk, StreamBridge};

/// What a callback context remembers about the request it serves.
#[derive(Clone, Default)]
pub struct RequestScope {
    /// Go `logging.GetRequestID`, added to `host.log` entries.
    pub request_id: String,
    /// The request's upstream capture (Go's request-log context): `host.http.*`
    /// records each plugin HTTP exchange in it.
    pub capture: cpa_core::exec::CaptureSink,
    /// The request's cancellation (Go's request context): `host.http.*` operations
    /// opened under the context end when it fires.
    pub cancel: tokio_util::sync::CancellationToken,
}

impl std::fmt::Debug for RequestScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestScope")
            .field("request_id", &self.request_id)
            .field("capture", &self.capture.enabled())
            .field("cancelled", &self.cancel.is_cancelled())
            .finish()
    }
}

pub(crate) type Cleanup = Box<dyn FnOnce() + Send>;

struct Context {
    plugin_id: String,
    instance: Option<Arc<CallbackInstance>>,
    scope: RequestScope,
    cleanups: Vec<(u64, Cleanup)>,
}

/// Closes its callback context when dropped (Go's `closeCallback`).
pub struct ContextGuard {
    id: String,
    callbacks: Weak<Inner>,
}

impl ContextGuard {
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for ContextGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.callbacks.upgrade() {
            inner.callbacks.close_context(&self.id);
        }
    }
}

pub(crate) struct Callbacks {
    host: Weak<Inner>,
    next_context: AtomicU64,
    next_cleanup: AtomicU64,
    contexts: Mutex<HashMap<String, Context>>,
    pub streams: Arc<StreamBridge>,
    pub http: crate::hosthttp::HttpBridge,
    /// The server's credential manager (Go `SetAuthManager`).
    pub auth: std::sync::RwLock<Option<Arc<dyn crate::hostauth::AuthManager>>>,
    runtime: OnceLock<tokio::runtime::Handle>,
}

impl Callbacks {
    pub fn new(host: Weak<Inner>) -> Self {
        Self {
            host,
            next_context: AtomicU64::new(0),
            next_cleanup: AtomicU64::new(0),
            contexts: Mutex::default(),
            streams: Arc::default(),
            http: Default::default(),
            auth: Default::default(),
            runtime: OnceLock::new(),
        }
    }

    /// The handler handed to loaded plugins. Captures the current Tokio runtime, which
    /// async callbacks run on.
    pub fn handler(&self) -> Dispatch {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let _ = self.runtime.set(handle);
        }
        Dispatch {
            host: self.host.clone(),
        }
    }

    /// Go `openCallbackContextForPluginInstance`.
    pub fn open(&self, plugin_id: &str, instance: Option<Arc<CallbackInstance>>, scope: RequestScope) -> ContextGuard {
        let id = (self.next_context.fetch_add(1, Ordering::SeqCst) + 1).to_string();
        lock(&self.contexts).insert(
            id.clone(),
            Context {
                plugin_id: plugin_id.trim().to_owned(),
                instance,
                scope,
                cleanups: Vec::new(),
            },
        );
        ContextGuard {
            id,
            callbacks: self.host.clone(),
        }
    }

    fn close_context(&self, id: &str) {
        let ctx = lock(&self.contexts).remove(id);
        if let Some(ctx) = ctx {
            for (_, cleanup) in ctx.cleanups {
                cleanup();
            }
        }
    }

    /// The runtime async callbacks run on (captured when plugins were handed the
    /// dispatcher).
    pub fn runtime_handle(&self) -> Option<tokio::runtime::Handle> {
        self.runtime.get().cloned()
    }

    /// Go `addCallbackCleanupHandle`: runs `cleanup` when the context closes; `None`
    /// (without running it) when the context is not open.
    pub fn add_cleanup(&self, id: &str, cleanup: Cleanup) -> Option<u64> {
        let handle = self.next_cleanup.fetch_add(1, Ordering::SeqCst) + 1;
        let mut contexts = lock(&self.contexts);
        let ctx = contexts.get_mut(id.trim())?;
        ctx.cleanups.push((handle, cleanup));
        Some(handle)
    }

    /// Drops a cleanup without running it.
    pub fn remove_cleanup(&self, id: &str, handle: u64) {
        if let Some(ctx) = lock(&self.contexts).get_mut(id.trim()) {
            ctx.cleanups.retain(|(h, _)| *h != handle);
        }
    }

    /// Go `callbackContextRegistry.lookup`.
    pub fn lookup(&self, id: &str) -> Option<(String, Option<Arc<CallbackInstance>>, RequestScope)> {
        let id = id.trim();
        if id.is_empty() {
            return None;
        }
        lock(&self.contexts)
            .get(id)
            .map(|c| (c.plugin_id.clone(), c.instance.clone(), c.scope.clone()))
    }

    /// Go `closeHostHTTPCallbackInstance`: rejects further callbacks from an instance
    /// and ends its HTTP operations and streams.
    pub fn close_instance(&self, plugin_id: &str, instance: &Arc<CallbackInstance>) {
        instance.close();
        if let Some(inner) = self.host.upgrade() {
            self.http.close_instance(&Host::from_inner(inner), plugin_id, instance);
        }
    }

    /// Go `closeHostHTTPPluginResources` for one plugin.
    pub fn close_plugin(&self, plugin_id: &str) {
        if let Some(inner) = self.host.upgrade() {
            self.http.close_plugin(&Host::from_inner(inner), plugin_id);
        }
    }

    pub fn close_all(&self) {
        if let Some(inner) = self.host.upgrade() {
            self.http.close_all(&Host::from_inner(inner));
        }
    }
}

/// The [`CallbackHandler`] plugins call into.
pub struct Dispatch {
    host: Weak<Inner>,
}

impl CallbackHandler for Dispatch {
    fn call_from_plugin(
        &self,
        plugin_id: &str,
        instance: &Arc<CallbackInstance>,
        method: &str,
        request: Bytes,
    ) -> Result<Bytes, CallbackError> {
        if instance.is_closed() {
            return Err(CallbackError::new("host plugin callback instance is closed"));
        }
        let Some(inner) = self.host.upgrade() else {
            return Err(CallbackError::new("host plugin callback instance is closed"));
        };
        let host = Host::from_inner(inner);
        let caller = Caller {
            plugin_id: plugin_id.to_owned(),
            instance: instance.clone(),
        };
        host.call_from_plugin(&caller, method, &request)
    }
}

/// The plugin instance making a callback.
#[derive(Clone)]
pub struct Caller {
    pub plugin_id: String,
    pub instance: Arc<CallbackInstance>,
}

go_struct! {
    pub struct HostLogRequest("pluginhost.rpcHostLogRequest") {
        "host_callback_id" omitempty => host_callback_id: String,
        "level" omitempty => level: String,
        "message" omitempty => message: String,
        "fields" omitempty => fields: Metadata,
    }
}

go_struct! {
    pub struct StreamEmitRequest("pluginhost.rpcStreamEmitRequest") {
        "stream_id" => stream_id: String,
        "payload" omitempty => payload: Bytes,
        "error" omitempty => error: String,
    }
}

go_struct! {
    pub struct StreamCloseRequest("pluginhost.rpcStreamCloseRequest") {
        "stream_id" => stream_id: String,
        "error" omitempty => error: String,
    }
}

fn decode<T: gojson::GoJson>(what: &str, raw: &[u8]) -> Result<T, CallbackError> {
    gojson::from_slice(raw).map_err(|e| CallbackError::new(format!("decode {what}: {e}")))
}

fn ok_empty() -> Result<Bytes, CallbackError> {
    Ok(abi::ok_envelope(&crate::rpc::Empty {}))
}

impl Host {
    /// Go `callFromPlugin`.
    pub(crate) fn call_from_plugin(
        &self,
        caller: &Caller,
        method: &str,
        request: &[u8],
    ) -> Result<Bytes, CallbackError> {
        match method {
            method::HOST_LOG => self.host_log(request),
            method::HOST_STREAM_EMIT => {
                let req: StreamEmitRequest = decode("stream emit request", request)?;
                let chunk = Chunk {
                    payload: req.payload,
                    error: (!req.error.is_empty()).then_some(req.error),
                };
                self.inner
                    .callbacks
                    .streams
                    .emit(&req.stream_id, chunk)
                    .map_err(CallbackError::new)?;
                ok_empty()
            }
            method::HOST_STREAM_CLOSE => {
                let req: StreamCloseRequest = decode("stream close request", request)?;
                self.inner.callbacks.streams.close(&req.stream_id, &req.error);
                ok_empty()
            }
            other => self.call_service(caller, other, request),
        }
    }

    /// Go `callHostLog`.
    fn host_log(&self, request: &[u8]) -> Result<Bytes, CallbackError> {
        let req: HostLogRequest = decode("host log request", request)?;
        let message = match req.message.trim() {
            "" => "plugin log",
            m => m,
        };
        let mut fields: serde_json::Map<String, serde_json::Value> = req
            .fields
            .into_iter()
            .filter_map(|(k, v)| (!k.trim().is_empty()).then(|| (k.trim().to_owned(), v)))
            .collect();
        if let Some((_, _, scope)) = self.inner.callbacks.lookup(&req.host_callback_id)
            && !scope.request_id.is_empty()
        {
            fields.insert("request_id".into(), scope.request_id.into());
        }
        let fields = serde_json::Value::Object(fields).to_string();
        match req.level.trim().to_ascii_lowercase().as_str() {
            "trace" => tracing::trace!(fields = %fields, "{message}"),
            "info" => tracing::info!(fields = %fields, "{message}"),
            "warn" | "warning" => tracing::warn!(fields = %fields, "{message}"),
            "error" => tracing::error!(fields = %fields, "{message}"),
            _ => tracing::debug!(fields = %fields, "{message}"),
        }
        ok_empty()
    }

    fn call_service(&self, caller: &Caller, method: &str, request: &[u8]) -> Result<Bytes, CallbackError> {
        match method {
            method::HOST_HTTP_DO => self.host_http_do(caller, request),
            method::HOST_HTTP_DO_STREAM => self.host_http_do_stream(caller, request),
            method::HOST_HTTP_OPERATION_OPEN => self.host_http_operation_open(caller, request),
            method::HOST_HTTP_CANCEL => self.host_http_cancel(caller, request),
            method::HOST_HTTP_STREAM_READ => self.host_http_stream_read(caller, request),
            method::HOST_HTTP_STREAM_CLOSE => self.host_http_stream_close(caller, request),
            method::HOST_AUTH_LIST => self.host_auth_list(request),
            method::HOST_AUTH_GET => self.host_auth_get(request),
            method::HOST_AUTH_GET_RUNTIME => self.host_auth_get_runtime(request),
            method::HOST_AUTH_SAVE => self.host_auth_save(request),
            method::HOST_AFFINITY_LOOKUP => self.host_affinity_lookup(request),
            _ => Err(CallbackError::new(format!("unsupported host callback {method}"))),
        }
    }
}
