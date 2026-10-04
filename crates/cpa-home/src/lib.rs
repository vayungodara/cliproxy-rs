//! The Home control plane (Go internal/home and the Home halves of sdk/cliproxy).
//!
//! Home is CLIProxyAPI's central credential scheduler. A node started with `-home-jwt`
//! enrolls for an mTLS client certificate, then speaks RESP2 to Home: the config comes
//! from Home (`GET config`, then `SUBSCRIBE config`), credentials are dispatched per
//! request (`RPOP <request json>`), concurrency leases are released in cumulative
//! frames, in-flight executions are reported in bounded snapshots, and shared caches
//! live in Home KV. An issued dispatch whose reply is lost fences the whole client
//! lifetime (PARITY risk 8): a lease may exist that this node can no longer account for.

pub mod applog;
pub mod cert;
pub mod client;
pub mod config;
pub mod dispatch;
pub mod error;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
mod gojson;
pub mod inflight;
pub mod kv;
mod private_fs;
pub mod refresh;
pub mod registry;
pub mod release;
pub mod resp;
pub mod session_alias;
mod subscriber;
mod tls;

// Mode-bit assertions: Unix only.
#[cfg(all(test, unix))]
mod cert_tests;
#[cfg(test)]
mod client_tests;

pub use client::{Client, DispatchRequest, ReleaseFrame, SetOptions, clear_current_if, current, set_current};
pub use config::{CredentialConcurrency, CredentialInFlight, HomeConfig, HomeTlsConfig, normalize_home_port};
pub use error::{Error, Result};
