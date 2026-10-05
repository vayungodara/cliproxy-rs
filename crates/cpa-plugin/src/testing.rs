//! Test support shared by this crate's tests and the server's plugin tests (feature
//! `test-support`): builds the Go test plugins in `tests/goplugins` (CLIProxyAPI
//! examples at 6fecc6e plus the recorder) as c-shared libraries.
//!
//! The build never touches the network (see [`go_command`]): modules come from the local
//! cache only, so run `go mod download` in `tests/goplugins` once. It needs Go 1.26 or
//! newer, looked up in `$HOME/sdk/go*/bin` first, then on `PATH`, then
//! `/usr/local/go/bin`. Without one, each native test prints a SKIPPED line naming
//! itself and returns, so a skip is visible in the test output.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The minimum Go version: `tests/goplugins/go.mod` says `go 1.26.0`.
const MIN_GO: (u32, u32) = (1, 26);

/// A `go` command that cannot reach the network: no toolchain download, no module proxy,
/// no direct fetch for private patterns (`GONOPROXY` would otherwise default to an
/// inherited `GOPRIVATE`), no checksum database, and telemetry off in a test-owned
/// directory so an opted-in user's uploader never starts.
pub fn go_command(go: &Path) -> Command {
    let telemetry = std::env::temp_dir().join(format!("cpa-plugin-go-telemetry-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&telemetry);
    let _ = std::fs::write(telemetry.join("mode"), "off");
    let mut cmd = Command::new(go);
    cmd.env("GOTOOLCHAIN", "local")
        .env("GOPROXY", "off")
        .env("GONOPROXY", "none")
        .env("GOPRIVATE", "")
        .env("GOSUMDB", "off")
        .env("GOFLAGS", "-mod=readonly")
        .env("TEST_TELEMETRY_DIR", telemetry);
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

/// The Go module with the test plugins (`tests/goplugins` in this crate).
pub fn goplugins_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/goplugins")
}

/// `<tmp>/goplugins/<example>.so` for every example, built once per process. `None`
/// (after printing a SKIPPED line for `test`) when no suitable Go is installed. `tmp`
/// is the calling test crate's `CARGO_TARGET_TMPDIR`.
pub fn built_plugins(tmp: &Path, test: &str) -> Option<&'static Path> {
    static BUILT: OnceLock<Option<PathBuf>> = OnceLock::new();
    let built = BUILT
        .get_or_init(|| {
            let go = go_binary()?;
            let src = goplugins_dir();
            let out = tmp.join("goplugins");
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
        // CI sets CPA_TEST_NO_SKIP: there a missing Go toolchain fails the test. A skip
        // line alone is not enough, because the harness captures test output and a
        // passing test's output is never shown.
        assert!(
            std::env::var_os("CPA_TEST_NO_SKIP").is_none(),
            "cpa-plugin {test}: no Go {}.{}+ toolchain in $HOME/sdk/go*/bin, PATH or /usr/local/go/bin, \
             and CPA_TEST_NO_SKIP is set",
            MIN_GO.0,
            MIN_GO.1
        );
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

/// A fresh scratch directory under `tmp`.
pub fn scratch(tmp: &Path, name: &str) -> PathBuf {
    let dir = tmp.join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The raw HTTP/1.1 upstream the `host.http.*` goldens use (the Go generator runs the
/// same server, `reference/pluginhost/upstream.go`): `/echo` answers 200 with the exact
/// request bytes received (this server's address written as `UPSTREAM`), `/stream` sends "one", "two", "three" as chunks 50 ms apart,
/// anything else is an empty 404. Every response closes the connection and has no
/// Date. Returns `host:port`; the server runs until the runtime ends.
pub async fn raw_upstream() -> String {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let own = addr.clone();
    tokio::spawn(async move {
        loop {
            let Ok((conn, _)) = listener.accept().await else {
                return;
            };
            let own = own.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(conn);
                let mut head = Vec::new();
                let (mut content_length, mut chunked) = (0usize, false);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.extend_from_slice(line.as_bytes());
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    if trimmed.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = trimmed.split_once(':') {
                        match name.trim().to_ascii_lowercase().as_str() {
                            "content-length" => content_length = value.trim().parse().unwrap_or(0),
                            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
                            _ => {}
                        }
                    }
                }
                let mut body = Vec::new();
                if chunked {
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        body.extend_from_slice(line.as_bytes());
                        if line.trim_end_matches(['\r', '\n']) == "0" {
                            let mut end = String::new();
                            let _ = reader.read_line(&mut end).await;
                            body.extend_from_slice(end.as_bytes());
                            break;
                        }
                    }
                } else if content_length > 0 {
                    body.resize(content_length, 0);
                    if reader.read_exact(&mut body).await.is_err() {
                        return;
                    }
                }
                let text = String::from_utf8_lossy(&head).into_owned();
                let path = text.split(' ').nth(1).unwrap_or_default().to_owned();
                let mut conn = reader.into_inner();
                if path.starts_with("/echo") {
                    let mut raw = head;
                    raw.extend_from_slice(&body);
                    // The echo names this server UPSTREAM so it compares across runs.
                    let echo = String::from_utf8_lossy(&raw).replace(&own, "UPSTREAM").into_bytes();
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Multi: a\r\nX-Multi: b\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                        echo.len()
                    );
                    let _ = conn.write_all(reply.as_bytes()).await;
                    let _ = conn.write_all(&echo).await;
                } else if path.starts_with("/stream") {
                    let _ = conn
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n")
                        .await;
                    for chunk in ["one", "two", "three"] {
                        let _ = conn
                            .write_all(format!("{:x}\r\n{chunk}\r\n", chunk.len()).as_bytes())
                            .await;
                        let _ = conn.flush().await;
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    let _ = conn.write_all(b"0\r\n\r\n").await;
                } else if path.starts_with("/slow") {
                    let _ = conn
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n3\r\none\r\n",
                        )
                        .await;
                    let _ = conn.flush().await;
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    let _ = conn.write_all(b"3\r\ntwo\r\n0\r\n\r\n").await;
                } else if let Some(reply) = canned(&path) {
                    let _ = conn.write_all(&reply).await;
                } else if !path.starts_with("/hangup") {
                    let _ = conn
                        .write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
                        .await;
                }
                let _ = conn.shutdown().await;
            });
        }
    });
    addr
}

/// The raw upstream's fixed replies (tests/reference/pluginhost/upstream.go).
fn canned(path: &str) -> Option<Vec<u8>> {
    let reply: &[u8] = if path.starts_with("/truncated") {
        b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 10\r\n\r\nabc"
    } else if path.starts_with("/http10") {
        b"HTTP/1.0 200 OK\r\nConnection: close\r\nTrailer: X-T\r\nContent-Length: 2\r\n\r\nok"
    } else if path.starts_with("/redirect") {
        b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/after\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    } else if path.starts_with("/trailer") {
        b"HTTP/1.1 200 OK\r\nConnection: close\r\nTrailer: X-T\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\nX-T: v\r\n\r\n"
    } else {
        return None;
    };
    Some(reply.to_vec())
}

/// Opens a callback context for `plugin_id`'s `instance`, as a capability call does
/// before calling the plugin. The context closes when the guard drops.
pub fn callback_context(
    host: &crate::Host,
    plugin_id: &str,
    instance: std::sync::Arc<crate::client::CallbackInstance>,
    scope: crate::callbacks::RequestScope,
) -> crate::callbacks::ContextGuard {
    // Plugins get the dispatcher when they load; this records the runtime the same way.
    let _ = host.inner.callbacks.handler();
    host.inner.callbacks.open(plugin_id, Some(instance), scope)
}

/// One `host.*` callback as the plugin `plugin_id`'s `instance` makes it. Call it from
/// a blocking thread, as plugins do.
pub fn call_from_plugin(
    host: &crate::Host,
    plugin_id: &str,
    instance: &std::sync::Arc<crate::client::CallbackInstance>,
    method: &str,
    request: &[u8],
) -> Result<bytes::Bytes, crate::client::CallbackError> {
    let caller = crate::callbacks::Caller {
        plugin_id: plugin_id.to_owned(),
        instance: instance.clone(),
    };
    host.call_from_plugin(&caller, method, request)
}

/// A zip archive of stored (uncompressed) entries, like Go's `zip.Writer` with
/// `zip.Store`: `(name, data, mode)`, where a non-zero mode is recorded as Unix
/// permission bits of a regular file (`FileHeader.SetMode`).
pub fn stored_zip(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    fn u16le(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn u32le(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data, mode) in entries {
        let mut crc = flate2::Crc::new();
        crc.update(data);
        let (crc, size, offset) = (crc.sum(), data.len() as u32, out.len() as u32);
        u32le(&mut out, 0x0403_4b50);
        for v in [20, 0, 0, 0, 0] {
            u16le(&mut out, v);
        }
        for v in [crc, size, size] {
            u32le(&mut out, v);
        }
        u16le(&mut out, name.len() as u16);
        u16le(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        // Central directory: Unix creator when a mode is set, so readers take the
        // permission bits from the external attributes.
        let creator = if *mode != 0 { (3 << 8) | 20 } else { 20 };
        let external = if *mode != 0 { (0o100000 | mode) << 16 } else { 0 };
        u32le(&mut central, 0x0201_4b50);
        for v in [creator, 20, 0, 0, 0, 0] {
            u16le(&mut central, v);
        }
        for v in [crc, size, size] {
            u32le(&mut central, v);
        }
        for v in [name.len() as u16, 0, 0, 0, 0] {
            u16le(&mut central, v);
        }
        u32le(&mut central, external);
        u32le(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let (central_offset, central_size) = (out.len() as u32, central.len() as u32);
    out.extend_from_slice(&central);
    u32le(&mut out, 0x0605_4b50);
    for v in [0, 0, entries.len() as u16, entries.len() as u16] {
        u16le(&mut out, v);
    }
    u32le(&mut out, central_size);
    u32le(&mut out, central_offset);
    u16le(&mut out, 0);
    out
}
