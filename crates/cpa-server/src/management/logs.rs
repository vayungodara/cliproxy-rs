//! `/observability/logs*` (Go internal/api/handlers/management/logs.go): the
//! application log (`main.log` and its rotations) with Go's incremental cursor, and
//! the request and error request logs.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::Management;
use super::auth_files::{Query, fail, reply};

const MAIN_LOG: &str = "main.log";
const CURSOR_VERSION: i64 = 1;
const FINGERPRINT_MAX: u64 = 4 * 1024;
const MAX_LINE: usize = 8 * 1024 * 1024;

type IoResult<T> = std::io::Result<T>;

/// The zone log line timestamps are read in: Go's `time.Local` when `None`.
pub(crate) type Zone = Option<chrono::FixedOffset>;

fn not_found(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound
}

fn setting(state: &Management, key: &str) -> bool {
    let cfg = state.rt.config();
    cfg.document
        .get("observability")
        .and_then(|o| o.get("logs"))
        .and_then(|l| l.get(key))
        .and_then(serde_yaml_ng::Value::as_bool)
        .unwrap_or(false)
}

/// `[2006-01-02 15:04:05` at the start of a line, in `zone`; 0 otherwise.
pub(crate) fn parse_timestamp(line: &str, zone: Zone) -> i64 {
    let b = line.as_bytes();
    let b = b.strip_prefix(b"[").unwrap_or(b);
    if b.len() < 19 {
        return 0;
    }
    let c = &b[..19];
    let digits = |r: std::ops::Range<usize>| -> Option<u32> {
        c[r].iter().try_fold(0u32, |acc, d| {
            d.is_ascii_digit().then(|| acc * 10 + u32::from(d - b'0'))
        })
    };
    if c[4] != b'-' || c[7] != b'-' || c[10] != b' ' || c[13] != b':' || c[16] != b':' {
        return 0;
    }
    let parsed = (|| {
        let date = chrono::NaiveDate::from_ymd_opt(digits(0..4)? as i32, digits(5..7)?, digits(8..10)?)?;
        let time = chrono::NaiveTime::from_hms_opt(digits(11..13)?, digits(14..16)?, digits(17..19)?)?;
        use chrono::TimeZone;
        let at = date.and_time(time);
        match zone {
            Some(fixed) => fixed.from_local_datetime(&at).earliest().map(|t| t.timestamp()),
            None => chrono::Local.from_local_datetime(&at).earliest().map(|t| t.timestamp()),
        }
    })();
    parsed.unwrap_or(0)
}

/// Go `rotationOrder`: `main.log.N` orders by N; lumberjack's
/// `main-2006-01-02T15-04-05.000.log` newest first.
fn rotation_order(name: &str) -> Option<i64> {
    if let Some(suffix) = name.strip_prefix("main.log.") {
        return suffix.parse::<i64>().ok();
    }
    let clean = name.strip_prefix("main-")?;
    let clean = clean.strip_suffix(".gz").unwrap_or(clean);
    let clean = clean.strip_suffix(".log")?;
    let clean = clean.split('.').next().unwrap_or_default();
    let b = clean.as_bytes();
    if b.len() != 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b'-' || b[16] != b'-' {
        return None;
    }
    let t = chrono::NaiveDateTime::parse_from_str(clean, "%Y-%m-%dT%H-%M-%S").ok()?;
    use chrono::TimeZone;
    let local = chrono::Local.from_local_datetime(&t).earliest()?;
    Some(i64::MAX - local.timestamp())
}

fn is_rotated(name: &str) -> bool {
    rotation_order(name).is_some()
}

/// Go `collectLogFiles`: `main.log` and its rotations, oldest first.
fn collect_log_files(dir: &Path) -> IoResult<Vec<PathBuf>> {
    let mut found: Vec<(i64, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == MAIN_LOG {
            found.push((0, dir.join(name)));
        } else if let Some(order) = rotation_order(&name) {
            found.push((order, dir.join(name)));
        }
    }
    found.sort_by_key(|(order, _)| *order);
    Ok(found.into_iter().rev().map(|(_, p)| p).collect())
}

