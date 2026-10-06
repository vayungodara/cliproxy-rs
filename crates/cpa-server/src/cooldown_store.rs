//! `routing.cooldown.save-cooldown-status`: cooldowns persisted as one `.cds` file per
//! credential under `auth-dir` (Go sdk/cliproxy/auth/cooldown_state.go). The files use
//! Go's layout and field names, so state survives a switch between CLIProxyAPI and
//! cliproxy-rs in either direction.

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// Go `CooldownStateRecord`. An empty `model` is the credential-level record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Record {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(default)]
    pub auth_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default, with = "go_time")]
    pub next_retry_after: Option<SystemTime>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default)]
    pub quota: Quota,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<LastError>,
    #[serde(default, with = "go_time")]
    pub updated_at: Option<SystemTime>,
    /// The credential file (Go `AuthFile`, `json:"-"`): decides the `.cds` path.
    #[serde(skip)]
    pub auth_file: Option<PathBuf>,
}

/// Go `QuotaState` cooldown fields (`cooldownFieldsOf`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Quota {
    #[serde(default)]
    pub exceeded: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, with = "go_time")]
    pub next_recover_at: Option<SystemTime>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub backoff_level: u32,
    #[serde(default, with = "go_time")]
    pub observed_at: Option<SystemTime>,
    /// cliproxy-rs only (`max-trusted-cooldown`): bounded windows spent, 0 for none.
    /// Written on every quota record cliproxy-rs saves, so its presence marks a record
    /// whose deadline restores as it is. Go's decoder ignores it; records without it
    /// (Go's, or older) get the first bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_windows: Option<u32>,
}

/// Go `Error`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LastError {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub code: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub http_status: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    #[serde(default)]
    version: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    auth_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    provider: String,
    #[serde(default, with = "go_time")]
    updated_at: Option<SystemTime>,
    #[serde(default)]
    records: Vec<Record>,
}

/// Go `time.Time` JSON: RFC 3339 with trimmed nanoseconds; the zero time is
/// `0001-01-01T00:00:00Z` (`None` here).
mod go_time {
    use super::*;

    const ZERO: &str = "0001-01-01T00:00:00Z";

    pub fn serialize<S: serde::Serializer>(t: &Option<SystemTime>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => s.serialize_str(&rfc3339_nano(*t)),
            None => s.serialize_str(ZERO),
        }
    }

    /// Go `time.RFC3339Nano`: nanoseconds with trailing zeros trimmed, no dot when zero.
    pub fn rfc3339_nano(t: SystemTime) -> String {
        let text = DateTime::<Utc>::from(t).to_rfc3339_opts(SecondsFormat::Nanos, true);
        let (head, _) = text.split_once('Z').unwrap_or((&text, ""));
        let head = head.trim_end_matches('0').trim_end_matches('.');
        format!("{head}Z")
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<SystemTime>, D::Error> {
        let raw = Option::<String>::deserialize(d)?.unwrap_or_default();
        if raw.is_empty() {
            return Ok(None);
        }
        let parsed = DateTime::parse_from_rfc3339(&raw).map_err(serde::de::Error::custom)?;
        // Go's zero time and anything before the epoch mean "no time".
        Ok(u64::try_from(parsed.timestamp())
            .ok()
            .map(|secs| SystemTime::UNIX_EPOCH + Duration::new(secs, parsed.timestamp_subsec_nanos())))
    }
}

/// Go `stateRelativePath`: the credential file's path below `dir` with a `.cds`
/// extension, else a sanitized file name.
fn relative_path(dir: &Path, record: &Record) -> Option<PathBuf> {
    match &record.auth_file {
        Some(file) if file.is_absolute() => match file.strip_prefix(dir) {
            Ok(rel) if !rel.as_os_str().is_empty() => cds_path_for_rel(rel),
            _ => file
                .file_name()
                .and_then(|n| sanitize(&n.to_string_lossy()))
                .map(PathBuf::from),
        },
        Some(file) => cds_path_for_rel(file),
        None => sanitize(record.auth_id.trim()).map(PathBuf::from),
    }
}

fn cds_path_for_rel(rel: &Path) -> Option<PathBuf> {
    if rel
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    let base = sanitize(&rel.file_name()?.to_string_lossy())?;
    Some(match rel.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(parent) => parent.join(base),
        None => PathBuf::from(base),
    })
}

/// Go `sanitizeCooldownFileName`: extension dropped, runs outside `[A-Za-z0-9._-]`
/// become `_`, edges trimmed of `._-`, then `.cds`.
fn sanitize(name: &str) -> Option<String> {
    let name = name.trim();
    let stem = name.rfind('.').map_or(name, |dot| &name[..dot]);
    let mut out = String::with_capacity(stem.len());
    let mut in_run = false;
    for c in stem.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('_');
            in_run = true;
        }
    }
    let out = out.trim_matches(|c| matches!(c, '.' | '_' | '-'));
    (!out.is_empty()).then(|| format!("{out}.cds"))
}

