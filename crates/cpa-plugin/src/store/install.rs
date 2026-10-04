//! Store installs (internal/pluginstore/install.go): a release or direct artifact is
//! downloaded, verified, and its library written atomically to
//! `<plugins-dir>/<goos>/<goarch>/<id>-v<version><ext>`.

use std::path::{Path, PathBuf};

use super::checksum::{parse_checksums, sha256_hex, verify_checksum};
use super::client::{Client, Release, StoreError, select_release_assets};
use super::manifest::{Manifest, release_version};
use super::registry::{
    Artifact, INSTALL_TYPE_DIRECT, INSTALL_TYPE_GITHUB_RELEASE, InstallPlan, Plugin, normalize_goarch, normalize_goos,
    normalize_install_plan, plugin_install_type, valid_plugin_id, valid_plugin_version, validate_install_plan,
    validate_plugin,
};
use super::version::normalize_version;
use super::zip::{Archive, ReadError};

/// The largest library an archive may expand to. Go inflates up to the size the
/// archive declares, whatever it is.
pub const MAX_LIBRARY_BYTES: u64 = 256 << 20;

/// Go `ErrLoadedPluginLocked`'s text.
pub const LOADED_PLUGIN_LOCKED: &str = "loaded plugin library cannot be overwritten while the server is running";

/// Go `InstallOptions`.
#[derive(Default)]
pub struct InstallOptions {
    pub plugins_dir: String,
    pub goos: String,
    pub goarch: String,
    /// Whether the plugin's library is loaded now; Windows installs that would
    /// overwrite a loaded library are refused.
    pub plugin_loaded: Option<Box<dyn Fn() -> bool + Send + Sync>>,
    /// Runs after verification, before an existing library is replaced.
    pub before_write: Option<Box<dyn Fn() -> Result<(), String> + Send + Sync>>,
}

/// Go `InstallResult`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallResult {
    pub id: String,
    pub version: String,
    pub release_tag: String,
    pub install_type: String,
    pub path: String,
    pub overwritten: bool,
    pub skipped: bool,
}

/// Go `normalizeInstallOptions` (idempotent): the plugins directory and target
/// platform, defaulted and canonical.
fn normalize_options(options: &mut InstallOptions) {
    options.plugins_dir = match options.plugins_dir.trim() {
        "" => "plugins".into(),
        d => d.into(),
    };
    let goos = match options.goos.trim() {
        "" => crate::platform::goos(),
        g => g,
    };
    let goarch = match options.goarch.trim() {
        "" => crate::platform::goarch(),
        g => g,
    };
    options.goos = normalize_goos(goos);
    options.goarch = normalize_goarch(goarch);
}

fn other(e: impl Into<String>) -> StoreError {
    StoreError::Other(e.into())
}

impl Client {
    /// Go `Install`: a direct install's plan, or the latest GitHub release.
    pub async fn install(&self, mut plugin: Plugin, mut options: InstallOptions) -> Result<InstallResult, StoreError> {
        validate_plugin(&plugin)?;
        normalize_options(&mut options);
        if plugin_install_type(&plugin) == INSTALL_TYPE_DIRECT {
            plugin.version = normalize_version(&plugin.version);
            let plan = plugin.install.clone();
            return self.install_direct(plugin, plan, options).await;
        }
        let release = self.fetch_latest_release(&plugin).await?;
        let latest = release_version(&release)?;
        plugin.version = latest.clone();
        self.install_release(plugin, &release, &latest, options).await
    }

    /// Go `InstallManifest`.
    pub async fn install_manifest(
        &self,
        manifest: &Manifest,
        mut options: InstallOptions,
    ) -> Result<InstallResult, StoreError> {
        manifest.validate()?;
        normalize_options(&mut options);
        match manifest.install_type().as_str() {
            INSTALL_TYPE_DIRECT => {
                let plugin = self.direct_plugin_from_manifest(manifest).await?;
                let plan = plugin.install.clone();
                self.install_direct(plugin, plan, options).await
            }
            INSTALL_TYPE_GITHUB_RELEASE => {
                self.install_version(manifest.plugin(), &manifest.release_tag, &manifest.version, options)
                    .await
            }
            _ => Err(other(format!(
                "unsupported install type {}",
                cpa_common::gostr::quote(&manifest.install.install_type)
            ))),
        }
    }