fn base_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn mod_nanos(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i64)
}

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
struct Cursor {
    v: i64,
    file: String,
    offset: i64,
    size: i64,
    #[serde(rename = "modTime")]
    mod_time: i64,
    #[serde(rename = "modTimeUnixNano", skip_serializing_if = "is_zero")]
    mod_time_unix_nano: i64,
    #[serde(rename = "latestTimestamp")]
    latest_timestamp: i64,
    fingerprint: String,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

impl Cursor {
    fn boundary(&self) -> i64 {
        if self.offset == 0 && self.size > 0 {
            self.size
        } else {
            self.offset
        }
    }

    fn mod_nanos(&self) -> i64 {
        if self.mod_time_unix_nano > 0 {
            self.mod_time_unix_nano
        } else {
            self.mod_time.saturating_mul(1_000_000_000)
        }
    }
}

fn decode_cursor(raw: &str) -> Option<Cursor> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let data = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(raw))
        .ok()?;
    let c: Cursor = serde_json::from_slice(&data).ok()?;
    let valid = c.v == CURSOR_VERSION
        && allowed_cursor_file(&c.file)
        && c.offset >= 0
        && c.size >= 0
        && c.mod_time >= 0
        && c.latest_timestamp >= 0
        && !c.fingerprint.trim().is_empty();
    valid.then_some(c)
}

fn allowed_cursor_file(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && (name == MAIN_LOG || is_rotated(name))
}

/// Go `logFileFingerprint`: SHA-256 over the first and last 4 KiB before `boundary`.
fn fingerprint(path: &Path, boundary: i64) -> IoResult<String> {
    let mut file = std::fs::File::open(path)?;
    let meta = file.metadata()?;
    if meta.is_dir() || boundary < 0 || boundary as u64 > meta.len() {
        return Err(std::io::Error::other("invalid fingerprint boundary"));
    }
    let boundary = boundary as u64;
    let mut hash = Sha256::new();
    hash.update(format!("log-cursor-v1:{boundary}:"));
    let head = boundary.min(FINGERPRINT_MAX);
    let mut buf = vec![0u8; head as usize];
    file.read_exact(&mut buf)?;
    hash.update(&buf);
    let tail_start = boundary - boundary.min(FINGERPRINT_MAX);
    hash.update(format!(":{tail_start}:"));
    file.seek(SeekFrom::Start(tail_start))?;
    let mut buf = vec![0u8; (boundary - tail_start) as usize];
    file.read_exact(&mut buf)?;
    hash.update(&buf);
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&hash.finalize()[..12]))
}

/// Go `newLogCursor`.
fn new_cursor(path: &Path, offset: i64, latest: i64) -> IoResult<String> {
    let meta = std::fs::metadata(path)?;
    let size = meta.len() as i64;
    if meta.is_dir() || offset < 0 || offset > size {
        return Err(std::io::Error::other("invalid cursor offset"));
    }
    let probe = Cursor {
        offset,
        size,
        ..Cursor::default()
    };
    let fingerprint = fingerprint(path, probe.boundary())?;
    let nanos = mod_nanos(&meta);
    let cursor = Cursor {
        v: CURSOR_VERSION,
        file: base_name(path),
        offset,
        size,
        mod_time: nanos.div_euclid(1_000_000_000),
        mod_time_unix_nano: nanos,
        latest_timestamp: latest,
        fingerprint,
    };
    let json = serde_json::to_vec(&cursor).map_err(std::io::Error::other)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json))
}

/// `(matches, truncated)`.
fn matches_cursor(path: &Path, c: &Cursor) -> IoResult<(bool, bool)> {
    let meta = std::fs::metadata(path)?;
    if meta.is_dir() {
        return Err(std::io::Error::other("invalid log file"));
    }
    let size = meta.len() as i64;
    if size < c.offset || size < c.boundary() {
        return Ok((false, true));
    }
    Ok((fingerprint(path, c.boundary())? == c.fingerprint, false))
}

fn changed_after(path: &Path, c: &Cursor) -> bool {
    std::fs::metadata(path).is_ok_and(|m| !m.is_dir() && m.len() > 0 && mod_nanos(&m) > c.mod_nanos())
}

fn empty_main(c: &Cursor) -> bool {
    c.file == MAIN_LOG && c.offset == 0 && c.size == 0
}

