//! The process log (Go internal/logging/global_logger.go, log_dir_cleaner.go and
//! `util.SetLogLevel`): Go's line format, stdout or a rotating `main.log`, the
//! log-directory size cleaner and the `debug` level switch.
//!
//! [`init`] installs the subscriber once; [`configure`] applies a config snapshot
//! and acts only on the settings that changed, as Go's reload does.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use cpa_core::config::Config;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, MakeWriter};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, reload};

/// lumberjack `MaxSize: 10` (megabytes of 1024 * 1024 bytes).
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;
const MAIN_LOG: &str = "main.log";
const CLEANER_INTERVAL: Duration = Duration::from_secs(60);

static OUTPUT: Mutex<Output> = Mutex::new(Output::Stdout);
static STATE: Mutex<Option<Applied>> = Mutex::new(None);
static LEVEL: OnceLock<reload::Handle<LevelFilter, tracing_subscriber::Registry>> = OnceLock::new();
static CLEANER_GENERATION: AtomicU64 = AtomicU64::new(0);
/// Set once [`init`] installed the subscriber; [`configure`] does nothing before.
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// The settings last applied, compared on every [`configure`].
#[derive(Clone, Copy, PartialEq)]
struct Applied {
    logging_to_file: bool,
    max_total_mb: i64,
    debug: bool,
}

enum Output {
    Stdout,
    File(RotatingFile),
}

/// Go `SetupBaseLogger`: Go's line format on stdout at info level. A set `RUST_LOG`
/// replaces the level switch with its filter. Later calls are no-ops.
pub fn init() {
    let filter = match EnvFilter::try_from_default_env() {
        Ok(env) => env.boxed(),
        Err(_) => {
            let (level, handle) = reload::Layer::new(LevelFilter::INFO);
            let _ = LEVEL.set(handle);
            level.boxed()
        }
    };
    let format = tracing_subscriber::fmt::layer()
        .event_format(GoFormat)
        .with_writer(GlobalWriter);
    if tracing_subscriber::registry()
        .with(filter)
        .with(format)
        .try_init()
        .is_ok()
    {
        INSTALLED.store(true, Ordering::SeqCst);
    }
}

/// Go `ConfigureLogOutput` (when `logging-to-file` or `logs-max-total-size-mb`
/// changed) and `SetLogLevel` (when `debug` changed), from the first call on.
/// Without [`init`] (libraries, tests) the process log is not ours to redirect.
pub fn configure(cfg: &Config) {
    if !INSTALLED.load(Ordering::SeqCst) {
        return;
    }
    let next = Applied {
        logging_to_file: logs_setting(cfg, "logging-to-file").and_then(serde_yaml_ng::Value::as_bool) == Some(true),
        max_total_mb: logs_setting(cfg, "logs-max-total-size-mb")
            .and_then(serde_yaml_ng::Value::as_i64)
            .unwrap_or(0),
        debug: logs_setting(cfg, "debug").and_then(serde_yaml_ng::Value::as_bool) == Some(true),
    };
    let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    let previous = *state;
    *state = Some(next);
    drop(state);
    let output_changed =
        previous.is_none_or(|p| p.logging_to_file != next.logging_to_file || p.max_total_mb != next.max_total_mb);
    if output_changed && let Err(error) = configure_output(&resolve_log_dir(cfg), next) {
        tracing::error!("failed to reconfigure log output: {error}");
    }
    if previous.is_none_or(|p| p.debug != next.debug) {
        set_level(next.debug);
    }
}

fn logs_setting<'a>(cfg: &'a Config, key: &str) -> Option<&'a serde_yaml_ng::Value> {
    cfg.document.get("observability")?.get("logs")?.get(key)
}

fn configure_output(dir: &Path, applied: Applied) -> io::Result<()> {
    let mut output = OUTPUT.lock().unwrap_or_else(PoisonError::into_inner);
    let protected = if applied.logging_to_file {
        create_dir(dir)
            .map_err(|e| io::Error::new(e.kind(), format!("logging: failed to create log directory: {e}")))?;
        let path = dir.join(MAIN_LOG);
        *output = Output::File(RotatingFile {
            path: path.clone(),
            file: None,
            size: 0,
        });
        Some(path)
    } else {
        *output = Output::Stdout;
        None
    };
    drop(output);
    start_cleaner(dir, applied.max_total_mb, protected);
    Ok(())
}

