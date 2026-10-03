//! Plugin file discovery and selection (internal/pluginhost/platform.go).
//!
//! Files are `<id>.<ext>` or `<id>-v<version>.<ext>` in `<dir>/<goos>/<goarch>` and then
//! `<dir>`. The first directory to name an ID fixes its position; later files for the
//! same ID replace the selection only when preferred (the desired version, else the
//! higher version).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginFile {
    pub id: String,
    pub path: PathBuf,
    pub version: String,
}

/// Go `pluginIDPattern`: `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`.
pub fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b[0].is_ascii_alphanumeric()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// Go `validPluginVersion`: `^[0-9][0-9A-Za-z.+-]*$`.
pub fn valid_version(version: &str) -> bool {
    let b = version.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_digit()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'+' | b'-'))
}

/// Go `runtime.GOOS` for this build.
pub fn goos() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

/// Go `runtime.GOARCH` for this build.
pub fn goarch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "loongarch64" => "loong64",
        "powerpc64" if cfg!(target_endian = "little") => "ppc64le",
        "powerpc64" => "ppc64",
        other => other,
    }
}

/// Go `PluginExtension`.
pub fn extension(goos: &str) -> &'static str {
    match goos {
        "darwin" => ".dylib",
        "windows" => ".dll",
        _ => ".so",
    }
}

/// Go `filepath.Clean`.
pub fn clean(path: &Path) -> PathBuf {
    let absolute = path.has_root();
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for c in path.components() {
        match c {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..".as_ref());
                }
            }
            Component::Normal(p) => parts.push(p),
        }
    }
    let mut out = if absolute { PathBuf::from("/") } else { PathBuf::new() };
    for p in parts {
        out.push(p);
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Go `filepath.Rel` (Unix rules): `targ` relative to `base`, both cleaned first;
/// `None` when one is absolute and the other is not, or `base` climbs past `targ`.
/// ponytail: Windows volume names and case-insensitive elements are not handled; the
/// plugin host only loads native plugins on Unix.
pub fn rel(base: &str, targ: &str) -> Option<String> {
    let base = clean(Path::new(base)).to_string_lossy().into_owned();
    let targ = clean(Path::new(targ)).to_string_lossy().into_owned();
    if targ == base {
        return Some(".".into());
    }
    let base = if base == "." { String::new() } else { base };
    if base.starts_with('/') != targ.starts_with('/') {
        return None;
    }
    let (b, t) = (base.as_bytes(), targ.as_bytes());
    let (mut b0, mut bi, mut t0, mut ti) = (0, 0, 0, 0);
    loop {
        while bi < b.len() && b[bi] != b'/' {
            bi += 1;
        }
        while ti < t.len() && t[ti] != b'/' {
            ti += 1;
        }
        if t[t0..ti] != b[b0..bi] {
            break;
        }
        if bi < b.len() {
            bi += 1;
        }
        if ti < t.len() {
            ti += 1;
        }
        b0 = bi;
        t0 = ti;
    }
    if &b[b0..bi] == b"..".as_slice() {
        return None;
    }
    if b0 != b.len() {
        let seps = b[b0..].iter().filter(|c| **c == b'/').count();
        let mut out = String::from("..");
        for _ in 0..seps {
            out.push_str("/..");
        }
        if t0 != t.len() {
            out.push('/');
            out.push_str(&targ[t0..]);
        }
        return Some(clean(Path::new(&out)).to_string_lossy().into_owned());
    }
    Some(targ[t0..].to_owned())
}

/// Go `pluginFileFromPath`.
pub fn file_from_path(path: &Path, required_extension: &str) -> Option<PluginFile> {
    let base = path.file_name()?.to_string_lossy().into_owned();
    let lower = base.to_ascii_lowercase();
    let extension = if required_extension.trim().is_empty() {
        [".so", ".dylib", ".dll"].into_iter().find(|e| lower.ends_with(e))?
    } else {
        let required = required_extension.trim().to_ascii_lowercase();
        if !lower.ends_with(&required) {
            return None;
        }
        return split_name(&base, required.len(), path);
    };
    split_name(&base, extension.len(), path)
}

fn split_name(base: &str, ext_len: usize, path: &Path) -> Option<PluginFile> {
    let name = &base[..base.len() - ext_len];
    let mut id = name.to_owned();
    let mut version = String::new();
    if let Some(i) = name.rfind("-v").filter(|i| *i > 0) {
        let (candidate_id, candidate_version) = (&name[..i], &name[i + 2..]);
        if valid_id(candidate_id) && valid_version(candidate_version) {
            id = candidate_id.to_owned();
            version = candidate_version.to_owned();
        }
    }
    valid_id(&id).then(|| PluginFile {
        id,
        path: path.to_owned(),
        version,
    })
}

/// Go `comparePluginVersions`: dot-separated non-negative integers; `None` when either
/// side has a non-numeric segment. Missing segments count as 0.
pub fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let (sa, sb): (Vec<&str>, Vec<&str>) = (a.split('.').collect(), b.split('.').collect());
    for i in 0..sa.len().max(sb.len()) {
        let seg = |s: &[&str]| match s.get(i) {
            None => Some(0),
            Some(x) => x.parse::<i64>().ok().filter(|n| *n >= 0),
        };
        let (na, nb) = (seg(&sa)?, seg(&sb)?);
        if na != nb {
            return Some(na.cmp(&nb));
        }
    }
    Some(std::cmp::Ordering::Equal)
}