/// Go `locateLogCursorFile`.
fn locate(files: &[PathBuf], c: &Cursor) -> IoResult<Option<usize>> {
    let is_main = |p: &PathBuf| base_name(p) == MAIN_LOG;
    let mut defer_empty_main = false;
    if let Some(index) = files.iter().position(|p| base_name(p) == c.file) {
        match matches_cursor(&files[index], c) {
            Err(e) if not_found(&e) => return Ok(None),
            Err(e) => return Err(e),
            Ok((true, false)) => {
                let defer = empty_main(c) && files.iter().any(|p| !is_main(p) && changed_after(p, c));
                if defer {
                    defer_empty_main = true;
                } else if reset_ambiguous_empty_main(files, index, c) {
                    return Ok(None);
                } else {
                    return Ok(Some(index));
                }
            }
            Ok(_) => {}
        }
    }
    if c.file != MAIN_LOG || (c.offset == 0 && c.size == 0 && !defer_empty_main) {
        return Ok(None);
    }
    let check = |i: usize| -> IoResult<Option<bool>> {
        match matches_cursor(&files[i], c) {
            Err(e) if not_found(&e) => Ok(None),
            Err(e) => Err(e),
            Ok((_, true)) => Ok(None),
            Ok((m, false)) => Ok(Some(m)),
        }
    };
    if c.offset == 0 && c.size == 0 {
        for (i, file) in files.iter().enumerate() {
            if is_main(file) || !changed_after(file, c) {
                continue;
            }
            if check(i)? == Some(true) {
                return Ok(Some(i));
            }
        }
        return Ok(None);
    }
    for i in (0..files.len()).rev() {
        if is_main(&files[i]) {
            continue;
        }
        if check(i)? == Some(true) {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

/// Go `shouldResetAmbiguousEmptyMainCursor`.
fn reset_ambiguous_empty_main(files: &[PathBuf], main: usize, c: &Cursor) -> bool {
    if !empty_main(c) {
        return false;
    }
    let Ok(meta) = std::fs::metadata(&files[main]) else {
        return false;
    };
    if meta.is_dir() || (meta.len() as i64 == c.size && mod_nanos(&meta) == c.mod_nanos()) {
        return false;
    }
    files.iter().enumerate().any(|(i, p)| {
        i != main
            && base_name(p) != MAIN_LOG
            && std::fs::metadata(p).is_ok_and(|m| !m.is_dir() && m.len() > 0)
            && !changed_after(p, c)
    })
}

#[derive(Default)]
struct LineRead {
    lines: Vec<String>,
    end_offset: i64,
    latest: i64,
    hit_limit: bool,
}

fn line_too_long() -> std::io::Error {
    std::io::Error::other(format!("log line exceeds {MAX_LINE} bytes"))
}

/// Go `readCompleteLogLines`: newline-terminated lines in `[offset, max)`.
fn read_complete(path: &Path, offset: i64, max: i64, limit: usize, zone: Zone) -> IoResult<LineRead> {
    let mut file = std::fs::File::open(path)?;
    let meta = file.metadata()?;
    if meta.is_dir() || offset < 0 {
        return Err(std::io::Error::other("invalid log offset"));
    }
    let size = meta.len() as i64;
    let max = if max < 0 || max > size { size } else { max };
    if offset > max {
        return Err(std::io::Error::other("invalid log offset"));
    }
    file.seek(SeekFrom::Start(offset as u64))?;
    let mut data = vec![0u8; (max - offset) as usize];
    file.read_exact(&mut data)?;
    let mut out = LineRead {
        end_offset: offset,
        ..LineRead::default()
    };
    let mut start = 0usize;
    while let Some(i) = data[start..].iter().position(|b| *b == b'\n') {
        if i > MAX_LINE {
            return Err(line_too_long());
        }
        let text = String::from_utf8_lossy(&data[start..start + i]);
        let text = text.trim_end_matches('\r').to_owned();
        out.latest = out.latest.max(parse_timestamp(&text, zone));
        out.lines.push(text);
        start += i + 1;
        out.end_offset = offset + start as i64;
        if limit > 0 && out.lines.len() >= limit {
            out.hit_limit = true;
            return Ok(out);
        }
    }
    if data.len() - start > MAX_LINE {
        return Err(line_too_long());
    }
    Ok(out)
}

/// Go `completeLogBoundary`: just past the last newline.
fn complete_boundary(path: &Path) -> IoResult<i64> {
    let mut file = std::fs::File::open(path)?;
    let meta = file.metadata()?;
    if meta.is_dir() {
        return Err(std::io::Error::other("invalid log file"));
    }
    let mut pos = meta.len();
    let mut buf = vec![0u8; 32 * 1024];
    while pos > 0 {
        let chunk = (buf.len() as u64).min(pos);
        pos -= chunk;
        file.seek(SeekFrom::Start(pos))?;
        file.read_exact(&mut buf[..chunk as usize])?;
        if let Some(i) = buf[..chunk as usize].iter().rposition(|b| *b == b'\n') {
            return Ok((pos + i as u64 + 1) as i64);
        }
    }
    Ok(0)
}

/// Go `tailStartOffset`: where the last `limit` lines before `boundary` start.
fn tail_start(path: &Path, boundary: i64, limit: usize) -> IoResult<i64> {
    if limit == 0 {
        return Ok(0);
    }
    let mut file = std::fs::File::open(path)?;
    let mut pos = boundary as u64;
    let mut breaks = 0usize;
    let mut buf = vec![0u8; 32 * 1024];
    while pos > 0 {
        let chunk = (buf.len() as u64).min(pos);
        pos -= chunk;
        file.seek(SeekFrom::Start(pos))?;
        file.read_exact(&mut buf[..chunk as usize])?;
        let mut data = &buf[..chunk as usize];
        while let Some(i) = data.iter().rposition(|b| *b == b'\n') {
            breaks += 1;
            if breaks > limit {
                return Ok((pos + i as u64 + 1) as i64);
            }
            data = &data[..i];
        }
    }
    Ok(0)
}

#[derive(Default)]
struct Output {
    lines: Vec<String>,
    latest: i64,
    next: String,
}

/// Go `tailLogFiles`.
fn tail(files: &[PathBuf], limit: usize, fallback: i64, zone: Zone) -> IoResult<Output> {
    let mut out = Output {
        latest: fallback,
        ..Output::default()
    };
    for path in files.iter().rev() {
        let remaining = if limit > 0 {
            if out.lines.len() >= limit {
                break;
            }
            limit - out.lines.len()
        } else {
            0
        };
        let read = (|| {
            let boundary = complete_boundary(path)?;
            if boundary == 0 {
                return Ok(LineRead::default());
            }
            let start = tail_start(path, boundary, remaining)?;
            read_complete(path, start, boundary, remaining, zone)
        })();
        let read = match read {
            Err(e) if not_found(&e) => continue,
            r => r?,
        };
        if read.lines.is_empty() {
            continue;
        }
        let mut lines = read.lines;
        lines.append(&mut out.lines);
        out.lines = lines;
        out.latest = out.latest.max(read.latest);
    }
    out.next = cursor_for_latest(files, out.latest)?;
    Ok(out)
}

/// Go `cursorForLatestLogFile`.
fn cursor_for_latest(files: &[PathBuf], latest: i64) -> IoResult<String> {
    for path in files.iter().rev() {
        let cursor = complete_boundary(path).and_then(|b| new_cursor(path, b, latest));
        match cursor {
            Err(e) if not_found(&e) => continue,
            r => return r,
        }
    }
    Ok(String::new())
}

/// Go `readLogFilesFromCursor`: `(output, reset)`.
fn read_from_cursor(dir: &Path, files: &[PathBuf], raw: &str, limit: usize, zone: Zone) -> IoResult<(Output, bool)> {
    let Some(c) = decode_cursor(raw) else {
        return Ok((Output::default(), true));
    };
    let mut out = Output {
        latest: c.latest_timestamp,
        next: raw.to_owned(),
        ..Output::default()
    };
    if !allowed_cursor_file(&c.file) || std::path::absolute(dir).is_err() {
        return Ok((out, true));
    }
    let Some(start) = locate(files, &c)? else {
        return Ok((out, true));
    };
    let (mut current, mut current_offset, mut advanced) = (files[start].clone(), c.offset, false);
    for (i, path) in files.iter().enumerate().skip(start) {
        let remaining = if limit > 0 {
            if out.lines.len() >= limit {
                break;
            }
            limit - out.lines.len()
        } else {
            0
        };
        let offset = if i == start { c.offset } else { 0 };
        let read = match read_complete(path, offset, -1, remaining, zone) {
            Err(e) if not_found(&e) => return Ok((out, true)),
            r => r?,
        };
        if !read.lines.is_empty() {
            out.lines.extend(read.lines);
            out.latest = out.latest.max(read.latest);
            current = path.clone();
            current_offset = read.end_offset;
            advanced = true;
        }
        if read.hit_limit {
            break;
        }
    }
    if !advanced {
        return Ok((out, false));
    }
    match new_cursor(&current, current_offset, out.latest) {
        Ok(next) => out.next = next,
        Err(e) if not_found(&e) => return Ok((out, true)),
        Err(e) => return Err(e),
    }
    Ok((out, false))
}

fn logs_response(lines: Vec<String>, count: usize, latest: i64, next: String, reset: bool) -> Response {
    let mut pairs = vec![
        ("lines", Value::from(lines)),
        ("line-count", count.into()),
        ("latest-timestamp", latest.into()),
        ("next-cursor", next.into()),
    ];
    if reset {
        pairs.push(("cursor-reset", true.into()));
    }
    reply(StatusCode::OK, pairs)
}

fn parse_cutoff(raw: &str) -> i64 {
    raw.trim().parse::<i64>().ok().filter(|v| *v > 0).unwrap_or(0)
}

/// Go `GetLogs`.
pub(crate) async fn get_logs(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery) -> Response {
    if !setting(&state, "logging-to-file") {
        return fail(StatusCode::BAD_REQUEST, "logging to file disabled");
    }
    let q = Query::parse(raw);
    let (cursor, after, limit) = (
        q.first("cursor").trim().to_owned(),
        q.first("after").to_owned(),
        q.first("limit").trim().to_owned(),
    );
    let (dir, zone) = (state.log_dir.clone(), state.log_zone);
    tokio::task::spawn_blocking(move || get_logs_sync(&dir, &cursor, &after, &limit, zone))
        .await
        .unwrap_or_else(|_| fail(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))
}

fn get_logs_sync(dir: &Path, cursor: &str, after: &str, limit: &str, zone: Zone) -> Response {
    let files = match collect_log_files(dir) {
        Ok(f) => f,
        Err(e) if not_found(&e) => {
            let mut latest = parse_cutoff(after);
            if let Some(c) = (!cursor.is_empty()).then(|| decode_cursor(cursor)).flatten() {
                latest = latest.max(c.latest_timestamp);
            }
            return logs_response(Vec::new(), 0, latest, String::new(), !cursor.is_empty());
        }
        Err(e) => {
            return fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to list log files: {e}"),
            );
        }
    };
    let limit = if limit.is_empty() {
        0
    } else {
        match limit.parse::<i64>() {
            Ok(n) if n > 0 => usize::try_from(n).unwrap_or(usize::MAX),
            Ok(_) => return fail(StatusCode::BAD_REQUEST, "invalid limit: must be greater than zero"),
            Err(_) => return fail(StatusCode::BAD_REQUEST, "invalid limit: must be a positive integer"),
        }
    };
    let cutoff = parse_cutoff(after);
    let read_error = |e: std::io::Error| {
        fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to read log files: {e}"),
        )
    };
    if !cursor.is_empty() {
        return match read_from_cursor(dir, &files, cursor, limit, zone) {
            Err(e) => read_error(e),
            Ok((out, true)) => match tail(&files, limit, out.latest, zone) {
                Ok(t) => {
                    let n = t.lines.len();
                    logs_response(t.lines, n, t.latest, t.next, true)
                }
                Err(e) => read_error(e),
            },
            Ok((out, false)) => {
                let n = out.lines.len();
                logs_response(out.lines, n, out.latest, out.next, false)
            }
        };
    }
    if cutoff == 0 && limit > 0 {
        return match tail(&files, limit, 0, zone) {
            Ok(t) => {
                let n = t.lines.len();
                logs_response(t.lines, n, t.latest, t.next, false)
            }
            Err(e) => read_error(e),
        };
    }
    // Go's accumulator: every line (timestamps gate following continuation lines).
    let (mut lines, mut total, mut latest, mut include) = (std::collections::VecDeque::new(), 0usize, 0i64, false);
    for path in &files {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) if not_found(&e) => continue,
            Err(e) => {
                return fail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to read log file: {e}"),
                );
            }
        };
        let mut parts: Vec<&[u8]> = data.split(|b| *b == b'\n').collect();
        if parts.last().is_some_and(|l| l.is_empty()) {
            parts.pop();
        }
        for part in parts {
            if part.len() > MAX_LINE {
                return fail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to read log file: bufio.Scanner: token too long",
                );
            }
            let text = String::from_utf8_lossy(part);
            let line = text.trim_end_matches('\r').to_owned();
            total += 1;
            let ts = parse_timestamp(&line, zone);
            latest = latest.max(ts);
            if ts > 0 {
                include = cutoff == 0 || ts > cutoff;
            }
            if cutoff == 0 || include {
                lines.push_back(line);
                if limit > 0 && lines.len() > limit {
                    lines.pop_front();
                }
            }
        }
    }
    if latest == 0 || latest < cutoff {
        latest = cutoff;
    }
    match cursor_for_latest(&files, latest) {
        Ok(next) => logs_response(lines.into(), total, latest, next, false),
        Err(e) => fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to prepare log cursor: {e}"),
        ),
    }
}

