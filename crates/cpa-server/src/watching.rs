//! Hot reload of `config.yaml` and the auth directory (Go internal/watcher).
//!
//! Go watches the config file and the top level of the auth directory with fsnotify.
//! A config event reloads after 150 ms without further events (`configReloadDebounce`);
//! auth events apply at once, removals after a 50 ms replace check
//! (`replaceCheckDelay`). Both compare content hashes, so self-writes and
//! unchanged rewrites are no-ops.
//!
//! ponytail: portable metadata polling stands in for fsnotify. Every 50 ms the watcher
//! stats the config file and the top-level `*.json` auth files and hashes only files
//! whose metadata changed or that were modified in the last two seconds (git's racy
//! timestamp rule), so the steady state reads nothing. A change applies once it has
//! been stable for 150 ms (config) or one tick (auth files).
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use cpa_core::config::{Config, credentials};
use cpa_core::credential::{Credential, Source};
use sha2::{Digest, Sha256};

use crate::management::Management;

const TICK: Duration = Duration::from_millis(50);
/// Go `configReloadDebounce`.
const CONFIG_SETTLE: Duration = Duration::from_millis(150);
/// Go `replaceCheckDelay`: one tick.
const AUTH_SETTLE: Duration = Duration::from_millis(50);
/// Files modified this recently are re-hashed even when their metadata is unchanged:
/// coarse file timestamps cannot tell two quick same-size writes apart.
const RACY: Duration = Duration::from_secs(2);

type Hash = [u8; 32];

/// Content identity of everything the watcher observes.
#[derive(Clone, Default, PartialEq, Eq)]
struct Snapshot {
    /// `None` when the config file cannot be read (missing).
    config: Option<Hash>,
    auth: BTreeMap<PathBuf, Hash>,
}

/// File metadata that changes with any write or replacement.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stat {
    len: u64,
    ino: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stat {
    fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            len: meta.len(),
            ino: meta.ino(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }
}

/// Hashes by path, reused while a file's metadata is unchanged and not racy.
#[derive(Default)]
struct HashCache(BTreeMap<PathBuf, (Stat, Hash)>);

impl HashCache {
    /// The content hash of `path`, or `None` when it cannot be read.
    fn hash(&mut self, path: &Path, meta: &std::fs::Metadata, now: SystemTime) -> Option<Hash> {
        let stat = Stat::of(meta);
        let racy = meta
            .modified()
            .map_or(true, |m| now.duration_since(m).map_or(true, |age| age < RACY));
        if let Some((cached, hash)) = self.0.get(path)
            && *cached == stat
            && !racy
        {
            return Some(*hash);
        }
        let data = std::fs::read(path).ok()?;
        let hash: Hash = Sha256::digest(&data).into();
        self.0.insert(path.to_owned(), (stat, hash));
        Some(hash)
    }
}

fn is_auth_json(path: &Path) -> bool {
    // Go matches the extension case-insensitively when scanning.
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().to_lowercase().ends_with(".json"))
}

fn observe(state: &Management, cache: &mut HashCache) -> Snapshot {
    let now = SystemTime::now();
    let mut seen = Vec::new();
    let config = std::fs::metadata(&state.path)
        .ok()
        .and_then(|meta| cache.hash(&state.path, &meta, now));
    seen.push(state.path.clone());
    let mut auth = BTreeMap::new();
    let dir = state.rt.config().auth_dir.clone();
    // An unreadable auth dir is an empty set, as in reload.
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if !meta.is_file() || !is_auth_json(&path) {
            continue;
        }
        // An unreadable file is present but unknown; reload skips it.
        let hash = cache.hash(&path, &meta, now).unwrap_or_default();
        seen.push(path.clone());
        auth.insert(path, hash);
    }
    cache.0.retain(|path, _| seen.contains(path));
    Snapshot { config, auth }
}

/// Go's per-event log lines for auth files that changed between two snapshots.
fn log_auth_changes(before: &Snapshot, after: &Snapshot) {
    let name = |p: &Path| p.file_name().unwrap_or_default().to_string_lossy().into_owned();
    for (path, hash) in &after.auth {
        let op = match before.auth.get(path) {
            None => "CREATE",
            Some(old) if old != hash => "WRITE",
            Some(_) => continue,
        };
        tracing::info!("auth file changed ({op}): {}, processing incrementally", name(path));
    }
    for path in before.auth.keys().filter(|p| !after.auth.contains_key(*p)) {
        tracing::info!("auth file changed (REMOVE): {}, processing incrementally", name(path));
    }
}

