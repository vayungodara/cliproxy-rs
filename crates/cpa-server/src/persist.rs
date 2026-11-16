//! Remote persistence for config and auth files (Go's `storePersister` and the token
//! store's `Delete`). The Postgres, git and object stores mirror their content into a
//! local workspace; the server keeps working on those files and reports each change
//! here so the store can push it.

use std::path::PathBuf;

use futures_util::future::BoxFuture;

/// Whether `path` still holds `expected`, or is gone. Any other read error counts as
/// changed, so the file is kept.
pub fn file_unchanged(path: &std::path::Path, expected: &[u8]) -> bool {
    match std::fs::read(path) {
        Ok(now) => now == expected,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Writes a credential for [`StorePersister::save_auth`] and returns its path.
pub type SaveFn<'a> = Box<dyn FnOnce() -> anyhow::Result<Option<PathBuf>> + 'a>;

pub trait StorePersister: Send + Sync + 'static {
    /// Go `PersistConfig`: the config file changed (or was removed).
    fn persist_config(&self) -> BoxFuture<'_, anyhow::Result<()>>;
    /// Go `PersistAuthFiles`. `message` is `Sync auth <name>` or `Remove auth <name>`.
    fn persist_auth_files(&self, message: String, paths: Vec<PathBuf>) -> BoxFuture<'_, anyhow::Result<()>>;
    /// Go `Store.Delete`: an explicit removal (management), unlike a file event.
    /// Blocking: called on a Tokio blocking thread (inside the runtime) that holds the
    /// management disk lock, so it must not wait for another blocking-pool task.
    fn delete_auth(&self, path: PathBuf) -> anyhow::Result<()>;
    /// [`Self::delete_auth`] only while the file still holds `expected` or is gone, for
    /// a file checked earlier (a legacy credential a login replaces). Returns whether it
    /// deleted. This default compares the mirrored file and then deletes; the git store
    /// compares after its pull, under its lock, so a replacement from another instance
    /// is kept.
    fn delete_auth_unchanged(&self, path: PathBuf, expected: &[u8]) -> anyhow::Result<bool> {
        if !file_unchanged(&path, expected) {
            return Ok(false);
        }
        self.delete_auth(path).map(|()| true)
    }
    /// Go `Store.Save`: writes one credential with `write` (`None`: nothing written) and
    /// returns once the store has it. Blocking, like `delete_auth`. This default writes
    /// and then pushes the file; the git store pulls first and holds its lock across
    /// the write and the commit.
    fn save_auth(&self, write: SaveFn<'_>) -> anyhow::Result<Option<PathBuf>> {
        let Some(path) = write()? else {
            return Ok(None);
        };
        let path = std::path::absolute(&path).unwrap_or(path);
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let message = format!("Update auth {name}");
        tokio::runtime::Handle::current().block_on(self.persist_auth_files(message, vec![path.clone()]))?;
        Ok(Some(path))
    }
    /// The mirrored auth directory; every config load uses it instead of `auth-dir`.
    fn auth_dir(&self) -> PathBuf;
}
