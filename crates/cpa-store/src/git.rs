//! `GITSTORE_*`: config and auth files in a git repository (Go
//! internal/store/gitstore.go). The working tree under `<root>` holds `config/` and
//! `auths/`; every persisted change becomes the branch's single, parentless commit
//! (history is squashed on purpose: it holds secrets) and is pushed with
//! force-with-lease, so concurrent writers never silently overwrite each other.
//!
//! A repository whose HEAD can no longer be read (missing objects or packs) is
//! recovered as Go does: a fresh clone beside it, the local edits the remote did not
//! touch carried over, then swapped in, with a rollback when any step fails.
//!
//! ponytail: drives the `git` executable instead of an embedded git library; the
//! container needs `git` installed. Any failure to read HEAD's objects counts as
//! corruption; go-git tells a missing object from other read errors, the CLI does not.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
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

/// What a recovery keeps of the corrupt repository: HEAD's tree (path to entry) and
/// the local edits.
#[derive(Clone)]
struct Baseline {
    tree: BTreeMap<String, String>,
    dirty: Vec<String>,
}

/// Go `os.MkdirTemp(parent, ".gitstore-recovery-")`.
fn recovery_dir(parent: &Path) -> std::io::Result<PathBuf> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    loop {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = parent.join(format!(".gitstore-recovery-{}{nanos:09}{n}", std::process::id()));
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(false);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Go `applyRecoveryLocalChanges`: each preserved path of `source` replaces the same
/// path in `target`; a path missing from `source` is removed from `target`.
fn apply_local_changes(source: &Path, target: &Path, paths: &[String]) -> Result<()> {
    for path in paths {
        let from = source.join(path);
        let to = target.join(path);
        let remove = |to: &Path| -> std::io::Result<()> {
            match std::fs::symlink_metadata(to) {
                Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(to),
                Ok(_) => std::fs::remove_file(to),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        };
        let info = match std::fs::symlink_metadata(&from) {
            Ok(info) => info,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                remove(&to).with_context(|| format!("preserve deletion {path}"))?;
                continue;
            }
            Err(e) => return Err(anyhow!("inspect local change {path}: {e}")),
        };
        remove(&to).with_context(|| format!("replace recovered path {path}"))?;
        if let Some(parent) = to.parent() {
            mkdir_0700(parent).with_context(|| format!("create recovered parent for {path}"))?;
        }
        if info.file_type().is_symlink() {
            let link = std::fs::read_link(&from).with_context(|| format!("read local symlink {path}"))?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link, &to).with_context(|| format!("write local symlink {path}"))?;
            #[cfg(not(unix))]
            bail!(
                "local change {path} is a symlink ({}); not supported here",
                link.display()
            );
        } else if info.is_file() {
            std::fs::copy(&from, &to).with_context(|| format!("write local change {path}"))?;
        } else {
            bail!("local change {path} has unsupported file mode");
        }
    }
    Ok(())
}

/// Go `moveWorktreeEntries`: every entry but `.git`, undone on failure. The error
/// carries whether entries were left in `target` (an undo failed too).
fn move_entries(source: &Path, target: &Path) -> std::result::Result<(), (bool, anyhow::Error)> {
    mkdir_0700(target).map_err(|e| (false, e))?;
    // Read every entry before moving any (Go `os.ReadDir`), so a read error can never
    // strand entries that were already moved.
    let entries = std::fs::read_dir(source)
        .and_then(|dir| {
            dir.map(|entry| entry.map(|e| e.file_name()))
                .collect::<std::io::Result<Vec<_>>>()
        })
        .map_err(|e| (false, anyhow!(e)))?;
    let mut moved: Vec<std::ffi::OsString> = Vec::new();
    for name in entries {
        if name == ".git" {
            continue;
        }
        if let Err(error) = std::fs::rename(source.join(&name), target.join(&name)) {
            let mut error = anyhow!("move {}: {error}", name.to_string_lossy());
            let mut kept = false;
            for done in moved.iter().rev() {
                if let Err(again) = std::fs::rename(target.join(done), source.join(done)) {
                    kept = true;
                    error = error.context(format!("restore {}: {again}", done.to_string_lossy()));
                }
            }
            return Err((kept, error));
        }
        moved.push(name);
    }
    Ok(())
}

