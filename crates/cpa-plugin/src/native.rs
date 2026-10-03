//! The native C ABI (internal/pluginhost/loader_unix.go, host_callbacks_unix.go).
//!
//! ```c
//! typedef struct { void* ptr; size_t len; } cliproxy_buffer;
//! typedef struct { uint32_t abi_version; void* host_ctx; host_call_fn call; host_free_fn free_buffer; } cliproxy_host_api;
//! typedef struct { uint32_t abi_version; plugin_call_fn call; plugin_free_fn free_buffer; plugin_shutdown_fn shutdown; } cliproxy_plugin_api;
//! int cliproxy_plugin_init(const cliproxy_host_api*, cliproxy_plugin_api*);
//! ```
//!
//! Memory ownership matches Go: the host owns request buffers for the duration of a
//! call; a response buffer belongs to whoever allocated it and is released through that
//! side's `free_buffer`. Host callback responses are `malloc`ed and freed with `free`.
//! The host API table and the `host_ctx` slot are `malloc`ed, stay valid until shutdown
//! (plugins keep the pointer), and `host_ctx` holds a numeric ID resolved through a
//! global table, so a stale context can never reach a dropped host.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use bytes::Bytes;

use crate::abi;
use crate::client::{CallbackHandler, CallbackInstance};

/// `cliproxy_buffer`.
#[repr(C)]
pub struct Buffer {
    pub ptr: *mut c_void,
    pub len: usize,
}

type HostCallFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const u8, usize, *mut Buffer) -> c_int;
type HostFreeFn = unsafe extern "C" fn(*mut c_void, usize);
type PluginCallFn = unsafe extern "C" fn(*const c_char, *const u8, usize, *mut Buffer) -> c_int;
type PluginFreeFn = unsafe extern "C" fn(*mut c_void, usize);
type PluginShutdownFn = unsafe extern "C" fn();
type InitFn = unsafe extern "C" fn(*const HostApi, *mut PluginApi) -> c_int;

/// `cliproxy_host_api`.
#[repr(C)]
pub struct HostApi {
    pub abi_version: u32,
    pub host_ctx: *mut c_void,
    pub call: Option<HostCallFn>,
    pub free_buffer: Option<HostFreeFn>,
}

/// `cliproxy_plugin_api`.
#[repr(C)]
#[derive(Default)]
pub struct PluginApi {
    pub abi_version: u32,
    pub call: Option<PluginCallFn>,
    pub free_buffer: Option<PluginFreeFn>,
    pub shutdown: Option<PluginShutdownFn>,
}

struct CallbackEntry {
    handler: Arc<dyn CallbackHandler>,
    plugin_id: String,
    instance: Arc<CallbackInstance>,
}

fn callbacks() -> &'static Mutex<HashMap<usize, CallbackEntry>> {
    static ENTRIES: OnceLock<Mutex<HashMap<usize, CallbackEntry>>> = OnceLock::new();
    ENTRIES.get_or_init(Default::default)
}

static NEXT_CALLBACK_ID: AtomicUsize = AtomicUsize::new(0);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `cliproxyHostCall`. Returns 0 with an error envelope for callback failures and 1 only
/// for an unusable context.
unsafe extern "C" fn host_call(
    ctx: *mut c_void,
    method: *const c_char,
    request: *const u8,
    request_len: usize,
    response: *mut Buffer,
) -> c_int {
    let run = || -> c_int {
        if !response.is_null() {
            // SAFETY: the plugin passes a valid, writable buffer slot.
            unsafe {
                (*response).ptr = std::ptr::null_mut();
                (*response).len = 0;
            }
        }
        if ctx.is_null() || method.is_null() {
            return 1;
        }
        // SAFETY: ctx is the host_ctx slot this module allocated; it holds the ID.
        let id = unsafe { *(ctx as *const usize) };
        let (handler, plugin_id, instance) = {
            let entries = lock(callbacks());
            let Some(entry) = entries.get(&id) else {
                return 1;
            };
            (entry.handler.clone(), entry.plugin_id.clone(), entry.instance.clone())
        };
        // SAFETY: method is a NUL-terminated string owned by the plugin for the call.
        let method = unsafe { CStr::from_ptr(method) }.to_string_lossy().into_owned();
        let request = if request.is_null() || request_len == 0 {
            Bytes::new()
        } else {
            // SAFETY: the plugin owns `request_len` readable bytes for the call.
            Bytes::copy_from_slice(unsafe { std::slice::from_raw_parts(request, request_len) })
        };
        let resp = match handler.call_from_plugin(&plugin_id, &instance, &method, request) {
            Ok(resp) => resp,
            Err(e) => abi::error_envelope("host_call_failed", &e.message, e.status),
        };
        if resp.is_empty() || response.is_null() {
            return 0;
        }
        // SAFETY: plain malloc; released by `host_free`.
        let ptr = unsafe { libc::malloc(resp.len()) };
        if ptr.is_null() {
            return 1;
        }
        // SAFETY: ptr has resp.len() bytes; response is valid (checked above).
        unsafe {
            std::ptr::copy_nonoverlapping(resp.as_ptr(), ptr.cast::<u8>(), resp.len());
            (*response).ptr = ptr;
            (*response).len = resp.len();
        }
        0
    };
    // Never unwind into the plugin.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).unwrap_or(1)
}

