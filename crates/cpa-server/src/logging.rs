//! The process log (Go internal/logging/global_logger.go, log_dir_cleaner.go and
//! `util.SetLogLevel`): Go's line format, stdout or a rotating `main.log`, the
//! log-directory size cleaner and the `debug` level switch. The binary's `--log-file`
//! replaces only stdout: `logging-to-file` still writes `main.log` and the cleaner
//! still runs.
//!
//! [`init`] installs the subscriber once; [`configure`] applies a config snapshot
//! and acts only on the settings that changed, as Go's reload does.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
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
/// Only CLI process rotations are bounded when Go's directory budget is unlimited.
const DEFAULT_PROCESS_ROTATIONS_SIZE: u64 = 32 * 1024 * 1024;
const MAIN_LOG: &str = "main.log";
const CLEANER_INTERVAL: Duration = Duration::from_secs(60);

static OUTPUT: Mutex<Output> = Mutex::new(Output::Stdout);
/// `--log-file`: where stdout output goes instead, kept across config reloads.
static LOG_FILE: OnceLock<PathBuf> = OnceLock::new();
static HOOK: Mutex<Option<Arc<LogHook>>> = Mutex::new(None);
static STATE: Mutex<Option<Applied>> = Mutex::new(None);
static LEVEL: OnceLock<reload::Handle<LevelFilter, tracing_subscriber::Registry>> = OnceLock::new();
static CLEANER_GENERATION: AtomicU64 = AtomicU64::new(0);
/// Set once [`init`] installed the subscriber; [`configure`] does nothing before.
static INSTALLED: AtomicBool = AtomicBool::new(false);
static HOOKS: Mutex<Vec<(u64, Hook)>> = Mutex::new(Vec::new());
static NEXT_HOOK: AtomicU64 = AtomicU64::new(1);

/// One process-log line as Go's logrus hooks see it (the Home app-log forwarder).
pub struct Entry<'a> {
    /// The formatted line, newline included.
    pub line: &'a str,
    /// logrus `Level.String()`: `warning` for WARN.
    pub level: &'static str,
    pub time: chrono::DateTime<chrono::FixedOffset>,
    /// The raw `request_id` field, empty when absent.
    pub request_id: &'a str,
}

pub type Hook = std::sync::Arc<dyn Fn(&Entry<'_>) + Send + Sync>;

/// Go `log.AddHook`: `hook` sees every line written from now on, at the levels the
/// logger writes. Returns the id for [`remove_hook`].
pub fn add_hook(hook: Hook) -> u64 {
    let id = NEXT_HOOK.fetch_add(1, Ordering::SeqCst);
    HOOKS.lock().unwrap_or_else(PoisonError::into_inner).push((id, hook));
    id
}

pub fn remove_hook(id: u64) {
    HOOKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|(i, _)| *i != id);
}

fn fire_hooks(entry: &Entry<'_>) {
    let hooks: Vec<Hook> = HOOKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|(_, hook)| hook.clone())
        .collect();
    for hook in hooks {
        hook(entry);
    }
}

/// The settings last applied, compared on every [`configure`].
#[derive(Clone, Copy, PartialEq)]
struct Applied {
    logging_to_file: bool,
    max_total_mb: i64,
    debug: bool,
}

enum Output {
    Stdout,
    ProcessFile(RotatingFile),
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

/// Go's TUI `LogHook` (internal/tui/loghook.go): every formatted log line, the
/// newest kept when full (Go drops the oldest buffered line).
pub struct LogHook {
    lines: Mutex<VecDeque<String>>,
    capacity: usize,
    ready: tokio::sync::Notify,
}

impl LogHook {
    fn push(&self, line: String) {
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.len() >= self.capacity {
            lines.pop_front();
        }
        lines.push_back(line);
        drop(lines);
        self.ready.notify_one();
    }

