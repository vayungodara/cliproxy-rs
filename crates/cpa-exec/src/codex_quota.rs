//! Codex passive quota observation: `codex.rate_limits` WebSocket events and error-frame
//! headers normalised into `X-Codex-*` headers (helps/codex_quota.go), and the
//! provider-scoped signal snapshot Go keeps per credential (sdk/cliproxy/auth/quota_signals.go).
//!
//! Snapshots replace, never merge: each attempt's observed headers become the credential's
//! new snapshot. The store is bounded by the number of credentials that have been used.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::SystemTime;

use gjson::Kind;
use http::HeaderMap;

const MAX_ADDITIONAL: usize = 8;
const MAX_SIGNALS: usize = 64;
const MAX_SIGNAL_VALUE: usize = 512;
const QUOTA_MARKERS: [&str; 8] = [
    "-allowed",
    "-limit-reached",
    "-limit-name",
    "-used-percent",
    "-window-minutes",
    "-reset-after-seconds",
    "-reset-at",
    "-over-secondary-limit-percent",
];

/// Header name → value, in the order Go would set them.
pub type Headers = Vec<(String, String)>;

fn set(out: &mut Headers, name: String, value: String) {
    match out.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(&name)) {
        Some(slot) => slot.1 = value,
        None => out.push((name, value)),
    }
}

/// `codexQuotaScalarValue`: strings trimmed, numbers and booleans raw.
fn scalar(v: &gjson::Value<'_>) -> String {
    match v.kind() {
        Kind::String => v.str().trim().to_owned(),
        Kind::Number | Kind::True | Kind::False => v.json().trim().to_owned(),
        _ => String::new(),
    }
}

fn first<'a>(object: &'a gjson::Value<'a>, paths: &[&'a str]) -> gjson::Value<'a> {
    for path in paths {
        let v = object.get(path);
        if v.exists() && v.kind() != Kind::Null {
            return v;
        }
    }
    gjson::Value::default()
}

fn first_string(object: &gjson::Value<'_>, paths: &[&str]) -> String {
    paths
        .iter()
        .map(|p| scalar(&first(object, &[p])))
        .find(|s| !s.is_empty())
        .unwrap_or_default()
}

fn set_scalar(out: &mut Headers, name: &str, object: &gjson::Value<'_>, paths: &[&str]) -> bool {
    let value = scalar(&first(object, paths));
    if value.is_empty() {
        return false;
    }
    set(out, name.to_owned(), value);
    true
}