/// `cliproxyHostFree`.
unsafe extern "C" fn host_free(ptr: *mut c_void, _len: usize) {
    if !ptr.is_null() {
        // SAFETY: allocated by `host_call` with malloc.
        unsafe { libc::free(ptr) };
    }
}

/// A loaded plugin library speaking ABI 1.
pub struct NativeClient {
    handle: *mut c_void,
    host_api: *mut HostApi,
    host_ctx: *mut usize,
    api: PluginApi,
    callback_id: usize,
    instance: Arc<CallbackInstance>,
    /// `true` once shut down. Calls hold it shared for their whole duration (including
    /// the response copy and `free_buffer`); shutdown takes it exclusively, so the
    /// library and host tables can never be freed under a running call.
    gate: RwLock<bool>,
}

// SAFETY: the raw pointers are owned by this client and only freed in `shutdown`, which
// the guarded client runs after every call has returned. Plugin entry points are
// required to be thread-safe by the ABI (Go calls them from any goroutine).
unsafe impl Send for NativeClient {}
unsafe impl Sync for NativeClient {}

impl NativeClient {
    /// Go `dynamicLibraryLoader.Open`: dlopen, resolve `cliproxy_plugin_init`, hand it the
    /// host table and validate the plugin table.
    pub fn open(
        path: &Path,
        plugin_id: &str,
        handler: Arc<dyn CallbackHandler>,
        instance: Arc<CallbackInstance>,
    ) -> Result<Self, String> {
        let c_path = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| format!("dlopen {}: path contains NUL", path.display()))?;
        // SAFETY: dlopen with a valid C string.
        let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(format!("dlopen {}: {}", path.display(), dlerror()));
        }
        // SAFETY: handle is live; symbol name is a valid C string.
        let init = unsafe { libc::dlsym(handle, c"cliproxy_plugin_init".as_ptr()) };
        if init.is_null() {
            let err = dlerror();
            // SAFETY: handle came from dlopen.
            unsafe { libc::dlclose(handle) };
            return Err(format!("missing cliproxy_plugin_init: {err}"));
        }
        // SAFETY: malloc'd tables, initialised before use.
        let host_api = unsafe { libc::malloc(std::mem::size_of::<HostApi>()) }.cast::<HostApi>();
        let host_ctx = unsafe { libc::malloc(std::mem::size_of::<usize>()) }.cast::<usize>();
        if host_api.is_null() || host_ctx.is_null() {
            // SAFETY: free(NULL) is a no-op; handle came from dlopen.
            unsafe {
                libc::free(host_api.cast());
                libc::free(host_ctx.cast());
                libc::dlclose(handle);
            }
            return Err("allocate host api".into());
        }
        let callback_id = NEXT_CALLBACK_ID.fetch_add(1, Ordering::SeqCst) + 1;
        lock(callbacks()).insert(
            callback_id,
            CallbackEntry {
                handler,
                plugin_id: plugin_id.to_owned(),
                instance: instance.clone(),
            },
        );
        // SAFETY: both allocations are valid for writes.
        unsafe {
            host_ctx.write(callback_id);
            host_api.write(HostApi {
                abi_version: abi::ABI_VERSION,
                host_ctx: host_ctx.cast(),
                call: Some(host_call),
                free_buffer: Some(host_free),
            });
        }
        let mut client = NativeClient {
            handle,
            host_api,
            host_ctx,
            api: PluginApi::default(),
            callback_id,
            instance,
            gate: RwLock::new(false),
        };
        // SAFETY: `init` is the plugin's exported cliproxy_plugin_init.
        let init: InitFn = unsafe { std::mem::transmute::<*mut c_void, InitFn>(init) };
        // SAFETY: both tables are valid; the plugin fills `api`.
        let rc = unsafe { init(client.host_api, &mut client.api) };
        if rc != 0 {
            client.shutdown();
            return Err(format!("cliproxy_plugin_init returned {rc}"));
        }
        if client.api.abi_version != abi::ABI_VERSION {
            let version = client.api.abi_version;
            client.shutdown();
            return Err(format!("plugin ABI version {version} is not supported"));
        }
        if client.api.call.is_none() || client.api.free_buffer.is_none() {
            client.shutdown();
            return Err("plugin function table is incomplete".into());
        }
        Ok(client)
    }

    pub fn instance(&self) -> &Arc<CallbackInstance> {
        &self.instance
    }

    /// Go `dynamicLibraryClient.Call`. A non-zero return with an error envelope is a
    /// normal RPC failure; any other non-zero return is a transport error.
    pub fn call(&self, method: &str, request: &[u8]) -> Result<Bytes, String> {
        let gate = self.gate.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        if *gate {
            return Err("plugin client is closed".into());
        }
        let (Some(call), Some(free)) = (self.api.call, self.api.free_buffer) else {
            return Err("plugin client is closed".into());
        };
        // C.CString stops at the first NUL as far as the plugin can see.
        let method_c = CString::new(method.split('\0').next().unwrap_or_default()).unwrap_or_default();
        let mut response = Buffer {
            ptr: std::ptr::null_mut(),
            len: 0,
        };
        let req_ptr = if request.is_empty() {
            std::ptr::null()
        } else {
            request.as_ptr()
        };
        // SAFETY: the plugin's call entry point with host-owned buffers.
        let rc = unsafe { call(method_c.as_ptr(), req_ptr, request.len(), &mut response) };
        let out = if !response.ptr.is_null() && response.len > 0 {
            // SAFETY: the plugin returned `len` readable bytes at `ptr`.
            Bytes::copy_from_slice(unsafe { std::slice::from_raw_parts(response.ptr.cast::<u8>(), response.len) })
        } else {
            Bytes::new()
        };
        if !response.ptr.is_null() {
            // SAFETY: released through the plugin's own allocator.
            unsafe { free(response.ptr, response.len) };
        }
        drop(gate);
        if rc != 0 {
            if abi::Envelope::is_error(&out) {
                return Ok(out);
            }
            return Err(format!(
                "plugin call {method} returned {rc}: {}",
                String::from_utf8_lossy(&out)
            ));
        }
        Ok(out)
    }

    /// Go `dynamicLibraryClient.Shutdown`: `shutdown()`, drop the callback entry, free the
    /// host tables, `dlclose`. Idempotent.
    pub fn shutdown(&self) {
        let mut shut = self.gate.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if *shut {
            return;
        }
        *shut = true;
        self.instance.close();
        if let Some(shutdown) = self.api.shutdown {
            // SAFETY: the plugin's shutdown entry point, called once.
            unsafe { shutdown() };
        }
        lock(callbacks()).remove(&self.callback_id);
        // SAFETY: allocated in `open`, freed exactly once here.
        unsafe {
            libc::free(self.host_ctx.cast());
            libc::free(self.host_api.cast());
            libc::dlclose(self.handle);
        }
    }
}

impl Drop for NativeClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn dlerror() -> String {
    // SAFETY: dlerror returns NULL or a thread-local C string.
    let err = unsafe { libc::dlerror() };
    if err.is_null() {
        return String::new();
    }
    // SAFETY: non-null C string from dlerror.
    unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned()
}