/// Go `installRecoveredGitDirectory`: moves the corrupt `.git` aside and the clone's in.
/// Sets `retain` when the corrupt directory could not be put back after a failure.
fn install_git_dir(git_dir: &Path, cloned: &Path, corrupt: &Path, retain: &mut bool) -> Result<()> {
    std::fs::rename(git_dir, corrupt).context("backup corrupt git directory")?;
    if let Err(error) = std::fs::rename(cloned, git_dir) {
        let installed = anyhow!("install recovered git directory: {error}");
        if let Err(again) = std::fs::rename(corrupt, git_dir) {
            *retain = true;
            return Err(installed.context(format!(
                "restore corrupt git directory; backup retained at {}: {again}",
                corrupt.display()
            )));
        }
        return Err(installed);
    }
    Ok(())
}

/// Go `removeWorktreeEntries`.
fn remove_entries(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(path)?;
        } else {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
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
            if let Err(corrupt) = self.verify_head() {
                self.recover(None).map_err(|error| {
                    anyhow!("git token store: verify repository before pull: {corrupt:#}; recovery failed: {error:#}")
                })?;
            }
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
        // Go captures the pre-pull tree and local edits for a recovery the pull may need.
        let baseline = || -> Option<Baseline> {
            Some(Baseline {
                tree: self.rev("HEAD").ok().flatten().and(self.tree_entries("HEAD").ok())?,
                dirty: self.dirty_paths().ok()?,
            })
        };
        let before = baseline();
        let recover = |cause: anyhow::Error| -> Result<()> {
            self.recover(before.clone())
                .map_err(|error| anyhow!("{cause:#}; recovery failed: {error:#}"))
        };
        if let Err(error) = self.pull_once() {
            return match self.verify_head() {
                Ok(()) => Err(error),
                Err(_) => recover(error),
            };
        }
        if let Err(corrupt) = self.verify_head() {
            return recover(anyhow!("git token store: verify repository after pull: {corrupt:#}"));
        }
        self.restore_missing()
    }

    fn pull_once(&self) -> Result<()> {
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
            return Ok(());
        };
        let base = self.rev("HEAD")?;
        if base.as_deref() == Some(remote.as_str()) {
            self.git(&["reset", "-q"])
                .context("git token store: repair index after up-to-date pull")?;
        } else {
            self.reconcile(base.as_deref(), &remote, &branch)?;
        }
        Ok(())
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

    /// Go `verifyRepositoryHead`: nothing to check without a HEAD commit; otherwise its
    /// commit, tree and every file's contents must be readable.
    fn verify_head(&self) -> Result<()> {
        let output = self.run_in(&self.repo, &["rev-parse", "-q", "--verify", "HEAD"])?;
        if !output.status.success() {
            return Ok(());
        }
        let head = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let archived = self
            .command(&self.repo, &["archive", "--format=tar", &head])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| anyhow!("run git: {e}"))?;
        if !archived.status.success() {
            bail!(
                "object not found: {}",
                self.redact(String::from_utf8_lossy(&archived.stderr).trim())
            );
        }
        Ok(())
    }

    /// The files of `rev`'s tree in `dir`: path to `mode type hash` (Go diffs trees by
    /// entry).
    fn tree_entries_in(&self, dir: &Path, rev: &str) -> Result<BTreeMap<String, String>> {
        let output = self.run_in(dir, &["ls-tree", "-r", "-z", "--full-tree", rev])?;
        if !output.status.success() {
            bail!("{}", self.redact(String::from_utf8_lossy(&output.stderr).trim()));
        }
        Ok(split_nul(&output.stdout)
            .into_iter()
            .filter_map(|line| {
                let (entry, path) = line.split_once('\t')?;
                Some((path.to_owned(), entry.to_owned()))
            })
            .collect())
    }

    fn tree_entries(&self, rev: &str) -> Result<BTreeMap<String, String>> {
        self.tree_entries_in(&self.repo, rev)
    }

    /// Go `recoverRepositoryLocked`: clones the remote beside the repository, carries
    /// the local edits over unless the remote changed the same paths, then swaps the
    /// clone's `.git` and files in. Every failure before the swap leaves the repository
    /// as it was; a failed swap is rolled back, and the backup is kept when even that
    /// fails.
    fn recover(&self, baseline: Option<Baseline>) -> Result<()> {
        let baseline = match baseline {
            Some(baseline) => baseline,
            None => self.inspect_baseline().context("inspect recovery baseline")?,
        };
        let parent = self.repo.parent().unwrap_or(&self.repo).to_path_buf();
        let root = recovery_dir(&parent).context("create recovery directory")?;
        let mut retain = false;
        let result = self.recover_into(&baseline, &root, &mut retain);
        if !retain {
            let _ = std::fs::remove_dir_all(&root);
        }
        result
    }

    /// Go `inspectRecoveryBaseline`: the local edits, then HEAD's tree.
    fn inspect_baseline(&self) -> Result<Baseline> {
        let dirty = self.dirty_paths().context("inspect worktree changes")?;
        let tree = self.tree_entries("HEAD").context("inspect head tree")?;
        Ok(Baseline { tree, dirty })
    }

    fn recover_into(&self, baseline: &Baseline, root: &Path, retain: &mut bool) -> Result<()> {
        let clone = root.join("clone");
        let clone_arg = clone.to_string_lossy().into_owned();
        let mut args = vec!["clone", "-q"];
        if !self.branch.is_empty() {
            args.extend(["--branch", &self.branch]);
        }
        args.extend(["--", &self.remote, &clone_arg]);
        let cloned = self.run_in(root, &args)?;
        if !cloned.status.success() {
            bail!(
                "clone remote repository: {}",
                self.redact(String::from_utf8_lossy(&cloned.stderr).trim())
            );
        }
        let cloned_store = GitStore {
            repo: clone.clone(),
            ..self.shallow()
        };
        cloned_store.verify_head().context("verify cloned repository")?;
        // Go `recoveryPreservedPaths`: local edits survive unless the remote changed an
        // overlapping path since the baseline.
        let remote = self
            .tree_entries_in(&clone, "HEAD")
            .context("inspect cloned repository tree")?;
        if !baseline.dirty.is_empty() {
            let changed = baseline
                .tree
                .keys()
                .chain(remote.keys())
                .filter(|path| baseline.tree.get(*path) != remote.get(*path));
            for path in changed {
                if let Some(local) = baseline.dirty.iter().find(|d| overlaps(path, d)) {
                    bail!("remote path {path} conflicts with local change {local} during repository recovery");
                }
            }
        }
        let mut preserved = baseline.dirty.clone();
        preserved.sort();
        preserved.dedup();
        apply_local_changes(&self.repo, &clone, &preserved).context("preserve local worktree changes")?;
        let backup = root.join("worktree");
        if let Err((kept, error)) = move_entries(&self.repo, &backup) {
            *retain = kept;
            return Err(if kept {
                error.context(format!(
                    "backup existing worktree; backup retained at {}",
                    backup.display()
                ))
            } else {
                error.context("backup existing worktree")
            });
        }
        let git_dir = self.repo.join(".git");
        let corrupt = root.join("corrupt.git");
        // Go puts the worktree back after any failure to install the git directory,
        // even when the corrupt directory itself could not be restored.
        if let Err(installed) = install_git_dir(&git_dir, &clone.join(".git"), &corrupt, retain) {
            if let Err((_, again)) = move_entries(&backup, &self.repo) {
                *retain = true;
                return Err(installed.context(format!(
                    "restore worktree; backup retained at {}: {again:#}",
                    backup.display()
                )));
            }
            return Err(installed);
        }
        let installed = move_entries(&clone, &self.repo)
            .map_err(|(_, error)| error.context("install recovered worktree"))
            .and_then(|()| self.verify_head().context("verify recovered repository"));
        if let Err(error) = installed {
            if let Err(again) = self.rollback(&corrupt, &backup) {
                *retain = true;
                return Err(error.context(format!(
                    "rollback recovered repository; backup retained at {}: {again:#}",
                    root.display()
                )));
            }
            return Err(error);
        }
        self.tighten_permissions()
    }

    /// Go `rollbackRecoveredRepository`.
    fn rollback(&self, corrupt: &Path, backup: &Path) -> Result<()> {
        remove_entries(&self.repo).context("remove recovered worktree")?;
        let git_dir = self.repo.join(".git");
        std::fs::remove_dir_all(&git_dir).context("remove recovered git directory")?;
        std::fs::rename(corrupt, &git_dir).context("restore original git directory")?;
        move_entries(backup, &self.repo).map_err(|(_, error)| error.context("restore original worktree"))
    }

    /// The same remote and credentials for another directory.
    fn shallow(&self) -> GitStore {
        GitStore {
            remote: self.remote.clone(),
            branch: self.branch.clone(),
            username: self.username.clone(),
            password: self.password.clone(),
            repo: self.repo.clone(),
            lock: Mutex::new(None),
        }
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

// Local git remotes and file modes: Unix only, like tests/git.rs.
#[cfg(all(test, unix))]
#[path = "git_tests.rs"]
mod tests;