/// Go `json.MarshalIndent(v, "", "  ")` plus a newline: serde's pretty layout matches;
/// Go additionally escapes `<`, `>`, `&`, U+2028 and U+2029, which only occur in strings.
fn marshal_indent(envelope: &Envelope) -> Vec<u8> {
    let text = serde_json::to_string_pretty(envelope).expect("cooldown records serialize");
    let mut out = String::with_capacity(text.len() + 1);
    for c in text.chars() {
        match c {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out.push('\n');
    out.into_bytes()
}

fn cds_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(_) if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("cds")) => out.push(path),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// A cooldown store other than `.cds` files (Go `CooldownStateStore` from a token store
/// that is a `CooldownStateStoreProvider`: PGSTORE's `cooldown_store` table). Used
/// while `save-cooldown-status` is on, in place of the files.
pub trait Backend: Send + Sync {
    /// Go `Load`.
    fn load(&self) -> Result<Vec<Record>, String>;
    /// Go `Save`: the complete live set; records no longer present are cleared.
    fn save(&self, records: Vec<Record>, now: SystemTime) -> Result<(), String>;
}

/// Go `FileCooldownStateStore.Load`: every `.cds` file below `dir`. A missing directory
/// is empty state; an unreadable file is an error.
pub fn load(dir: &Path) -> Result<Vec<Record>, String> {
    let mut records = Vec::new();
    for path in cds_files(dir) {
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("read cooldown state {}: {e}", path.display())),
        };
        if data.trim_ascii().is_empty() {
            continue;
        }
        let envelope: Envelope =
            serde_json::from_slice(&data).map_err(|e| format!("parse cooldown state {}: {e}", path.display()))?;
        records.extend(envelope.records);
    }
    Ok(records)
}

/// Go `FileCooldownStateStore.Save`: one atomically replaced file per credential, records
/// sorted by model; `.cds` files no longer backed by a record are removed.
pub fn save(dir: &Path, records: Vec<Record>, now: SystemTime) -> Result<(), String> {
    let mut groups: std::collections::BTreeMap<PathBuf, Vec<Record>> = Default::default();
    for record in records {
        if record.auth_id.trim().is_empty() {
            continue;
        }
        let rel = relative_path(dir, &record).ok_or("cooldown state path: missing auth identity")?;
        groups.entry(dir.join(rel)).or_default().push(record);
    }
    for (path, mut records) in groups.clone() {
        records.sort_by(|a, b| a.model.cmp(&b.model));
        let envelope = Envelope {
            version: 1,
            auth_id: records[0].auth_id.clone(),
            provider: records[0].provider.clone(),
            updated_at: Some(now),
            records,
        };
        create_private_dir(path.parent().unwrap_or(dir))?;
        // Go `os.CreateTemp` + rename: an exclusively created 0600 temporary file.
        crate::runtime::write_bytes_atomic(&path, &marshal_indent(&envelope))
            .map_err(|e| format!("replace cooldown state file: {e}"))?;
    }
    for path in cds_files(dir) {
        if !groups.contains_key(&path)
            && let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(format!("remove stale cooldown state {}: {e}", path.display()));
        }
    }
    Ok(())
}

