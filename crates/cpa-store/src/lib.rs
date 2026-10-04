//! Remote homes for `config.yaml` and the auth files, chosen by `PGSTORE_*`,
//! `OBJECTSTORE_*` and `GITSTORE_*` (Go cmd/server/main.go and internal/store). Each
//! store mirrors its content into a local workspace; the server runs on those files
//! and reports changes back through [`cpa_server::persist::StorePersister`].

// Used only by the object-store tests, which assert Unix modes.
#[cfg(all(test, unix))]
mod fake_s3;
mod git;
mod object;
mod pgconn;
mod postgres;
mod private_fs;
mod sigv4;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub use git::{GitPersister, GitStore};
pub use object::{ObjectConfig, ObjectPersister, ObjectStore};
pub use postgres::{PgCooldown, PostgresConfig, PostgresPersister, PostgresStore};

use cpa_server::cooldown_store::Backend as CooldownBackend;
use cpa_server::persist::StorePersister;

/// Go main's 30-second contexts around store setup.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);

/// The store the environment asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    Postgres {
        dsn: String,
        schema: String,
        /// `<local path>/pgstore`.
        spool: PathBuf,
    },
    Object {
        endpoint: String,
        access_key: String,
        secret_key: String,
        bucket: String,
        /// `<local path>/objectstore`.
        root: PathBuf,
    },
    Git {
        url: String,
        username: String,
        token: String,
        branch: String,
        /// `<local path>/gitstore`.
        root: PathBuf,
    },
}

/// Go `filepath.Clean`, lexically.
pub(crate) fn clean(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) => {}
                _ => out.push(component),
            },
            other => out.push(other),
        }
    }
    if out.is_empty() {
        return PathBuf::from(".");
    }
    out.iter().collect()
}

/// Go `filepath.Abs`: joined with the working directory, then cleaned.
pub(crate) fn go_abs(path: &Path) -> std::io::Result<PathBuf> {
    std::path::absolute(path).map(|p| clean(&p))
}

/// Go main's `lookupEnv`: the first key holding a non-blank value, trimmed.
fn lookup(env: &dyn Fn(&str) -> Option<String>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| env(key))
        .map(|value| value.trim().to_owned())
        .find(|value| !value.is_empty())
}

/// Go `util.WritablePath`.
fn writable_path(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    lookup(env, &["WRITABLE_PATH", "writable_path"]).map(PathBuf::from)
}

/// Go main's store choice: a Home JWT disables every store; otherwise Postgres, then
/// the object store, then git. `wd` is the working directory.
pub fn select(env: &dyn Fn(&str) -> Option<String>, wd: &Path, home: bool) -> Option<Selection> {
    if home {
        return None;
    }
    let get = |upper: &str| lookup(env, &[upper, &upper.to_lowercase()]);
    let local = |key: &str| {
        get(key)
            .map(PathBuf::from)
            .or_else(|| writable_path(env))
            .unwrap_or_else(|| wd.to_path_buf())
    };
    if let Some(dsn) = get("PGSTORE_DSN") {
        return Some(Selection::Postgres {
            dsn,
            schema: get("PGSTORE_SCHEMA").unwrap_or_default(),
            spool: local("PGSTORE_LOCAL_PATH").join("pgstore"),
        });
    }
    if let Some(endpoint) = get("OBJECTSTORE_ENDPOINT") {
        return Some(Selection::Object {
            endpoint,
            access_key: get("OBJECTSTORE_ACCESS_KEY").unwrap_or_default(),
            secret_key: get("OBJECTSTORE_SECRET_KEY").unwrap_or_default(),
            bucket: get("OBJECTSTORE_BUCKET").unwrap_or_default(),
            root: local("OBJECTSTORE_LOCAL_PATH").join("objectstore"),
        });
    }
    get("GITSTORE_GIT_URL").map(|url| Selection::Git {
        url,
        username: get("GITSTORE_GIT_USERNAME").unwrap_or_default(),
        token: get("GITSTORE_GIT_TOKEN").unwrap_or_default(),
        branch: get("GITSTORE_GIT_BRANCH").unwrap_or_default(),
        root: local("GITSTORE_LOCAL_PATH").join("gitstore"),
    })
}

/// A store ready to serve: its config file, its auth mirror and the hooks the server
/// needs.
pub struct Store {
    pub config_path: PathBuf,
    pub auth_dir: PathBuf,
    pub persister: Arc<dyn StorePersister>,
    /// Postgres keeps cooldown state in its own table.
    pub cooldown: Option<Arc<dyn CooldownBackend>>,
    /// Go's info line once the config loaded.
    pub enabled: String,
}

async fn bounded<T>(work: impl Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    tokio::time::timeout(SETUP_TIMEOUT, work)
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("context deadline exceeded")))
}

