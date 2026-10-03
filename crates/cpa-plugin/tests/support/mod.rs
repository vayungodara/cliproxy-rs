//! Builds the Go test plugins in `tests/goplugins` (CLIProxyAPI examples at 6fecc6e plus
//! the recorder) as c-shared libraries.
//!
//! The build never touches the network: `GOTOOLCHAIN=local` (no toolchain download) and
//! `GOPROXY=off` (modules from the local cache only; run `go mod download` in
//! `tests/goplugins` once). It needs Go 1.26 or newer, looked up in `$HOME/sdk/go*/bin`
//! first, then on `PATH`, then `/usr/local/go/bin`. Without one, each native test prints
//! a SKIPPED line naming itself and returns, so a skip is visible in the test output.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The minimum Go version: `tests/goplugins/go.mod` says `go 1.26.0`.
const MIN_GO: (u32, u32) = (1, 26);

fn go_command(go: &Path) -> Command {
    let mut cmd = Command::new(go);
    cmd.env("GOTOOLCHAIN", "local")
        .env("GOPROXY", "off")
        .env("GOFLAGS", "-mod=readonly");
    cmd
}

/// `(major, minor)` of a Go binary's own toolchain, if it runs.
fn go_version(go: &Path) -> Option<(u32, u32)> {
    let out = go_command(go).args(["env", "GOVERSION"]).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let mut parts = text.trim().strip_prefix("go")?.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// The first Go toolchain new enough to build the plugins.
pub fn go_binary() -> Option<PathBuf> {
    let mut sdk: Vec<PathBuf> = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| std::fs::read_dir(home.join("sdk")).ok())
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("bin/go"))
        .collect();
    sdk.sort();
    sdk.reverse();
    let path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|dir| dir.join("go"));
    sdk.into_iter()
        .chain(path)
        .chain([PathBuf::from("/usr/local/go/bin/go")])
        .filter(|p| p.is_file())
        .find(|p| go_version(p).is_some_and(|v| v >= MIN_GO))
}

/// Directory with `<example>.so` for every example, built once per test process. `None`
/// (after printing a SKIPPED line for `test`) when no suitable Go is installed.
pub fn built_plugins_for(test: &str) -> Option<&'static Path> {
    static BUILT: OnceLock<Option<PathBuf>> = OnceLock::new();
    let built = BUILT
        .get_or_init(|| {
            let go = go_binary()?;
            let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/goplugins");
            let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("goplugins");
            std::fs::create_dir_all(&out).unwrap();
            for entry in std::fs::read_dir(src.join("examples")).unwrap() {
                let name = entry.unwrap().file_name().to_string_lossy().into_owned();
                let status = go_command(&go)
                    .current_dir(&src)
                    .args(["build", "-buildmode=c-shared", "-o"])
                    .arg(out.join(format!("{name}.so")))
                    .arg(format!("./examples/{name}"))
                    .env("CGO_ENABLED", "1")
                    .status()
                    .expect("run go build");
                assert!(
                    status.success(),
                    "go build of plugin example {name} failed with {}; the build is offline \
                     (GOPROXY=off), so run `go mod download` in {} first if modules are missing",
                    go.display(),
                    src.display()
                );
            }
            Some(out)
        })
        .as_deref();
    if built.is_none() {
        // Straight to the process's stderr: the test harness hides `eprintln!` output
        // of passing tests, and a skip must not look like a real run.
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED: cpa-plugin {test}: no Go {}.{}+ toolchain in $HOME/sdk/go*/bin, PATH or /usr/local/go/bin; \
             the Go plugin comparison did not run",
            MIN_GO.0,
            MIN_GO.1
        );
    }
    built
}

/// A fresh scratch directory under the test target dir.
pub fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
