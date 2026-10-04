//! Store registries (internal/pluginstore/registry.go): sources, the registry
//! document, and its validation.

use sha2::{Digest, Sha256};

use super::url;
use super::version::normalize_version;
use crate::go_struct;
use crate::gojson;

pub const DEFAULT_REGISTRY_URL: &str =
    "https://raw.githubusercontent.com/router-for-me/CLIProxyAPI-Plugins-Store/main/registry.json";
pub const DEFAULT_SOURCE_ID: &str = "official";
pub const DEFAULT_SOURCE_NAME: &str = "Official";
pub const SCHEMA_VERSION: isize = 1;
pub const SCHEMA_VERSION_V2: isize = 2;
pub const INSTALL_TYPE_GITHUB_RELEASE: &str = "github-release";
pub const INSTALL_TYPE_DIRECT: &str = "direct";

/// One registry the store lists plugins from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub id: String,
    pub name: String,
    pub url: String,
}

go_struct! {
    pub struct Artifact("pluginstore.Artifact") {
        "goos" omitempty => goos: String,
        "goarch" omitempty => goarch: String,
        "url" omitempty => url: String,
        "sha256" omitempty => sha256: String,
        "size" omitempty => size: i64,
    }
}

go_struct! {
    pub struct InstallPlan("pluginstore.InstallPlan") {
        "type" omitempty => install_type: String,
        "artifacts" omitempty => artifacts: Vec<Artifact>,
    }
}

go_struct! {
    pub struct Version("pluginstore.Version") {
        "version" => version: String,
        "install" omitempty => install: InstallPlan,
    }
}

go_struct! {
    pub struct Plugin("pluginstore.Plugin") {
        "id" => id: String,
        "name" => name: String,
        "description" => description: String,
        "author" => author: String,
        "version" => version: String,
        "versions" omitempty => versions: Vec<Version>,
        "repository" omitempty => repository: String,
        "logo" omitempty => logo: String,
        "homepage" omitempty => homepage: String,
        "license" omitempty => license: String,
        "tags" omitempty => tags: Vec<String>,
        "install" omitempty => install: InstallPlan,
        "auth_required" omitempty => auth_required: bool,
    }
}

go_struct! {
    pub struct Registry("pluginstore.Registry") {
        "schema_version" => schema_version: isize,
        "plugins" => plugins: Vec<Plugin>,
    }
}

/// A platform a direct install ships an artifact for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Platform {
    pub goos: String,
    pub goarch: String,
}

pub fn default_source() -> Source {
    Source {
        id: DEFAULT_SOURCE_ID.into(),
        name: DEFAULT_SOURCE_NAME.into(),
        url: DEFAULT_REGISTRY_URL.into(),
    }
}

/// Go `NormalizeSources`: the official source, then each extra URL once.
pub fn normalize_sources(registry_urls: &[String]) -> Result<Vec<Source>, String> {
    let mut out = vec![default_source()];
    let mut seen_ids: Vec<(String, String)> = vec![(DEFAULT_SOURCE_ID.into(), DEFAULT_REGISTRY_URL.into())];
    let mut seen_urls: Vec<String> = vec![DEFAULT_REGISTRY_URL.into()];
    for raw in registry_urls {
        let registry_url = raw.trim();
        if registry_url.is_empty() || seen_urls.iter().any(|u| u == registry_url) {
            continue;
        }
        let source = Source {
            id: source_id(registry_url),
            name: source_name(registry_url),
            url: registry_url.into(),
        };
        if let Some((_, existing)) = seen_ids.iter().find(|(id, _)| *id == source.id) {
            return Err(format!(
                "plugin store source id collision for {} and {}",
                cpa_common::gostr::quote(existing),
                cpa_common::gostr::quote(registry_url)
            ));
        }
        seen_ids.push((source.id.clone(), registry_url.into()));
        seen_urls.push(registry_url.into());
        out.push(source);
    }
    Ok(out)
}

/// Go `SourceID`: `source-` and the first 12 hex digits of the URL's SHA-256.
pub fn source_id(registry_url: &str) -> String {
    let sum = Sha256::digest(registry_url.trim().as_bytes());
    let hex: String = sum.iter().map(|b| format!("{b:02x}")).collect();
    format!("source-{}", &hex[..12])
}

