//! Hot reload of `config.yaml` and the auth directory (Go internal/watcher).
//!
//! Go watches the config file and the top level of the auth directory with fsnotify.
//! A config event reloads after 150 ms without further events (`configReloadDebounce`);
//! auth events apply at once, removals after a 50 ms replace check
//! (`replaceCheckDelay`). Both compare content hashes, so self-writes and
//! unchanged rewrites are no-ops.
//!
//! The watcher sleeps until the operating system reports a change in the config file's
//! folder or the auth folder ([`crate::fs_events`]). It then stats the config file and
//! the top-level `*.json` auth files every 50 ms, hashing only files whose metadata
//! changed or that were modified in the last two seconds (git's racy timestamp rule),
//! until the change has been stable for 150 ms (config) or one tick (auth files) and is
//! applied. While nothing changes it does no work and sets no timer. As in Go, changes
//! made by another machine on a network or FUSE file system raise no event and are seen
//! with the next local change.
use cpa_core::gostr::GoStr;
use std::collections::{BTreeMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use cpa_core::config::{Config, credentials};
use cpa_core::credential::{Credential, Source};
use sha2::{Digest, Sha256};

use crate::fs_events::{Events, Targets};
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
    #[cfg(unix)]
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            len: meta.len(),
            ino: meta.ino(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }

    /// ponytail: Windows exposes no stable file identity or change time here, so a
    /// same-size replacement with an unchanged modification time older than two
    /// seconds goes unnoticed until its next change.
    #[cfg(not(unix))]
    fn of(meta: &std::fs::Metadata) -> Self {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or((0, 0), |d| (d.as_secs() as i64, i64::from(d.subsec_nanos())));
        Self {
            len: meta.len(),
            ino: 0,
            mtime,
            ctime: (0, 0),
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
        .is_some_and(|n| n.to_string_lossy().go_lower().ends_with(".json"))
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
    let first = targets(state, None);
    // A plugin auth provider loaded, unloaded or fused changes what auth files load to
    // with no file changing: the change wakes this task, which reloads the auth folder.
    let plugin_auth = Arc::new(tokio::sync::Notify::new());
    credentials::on_plugin_auth_change(Some(Arc::new({
        let plugin_auth = plugin_auth.clone();
        move || plugin_auth.notify_one()
    })));
    let weak = Arc::downgrade(state);
    let state = state.clone();
    tokio::spawn(async move {
        // Off the request threads: reading the file waits for the disk lock, setup can
        // block on a slow network path, and on Windows it waits for the watch thread. A
        // panic in the setup leaves the 2-second poll. Go stores its own serialized
        // config snapshot, independently of management mutations of the published
        // runtime config before their disk event arrives.
        let started = tokio::task::spawn_blocking(move || {
            let snapshot = {
                let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
                let hash = std::fs::read(&state.path)
                    .ok()
                    .map(|data| <Hash>::from(Sha256::digest(data)));
                (state.rt.config(), hash)
            };
            let events = catch_unwind(AssertUnwindSafe(|| Events::new(&first))).unwrap_or_default();
            (snapshot, events)
        })
        .await;
        let Ok(((mut previous_config, initial_hash), mut events)) = started else {
            return;
        };
        let state = weak;
        let mut cache = HashCache::default();
        // The state last reloaded; the first stable observation reloads once, as Go's
        // watcher reloads clients when it starts.
        let mut applied: Option<Snapshot> = None;
        let mut observed: Option<Snapshot> = None;
        let mut since = Instant::now();
        // `credentials::plugin_auth_epoch` when the last reload started.
        let mut applied_epoch = credentials::plugin_auth_epoch();
        loop {
            let woke = applied.is_some() && applied == observed && applied_epoch == credentials::plugin_auth_epoch();
            if woke {
                tokio::select! {
                    () = events.changed() => {}
                    () = plugin_auth.notified() => {}
                }
            } else {
                tokio::time::sleep(TICK).await;
            }
            let Some(state) = state.upgrade() else {
                return;
            };
            // After an event, retarget before looking: a file that lands before the new
            // watch exists shows up in this look, and one that lands after raises an
            // event. Cost per wake: a few watch calls (one stat per watched file on
            // macOS), on the blocking pool.
            let wake_targets = woke.then(|| targets(&state, observed.as_ref()));
            // A panic in the work is caught inside the closure, so `events` always comes
            // back and notifications stay on.
            let looked = tokio::task::spawn_blocking({
                let state = state.clone();
                move || {
                    let snapshot = catch_unwind(AssertUnwindSafe(|| {
                        if let Some(t) = wake_targets {
                            events.retarget(&t);
                        }
                        let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
                        observe(&state, &mut cache)
                    }));
                    (snapshot.ok(), cache, events)
                }
            })
            .await;
            let snapshot = match looked {
                Ok((snapshot, returned_cache, returned_events)) => {
                    cache = returned_cache;
                    events = returned_events;
                    snapshot
                }
                // Cancelled at shutdown.
                Err(_) => {
                    cache = HashCache::default();
                    events = Events::default();
                    None
                }
            };
            let Some(snapshot) = snapshot else {
                tracing::error!(
                    "config watcher: looking at the config and auth files failed; retrying on the next change"
                );
                continue;
            };
            if observed.as_ref() != Some(&snapshot) {
                observed = Some(snapshot);
                since = Instant::now();
                continue;
            }
            let Some(current) = observed.clone() else { continue };
            let epoch = credentials::plugin_auth_epoch();
            if applied.as_ref() == Some(&current) && applied_epoch == epoch {
                continue;
            }
            let config_changed = applied.as_ref().map_or(initial_hash, |a| a.config) != current.config;
            // The first observation always reads the file: an edit made after the
            // runtime loaded its config but before `start` hashed the file matches
            // `initial_hash`, yet the runtime has never seen it.
            let read_config = config_changed || applied.is_none();
            if since.elapsed() < if read_config { CONFIG_SETTLE } else { AUTH_SETTLE } {
                continue;
            }
            if let Some(before) = &applied {
                log_auth_changes(before, &current);
                if config_changed && current.config.is_some() {
                    tracing::info!("config file changed, reloading: {}", state.path.display());
                }
            }
            // The reload may move auth-dir, and new auth files need their own watches on
            // macOS: retarget after it, on the same blocking thread. A change that lands
            // before a new watch exists is caught by the extra look `Events` then makes.
            applied_epoch = epoch;
            let reloaded = tokio::task::spawn_blocking({
                let state = state.clone();
                let current = current.clone();
                move || {
                    let loaded = catch_unwind(AssertUnwindSafe(|| reload_config(&state, read_config, &current)));
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        events.retarget(&targets(&state, Some(&current)));
                    }));
                    (loaded.map_err(|_| ()), events)
                }
            })
            .await;
            let (loaded, read) = match reloaded {
                Ok((loaded, returned)) => {
                    events = returned;
                    match loaded {
                        Ok((loaded, read)) => (Ok(loaded), Some(read)),
                        Err(()) => (Err(()), None),
                    }
                }
                Err(_) => {
                    events = Events::default();
                    (Err(()), None)
                }
            };
            if loaded.is_err() {
                tracing::error!("config watcher: reload failed unexpectedly; retrying on the next change");
            }
            if let Some(before) = &applied {
                let accepted = matches!(loaded, Ok(Ok(Some(_))));
                persist_changes(&state, before, &current, config_changed && accepted);
            }
            match loaded {
                Ok(Ok(Some(next_config))) => {
                    // The first observation's read of the file the runtime started
                    // with is quiet; Go does not reload at startup.
                    if config_changed || next_config.document != previous_config.document {
                        crate::config_diff::log(&previous_config, &next_config);
                        tracing::info!("config successfully reloaded, triggering client reload");
                    }
                    previous_config = Arc::new(next_config);
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
            applied = Some(record(&mut cache, current, read));
        }
    })
}

/// What a reload applied: what it read, not what was observed before it, so a write
/// landing in between is reloaded when it is next observed (Go records the parsed
/// config's `SourceHash` and forgets the hash of an incomplete auth read). When the two
/// differ, cached hashes may be the stale ones (a same-size write with the same
/// modification time, or a lasting read denial), so the next observation reads afresh.
fn record(cache: &mut HashCache, observed: Snapshot, read: Option<Snapshot>) -> Snapshot {
    let read = read.unwrap_or_else(|| observed.clone());
    if read != observed {
        *cache = HashCache::default();
    }
    read
}

/// What the watch should cover: the config file, the auth folder of the published
/// config, and the auth files last seen there.
fn targets(state: &Management, current: Option<&Snapshot>) -> Targets {
    Targets {
        config: state.path.clone(),
        auth_dir: state.rt.config().auth_dir.clone(),
        files: current.map(|s| s.auth.keys().cloned().collect()).unwrap_or_default(),
    }
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
    reload_config(state, true, &Snapshot::default()).0.map(|_| ())
}

/// Return the config actually accepted under the disk lock, not a later runtime
/// snapshot possibly replaced by management (None is an auth-only or empty reload), and
/// the hashes of what this reload read. Entries it did not read are as `observed`.
fn reload_config(
    state: &Management,
    read_config: bool,
    observed: &Snapshot,
) -> (Result<Option<Config>, ReloadError>, Snapshot) {
    // Plugin auth providers parse before the lock is taken, so one can save an auth or
    // wait on its own locks meanwhile; the scan under the lock takes their answers for
    // the bytes it reads and defers a file changed since (Go's synthesizer holds no
    // lock). One atomic load while no plugin offers an auth provider.
    if credentials::plugin_auth_parser_active() {
        credentials::preparse_auth_dir(&state.rt.config());
    }
    let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
    let mut read = Snapshot {
        config: observed.config,
        auth: BTreeMap::new(),
    };
    // Recorded as the observation records it: the hash of the bytes read, `None` when
    // the read fails.
    let (mut config, config_error, accepted) = match read_config.then(|| std::fs::read(&state.path)) {
        None => ((*state.rt.config()).clone(), None, false),
        Some(Err(error)) => {
            read.config = None;
            let error = if std::fs::metadata(&state.path).is_err() {
                ReloadError::Missing(error)
            } else {
                ReloadError::Invalid(error.into())
            };
            ((*state.rt.config()).clone(), Some(error), false)
        }
        Some(Ok(data)) => {
            read.config = Some(Sha256::digest(&data).into());
            let loaded = (!data.is_empty()).then(|| {
                std::str::from_utf8(&data)
                    .map_err(anyhow::Error::from)
                    .and_then(|text| Config::load_text(&state.path, text))
            });
            match loaded {
                // Go ignores an empty config write.
                None => ((*state.rt.config()).clone(), None, false),
                Some(Ok(config)) => (config, None, true),
                Some(Err(error)) => ((*state.rt.config()).clone(), Some(ReloadError::Invalid(error)), false),
            }
        }
    };
    state.lock_auth_dir(&mut config);
    let mut scan = credentials::scan_auth_dir(&config);
    keep_incomplete(state, &mut scan.credentials, &scan.incomplete);
    read.auth = scan.read.clone();
    for path in &scan.unapplied {
        read.auth.remove(path);
    }
    // A file neither could read stays as observed; recording it as missing would
    // reload it on every tick.
    for (path, hash) in &observed.auth {
        if *hash == Hash::default() {
            read.auth.entry(path.clone()).or_insert(*hash);
        }
    }
    let result = accepted.then(|| config.clone());
    state.publish(config, Some(scan));
    let result = match config_error {
        Some(error) => Err(error),
        None => Ok(result),
    };
    (result, read)
}

/// An empty or half-written file is not a deletion (Go 7484a521): keep the
/// last good credentials of the files the scan found incomplete. Deciding from the scan's
/// own read, never a second one, means a file completed after the scan is picked up by
/// the reload its write triggers instead of being dropped now. Removal and valid JSON
/// without a credential are reconciled normally.
fn keep_incomplete(state: &Management, files: &mut Vec<Credential>, incomplete: &HashSet<PathBuf>) {
    for existing in state.rt.store().snapshot() {
        if let Source::File(path) = &existing.source
            && incomplete.contains(path)
            && !files.iter().any(|c| c.id == existing.id)
        {
            files.push(Credential::clone(&existing));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (Arc<Management>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("cpa-watch-outcome-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(&path, format!("oauth: {{auth-dir: {}}}\n", dir.display())).unwrap();
        let rt = Arc::new(crate::testing::runtime(
            Config::load(&path).unwrap(),
            Vec::new(),
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        (Management::new(rt, path), dir)
    }

    /// Also Go `TestSetConfigAppliesCurrentFileAndNonFileConfigs_Issue6631`: an external
    /// edit is published by the reload that read it, and a config that did not come from
    /// the file (a management or Home publication) still applies.
    #[tokio::test]
    async fn accepted_config_is_captured_and_skipped_reloads_do_not_advance_it() {
        let (state, dir) = state();
        let config = |port| format!("port: {port}\noauth: {{auth-dir: {}}}\n", dir.display());
        std::fs::write(&state.path, config(9001)).unwrap();
        let accepted = reload_config(&state, true, &Snapshot::default()).0.unwrap().unwrap();
        state.rt.publish_config(Config::parse(&config(9002)).unwrap());
        assert_eq!(accepted.port, 9001); // Not the later management publication.
        std::fs::write(&state.path, "").unwrap();
        assert!(reload_config(&state, true, &Snapshot::default()).0.unwrap().is_none());
        assert_eq!(state.rt.config().port, 9002);
        std::fs::write(&state.path, config(9003)).unwrap();
        assert!(reload_config(&state, false, &Snapshot::default()).0.unwrap().is_none()); // Auth-only event.
        assert_eq!(state.rt.config().port, 9002);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// One watcher reload as `start` makes it: reload what was observed, then record
    /// what was applied.
    fn step(state: &Management, cache: &mut HashCache, read_config: bool, observed: &Snapshot) -> Snapshot {
        let (_, read) = reload_config(state, read_config, observed);
        record(cache, observed.clone(), Some(read))
    }

    /// The watcher records what its reload read, not what it observed before: a write
    /// landing after the read is reloaded when next observed, even when it restores
    /// bytes observed earlier (Go 1631875b
    /// `TestReloadConfigIfChangedReloadsWriteLandingDuringReload_Issue6631` and
    /// `TestReloadConfigIfChangedReloadsAfterWriteBetweenHashAndLoad_Issue6631`), and an
    /// auth file truncated before the scan and finished after it is reloaded (Go
    /// 7484a521 forgets the hash of an incomplete read). Go
    /// `TestReloadConfigIfChangedSerializesConcurrentReloads_Issue6631` has no Rust
    /// interleaving: one watcher task reloads, under the disk lock.
    #[tokio::test]
    async fn the_watcher_records_what_its_reload_read_not_what_it_observed() {
        let (state, dir) = state();
        let mut cache = HashCache::default();
        let config = |port| format!("port: {port}\noauth: {{auth-dir: {}}}\n", dir.display());
        std::fs::write(&state.path, config(9001)).unwrap();
        let observed = observe(&state, &mut cache);
        std::fs::write(&state.path, config(9002)).unwrap(); // Lands before the reload reads.
        let applied = step(&state, &mut cache, true, &observed);
        assert_eq!(state.rt.config().port, 9002);
        std::fs::write(&state.path, config(9001)).unwrap(); // Restores the observed bytes.
        let restored = observe(&state, &mut cache);
        assert!(restored != applied, "the restore must be reloaded");
        step(&state, &mut cache, true, &restored);
        assert_eq!(state.rt.config().port, 9001);

        let file = dir.join("a.json");
        let account = |email: &str| format!(r#"{{"type":"codex","email":"{email}"}}"#);
        let email = |state: &Management| {
            let creds = state.rt.store().snapshot();
            let cred = creds.iter().find(|c| c.id == "a.json").expect("a.json is loaded");
            cred.str("email").unwrap_or_default().to_owned()
        };
        std::fs::write(&file, account("a@example.com")).unwrap();
        reload(&state).unwrap();
        std::fs::write(&file, account("b@example.com")).unwrap();
        let observed = observe(&state, &mut cache);
        std::fs::write(&file, "").unwrap(); // Truncated before the scan.
        let applied = step(&state, &mut cache, false, &observed);
        assert_eq!(email(&state), "a@example.com"); // Kept while incomplete.
        std::fs::write(&file, account("b@example.com")).unwrap(); // Finished after it.
        let finished = observe(&state, &mut cache);
        assert!(
            finished == observed && finished != applied,
            "the finished write must be reloaded"
        );
        let applied = step(&state, &mut cache, false, &finished);
        assert_eq!(email(&state), "b@example.com");
        assert!(observe(&state, &mut cache) == applied, "nothing left to reload");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A config whose metadata reads as empty but whose contents cannot be read is
    /// recorded as the observation sees it, unreadable, so it does not reload every
    /// tick; it reloads once it changes.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_unreadable_empty_config_converges() {
        let (state, dir) = state();
        let mut cache = HashCache::default();
        let port = state.rt.config().port;
        std::fs::remove_file(&state.path).unwrap();
        // Zero bytes by its metadata, and every read fails (EIO), even for root.
        std::os::unix::fs::symlink("/proc/self/mem", &state.path).unwrap();
        let observed = observe(&state, &mut cache);
        assert_eq!(observed.config, None);
        let applied = step(&state, &mut cache, true, &observed);
        assert!(observe(&state, &mut cache) == applied, "no reload without a change");
        assert_eq!(state.rt.config().port, port);
        std::fs::remove_file(&state.path).unwrap();
        std::fs::write(
            &state.path,
            format!("port: 9005\noauth: {{auth-dir: {}}}\n", dir.display()),
        )
        .unwrap();
        let changed = observe(&state, &mut cache);
        assert!(changed != applied);
        step(&state, &mut cache, true, &changed);
        assert_eq!(state.rt.config().port, 9005);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A cached hash can be stale when a same-size write keeps the metadata (Windows has
    /// no inode or change time): once the reload read something else, the next
    /// observation reads afresh and agrees with it instead of reloading every tick.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stale_cached_hash_does_not_reload_forever() {
        let (state, dir) = state();
        let mut cache = HashCache::default();
        let file = dir.join("a.json");
        std::fs::write(&file, r#"{"type":"codex","email":"b@example.com"}"#).unwrap();
        let old = SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(old)
            .unwrap();
        // Not racy, and cached for an earlier same-size write with the same metadata.
        let meta = std::fs::metadata(&file).unwrap();
        cache.0.insert(file.clone(), (Stat::of(&meta), [7; 32]));
        let observed = observe(&state, &mut cache);
        assert_eq!(observed.auth[&file], [7; 32]);
        let applied = step(&state, &mut cache, false, &observed);
        assert!(
            state
                .rt
                .store()
                .snapshot()
                .iter()
                .any(|c| c.str("email") == Some("b@example.com"))
        );
        assert!(observe(&state, &mut cache) == applied, "converges after one reload");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go 7484a521: an empty or partial in-place write keeps the credential; valid
    /// JSON without one, or a removed file, deletes it (also the empty, partial and
    /// valid-but-empty files of Go `TestFileSynthesizerRecordsIncompleteFiles`).
    #[tokio::test]
    async fn incomplete_auth_writes_keep_the_credential_until_the_file_is_valid_json() {
        let (state, dir) = state();
        let file = dir.join("a.json");
        let loaded = |state: &Management| state.rt.store().snapshot().iter().any(|c| c.id == "a.json");
        std::fs::write(&file, r#"{"type":"codex","email":"a@example.com"}"#).unwrap();
        reload(&state).unwrap();
        assert!(loaded(&state));
        for (contents, kept) in [
            ("", true),
            (r#"{"type":"codex""#, true),
            (" \n", true),
            ("null", false),
            ("{}", false),
        ] {
            std::fs::write(&file, r#"{"type":"codex","email":"a@example.com"}"#).unwrap();
            reload(&state).unwrap();
            std::fs::write(&file, contents).unwrap();
            reload(&state).unwrap();
            assert_eq!(loaded(&state), kept, "{contents:?}");
        }
        std::fs::write(&file, r#"{"type":"codex"}"#).unwrap();
        reload(&state).unwrap();
        std::fs::remove_file(&file).unwrap();
        reload(&state).unwrap();
        assert!(!loaded(&state));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go 5fc580b1 (TestAuthDeleteFileRemovalClassification, the service delete recheck):
    /// a removal is decided from the folder when the reload reads it, so a file removed
    /// and re-created before the reload keeps its credential, and one re-created after a
    /// reload that dropped it comes back.
    #[tokio::test]
    async fn a_removed_and_recreated_file_keeps_its_credential() {
        let (state, dir) = state();
        let file = dir.join("a.json");
        let good = |email: &str| format!(r#"{{"type":"codex","email":"{email}"}}"#);
        let loaded = |state: &Management| {
            state
                .rt
                .store()
                .snapshot()
                .iter()
                .find(|c| c.id == "a.json")
                .and_then(|c| c.str("email").map(str::to_owned))
        };
        std::fs::write(&file, good("a@example.com")).unwrap();
        reload(&state).unwrap();
        assert_eq!(loaded(&state).as_deref(), Some("a@example.com"));
        std::fs::remove_file(&file).unwrap();
        std::fs::write(&file, good("b@example.com")).unwrap();
        reload(&state).unwrap();
        assert_eq!(
            loaded(&state).as_deref(),
            Some("b@example.com"),
            "re-created before the reload"
        );
        std::fs::remove_file(&file).unwrap();
        reload(&state).unwrap();
        assert_eq!(loaded(&state), None, "removed");
        std::fs::write(&file, good("c@example.com")).unwrap();
        reload(&state).unwrap();
        assert_eq!(
            loaded(&state).as_deref(),
            Some("c@example.com"),
            "re-created after the reload"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The editor finishes the file after the scan read it half-written and before the
    /// reload publishes: the old credential is kept (a second read would see valid JSON,
    /// keep nothing and drop the credential until the next reload).
    #[tokio::test]
    async fn a_file_completed_after_the_scan_keeps_its_credential() {
        let (state, dir) = state();
        let file = dir.join("a.json");
        let good = r#"{"type":"codex","email":"a@example.com"}"#;
        std::fs::write(&file, good).unwrap();
        reload(&state).unwrap();
        std::fs::write(&file, r#"{"type":"codex""#).unwrap();
        let scan = credentials::scan_auth_dir(&state.rt.config());
        let mut files = scan.credentials;
        assert!(scan.incomplete.contains(&file) && files.is_empty());
        std::fs::write(&file, good).unwrap();
        keep_incomplete(&state, &mut files, &scan.incomplete);
        assert_eq!(files.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["a.json"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go c4c1d43e through the publication the watcher makes: a token refresh committed
    /// after the reload's scan read the file is kept, not replaced by the scanned token.
    #[tokio::test]
    async fn a_reload_scan_does_not_undo_a_refresh_committed_after_it() {
        let (state, dir) = state();
        std::fs::write(dir.join("a.json"), r#"{"type":"codex","access_token":"old"}"#).unwrap();
        reload(&state).unwrap();
        let config = (*state.rt.config()).clone();
        let scan = credentials::scan_auth_dir(&config);
        let store = state.rt.store();
        let revision = store.get("a.json").unwrap().revision;
        let mut patch = cpa_core::credential::MetadataPatch::default();
        patch.set.insert("access_token".into(), "refreshed".into());
        store.apply_patch("a.json", revision, &patch).unwrap();
        state.publish(config, Some(scan));
        assert_eq!(store.get("a.json").unwrap().metadata["access_token"], "refreshed");
        reload(&state).unwrap();
        assert_eq!(store.get("a.json").unwrap().metadata["access_token"], "refreshed");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go c4c1d43e for upload fallbacks, which are rebuilt from a second read: a scan
    /// taken before an external edit (B) and a persist of the current credential (A)
    /// does not publish B over A, whether the auth-dir is absolute or relative (where
    /// the scan's paths are relative and the fallback's absolute).
    #[tokio::test]
    async fn a_reload_does_not_undo_a_write_to_an_upload_fallback() {
        for relative in [false, true] {
            let (state, dir) = state();
            // A relative auth-dir lives under the working directory: no relative path
            // reaches a temp folder on another Windows drive.
            let local = PathBuf::from(format!("cpa-watch-relative-{}", uuid::Uuid::new_v4()));
            if relative {
                std::fs::create_dir_all(&local).unwrap();
                std::fs::write(&state.path, format!("oauth: {{auth-dir: {}}}\n", local.display())).unwrap();
            }
            reload(&state).unwrap();
            let config = (*state.rt.config()).clone();
            assert_eq!(config.auth_dir.is_relative(), relative);
            // The path an upload records.
            let file = std::path::absolute(config.auth_dir.join("a.json")).unwrap();
            let a = r#"{"type":"gemini","v":"A"}"#;
            std::fs::write(&file, a).unwrap();
            state.fallbacks.lock().unwrap().insert(file.clone());
            reload(&state).unwrap();
            let store = state.rt.store();
            let before = store.get("a.json").unwrap();
            assert_eq!(before.metadata["v"], "A");
            let scan = credentials::scan_auth_dir(&config);
            std::fs::write(&file, r#"{"type":"gemini","v":"B"}"#).unwrap();
            let persisted = file.clone();
            crate::management::BEFORE_RECONCILE.lock().unwrap().push((
                config.auth_dir.clone(),
                Box::new(move || std::fs::write(persisted, a).unwrap()),
            ));
            state.publish(config, Some(scan));
            let after = store.get("a.json").unwrap();
            assert_eq!(
                (&after.metadata["v"], after.revision),
                (&"A".into(), before.revision),
                "relative: {relative}"
            );
            std::fs::remove_dir_all(dir).unwrap();
            if relative {
                std::fs::remove_dir_all(local).unwrap();
            }
        }
    }

    /// A fallback stays tracked while its file still holds it: when the scan read a
    /// synthesizable rewrite that the freshness check rejects, the next reload keeps the
    /// fallback; an accepted rewrite still retires it.
    #[tokio::test]
    async fn a_fallback_whose_replacement_was_rejected_stays_tracked() {
        let (state, dir) = state();
        let file = dir.join("a.json");
        let a = r#"{"type":"gemini","v":"A"}"#;
        std::fs::write(&file, a).unwrap();
        state.fallbacks.lock().unwrap().insert(file.clone());
        reload(&state).unwrap();
        let store = state.rt.store();
        let before = store.get("a.json").unwrap();
        let codex = r#"{"type":"codex","access_token":"B"}"#;
        std::fs::write(&file, codex).unwrap();
        let config = (*state.rt.config()).clone();
        let scan = credentials::scan_auth_dir(&config);
        let persisted = file.clone();
        crate::management::BEFORE_RECONCILE.lock().unwrap().push((
            config.auth_dir.clone(),
            Box::new(move || std::fs::write(persisted, a).unwrap()),
        ));
        state.publish(config, Some(scan));
        let kept = store.get("a.json").unwrap();
        assert_eq!((kept.provider.as_str(), kept.revision), ("gemini", before.revision));
        reload(&state).unwrap();
        assert_eq!(store.get("a.json").unwrap().provider, "gemini");
        assert!(state.fallbacks.lock().unwrap().contains(&file));
        std::fs::write(&file, codex).unwrap();
        reload(&state).unwrap();
        assert_eq!(store.get("a.json").unwrap().provider, "codex");
        assert!(state.fallbacks.lock().unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A file that cannot be read (here a symlink loop, for every user) drops its
    /// credential, as Go's full scan does; only a read that succeeded can mark a file
    /// incomplete. The watcher does not observe such an entry, so keeping it would
    /// leave the credential live after the entry is deleted.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_file_drops_its_credential() {
        let (state, dir) = state();
        let file = dir.join("a.json");
        std::fs::write(&file, r#"{"type":"codex","email":"a@example.com"}"#).unwrap();
        reload(&state).unwrap();
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&file, &file).unwrap();
        assert!(std::fs::read(&file).is_err());
        reload(&state).unwrap();
        assert!(!state.rt.store().snapshot().iter().any(|c| c.id == "a.json"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The scan counts the entries the watcher observes: a credential whose file is
    /// replaced by a link to `/dev/null` is dropped by the reload that replacement
    /// triggers, so deleting the link (which the watcher cannot see, having never
    /// observed it) leaves nothing live.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_file_replaced_by_a_special_file_link_drops_its_credential() {
        let (state, dir) = state();
        let file = dir.join("a.json");
        std::fs::write(&file, r#"{"type":"codex","email":"a@example.com"}"#).unwrap();
        reload(&state).unwrap();
        let loaded = |state: &Management| state.rt.store().snapshot().iter().any(|c| c.id == "a.json");
        assert!(loaded(&state));
        let capture = Capture::default();
        let _guard = capture.install();
        let watcher = start(&state);
        tokio::time::sleep(Duration::from_millis(500)).await;
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink("/dev/null", &file).unwrap();
        capture.wait_for("auth file changed (REMOVE): a.json").await;
        tokio::time::timeout(Duration::from_secs(3), async {
            while loaded(&state) {
                tokio::time::sleep(TICK).await;
            }
        })
        .await
        .expect("the reload the replacement triggered drops the credential");
        std::fs::remove_file(&file).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!loaded(&state));
        watcher.abort();
        let _ = watcher.await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>, Arc<tokio::sync::Notify>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            self.1.notify_one();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        fn install(&self) -> tracing::subscriber::DefaultGuard {
            let writer = self.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::set_default(subscriber)
        }

        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }

        async fn wait_for(&self, needle: &str) {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let notified = self.1.notified();
                    if self.text().contains(needle) {
                        break;
                    }
                    notified.await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{needle:?} was not logged:\n{}", self.text()));
        }
    }

    /// Go `updateInternalLocked` through the watcher's incremental reload: a credential
    /// an upstream 401 blocked is selectable as soon as its file holds a new access token
    /// (a re-login), with no restart, and a model's 429 cooldown on it stays. An edit
    /// that keeps the tokens keeps the block.
    #[tokio::test]
    async fn a_rewritten_token_clears_its_401_block_and_keeps_a_429() {
        use crate::runtime::{Outcome, Selection};
        use cpa_core::exec::{ExecError, FailureScope};
        let (state, dir) = state();
        let file = dir.join("claude-a.json");
        let doc = |token: &str, email: &str| {
            serde_json::json!({"type": "claude", "access_token": token, "refresh_token": "fake-rt", "email": email})
                .to_string()
        };
        std::fs::write(&file, doc("sk-ant-oat-old-fake", "a@example.invalid")).unwrap();
        let watcher = start(&state);
        let store = state.rt.store();
        let wait = |want: &'static str| {
            let store = store.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while store.get("claude-a.json").is_none_or(|c| c.str("email") != Some(want)) {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("{want} was not reloaded"));
            }
        };
        wait("a@example.invalid").await;
        let select = |model: &str| store.select(Selection::new("claude", model));
        let mut quota = ExecError::local(429, FailureScope::Model, "rate limited");
        quota.retry_after = Some(Duration::from_secs(3600));
        select("q").unwrap().complete(Outcome::Failure(quota));
        let unauthorized = ExecError::local(
            401,
            FailureScope::Credential,
            "authentication_error: OAuth access token",
        );
        select("m").unwrap().complete(Outcome::Failure(unauthorized));
        assert!(select("m").is_none(), "the 401 blocks the credential");
        // Same tokens, another email: Go keeps the block.
        std::fs::write(&file, doc("sk-ant-oat-old-fake", "b@example.invalid")).unwrap();
        wait("b@example.invalid").await;
        assert!(select("m").is_none(), "an edit that keeps the tokens keeps the block");
        std::fs::write(&file, doc("sk-ant-oat-new-fake", "c@example.invalid")).unwrap();
        wait("c@example.invalid").await;
        let lease = select("m").expect("the new token is selectable at once");
        assert_eq!(lease.credential.str("access_token"), Some("sk-ant-oat-new-fake"));
        drop(lease);
        assert!(select("q").is_none(), "the 429 cooldown survives the reload");
        let left = store.cooldowns("claude-a.json");
        assert_eq!(
            left.iter().map(|c| (c.model.as_str(), c.status)).collect::<Vec<_>>(),
            [("q", 429)]
        );
        watcher.abort();
        let _ = watcher.await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn edit_before_first_stable_observation_is_logged_once() {
        let (state, dir) = state();
        let capture = Capture::default();
        let _guard = capture.install();
        let watcher = start(&state);
        std::fs::write(
            &state.path,
            format!("port: 9001\noauth: {{auth-dir: {}}}\n", dir.display()),
        )
        .unwrap();
        capture.wait_for("port: 0 -> 9001").await;
        watcher.abort();
        let _ = watcher.await;
        assert_eq!(capture.text().matches("port: 0 -> 9001").count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn edit_between_runtime_load_and_start_is_applied() {
        let (state, dir) = state();
        let capture = Capture::default();
        let _guard = capture.install();
        // The runtime holds port 0; the file changes before the watcher hashes it.
        std::fs::write(
            &state.path,
            format!("port: 9001\noauth: {{auth-dir: {}}}\n", dir.display()),
        )
        .unwrap();
        let watcher = start(&state);
        capture.wait_for("config successfully reloaded").await;
        assert_eq!(state.rt.config().port, 9001);
        watcher.abort();
        let _ = watcher.await;
        let text = capture.text();
        assert_eq!(text.matches("port: 0 -> 9001").count(), 1);
        assert_eq!(text.matches("config successfully reloaded").count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn unchanged_config_is_read_quietly_on_first_observation() {
        let (state, dir) = state();
        let capture = Capture::default();
        let _guard = capture.install();
        let initial = state.rt.config();
        let watcher = start(&state);
        // The first reload publishes a new snapshot. An auth file created after that
        // is logged by a later pass of the loop, so every line of the first pass
        // precedes its log line.
        tokio::time::timeout(Duration::from_secs(3), async {
            while Arc::ptr_eq(&state.rt.config(), &initial) {
                tokio::time::sleep(TICK).await;
            }
        })
        .await
        .unwrap();
        std::fs::write(dir.join("a.json"), "{}").unwrap();
        capture.wait_for("auth file changed (CREATE): a.json").await;
        watcher.abort();
        let _ = watcher.await;
        let text = capture.text();
        assert!(!text.contains("config successfully reloaded"), "{text}");
        assert!(!text.contains("config changes detected"), "{text}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A missing auth folder is watched through its parent; once it is created (empty,
    /// so nothing reloads), files written into it are still seen.
    #[tokio::test]
    async fn auth_folder_created_after_start_is_followed() {
        let (state, dir) = state();
        let auth = dir.join("later");
        std::fs::write(&state.path, format!("oauth: {{auth-dir: {}}}\n", auth.display())).unwrap();
        reload(&state).unwrap();
        let capture = Capture::default();
        let _guard = capture.install();
        let initial = state.rt.config();
        let watcher = start(&state);
        tokio::time::timeout(Duration::from_secs(3), async {
            while Arc::ptr_eq(&state.rt.config(), &initial) {
                tokio::time::sleep(TICK).await;
            }
        })
        .await
        .unwrap();
        std::fs::create_dir(&auth).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        std::fs::write(auth.join("a.json"), "{}").unwrap();
        capture.wait_for("auth file changed (CREATE): a.json").await;
        watcher.abort();
        let _ = watcher.await;
        std::fs::remove_dir_all(dir).unwrap();
    }

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

    /// A file whose bytes no plugin auth provider has parsed yet keeps the credential
    /// the store holds when the scan is published, not when it was read: a refresh that
    /// commits in between survives (the kept credential is taken under the store's
    /// reconciliation gate).
    #[tokio::test]
    async fn a_deferred_file_keeps_a_refresh_committed_before_publication() {
        use cpa_core::config::credentials::{ParseRequest, Parsed, ParsedAuth, PluginAuthParser};
        struct Acme;
        impl PluginAuthParser for Acme {
            fn parse_auths(&self, _: &ParseRequest<'_>) -> Parsed {
                Parsed::Handled(vec![ParsedAuth {
                    provider: "acme".into(),
                    id: "acme-a".into(),
                    metadata: serde_json::json!({"type": "acme", "token": "old"})
                        .as_object()
                        .cloned()
                        .unwrap(),
                    ..Default::default()
                }])
            }
        }
        // This thread only: tests sharing the process never see the parser.
        credentials::with_thread_plugin_auth_parser(Arc::new(Acme), || {
            let (state, dir) = state();
            let file = dir.join("acme.json");
            std::fs::write(&file, r#"{"type":"acme","v":1}"#).unwrap();
            reload(&state).unwrap();
            let store = state.rt.store().clone();
            assert_eq!(store.get("acme-a").unwrap().metadata["token"], "old");
            // Changed after the parse: the locked scan defers it.
            std::fs::write(&file, r#"{"type":"acme","v":2}"#).unwrap();
            let config = (*state.rt.config()).clone();
            let scan = credentials::scan_auth_dir(&config);
            assert!(scan.deferred.contains(&file));
            let refreshed = store.clone();
            crate::management::BEFORE_RECONCILE.lock().unwrap().push((
                config.auth_dir.clone(),
                Box::new(move || {
                    let current = refreshed.get("acme-a").unwrap();
                    let mut patch = cpa_core::credential::MetadataPatch::default();
                    patch.set.insert("token".into(), "refreshed".into());
                    refreshed.apply_patch("acme-a", current.revision, &patch).unwrap();
                }),
            ));
            state.publish(config, Some(scan));
            assert_eq!(store.get("acme-a").unwrap().metadata["token"], "refreshed");
            std::fs::remove_dir_all(dir).unwrap();
        });
    }

    /// A new auth-dir whose files plugins have not parsed yet publishes nothing for
    /// them, never the built-in reading: Go's watcher asks the plugin first. Here a
    /// Claude multi-auth bundle a plugin parses into two credentials; the reload that
    /// switches to its folder preparsed the old one, and the next reload parses it.
    #[tokio::test]
    async fn a_new_auth_dir_publishes_a_plugin_bundle_only_once_parsed() {
        use cpa_core::config::credentials::{ParseRequest, Parsed, ParsedAuth, PluginAuthParser};
        struct Bundles;
        impl PluginAuthParser for Bundles {
            fn parse_auths(&self, req: &ParseRequest<'_>) -> Parsed {
                if req.file_name != "bundle.json" {
                    return Parsed::NotHandled;
                }
                let auth = |id: &str| ParsedAuth {
                    provider: "claude".into(),
                    id: id.into(),
                    metadata: serde_json::json!({"type": "claude", "access_token": id})
                        .as_object()
                        .cloned()
                        .unwrap(),
                    ..Default::default()
                };
                Parsed::Handled(vec![auth("claude-a"), auth("claude-b")])
            }
        }
        // This thread only: tests sharing the process never see the parser.
        credentials::with_thread_plugin_auth_parser(Arc::new(Bundles), || {
            let (state, dir) = state();
            reload(&state).unwrap();
            let other = dir.join("other");
            std::fs::create_dir_all(&other).unwrap();
            std::fs::write(
                other.join("bundle.json"),
                r#"{"type":"claude","accounts":[{"access_token":"a"},{"access_token":"b"}]}"#,
            )
            .unwrap();
            std::fs::write(&state.path, format!("oauth: {{auth-dir: {}}}\n", other.display())).unwrap();
            reload(&state).unwrap();
            let ids = || -> Vec<String> {
                let mut ids: Vec<String> = state.rt.store().snapshot().iter().map(|c| c.id.clone()).collect();
                ids.sort();
                ids
            };
            assert!(ids().is_empty(), "published before it was parsed: {:?}", ids());
            reload(&state).unwrap();
            assert_eq!(ids(), ["claude-a", "claude-b"]);
            std::fs::remove_dir_all(dir).unwrap();
        });
    }
}