fn create_private_dir(dir: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .map_err(|e| format!("create cooldown state directory: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_follow_go_sanitizing() {
        let dir = Path::new("/auth");
        let rec = |file: Option<&str>, id: &str| Record {
            auth_id: id.into(),
            auth_file: file.map(PathBuf::from),
            ..Default::default()
        };
        let path = |r: &Record| relative_path(dir, r).map(|p| p.to_string_lossy().into_owned());
        assert_eq!(
            path(&rec(Some("/auth/claude-a.json"), "x")).as_deref(),
            Some("claude-a.cds")
        );
        assert_eq!(
            path(&rec(Some("/auth/sub/team b.json"), "x")).as_deref(),
            Some("sub/team_b.cds")
        );
        assert_eq!(path(&rec(Some("/elsewhere/k!!.json"), "x")).as_deref(), Some("k.cds"));
        assert_eq!(path(&rec(Some("rel/a b.json"), "x")).as_deref(), Some("rel/a_b.cds"));
        assert_eq!(path(&rec(Some("../escape.json"), "x")), None);
        assert_eq!(path(&rec(None, "cfg:key/0")).as_deref(), Some("cfg_key_0.cds"));
        assert_eq!(path(&rec(None, "...")), None);
    }

    /// Go `FileCooldownStateStore` output for the same `MarkResult` sequence
    /// (tests/reference/server/main.go `cooldown_files`), compared with timestamps
    /// masked. Go also writes an aggregate credential-level record derived from its model
    /// states (`updateAggregatedAvailability`); Go recomputes it on restore, so Rust
    /// writes only the credential-wide quota record and the comparison skips the rest.
    #[test]
    fn files_match_go_store() {
        use crate::scheduler::{Policy, Scheduler};
        use cpa_core::credential::{Credential, Source};
        use cpa_core::exec::{ExecError, FailureScope};
        use std::time::Instant;

        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/server_go.json")).unwrap();
        let go = fixture["cooldown_files"].as_object().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "cds-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file = |id: &str, disabled: bool| {
            let metadata = serde_json::json!({"type": "claude"}).as_object().unwrap().clone();
            let mut c = Credential::from_file(&dir, &dir.join(id), metadata).unwrap();
            c.disabled = disabled;
            c
        };
        let mut config = file("x.json", false);
        config.id = "cfg:key/0".into();
        config.source = Source::Config {
            section: "claude-api-key".into(),
            index: 0,
        };
        let creds = [
            file("claude-a.json", false),
            file("sub/team b.json", false),
            config,
            file("off.json", true),
        ];
        let policy = Policy::default();
        let mut s = Scheduler::default();
        let now = Instant::now();
        let fail =
            |s: &mut Scheduler, i: usize, model: &str, status: u16, message: &str, hint: u64, credential: bool| {
                let scope = match (credential, status) {
                    (true, _) => FailureScope::Credential,
                    (_, 429) => FailureScope::Model,
                    (_, 401..=403 | 500..) => FailureScope::Credential,
                    _ => FailureScope::Request,
                };
                let mut e = ExecError::local(status, scope, message);
                e.headers.insert("content-type", "text/plain".parse().unwrap());
                e.retry_after = (hint > 0).then(|| Duration::from_secs(hint));
                s.record(&creds[i], model, &crate::runtime::Outcome::Failure(e), &policy, now);
            };
        fail(&mut s, 0, "m1", 429, "rate limited", 30, false);
        fail(&mut s, 0, "m2", 401, "unauthorized", 0, false);
        fail(&mut s, 1, "m2", 500, "boom", 0, false);
        fail(&mut s, 1, "m1", 429, "credential quota", 60, true);
        fail(&mut s, 2, "m3", 403, "challenge-platform", 0, false);
        fail(&mut s, 3, "m1", 500, "boom", 0, false);
        let wall = SystemTime::now();
        let records: Vec<Record> = creds
            .iter()
            .filter(|c| !c.disabled)
            .flat_map(|c| s.records(c, now, wall))
            .collect();
        save(&dir, records, wall).unwrap();

        let mask = |v: &mut serde_json::Value| {
            fn walk(v: &mut serde_json::Value) {
                match v {
                    serde_json::Value::Object(map) => {
                        for (k, v) in map.iter_mut() {
                            if matches!(k.as_str(), "updated_at" | "next_retry_after" | "next_recover_at")
                                && v.as_str() != Some("0001-01-01T00:00:00Z")
                            {
                                *v = "<time>".into();
                            } else {
                                walk(v);
                            }
                        }
                    }
                    serde_json::Value::Array(items) => items.iter_mut().for_each(walk),
                    _ => {}
                }
            }
            walk(v);
        };
        let mut rust_files: Vec<String> = cds_files(&dir)
            .iter()
            .map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned())
            .collect();
        rust_files.sort();
        let mut go_files: Vec<String> = go.keys().cloned().collect();
        go_files.sort();
        assert_eq!(rust_files, go_files, "same files, none for the disabled credential");
        for (name, text) in go {
            let mut expected: serde_json::Value = serde_json::from_str(text.as_str().unwrap()).unwrap();
            expected["records"]
                .as_array_mut()
                .unwrap()
                .retain(|r| r["model"].as_str().is_some() || r["quota"]["reason"] == "credential_quota");
            let raw = std::fs::read_to_string(dir.join(name)).unwrap();
            assert!(raw.ends_with("}\n"), "{name}: MarshalIndent plus newline");
            let mut actual: serde_json::Value = serde_json::from_str(&raw).unwrap();
            mask(&mut actual);
            // The one deliberate difference (docs/DIFFERENCES-FROM-GO.md): every quota
            // record cliproxy-rs writes carries `trust_windows`, which Go's decoder skips.
            for record in actual["records"].as_array_mut().unwrap() {
                if record["quota"]["exceeded"] == true {
                    let removed = record["quota"].as_object_mut().unwrap().remove("trust_windows");
                    assert!(
                        removed.is_some_and(|v| v.is_u64()),
                        "{name}: trust_windows on quota records"
                    );
                }
            }
            assert_eq!(actual, expected, "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn times_round_trip_go_layout() {
        let t = SystemTime::UNIX_EPOCH + Duration::new(1_790_000_000, 120_000_000);
        let quota = Quota {
            next_recover_at: Some(t),
            ..Default::default()
        };
        let json = serde_json::to_string(&quota).unwrap();
        assert_eq!(
            json,
            r#"{"exceeded":false,"next_recover_at":"2026-09-21T14:13:20.12Z","observed_at":"0001-01-01T00:00:00Z"}"#
        );
        let back: Quota = serde_json::from_str(&json).unwrap();
        assert_eq!(back, quota);
        // Go writes local time with an offset; any offset parses.
        let offset: Quota = serde_json::from_str(r#"{"next_recover_at":"2026-09-21T16:13:20.12+02:00"}"#).unwrap();
        assert_eq!(offset.next_recover_at, Some(t));
        let whole = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        assert_eq!(go_time::rfc3339_nano(whole), "2026-09-21T14:13:20Z");
    }
}