/// Go `pluginFilePreferred`.
fn preferred(candidate: &PluginFile, current: &PluginFile) -> bool {
    if candidate.version.is_empty() {
        return false;
    }
    if current.version.is_empty() {
        return true;
    }
    match compare_versions(&candidate.version, &current.version) {
        Some(ordering) => ordering.is_gt(),
        None => candidate.version > current.version,
    }
}

/// Go `pluginFilePreferredForDesired`.
fn preferred_for_desired(candidate: &PluginFile, current: &PluginFile, desired: &str) -> bool {
    let desired = crate::config::normalize_version(desired);
    if !desired.is_empty() {
        let (cm, um) = (candidate.version == desired, current.version == desired);
        if cm != um {
            return cm;
        }
    }
    preferred(candidate, current)
}

/// Go `candidateDirs`.
pub fn candidate_dirs(root: &Path, goos: &str, goarch: &str) -> [PathBuf; 2] {
    [root.join(goos).join(goarch), root.to_owned()]
}

/// Go `selectPluginFilesWithCandidates`: the selected file per ID (in first-seen order)
/// and every candidate. An ID whose desired version has no file is not selected.
pub fn select_with_candidates(
    root: &Path,
    desired: &BTreeMap<String, String>,
) -> std::io::Result<(Vec<PluginFile>, Vec<PluginFile>)> {
    let root = if root.as_os_str().is_empty() {
        Path::new("plugins")
    } else {
        root
    };
    let desired: HashMap<String, String> = desired
        .iter()
        .filter_map(|(id, v)| {
            let (id, v) = (id.trim(), crate::config::normalize_version(v));
            (!id.is_empty() && !v.is_empty()).then(|| (id.to_owned(), v))
        })
        .collect();
    let extension = extension(goos());
    let mut selected: HashMap<String, PluginFile> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut all = Vec::new();
    for dir in candidate_dirs(root, goos(), goarch()) {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry?;
            // Go's DirEntry type is lstat-based: symlinks are not regular files.
            if !entry.file_type()?.is_file() {
                continue;
            }
            if entry
                .file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(extension)
            {
                files.push(dir.join(entry.file_name()));
            }
        }
        files.sort_by(|a, b| a.as_os_str().as_encoded_bytes().cmp(b.as_os_str().as_encoded_bytes()));
        for path in files {
            let Some(file) = file_from_path(&path, extension) else {
                continue;
            };
            all.push(file.clone());
            match selected.get(&file.id) {
                None => {
                    order.push(file.id.clone());
                    selected.insert(file.id.clone(), file);
                }
                Some(current) => {
                    let want = desired.get(&file.id).map(String::as_str).unwrap_or_default();
                    if preferred_for_desired(&file, current, want) {
                        selected.insert(file.id.clone(), file);
                    }
                }
            }
        }
    }
    let picked = order
        .into_iter()
        .filter_map(|id| {
            let file = selected.remove(&id)?;
            match desired.get(&id) {
                Some(want) if file.version != *want => None,
                _ => Some(file),
            }
        })
        .collect();
    Ok((picked, all))
}