/// Go `DeleteLogs`: truncates `main.log`, removes its rotations.
pub(crate) async fn delete_logs(State(state): State<Arc<Management>>) -> Response {
    if !setting(&state, "logging-to-file") {
        return fail(StatusCode::BAD_REQUEST, "logging to file disabled");
    }
    let dir = state.log_dir.clone();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if not_found(&e) => return fail(StatusCode::NOT_FOUND, "log directory not found"),
        Err(e) => {
            return fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to list log directory: {e}"),
            );
        }
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| !e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut removed = 0;
    for name in names {
        let path = dir.join(&name);
        if name == MAIN_LOG {
            let truncated = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_len(0));
            if let Err(e) = truncated
                && !not_found(&e)
            {
                return fail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to truncate log file: {e}"),
                );
            }
        } else if is_rotated(&name) {
            match std::fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(e) if not_found(&e) => removed += 1,
                Err(e) => {
                    return fail(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("failed to remove {name}: {e}"),
                    );
                }
            }
        }
    }
    reply(
        StatusCode::OK,
        [
            ("success", true.into()),
            ("message", "Logs cleared successfully".into()),
            ("removed", removed.into()),
        ],
    )
}

/// Go `GetRequestErrorLogs`: `error-*.log`, newest first; none while request logging
/// is on.
pub(crate) async fn error_logs(State(state): State<Arc<Management>>) -> Response {
    let empty = || reply(StatusCode::OK, [("files", Value::Array(Vec::new()))]);
    if setting(&state, "request-log") {
        return empty();
    }
    let entries = match std::fs::read_dir(&state.log_dir) {
        Ok(e) => e,
        Err(e) if not_found(&e) => return empty(),
        Err(e) => {
            return fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to list request error logs: {e}"),
            );
        }
    };
    let mut files: Vec<(String, u64, i64)> = Vec::new();
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("error-") || !name.ends_with(".log") {
            continue;
        }
        match entry.metadata() {
            Ok(m) => files.push((name, m.len(), mod_nanos(&m).div_euclid(1_000_000_000))),
            Err(e) => {
                return fail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to read log info for {name}: {e}"),
                );
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files.sort_by_key(|f| std::cmp::Reverse(f.2));
    let files: Vec<Value> = files
        .into_iter()
        .map(|(name, size, modified)| serde_json::json!({"name": name, "size": size, "modified": modified}))
        .collect();
    // errorLog structs: name, size, modified.
    super::auth_files::reply_ordered(StatusCode::OK, [("files", Value::Array(files))])
}

