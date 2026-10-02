//! Hot publication through existing runtime/store contracts.
use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use cpa_core::config::{Config, credentials};
use cpa_core::credential::{Credential, Source};

use crate::management::Management;

#[derive(Default, PartialEq, Eq)]
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
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(result),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "json") {
            result.auth.insert(path.clone(), std::fs::read(path)?);
        }
    }
    Ok(result)
}

pub fn start(state: &Arc<Management>) -> tokio::task::JoinHandle<()> {
    let state = Arc::downgrade(state);
    tokio::spawn(async move {
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
            if matches!(loaded, Ok(Ok(()))) {
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
    let (config, config_error) = if std::fs::metadata(&state.path)?.len() == 0 {
        ((*state.rt.config()).clone(), None)
    } else {
        match Config::load(&state.path) {
            Ok(config) => (config, None),
            Err(error) => ((*state.rt.config()).clone(), Some(error)),
        }
    };
    let mut files = credentials::from_auth_dir(&config)?;
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
    state.publish(config, Some(files))?;
    if let Some(error) = config_error {
        return Err(error);
    }
    Ok(())
}
