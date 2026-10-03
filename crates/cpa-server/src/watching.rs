//! Hot publication through existing runtime/store contracts.
use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use cpa_core::config::{Config, credentials};
use cpa_core::credential::{Credential, Source};

use crate::management::Management;

#[derive(Clone, Default, PartialEq, Eq)]
struct Fingerprint {
    config: Vec<u8>,
    auth: BTreeMap<std::path::PathBuf, Vec<u8>>,
}

fn fingerprint(state: &Management) -> std::io::Result<Fingerprint> {
    let mut result = Fingerprint {
        config: std::fs::read(&state.path)?,
        auth: BTreeMap::new(),
    };
    let dir = state.rt.config().auth_dir.clone();
    // An unreadable auth dir is an empty set (as in reload); config edits must still
    // be noticed.
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(result);
    };
    for entry in entries {
        let path = entry?.path();
        // Go matches the extension case-insensitively.
        let json = path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().to_lowercase().ends_with(".json"));
        if path.is_file() && json {
            result.auth.insert(path.clone(), std::fs::read(path)?);
        }
    }
    Ok(result)
}

/// Go `persistConfigAsync` / `persistAuthAsync`: what changed since the last
/// persisted snapshot goes to the remote store, in the background. Config changes count
/// only once a reload accepted them; auth files that are not JSON objects are skipped,
/// as Go skips files its synthesizer rejects.
fn persist_changes(state: &Management, baseline: &mut Fingerprint, current: &Fingerprint, config_ok: bool) {
    let Some(store) = state.store.clone() else {
        return;
    };
    let mut auth = Vec::new();
    for (path, data) in &current.auth {
        if baseline.auth.get(path) != Some(data)
            && serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(data).is_ok()
        {
            auth.push(("Sync", path.clone()));
        }
    }
    for path in baseline.auth.keys() {
        if !current.auth.contains_key(path) {
            auth.push(("Remove", path.clone()));
        }
    }
    let config = config_ok && baseline.config != current.config;
    baseline.auth.clone_from(&current.auth);
    if config_ok {
        baseline.config.clone_from(&current.config);
    }
    if !config && auth.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let bound = Duration::from_secs(30);
        if config {
            match tokio::time::timeout(bound, store.persist_config()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!("failed to persist config change: {error}"),
                Err(_) => tracing::error!("failed to persist config change: context deadline exceeded"),
            }
        }
        for (verb, path) in auth {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let message = format!("{verb} auth {name}");
            match tokio::time::timeout(bound, store.persist_auth_files(message, vec![path])).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!("failed to persist auth changes: {error}"),
                Err(_) => tracing::error!("failed to persist auth changes: context deadline exceeded"),
            }
        }
    });
}

pub fn start(state: &Arc<Management>) -> tokio::task::JoinHandle<()> {
    let state = Arc::downgrade(state);
    tokio::spawn(async move {
        // The first snapshot is the store's own mirror: nothing to persist yet.
        let mut persisted: Option<Fingerprint> = None;
        let mut previous = None;
        let mut observed = None;
        let mut since = Instant::now();
        // ponytail: portable content polling rather than platform fsnotify. This scans
        // top-level auth JSON every 50ms; replace with notify for very large auth dirs.
        // Content identity ignores duplicate/self-write events and atomic replacements.
        let mut ticks = tokio::time::interval(Duration::from_millis(50));
        loop {
            ticks.tick().await;
            let Some(state) = state.upgrade() else {
                return;
            };
            let snapshot = tokio::task::spawn_blocking({
                let state = state.clone();
                move || {
                    let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
                    fingerprint(&state)
                }
            })
            .await;
            let Ok(Ok(snapshot)) = snapshot else {
                continue;
            };
            if observed.as_ref() != Some(&snapshot) {
                observed = Some(snapshot);
                since = Instant::now();
                continue;
            }
            if since.elapsed() < Duration::from_millis(150) || observed == previous {
                continue;
            }
            let loaded = tokio::task::spawn_blocking({
                let state = state.clone();
                move || reload(&state)
            })
            .await;
            let ok = matches!(loaded, Ok(Ok(())));
            if let Some(current) = &observed {
                match &mut persisted {
                    None => persisted = Some(current.clone()),
                    Some(baseline) => persist_changes(&state, baseline, current, ok),
                }
            }
            if ok {
                previous = observed.take();
            }
        }
    })
}

/// Blocking reload. Shared with deterministic tests; no provider calls are made.
pub fn reload(state: &Management) -> anyhow::Result<()> {
    let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
    // A rejected config must not prevent disabled/deleted auth files from taking
    // effect. Reconcile against the last good config while retaining the error.
    let (mut config, config_error) = if std::fs::metadata(&state.path)?.len() == 0 {
        ((*state.rt.config()).clone(), None)
    } else {
        match Config::load(&state.path) {
            Ok(config) => (config, None),
            Err(error) => ((*state.rt.config()).clone(), Some(error)),
        }
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
    if let Some(error) = config_error {
        return Err(error);
    }
    Ok(())
}