/// gin `FileAttachment` over `http.ServeFile` for a plain log file.
// ponytail: no Range or conditional requests (the dashboard downloads whole files).
fn attachment(path: &Path, name: &str) -> Response {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) if not_found(&e) => return fail(StatusCode::NOT_FOUND, "log file not found"),
        Err(e) => {
            return fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to read log file: {e}"),
            );
        }
    };
    let disposition = if name.is_ascii() {
        format!(
            "attachment; filename=\"{}\"",
            name.replace('\\', "\\\\").replace('"', "\\\"")
        )
    } else {
        let encoded: String = url_query_escape(name);
        format!("attachment; filename*=UTF-8''{encoded}")
    };
    let modified = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| {
            chrono::DateTime::<chrono::Utc>::from(t)
                .format("%a, %d %b %Y %H:%M:%S GMT")
                .to_string()
        })
        .unwrap_or_default();
    let content_type = content_type(name, &data);
    let mut response = (StatusCode::OK, data).into_response();
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_DISPOSITION, disposition),
        (header::CONTENT_TYPE, content_type),
        (header::ACCEPT_RANGES, "bytes".to_owned()),
        (header::LAST_MODIFIED, modified),
    ] {
        if let Ok(v) = HeaderValue::from_str(&value) {
            headers.insert(name, v);
        }
    }
    response
}

