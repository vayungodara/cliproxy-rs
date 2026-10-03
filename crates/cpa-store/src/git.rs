//! `GITSTORE_*`: config and auth files in a git repository (Go
//! internal/store/gitstore.go). The working tree under `<root>` holds `config/` and
//! `auths/`; every persisted change becomes the branch's single, parentless commit
//! (history is squashed on purpose: it holds secrets) and is pushed with
//! force-with-lease, so concurrent writers never silently overwrite each other.
//!
//! ponytail: drives the `git` executable instead of an embedded git library; the
//! container needs `git` installed. go-git's corruption recovery (re-clone while
//! preserving local edits) is not ported: a corrupt repository is reported and the
//! operator removes `<root>` to re-clone.

use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use futures_util::future::BoxFuture;

const GC_INTERVAL: Duration = Duration::from_secs(5 * 60);
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const AUTHOR_NAME: &str = "CLIProxyAPI";
const AUTHOR_EMAIL: &str = "cliproxy@local";

pub struct GitStore {
    remote: String,
    branch: String,
    username: String,
    password: String,
    repo: PathBuf,
    /// Serializes repository work (Go `GitTokenStore.mu`).
    lock: Mutex<Option<Instant>>,
}

fn mkdir_0700(path: &Path) -> Result<()> {
    crate::private_fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))
}

fn write_0600(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        mkdir_0700(parent)?;
    }
    let mut file = crate::private_fs::create_truncate(path).with_context(|| format!("write {}", path.display()))?;
    file.write_all(bytes)?;
    crate::private_fs::restrict(path)?;
    Ok(())
}

/// Go `normalizeManagedPaths`: repository-relative, slash-separated, de-duplicated.
fn normalize(paths: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for path in paths {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = Vec::new();
        for component in Path::new(trimmed).components() {
            match component {
                Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    bail!("path {path:?} is not a repository-relative file")
                }
            }
        }
        if parts.is_empty() {
            bail!("path {path:?} is not a repository-relative file");
        }
        let clean = parts.join("/");
        if !out.contains(&clean) {
            out.push(clean);
        }
    }
    Ok(out)
}

/// Go `overlappingDirtyPath`.
fn overlaps(a: &str, b: &str) -> bool {
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
}

impl GitStore {
    /// Go `NewGitTokenStore` + `SetBaseDir(<root>/auths)`.
    pub fn new(remote: &str, username: &str, password: &str, branch: &str, root: &Path) -> Self {
        let repo = crate::go_abs(root).unwrap_or_else(|_| root.to_path_buf());
        Self {
            remote: remote.to_owned(),
            branch: branch.trim().to_owned(),
            username: username.to_owned(),
            password: password.to_owned(),
            repo,
            lock: Mutex::new(None),
        }
    }

    pub fn auth_dir(&self) -> PathBuf {
        self.repo.join("auths")
    }

    pub fn config_path(&self) -> PathBuf {
        self.repo.join("config").join("config.yaml")
    }

    /// Never echoes the token: git error text can contain the remote URL.
    fn redact(&self, text: &str) -> String {
        let mut out = text.to_owned();
        if !self.password.is_empty() {
            out = out.replace(&self.password, "***");
        }
        if let Ok(url) = url::Url::parse(&self.remote)
            && let Some(password) = url.password()
            && !password.is_empty()
        {
            out = out.replace(password, "***");
        }
        out
    }

    fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
            .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
            .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
            .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL);
        // Go `gitClientOptions`: HTTP basic auth, user "git" when only a token is set.
        // Passed through the environment so it never appears in a process listing.
        if !self.username.is_empty() || !self.password.is_empty() {
            let user = if self.username.is_empty() {
                "git"
            } else {
                &self.username
            };
            let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{}", self.password));
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.extraHeader")
                .env("GIT_CONFIG_VALUE_0", format!("Authorization: Basic {token}"));
        }
        cmd
    }

    fn run_in(&self, dir: &Path, args: &[&str]) -> Result<Output> {
        let output = self
            .command(dir, args)
            .output()
            .map_err(|e| anyhow!("git token store: run git: {e} (is git installed?)"))?;
        Ok(output)
    }

    /// Runs git in the repository; a non-zero exit is an error with git's message.
    fn git(&self, args: &[&str]) -> Result<Vec<u8>> {
        let output = self.run_in(&self.repo, args)?;
        if !output.status.success() {
            bail!(
                "git {}: {}",
                args.first().unwrap_or(&""),
                self.redact(String::from_utf8_lossy(&output.stderr).trim())
            );
        }
        Ok(output.stdout)
    }

    /// Runs git and reports only whether it succeeded.
    fn git_ok(&self, args: &[&str]) -> Result<bool> {
        Ok(self.run_in(&self.repo, args)?.status.success())
    }

    fn rev(&self, name: &str) -> Result<Option<String>> {
        let output = self.run_in(
            &self.repo,
            &["rev-parse", "--verify", "-q", &format!("{name}^{{commit}}")],
        )?;
        Ok(output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()))
    }

    fn current_branch(&self) -> Result<Option<String>> {
        let output = self.run_in(&self.repo, &["symbolic-ref", "-q", "HEAD"])?;
        Ok(output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()))
    }

    fn blob(&self, rev: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let output = self.run_in(&self.repo, &["cat-file", "blob", &format!("{rev}:{path}")])?;
        Ok(output.status.success().then_some(output.stdout))
    }

    /// Go `EnsureRepository`.
    pub fn ensure_repository(&self) -> Result<()> {
        let mut last_gc = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.ensure_locked(&mut last_gc)
    }

    fn ensure_locked(&self, last_gc: &mut Option<Instant>) -> Result<()> {
        if self.remote.is_empty() {
            bail!("git token store: remote not configured");
        }
        mkdir_0700(&self.repo).context("git token store: create repo dir")?;
        let mut init_paths = Vec::new();
        if !self.repo.join(".git").exists() {
            let listed = self
                .run_in(&self.repo, &["ls-remote", "--", &self.remote])
                .context("git token store: clone remote")?;
            if !listed.status.success() {
                bail!(
                    "git token store: clone remote: {}",
                    self.redact(String::from_utf8_lossy(&listed.stderr).trim())
                );
            }
            if listed.stdout.is_empty() {
                // An empty remote: initialize it (Go's ErrEmptyRemoteRepository path).
                self.git(&["init", "-q"]).context("git token store: init empty repo")?;
                if !self.branch.is_empty() {
                    self.git(&["symbolic-ref", "HEAD", &format!("refs/heads/{}", self.branch)])
                        .with_context(|| format!("git token store: set head to branch {}", self.branch))?;
                }
                self.git(&["remote", "add", "origin", &self.remote])
                    .context("git token store: configure remote")?;
                for dir in ["auths", "config"] {
                    let keep = self.repo.join(dir).join(".gitkeep");
                    if !keep.exists() {
                        write_0600(&keep, b"")?;
                    }
                    init_paths.push(format!("{dir}/.gitkeep"));
                }
            } else {
                let mut args = vec!["clone", "-q"];
                if !self.branch.is_empty() {
                    args.extend(["--branch", &self.branch]);
                }
                args.extend(["--", &self.remote, "."]);
                self.git(&args).context("git token store: clone remote")?;
                self.tighten_permissions()?;
            }
        } else {
            self.checkout_branch()?;
            self.pull()?;
        }
        self.git(&["config", "commit.gpgsign", "false"])
            .context("git token store: disable commit signing")?;
        mkdir_0700(&self.auth_dir()).context("git token store: create auth dir")?;
        mkdir_0700(&self.repo.join("config")).context("git token store: create config dir")?;
        if !init_paths.is_empty() {
            self.commit_and_push(last_gc, "Initialize git token store", &init_paths, true)?;
        }
        Ok(())
    }

    /// Clone writes files with the umask; secrets get 0600 like Go's own writes.
    fn tighten_permissions(&self) -> Result<()> {
        let listed = self.git(&["ls-files", "-z"])?;
        for path in split_nul(&listed) {
            let full = self.repo.join(&path);
            if full.is_file() {
                crate::private_fs::restrict(&full)?;
            }
        }
        Ok(())
    }

    /// Go `checkoutConfiguredBranch` / `checkoutRemoteDefaultBranch`.
    fn checkout_branch(&self) -> Result<()> {
        let target = if self.branch.is_empty() {
            match self.remote_default_branch() {
                Ok(branch) => branch,
                // Go keeps the current branch when the remote is unreachable or empty.
                Err(_) if self.rev("HEAD")?.is_some() => return Ok(()),
                Err(error) => return Err(error.context("git token store: checkout remote default")),
            }
        } else {
            self.branch.clone()
        };
        let reference = format!("refs/heads/{target}");
        if self.current_branch()?.as_deref() == Some(reference.as_str()) {
            return Ok(());
        }
        if self.rev(&reference)?.is_some() {
            self.git(&["checkout", "-q", &target])
                .with_context(|| format!("git token store: checkout branch {target}"))?;
            return Ok(());
        }
        let tracking = format!("refs/remotes/origin/{target}");
        if self.rev(&tracking)?.is_none() {
            self.git(&["fetch", "-q", "origin"])
                .with_context(|| format!("git token store: checkout branch {target}: sync remote refs"))?;
        }
        self.git(&["checkout", "-q", "-b", &target, "--track", &format!("origin/{target}")])
            .with_context(|| format!("git token store: checkout branch {target}"))?;
        Ok(())
    }

    /// Go `resolveRemoteDefaultBranch`: origin's HEAD target.
    fn remote_default_branch(&self) -> Result<String> {
        let listed = self.git(&["ls-remote", "--symref", "origin", "HEAD"])?;
        let text = String::from_utf8_lossy(&listed);
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("ref: refs/heads/")
                && let Some((branch, _)) = rest.split_once('\t')
            {
                return Ok(branch.to_owned());
            }
        }
        // Fall back to the local origin/HEAD, then any remote branch.
        if let Ok(local) = self.git(&["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])
            && let Some(branch) = String::from_utf8_lossy(&local)
                .trim()
                .strip_prefix("refs/remotes/origin/")
        {
            return Ok(branch.to_owned());
        }
        bail!("resolve remote default: remote default branch not found")
    }

    /// Go's pull plus `reconcileRemoteWorktree`: take the remote branch, keeping local
    /// changes to paths the remote did not touch, then restore tracked files that went
    /// missing from the working tree.
    fn pull(&self) -> Result<()> {
        let fetched = self.run_in(&self.repo, &["fetch", "-q", "origin"])?;
        if !fetched.status.success() {
            let message = String::from_utf8_lossy(&fetched.stderr).to_lowercase();
            // Go ignores authentication prompts and empty remotes during this sync.
            let benign = message.contains("authentication failed")
                || message.contains("could not read username")
                || message.contains("terminal prompts disabled");
            if !benign {
                bail!(
                    "git token store: pull: {}",
                    self.redact(String::from_utf8_lossy(&fetched.stderr).trim())
                );
            }
        }
        let Some(branch) = self.current_branch()? else {
            bail!("git token store: reconcile pull without a local branch");
        };
        let short = branch.trim_start_matches("refs/heads/").to_owned();
        let Some(remote) = self.rev(&format!("refs/remotes/origin/{short}"))? else {
            // Go: an empty remote is ignored; a configured branch missing from a
            // non-empty remote is a pull error; following the remote default, ignored.
            let remote_empty = self
                .git(&["for-each-ref", "--count=1", "refs/remotes/origin/"])?
                .is_empty();
            if !self.branch.is_empty() && !remote_empty {
                bail!("git token store: pull: reference not found");
            }
            return self.restore_missing();
        };
        let base = self.rev("HEAD")?;
        if base.as_deref() == Some(remote.as_str()) {
            self.git(&["reset", "-q"])
                .context("git token store: repair index after up-to-date pull")?;
        } else {
            self.reconcile(base.as_deref(), &remote, &branch)?;
        }
        self.restore_missing()
    }

    fn reconcile(&self, base: Option<&str>, remote: &str, branch: &str) -> Result<()> {
        let dirty = self.dirty_paths()?;
        let changed = split_nul(&self.git(&[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            base.unwrap_or(EMPTY_TREE),
            remote,
        ])?);
        for path in &changed {
            if let Some(local) = dirty.iter().find(|d| overlaps(path, d)) {
                bail!(
                    "git token store: reconcile remote changes: remote path {path} conflicts with local change {local}"
                );
            }
        }
        for path in &changed {
            let destination = self.repo.join(path);
            match self.blob(remote, path)? {
                Some(contents) => write_0600(&destination, &contents)?,
                None => match std::fs::remove_file(&destination) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(anyhow!("git token store: remove {path}: {e}")),
                },
            }
        }
        self.git(&["update-ref", branch, remote])
            .with_context(|| format!("git token store: update branch {branch}"))?;
        self.git(&["reset", "-q", remote])
            .context("git token store: reset index to remote branch")?;
        Ok(())
    }

    /// Go `worktreeDirtyPaths`: staged, modified or untracked paths.
    fn dirty_paths(&self) -> Result<Vec<String>> {
        let status = self.git(&[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ])?;
        Ok(split_nul(&status)
            .into_iter()
            .filter_map(|entry| entry.get(3..).map(str::to_owned))
            .collect())
    }

    /// Go `restoreMissingTrackedFiles`.
    fn restore_missing(&self) -> Result<()> {
        if self.rev("HEAD")?.is_none() {
            return Ok(());
        }
        let tracked = split_nul(&self.git(&["ls-tree", "-r", "-z", "--name-only", "HEAD"])?);
        for path in tracked {
            let destination = self.repo.join(&path);
            if destination.symlink_metadata().is_ok() {
                continue;
            }
            if let Some(contents) = self.blob("HEAD", &path)? {
                write_0600(&destination, &contents).context("git token store: restore tracked worktree files")?;
            }
        }
        Ok(())
    }

    /// Go `commitAndPushWithOptionsLocked`: one squashed commit of `paths`, pushed with
    /// force-with-lease; the branch is restored when the push is rejected.
    fn commit_and_push(
        &self,
        last_gc: &mut Option<Instant>,
        message: &str,
        paths: &[String],
        allow_missing_remote: bool,
    ) -> Result<()> {
        let managed = normalize(paths).context("git token store: validate commit paths")?;
        if managed.is_empty() {
            return Ok(());
        }
        let base = self.rev("HEAD")?;
        if base.is_some() {
            self.git(&["reset", "-q"])
                .context("git token store: reset index before commit")?;
        }
        let mut added = false;
        for path in &managed {
            let output = self.run_in(&self.repo, &["add", "-A", "--", path])?;
            if output.status.success() {
                added = true;
                continue;
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("did not match any files") {
                continue;
            }
            bail!("git token store: add {path}: {}", self.redact(stderr.trim()));
        }
        if !added || self.git_ok(&["diff", "--cached", "--quiet"])? {
            return Ok(());
        }
        let message = if message.trim().is_empty() {
            "Update auth store"
        } else {
            message
        };
        let tree = String::from_utf8_lossy(&self.git(&["write-tree"])?).trim().to_owned();
        if let Some(base) = &base {
            // Go `validateManagedTreeChanges`.
            let changed = split_nul(&self.git(&["diff", "--name-only", "--no-renames", "-z", base, &tree])?);
            if let Some(path) = changed
                .iter()
                .find(|p| !managed.iter().any(|m| p == &m || p.starts_with(&format!("{m}/"))))
            {
                self.git(&["reset", "-q"])?;
                bail!(
                    "git token store: validate commit tree: unexpected indexed change outside requested paths: {path}"
                );
            }
        }
        // Go commits, then rewrites the tip as a parentless commit of the same tree.
        let commit = String::from_utf8_lossy(&self.git(&["commit-tree", &tree, "-m", message])?)
            .trim()
            .to_owned();
        let Some(branch) = self.current_branch()? else {
            bail!("git token store: head is not a branch");
        };
        self.git(&["update-ref", &branch, &commit])
            .context("git token store: update branch reference")?;
        let restore = |error: anyhow::Error| -> anyhow::Error {
            let Some(base) = &base else { return error };
            let restored = self
                .git(&["update-ref", &branch, base])
                .and_then(|_| self.git(&["reset", "-q", base]));
            match restored {
                Ok(_) => error,
                Err(again) => error.context(format!("git token store: restore head after rejected push: {again}")),
            }
        };
        let short = branch.trim_start_matches("refs/heads/");
        let tracking = format!("refs/remotes/origin/{short}");
        let refspec = format!("{branch}:{branch}");
        let lease = match self.rev(&tracking)? {
            Some(hash) => Some(format!("--force-with-lease={branch}:{hash}")),
            None if allow_missing_remote => None,
            None => {
                return Err(restore(anyhow!(
                    "git token store: remote tracking branch {tracking} not found"
                )));
            }
        };
        let mut args = vec!["push", "-q", "--porcelain"];
        if let Some(lease) = &lease {
            args.push(lease);
        }
        args.extend(["origin", &refspec]);
        if let Err(error) = self.git(&args) {
            return Err(restore(anyhow!("git token store: push: {error}")));
        }
        self.git(&["update-ref", &tracking, &commit])
            .with_context(|| format!("git token store: update remote tracking branch {tracking}"))?;
        if last_gc.is_none_or(|t| t.elapsed() >= GC_INTERVAL) {
            *last_gc = Some(Instant::now());
            let _ = self.git(&["gc", "--quiet", "--prune=24.hours.ago"]);
        }
        Ok(())
    }

    fn relative(&self, path: &Path) -> Result<String> {
        let path = crate::go_abs(path).unwrap_or_else(|_| path.to_path_buf());
        let rel = path
            .strip_prefix(&self.repo)
            .map_err(|_| anyhow!("git token store: path outside repository"))?;
        Ok(rel.to_string_lossy().replace('\\', "/"))
    }

    /// Go `guardWatcherAuthRemovalLocked`: a file event must not delete tracked auth
    /// from the repository; only an explicit delete may. `Some(())` when handled.
    fn guard_removal(&self, message: &str, rels: &[String]) -> Result<Option<()>> {
        if !message.trim().starts_with("Remove auth ") {
            return Ok(None);
        }
        if self.rev("HEAD")?.is_none() {
            return Ok(Some(()));
        }
        let mut existing = false;
        for rel in normalize(rels)? {
            if self.repo.join(&rel).exists() {
                existing = true;
                continue;
            }
            if self.blob("HEAD", &rel)?.is_some() {
                bail!(
                    "git token store: refusing watcher-originated removal of tracked auth {rel}; use an explicit delete"
                );
            }
        }
        // Without a surviving path the explicit delete already committed the removal.
        Ok((!existing).then_some(()))
    }

    /// Go `PersistAuthFiles`.
    pub fn persist_auth_files(&self, message: &str, paths: &[PathBuf]) -> Result<()> {
        let mut last_gc = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let rels = paths
            .iter()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| self.relative(p))
            .collect::<Result<Vec<_>>>()?;
        if rels.is_empty() {
            return Ok(());
        }
        let message = if message.trim().is_empty() {
            "Sync watcher updates"
        } else {
            message
        };
        // Inspect removals before the sync restores missing tracked files.
        if self.repo.join(".git").exists() && self.guard_removal(message, &rels)?.is_some() {
            return Ok(());
        }
        self.ensure_locked(&mut last_gc)?;
        if self.guard_removal(message, &rels)?.is_some() {
            return Ok(());
        }
        self.commit_and_push(&mut last_gc, message, &rels, false)
    }

    /// Go `Delete`: removes the file and commits the removal.
    pub fn delete(&self, path: &Path) -> Result<()> {
        let mut last_gc = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.ensure_locked(&mut last_gc)?;
        let rel = self.relative(path)?;
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => bail!("auth filestore: delete failed: {e}"),
        }
        self.commit_and_push(&mut last_gc, &format!("Delete auth {}", path.display()), &[rel], false)
    }

    /// Go `PersistConfig`.
    pub fn persist_config(&self) -> Result<()> {
        let mut last_gc = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.ensure_locked(&mut last_gc)?;
        let config = self.config_path();
        if !config.exists() {
            return Ok(());
        }
        let rel = self.relative(&config)?;
        self.commit_and_push(&mut last_gc, "Update config", &[rel], false)
    }
}

/// The async face the server calls; git work runs on the blocking pool.
pub struct GitPersister(pub Arc<GitStore>);

impl cpa_server::persist::StorePersister for GitPersister {
    fn persist_config(&self) -> BoxFuture<'_, Result<()>> {
        let store = self.0.clone();
        Box::pin(async move { blocking(move || store.persist_config()).await })
    }

    fn persist_auth_files(&self, message: String, paths: Vec<PathBuf>) -> BoxFuture<'_, Result<()>> {
        let store = self.0.clone();
        Box::pin(async move { blocking(move || store.persist_auth_files(&message, &paths)).await })
    }

    fn delete_auth(&self, path: PathBuf) -> Result<()> {
        // Inline on the caller's blocking thread: no second blocking-pool slot.
        self.0.delete(&path)
    }

    fn auth_dir(&self) -> PathBuf {
        self.0.auth_dir()
    }
}

pub(crate) async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| anyhow!("store task failed: {e}"))?
}
