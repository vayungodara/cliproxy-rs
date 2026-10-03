//! `GITSTORE_*` against local bare repositories: two nodes sharing one remote, as two
//! CLIProxyAPI instances share a token repository.

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