/// Go `SetLogLevel`: debug or info, announced when it changes.
fn set_level(enabled: bool) {
    let Some(handle) = LEVEL.get() else { return };
    let next = if enabled { LevelFilter::DEBUG } else { LevelFilter::INFO };
    let Some(current) = handle.clone_current() else { return };
    if current == next {
        return;
    }
    let _ = handle.modify(|level| *level = next);
    let name = |l: LevelFilter| l.to_string().to_lowercase();
    tracing::info!(
        "log level changed from {} to {} (debug={})",
        name(current),
        name(next),
        enabled
    );
}

/// Go `ResolveLogDirectory`: `$WRITABLE_PATH/logs`, else `logs` when it is a writable
/// directory, else `<auth-dir>/logs`.
pub fn resolve_log_dir(cfg: &Config) -> PathBuf {
    for key in ["WRITABLE_PATH", "writable_path"] {
        if let Ok(v) = std::env::var(key)
            && !v.trim().is_empty()
        {
            return Path::new(v.trim()).join("logs");
        }
    }
    let local = Path::new("logs");
    let writable = local.is_dir() && {
        let probe = local.join(".perm_test");
        let ok = File::create(&probe).is_ok();
        let _ = std::fs::remove_file(&probe);
        ok
    };
    if writable || cfg.auth_dir.as_os_str().is_empty() {
        return local.to_owned();
    }
    cfg.auth_dir.join("logs")
}

/// Go `configureLogDirCleanerLocked`: replaces the running cleaner. A positive
/// limit starts one that runs now and every minute.
fn start_cleaner(dir: &Path, max_total_mb: i64, protected: Option<PathBuf>) {
    let generation = CLEANER_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let Some(max_bytes) = u64::try_from(max_total_mb)
        .ok()
        .filter(|&mb| mb > 0)
        .map(|mb| mb * 1024 * 1024)
    else {
        return;
    };
    let dir = dir.to_owned();
    // ponytail: a stopped cleaner notices on its next wake-up, up to a minute later.
    let spawned = std::thread::Builder::new()
        .name("log-dir-cleaner".into())
        .spawn(move || {
            while CLEANER_GENERATION.load(Ordering::SeqCst) == generation {
                match enforce_size_limit(&dir, max_bytes, protected.as_deref()) {
                    Ok(0) => {}
                    Ok(deleted) => tracing::debug!(
                        "logging: removed {deleted} old log file(s) to enforce log directory size limit"
                    ),
                    Err(error) => tracing::warn!("logging: failed to enforce log directory size limit: {error}"),
                }
                std::thread::sleep(CLEANER_INTERVAL);
            }
        });
    if let Err(error) = spawned {
        tracing::warn!("logging: failed to start the log directory cleaner: {error}");
    }
}

/// Go `enforceLogDirSizeLimit`: deletes the oldest `*.log` / `*.log.gz` files (by
/// modification time, never `protected`) until the directory is within `max_bytes`.
pub(crate) fn enforce_size_limit(dir: &Path, max_bytes: u64, protected: Option<&Path>) -> io::Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut files = Vec::new();
    let mut total = 0u64;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().trim().to_lowercase();
        if !(name.ends_with(".log") || name.ends_with(".log.gz")) {
            continue;
        }
        let Ok(info) = entry.metadata() else { continue };
        if !info.is_file() {
            continue;
        }
        total += info.len();
        files.push((info.modified().ok(), info.len(), entry.path()));
    }
    if total <= max_bytes {
        return Ok(0);
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    let mut deleted = 0;
    for (_, size, path) in files {
        if total <= max_bytes {
            break;
        }
        if protected.is_some_and(|p| p == path) {
            continue;
        }
        if let Err(error) = std::fs::remove_file(&path) {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            tracing::warn!("logging: failed to remove old log file: {name}: {error}");
            continue;
        }
        total -= size;
        deleted += 1;
    }
    Ok(deleted)
}

