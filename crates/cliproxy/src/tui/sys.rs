//! The desktop helpers Go's TUI calls: `openBrowser` (internal/tui/browser.go) and
//! `clipboard.WriteAll` (github.com/atotto/clipboard v0.1.4).
use std::io::Write;
use std::process::{Command, Stdio};

/// Go `openBrowser`: starts the platform opener without waiting for it.
pub fn open_browser(url: &str) -> std::io::Result<()> {
    let mut cmd = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        Command::new("xdg-open")
    };
    cmd.arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// atotto's copy command: wl-copy under Wayland (when wl-paste exists too), then
/// xclip, xsel, termux-clipboard-set and clip.exe; pbcopy on macOS.
fn copy_command() -> Option<Vec<&'static str>> {
    if cfg!(target_os = "macos") {
        return Some(vec!["pbcopy"]);
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty()) && on_path("wl-copy") && on_path("wl-paste") {
        return Some(vec!["wl-copy"]);
    }
    [
        vec!["xclip", "-in", "-selection", "clipboard"],
        vec!["xsel", "--input", "--clipboard"],
        vec!["termux-clipboard-set"],
        vec!["clip.exe"],
    ]
    .into_iter()
    .find(|args| on_path(args[0]))
}

/// Go `clipboard.WriteAll`.
pub fn write_clipboard(text: &str) -> Result<(), String> {
    let Some(args) = copy_command() else {
        return Err(
            "No clipboard utilities available. Please install xsel, xclip, wl-clipboard or Termux:API add-on for termux-clipboard-get/set."
                .into(),
        );
    };
    let mut child = Command::new(args[0])
        .args(&args[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("exit status {}", status.code().unwrap_or(-1)))
    }
}
