//! Native plugin host for CLIProxyAPI plugins (internal/pluginhost, sdk/pluginabi,
//! sdk/pluginapi).
//!
//! - [`abi`]: C ABI 1, schema 6, method names, the JSON envelope.
//! - [`gojson`] and [`api`]: Go `encoding/json` and the pluginapi schemas.
//! - [`native`]: `dlopen` loading, the plugin call and host callback trampolines.
//! - [`host`]: discovery, load/register/reconfigure/hot reload/unload, priority snapshot.
//! - [`callbacks`] and [`streams`]: `host.*` callbacks and plugin-fed streams.
//! - [`management`]: plugin-declared management and resource routes.

pub mod abi;
pub mod api;
pub mod callbacks;
pub mod client;
pub mod config;
pub mod gojson;
pub mod host;
pub mod management;
#[cfg(unix)]
pub mod native;
pub mod platform;
pub mod rpc;
pub mod streams;

pub use host::{Host, Record, RegisteredPluginInfo, Snapshot};