    /// Go `InstallVersion`: a fixed release tag and version.
    pub async fn install_version(
        &self,
        mut plugin: Plugin,
        release_tag: &str,
        version: &str,
        mut options: InstallOptions,
    ) -> Result<InstallResult, StoreError> {
        validate_plugin(&plugin)?;
        normalize_options(&mut options);
        let version = normalize_version(version);
        if !valid_plugin_version(&version) {
            return Err(other(format!(
                "invalid plugin version {}",
                cpa_common::gostr::quote(&version)
            )));
        }
        let release_tag = match release_tag.trim() {
            "" => version.clone(),
            t => t.to_owned(),
        };
        let release = self.fetch_release_by_tag(&plugin, &release_tag).await?;
        let resolved = release_version(&release)?;
        if resolved != version {
            return Err(other(format!(
                "release tag {} resolved version {}, want {}",
                cpa_common::gostr::quote(&release_tag),
                cpa_common::gostr::quote(&resolved),
                cpa_common::gostr::quote(&version)
            )));
        }
        plugin.version = version.clone();
        self.install_release(plugin, &release, &version, options).await
    }

    async fn install_release(
        &self,
        mut plugin: Plugin,
        release: &Release,
        version: &str,
        options: InstallOptions,
    ) -> Result<InstallResult, StoreError> {
        let (archive, checksums) =
            select_release_assets(release, &plugin.id, &plugin.version, &options.goos, &options.goarch)?;
        let archive_data = self
            .download_asset(&archive)
            .await
            .map_err(|e| e.context(&format!("download {}", archive.name)))?;
        let checksum_data = self
            .download_asset(&checksums)
            .await
            .map_err(|e| e.context("download checksums.txt"))?;
        let sums = parse_checksums(&checksum_data)?;
        verify_checksum(&archive.name, &archive_data, &sums)?;
        plugin.version = version.to_owned();
        let mut result = install_archive(&archive_data, &plugin, options)?;
        result.install_type = INSTALL_TYPE_GITHUB_RELEASE.into();
        result.release_tag = release.tag_name.trim().to_owned();
        Ok(result)
    }

    /// Go `InstallDirect`.
    pub async fn install_direct(
        &self,
        mut plugin: Plugin,
        plan: InstallPlan,
        mut options: InstallOptions,
    ) -> Result<InstallResult, StoreError> {
        plugin.id = plugin.id.trim().to_owned();
        plugin.version = normalize_version(&plugin.version);
        if !valid_plugin_id(&plugin.id) {
            return Err(other(format!(
                "invalid plugin id {}",
                cpa_common::gostr::quote(&plugin.id)
            )));
        }
        if !valid_plugin_version(&plugin.version) {
            return Err(other(format!(
                "invalid plugin version {}",
                cpa_common::gostr::quote(&plugin.version)
            )));
        }
        let mut plan = normalize_install_plan(&plan);
        plan.install_type = INSTALL_TYPE_DIRECT.into();
        validate_install_plan(&plan)?;
        normalize_options(&mut options);
        let artifact = select_artifact(&plan, &options.goos, &options.goarch)?;
        let data = self
            .download_artifact(&artifact)
            .await
            .map_err(|e| e.context("download artifact"))?;
        verify_artifact_checksum(&artifact, &data)?;
        let mut result = install_archive(&data, &plugin, options)?;
        result.install_type = INSTALL_TYPE_DIRECT.into();
        Ok(result)
    }

    /// Go `directPluginFromManifest`: pinned artifacts, or the plugin re-read from
    /// its source registry.
    async fn direct_plugin_from_manifest(&self, manifest: &Manifest) -> Result<Plugin, StoreError> {
        let mut plugin = manifest.plugin();
        plugin.version = normalize_version(&manifest.version);
        plugin.install = normalize_install_plan(&plugin.install);
        plugin.install.install_type = INSTALL_TYPE_DIRECT.into();
        if !plugin.install.artifacts.is_empty() {
            return Ok(plugin);
        }
        let source_url = match manifest.source_url.trim() {
            "" => self.registry_url.trim().to_owned(),
            u => u.to_owned(),
        };
        if source_url.is_empty() {
            return Err(other("direct install manifest missing source-url"));
        }
        let mut source_client = self.clone();
        source_client.registry_url = source_url;
        let registry = source_client
            .fetch_registry()
            .await
            .map_err(|e| e.context("fetch direct install source"))?;
        let id = manifest.id.trim();
        let resolved = registry.plugin_by_id(id).ok_or_else(|| {
            other(format!(
                "direct install plugin {} not found in source",
                cpa_common::gostr::quote(id)
            ))
        })?;
        let install_type = plugin_install_type(resolved);
        if install_type != INSTALL_TYPE_DIRECT {
            return Err(other(format!(
                "direct install plugin {} resolved as {}",
                cpa_common::gostr::quote(id),
                cpa_common::gostr::quote(&install_type)
            )));
        }
        Ok(direct_plugin_version(resolved.clone(), id, &manifest.version)?)
    }
}