/// Go `SourceName`: the URL's host, or the URL itself.
pub fn source_name(registry_url: &str) -> String {
    match url::parse(registry_url.trim()) {
        Some(u) if !u.host.trim().is_empty() => u.host,
        _ => registry_url.trim().into(),
    }
}

/// Go `ParseRegistry`.
// ponytail: Go's json.Decoder reads the first value and ignores anything after it;
// trailing data is rejected here.
pub fn parse_registry(data: &[u8]) -> Result<Registry, String> {
    // Go decodes with json.NewDecoder: the first value, trailing bytes ignored.
    let mut registry: Registry = gojson::from_slice_first(data).map_err(|e| format!("decode registry: {e}"))?;
    normalize_registry(&mut registry);
    validate_registry(&registry)?;
    Ok(registry)
}

fn normalize_registry(registry: &mut Registry) {
    for plugin in &mut registry.plugins {
        for field in [
            &mut plugin.id,
            &mut plugin.name,
            &mut plugin.description,
            &mut plugin.author,
            &mut plugin.version,
            &mut plugin.repository,
            &mut plugin.logo,
            &mut plugin.homepage,
            &mut plugin.license,
        ] {
            *field = field.trim().to_owned();
        }
        plugin.install = normalize_install_plan(&plugin.install);
        for version in &mut plugin.versions {
            version.version = normalize_version(&version.version);
            version.install = normalize_install_plan(&version.install);
        }
        for tag in &mut plugin.tags {
            *tag = tag.trim().to_owned();
        }
    }
}

/// Go `ValidateRegistry`.
pub fn validate_registry(registry: &Registry) -> Result<(), String> {
    if registry.schema_version != SCHEMA_VERSION && registry.schema_version != SCHEMA_VERSION_V2 {
        return Err(format!("unsupported schema_version {}", registry.schema_version));
    }
    let mut seen: Vec<&str> = Vec::new();
    for (index, plugin) in registry.plugins.iter().enumerate() {
        if registry.schema_version == SCHEMA_VERSION && plugin_install_type(plugin) == INSTALL_TYPE_DIRECT {
            return Err(format!(
                "plugins[{index}]: direct install requires schema_version {SCHEMA_VERSION_V2}"
            ));
        }
        validate_plugin(plugin).map_err(|e| format!("plugins[{index}]: {e}"))?;
        let id = plugin.id.trim();
        if seen.contains(&id) {
            return Err(format!(
                "plugins[{index}]: duplicate plugin id {}",
                cpa_common::gostr::quote(id)
            ));
        }
        seen.push(id);
    }
    Ok(())
}

/// Go `ValidatePlugin`.
// ponytail: Go checks the required fields in map order, so with several missing it
// names any one of them; this names the first in declaration order.
pub fn validate_plugin(plugin: &Plugin) -> Result<(), String> {
    let install_type = plugin_install_type(plugin);
    let mut required = vec![
        ("id", &plugin.id),
        ("name", &plugin.name),
        ("description", &plugin.description),
        ("author", &plugin.author),
    ];
    if install_type == INSTALL_TYPE_GITHUB_RELEASE {
        required.push(("repository", &plugin.repository));
    }
    if let Some((field, _)) = required.iter().find(|(_, v)| v.trim().is_empty()) {
        return Err(format!("missing required field {field}"));
    }
    if !valid_plugin_id(plugin.id.trim()) {
        return Err(format!("invalid plugin id {}", cpa_common::gostr::quote(&plugin.id)));
    }
    let version = plugin.version.trim();
    if !version.is_empty() && !valid_plugin_version(version) {
        return Err(format!(
            "invalid plugin version {}",
            cpa_common::gostr::quote(&plugin.version)
        ));
    }
    match install_type.as_str() {
        INSTALL_TYPE_GITHUB_RELEASE => github_repository_parts(&plugin.repository).map(|_| ()),
        INSTALL_TYPE_DIRECT => {
            if version.is_empty() {
                return Err("missing required field version".into());
            }
            validate_install_plan(&plugin.install)?;
            validate_plugin_versions(plugin)
        }
        _ => Err(format!(
            "unsupported install type {}",
            cpa_common::gostr::quote(&plugin.install.install_type)
        )),
    }
}