/// net/http `ServeContent`'s type: `mime.TypeByExtension`, else sniffed.
fn content_type(name: &str, data: &[u8]) -> String {
    let ext = name.rfind('.').map_or("", |i| &name[i..]);
    if ext == ".log"
        && let Some(t) = system_log_type()
    {
        return t.clone();
    }
    // ponytail: Go's sniffer also recognises HTML, XML and binary signatures; log text
    // without binary bytes is text/plain, as there.
    let binary = data
        .iter()
        .take(512)
        .any(|&b| matches!(b, 0x00..=0x08 | 0x0B | 0x0E..=0x1A | 0x1C..=0x1F));
    if binary {
        "application/octet-stream"
    } else {
        "text/plain; charset=utf-8"
    }
    .to_owned()
}

/// Go `mime.TypeByExtension(".log")` on Unix (no built-in entry): the first
/// freedesktop `globs2` database that exists (first entry wins), else the
/// `mime.types` files (last entry wins). `text/*` types gain `charset=utf-8`.
fn system_log_type() -> Option<&'static String> {
    static TYPE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    TYPE.get_or_init(|| {
        let with_charset = |t: &str| {
            if t.starts_with("text/") && !t.contains("charset=") {
                format!("{t}; charset=utf-8")
            } else {
                t.to_owned()
            }
        };
        for globs in ["/usr/local/share/mime/globs2", "/usr/share/mime/globs2"] {
            let Ok(text) = std::fs::read_to_string(globs) else {
                continue;
            };
            return text.lines().find_map(|line| {
                let fields: Vec<&str> = line.split(':').collect();
                (fields.len() >= 3 && !fields[0].is_empty() && !fields[0].starts_with('#') && fields[2] == "*.log")
                    .then(|| with_charset(fields[1]))
            });
        }
        let mut found = None;
        for types in [
            "/etc/mime.types",
            "/etc/apache2/mime.types",
            "/etc/apache/mime.types",
            "/etc/httpd/conf/mime.types",
        ] {
            let Ok(text) = std::fs::read_to_string(types) else {
                continue;
            };
            for line in text.lines() {
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.len() <= 1 || fields[0].starts_with('#') {
                    continue;
                }
                if fields[1..]
                    .iter()
                    .take_while(|e| !e.starts_with('#'))
                    .any(|e| *e == "log")
                {
                    found = Some(with_charset(fields[0]));
                }
            }
        }
        found
    })
    .as_ref()
}