/// lumberjack.Logger with Go's settings: 10 MiB files, rotations kept forever,
/// uncompressed, named in UTC.
pub(crate) struct RotatingFile {
    path: PathBuf,
    file: Option<File>,
    size: u64,
}

impl RotatingFile {
    #[cfg(test)]
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: None,
            size: 0,
        }
    }

    fn write(&mut self, p: &[u8], max: u64) -> io::Result<usize> {
        let len = p.len() as u64;
        if len > max {
            return Err(io::Error::other(format!(
                "write length {len} exceeds maximum file size {max}"
            )));
        }
        if self.file.is_none() {
            self.open_existing_or_new(len, max)?;
        }
        if self.size + len > max {
            self.rotate()?;
        }
        let file = self.file.as_mut().ok_or_else(|| io::Error::other("log file closed"))?;
        let n = file.write(p)?;
        self.size += n as u64;
        Ok(n)
    }

    fn open_existing_or_new(&mut self, len: u64, max: u64) -> io::Result<()> {
        let info = match std::fs::metadata(&self.path) {
            Ok(info) => info,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return self.open_new(),
            Err(e) => return Err(io::Error::new(e.kind(), format!("error getting log file info: {e}"))),
        };
        if info.len() + len >= max {
            return self.rotate();
        }
        match OpenOptions::new().append(true).open(&self.path) {
            Ok(file) => {
                self.file = Some(file);
                self.size = info.len();
                Ok(())
            }
            // lumberjack: an existing file that cannot be opened is replaced.
            Err(_) => self.open_new(),
        }
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        self.open_new()
    }

    fn open_new(&mut self) -> io::Result<()> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        create_dir(dir)
            .map_err(|e| io::Error::new(e.kind(), format!("can't make directories for new logfile: {e}")))?;
        let previous = std::fs::metadata(&self.path).ok();
        if previous.is_some() {
            std::fs::rename(&self.path, backup_name(&self.path, chrono::Utc::now()))
                .map_err(|e| io::Error::new(e.kind(), format!("can't rename log file: {e}")))?;
        }
        let mut options = OpenOptions::new();
        options.create(true).write(true).truncate(true);
        // lumberjack: 0600, or the mode of the file being rotated. Windows has no
        // mode bits (Go's mode is ignored there).
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            options.mode(previous.map_or(0o600, |info| info.permissions().mode() & 0o7777));
        }
        let file = options
            .open(&self.path)
            .map_err(|e| io::Error::new(e.kind(), format!("can't open new logfile: {e}")))?;
        self.file = Some(file);
        self.size = 0;
        Ok(())
    }
}

/// `os.MkdirAll(dir, 0755)`.
fn create_dir(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o755);
    builder.create(dir)
}

/// lumberjack `backupName`: `main.log` -> `main-2006-01-02T15-04-05.000.log`.
fn backup_name(path: &Path, now: chrono::DateTime<chrono::Utc>) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (stem, ext) = match name.rfind('.') {
        Some(i) => (&name[..i], &name[i..]),
        None => (&name[..], ""),
    };
    path.with_file_name(format!("{stem}-{}{ext}", now.format("%Y-%m-%dT%H-%M-%S%.3f")))
}

/// Writes each formatted event, whole, to the current output.
struct GlobalWriter;

