//! `--working-dir` in the real binary: the directory changes before `.env`, the default
//! config and relative paths are read, and `--log-file` replaces only stdout, so
//! `logging-to-file` still writes `main.log`. Runs a short import command; no network.
use std::process::Command;

#[test]
fn working_dir_applies_before_dotenv_config_and_log_paths() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("cliproxy-wd-{}-{nanos}", std::process::id()));
    let work = root.join("work");
    std::fs::create_dir_all(work.join("auth")).unwrap();
    std::fs::write(
        work.join("config.yaml"),
        "config-version: 8\nport: 0\nauth-dir: ./auth\nobservability:\n  logs:\n    logging-to-file: true\n",
    )
    .unwrap();
    std::fs::write(work.join(".env"), "WRITABLE_PATH=from-dotenv\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cliproxy"))
        .args([
            "-local-model",
            "--working-dir",
            "work",
            "--log-file",
            "process.log",
            "-vertex-import",
            "missing-key.json",
        ])
        .current_dir(&root)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &work)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    // Relative to the working directory, not the caller's.
    assert!(work.join("process.log").is_file(), "{stderr}");
    assert!(!root.join("process.log").exists());
    // `.env` from the working directory chose the log directory, the default config
    // there turned on `logging-to-file`, and the import error went to main.log.
    let main_log = std::fs::read_to_string(work.join("from-dotenv/logs/main.log")).unwrap_or_default();
    assert!(
        main_log.contains("missing-key.json"),
        "main.log: {main_log}\nstderr: {stderr}"
    );
    let process_log = std::fs::read_to_string(work.join("process.log")).unwrap();
    assert!(!process_log.contains("missing-key.json"), "{process_log}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_working_dir_fails_before_anything_runs() {
    let output = Command::new(env!("CARGO_BIN_EXE_cliproxy"))
        .args(["-working-dir=/nonexistent/cliproxy-wd", "-vertex-import", "x.json"])
        .current_dir(std::env::temp_dir())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("working directory"), "{stderr}");
}