/// Go `url.QueryEscape`.
fn url_query_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".to_owned(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Go `DownloadRequestErrorLog`.
pub(crate) async fn download_error_log(
    State(state): State<Arc<Management>>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    let name = name.trim().to_owned();
    if name.is_empty() || name.contains(['/', '\\']) {
        return fail(StatusCode::BAD_REQUEST, "invalid log file name");
    }
    if !name.starts_with("error-") || !name.ends_with(".log") {
        return fail(StatusCode::NOT_FOUND, "log file not found");
    }
    serve_log(&state.log_dir, &name)
}

fn serve_log(dir: &Path, name: &str) -> Response {
    let Ok(dir) = std::path::absolute(dir) else {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "failed to resolve log directory");
    };
    let path = dir.join(name);
    if path.parent() != Some(dir.as_path()) {
        return fail(StatusCode::BAD_REQUEST, "invalid log file path");
    }
    match std::fs::metadata(&path) {
        Err(e) if not_found(&e) => fail(StatusCode::NOT_FOUND, "log file not found"),
        Err(e) => fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to read log file: {e}"),
        ),
        Ok(m) if m.is_dir() => fail(StatusCode::BAD_REQUEST, "invalid log file"),
        Ok(_) => attachment(&path, name),
    }
}

/// Go `parseLogMetadata`: `(prefix, time, seq)` of `<prefix>-<2006-01-02T150405>[_seq]-<id>.log`.
fn log_meta(name: &str) -> (String, Option<chrono::NaiveDateTime>, i64) {
    let base = name.rsplit_once('.').map_or(name, |(b, _)| b);
    let Some(hyphen) = base.rfind('-').filter(|i| *i > 0) else {
        return (base.to_owned(), None, 0);
    };
    let before_id = &base[..hyphen];
    let (mut seq, mut before_seq) = (0, before_id);
    if let Some(u) = before_id.rfind('_')
        && let Ok(n) = before_id[u + 1..].parse::<i64>()
    {
        seq = n;
        before_seq = &before_id[..u];
    }
    const TS: usize = 17;
    if before_seq.len() >= TS
        && before_seq.is_char_boundary(before_seq.len() - TS)
        && let Ok(t) = chrono::NaiveDateTime::parse_from_str(&before_seq[before_seq.len() - TS..], "%Y-%m-%dT%H%M%S")
    {
        let mut prefix = before_seq;
        if before_seq.len() > TS && before_seq.as_bytes()[before_seq.len() - TS - 1] == b'-' {
            prefix = &before_seq[..before_seq.len() - TS - 1];
        }
        return (prefix.to_owned(), Some(t), seq);
    }
    (before_seq.to_owned(), None, seq)
}