/// Go `ValidatePluginVersions`.
pub fn validate_plugin_versions(plugin: &Plugin) -> Result<(), String> {
    let mut seen: Vec<String> = Vec::new();
    let plugin_type = plugin_install_type(plugin);
    for (index, version) in plugin.versions.iter().enumerate() {
        let v = normalize_version(&version.version);
        if !valid_plugin_version(&v) {
            return Err(format!(
                "versions[{index}]: invalid plugin version {}",
                cpa_common::gostr::quote(&v)
            ));
        }
        if seen.contains(&v) {
            return Err(format!(
                "versions[{index}]: duplicate plugin version {}",
                cpa_common::gostr::quote(&v)
            ));
        }
        seen.push(v);
        let mut install = version.install.clone();
        let mut install_type = install.install_type.trim().to_lowercase();
        if install_type.is_empty() {
            install_type = plugin_type.clone();
            install.install_type = install_type.clone();
        }
        if install_type != plugin_type {
            return Err(format!(
                "versions[{index}]: install type {} does not match plugin install type {}",
                cpa_common::gostr::quote(&install_type),
                cpa_common::gostr::quote(&plugin_type)
            ));
        }
        validate_install_plan(&install).map_err(|e| format!("versions[{index}]: {e}"))?;
    }
    Ok(())
}

/// Go `PluginInstallType`: the plan's type, GitHub releases by default.
pub fn plugin_install_type(plugin: &Plugin) -> String {
    match plugin.install.install_type.trim().to_lowercase() {
        t if t.is_empty() => INSTALL_TYPE_GITHUB_RELEASE.into(),
        t => t,
    }
}

/// Go `NormalizeInstallPlan`.
pub fn normalize_install_plan(plan: &InstallPlan) -> InstallPlan {
    let mut plan = plan.clone();
    plan.install_type = plan.install_type.trim().to_lowercase();
    for artifact in &mut plan.artifacts {
        normalize_artifact(artifact);
    }
    plan
}

fn normalize_artifact(artifact: &mut Artifact) {
    artifact.goos = normalize_goos(&artifact.goos);
    artifact.goarch = normalize_goarch(&artifact.goarch);
    artifact.url = artifact.url.trim().to_owned();
    artifact.sha256 = artifact.sha256.trim().to_lowercase();
}

/// Go `ValidateInstallPlan`.
pub fn validate_install_plan(plan: &InstallPlan) -> Result<(), String> {
    let plan = normalize_install_plan(plan);
    if plan.install_type.is_empty() {
        return Err("missing install type".into());
    }
    if plan.install_type != INSTALL_TYPE_DIRECT && plan.install_type != INSTALL_TYPE_GITHUB_RELEASE {
        return Err(format!(
            "unsupported install type {}",
            cpa_common::gostr::quote(&plan.install_type)
        ));
    }
    if plan.install_type != INSTALL_TYPE_DIRECT {
        return Ok(());
    }
    if plan.artifacts.is_empty() {
        return Err("direct install requires at least one artifact".into());
    }
    for (index, artifact) in plan.artifacts.iter().enumerate() {
        validate_artifact(artifact).map_err(|e| format!("artifacts[{index}]: {e}"))?;
    }
    Ok(())
}

/// Go `ValidateArtifact`.
pub fn validate_artifact(artifact: &Artifact) -> Result<(), String> {
    let mut artifact = artifact.clone();
    normalize_artifact(&mut artifact);
    if artifact.goos.is_empty() {
        return Err("missing goos".into());
    }
    if artifact.goarch.is_empty() {
        return Err("missing goarch".into());
    }
    if artifact.url.is_empty() {
        return Err("missing url".into());
    }
    let parsed = match url::parse(&artifact.url) {
        Some(u) if !u.scheme.is_empty() && !u.host.is_empty() => u,
        _ => return Err("invalid artifact url".into()),
    };
    if parsed.scheme != "https" && parsed.scheme != "http" {
        return Err("artifact url must use http or https".into());
    }
    if url::has_sensitive_query_parameter(&parsed) {
        return Err("artifact url contains sensitive query parameter".into());
    }
    if artifact.sha256.is_empty() {
        return Err("missing sha256".into());
    }
    if artifact.sha256.len() != 64 {
        return Err("invalid sha256 length".into());
    }
    if let Some(bad) = artifact.sha256.bytes().position(|b| !b.is_ascii_hexdigit()) {
        return Err(format!(
            "invalid sha256: encoding/hex: invalid byte: {}",
            go_rune_literal(char::from(artifact.sha256.as_bytes()[bad]))
        ));
    }
    if artifact.size < 0 {
        return Err("invalid size".into());
    }
    Ok(())
}