/// Go `selectPluginFiles` / `DiscoverPluginFiles`.
pub fn discover(root: &Path, desired: &BTreeMap<String, String>) -> std::io::Result<Vec<PluginFile>> {
    select_with_candidates(root, desired).map(|(selected, _)| selected)
}

/// Go `cleanupUnselectedPluginFiles`: removes other versions of plugins that loaded.
pub fn cleanup_unselected(root: &Path, loaded: &[PluginFile]) -> Result<(), String> {
    if loaded.is_empty() {
        return Ok(());
    }
    let (_, candidates) = select_with_candidates(root, &BTreeMap::new()).map_err(|e| e.to_string())?;
    let mut keep: HashMap<&str, HashSet<PathBuf>> = HashMap::new();
    for file in loaded {
        if !file.id.trim().is_empty() && !file.path.as_os_str().is_empty() {
            keep.entry(file.id.as_str()).or_default().insert(clean(&file.path));
        }
    }
    let mut errors = Vec::new();
    for candidate in candidates {
        let Some(paths) = keep.get(candidate.id.as_str()) else {
            continue;
        };
        if paths.contains(&clean(&candidate.path)) {
            continue;
        }
        match std::fs::remove_file(&candidate.path) {
            Ok(()) => {
                tracing::info!(plugin_id = %candidate.id, version = %candidate.version, path = %candidate.path.display(), "pluginhost: old plugin file removed")
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %candidate.path.display(), "pluginhost: failed to remove old plugin file {}: {e}", candidate.path.display());
                errors.push(e.to_string());
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_versions_follow_go() {
        let f = file_from_path(Path::new("/p/my-plugin-v1.2.0.SO"), "").unwrap();
        assert_eq!((f.id.as_str(), f.version.as_str()), ("my-plugin", "1.2.0"));
        let f = file_from_path(Path::new("/p/a-vbeta.so"), ".so").unwrap();
        assert_eq!((f.id.as_str(), f.version.as_str()), ("a-vbeta", ""));
        assert!(file_from_path(Path::new("/p/-v1.so"), ".so").is_none());
        assert!(file_from_path(Path::new("/p/a.dll"), ".so").is_none());
        assert_eq!(compare_versions("1.10", "1.9"), Some(std::cmp::Ordering::Greater));
        assert_eq!(compare_versions("1.0", "1"), Some(std::cmp::Ordering::Equal));
        assert_eq!(compare_versions("1.0-rc1", "1.0"), None);
        assert!(valid_id("a.b_c-d") && !valid_id(".a") && !valid_id(&"a".repeat(129)));
    }

    /// Go `TestSelectPluginFiles*`: the arch directory fixes order, the highest version
    /// wins, a desired version wins over a higher one and hides the ID when missing.
    #[test]
    fn selection_prefers_desired_then_highest() {
        let root = std::env::temp_dir().join(format!("cpa-plugin-select-{}", std::process::id()));
        let arch = root.join(goos()).join(goarch());
        std::fs::create_dir_all(&arch).unwrap();
        let ext = extension(goos());
        for name in ["b-v1.0.0", "b-v1.10.0", "a", "c-v2.0.0"] {
            std::fs::write(root.join(format!("{name}{ext}")), b"").unwrap();
        }
        std::fs::write(arch.join(format!("z-v0.1.0{ext}")), b"").unwrap();
        let pick = |desired: &[(&str, &str)]| {
            let desired = desired.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            discover(&root, &desired)
                .unwrap()
                .into_iter()
                .map(|f| format!("{}@{}", f.id, f.version))
                .collect::<Vec<_>>()
        };
        assert_eq!(pick(&[]), ["z@0.1.0", "a@", "b@1.10.0", "c@2.0.0"]);
        assert_eq!(pick(&[("b", "v1.0.0"), ("c", "3.0.0")]), ["z@0.1.0", "a@", "b@1.0.0"]);
        let loaded = discover(&root, &[("b".to_owned(), "1.0.0".to_owned())].into()).unwrap();
        cleanup_unselected(&root, &loaded).unwrap();
        assert!(!root.join(format!("b-v1.10.0{ext}")).exists());
        assert!(root.join(format!("b-v1.0.0{ext}")).exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