/// Go `logFileIsNewer`.
fn newer(cand: &str, cand_mod: i64, cur: &str, cur_mod: i64) -> bool {
    if cand_mod != cur_mod {
        return cand_mod > cur_mod;
    }
    let (cp, ct, cs) = log_meta(cand);
    let (up, ut, us) = log_meta(cur);
    if let (Some(ct), Some(ut)) = (ct, ut) {
        if ct != ut {
            return ct > ut;
        }
        if cp == up && cs != us {
            return cs > us;
        }
    }
    cand > cur
}

/// Go `GetRequestLogByID`: the newest `*-<last 8 of id>.log`.
pub(crate) async fn request_log(State(state): State<Arc<Management>>, UrlPath(id): UrlPath<String>) -> Response {
    let id = id.trim().to_owned();
    if id.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "missing request ID");
    }
    if id.contains(['/', '\\']) {
        return fail(StatusCode::BAD_REQUEST, "invalid request ID");
    }
    let entries = match std::fs::read_dir(&state.log_dir) {
        Ok(e) => e,
        Err(e) if not_found(&e) => return fail(StatusCode::NOT_FOUND, "log directory not found"),
        Err(e) => {
            return fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to list log directory: {e}"),
            );
        }
    };
    let short = if id.len() > 8 && id.is_char_boundary(id.len() - 8) {
        &id[id.len() - 8..]
    } else {
        id.as_str()
    };
    let suffix = format!("-{short}.log");
    let mut names: Vec<(String, Option<i64>)> = entries
        .flatten()
        .filter(|e| !e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                e.metadata().ok().map(|m| mod_nanos(&m)),
            )
        })
        .filter(|(n, _)| n.ends_with(&suffix))
        .collect();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    let mut matched: Option<(String, i64)> = None;
    for (name, modified) in names {
        match (&matched, modified) {
            (None, Some(m)) => matched = Some((name, m)),
            (None, None) => matched = Some((name, 0)),
            (Some(_), None) => {}
            (Some((cur, cur_mod)), Some(m)) => {
                if newer(&name, m, cur, *cur_mod) {
                    matched = Some((name, m));
                }
            }
        }
    }
    match matched {
        None => fail(StatusCode::NOT_FOUND, "log file not found for the given request ID"),
        Some((name, _)) => serve_log(&state.log_dir, &name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_names_and_timestamps_follow_go() {
        assert_eq!(rotation_order("main.log.3"), Some(3));
        assert_eq!(rotation_order("main.log.x"), None);
        assert!(rotation_order("main-2026-10-02T10-20-30.123.log").is_some());
        assert!(rotation_order("main-2026-10-02T10-20-30.log.gz").is_some());
        assert_eq!(rotation_order("main-2026-13-02T10-20-30.log"), None);
        assert_eq!(rotation_order("other.log"), None);
        assert!(parse_timestamp("[2026-10-02 10:20:30] [--------] [info ] x", None) > 0);
        assert_eq!(parse_timestamp("[2026-02-30 10:20:30] x", None), 0);
        assert_eq!(parse_timestamp("short", None), 0);
        assert_eq!(parse_timestamp("[2026-10-02 1é:20:30] x", None), 0);
    }

    #[test]
    fn request_log_names_order_like_go() {
        assert!(newer(
            "v1-2026-10-02T100000-abcdefgh.log",
            5,
            "v1-2026-10-02T090000-abcdefgh.log",
            5
        ));
        assert!(newer(
            "v1-2026-10-02T100000_2-abcdefgh.log",
            5,
            "v1-2026-10-02T100000_1-abcdefgh.log",
            5
        ));
        assert!(!newer("a-abcdefgh.log", 4, "b-abcdefgh.log", 5));
    }
}
