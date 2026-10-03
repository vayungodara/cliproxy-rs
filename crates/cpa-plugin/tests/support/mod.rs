//! Builds the Go test plugins in `tests/goplugins` (CLIProxyAPI examples at 6fecc6e plus
//! the recorder) as c-shared libraries. Needs a Go toolchain: `go` on PATH,
//! `$HOME/sdk/go*/bin/go` or `/usr/local/go/bin/go`. Without one the native tests print
//! a notice and pass, so the workspace still tests on machines without Go.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub fn go_binary() -> Option<PathBuf> {
    let candidates = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|dir| dir.join("go"))
        .chain(
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .and_then(|home| std::fs::read_dir(home.join("sdk")).ok())
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.path().join("bin/go")),
        )
        .chain([PathBuf::from("/usr/local/go/bin/go")]);
    candidates.into_iter().find(|p| p.is_file())
}

/// Directory with `<example>.so` for every example, built once per test process.
pub fn built_plugins() -> Option<&'static Path> {
    static BUILT: OnceLock<Option<PathBuf>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let Some(go) = go_binary() else {
                eprintln!("skipping native plugin tests: no Go toolchain found");
                return None;
            };
            let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/goplugins");
            let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("goplugins");
            std::fs::create_dir_all(&out).unwrap();
            for entry in std::fs::read_dir(src.join("examples")).unwrap() {
                let name = entry.unwrap().file_name().to_string_lossy().into_owned();
                let status = Command::new(&go)
                    .current_dir(&src)
                    .args(["build", "-buildmode=c-shared", "-o"])
                    .arg(out.join(format!("{name}.so")))
                    .arg(format!("./examples/{name}"))
                    .env("CGO_ENABLED", "1")
                    .status()
                    .expect("run go build");
                assert!(status.success(), "go build of plugin example {name} failed");
            }
            Some(out)
        })
        .as_deref()
}

/// A fresh scratch directory under the test target dir.
pub fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