    /// The next line, waiting for one.
    pub async fn next(&self) -> String {
        loop {
            if let Some(line) = self.lines.lock().unwrap_or_else(PoisonError::into_inner).pop_front() {
                return line;
            }
            self.ready.notified().await;
        }
    }
}

/// The standalone TUI's log capture (Go `log.AddHook(hook)` with the logger output set
/// to `io.Discard`): every line also goes to the returned hook, and stdout output is
/// dropped until [`release`]. Output to `main.log` continues.
pub fn capture(capacity: usize) -> Arc<LogHook> {
    let hook = Arc::new(LogHook {
        lines: Mutex::new(VecDeque::new()),
        capacity: capacity.max(1),
        ready: tokio::sync::Notify::new(),
    });
    *HOOK.lock().unwrap_or_else(PoisonError::into_inner) = Some(hook.clone());
    hook
}

/// Ends [`capture`]: lines go to stdout again.
pub fn release() {
    *HOOK.lock().unwrap_or_else(PoisonError::into_inner) = None;
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

/// Opt-in replacement for stdout, kept across config reloads: lines that would go
/// to stdout go to `path` instead, while `logging-to-file` still selects `main.log`
/// and its cleaner. Uses the existing synchronous 10 MiB rotation: one fd, no new
/// thread, timer or per-request work.
pub fn set_log_file(path: PathBuf) -> io::Result<()> {
    let path = std::path::absolute(path)?;
    let mut file = RotatingFile {
        path: path.clone(),
        file: None,
        size: 0,
        // The loaded config chooses the budget, before any server/login runs.
        rotation_budget: None,
    };
    file.open_existing_or_new(0, MAX_FILE_SIZE)?;
    LOG_FILE
        .set(path)
        .map_err(|_| io::Error::other("log file already set"))?;
    *OUTPUT.lock().unwrap_or_else(PoisonError::into_inner) = Output::ProcessFile(file);
    Ok(())
}

fn configure_output(dir: &Path, applied: Applied) -> io::Result<()> {
    let mut output = OUTPUT.lock().unwrap_or_else(PoisonError::into_inner);
    let rotation_budget = (applied.max_total_mb <= 0).then_some(DEFAULT_PROCESS_ROTATIONS_SIZE);
    if applied.logging_to_file
        && let Some(path) = LOG_FILE.get()
        && let Some(budget) = rotation_budget
    {
        prune_process_rotations(path, budget);
    }
    let protected = if applied.logging_to_file {
        create_dir(dir)
            .map_err(|e| io::Error::new(e.kind(), format!("logging: failed to create log directory: {e}")))?;
        let path = dir.join(MAIN_LOG);
        *output = Output::File(RotatingFile {
            path: path.clone(),
            file: None,
            size: 0,
            rotation_budget: None,
        });
        Some(std::path::absolute(path)?)
    } else if let Some(path) = LOG_FILE.get() {
        let mut file = RotatingFile {
            path: path.clone(),
            file: None,
            size: 0,
            rotation_budget,
        };
        file.open_existing_or_new(0, MAX_FILE_SIZE)?;
        *output = Output::ProcessFile(file);
        Some(path.clone())
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
                match enforce_size_limit(
                    &dir,
                    max_bytes,
                    protected.as_deref(),
                    LOG_FILE.get().map(PathBuf::as_path),
                ) {
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

fn canonical_log_path(path: &Path) -> io::Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => std::path::absolute(path),
        result => result,
    }
}

/// Go `enforceLogDirSizeLimit`: deletes the oldest `*.log` / `*.log.gz` files (by
/// modification time, never `protected`) and the selected process-log family until
/// their combined size is within `max_bytes`. No other files outside `dir` qualify.
pub(crate) fn enforce_size_limit(
    dir: &Path,
    max_bytes: u64,
    protected: Option<&Path>,
    process_log: Option<&Path>,
) -> io::Result<usize> {
    let protected = protected.map(canonical_log_path).transpose()?;
    let dir = canonical_log_path(dir)?;
    let process_log = process_log.map(canonical_log_path).transpose()?;
    let process_dir = process_log
        .as_deref()
        .and_then(Path::parent)
        .map(canonical_log_path)
        .transpose()?;
    let process_dir = process_dir.as_deref();
    let mut dirs = vec![dir.clone()];
    if let Some(parent) = process_dir
        && parent != dir
    {
        dirs.push(parent.to_owned());
    }
    let mut files = Vec::new();
    let mut total = 0u64;
    for scan_dir in dirs {
        let entries = match std::fs::read_dir(&scan_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in entries.flatten() {
            let filename = entry.file_name();
            let name = filename.to_string_lossy();
            let normal_log = scan_dir == dir && {
                let name = name.trim().to_lowercase();
                name.ends_with(".log") || name.ends_with(".log.gz")
            };
            let process_file = process_log.as_ref().is_some_and(|path| {
                if Some(scan_dir.as_path()) != process_dir {
                    return false;
                }
                let base = path.file_name().unwrap_or_default().to_string_lossy();
                name == base || is_process_rotation(&name, &base)
            });
            if !normal_log && !process_file {
                continue;
            }
            let Ok(info) = entry.metadata() else { continue };
            if !info.is_file() {
                continue;
            }
            total += info.len();
            files.push((info.modified().ok(), info.len(), entry.path()));
        }
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
        let Ok(canonical) = canonical_log_path(&path) else {
            continue;
        };
        if protected.as_ref().is_some_and(|p| *p == canonical) {
            continue;
        }
        // Serialize deletion with rotation/reconfiguration only for --log-file.
        // A previous cleaner generation must not unlink the newly active output.
        let output = process_log
            .as_ref()
            .map(|_| OUTPUT.lock().unwrap_or_else(PoisonError::into_inner));
        if output.as_ref().is_some_and(|output| match &**output {
            Output::ProcessFile(file) | Output::File(file) => {
                canonical_log_path(&file.path).is_ok_and(|active| active == canonical)
            }
            Output::Stdout => false,
        }) {
            continue;
        }
        let removed = std::fs::remove_file(&path);
        drop(output);
        if let Err(error) = removed {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            tracing::warn!("logging: failed to remove old log file: {name}: {error}");
            continue;
        }
        total -= size;
        deleted += 1;
    }
    Ok(deleted)
}

/// lumberjack.Logger with Go's settings, except the CLI process output can cap
/// its rotations synchronously without starting another cleaner timer.
pub(crate) struct RotatingFile {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    rotation_budget: Option<u64>,
}

impl RotatingFile {
    #[cfg(test)]
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: None,
            size: 0,
            rotation_budget: None,
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
                if let Some(budget) = self.rotation_budget {
                    prune_process_rotations(&self.path, budget);
                }
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
        if let Some(budget) = self.rotation_budget {
            prune_process_rotations(&self.path, budget);
        }
        Ok(())
    }
}

fn is_process_rotation(name: &str, base: &str) -> bool {
    let (stem, ext) = base.rfind('.').map_or((base, ""), |i| (&base[..i], &base[i..]));
    name.strip_prefix(stem)
        .and_then(|v| v.strip_prefix('-'))
        .and_then(|v| v.strip_suffix(ext))
        .is_some_and(|timestamp| chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H-%M-%S%.3f").is_ok())
}

/// Runs only at startup/reconfiguration and rotation, under the writer's lock.
/// Excluding the base filename keeps the open file out of both deletion and budget.
/// Cleanup is best-effort: it must not fail log writes or log reconfiguration.
fn prune_process_rotations(path: &Path, max_bytes: u64) {
    let Ok(entries) = std::fs::read_dir(path.parent().unwrap_or(Path::new("."))) else {
        return;
    };
    let base = path.file_name().unwrap_or_default().to_string_lossy();
    let mut files = Vec::new();
    let mut total = 0u64;
    for entry in entries.flatten() {
        if !is_process_rotation(&entry.file_name().to_string_lossy(), &base) {
            continue;
        }
        let Ok(info) = entry.metadata() else { continue };
        if !info.is_file() {
            continue;
        }
        total += info.len();
        files.push((info.modified().ok(), info.len(), entry.path()));
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    for (_, size, path) in files {
        if total <= max_bytes {
            break;
        }
        match std::fs::remove_file(path) {
            Ok(()) => total -= size,
            Err(e) if e.kind() == io::ErrorKind::NotFound => total -= size,
            Err(_) => continue,
        }
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
        // tracing writes each formatted event in one call.
        let hooked = HOOK.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(hook) = &hooked {
            hook.push(String::from_utf8_lossy(buf).trim_end_matches(['\n', '\r']).to_owned());
        }
        match &mut *self.0 {
            Output::Stdout | Output::ProcessFile(_) if hooked.is_some() => Ok(buf.len()),
            Output::Stdout => io::stdout().write(buf),
            Output::ProcessFile(file) | Output::File(file) => file.write(buf, MAX_FILE_SIZE),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut *self.0 {
            Output::Stdout => io::stdout().flush(),
            Output::ProcessFile(file) | Output::File(file) => file.file.as_mut().map_or(Ok(()), Write::flush),
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
        let now = chrono::Local::now();
        // ponytail: Go's hook drops a non-string request_id; Rust records every
        // request_id value as text.
        let request_id = fields.request_id.clone();
        let level = event.metadata().level();
        let line = format_line(
            now.naive_local(),
            level,
            event.metadata().file().zip(event.metadata().line()),
            fields,
        );
        fire_hooks(&Entry {
            line: &line,
            level: logrus_level(level),
            time: now.fixed_offset(),
            request_id: &request_id,
        });
        writer.write_str(&line)
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

/// logrus `Level.String()`.
fn logrus_level(level: &Level) -> &'static str {
    match *level {
        Level::ERROR => "error",
        Level::WARN => "warning",
        Level::INFO => "info",
        Level::DEBUG => "debug",
        Level::TRACE => "trace",
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
    fn process_rotations_prune_on_open_and_rotation_without_touching_active_or_other_logs() {
        let dir = scratch("process-rotation-default");
        create_dir(&dir).unwrap();
        let path = dir.join("process.txt");
        let older = dir.join("process-2001-09-09T01-46-40.000.txt");
        let newer = dir.join("process-2001-09-09T01-46-41.000.txt");
        for (file, contents, seconds) in [(&older, "123", 1_000_000_000), (&newer, "45", 1_000_000_001)] {
            std::fs::write(file, contents).unwrap();
            File::options()
                .write(true)
                .open(file)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(seconds))
                .unwrap();
        }
        let unrelated = dir.join("main.log");
        let malformed = dir.join("process-not-a-timestamp.txt");
        std::fs::write(&unrelated, b"unlimited Go logs").unwrap();
        std::fs::write(&malformed, b"not a rotation").unwrap();
        std::fs::write(&path, b"a").unwrap();
        let mut file = RotatingFile::new(path.clone());
        file.rotation_budget = Some(4);
        file.write(b"b", 10).unwrap();
        assert!(
            !older.exists() && newer.exists(),
            "startup must remove oldest rotations only"
        );
        // Filesystem mtimes can share a tick; give rotations distinct ages.
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_002))
            .unwrap();
        file.write(b"123456789", 10).unwrap();
        assert!(newer.exists(), "two 2-byte rotations fit the 4-byte budget exactly");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"123456789",
            "active file can exceed rotation budget"
        );
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_003))
            .unwrap();
        // Distinct millisecond backup names, as in production's 10 MiB rotations.
        std::thread::sleep(Duration::from_millis(2));
        file.write(b"xy", 10).unwrap();
        assert!(!newer.exists());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"xy",
            "the active writer must stay linked"
        );
        assert!(
            std::fs::read_dir(&dir)
                .unwrap()
                .all(|entry| { !is_process_rotation(&entry.unwrap().file_name().to_string_lossy(), "process.txt") })
        );
        assert_eq!(std::fs::read(&unrelated).unwrap(), b"unlimited Go logs");
        assert_eq!(std::fs::read(&malformed).unwrap(), b"not a rotation");
        drop(file);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn process_pruning_does_not_drop_a_line_when_directory_scanning_fails() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch("process-prune-denied");
        let path = dir.join("process.log");
        let mut file = RotatingFile::new(path.clone());
        file.rotation_budget = Some(4);
        file.write(b"a", 1).unwrap();
        // Write + search permits rotation but not enumeration by the cleaner.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o300)).unwrap();
        let denied = std::fs::read_dir(&dir).is_err();
        let result = file.write(b"b", 1);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Root can bypass mode bits; ordinary users exercise the failure path.
        if denied {
            result.unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"b");
        }
        drop(file);
        std::fs::remove_dir_all(dir).unwrap();
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
        assert_eq!(enforce_size_limit(dir, 800, Some(&protected), None).unwrap(), 2);
        let mut left: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["main.log", "notes.txt", "v1-responses-b.LOG"]);
        // Within the limit nothing happens; a missing directory is not an error.
        assert_eq!(enforce_size_limit(dir, 800, Some(&protected), None).unwrap(), 0);
        assert_eq!(enforce_size_limit(&dir.join("none"), 1, None, None).unwrap(), 0);
    }

