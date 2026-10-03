//! Remote persistence for config and auth files (Go's `storePersister` and the token
//! store's `Delete`). The Postgres, git and object stores mirror their content into a
//! local workspace; the server keeps working on those files and reports each change
//! here so the store can push it.

use std::path::PathBuf;

use futures_util::future::BoxFuture;

pub trait StorePersister: Send + Sync + 'static {
    /// Go `PersistConfig`: the config file changed (or was removed).
    fn persist_config(&self) -> BoxFuture<'_, anyhow::Result<()>>;
    /// Go `PersistAuthFiles`. `message` is `Sync auth <name>` or `Remove auth <name>`.
    fn persist_auth_files(&self, message: String, paths: Vec<PathBuf>) -> BoxFuture<'_, anyhow::Result<()>>;
    /// Go `Store.Delete`: an explicit removal (management), unlike a file event.
    /// Blocking: called on a Tokio blocking thread (inside the runtime) that holds the
    /// management disk lock, so it must not wait for another blocking-pool task.
    fn delete_auth(&self, path: PathBuf) -> anyhow::Result<()>;
    /// The mirrored auth directory; every config load uses it instead of `auth-dir`.
    fn auth_dir(&self) -> PathBuf;
}