pub fn start(state: &Arc<Management>) -> tokio::task::JoinHandle<()> {
    let state = Arc::downgrade(state);
    tokio::spawn(async move {
        let mut cache = HashCache::default();
        // The state last reloaded; the first stable observation reloads once, as Go's
        // watcher reloads clients when it starts.
        let mut applied: Option<Snapshot> = None;
        let mut observed: Option<Snapshot> = None;
        let mut since = Instant::now();
        let mut ticks = tokio::time::interval(TICK);
        loop {
            ticks.tick().await;
            let Some(state) = state.upgrade() else {
                return;
            };
            let (snapshot, returned) = tokio::task::spawn_blocking({
                let state = state.clone();
                move || {
                    let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
                    let snapshot = observe(&state, &mut cache);
                    (snapshot, cache)
                }
            })
            .await
            .unwrap_or_default();
            cache = returned;
            if observed.as_ref() != Some(&snapshot) {
                observed = Some(snapshot);
                since = Instant::now();
                continue;
            }
            let Some(current) = observed.clone() else { continue };
            if applied.as_ref() == Some(&current) {
                continue;
            }
            let config_changed = applied.as_ref().is_none_or(|a| a.config != current.config);
            if since.elapsed() < if config_changed { CONFIG_SETTLE } else { AUTH_SETTLE } {
                continue;
            }
            if let Some(before) = &applied {
                log_auth_changes(before, &current);
                if config_changed && current.config.is_some() {
                    tracing::info!("config file changed, reloading: {}", state.path.display());
                }
            }
            let loaded = tokio::task::spawn_blocking({
                let state = state.clone();
                move || reload(&state)
            })
            .await;
            if let Some(before) = &applied {
                let accepted = matches!(loaded, Ok(Ok(())));
                persist_changes(&state, before, &current, config_changed && accepted);
            }
            match loaded {
                Ok(Ok(())) if config_changed && applied.is_some() => {
                    tracing::info!("config successfully reloaded, triggering client reload");
                }
                Ok(Err(error)) if config_changed || applied.is_none() => match error {
                    ReloadError::Missing(e) => tracing::error!(
                        "failed to read config file for hash check: open {}: {}",
                        state.path.display(),
                        go_errno_text(&e)
                    ),
                    ReloadError::Invalid(e) => tracing::error!("failed to reload config: {e:#}"),
                },
                _ => {}
            }
            // Applied once either way: a failed config load retries on its next change,
            // as Go retries on its next file event, rather than every tick.
            applied = Some(current);
        }
    })
}

/// Go `persistConfigAsync` / `persistAuthAsync`: with a remote store, what changed
/// between two applied snapshots is pushed in the background. The config goes only
/// after a reload accepted it, and never when empty or missing (Go ignores those
/// events). An auth write goes as "Sync auth <name>" unless the file is empty or not a
/// JSON object; a removal goes as "Remove auth <name>".
// ponytail: Go also skips files whose JSON does not decode into its `Auth` struct or
// whose weight is invalid; here any JSON object is pushed.
fn persist_changes(state: &Management, before: &Snapshot, after: &Snapshot, config_accepted: bool) {
    let Some(store) = state.store.clone() else {
        return;
    };
    let config = config_accepted && std::fs::metadata(&state.path).is_ok_and(|m| m.len() > 0);
    let mut auth = Vec::new();
    for (path, hash) in &after.auth {
        if before.auth.get(path) == Some(hash) {
            continue;
        }
        let object = std::fs::read(path)
            .ok()
            .filter(|data| !data.is_empty())
            .is_some_and(|data| serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&data).is_ok());
        if object {
            auth.push(("Sync", path.clone()));
        }
    }
    for path in before.auth.keys().filter(|p| !after.auth.contains_key(*p)) {
        auth.push(("Remove", path.clone()));
    }
    if !config && auth.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let bound = Duration::from_secs(30);
        if config {
            match tokio::time::timeout(bound, store.persist_config()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!("failed to persist config change: {error:#}"),
                Err(_) => tracing::error!("failed to persist config change: context deadline exceeded"),
            }
        }
        for (verb, path) in auth {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let message = format!("{verb} auth {name}");
            match tokio::time::timeout(bound, store.persist_auth_files(message, vec![path])).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!("failed to persist auth changes: {error:#}"),
                Err(_) => tracing::error!("failed to persist auth changes: context deadline exceeded"),
            }
        }
    });
}