    #[test]
    fn cleaner_shares_budget_with_only_the_selected_process_log_family() {
        let root = scratch("process-budget");
        let logs = root.join("logs");
        create_dir(&logs).unwrap();
        let cli = root.join("process output.txt");
        let put = |path: &Path, size: usize, seconds: u64| {
            std::fs::write(path, vec![b'x'; size]).unwrap();
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + seconds))
                .unwrap();
        };
        put(&cli, 200, 0); // oldest, protected despite exceeding a smaller budget
        let old_cli = root.join("process output-2026-01-01T00-00-00.001.txt");
        let old_main = logs.join("main-2026-01-02T00-00-00.002.log");
        let new_cli = root.join("process output-2026-01-03T00-00-00.003.txt");
        put(&old_cli, 300, 1);
        put(&old_main, 300, 2);
        put(&logs.join(MAIN_LOG), 400, 3);
        put(&new_cli, 200, 4);
        let unrelated = root.join("unrelated.log");
        let near_match = root.join("process output-not-a-timestamp.txt");
        put(&unrelated, 5000, 0);
        put(&near_match, 5000, 0);
        // Combined 1400 > 800, though each directory separately fits 800.
        assert_eq!(enforce_size_limit(&logs, 800, Some(&cli), Some(&cli)).unwrap(), 2);
        assert!(!old_cli.exists() && !old_main.exists());
        assert!(new_cli.exists() && logs.join(MAIN_LOG).exists() && cli.exists());
        assert!(unrelated.exists() && near_match.exists());
        assert_eq!(enforce_size_limit(&logs, 800, Some(&cli), Some(&cli)).unwrap(), 0);
        // A missing normal log directory must not disable external rotations.
        assert_eq!(
            enforce_size_limit(&root.join("missing"), 200, Some(&cli), Some(&cli)).unwrap(),
            1
        );
        assert!(cli.exists() && !new_cli.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    /// `--log-file` stands in for stdout only: `logging-to-file: true` still writes
    /// `main.log` with the directory cleaner, and switching it off goes back to the
    /// CLI file while the cleaner keeps its budget. The only test that touches the
    /// process-wide output.
    #[test]
    fn log_file_replaces_only_stdout() {
        let dir = scratch("cli-file");
        let logs = dir.join("logs");
        let cli = dir.join("process output.txt");
        set_log_file(cli.clone()).unwrap();
        let emit = |line: &str| GlobalWriter.make_writer().write_all(line.as_bytes()).unwrap();
        let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
        // An old rotation over the 1 MiB budget, older than anything written here.
        let stale = |name: &str| {
            let path = logs.join(name);
            std::fs::write(&path, vec![b'x'; 2 * 1024 * 1024]).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000))
                .unwrap();
            path
        };
        let gone = |path: &Path| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while path.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            !path.exists()
        };
        emit("before-config\n");
        assert_eq!(read(&cli), "before-config\n");

        configure_output(
            &logs,
            Applied {
                logging_to_file: false,
                max_total_mb: 0,
                debug: false,
            },
        )
        .unwrap();
        assert!(matches!(&*OUTPUT.lock().unwrap(), Output::ProcessFile(file)
            if file.rotation_budget == Some(32 * 1024 * 1024)));
        create_dir(&logs).unwrap();
        let old = stale("main-2001-09-09T01-46-40.000.log");
        let on = Applied {
            logging_to_file: true,
            max_total_mb: 1,
            debug: false,
        };
        configure_output(&logs, on).unwrap();
        emit("to-main\n");
        assert_eq!(read(&logs.join(MAIN_LOG)), "to-main\n");
        assert_eq!(read(&cli), "before-config\n");
        assert!(gone(&old), "the cleaner runs with logging-to-file and --log-file");

        let old = stale("error-old.log");
        configure_output(
            &logs,
            Applied {
                logging_to_file: false,
                ..on
            },
        )
        .unwrap();
        assert!(
            matches!(&*OUTPUT.lock().unwrap(), Output::ProcessFile(file) if file.rotation_budget.is_none()),
            "a configured positive budget must replace the rotation-only default"
        );
        emit("to-cli\n");
        assert_eq!(read(&cli), "before-config\nto-cli\n");
        assert_eq!(read(&logs.join(MAIN_LOG)), "to-main\n");
        assert!(gone(&old), "the cleaner keeps running with only --log-file");

        // Even when the active CLI file alone exceeds the budget, keep it linked.
        let large = "x".repeat(2 * 1024 * 1024);
        emit(&large);
        // Delete the CLI file first if configure_output forgets its protected path.
        File::options()
            .write(true)
            .open(&cli)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000))
            .unwrap();
        configure_output(
            &logs,
            Applied {
                logging_to_file: false,
                ..on
            },
        )
        .unwrap();
        assert!(
            gone(&logs.join(MAIN_LOG)),
            "the active cleaner must run with the CLI file protected"
        );
        assert!(cli.exists(), "configure_output must protect the active process log");
        // A relative/dotted directory spelling must match the absolute protected path.
        enforce_size_limit(
            &logs.join("."),
            1,
            LOG_FILE.get().map(PathBuf::as_path),
            LOG_FILE.get().map(PathBuf::as_path),
        )
        .unwrap();
        assert!(cli.exists(), "the cleaner must never unlink the active process log");
        emit("still-linked\n");
        assert_eq!(read(&cli), format!("before-config\nto-cli\n{large}still-linked\n"));

        // No stale protected path: even a previous generation must respect the
        // output currently open, while removing its rotations outside logs/.
        let old = backup_name(&cli, chrono::Utc::now());
        std::fs::write(&old, b"rotation").unwrap();
        enforce_size_limit(&logs, 1, Some(&logs.join(MAIN_LOG)), Some(&cli)).unwrap();
        assert!(cli.exists() && !old.exists());
        emit("after-clean\n");
        assert!(read(&cli).ends_with("after-clean\n"));

        #[cfg(unix)]
        {
            let alias = dir.join("alias");
            std::os::unix::fs::symlink(&dir, &alias).unwrap();
            enforce_size_limit(
                &logs,
                1,
                Some(&logs.join(MAIN_LOG)),
                Some(&alias.join("process output.txt")),
            )
            .unwrap();
            assert!(
                cli.exists(),
                "directory aliases must not hide the file open for writing"
            );
            emit("after-alias-clean\n");
            assert!(read(&cli).ends_with("after-alias-clean\n"));
            std::fs::remove_file(alias).unwrap();
        }

        // Stop the asynchronous cleaner before checking exact deletion counts.
        configure_output(&logs, Applied { max_total_mb: 0, ..on }).unwrap();
        let inside = logs.join("process.log");
        let spelling = logs.join("..").join("logs").join("process.log");
        *OUTPUT.lock().unwrap() = Output::ProcessFile(RotatingFile::new(spelling.clone()));
        emit(&"a".repeat(600));
        let older = logs.join("error-dedupe.log");
        let rotation = backup_name(&inside, chrono::Utc::now());
        std::fs::write(&older, vec![b'b'; 400]).unwrap();
        std::fs::write(&rotation, vec![b'c'; 200]).unwrap();
        for (path, seconds) in [
            (&inside, 1_000_000_000),
            (&older, 1_000_000_001),
            (&rotation, 1_000_000_002),
        ] {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(seconds))
                .unwrap();
        }
        // 1,200 bytes, not 2,000: the alias must not count the process family twice.
        assert_eq!(
            enforce_size_limit(&logs, 1_000, Some(&spelling), Some(&spelling)).unwrap(),
            1
        );
        assert!(inside.exists() && rotation.exists() && !older.exists());
        assert_eq!(
            enforce_size_limit(&logs, 1, Some(&logs.join(MAIN_LOG)), Some(&spelling)).unwrap(),
            1
        );
        emit("still-linked-inside\n");
        assert!(read(&inside).ends_with("still-linked-inside\n"));
        #[cfg(unix)]
        {
            let alias = dir.join("alias-logs");
            std::os::unix::fs::symlink(&logs, &alias).unwrap();
            assert_eq!(
                enforce_size_limit(&alias, 1, Some(&alias.join("process.log")), Some(&spelling)).unwrap(),
                0
            );
            std::fs::remove_file(alias).unwrap();
        }
        #[cfg(windows)]
        assert_eq!(
            enforce_size_limit(&logs, 1, Some(&logs.join("PROCESS.LOG")), Some(&spelling)).unwrap(),
            0
        );

        // Restore stdout for the rest of the process.
        *OUTPUT.lock().unwrap() = Output::Stdout;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hooks see the written line itself, logrus level names, the local time and
    /// the raw request ID; removed hooks see nothing more.
    #[test]
    fn hooks_see_each_written_line() {
        use std::sync::Arc;
        type Seen = Vec<(String, &'static str, String)>;
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let id = add_hook({
            let seen = seen.clone();
            Arc::new(move |e: &Entry<'_>| {
                if e.line.contains("hook-marker") {
                    seen.lock()
                        .unwrap()
                        .push((e.line.to_owned(), e.level, e.request_id.to_owned()));
                }
            })
        });
        let written = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = {
            let written = written.clone();
            move || SharedWriter(written.clone())
        };
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .event_format(GoFormat)
                .with_writer(writer),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(request_id = " req-123456789 ", "hook-marker one");
            tracing::error!("hook-marker two");
            remove_hook(id);
            tracing::info!("hook-marker three");
        });
        let seen = seen.lock().unwrap().clone();
        let written = String::from_utf8(written.lock().unwrap().clone()).unwrap();
        let lines: Vec<String> = written.split_inclusive('\n').take(2).map(str::to_owned).collect();
        assert_eq!(
            seen,
            [
                (lines[0].clone(), "warning", " req-123456789 ".to_owned()),
                (lines[1].clone(), "error", String::new()),
            ]
        );
        assert!(lines[0].contains("] [23456789] [warn ] ["), "{written}");
    }

    struct SharedWriter(std::sync::Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
