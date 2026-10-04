//! The binary in remote-store mode (Go cmd/server/main.go's store branches), against a
//! local bare git repository and a closed local port. No network beyond loopback.
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn scratch(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("cliproxy-store-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // The binary resolves its store root from the canonical working directory
    // (macOS temp paths sit behind the /var -> /private/var symlink).
    dir.canonicalize().unwrap()
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").current_dir(dir).args(args).output().unwrap();
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The child process, killed and reaped even when an assertion fails first.
struct Server(Child);

impl std::ops::Deref for Server {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl std::ops::DerefMut for Server {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The binary with a clean environment: only what the store selection needs.
/// `-local-model` keeps the model catalog updaters from fetching remote URLs.
fn cliproxy(wd: &Path, env: &[(&str, &str)]) -> Server {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cliproxy"));
    cmd.arg("-local-model")
        .current_dir(wd)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", wd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        cmd.env(key, value);
    }
    Server(cmd.spawn().unwrap())
}

/// Lines from stdout and stderr as they arrive.
fn lines(child: &mut Child) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    for stream in [Box::new(out) as Box<dyn std::io::Read + Send>, Box::new(err)] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
    }
    rx
}

fn wait_line(rx: &mpsc::Receiver<String>, seen: &mut Vec<String>, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !seen.iter().any(|l| l.contains(needle)) {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => seen.push(line),
            Err(_) => panic!("no line containing {needle:?}; got:\n{}", seen.join("\n")),
        }
    }
}

#[test]
fn git_store_mode_bootstraps_from_the_template_and_pushes_watcher_changes() {
    let wd = scratch("git");
    let remote = wd.join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare"]).unwrap();
    git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]).unwrap();
    // `auth-dir` must be overridden by the store's mirror.
    let template = "host: 127.0.0.1\nport: 0\nauth-dir: /nonexistent/elsewhere\napi-keys: [fake-client]\n";
    std::fs::write(wd.join("config.example.yaml"), template).unwrap();
    let remote_url = remote.to_string_lossy().into_owned();
    let mut child = cliproxy(
        &wd,
        &[("GITSTORE_GIT_URL", &remote_url), ("gitstore_git_branch", "main")],
    );
    let rx = lines(&mut child);
    let mut seen = Vec::new();
    let root = wd.join("gitstore");
    wait_line(
        &rx,
        &mut seen,
        &format!(
            "git-backed config initialized from template: {}",
            root.join("config/config.yaml").display()
        ),
    );
    wait_line(
        &rx,
        &mut seen,
        &format!("git-backed token store enabled, repository path: {}", root.display()),
    );
    wait_line(&rx, &mut seen, "listening");
    // Go's bootstrap: the template is committed as the store's config.
    assert_eq!(
        git(&remote, &["show", "main:config/config.yaml"]).unwrap(),
        template.trim_end()
    );
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%s", "main"]).unwrap(),
        "Update config"
    );

    // A credential dropped into the mirror is loaded and pushed by the watcher. The
    // watcher's first snapshot is its baseline (Go's startup scan persists nothing), so
    // the file is rewritten until the watcher reports a change after that baseline.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut attempt = 0;
    while !seen.iter().any(|l| l.contains("auth file changed (")) {
        assert!(Instant::now() < deadline, "no watcher event; log:\n{}", seen.join("\n"));
        attempt += 1;
        let body = format!(r#"{{"type":"claude","access_token":"fake-{attempt}"}}"#);
        std::fs::write(root.join("auths/a.json"), body).unwrap();
        while let Ok(line) = rx.recv_timeout(Duration::from_millis(500)) {
            seen.push(line);
        }
    }
    while git(&remote, &["show", "main:auths/a.json"]).is_none() {
        assert!(
            Instant::now() < deadline,
            "auths/a.json never pushed; log:\n{}",
            seen.join("\n")
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%s", "main"]).unwrap(),
        "Sync auth a.json"
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_failing_store_logs_go_s_line_and_starts_nothing() {
    let wd = scratch("pg");
    let mut child = cliproxy(
        &wd,
        &[
            // Postgres wins over git, as in Go; nothing listens on port 1.
            (
                "PGSTORE_DSN",
                "postgres://fake:fake-password@127.0.0.1:1/db?sslmode=disable&connect_timeout=2",
            ),
            ("GITSTORE_GIT_URL", "/nonexistent"),
        ],
    );
    let rx = lines(&mut child);
    let status = child.wait().unwrap();
    // Both readers end at EOF once the process has exited.
    let output: Vec<String> = rx.iter().collect();
    let text = output.join("\n");
    assert!(status.success(), "Go returns from main: {text}");
    assert!(
        text.contains("failed to initialize postgres token store: postgres store: ping database: "),
        "{text}"
    );
    assert!(!text.contains("fake-password"), "{text}");
    assert!(!text.contains("listening"), "{text}");
    assert!(
        wd.join("pgstore/auths").is_dir(),
        "Go creates the spool before connecting"
    );
}
