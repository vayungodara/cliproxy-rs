//! Go internal/store/gitstore_test.go cases (M4-0361) against local bare remotes. Each
//! test names the Go test it mirrors; the expected values are Go's assertions.
//!
//! Not ported: the go-git handle cases (`TestRecoverRepositoryCloseFailures*`,
//! `TestInspectRecoveryBaselineFailureClosesRepositoryHandle`, rename-failure
//! injection), which test go-git repository handles the git executable does not have,
//! and `TestGitTokenStoreDisabledLoginReachesTokenStorage`, whose `Save` policy lives in
//! the server's persist hooks here, not in the store.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::GitStore;

fn scratch(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("cpa-gitgo-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=CLIProxyAPI", "-c", "user.email=cliproxy@local"])
        .args(["-c", "commit.gpgsign=false", "-c", "init.defaultBranch=master"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Go `setupGitRemoteRepository`: a bare remote whose branches each hold `branch.txt`,
/// seeded from `<root>/seed` (later advanced from there), HEAD naming `default`.
fn setup_remote(root: &Path, default: &str, branches: &[(&str, &str)]) -> String {
    let remote = root.join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare"]);
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q"]);
    git(&seed, &["symbolic-ref", "HEAD", &format!("refs/heads/{default}")]);
    let marker = |name: &str, contents: &str, message: &str| {
        let _ = name;
        std::fs::write(seed.join("branch.txt"), contents).unwrap();
        git(&seed, &["add", "branch.txt"]);
        git(&seed, &["commit", "-q", "-m", message]);
    };
    let (_, contents) = branches.iter().find(|(name, _)| *name == default).unwrap();
    marker(default, contents, "seed default branch");
    for (name, contents) in branches.iter().filter(|(name, _)| *name != default) {
        git(&seed, &["checkout", "-q", default]);
        git(&seed, &["checkout", "-q", "-b", name]);
        marker(name, contents, &format!("seed branch {name}"));
    }
    git(&seed, &["remote", "add", "origin", &remote.to_string_lossy()]);
    git(&seed, &["push", "-q", "origin", "refs/heads/*:refs/heads/*"]);
    git(&remote, &["symbolic-ref", "HEAD", &format!("refs/heads/{default}")]);
    remote.to_string_lossy().into_owned()
}

/// Go `advanceRemoteBranch` (from the branch) and `advanceRemoteBranchFromNewBranch`
/// (a new branch from master).
fn advance(root: &Path, branch: &str, contents: &str, new_from_master: bool) {
    let seed = root.join("seed");
    if new_from_master {
        git(&seed, &["checkout", "-q", "master"]);
        git(&seed, &["checkout", "-q", "-b", branch]);
    } else {
        git(&seed, &["checkout", "-q", branch]);
    }
    std::fs::write(seed.join("branch.txt"), contents).unwrap();
    git(&seed, &["add", "branch.txt"]);
    git(&seed, &["commit", "-q", "-m", &format!("advance {branch}")]);
    git(
        &seed,
        &[
            "push",
            "-q",
            "origin",
            &format!("refs/heads/{branch}:refs/heads/{branch}"),
        ],
    );
}

fn head_branch(repo: &Path) -> String {
    git(repo, &["symbolic-ref", "--short", "HEAD"])
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

fn remote_file(remote: &str, branch: &str, path: &str) -> Option<String> {
    let output = Command::new("git")
        .current_dir(remote)
        .args(["show", &format!("{branch}:{path}")])
        .output()
        .unwrap();
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn remote_head(remote: &str) -> String {
    git(Path::new(remote), &["symbolic-ref", "--short", "HEAD"])
}

/// Go `assertRepositoryBranchAndContents`.
fn branch_and_contents(repo: &Path, branch: &str, contents: &str) {
    assert_eq!(head_branch(repo), branch);
    assert_eq!(read(&repo.join("branch.txt")), contents);
}

fn store(remote: &str, branch: &str, repo: &Path) -> GitStore {
    GitStore::new(remote, "", "", branch, repo)
}

/// Writes `auths/<name>` and persists it (Go `Save`).
fn save(store: &GitStore, name: &str, contents: &str) -> PathBuf {
    let path = store.auth_dir().join(name);
    std::fs::write(&path, contents).unwrap();
    store
        .persist_auth_files(&format!("Sync auth {name}"), std::slice::from_ref(&path))
        .unwrap();
    path
}

fn commit(store: &GitStore, message: &str, paths: &[&str]) -> anyhow::Result<()> {
    let mut last_gc = store.lock.lock().unwrap();
    let paths: Vec<String> = paths.iter().map(|p| (*p).to_owned()).collect();
    store.commit_and_push(&mut last_gc, message, &paths, false)
}

#[test]
fn uses_remote_default_branch_when_branch_not_configured() {
    let root = scratch("default");
    let remote = setup_remote(
        &root,
        "trunk",
        &[
            ("trunk", "remote default branch\n"),
            ("release/2026", "release branch\n"),
        ],
    );
    let ws = root.join("workspace");
    let s = store(&remote, "", &ws);
    s.ensure_repository().unwrap();
    branch_and_contents(&ws, "trunk", "remote default branch\n");
    advance(&root, "trunk", "remote default branch updated\n", false);
    advance(&root, "release/2026", "release branch updated\n", false);
    s.ensure_repository().unwrap();
    branch_and_contents(&ws, "trunk", "remote default branch updated\n");
    assert_eq!(remote_head(&remote), "trunk");
}

#[test]
fn uses_configured_branch_when_explicitly_set() {
    let root = scratch("configured");
    let remote = setup_remote(
        &root,
        "trunk",
        &[
            ("trunk", "remote default branch\n"),
            ("release/2026", "release branch\n"),
        ],
    );
    let ws = root.join("workspace");
    let s = store(&remote, "release/2026", &ws);
    s.ensure_repository().unwrap();
    branch_and_contents(&ws, "release/2026", "release branch\n");
    advance(&root, "trunk", "remote default branch updated\n", false);
    advance(&root, "release/2026", "release branch updated\n", false);
    s.ensure_repository().unwrap();
    branch_and_contents(&ws, "release/2026", "release branch updated\n");
    assert_eq!(remote_head(&remote), "trunk");
}

#[test]
fn a_missing_configured_branch_fails() {
    let root = scratch("missing");
    let remote = setup_remote(&root, "trunk", &[("trunk", "remote default branch\n")]);
    assert!(
        store(&remote, "missing-branch", &root.join("workspace"))
            .ensure_repository()
            .is_err()
    );
    assert_eq!(remote_head(&remote), "trunk");

    // Go `...OnExistingRepositoryPull`: the reopened store fails and HEAD stays.
    let root = scratch("missing-pull");
    let remote = setup_remote(&root, "trunk", &[("trunk", "remote default branch\n")]);
    let ws = root.join("workspace");
    store(&remote, "", &ws).ensure_repository().unwrap();
    assert!(store(&remote, "missing-branch", &ws).ensure_repository().is_err());
    assert_eq!(head_branch(&ws), "trunk");
    assert_eq!(remote_head(&remote), "trunk");
}

#[test]
fn initializes_an_empty_remote_using_the_configured_branch() {
    let root = scratch("empty-branch");
    let remote = root.join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare"]);
    let remote = remote.to_string_lossy().into_owned();
    let ws = root.join("workspace");
    store(&remote, "feature/gemini-fix", &ws).ensure_repository().unwrap();
    assert_eq!(head_branch(&ws), "feature/gemini-fix");
    git(
        Path::new(&remote),
        &["rev-parse", "--verify", "refs/heads/feature/gemini-fix"],
    );
    let master = Command::new("git")
        .current_dir(&remote)
        .args(["rev-parse", "--verify", "-q", "refs/heads/master"])
        .output()
        .unwrap();
    assert!(!master.status.success(), "no master branch is created");
}

#[test]
fn an_existing_repository_switches_to_the_configured_branch() {
    let root = scratch("switch");
    let remote = setup_remote(
        &root,
        "master",
        &[
            ("master", "remote master branch\n"),
            ("develop", "remote develop branch\n"),
        ],
    );
    let ws = root.join("workspace");
    store(&remote, "", &ws).ensure_repository().unwrap();
    branch_and_contents(&ws, "master", "remote master branch\n");
    let reopened = store(&remote, "develop", &ws);
    reopened.ensure_repository().unwrap();
    branch_and_contents(&ws, "develop", "remote develop branch\n");
    std::fs::write(ws.join("branch.txt"), "local develop update\n").unwrap();
    commit(&reopened, "Update develop branch marker", &["branch.txt"]).unwrap();
    assert_eq!(head_branch(&ws), "develop");
    assert_eq!(
        remote_file(&remote, "develop", "branch.txt").unwrap(),
        "local develop update\n"
    );
    assert_eq!(
        remote_file(&remote, "master", "branch.txt").unwrap(),
        "remote master branch\n"
    );
}

#[test]
fn switches_to_a_configured_branch_created_after_clone() {
    let root = scratch("switch-new");
    let remote = setup_remote(&root, "master", &[("master", "remote master branch\n")]);
    let ws = root.join("workspace");
    store(&remote, "", &ws).ensure_repository().unwrap();
    branch_and_contents(&ws, "master", "remote master branch\n");
    advance(&root, "release/2026", "release branch\n", true);
    store(&remote, "release/2026", &ws).ensure_repository().unwrap();
    branch_and_contents(&ws, "release/2026", "release branch\n");
}

#[test]
fn resets_to_the_remote_default_when_branch_unset() {
    let root = scratch("reset-default");
    let remote = setup_remote(
        &root,
        "master",
        &[
            ("master", "remote master branch\n"),
            ("develop", "remote develop branch\n"),
        ],
    );
    let ws = root.join("workspace");
    store(&remote, "develop", &ws).ensure_repository().unwrap();
    branch_and_contents(&ws, "develop", "remote develop branch\n");
    let default = store(&remote, "", &ws);
    default.ensure_repository().unwrap();
    assert_eq!(head_branch(&ws), "master");
    std::fs::write(ws.join("branch.txt"), "local master update\n").unwrap();
    commit(&default, "Update master marker", &["branch.txt"]).unwrap();
    assert_eq!(
        remote_file(&remote, "master", "branch.txt").unwrap(),
        "local master update\n"
    );
}

#[test]
fn a_repeated_delete_does_not_overwrite_remote_only_changes() {
    let root = scratch("repeat-delete");
    let remote = setup_remote(&root, "master", &[("master", "remote master branch\n")]);
    let a = store(&remote, "", &root.join("workspace-a"));
    a.ensure_repository().unwrap();
    let path = save(&a, "a.json", r#"{"access_token":"a"}"#);
    a.delete(&path).unwrap();
    let b = store(&remote, "", &root.join("workspace-b"));
    b.ensure_repository().unwrap();
    save(&b, "b.json", r#"{"access_token":"b"}"#);
    assert!(remote_file(&remote, "master", "auths/b.json").is_some());
    a.delete(&path).unwrap();
    assert!(remote_file(&remote, "master", "auths/b.json").is_some());
}

#[test]
fn rejects_paths_outside_the_repository_before_mutation() {
    let root = scratch("outside");
    let remote = setup_remote(&root, "master", &[("master", "remote master branch\n")]);
    let s = store(&remote, "", &root.join("workspace"));
    s.ensure_repository().unwrap();
    let outside = root.join("outside.json");
    std::fs::write(&outside, "outside\n").unwrap();
    assert!(s.delete(&outside).is_err());
    assert_eq!(read(&outside), "outside\n");
    assert!(
        s.persist_auth_files("Sync auth outside.json", std::slice::from_ref(&outside))
            .is_err()
    );
    assert!(remote_file(&remote, "master", "outside.json").is_none());
}

#[test]
fn persist_config_drops_unrelated_staged_deletions() {
    let root = scratch("staged-delete");
    let remote = setup_remote(&root, "master", &[("master", "remote master branch\n")]);
    let ws = root.join("workspace");
    let s = store(&remote, "", &ws);
    s.ensure_repository().unwrap();
    let auth = save(&s, "protected.json", r#"{"access_token":"token"}"#);
    std::fs::write(s.config_path(), "version: one\n").unwrap();
    s.persist_config().unwrap();
    git(&ws, &["rm", "-q", "auths/protected.json"]);
    assert!(!auth.exists());
    std::fs::write(s.config_path(), "version: two\n").unwrap();
    s.persist_config().unwrap();
    assert!(remote_file(&remote, "master", "auths/protected.json").is_some());
    assert_eq!(
        remote_file(&remote, "master", "config/config.yaml").unwrap(),
        "version: two\n"
    );
}

#[test]
fn persist_config_repairs_the_index_after_an_unstaged_pull() {
    let root = scratch("unstaged-pull");
    let remote = setup_remote(&root, "master", &[("master", "remote master branch\n")]);
    let s = store(&remote, "", &root.join("workspace"));
    s.ensure_repository().unwrap();
    std::fs::write(s.config_path(), "source: local-config\n").unwrap();
    advance(&root, "master", "remote branch advanced\n", false);
    s.persist_config().unwrap();
    assert_eq!(
        remote_file(&remote, "master", "branch.txt").unwrap(),
        "remote branch advanced\n"
    );
    assert_eq!(
        remote_file(&remote, "master", "config/config.yaml").unwrap(),
        "source: local-config\n"
    );
}

#[test]
fn persist_config_preserves_remote_only_auth_after_divergence() {
    let root = scratch("divergence");
    let remote = setup_remote(&root, "master", &[("master", "remote master branch\n")]);
    let a = store(&remote, "", &root.join("workspace-a"));
    a.ensure_repository().unwrap();
    let b = store(&remote, "", &root.join("workspace-b"));
    b.ensure_repository().unwrap();
    save(&b, "remote-only.json", r#"{"access_token":"remote"}"#);
    std::fs::write(a.config_path(), "source: store-a\n").unwrap();
    a.persist_config().unwrap();
    assert!(remote_file(&remote, "master", "auths/remote-only.json").is_some());
    assert_eq!(
        remote_file(&remote, "master", "config/config.yaml").unwrap(),
        "source: store-a\n"
    );
}

/// Go `TestGitTokenStoreRejectsStaleForcePush` and
/// `TestGitTokenStoreSaveRetryAfterLeaseConflictCommitsMatchingContent`: a push whose
/// lease is stale is rejected without touching the remote; the retry, which pulls
/// first, lands.
#[test]
fn a_stale_lease_is_rejected_and_the_retry_lands() {
    let root = scratch("stale-lease");
    let remote = setup_remote(&root, "master", &[("master", "remote master branch\n")]);
    let a = store(&remote, "", &root.join("workspace-a"));
    a.ensure_repository().unwrap();
    let b = store(&remote, "", &root.join("workspace-b"));
    b.ensure_repository().unwrap();
    save(&b, "concurrent.json", r#"{"access_token":"remote"}"#);
    std::fs::write(a.config_path(), "source: stale-a\n").unwrap();
    assert!(commit(&a, "Update stale config", &["config/config.yaml"]).is_err());
    assert!(remote_file(&remote, "master", "auths/concurrent.json").is_some());
    assert!(remote_file(&remote, "master", "config/config.yaml").is_none());
    a.persist_config().unwrap();
    assert!(remote_file(&remote, "master", "auths/concurrent.json").is_some());
    assert_eq!(
        remote_file(&remote, "master", "config/config.yaml").unwrap(),
        "source: stale-a\n"
    );
}

/// Go `TestEnsureRepositoryRetryRestoresTrackedAuthOnUpToDatePull`: an unreachable
/// remote fails the sync without restoring anything; the retry restores the tracked
/// auth, which an explicit delete then removes.
#[test]
fn a_retry_restores_a_tracked_auth_on_an_up_to_date_pull() {
    let root = scratch("retry-restore");
    let remote = setup_remote(&root, "master", &[("master", "remote master branch\n")]);
    let ws = root.join("workspace");
    let s = store(&remote, "", &ws);
    s.ensure_repository().unwrap();
    let auth = save(&s, "retry.json", r#"{"access_token":"remote"}"#);
    git(&ws, &["rm", "-q", "auths/retry.json"]);
    let missing = root.join("missing.git").to_string_lossy().into_owned();
    git(&ws, &["remote", "set-url", "origin", &missing]);
    assert!(s.ensure_repository().is_err());
    assert!(!auth.exists());
    git(&ws, &["remote", "set-url", "origin", &remote]);
    s.ensure_repository().unwrap();
    assert_eq!(read(&auth), r#"{"access_token":"remote"}"#);
    s.delete(&auth).unwrap();
    assert!(remote_file(&remote, "master", "auths/retry.json").is_none());
    assert!(!auth.exists());
}

#[test]
fn reconciles_remote_auth_changes_around_local_config() {
    let root = scratch("reconcile-auth");
    let remote = setup_remote(&root, "master", &[("master", "remote master branch\n")]);
    let owner = store(&remote, "", &root.join("owner"));
    owner.ensure_repository().unwrap();
    for name in ["modified.json", "deleted.json"] {
        save(&owner, name, r#"{"access_token":"old"}"#);
    }
    std::fs::write(owner.config_path(), "source: original\n").unwrap();
    owner.persist_config().unwrap();
    let a = store(&remote, "", &root.join("workspace-a"));
    a.ensure_repository().unwrap();
    let b = store(&remote, "", &root.join("workspace-b"));
    b.ensure_repository().unwrap();
    std::fs::write(a.config_path(), "source: local-a\n").unwrap();
    save(&b, "modified.json", r#"{"access_token":"new"}"#);
    b.delete(&b.auth_dir().join("deleted.json")).unwrap();
    a.ensure_repository().unwrap();
    assert_eq!(read(&a.config_path()), "source: local-a\n");
    assert_eq!(read(&a.auth_dir().join("modified.json")), r#"{"access_token":"new"}"#);
    assert!(!a.auth_dir().join("deleted.json").exists());
}

#[test]
fn reconciles_remote_config_changes_around_local_auth() {
    let root = scratch("reconcile-config");
    let remote = setup_remote(&root, "master", &[("master", "remote master branch\n")]);
    let owner = store(&remote, "", &root.join("owner"));
    owner.ensure_repository().unwrap();
    save(&owner, "local.json", r#"{"access_token":"old"}"#);
    std::fs::write(owner.config_path(), "source: original\n").unwrap();
    owner.persist_config().unwrap();
    let a = store(&remote, "", &root.join("workspace-a"));
    a.ensure_repository().unwrap();
    let b = store(&remote, "", &root.join("workspace-b"));
    b.ensure_repository().unwrap();
    let local = a.auth_dir().join("local.json");
    let dirty = r#"{"type":"codex","access_token":"local-dirty"}"#;
    std::fs::write(&local, dirty).unwrap();
    std::fs::write(b.config_path(), "source: remote-modified\n").unwrap();
    b.persist_config().unwrap();
    a.ensure_repository().unwrap();
    assert_eq!(read(&a.config_path()), "source: remote-modified\n");
    assert_eq!(read(&local), dirty);

    std::fs::remove_file(b.config_path()).unwrap();
    commit(&b, "Delete config", &["config/config.yaml"]).unwrap();
    a.ensure_repository().unwrap();
    assert!(!a.config_path().exists());
    assert_eq!(read(&local), dirty);
}

#[test]
fn fails_closed_on_a_same_path_conflict() {
    let root = scratch("same-path");
    let remote = setup_remote(&root, "master", &[("master", "remote master branch\n")]);
    let owner = store(&remote, "", &root.join("owner"));
    owner.ensure_repository().unwrap();
    std::fs::write(owner.config_path(), "source: original\n").unwrap();
    owner.persist_config().unwrap();
    let a = store(&remote, "", &root.join("workspace-a"));
    a.ensure_repository().unwrap();
    let b = store(&remote, "", &root.join("workspace-b"));
    b.ensure_repository().unwrap();
    std::fs::write(a.config_path(), "source: local\n").unwrap();
    std::fs::write(b.config_path(), "source: remote\n").unwrap();
    b.persist_config().unwrap();
    let error = format!("{:#}", a.ensure_repository().unwrap_err());
    assert!(error.contains("conflicts with local change"), "{error}");
    assert_eq!(read(&a.config_path()), "source: local\n");
    assert_eq!(
        remote_file(&remote, "master", "config/config.yaml").unwrap(),
        "source: remote\n"
    );
}

/// Go `TestCommitAndPushLockedPushesBeforeRunningGC`: a due GC never delays the push.
#[test]
fn pushes_before_running_gc() {
    let root = scratch("gc");
    let remote = setup_remote(&root, "master", &[("master", "remote master branch\n")]);
    let ws = root.join("workspace");
    let s = store(&remote, "", &ws);
    s.ensure_repository().unwrap();
    for contents in ["local master update one\n", "local master update two\n"] {
        std::fs::write(ws.join("branch.txt"), contents).unwrap();
        *s.lock.lock().unwrap() = None;
        commit(&s, "Update master marker", &["branch.txt"]).unwrap();
        assert_eq!(remote_file(&remote, "master", "branch.txt").unwrap(), contents);
    }
}

#[test]
fn follows_a_renamed_remote_default_branch() {
    let root = scratch("renamed");
    let remote = setup_remote(
        &root,
        "master",
        &[("master", "remote master branch\n"), ("main", "remote main branch\n")],
    );
    let ws = root.join("workspace");
    store(&remote, "", &ws).ensure_repository().unwrap();
    branch_and_contents(&ws, "master", "remote master branch\n");
    git(Path::new(&remote), &["symbolic-ref", "HEAD", "refs/heads/main"]);
    advance(&root, "main", "remote main branch updated\n", false);
    store(&remote, "", &ws).ensure_repository().unwrap();
    branch_and_contents(&ws, "main", "remote main branch updated\n");
    assert_eq!(remote_head(&remote), "main");
}

/// Go `TestEnsureRepositoryKeepsCurrentBranchWhenRemoteDefaultCannotBeResolved`: a
/// remote that demands authentication leaves the pinned branch checked out.
#[test]
fn keeps_the_current_branch_when_the_remote_default_cannot_be_resolved() {
    use std::io::{Read, Write};
    let root = scratch("unresolved");
    let remote = setup_remote(
        &root,
        "master",
        &[
            ("master", "remote master branch\n"),
            ("develop", "remote develop branch\n"),
        ],
    );
    let ws = root.join("workspace");
    store(&remote, "develop", &ws).ensure_repository().unwrap();
    branch_and_contents(&ws, "develop", "remote develop branch\n");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/tokens.git", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"git\"\r\nContent-Length: 13\r\nConnection: close\r\n\r\nauth required",
            );
        }
    });
    git(&ws, &["remote", "set-url", "origin", &url]);
    store(&remote, "", &ws).ensure_repository().unwrap();
    assert_eq!(head_branch(&ws), "develop");
}