/// Go's `syscall.Errno` text: strerror in lower case, without Rust's suffix.
fn go_errno_text(e: &std::io::Error) -> String {
    let text = e.to_string();
    let text = text.split(" (os error").next().unwrap_or_default();
    let mut chars = text.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_lowercase().chain(chars).collect())
}

/// Why [`reload`] kept the last good config.
#[derive(Debug)]
pub enum ReloadError {
    /// The config file could not be read (Go logs it and keeps running).
    Missing(std::io::Error),
    /// The config file did not load (Go `failed to reload config`).
    Invalid(anyhow::Error),
}

impl std::fmt::Display for ReloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(e) => write!(f, "{e}"),
            Self::Invalid(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for ReloadError {}

/// Blocking reload. Shared with deterministic tests; no provider calls are made.
///
/// A missing, empty or rejected config keeps the last good config, and the auth
/// directory is reconciled against it anyway: Go handles auth events independently
/// of the config file.
pub fn reload(state: &Management) -> Result<(), ReloadError> {
    let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
    let (mut config, config_error) = match std::fs::metadata(&state.path) {
        Err(error) => ((*state.rt.config()).clone(), Some(ReloadError::Missing(error))),
        // Go ignores an empty config write.
        Ok(meta) if meta.len() == 0 => ((*state.rt.config()).clone(), None),
        Ok(_) => match Config::load(&state.path) {
            Ok(config) => (config, None),
            Err(error) => ((*state.rt.config()).clone(), Some(ReloadError::Invalid(error))),
        },
    };
    state.lock_auth_dir(&mut config);
    let mut files = credentials::from_auth_dir(&config);
    // A malformed in-place auth write is not a deletion. Keep the last good value;
    // actual removal and a valid disabled update are reconciled normally.
    for existing in state.rt.store().snapshot() {
        if let Source::File(path) = &existing.source
            && path.parent() == Some(config.auth_dir.as_path())
            && path.exists()
            && !files.iter().any(|c| c.id == existing.id)
            && std::fs::read(path).ok().is_some_and(|data| {
                serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&data).is_err()
            })
        {
            files.push(Credential::clone(&existing));
        }
    }
    state.publish(config, Some(files));
    match config_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_cache_rereads_only_changed_or_racy_files() {
        let dir = std::env::temp_dir().join(format!("cpa-watch-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.json");
        std::fs::write(&path, b"one").unwrap();
        let old = SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let mut cache = HashCache::default();
        let now = SystemTime::now();
        let first = cache.hash(&path, &std::fs::metadata(&path).unwrap(), now).unwrap();
        // Same metadata, old mtime: the cached hash is reused even if the bytes were
        // swapped behind the cache's back (proves no re-read happens).
        let stat = cache.0[&path].0;
        cache.0.insert(path.clone(), (stat, [7; 32]));
        assert_eq!(
            cache.hash(&path, &std::fs::metadata(&path).unwrap(), now),
            Some([7; 32])
        );
        // A real change alters the metadata and is hashed again.
        std::fs::write(&path, b"two!").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let second = cache.hash(&path, &std::fs::metadata(&path).unwrap(), now).unwrap();
        assert_ne!(first, second);
        assert_eq!(second, <Hash>::from(Sha256::digest(b"two!")));
        // A recently modified file is re-read even with identical cached metadata.
        std::fs::write(&path, b"thr!").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        cache.0.insert(path.clone(), (Stat::of(&meta), [7; 32]));
        assert_eq!(
            cache.hash(&path, &meta, SystemTime::now()),
            Some(Sha256::digest(b"thr!").into())
        );
        let _ = std::fs::remove_file(&path);
    }
}