/// `encoding/hex.InvalidByteError`'s `%#U` rendering: `U+0067 'g'`.
pub(super) fn go_rune_literal(c: char) -> String {
    if c.is_control() {
        format!("U+{:04X}", c as u32)
    } else {
        format!("U+{:04X} '{c}'", c as u32)
    }
}

/// Go `PluginPlatforms`: the distinct platforms of a direct install's artifacts.
pub fn plugin_platforms(plugin: &Plugin) -> Vec<Platform> {
    let mut out: Vec<Platform> = Vec::new();
    for artifact in plugin_artifacts(plugin) {
        let platform = Platform {
            goos: artifact.goos,
            goarch: artifact.goarch,
        };
        if platform.goos.is_empty() || platform.goarch.is_empty() || out.contains(&platform) {
            continue;
        }
        out.push(platform);
    }
    out
}

/// Go `PluginArtifacts`: the plan's artifacts, then every version's.
pub fn plugin_artifacts(plugin: &Plugin) -> Vec<Artifact> {
    if plugin_install_type(plugin) != INSTALL_TYPE_DIRECT {
        return Vec::new();
    }
    let mut out = normalize_install_plan(&plugin.install).artifacts;
    for version in &plugin.versions {
        out.extend(normalize_install_plan(&version.install).artifacts);
    }
    out
}

pub(super) fn normalize_goos(goos: &str) -> String {
    match goos.trim().to_lowercase().as_str() {
        "mac" | "macos" | "osx" => "darwin".into(),
        other => other.into(),
    }
}

pub(super) fn normalize_goarch(goarch: &str) -> String {
    match goarch.trim().to_lowercase().as_str() {
        "x64" | "x86_64" => "amd64".into(),
        "aarch64" => "arm64".into(),
        other => other.into(),
    }
}

/// Go `validPluginVersion`: `^[0-9][0-9A-Za-z.+-]*$`, no leading `v`.
pub(super) fn valid_plugin_version(version: &str) -> bool {
    let b = version.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_digit()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'+' | b'-'))
}

/// Go `validPluginID`: `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`.
pub(super) fn valid_plugin_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// Go `GitHubRepositoryParts`: owner and repository of `https://github.com/{owner}/{repo}`.
pub fn github_repository_parts(repository: &str) -> Result<(String, String), String> {
    const SHAPE: &str = "repository must be https://github.com/{owner}/{repo}";
    let parsed = url::parse_err(repository.trim()).map_err(|e| format!("invalid repository URL: {e}"))?;
    if parsed.scheme != "https"
        || parsed.host != "github.com"
        || !parsed.raw_query.is_empty()
        || !parsed.fragment.is_empty()
    {
        return Err(SHAPE.into());
    }
    let segments: Vec<&str> = parsed.raw_path.trim_matches('/').split('/').collect();
    if segments.len() != 2 || segments[0].is_empty() || segments[1].is_empty() {
        return Err(SHAPE.into());
    }
    let owner = url::path_unescape(segments[0]).map_err(|e| format!("invalid repository owner: {e}"))?;
    let repo = url::path_unescape(segments[1]).map_err(|e| format!("invalid repository name: {e}"))?;
    if repo.ends_with(".git") {
        return Err(SHAPE.into());
    }
    Ok((owner, repo))
}

impl Registry {
    /// Go `PluginByID`.
    pub fn plugin_by_id(&self, id: &str) -> Option<&Plugin> {
        let id = id.trim();
        self.plugins.iter().find(|p| p.id.trim() == id)
    }
}