/// Go main's store setup. The error is the line Go logs before exiting.
pub async fn bootstrap(selection: Selection, wd: &Path) -> Result<Store, String> {
    let example = wd.join("config.example.yaml");
    match selection {
        Selection::Postgres { dsn, schema, spool } => {
            let cfg = PostgresConfig {
                dsn,
                schema,
                spool_dir: spool,
            };
            let store = PostgresStore::connect(cfg)
                .await
                .map_err(|e| format!("failed to initialize postgres token store: {e:#}"))?;
            bounded(store.bootstrap(&example))
                .await
                .map_err(|e| format!("failed to bootstrap postgres-backed config: {e:#}"))?;
            let store = Arc::new(store);
            Ok(Store {
                config_path: store.config_path(),
                auth_dir: store.auth_dir(),
                cooldown: Some(Arc::new(store.cooldown_backend())),
                enabled: format!(
                    "postgres-backed token store enabled, workspace path: {}",
                    store.work_dir().display()
                ),
                persister: Arc::new(PostgresPersister(store)),
            })
        }
        Selection::Object {
            endpoint,
            access_key,
            secret_key,
            bucket,
            root,
        } => {
            let (resolved, use_ssl) = ObjectConfig::endpoint_from(&endpoint).map_err(|e| e.to_string())?;
            let cfg = ObjectConfig {
                endpoint: resolved,
                bucket: bucket.clone(),
                access_key,
                secret_key,
                local_root: root,
                use_ssl,
            };
            let store = ObjectStore::new(cfg).map_err(|e| format!("failed to initialize object token store: {e:#}"))?;
            bounded(store.bootstrap(&example))
                .await
                .map_err(|e| format!("failed to bootstrap object-backed config: {e:#}"))?;
            let store = Arc::new(store);
            Ok(Store {
                config_path: store.config_path(),
                auth_dir: store.auth_dir(),
                cooldown: None,
                enabled: format!("object-backed token store enabled, bucket: {bucket}"),
                persister: Arc::new(ObjectPersister(store)),
            })
        }
        Selection::Git {
            url,
            username,
            token,
            branch,
            root,
        } => {
            let store = Arc::new(GitStore::new(&url, &username, &token, &branch, &root));
            let config_path = store.config_path();
            let prepared = store.clone();
            git::blocking(move || prepared.ensure_repository())
                .await
                .map_err(|e| format!("failed to prepare git token store: {e:#}"))?;
            match std::fs::metadata(&config_path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::metadata(&example)
                        .map_err(|e| format!("failed to find template config file: stat {}: {e}", example.display()))?;
                    object::seed_config(&example, &config_path)
                        .map_err(|e| format!("failed to bootstrap git-backed config: {e:#}"))?;
                    let committing = store.clone();
                    git::blocking(move || committing.persist_config())
                        .await
                        .map_err(|e| format!("failed to commit initial git-backed config: {e:#}"))?;
                    tracing::info!("git-backed config initialized from template: {}", config_path.display());
                }
                Err(e) => return Err(format!("failed to inspect git-backed config: {e}")),
                Ok(_) => {}
            }
            Ok(Store {
                config_path,
                auth_dir: store.auth_dir(),
                cooldown: None,
                enabled: format!("git-backed token store enabled, repository path: {}", root.display()),
                persister: Arc::new(GitPersister(store)),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn selection_follows_go_main() {
        let wd = Path::new("/srv/app");
        // Postgres wins over the others; blank values do not count; lower-case keys work.
        let env = env_of(&[
            ("PGSTORE_DSN", "   "),
            ("pgstore_dsn", " postgres://db/x "),
            ("OBJECTSTORE_ENDPOINT", "http://s3"),
            ("GITSTORE_GIT_URL", "https://git/x.git"),
            ("WRITABLE_PATH", "/data"),
        ]);
        assert_eq!(
            select(&env, wd, false),
            Some(Selection::Postgres {
                dsn: "postgres://db/x".into(),
                schema: String::new(),
                spool: "/data/pgstore".into(),
            })
        );
        // A Home JWT disables every store.
        assert_eq!(select(&env, wd, true), None);
        let env = env_of(&[
            ("OBJECTSTORE_ENDPOINT", "http://s3"),
            ("OBJECTSTORE_BUCKET", "b"),
            ("OBJECTSTORE_LOCAL_PATH", "/obj"),
            ("GITSTORE_GIT_URL", "https://git/x.git"),
        ]);
        assert!(
            matches!(select(&env, wd, false), Some(Selection::Object { root, bucket, .. }) if root == Path::new("/obj/objectstore") && bucket == "b")
        );
        let env = env_of(&[
            ("GITSTORE_GIT_URL", "https://git/x.git"),
            ("gitstore_git_branch", "main"),
        ]);
        assert_eq!(
            select(&env, wd, false),
            Some(Selection::Git {
                url: "https://git/x.git".into(),
                username: String::new(),
                token: String::new(),
                branch: "main".into(),
                root: "/srv/app/gitstore".into(),
            })
        );
        assert_eq!(select(&env_of(&[]), wd, false), None);
    }
}