/// Go `directPluginVersion`: the plugin's own version or one of its versions.
fn direct_plugin_version(mut plugin: Plugin, id: &str, version: &str) -> Result<Plugin, String> {
    let version = normalize_version(version);
    let q = cpa_common::gostr::quote;
    if normalize_version(&plugin.version) == version {
        plugin.version = version.clone();
        plugin.install = normalize_install_plan(&plugin.install);
        plugin.install.install_type = INSTALL_TYPE_DIRECT.into();
        validate_install_plan(&plugin.install)
            .map_err(|e| format!("direct install plugin {} version {}: {e}", q(id), q(&version)))?;
        return Ok(plugin);
    }
    let candidate = plugin
        .versions
        .iter()
        .find(|c| normalize_version(&c.version) == version)
        .cloned()
        .ok_or_else(|| {
            format!(
                "direct install plugin {} version {} not found in source",
                q(id),
                q(&version)
            )
        })?;
    plugin.version = version.clone();
    plugin.install = normalize_install_plan(&candidate.install);
    if plugin.install.install_type.is_empty() {
        plugin.install.install_type = INSTALL_TYPE_DIRECT.into();
    }
    if plugin.install.install_type != INSTALL_TYPE_DIRECT {
        return Err(format!(
            "direct install plugin {} version {} resolved as {}",
            q(id),
            q(&version),
            q(&plugin.install.install_type)
        ));
    }
    validate_install_plan(&plugin.install)
        .map_err(|e| format!("direct install plugin {} version {}: {e}", q(id), q(&version)))?;
    Ok(plugin)
}

/// Go `SelectArtifact`.
pub fn select_artifact(plan: &InstallPlan, goos: &str, goarch: &str) -> Result<Artifact, String> {
    let plan = normalize_install_plan(plan);
    let (goos, goarch) = (normalize_goos(goos), normalize_goarch(goarch));
    if plan.install_type != INSTALL_TYPE_DIRECT {
        return Err(format!(
            "install type {} is not direct",
            cpa_common::gostr::quote(&plan.install_type)
        ));
    }
    plan.artifacts
        .into_iter()
        .find(|a| a.goos == goos && a.goarch == goarch)
        .ok_or_else(|| format!("artifact not found for {goos}/{goarch}"))
}

/// Go `VerifyArtifactChecksum`.
pub fn verify_artifact_checksum(artifact: &Artifact, data: &[u8]) -> Result<(), String> {
    let expected = artifact.sha256.trim().to_lowercase();
    if expected.is_empty() {
        return Err("artifact checksum missing".into());
    }
    if sha256_hex(data) != expected {
        return Err("artifact checksum mismatch".into());
    }
    Ok(())
}

/// Go `InstallArchive`: the target library from the archive root, written unless
/// the installed file is identical.
pub fn install_archive(data: &[u8], plugin: &Plugin, options: InstallOptions) -> Result<InstallResult, StoreError> {
    let mut options = options;
    normalize_options(&mut options);
    let options = &options;
    let q = cpa_common::gostr::quote;
    let id = plugin.id.trim();
    if !valid_plugin_id(id) {
        return Err(format!("invalid plugin id {}", q(&plugin.id)).into());
    }
    let version = normalize_version(&plugin.version);
    if !valid_plugin_version(&version) {
        return Err(format!("invalid plugin version {}", q(&plugin.version)).into());
    }
    let archive = Archive::new(data).map_err(|e| format!("open zip: {e}"))?;
    let (library, mode) = read_target_library(&archive, id, &version, &options.goos)?;
    let target = install_target_path(options, id, &version)?;
    let overwritten = match std::fs::metadata(&target) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(format!("stat target plugin: {}", go_path_error("stat", &target, &e)).into()),
    };
    let result = |overwritten: bool, skipped: bool| InstallResult {
        id: id.to_owned(),
        version: version.clone(),
        path: target.to_string_lossy().into_owned(),
        overwritten,
        skipped,
        ..Default::default()
    };
    if overwritten {
        let existing = std::fs::read(&target)
            .map_err(|e| format!("read target plugin: {}", go_path_error("open", &target, &e)))?;
        if existing == library {
            return Ok(result(true, true));
        }
        if let Some(before_write) = &options.before_write {
            before_write().map_err(|e| format!("prepare plugin write: {e}"))?;
        }
        // Go `loadedPluginInstallBlocked`: the platform first, then the callback.
        if options.goos.eq_ignore_ascii_case("windows") && options.plugin_loaded.as_ref().is_some_and(|f| f()) {
            return Err(StoreError::LoadedPluginLocked);
        }
    }
    write_file_atomic(&target, &library, mode)?;
    Ok(result(overwritten, false))
}

