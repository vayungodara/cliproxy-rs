//! The golden's auth manager: Go's `coreauth.Manager` as `auth_scenarios.go`
//! registers credentials into it, without a selector.

use std::path::Path;
use std::sync::Mutex;

use cpa_plugin::gojson::GoTime;
use cpa_plugin::hostauth::{AuthManager, HostAuth};
use serde_json::Value;

pub struct GoldenManager(Mutex<Vec<HostAuth>>);

impl GoldenManager {
    /// The `auth_manager` step's specs, with `AUTHDIR` in attributes resolved.
    pub fn from_specs(specs: &[Value], auth_dir: &Path) -> Self {
        let now = GoTime::now_utc();
        let text = |spec: &Value, key: &str| spec[key].as_str().unwrap_or_default().to_owned();
        let auths = specs
            .iter()
            .map(|spec| HostAuth {
                id: text(spec, "id"),
                index: text(spec, "index"),
                provider: text(spec, "provider"),
                file_name: text(spec, "file_name"),
                label: text(spec, "label"),
                status: text(spec, "status"),
                status_message: text(spec, "status_message"),
                disabled: spec["disabled"].as_bool().unwrap_or(false),
                unavailable: spec["unavailable"].as_bool().unwrap_or(false),
                attributes: spec["attributes"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(k, v)| {
                        let v = v.as_str().unwrap().replace("AUTHDIR", &auth_dir.to_string_lossy());
                        (k.clone(), v)
                    })
                    .collect(),
                metadata: spec["metadata"].as_object().cloned().unwrap_or_default(),
                // Register stamps both; the snapshot always has its 20 buckets.
                created_at: now,
                updated_at: now,
                recent_requests: (0..20).map(|_| ("00:00-00:10".to_owned(), 0, 0)).collect(),
                account_type: text(spec, "account_type"),
                account: text(spec, "account"),
                ..Default::default()
            })
            .collect();
        Self(Mutex::new(auths))
    }
}

impl AuthManager for GoldenManager {
    fn list(&self) -> Vec<HostAuth> {
        self.0.lock().unwrap().clone()
    }

    fn get_by_id(&self, id: &str) -> Option<HostAuth> {
        self.0.lock().unwrap().iter().find(|a| a.id == id).cloned()
    }

    /// The file is written as is (no store, so no rewrite); `Update` keeps the index
    /// and request counts of the credential it replaces.
    fn save(&self, mut auth: HostAuth, write: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        write()?;
        let mut auths = self.0.lock().unwrap();
        match auths.iter_mut().find(|a| a.id == auth.id) {
            Some(existing) => {
                if auth.index.is_empty() {
                    auth.index = existing.index.clone();
                }
                auth.success = existing.success;
                auth.failed = existing.failed;
                auth.recent_requests = existing.recent_requests.clone();
                *existing = auth;
            }
            None => auths.push(auth),
        }
        Ok(())
    }

    /// `NewManager(nil, nil, nil)` has no selector to observe.
    fn lookup_session_affinity(&self, _: &str, _: &str, _: &str) -> (String, Option<HostAuth>) {
        ("unsupported".into(), None)
    }
}

/// The generator's `normalizeTimes`: RFC 3339 timestamps become `TIME`, recent-request
/// bucket labels (`HH:MM-HH:MM`) become `BUCKET`.
pub fn normalize_times(text: &str) -> String {
    let timestamp = |s: &[u8]| -> Option<usize> {
        let digits = |range: std::ops::Range<usize>| range.clone().all(|i| s.get(i).is_some_and(u8::is_ascii_digit));
        let at = |i: usize, c: u8| s.get(i) == Some(&c);
        if !(digits(0..4) && at(4, b'-') && digits(5..7) && at(7, b'-') && digits(8..10) && at(10, b'T'))
            || !(digits(11..13) && at(13, b':') && digits(14..16) && at(16, b':') && digits(17..19))
        {
            return None;
        }
        let mut end = 19;
        if at(end, b'.') && s.get(end + 1).is_some_and(u8::is_ascii_digit) {
            end += 1;
            while s.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
        }
        if at(end, b'Z') {
            return Some(end + 1);
        }
        let zone = (at(end, b'+') || at(end, b'-'))
            && digits(end + 1..end + 3)
            && at(end + 3, b':')
            && digits(end + 4..end + 6);
        zone.then_some(end + 6)
    };
    let bucket = |s: &[u8]| -> Option<usize> {
        let d = |i: usize| s.get(i).is_some_and(u8::is_ascii_digit);
        (d(0)
            && d(1)
            && s.get(2) == Some(&b':')
            && d(3)
            && d(4)
            && s.get(5) == Some(&b'-')
            && d(6)
            && d(7)
            && s.get(8) == Some(&b':')
            && d(9)
            && d(10))
        .then_some(11)
    };
    // Buckets after timestamps, as the generator applies them.
    replace_matches(&replace_matches(text, &timestamp, "TIME"), &bucket, "BUCKET")
}

/// Replaces every match of `matcher` (the length of a match at the start of a slice).
fn replace_matches(text: &str, matcher: &dyn Fn(&[u8]) -> Option<usize>, placeholder: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        match matcher(rest.as_bytes()) {
            Some(n) => {
                out.push_str(placeholder);
                rest = &rest[n..];
            }
            None => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out
}
