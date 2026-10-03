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
#[derive(Debug, Clone, Default)]
pub struct RequestScope {
    /// Go `logging.GetRequestID`, added to `host.log` entries.
    pub request_id: String,
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
    contexts: Mutex<HashMap<String, Context>>,
    pub streams: Arc<StreamBridge>,
    runtime: OnceLock<tokio::runtime::Handle>,
}

impl Callbacks {
    pub fn new(host: Weak<Inner>) -> Self {
        Self {
            host,
            next_context: AtomicU64::new(0),
            contexts: Mutex::default(),
            streams: Arc::default(),
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

    /// Go `closeHostHTTPCallbackInstance`: rejects further callbacks from an instance.
    pub fn close_instance(&self, _plugin_id: &str, instance: &Arc<CallbackInstance>) {
        instance.close();
    }

    /// Go `closeHostHTTPPluginResources` for one plugin.
    pub fn close_plugin(&self, _plugin_id: &str) {}

    pub fn close_all(&self) {}
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
                let chunk = if req.error.is_empty() {
                    Chunk::Data(req.payload)
                } else {
                    Chunk::Error(req.error)
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

    fn call_service(&self, _caller: &Caller, method: &str, _request: &[u8]) -> Result<Bytes, CallbackError> {
        Err(CallbackError::new(format!("unsupported host callback {method}")))
    }
}