fn install_target_path(options: &InstallOptions, id: &str, version: &str) -> Result<PathBuf, String> {
    let version = normalize_version(version);
    if !valid_plugin_version(&version) {
        return Err(format!("invalid plugin version {}", cpa_common::gostr::quote(&version)));
    }
    Ok(Path::new(&options.plugins_dir)
        .join(&options.goos)
        .join(&options.goarch)
        .join(versioned_file_name(id, &version, &options.goos)))
}

/// Go `readTargetLibrary`: exactly one library at the archive root, named for the
/// plugin (optionally versioned); its permission bits, 0755 when none.
fn read_target_library(archive: &Archive<'_>, id: &str, version: &str, goos: &str) -> Result<(Vec<u8>, u32), String> {
    let target_name = format!("{}{}", id.trim(), extension(goos));
    let versioned = versioned_file_name(id, version, goos);
    let mut target = None;
    for entry in &archive.entries {
        let cleaned = clean_zip_name(&entry.name)?;
        if entry.is_dir() {
            continue;
        }
        if !entry.is_regular() {
            return Err(format!("zip entry {} is not a regular file", entry.name));
        }
        let lower = cleaned.to_lowercase();
        if !(lower.ends_with(".dylib") || lower.ends_with(".so") || lower.ends_with(".dll")) {
            continue;
        }
        if cleaned != target_name && cleaned != versioned {
            let base = cleaned.rsplit('/').next().unwrap_or_default();
            if base == target_name || base == versioned {
                return Err("target dynamic library must be at zip root".into());
            }
            return Err(format!("dynamic library filename must be {target_name} or {versioned}"));
        }
        if target.is_some() {
            return Err("zip contains multiple target dynamic libraries".into());
        }
        target = Some(entry);
    }
    let entry = target.ok_or_else(|| format!("zip does not contain {target_name}"))?;
    // Reading stops one byte past the declared size, so bounding it bounds inflation.
    if entry.size() > MAX_LIBRARY_BYTES {
        return Err(format!(
            "{target_name} exceeds maximum allowed size of {MAX_LIBRARY_BYTES} bytes"
        ));
    }
    let data = archive.read(entry).map_err(|e| match e {
        ReadError::Open(e) => format!("open {target_name}: {e}"),
        ReadError::Read(e) => format!("read {target_name}: {e}"),
    })?;
    let mode = match entry.perm() {
        0 => 0o755,
        m => m,
    };
    Ok((data, mode))
}

fn versioned_file_name(id: &str, version: &str, goos: &str) -> String {
    format!("{}-v{}{}", id.trim(), normalize_version(version), extension(goos))
}

/// Go `pluginExtension`.
fn extension(goos: &str) -> &'static str {
    match goos.trim().to_lowercase().as_str() {
        "darwin" | "mac" | "macos" | "osx" => ".dylib",
        "windows" => ".dll",
        _ => ".so",
    }
}

/// Go `cleanZipName`: relative, slash-separated, inside the archive root.
fn clean_zip_name(name: &str) -> Result<String, String> {
    if name.trim().is_empty() {
        return Err("zip entry has empty name".into());
    }
    if name.contains('\\') {
        return Err(format!("zip entry {name} uses backslash path separators"));
    }
    if name.starts_with('/') {
        return Err(format!("zip entry {name} is absolute"));
    }
    let cleaned = clean_path(name);
    if cleaned == "." || cleaned == ".." || cleaned.starts_with("../") {
        return Err(format!("zip entry {name} escapes archive root"));
    }
    Ok(cleaned)
}

/// Go `path.Clean` for a relative path.
fn clean_path(name: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for segment in name.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|s| *s != "..") {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            s => out.push(s),
        }
    }
    if out.is_empty() { ".".into() } else { out.join("/") }
}