fn valid_identifier(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty()
        && s.len() <= 256
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn valid_text(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty() && s.len() <= 256 && !s.contains(['\r', '\n'])
}

/// `normalizeCodexQuotaHeaderIdentifier`.
fn header_identifier(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 128 {
        return None;
    }
    let mut out = String::with_capacity(value.len());
    let mut last_dash = false;
    for c in value.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let id = out.trim_matches(['-', '_', '.']).to_owned();
    valid_identifier(&id).then_some(id)
}

fn add_rate_limit(out: &mut Headers, prefix: &str, info: &gjson::Value<'_>) -> bool {
    if info.kind() != Kind::Object {
        return false;
    }
    let mut changed = set_scalar(out, &format!("{prefix}Allowed"), info, &["allowed"]);
    changed |= set_scalar(
        out,
        &format!("{prefix}Limit-Reached"),
        info,
        &["limit_reached", "limitReached"],
    );
    for (key, label) in [("primary", "Primary"), ("secondary", "Secondary")] {
        let window = first(info, &[key]);
        if window.kind() != Kind::Object {
            continue;
        }
        let used = first(&window, &["used_percent", "usedPercent"]);
        let minutes = first(&window, &["window_minutes", "windowMinutes"]);
        let after = first(&window, &["reset_after_seconds", "resetAfterSeconds"]);
        let at = first(&window, &["reset_at", "resetAt"]);
        let has_after = after.exists() && after.i64() >= 0;
        let has_at = at.exists() && at.i64() > 0;
        if !used.exists()
            || !minutes.exists()
            || !(0.0..=100.0).contains(&used.f64())
            || minutes.i64() <= 0
            || (!has_after && !has_at)
        {
            continue;
        }
        let p = format!("{prefix}{label}-");
        set_scalar(
            out,
            &format!("{p}Used-Percent"),
            &window,
            &["used_percent", "usedPercent"],
        );
        set_scalar(
            out,
            &format!("{p}Window-Minutes"),
            &window,
            &["window_minutes", "windowMinutes"],
        );
        if has_after {
            set_scalar(
                out,
                &format!("{p}Reset-After-Seconds"),
                &window,
                &["reset_after_seconds", "resetAfterSeconds"],
            );
        }
        if has_at {
            set_scalar(out, &format!("{p}Reset-At"), &window, &["reset_at", "resetAt"]);
        }
        changed = true;
    }
    changed
}

/// Go `http.CanonicalHeaderKey`.
fn canonical(name: &str) -> String {
    let mut upper = true;
    name.chars()
        .map(|c| {
            let out = if upper {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            upper = c == '-';
            out
        })
        .collect()
}

fn is_quota_header(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    if lower == "retry-after" || lower.starts_with("x-ratelimit-") {
        return true;
    }
    if lower == "x-codex-active-limit" || lower == "x-codex-plan-type" || lower.starts_with("x-codex-credits-") {
        return true;
    }
    lower.starts_with("x-codex-") && QUOTA_MARKERS.iter().any(|m| lower.contains(m))
}

/// `ParseCodexQuotaEventHeaders`: headers from a `codex.rate_limits` event, or the quota
/// headers carried by an `error` frame. `None` for any other payload.
pub fn event_headers(payload: &str) -> Option<Headers> {
    let root = gjson::parse(payload);
    let kind = event_kind(payload.as_bytes())?;
    let mut out = Headers::new();
    if kind == EventKind::Error {
        let headers = root.get("headers");
        if headers.kind() != Kind::Object {
            return None;
        }
        headers.each(|key, value| {
            let name = canonical(key.str().trim());
            let value = scalar(&value);
            if is_quota_header(&name) && !value.is_empty() {
                set(&mut out, name, value);
            }
            true
        });
        return (!out.is_empty()).then_some(out);
    }
    let mut has = add_rate_limit(&mut out, "X-Codex-", &first(&root, &["rate_limits", "rateLimit"]));
    let additional = first(&root, &["additional_rate_limits", "additionalRateLimits"]);
    if matches!(additional.kind(), Kind::Object | Kind::Array) {
        let mut count = 0;
        let is_array = additional.kind() == Kind::Array;
        additional.each(|key, value| {
            if count >= MAX_ADDITIONAL {
                return false;
            }
            let name = if is_array {
                first_string(&value, &["limit_name", "limitName", "name"])
            } else {
                key.str().trim().to_owned()
            };
            let Some(id) = header_identifier(&name) else {
                return true;
            };
            let nested = first(&value, &["rate_limit", "rateLimit"]);
            let info = if nested.exists() { nested } else { value };
            let prefix = format!("X-Codex-Additional-{id}-");
            if add_rate_limit(&mut out, &prefix, &info) {
                if valid_text(&name) {
                    set(&mut out, format!("{prefix}Limit-Name"), name);
                }
                has = true;
                count += 1;
            }
            true
        });
    }
    has |= add_rate_limit(
        &mut out,
        "X-Codex-Code-Review-",
        &first(&root, &["code_review_rate_limits", "codeReviewRateLimits"]),
    );
    let credits = first(&root, &["credits"]);
    if credits.kind() == Kind::Object {
        has |= set_scalar(
            &mut out,
            "X-Codex-Credits-Has-Credits",
            &credits,
            &["has_credits", "hasCredits"],
        );
        has |= set_scalar(&mut out, "X-Codex-Credits-Unlimited", &credits, &["unlimited"]);
        has |= set_scalar(&mut out, "X-Codex-Credits-Balance", &credits, &["balance"]);
    }
    if !has {
        return None;
    }
    let active = first_string(
        &root,
        &["metered_limit_name", "meteredLimitName", "limit_name", "limitName"],
    );
    if valid_identifier(&active) {
        set(&mut out, "X-Codex-Active-Limit".into(), active);
    }
    let plan = first_string(&root, &["plan_type", "planType"]);
    if valid_text(&plan) {
        set(&mut out, "X-Codex-Plan-Type".into(), plan);
    }
    Some(out)
}

#[derive(PartialEq, Eq)]
enum EventKind {
    Error,
    RateLimits,
}

/// `codexQuotaEventKind`: a `"type"` key in the first 256 bytes, no full parse.
fn event_kind(payload: &[u8]) -> Option<EventKind> {
    let end = payload.len().min(256);
    let p = &payload[..end];
    let mut i = 0;
    while i + 6 < end {
        if &p[i..i + 6] != b"\"type\"" {
            i += 1;
            continue;
        }
        let mut j = i + 6;
        while j < end && matches!(p[j], b' ' | b'\t' | b'\r' | b'\n') {
            j += 1;
        }
        if j >= end || p[j] != b':' {
            i += 1;
            continue;
        }
        j += 1;
        while j < end && matches!(p[j], b' ' | b'\t' | b'\r' | b'\n') {
            j += 1;
        }
        if p[j..].starts_with(b"\"error\"") {
            return Some(EventKind::Error);
        }
        if p[j..].starts_with(b"\"codex.rate_limits\"") {
            return Some(EventKind::RateLimits);
        }
        i += 1;
    }
    None
}

/// `collectQuotaSignals("codex", headers)`: the Codex quota headers, ranked, capped at 64.
pub fn collect_signals(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut values: Vec<(String, String)> = Vec::new();
    for name in headers.keys() {
        let canonical = canonical(name.as_str());
        let lower = canonical.to_ascii_lowercase();
        let accepted = lower == "retry-after" || lower.starts_with("x-ratelimit-") || {
            lower.starts_with("x-codex-")
                && (lower == "x-codex-active-limit"
                    || lower == "x-codex-plan-type"
                    || lower.starts_with("x-codex-credits-")
                    || QUOTA_MARKERS.iter().any(|m| lower.contains(m)))
        };
        let Some(value) = headers.get_all(name).iter().next_back() else {
            continue;
        };
        let Ok(value) = value.to_str() else { continue };
        let value = value.trim();
        if !accepted
            || value.is_empty()
            || value.len() > MAX_SIGNAL_VALUE
            || value.chars().any(|c| (c as u32) < 0x20 || c == '\u{7f}')
        {
            continue;
        }
        values.push((canonical, value.to_owned()));
    }
    let rank = |name: &str| {
        let lower = name.to_ascii_lowercase();
        match () {
            _ if lower == "retry-after" => 0,
            _ if lower == "x-codex-plan-type"
                || lower == "x-codex-active-limit"
                || lower.starts_with("x-codex-credits-") =>
            {
                1
            }
            _ if lower == "x-codex-allowed"
                || lower == "x-codex-limit-reached"
                || lower.starts_with("x-codex-primary-")
                || lower.starts_with("x-codex-secondary-") =>
            {
                2
            }
            _ if lower.starts_with("x-codex-code-review-") => 3,
            _ if lower.starts_with("x-codex-additional-") => 5,
            _ if lower.starts_with("x-codex-") => 4,
            _ => 6,
        }
    };
    values.sort_by(|a, b| rank(&a.0).cmp(&rank(&b.0)).then_with(|| a.0.cmp(&b.0)));
    values.into_iter().take(MAX_SIGNALS).collect()
}

/// The latest passive quota snapshot per credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub observed_at: SystemTime,
    pub signals: BTreeMap<String, String>,
}

/// One credential's snapshots: its own and one per model (Go `Auth.Quota` and
/// `ModelState.Quota`).
#[derive(Default)]
struct Entry {
    credential: Option<Snapshot>,
    models: BTreeMap<String, Snapshot>,
}

#[derive(Default)]
pub struct QuotaSignals {
    snapshots: Mutex<HashMap<String, Entry>>,
}

impl QuotaSignals {
    /// Go `ObserveResponseHeadersForProvider` on the credential and on `model`'s state
    /// (`canonicalModelKey`): replaced when `headers` carry any Codex quota signal,
    /// untouched otherwise.
    // ponytail: Go keys model state by the conductor's state model; executors see the
    // upstream model, which differs only for aliases mapped to another upstream name.
    pub fn observe(&self, credential_id: &str, model: &str, headers: &HeaderMap) {
        self.record(credential_id, model, collect_signals(headers), SystemTime::now());
    }

    /// Stores one response's already collected `signals` as the credential's and
    /// `model`'s snapshot; empty signals leave both untouched. Providers with their own
    /// header rules (Claude) share this store.
    pub(crate) fn record(
        &self,
        credential_id: &str,
        model: &str,
        signals: BTreeMap<String, String>,
        observed_at: SystemTime,
    ) {
        if signals.is_empty() {
            return;
        }
        let snapshot = Snapshot { observed_at, signals };
        let mut snapshots = self.snapshots.lock().expect("quota snapshots");
        let entry = snapshots.entry(credential_id.to_owned()).or_default();
        let model = cpa_core::registry::dynamic::canonical_model(model);
        if !model.is_empty() {
            entry.models.insert(model.to_owned(), snapshot.clone());
        }
        entry.credential = Some(snapshot);
    }

    /// The credential's latest snapshot (`quota`).
    pub fn snapshot(&self, credential_id: &str) -> Option<Snapshot> {
        self.snapshots
            .lock()
            .expect("quota snapshots")
            .get(credential_id)
            .and_then(|e| e.credential.clone())
    }

    /// The latest snapshot per model (`model_quotas`), empty when none was observed.
    pub fn model_snapshots(&self, credential_id: &str) -> BTreeMap<String, Snapshot> {
        self.snapshots
            .lock()
            .expect("quota snapshots")
            .get(credential_id)
            .map(|e| e.models.clone())
            .unwrap_or_default()
    }

    /// Drops state for credentials that no longer exist.
    pub fn retain(&self, keep: impl Fn(&str) -> bool) {
        self.snapshots.lock().expect("quota snapshots").retain(|id, _| keep(id));
    }
}

/// Merges event-derived headers into an attempt's observed header set.
pub(crate) fn merge(target: &mut HeaderMap, headers: &Headers) {
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::try_from(name.as_str()),
            http::HeaderValue::from_str(value),
        ) {
            target.insert(name, value);
        }
    }
}
