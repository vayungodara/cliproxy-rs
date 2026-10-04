//! Native plugin host for CLIProxyAPI plugins (internal/pluginhost, sdk/pluginabi,
//! sdk/pluginapi).
//!
//! - [`abi`]: C ABI 1, schema 6, method names, the JSON envelope.
//! - [`gojson`] and [`api`]: Go `encoding/json` and the pluginapi schemas.
//! - [`native`]: `dlopen` loading, the plugin call and host callback trampolines.
//! - [`host`]: discovery, load/register/reconfigure/hot reload/unload, priority snapshot.
//! - [`callbacks`] and [`streams`]: `host.*` callbacks and plugin-fed streams;
//!   [`hostauth`]: `host.auth.*` and `host.affinity.lookup` over the server's
//!   [`hostauth::AuthManager`].
//! - [`management`]: plugin-declared management and resource routes.
//! - [`store`]: the plugin store (registries, verified installs, store auth).

pub mod abi;
pub mod api;
pub mod auth;
pub mod callbacks;
pub mod cli;
pub mod client;
pub mod config;
pub mod executor;
pub mod gojson;
pub mod host;
pub mod hostauth;
mod hosthttp;
pub use hosthttp::{go_preflight, go_read_error_text, go_url_error, go_url_error_text};
pub mod interceptors;
pub mod management;
pub mod models;
#[cfg(unix)]
pub mod native;
pub mod platform;
pub mod quota;
pub mod routing;
pub mod rpc;
pub mod store;
pub mod streams;
#[cfg(feature = "test-support")]
pub mod testing;
pub mod transform;

pub use host::{Host, Record, RegisteredPluginInfo, Snapshot};

/// Go `pluginhost.SupportPluginHeaderValue` (`X-CPA-SUPPORT-PLUGIN`): "1" where native
/// plugins load, as in Go's cgo builds.
/// ponytail: Windows DLL loading is not ported, so Windows reports "0".
pub const SUPPORT_PLUGIN: &str = if cfg!(unix) { "1" } else { "0" };