/// Go `writeFileAtomic`: a temp file beside the target, synced, then renamed.
fn write_file_atomic(target: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    use std::io::Write as _;
    let dir = target.parent().unwrap_or(Path::new("."));
    // Go `os.MkdirAll(dir, 0o755)`.
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o755);
    builder
        .create(dir)
        .map_err(|e| format!("create plugin directory: {}", go_path_error("mkdir", dir, &e)))?;
    let base = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (mut temp, temp_path) = create_temp(dir, &base)?;
    let cleanup = |e: String| {
        let _ = std::fs::remove_file(&temp_path);
        e
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        temp.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|e| {
                cleanup(format!(
                    "chmod temp plugin file: {}",
                    go_path_error("chmod", &temp_path, &e)
                ))
            })?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    temp.write_all(data).map_err(|e| {
        cleanup(format!(
            "write temp plugin file: {}",
            go_path_error("write", &temp_path, &e)
        ))
    })?;
    temp.sync_all().map_err(|e| {
        cleanup(format!(
            "sync temp plugin file: {}",
            go_path_error("sync", &temp_path, &e)
        ))
    })?;
    drop(temp);
    if let Err(e) = std::fs::rename(&temp_path, target) {
        if cfg!(windows) {
            if let Err(e) = std::fs::remove_file(target)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                return Err(cleanup(format!(
                    "remove old plugin file: {}",
                    go_path_error("remove", target, &e)
                )));
            }
            return std::fs::rename(&temp_path, target).map_err(|e| {
                cleanup(format!(
                    "install plugin file: {}",
                    go_link_error(&temp_path, target, &e)
                ))
            });
        }
        return Err(cleanup(format!(
            "install plugin file: {}",
            go_link_error(&temp_path, target, &e)
        )));
    }
    Ok(())
}

/// `os.CreateTemp(dir, "."+base+".tmp-*")`.
fn create_temp(dir: &Path, base: &str) -> Result<(std::fs::File, PathBuf), String> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    for attempt in 0..10_000u128 {
        let suffix = (seed.wrapping_add(attempt * 7919) % 4_294_967_296) as u32;
        let path = dir.join(format!(".{base}.tmp-{suffix}"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(format!(
                    "create temp plugin file: {}",
                    go_path_error("open", &dir.join(format!(".{base}.tmp-*")), &e)
                ));
            }
        }
    }
    Err("create temp plugin file: too many attempts".into())
}

/// Go's `*PathError` text.
fn go_path_error(op: &str, path: &Path, e: &std::io::Error) -> String {
    format!("{op} {}: {}", path.display(), errno_text(e))
}

/// Go's `*LinkError` text for a rename.
fn go_link_error(from: &Path, to: &Path, e: &std::io::Error) -> String {
    format!("rename {} {}: {}", from.display(), to.display(), errno_text(e))
}

fn errno_text(e: &std::io::Error) -> String {
    let text = e.to_string();
    let text = text.split(" (os error").next().unwrap_or_default();
    let mut chars = text.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_lowercase().chain(chars).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_names_follow_go() {
        assert_eq!(clean_zip_name("a/./b.so"), Ok("a/b.so".into()));
        assert_eq!(clean_zip_name("a/../b.so"), Ok("b.so".into()));
        assert!(clean_zip_name("../b.so").is_err());
        assert!(clean_zip_name("a/../../b.so").is_err());
        assert!(clean_zip_name("/b.so").is_err());
        assert!(clean_zip_name("a\\b.so").is_err());
        assert_eq!(versioned_file_name("p", "v1.2", "darwin"), "p-v1.2.dylib");
    }

    /// A library declaring more than the ceiling is refused before it is inflated; at
    /// the ceiling's side of the check it is read as usual.
    #[test]
    fn library_size_is_bounded() {
        let mut data = crate::testing::stored_zip(&[("p.so", b"lib", 0o755)]);
        assert_eq!(
            read_target_library(&Archive::new(&data).unwrap(), "p", "1.0.0", "linux")
                .unwrap()
                .0,
            b"lib"
        );
        // The central directory's uncompressed size: EOCD offset field, then +24.
        let eocd = data.len() - 22;
        let central = u32::from_le_bytes(data[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
        let declared = (MAX_LIBRARY_BYTES + 1) as u32;
        data[central + 24..central + 28].copy_from_slice(&declared.to_le_bytes());
        let err = read_target_library(&Archive::new(&data).unwrap(), "p", "1.0.0", "linux").unwrap_err();
        assert_eq!(
            err,
            format!("p.so exceeds maximum allowed size of {MAX_LIBRARY_BYTES} bytes")
        );
    }
}
