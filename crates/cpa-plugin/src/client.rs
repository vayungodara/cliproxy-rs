//! Plugin clients and the call guard (internal/pluginhost/client_guard.go).
//!
//! Plugin entry points block, so every call runs on Tokio's blocking pool. A caller that
//! stops waiting (its future is dropped) does not stop the call; the guard counts calls
//! still running and shutdown waits for them before the library is closed, as Go's
//! `guardedPluginClient` does.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use bytes::Bytes;

use std::sync::atomic::{AtomicBool, Ordering};

/// Rejects callbacks from a plugin instance once its load was abandoned or it was
/// unloaded (Go `hostCallbackInstance`).
#[derive(Debug, Default)]
pub struct CallbackInstance {
    closed: AtomicBool,
}

impl CallbackInstance {
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

/// A host callback failure: `host_call_failed` with an optional HTTP status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackError {
    pub message: String,
    pub status: u16,
}

impl CallbackError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: 0,
        }
    }
}

/// Receives `host.*` calls from plugins. Called on the plugin's thread; implementations
/// may block.
pub trait CallbackHandler: Send + Sync {
    fn call_from_plugin(
        &self,
        plugin_id: &str,
        instance: &Arc<CallbackInstance>,
        method: &str,
        request: Bytes,
    ) -> Result<Bytes, CallbackError>;
}

/// One loaded plugin, native or test double.
pub trait PluginClient: Send + Sync + 'static {
    /// Blocking RPC call: request JSON in, response envelope out.
    fn call(&self, method: &str, request: &[u8]) -> Result<Bytes, String>;
    /// Releases the plugin. Called once, after every call has returned.
    fn shutdown(&self);
}

#[cfg(unix)]
impl PluginClient for crate::native::NativeClient {
    fn call(&self, method: &str, request: &[u8]) -> Result<Bytes, String> {
        crate::native::NativeClient::call(self, method, request)
    }
    fn shutdown(&self) {
        crate::native::NativeClient::shutdown(self)
    }
}

/// Why a guarded call produced no response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardError {
    Closed,
    /// The call failed below the RPC layer (a non-zero return without an error envelope).
    Transport(String),
    /// Host-side code panicked while serving the call; the plugin gets fused.
    Panic(String),
}

impl std::fmt::Display for GuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuardError::Closed => f.write_str("plugin client is closed"),
            GuardError::Transport(e) => f.write_str(e),
            GuardError::Panic(e) => write!(f, "plugin call panic: {e}"),
        }
    }
}

#[derive(Default)]
struct GuardState {
    inner: Option<Arc<dyn PluginClient>>,
    calls: usize,
    closed: bool,
}

/// Go `guardedPluginClient`.
pub struct GuardedClient {
    state: Mutex<GuardState>,
    changed: Condvar,
    instance: Arc<CallbackInstance>,
    /// Flips to `true` once the plugin has been shut down.
    done: tokio::sync::watch::Sender<bool>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl GuardedClient {
    pub fn new(inner: Arc<dyn PluginClient>, instance: Arc<CallbackInstance>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(GuardState {
                inner: Some(inner),
                ..Default::default()
            }),
            changed: Condvar::new(),
            instance,
            done: tokio::sync::watch::Sender::new(false),
        })
    }

    pub fn instance(&self) -> &Arc<CallbackInstance> {
        &self.instance
    }

    fn acquire(&self) -> Result<Arc<dyn PluginClient>, GuardError> {
        let mut state = lock(&self.state);
        match (&state.inner, state.closed) {
            (Some(inner), false) => {
                let inner = inner.clone();
                state.calls += 1;
                Ok(inner)
            }
            _ => Err(GuardError::Closed),
        }
    }

    fn release(&self) {
        let mut state = lock(&self.state);
        state.calls -= 1;
        if state.calls == 0 {
            self.changed.notify_all();
        }
    }

    /// Runs one call on the blocking pool.
    pub async fn call(self: &Arc<Self>, method: &str, request: Vec<u8>) -> Result<Bytes, GuardError> {
        let inner = self.acquire()?;
        let this = self.clone();
        let method = method.to_owned();
        let task = tokio::task::spawn_blocking(move || {
            struct Release(Arc<GuardedClient>);
            impl Drop for Release {
                fn drop(&mut self) {
                    self.0.release();
                }
            }
            let _release = Release(this);
            inner.call(&method, &request)
        });
        match task.await {
            Ok(result) => result.map_err(GuardError::Transport),
            Err(e) => Err(GuardError::Panic(e.to_string())),
        }
    }

    /// Detaches the client at once, then waits up to `wait` for running calls to drain
    /// and the plugin to shut down. Shutdown itself always completes on a dedicated
    /// thread (Go `ShutdownContext`). `None` waits indefinitely.
    pub async fn shutdown(self: &Arc<Self>, wait: Option<Duration>) {
        let start = {
            let mut state = lock(&self.state);
            !std::mem::replace(&mut state.closed, true)
        };
        if start {
            let this = self.clone();
            std::thread::spawn(move || {
                let mut state = lock(&this.state);
                while state.calls > 0 {
                    state = this
                        .changed
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                let inner = state.inner.take();
                drop(state);
                if let Some(inner) = inner {
                    inner.shutdown();
                }
                this.done.send_replace(true);
            });
        }
        let mut done = self.done.subscribe();
        let finished = done.wait_for(|done| *done);
        match wait {
            Some(limit) => {
                let _ = tokio::time::timeout(limit, finished).await;
            }
            None => {
                let _ = finished.await;
            }
        }
    }

    /// Whether shutdown has finished (the library is closed).
    pub fn is_shut_down(&self) -> bool {
        *self.done.borrow()
    }

    pub fn active_calls(&self) -> usize {
        lock(&self.state).calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Slow {
        release: Arc<(Mutex<bool>, Condvar)>,
        shut: Arc<AtomicBool>,
    }

    impl PluginClient for Slow {
        fn call(&self, _: &str, _: &[u8]) -> Result<Bytes, String> {
            let (m, c) = &*self.release;
            let mut go = lock(m);
            while !*go {
                go = c.wait(go).unwrap();
            }
            Ok(Bytes::from_static(b"{}"))
        }
        fn shutdown(&self) {
            self.shut.store(true, Ordering::SeqCst);
        }
    }

    /// Go `TestGuardedPluginClientShutdownContextDetachesBlockedCall`: shutdown stops
    /// waiting at its deadline, rejects new calls at once, and closes the plugin only
    /// after the blocked call returns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_detaches_then_waits_for_running_calls() {
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let shut = Arc::new(AtomicBool::new(false));
        let client = GuardedClient::new(
            Arc::new(Slow {
                release: release.clone(),
                shut: shut.clone(),
            }),
            Arc::default(),
        );
        let running = {
            let client = client.clone();
            tokio::spawn(async move { client.call("m", vec![]).await })
        };
        while client.active_calls() == 0 {
            tokio::task::yield_now().await;
        }
        client.shutdown(Some(Duration::from_millis(20))).await;
        assert!(!shut.load(Ordering::SeqCst), "blocked call must finish before shutdown");
        assert_eq!(client.call("m", vec![]).await, Err(GuardError::Closed));
        *lock(&release.0) = true;
        release.1.notify_all();
        assert!(running.await.unwrap().is_ok());
        client.shutdown(None).await;
        assert!(shut.load(Ordering::SeqCst));
    }
}
