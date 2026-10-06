//! `GITSTORE_*` against local bare repositories: two nodes sharing one remote, as two
//! CLIProxyAPI instances share a token repository.
// Mode bits, initdb and local git remotes: Unix only.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use cpa_store::GitStore;

fn scratch(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("cpa-git-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git").current_dir(dir).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// An empty bare remote whose HEAD names `main`.
fn bare_remote(dir: &Path) -> String {
    let remote = dir.join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare"]);
    git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    remote.to_string_lossy().into_owned()
}

/// `(subject, parents)` of the remote branch tip, and the tree's files.
fn remote_tip(remote: &str, branch: &str) -> (String, String, Vec<String>) {
    let dir = Path::new(remote);
    let subject = git(dir, &["log", "-1", "--format=%s", branch]);
    let parents = git(dir, &["log", "-1", "--format=%P", branch]);
    let files = git(dir, &["ls-tree", "-r", "--name-only", branch])
        .lines()
        .map(str::to_owned)
        .collect();
    (subject, parents, files)
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

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn the_first_node_initializes_an_empty_remote() {
    let dir = scratch("init");
    let remote = bare_remote(&dir);
    let store = GitStore::new(&remote, "", "", "main", &dir.join("a"));
    store.ensure_repository().unwrap();
    let (subject, parents, files) = remote_tip(&remote, "main");
    assert_eq!(subject, "Initialize git token store");
    assert_eq!(parents, "", "Go pushes one parentless commit");
    assert_eq!(files, vec!["auths/.gitkeep", "config/.gitkeep"]);
    assert!(store.auth_dir().is_dir() && store.config_path().parent().unwrap().is_dir());
    // A second call is a pull, not another initialization.
    store.ensure_repository().unwrap();
    assert_eq!(git(Path::new(&remote), &["rev-list", "--count", "main"]), "1");
}

#[test]
fn two_nodes_share_auth_files_and_config() {
    let dir = scratch("share");
    let remote = bare_remote(&dir);
    let a = GitStore::new(&remote, "", "", "main", &dir.join("a"));
    a.ensure_repository().unwrap();
    std::fs::write(a.auth_dir().join("a.json"), "{\"a\":1}").unwrap();
    a.persist_auth_files("Sync auth a.json", &[a.auth_dir().join("a.json")])
        .unwrap();
    let (subject, parents, files) = remote_tip(&remote, "main");
    assert_eq!(subject, "Sync auth a.json");
    assert_eq!(parents, "", "history stays squashed to one commit");
    assert!(files.contains(&"auths/a.json".to_owned()));

    // No branch configured: B follows the remote's default branch.
    let b = GitStore::new(&remote, "", "", "", &dir.join("b"));
    b.ensure_repository().unwrap();
    let mirrored = b.auth_dir().join("a.json");
    assert_eq!(std::fs::read_to_string(&mirrored).unwrap(), "{\"a\":1}");
    assert_eq!(mode(&mirrored), 0o600, "secrets are written 0600");
    std::fs::write(b.auth_dir().join("b.json"), "{\"b\":1}").unwrap();
    b.persist_auth_files("Sync auth b.json", &[b.auth_dir().join("b.json")])
        .unwrap();

    // A pulls B's file before committing its own config change.
    std::fs::write(a.config_path(), "port: 8317\n").unwrap();
    a.persist_config().unwrap();
    let (subject, _, files) = remote_tip(&remote, "main");
    assert_eq!(subject, "Update config");
    for path in ["auths/a.json", "auths/b.json", "config/config.yaml"] {
        assert!(files.contains(&path.to_owned()), "{path} in {files:?}");
    }
    assert_eq!(
        std::fs::read_to_string(a.auth_dir().join("b.json")).unwrap(),
        "{\"b\":1}"
    );
    assert_eq!(
        remote_file(&remote, "main", "config/config.yaml").unwrap(),
        "port: 8317\n"
    );
}

#[test]
fn local_changes_survive_unless_the_remote_touched_them() {
    let dir = scratch("reconcile");
    let remote = bare_remote(&dir);
    let a = GitStore::new(&remote, "", "", "main", &dir.join("a"));
    a.ensure_repository().unwrap();
    std::fs::write(a.auth_dir().join("a.json"), "{\"v\":1}").unwrap();
    a.persist_auth_files("Sync auth a.json", &[a.auth_dir().join("a.json")])
        .unwrap();
    let b = GitStore::new(&remote, "", "", "main", &dir.join("b"));
    b.ensure_repository().unwrap();

    // An untracked local file is kept while the remote change arrives.
    std::fs::write(b.auth_dir().join("c.json"), "{\"c\":1}").unwrap();
    std::fs::write(a.auth_dir().join("a.json"), "{\"v\":2}").unwrap();
    a.persist_auth_files("Sync auth a.json", &[a.auth_dir().join("a.json")])
        .unwrap();
    b.ensure_repository().unwrap();
    assert_eq!(
        std::fs::read_to_string(b.auth_dir().join("a.json")).unwrap(),
        "{\"v\":2}"
    );
    assert_eq!(
        std::fs::read_to_string(b.auth_dir().join("c.json")).unwrap(),
        "{\"c\":1}"
    );

    // A local edit to a file the remote also changed is a conflict, not a silent loss.
    std::fs::write(b.auth_dir().join("a.json"), "{\"v\":\"local\"}").unwrap();
    std::fs::write(a.auth_dir().join("a.json"), "{\"v\":3}").unwrap();
    a.persist_auth_files("Sync auth a.json", &[a.auth_dir().join("a.json")])
        .unwrap();
    let error = format!("{:#}", b.ensure_repository().unwrap_err());
    assert!(
        error.contains("remote path auths/a.json conflicts with local change auths/a.json"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(b.auth_dir().join("a.json")).unwrap(),
        "{\"v\":\"local\"}"
    );

    // A tracked file deleted locally comes back from the repository.
    std::fs::remove_file(a.auth_dir().join("a.json")).unwrap();
    a.ensure_repository().unwrap();
    assert_eq!(
        std::fs::read_to_string(a.auth_dir().join("a.json")).unwrap(),
        "{\"v\":3}"
    );
}

#[test]
fn watcher_removals_are_refused_and_explicit_deletes_commit() {
    let dir = scratch("delete");
    let remote = bare_remote(&dir);
    let a = GitStore::new(&remote, "", "", "main", &dir.join("a"));
    a.ensure_repository().unwrap();
    let path = a.auth_dir().join("a.json");
    std::fs::write(&path, "{}").unwrap();
    a.persist_auth_files("Sync auth a.json", std::slice::from_ref(&path))
        .unwrap();

    std::fs::remove_file(&path).unwrap();
    let error = format!(
        "{:#}",
        a.persist_auth_files("Remove auth a.json", std::slice::from_ref(&path))
            .unwrap_err()
    );
    assert!(
        error.contains("refusing watcher-originated removal of tracked auth auths/a.json"),
        "{error}"
    );
    assert!(remote_file(&remote, "main", "auths/a.json").is_some());

    a.delete(&path).unwrap();
    assert!(!path.exists());
    let (subject, _, files) = remote_tip(&remote, "main");
    assert_eq!(subject, format!("Delete auth {}", path.display()));
    assert!(!files.contains(&"auths/a.json".to_owned()));
    // After the explicit delete the watcher's removal event is a no-op.
    a.persist_auth_files("Remove auth a.json", std::slice::from_ref(&path))
        .unwrap();
}

#[test]
fn credentials_never_appear_in_errors() {
    let dir = scratch("secret");
    // Nothing listens on port 1: the clone fails quickly.
    let store = GitStore::new(
        "http://127.0.0.1:1/tokens.git",
        "user",
        "tok-s3cret",
        "",
        &dir.join("a"),
    );
    let error = format!("{:#}", store.ensure_repository().unwrap_err());
    assert!(error.contains("git token store: clone remote"), "{error}");
    assert!(!error.contains("tok-s3cret"), "{error}");
    use base64::Engine;
    let basic = base64::engine::general_purpose::STANDARD.encode("user:tok-s3cret");
    assert!(!error.contains(&basic), "{error}");
}

/// The management delete runs on a blocking thread holding the disk lock; the store
/// must finish it there. With one blocking thread, a delete that queued another
/// blocking task would never complete.
#[test]
fn explicit_delete_needs_no_second_blocking_thread() {
    use cpa_server::persist::StorePersister;
    let dir = scratch("delete-pool");
    let remote = bare_remote(&dir);
    let store = std::sync::Arc::new(GitStore::new(&remote, "", "", "main", &dir.join("a")));
    store.ensure_repository().unwrap();
    let path = store.auth_dir().join("a.json");
    std::fs::write(&path, "{}").unwrap();
    store
        .persist_auth_files("Sync auth a.json", std::slice::from_ref(&path))
        .unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let persister = cpa_store::GitPersister(store);
    let deleted = runtime.block_on(async move {
        let task = tokio::task::spawn_blocking(move || persister.delete_auth(path));
        tokio::time::timeout(std::time::Duration::from_secs(20), task).await
    });
    deleted.expect("delete deadlocked").unwrap().unwrap();
    assert!(remote_file(&remote, "main", "auths/a.json").is_none());
}

// ---- corruption recovery (Go gitstore_test.go TestGitTokenStoreCorruption*) ----------

/// Go `removeHeadFileObject`: commits `path` locally, then deletes its blob object.
fn remove_head_file_object(repo: &Path, path: &str) {
    std::fs::write(repo.join(path), "corrupt me\n").unwrap();
    git(repo, &["add", "--", path]);
    git(
        repo,
        &[
            "-c",
            "user.name=CLIProxyAPI",
            "-c",
            "user.email=cliproxy@local",
            "commit",
            "-q",
            "-m",
            "Add corruption marker",
        ],
    );
    let blob = git(repo, &["rev-parse", &format!("HEAD:{path}")]);
    std::fs::remove_file(repo.join(".git/objects").join(&blob[..2]).join(&blob[2..])).unwrap();
}

/// Deletes the object of tracked `path` from the tip the remote shares: the clone's
/// packs are loosened first so that one object can go.
fn drop_shared_object(repo: &Path, path: &str) {
    let blob = git(repo, &["rev-parse", &format!("HEAD:{path}")]);
    for entry in std::fs::read_dir(repo.join(".git/objects/pack")).unwrap() {
        let pack = entry.unwrap().path();
        if !pack.extension().is_some_and(|e| e == "pack") {
            continue;
        }
        let data = std::fs::read(&pack).unwrap();
        for ext in ["pack", "idx", "rev"] {
            let _ = std::fs::remove_file(pack.with_extension(ext));
        }
        let mut child = Command::new("git")
            .current_dir(repo)
            .args(["unpack-objects", "-q"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), &data).unwrap();
        assert!(child.wait().unwrap().success());
    }
    std::fs::remove_file(repo.join(".git/objects").join(&blob[..2]).join(&blob[2..])).unwrap();
}

/// Go `corruptGitRepository`: every object gone (loose ones and packs).
fn corrupt_git_repository(repo: &Path) {
    git(repo, &["repack", "-a", "-d", "-q"]);
    let objects = repo.join(".git/objects");
    for entry in std::fs::read_dir(&objects).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().unwrap().is_dir() && name.len() == 2 {
            std::fs::remove_dir_all(entry.path()).unwrap();
        }
    }
    let mut packs = 0;
    for entry in std::fs::read_dir(objects.join("pack")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "pack") {
            std::fs::remove_file(path).unwrap();
            packs += 1;
        }
    }
    assert!(packs > 0, "no packfiles found to corrupt");
}

/// Whether HEAD's commit, tree and files can all be read.
fn head_readable(repo: &Path) -> bool {
    Command::new("git")
        .current_dir(repo)
        .args(["archive", "--format=tar", "HEAD"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// No recovery directory is left beside the repository.
fn no_recovery_leftovers(dir: &Path) {
    let left: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().to_string_lossy().into_owned();
            name.starts_with(".gitstore-recovery-").then_some(name)
        })
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// An owner holding `auths/victim.json` (`remote-old`) and a workspace cloned after it.
fn owner_and_workspace(name: &str) -> (PathBuf, String, GitStore, GitStore) {
    let dir = scratch(name);
    let remote = bare_remote(&dir);
    let owner = GitStore::new(&remote, "", "", "main", &dir.join("owner"));
    owner.ensure_repository().unwrap();
    let victim = owner.auth_dir().join("victim.json");
    std::fs::write(&victim, r#"{"access_token":"remote-old"}"#).unwrap();
    owner.persist_auth_files("Sync auth victim.json", &[victim]).unwrap();
    let workspace = GitStore::new(&remote, "", "", "main", &dir.join("workspace"));
    workspace.ensure_repository().unwrap();
    (dir, remote, owner, workspace)
}

/// Go `TestGitTokenStoreCorruptionRecoveryUsesLatestRemoteAuthTree`: a HEAD object gone
/// missing is recovered from a fresh clone of the latest remote (a modification and a
/// deletion both arrive), and the store keeps working afterwards.
#[test]
fn a_missing_head_object_is_recovered_from_the_latest_remote() {
    for deletion in [false, true] {
        let (dir, remote, owner, workspace) = owner_and_workspace(if deletion { "recover-del" } else { "recover-mod" });
        let owned = owner.auth_dir().join("victim.json");
        if deletion {
            owner.delete(&owned).unwrap();
        } else {
            std::fs::write(&owned, r#"{"access_token":"remote-new"}"#).unwrap();
            owner.persist_auth_files("Sync auth victim.json", &[owned]).unwrap();
        }
        let repo = dir.join("workspace");
        remove_head_file_object(&repo, "corrupt-object.txt");
        assert!(!head_readable(&repo));

        workspace.ensure_repository().unwrap();
        assert!(head_readable(&repo), "the recovered repository is whole");
        no_recovery_leftovers(&dir);
        let victim = workspace.auth_dir().join("victim.json");
        if deletion {
            assert!(!victim.exists());
        } else {
            assert_eq!(
                std::fs::read_to_string(&victim).unwrap(),
                r#"{"access_token":"remote-new"}"#
            );
            assert_eq!(mode(&victim), 0o600);
        }
        assert!(!repo.join("corrupt-object.txt").exists(), "the unpushed commit is gone");

        let unrelated = workspace.auth_dir().join("unrelated.json");
        std::fs::write(&unrelated, r#"{"access_token":"local"}"#).unwrap();
        workspace
            .persist_auth_files("Sync auth unrelated.json", &[unrelated])
            .unwrap();
        assert!(remote_file(&remote, "main", "auths/unrelated.json").is_some());
        assert_eq!(remote_file(&remote, "main", "auths/victim.json").is_some(), !deletion);
    }
}

/// Go `verifyRepositoryHead` before the pull: an object missing from the tip the remote
/// shares survives any pull, so only a recovery makes the repository whole again.
#[test]
fn a_missing_shared_object_is_recovered() {
    let (dir, remote, _owner, workspace) = owner_and_workspace("recover-shared");
    let repo = dir.join("workspace");
    drop_shared_object(&repo, "auths/victim.json");
    assert!(!head_readable(&repo));
    workspace.ensure_repository().unwrap();
    assert!(head_readable(&repo), "the recovered repository is whole");
    no_recovery_leftovers(&dir);
    let victim = workspace.auth_dir().join("victim.json");
    std::fs::write(&victim, r#"{"access_token":"after"}"#).unwrap();
    workspace
        .persist_auth_files("Sync auth victim.json", std::slice::from_ref(&victim))
        .unwrap();
    assert_eq!(
        remote_file(&remote, "main", "auths/victim.json").unwrap(),
        r#"{"access_token":"after"}"#
    );
}

/// Go `TestGitTokenStoreCorruptionRecoveryPreservesOnlyNonConflictingLocalChanges`.
#[test]
fn recovery_keeps_local_edits_unless_the_remote_changed_them() {
    // A local config edit survives; the remote's auth change arrives.
    let (dir, _remote, owner, workspace) = owner_and_workspace("recover-keep");
    std::fs::write(workspace.config_path(), "source: local\n").unwrap();
    let owned = owner.auth_dir().join("victim.json");
    std::fs::write(&owned, r#"{"access_token":"remote-new"}"#).unwrap();
    owner.persist_auth_files("Sync auth victim.json", &[owned]).unwrap();
    remove_head_file_object(&dir.join("workspace"), "corrupt-object.txt");
    workspace.ensure_repository().unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.config_path()).unwrap(),
        "source: local\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.auth_dir().join("victim.json")).unwrap(),
        r#"{"access_token":"remote-new"}"#
    );

    // The same path changed on both sides: recovery fails closed and touches nothing.
    let (dir, remote, owner, workspace) = owner_and_workspace("recover-conflict");
    let victim = workspace.auth_dir().join("victim.json");
    std::fs::write(&victim, r#"{"access_token":"local"}"#).unwrap();
    let owned = owner.auth_dir().join("victim.json");
    std::fs::write(&owned, r#"{"access_token":"remote-new"}"#).unwrap();
    owner.persist_auth_files("Sync auth victim.json", &[owned]).unwrap();
    remove_head_file_object(&dir.join("workspace"), "corrupt-object.txt");
    let error = format!("{:#}", workspace.ensure_repository().unwrap_err());
    assert!(
        error.contains(
            "remote path auths/victim.json conflicts with local change auths/victim.json during repository recovery"
        ),
        "{error}"
    );
    assert!(error.contains("verify repository before pull"), "{error}");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), r#"{"access_token":"local"}"#);
    assert_eq!(
        remote_file(&remote, "main", "auths/victim.json").unwrap(),
        r#"{"access_token":"remote-new"}"#
    );
    no_recovery_leftovers(&dir);
}

/// Go `recoverRepositoryLocked` restores the worktree after any failure to install the
/// recovered `.git`, including the first step, moving the corrupt one aside. Moving a
/// directory to a new parent needs write permission on it, so a read-only `.git`
/// fails that step after the worktree was already moved to the backup.
#[test]
fn a_failed_git_directory_swap_puts_the_worktree_back() {
    let (dir, _remote, _owner, workspace) = owner_and_workspace("recover-swap");
    let repo = dir.join("workspace");
    // Root ignores directory permissions; there is nothing to test then.
    let probe = dir.join("probe");
    std::fs::create_dir_all(probe.join("inner")).unwrap();
    std::fs::set_permissions(probe.join("inner"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let enforced = std::fs::rename(probe.join("inner"), dir.join("inner")).is_err();
    if !enforced {
        // CI sets CPA_TEST_NO_SKIP: a run as root must not pass without the check.
        assert!(
            std::env::var_os("CPA_TEST_NO_SKIP").is_none(),
            "directory permissions are not enforced (running as root?), and CPA_TEST_NO_SKIP is set"
        );
        return;
    }
    std::fs::write(workspace.config_path(), "source: local\n").unwrap();
    remove_head_file_object(&repo, "corrupt-object.txt");
    std::fs::set_permissions(repo.join(".git"), std::fs::Permissions::from_mode(0o555)).unwrap();

    let error = format!("{:#}", workspace.ensure_repository().unwrap_err());
    std::fs::set_permissions(repo.join(".git"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(error.contains("backup corrupt git directory"), "{error}");
    assert_eq!(
        std::fs::read_to_string(workspace.config_path()).unwrap(),
        "source: local\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.auth_dir().join("victim.json")).unwrap(),
        r#"{"access_token":"remote-old"}"#
    );
    no_recovery_leftovers(&dir);
}

/// Go `TestGitTokenStoreFullPackfileCorruptionFailsClosedWithDirtyManagedFile` and
/// `TestGitTokenStoreMissingPackfileRecoveryFailsClosedWithoutBaseline`: without a
/// readable HEAD commit there is no baseline to recover from; the store fails closed
/// and neither the local files nor the remote change.
#[test]
fn a_lost_head_commit_fails_closed() {
    let (dir, remote, _owner, workspace) = owner_and_workspace("recover-lost");
    std::fs::write(workspace.config_path(), "source: remote\n").unwrap();
    workspace.persist_config().unwrap();
    std::fs::write(workspace.config_path(), "source: local-dirty\n").unwrap();
    corrupt_git_repository(&dir.join("workspace"));
    let error = format!("{:#}", workspace.persist_config().unwrap_err());
    assert!(error.contains("inspect recovery baseline"), "{error}");
    assert_eq!(
        std::fs::read_to_string(workspace.config_path()).unwrap(),
        "source: local-dirty\n"
    );
    assert_eq!(
        remote_file(&remote, "main", "config/config.yaml").unwrap(),
        "source: remote\n"
    );

    let (dir, remote, _owner, workspace) = owner_and_workspace("recover-lost-deleted");
    corrupt_git_repository(&dir.join("workspace"));
    let victim = workspace.auth_dir().join("victim.json");
    std::fs::remove_file(&victim).unwrap();
    let error = format!("{:#}", workspace.ensure_repository().unwrap_err());
    assert!(error.contains("inspect recovery baseline"), "{error}");
    assert!(!victim.exists(), "a local deletion is not undone");
    assert!(remote_file(&remote, "main", "auths/victim.json").is_some());
    no_recovery_leftovers(&dir);
}

/// A remote that refuses every push until `allow` is called.
fn refuse_pushes(remote: &str) -> impl Fn() {
    let hook = Path::new(remote).join("hooks/pre-receive");
    std::fs::write(&hook, "#!/bin/sh\necho refused by test >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    move || std::fs::remove_file(&hook).unwrap()
}

/// Go `commitAndPushWithOptionsLocked` with a rejected push (a stale lease or a hook):
/// the branch and index go back to the base, so the next persist commits everything
/// again on top of the remote.
#[test]
fn a_rejected_push_restores_the_branch() {
    let dir = scratch("rejected");
    let remote = bare_remote(&dir);
    let a = GitStore::new(&remote, "", "", "main", &dir.join("a"));
    a.ensure_repository().unwrap();
    let repo = dir.join("a");
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let allow = refuse_pushes(&remote);
    let path = a.auth_dir().join("a.json");
    std::fs::write(&path, "{}").unwrap();
    let error = format!(
        "{:#}",
        a.persist_auth_files("Sync auth a.json", std::slice::from_ref(&path))
            .unwrap_err()
    );
    assert!(error.contains("git token store: push"), "{error}");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base, "HEAD is restored");
    assert_eq!(
        git(&repo, &["status", "--porcelain", "--untracked-files=all"]),
        "?? auths/a.json",
        "the index is restored and the file kept"
    );
    allow();
    a.persist_auth_files("Sync auth a.json", std::slice::from_ref(&path))
        .unwrap();
    assert_eq!(remote_file(&remote, "main", "auths/a.json").unwrap(), "{}");
}

/// Two nodes initializing one empty remote: the loser's push fails, and its next call
/// adopts the winner's history instead of failing on unrelated histories.
#[test]
fn a_lost_initialization_race_adopts_the_winner() {
    let dir = scratch("init-race");
    let remote = bare_remote(&dir);
    let loser = GitStore::new(&remote, "", "", "main", &dir.join("loser"));
    let allow = refuse_pushes(&remote);
    assert!(loser.ensure_repository().is_err());
    allow();
    let winner = GitStore::new(&remote, "", "", "main", &dir.join("winner"));
    winner.ensure_repository().unwrap();
    let path = winner.auth_dir().join("w.json");
    std::fs::write(&path, "{\"w\":1}").unwrap();
    winner.persist_auth_files("Sync auth w.json", &[path]).unwrap();

    loser.ensure_repository().unwrap();
    assert_eq!(
        std::fs::read_to_string(loser.auth_dir().join("w.json")).unwrap(),
        "{\"w\":1}"
    );
    let path = loser.auth_dir().join("l.json");
    std::fs::write(&path, "{\"l\":1}").unwrap();
    loser.persist_auth_files("Sync auth l.json", &[path]).unwrap();
    let (_, parents, files) = remote_tip(&remote, "main");
    assert_eq!(parents, "");
    assert!(files.contains(&"auths/w.json".to_owned()) && files.contains(&"auths/l.json".to_owned()));
}

/// Go `Delete` of an auth that is already gone: nothing to commit, nothing pushed.
#[test]
fn deleting_twice_is_a_no_op() {
    let dir = scratch("delete-twice");
    let remote = bare_remote(&dir);
    let a = GitStore::new(&remote, "", "", "main", &dir.join("a"));
    a.ensure_repository().unwrap();
    let path = a.auth_dir().join("a.json");
    std::fs::write(&path, "{}").unwrap();
    a.persist_auth_files("Sync auth a.json", std::slice::from_ref(&path))
        .unwrap();
    a.delete(&path).unwrap();
    let tip = git(Path::new(&remote), &["rev-parse", "main"]);
    a.delete(&path).unwrap();
    assert_eq!(git(Path::new(&remote), &["rev-parse", "main"]), tip);
}
