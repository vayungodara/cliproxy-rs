//! Contracts shared by every cliproxy-rs crate.
//!
//! - [`config`]: `config.yaml`, compatible with CLIProxyAPI.
//! - [`credential`]: runtime credentials loaded from `auth-dir` or config.
//! - [`exec`]: the execution envelope passed between the runtime and executors.
//! - [`format`]: request/response wire formats.
//! - [`registry`]: the pinned static model catalog.

pub mod config;
pub mod credential;
pub mod exec;
pub mod format;
pub mod registry;