struct OutputGuard(MutexGuard<'static, Output>);

impl<'a> MakeWriter<'a> for GlobalWriter {
    type Writer = OutputGuard;

    fn make_writer(&'a self) -> OutputGuard {
        OutputGuard(OUTPUT.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl Write for OutputGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut *self.0 {
            Output::Stdout => io::stdout().write(buf),
            Output::File(file) => file.write(buf, MAX_FILE_SIZE),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut *self.0 {
            Output::Stdout => io::stdout().flush(),
            Output::File(file) => file.file.as_mut().map_or(Ok(()), Write::flush),
        }
    }
}

/// Go `logFieldOrder`: the fields Go prints, in order.
const FIELD_ORDER: &[&str] = &[
    "provider",
    "model",
    "plugin_id",
    "plugin_name",
    "source_id",
    "version",
    "active_version",
    "retired_version",
    "overwritten",
    "mode",
    "budget",
    "level",
    "original_mode",
    "original_value",
    "min",
    "max",
    "clamped_to",
    "error",
    "credential",
    "auth_id",
    "connection",
    "proxy_scheme",
    "remote_transport",
    "media_session_id",
    "call_id",
    "peer",
    "state",
    "reason",
];

/// Go `quotedLogFields`: string values printed with `strconv.Quote`.
const QUOTED_FIELDS: &[&str] = &[
    "credential",
    "auth_id",
    "connection",
    "proxy_scheme",
    "remote_transport",
    "media_session_id",
    "call_id",
    "peer",
    "state",
    "reason",
];

/// Go `LogFormatter`:
/// `[2006-01-02 15:04:05] [reqid8ch] [info ] [file.rs:12] message key=value`.
struct GoFormat;

impl<S, N> FormatEvent<S, N> for GoFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(&self, _: &FmtContext<'_, S, N>, mut writer: Writer<'_>, event: &Event<'_>) -> std::fmt::Result {
        let mut fields = Fields::default();
        event.record(&mut fields);
        writer.write_str(&format_line(
            chrono::Local::now().naive_local(),
            event.metadata().level(),
            event.metadata().file().zip(event.metadata().line()),
            fields,
        ))
    }
}

#[derive(Default)]
struct Fields {
    message: String,
    request_id: String,
    /// Name, value and whether the value was a string, in recording order.
    rest: Vec<(&'static str, String, bool)>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.message = value.to_owned(),
            "request_id" => self.request_id = value.to_owned(),
            name => self.rest.push((name, value.to_owned(), true)),
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => self.message = format!("{value:?}"),
            "request_id" => self.request_id = format!("{value:?}"),
            name => self.rest.push((name, format!("{value:?}"), false)),
        }
    }
}

fn format_line(now: chrono::NaiveDateTime, level: &Level, caller: Option<(&str, u32)>, fields: Fields) -> String {
    let request_id = match fields.request_id.trim() {
        "" => "--------",
        // Go `ShortRequestID`: the trailing 8 bytes.
        id => id.get(id.len().saturating_sub(8)..).unwrap_or(id),
    };
    let level = match *level {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        Level::DEBUG => "debug",
        Level::TRACE => "trace",
    };
    let mut line = format!("[{}] [{request_id}] [{level:<5}] ", now.format("%Y-%m-%d %H:%M:%S"));
    if let Some((file, number)) = caller {
        let base = file.rsplit(['/', '\\']).next().unwrap_or(file);
        line.push_str(&format!("[{base}:{number}] "));
    }
    line.push_str(fields.message.trim_end_matches(['\r', '\n']));
    let mut rest = fields.rest;
    // ponytail: Go prints only the known fields; Rust call sites also log structured
    // context Go writes into the message, so the other fields follow in call order.
    rest.sort_by_key(|(name, _, _)| FIELD_ORDER.iter().position(|k| k == name).unwrap_or(FIELD_ORDER.len()));
    for (name, value, is_str) in rest {
        if is_str && QUOTED_FIELDS.contains(&name) {
            // ponytail: Rust's string escaping stands in for strconv.Quote; they agree
            // on printable ASCII, quotes, backslashes and \n \r \t.
            line.push_str(&format!(" {name}={value:?}"));
        } else {
            line.push_str(&format!(" {name}={value}"));
        }
    }
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the system temp dir, unique per test and process.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cpa-logging-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fields(message: &str, request_id: &str, rest: &[(&'static str, &str, bool)]) -> Fields {
        Fields {
            message: message.into(),
            request_id: request_id.into(),
            rest: rest.iter().map(|(n, v, s)| (*n, (*v).to_owned(), *s)).collect(),
        }
    }

    #[test]
    fn lines_follow_go_log_formatter() {
        let now = chrono::NaiveDate::from_ymd_opt(2025, 12, 23)
            .unwrap()
            .and_hms_opt(20, 14, 4)
            .unwrap();
        // Go: [2025-12-23 20:14:04] [debug] [manager.go:524] ... with request id and
        // padded level; warning prints as "warn ".
        assert_eq!(
            format_line(
                now,
                &Level::WARN,
                Some(("crates/x/src/manager.rs", 524)),
                fields("Use API key\r\n", "0198-aaaa-a1b2c3d4", &[])
            ),
            "[2025-12-23 20:14:04] [a1b2c3d4] [warn ] [manager.rs:524] Use API key\n"
        );
        // Known fields in Go's order (model before auth_id; quoted auth_id), others after.
        assert_eq!(
            format_line(
                now,
                &Level::INFO,
                None,
                fields(
                    "done",
                    "",
                    &[
                        ("credentials", "3", false),
                        ("auth_id", "a b", true),
                        ("model", "m", true)
                    ]
                )
            ),
            "[2025-12-23 20:14:04] [--------] [info ] done model=m auth_id=\"a b\" credentials=3\n"
        );
    }

    #[test]
    fn rotating_file_follows_lumberjack() {
        let dir = scratch("rotate");
        let path = dir.join("logs").join(MAIN_LOG);
        let mut file = RotatingFile::new(path.clone());
        // New file: directory created, mode 0600.
        assert_eq!(file.write(b"12345", 10).unwrap(), 5);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // size + len == max fits; one more byte rotates.
        file.write(b"67890", 10).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"1234567890");
        file.write(b"x", 10).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
        let rotated: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != MAIN_LOG)
            .collect();
        assert_eq!(rotated.len(), 1);
        let name = &rotated[0];
        assert!(
            name.starts_with("main-")
                && name.ends_with(".log")
                && name.len() == "main-2006-01-02T15-04-05.000.log".len(),
            "{name}"
        );
        assert_eq!(std::fs::read(path.with_file_name(name)).unwrap(), b"1234567890");
        // Oversized writes fail without touching the file.
        assert!(file.write(b"0123456789ab", 10).is_err());
        // Reopening: an existing file where size + len >= max rotates first.
        let mut reopened = RotatingFile::new(path.clone());
        std::fs::write(&path, b"123456789").unwrap();
        reopened.write(b"z", 10).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"z");
        // Below the limit it appends.
        let mut appended = RotatingFile::new(path.clone());
        appended.write(b"w", 10).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"zw");
    }

    #[test]
    fn backup_names_use_utc_milliseconds() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-10-03T04:05:06.789Z")
            .unwrap()
            .to_utc();
        assert_eq!(
            backup_name(Path::new("/l/main.log"), at),
            PathBuf::from("/l/main-2026-10-03T04-05-06.789.log")
        );
    }

    #[test]
    fn cleaner_removes_oldest_logs_but_keeps_main_log() {
        let dir = scratch("clean");
        let dir = dir.as_path();
        let at = |secs: u64| std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs);
        let put = |name: &str, len: usize, secs: u64| {
            let path = dir.join(name);
            std::fs::write(&path, vec![b'x'; len]).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(at(secs))
                .unwrap();
        };
        put(MAIN_LOG, 400, 0); // oldest, but protected
        put("main-2026-01-01T00-00-00.000.log", 300, 1);
        put("error-a.log.gz", 300, 2);
        put("v1-responses-b.LOG", 300, 3);
        put("notes.txt", 5000, 0); // not a log
        // Total 1300 > 800: drop the two oldest unprotected logs.
        let protected = dir.join(MAIN_LOG);
        assert_eq!(enforce_size_limit(dir, 800, Some(&protected)).unwrap(), 2);
        let mut left: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["main.log", "notes.txt", "v1-responses-b.LOG"]);
        // Within the limit nothing happens; a missing directory is not an error.
        assert_eq!(enforce_size_limit(dir, 800, Some(&protected)).unwrap(), 0);
        assert_eq!(enforce_size_limit(&dir.join("none"), 1, None).unwrap(), 0);
    }
}
