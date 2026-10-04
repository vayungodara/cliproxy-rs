//! The plugin store (internal/pluginstore): registries that list installable
//! plugins, GitHub release and direct-download installs verified by SHA-256, store
//! authentication rules, the shared GitHub rate limiter, and the Home plugin-sync
//! payloads.
//!
//! The HTTP side goes through a [`Doer`] (Go's `httpfetch.Doer`), so the server
//! supplies its proxy-aware client and tests a local fake; nothing here opens a
//! connection by itself.

mod auth;
mod checksum;
mod client;
mod doer;
mod home_sync;
mod install;
mod manifest;
mod rate_limit;
mod registry;
mod url;
mod version;
mod zip;

pub use auth::{AuthConfig, Headers, ResolvedAuthConfig, Secret, normalize_auth_configs, plugin_auth_configured};
pub use checksum::{parse_checksums, verify_checksum};
pub use client::{Client, Doer, DoerResponse, Release, ReleaseAsset, StoreError, archive_name, select_release_assets};
pub use doer::WreqDoer;
pub use home_sync::{PluginSyncItem, PluginSyncRequest, PluginSyncResponse};
pub use install::{InstallOptions, InstallResult, LOADED_PLUGIN_LOCKED, install_archive};
pub use manifest::{Manifest, manifest_from_plugin, manifest_from_release, release_version};
pub use rate_limit::{GitHubRateLimiter, RateLimitError};
pub use registry::{
    Artifact, DEFAULT_REGISTRY_URL, DEFAULT_SOURCE_ID, INSTALL_TYPE_DIRECT, INSTALL_TYPE_GITHUB_RELEASE, InstallPlan,
    Platform, Plugin, Registry, Source, Version, default_source, github_repository_parts, normalize_install_plan,
    normalize_sources, parse_registry, plugin_install_type, plugin_platforms, source_id, source_name, validate_plugin,
};
pub use version::{normalize_version, update_available};

/// Go `RequestKind*`: what a store request fetches, for auth rules.
pub const REQUEST_KIND_REGISTRY: &str = "registry";
pub const REQUEST_KIND_METADATA: &str = "metadata";
pub const REQUEST_KIND_ARTIFACT: &str = "artifact";
