//! Anthropic shared-window vs model/entitlement rejection (helps/claude_ratelimit.go).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cpa_core::exec::{ExecError, FailureScope};
use http::HeaderMap;

fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim()
        .to_owned()
}
fn status(headers: &HeaderMap, window: &str) -> String {
    header(headers, &format!("anthropic-ratelimit-unified-{window}status")).to_ascii_lowercase()
}
fn allowed(status: &str) -> bool {
    matches!(status, "allowed" | "allowed_warning")
}
fn healthy(headers: &HeaderMap, window: &str) -> bool {
    header(headers, &format!("anthropic-ratelimit-unified-{window}-utilization"))
        .parse::<f64>()
        .is_ok_and(|n| n.is_finite() && (0.0..1.0).contains(&n))
}
fn overage_only(headers: &HeaderMap) -> bool {
    let five = status(headers, "5h-");
    let seven = status(headers, "7d-");
    if five == "rejected" || seven == "rejected" {
        return false;
    }
    let overage = status(headers, "7d_oi-") == "rejected"
        || status(headers, "overage-") == "rejected"
        || !header(headers, "anthropic-ratelimit-unified-overage-disabled-reason").is_empty()
        || header(headers, "anthropic-ratelimit-unified-representative-claim")
            .to_ascii_lowercase()
            .contains("overage");
    overage
        && ((allowed(&five) && allowed(&seven))
            || (allowed(&seven) && five.is_empty() && healthy(headers, "5h"))
            || (allowed(&five) && seven.is_empty() && healthy(headers, "7d")))
}
/// `ClaudeHeadersIndicateUnifiedRateLimitRejection`.
pub(crate) fn shared_rejection(headers: &HeaderMap) -> bool {
    status(headers, "5h-") == "rejected"
        || status(headers, "7d-") == "rejected"
        || (status(headers, "") == "rejected" && !overage_only(headers))
}

/// `classifyClaudeUpstreamErrorWithCooling`. With `model_level_cooling` a shared-window
/// rejection cools only the model, like any other 429.
pub(crate) fn classify(mut error: ExecError, model_level_cooling: bool) -> ExecError {
    if (400..600).contains(&error.status) {
        error.retry_after = rate_limit_reset(&error.headers);
    }
    if error.status == 429 {
        let value: serde_json::Value = serde_json::from_slice(&error.body).unwrap_or_default();
        let message = value["error"]["message"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| String::from_utf8_lossy(&error.body).into_owned())
            .to_lowercase();
        error.scope = if !model_level_cooling && shared_rejection(&error.headers) {
            FailureScope::Credential
        } else if message.contains("fast request rejected")
            || (message.contains("fast")
                && (message.contains("usage credits") || message.contains("credits are required")))
        {
            FailureScope::Request
        } else {
            FailureScope::Model
        };
    }
    error
}

/// `ParseClaudeRateLimitReset` now, with Go's 1–30 s fuzz.
pub(crate) fn rate_limit_reset(headers: &HeaderMap) -> Option<Duration> {
    reset(headers, SystemTime::now(), fuzz())
}

fn fuzz() -> Duration {
    // Rejection sampling avoids modulo bias, matching crypto/rand.Int(30).
    loop {
        let mut byte = [0];
        if getrandom::fill(&mut byte).is_err() {
            return Duration::from_secs(1);
        }
        if byte[0] < 240 {
            return Duration::from_secs(u64::from(byte[0] % 30) + 1);
        }
    }
}

fn reset(headers: &HeaderMap, now: SystemTime, fuzz: Duration) -> Option<Duration> {
    let overage = overage_only(headers);
    let five = status(headers, "5h-");
    let seven = status(headers, "7d-");
    let oi = status(headers, "7d_oi-");
    let unified = status(headers, "");
    let mut deadlines = Vec::new();
    if !overage && let Some(deadline) = retry_after(&header(headers, "retry-after"), now) {
        deadlines.push(deadline);
    }
    for (window, rejected) in [
        ("5h-", five == "rejected"),
        ("7d-", seven == "rejected"),
        ("7d_oi-", oi == "rejected" && !overage),
        (
            "",
            !overage
                && (unified == "rejected"
                    || five == "rejected"
                    || seven == "rejected"
                    || oi == "rejected"
                    || (unified.is_empty() && !allowed(&five) && !allowed(&seven))),
        ),
    ] {
        if rejected
            && let Some(deadline) = timestamp(&header(headers, &format!("anthropic-ratelimit-unified-{window}reset")))
        {
            deadlines.push(deadline);
        }
    }
    deadlines
        .into_iter()
        .filter_map(|t| t.duration_since(now).ok())
        .filter(|d| !d.is_zero())
        .max()
        .and_then(|d| d.checked_add(fuzz))
}

pub(crate) fn retry_after(raw: &str, now: SystemTime) -> Option<SystemTime> {
    if let Ok(seconds) = raw.trim().parse::<f64>()
        && seconds.is_finite()
        && seconds > 0.0
    {
        return Duration::try_from_secs_f64(seconds)
            .ok()
            .and_then(|d| now.checked_add(d));
    }
    timestamp_date(raw)
}
fn timestamp(raw: &str) -> Option<SystemTime> {
    if let Ok(seconds) = raw.trim().parse::<f64>()
        && seconds.is_finite()
        && seconds > 0.0
    {
        return Duration::try_from_secs_f64(seconds)
            .ok()
            .and_then(|d| UNIX_EPOCH.checked_add(d));
    }
    timestamp_date(raw)
}
fn timestamp_date(raw: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(SystemTime::from)
        .or_else(|| httpdate::parse_http_date(raw.trim()).ok())
}

/// Go `collectQuotaSignals("claude", headers)`: `Retry-After` and every
/// `Anthropic-Ratelimit-Unified-*` header under its canonical name with its last value,
/// trimmed; empty, oversized (over 512 bytes) and control-character values are dropped.
/// Both kinds rank first, so at most 64 are kept in name order.
pub fn claude_signals(headers: &HeaderMap) -> std::collections::BTreeMap<String, String> {
    const MAX_SIGNALS: usize = 64;
    const MAX_VALUE: usize = 512;
    let mut signals = std::collections::BTreeMap::new();
    for name in headers.keys() {
        let lower = name.as_str();
        if lower != "retry-after" && !lower.starts_with("anthropic-ratelimit-unified-") {
            continue;
        }
        let Some(value) = headers.get_all(name).iter().next_back() else {
            continue;
        };
        let value = String::from_utf8_lossy(value.as_bytes());
        let value = value.trim();
        if value.is_empty() || value.len() > MAX_VALUE || value.chars().any(|c| c < '\u{20}' || c == '\u{7f}') {
            continue;
        }
        signals.insert(crate::proxy::canonical_header(lower), value.to_owned());
    }
    signals.into_iter().take(MAX_SIGNALS).collect()
}

/// The latest passive quota snapshots per Claude credential and per model (Go
/// `QuotaState.ObserveResponseHeadersForProvider` for provider `claude` on `Auth.Quota`
/// and the result model's `ModelState.Quota`): each upstream Messages response that
/// carries a signal replaces both snapshots; one without leaves them untouched. The
/// store is Codex's; only the header rules differ.
// ponytail: snapshots stay until the process restarts, one per Claude credential ever
// used; Go drops them with the auth. Add a retain pass if credential churn ever matters.
#[derive(Default)]
pub struct Observations(crate::codex_quota::QuotaSignals);

impl Observations {
    /// The credential's snapshot only.
    pub fn observe(&self, credential_id: &str, headers: &HeaderMap) {
        self.observe_model(credential_id, "", headers);
    }

    /// The credential's and `model`'s snapshots (keyed by the model without its
    /// thinking suffix).
    pub fn observe_model(&self, credential_id: &str, model: &str, headers: &HeaderMap) {
        self.observe_at(credential_id, model, headers, SystemTime::now());
    }

    fn observe_at(&self, credential_id: &str, model: &str, headers: &HeaderMap, observed_at: SystemTime) {
        self.0
            .record(credential_id, model, claude_signals(headers), observed_at);
    }

    pub fn snapshot(&self, credential_id: &str) -> Option<crate::codex_quota::Snapshot> {
        self.0.snapshot(credential_id)
    }

    /// The latest snapshot per model (Go `model_quotas`), empty when none was observed.
    pub fn model_snapshots(
        &self,
        credential_id: &str,
    ) -> std::collections::BTreeMap<String, crate::codex_quota::Snapshot> {
        self.0.model_snapshots(credential_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(values: &[(&str, &str)]) -> HeaderMap {
        values
            .iter()
            .map(|(key, value)| {
                (
                    http::HeaderName::from_bytes(key.as_bytes()).unwrap(),
                    value.parse().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn model_shared_and_fast_entitlement_scopes() {
        let mut error = ExecError::local(429, FailureScope::Credential, r#"{"error":{"message":"slow down"}}"#);
        assert_eq!(classify(error.clone(), false).scope, FailureScope::Model);
        error
            .headers
            .insert("anthropic-ratelimit-unified-5h-status", " Rejected ".parse().unwrap());
        assert_eq!(classify(error.clone(), false).scope, FailureScope::Credential);
        error.body = bytes::Bytes::from_static(br#"{"error":{"message":"Usage credits are required for fast mode"}}"#);
        assert_eq!(
            classify(error.clone(), false).scope,
            FailureScope::Credential,
            "shared window takes precedence"
        );
        error.headers.clear();
        assert_eq!(classify(error, false).scope, FailureScope::Request);
    }

    #[test]
    fn overage_only_excludes_retry_after_and_unhealthy_missing_windows_do_not() {
        let now = UNIX_EPOCH + Duration::from_secs(1000);
        let mut h = headers(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-7d-status", "allowed_warning"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.0"),
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", "2000"),
            ("anthropic-ratelimit-unified-7d_oi-reset", "4000"),
            ("retry-after", "5000"),
        ]);
        assert!(!shared_rejection(&h));
        assert_eq!(reset(&h, now, Duration::ZERO), None);
        for utilization in ["NaN", "-0.1", "1", "inf", "bad"] {
            h.insert(
                "anthropic-ratelimit-unified-5h-utilization",
                utilization.parse().unwrap(),
            );
            assert!(shared_rejection(&h), "{utilization}");
            assert_eq!(reset(&h, now, Duration::from_secs(3)), Some(Duration::from_secs(5003)));
        }
    }

    #[test]
    fn latest_relevant_deadline_fractional_seconds_and_dates() {
        let now = UNIX_EPOCH + Duration::from_secs(1000);
        let h = headers(&[
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-reset", "1030.5"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-reset", "999999"),
            ("retry-after", "20.25"),
        ]);
        assert_eq!(
            reset(&h, now, Duration::from_secs(7)),
            Some(Duration::from_millis(37500))
        );
        let date = "Fri, 02 Oct 2026 12:00:00 GMT";
        let expected = httpdate::parse_http_date(date).unwrap();
        assert_eq!(retry_after(date, now), Some(expected));
        assert_eq!(timestamp("2026-10-02T12:00:00Z"), Some(expected));
        for bad in ["NaN", "inf", "-1", "0", "1e100", "wrong"] {
            assert_eq!(retry_after(bad, now), None);
        }
    }

    /// TestQuotaStateObserveResponseHeadersRetainsMeasuredClaudeAndCodexWatermarks (the
    /// Claude half), quota_signals_test.go.
    #[test]
    fn claude_observation_keeps_go_measured_watermarks() {
        let measured = [
            ("Anthropic-Ratelimit-Unified-5h-Status", "allowed"),
            ("Anthropic-Ratelimit-Unified-5h-Utilization", "0.0"),
            ("Anthropic-Ratelimit-Unified-5h-Reset", "1787296800"),
            ("Anthropic-Ratelimit-Unified-7d-Status", "allowed"),
            ("Anthropic-Ratelimit-Unified-7d-Utilization", "0.53"),
            ("Anthropic-Ratelimit-Unified-7d-Reset", "1787695200"),
            ("Anthropic-Ratelimit-Unified-Fallback-Percentage", "0.5"),
            (
                "Anthropic-Ratelimit-Unified-Overage-Disabled-Reason",
                "member_zero_credit_limit",
            ),
            ("Anthropic-Ratelimit-Unified-Overage-Status", "rejected"),
            ("Anthropic-Ratelimit-Unified-Representative-Claim", "five_hour"),
            ("Anthropic-Ratelimit-Unified-Reset", "1787296800"),
            ("Anthropic-Ratelimit-Unified-Status", "allowed"),
        ];
        let mut all = measured.to_vec();
        all.push(("Anthropic-Workspace-Id", "workspace-must-not-be-quota-signal"));
        // Codex's signals are not Claude's (TestQuotaStateObserveResponseHeadersKeepsProviderScopedSignals).
        all.push(("X-Codex-Primary-Used-Percent", "2"));
        all.push(("Authorization", "Bearer secret"));
        let signals = claude_signals(&headers(&all));
        let want: std::collections::BTreeMap<String, String> = measured
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        assert_eq!(signals, want);
    }

    /// TestObserveResponseHeadersReplacesStaleWatermarks,
    /// ...KeepsSnapshotWhenResponseCarriesNoSignal, ...AdvancesObservedAtOnRepeatedValues
    /// and ...RejectsControlCharacterValues, for Claude's headers; per model as in
    /// TestMarkResultQuotaFailureDoesNotEraseSiblingObservation.
    #[test]
    fn observations_replace_keep_advance_and_reject_like_go() {
        let at = |secs| UNIX_EPOCH + Duration::from_secs(secs);
        let obs = Observations::default();
        obs.observe_at(
            "c",
            "claude-a(high)",
            &headers(&[
                ("Retry-After", "120"),
                ("Anthropic-Ratelimit-Unified-Status", "rejected"),
            ]),
            at(100),
        );
        assert_eq!(obs.snapshot("c").unwrap().signals["Retry-After"], "120");
        obs.observe_at(
            "c",
            "claude-b",
            &headers(&[("Anthropic-Ratelimit-Unified-Status", "allowed")]),
            at(200),
        );
        let snap = obs.snapshot("c").unwrap();
        assert!(
            !snap.signals.contains_key("Retry-After"),
            "a stale Retry-After never survives"
        );
        assert_eq!(snap.signals["Anthropic-Ratelimit-Unified-Status"], "allowed");
        assert_eq!(snap.observed_at, at(200));
        // Each model keeps its own latest snapshot, keyed without the thinking suffix:
        // a response for one model never replaces a sibling's.
        let models = obs.model_snapshots("c");
        assert_eq!(models.keys().collect::<Vec<_>>(), ["claude-a", "claude-b"]);
        assert_eq!(models["claude-a"].signals["Retry-After"], "120");
        assert_eq!(models["claude-a"].observed_at, at(100));
        assert_eq!(
            models["claude-b"].signals["Anthropic-Ratelimit-Unified-Status"],
            "allowed"
        );
        // No signal: the previous snapshots stay.
        obs.observe_at(
            "c",
            "claude-b",
            &headers(&[("Content-Type", "application/json")]),
            at(300),
        );
        assert_eq!(obs.snapshot("c").unwrap().observed_at, at(200));
        assert_eq!(obs.model_snapshots("c")["claude-b"].observed_at, at(200));
        // Repeated values still advance the observation time.
        obs.observe_at(
            "c",
            "claude-b",
            &headers(&[("Anthropic-Ratelimit-Unified-Status", "allowed")]),
            at(400),
        );
        assert_eq!(obs.snapshot("c").unwrap().observed_at, at(400));
        // Control characters, empty and oversized values are never stored.
        let mut bad = headers(&[("Anthropic-Ratelimit-Unified-Status", "")]);
        bad.insert(
            "anthropic-ratelimit-unified-reset",
            // The only control byte an HTTP header value can carry.
            http::HeaderValue::from_bytes(b"1\tX-Injected: 1").unwrap(),
        );
        bad.insert("anthropic-ratelimit-unified-claim", "x".repeat(513).parse().unwrap());
        assert!(claude_signals(&bad).is_empty());
        assert!(obs.snapshot("other").is_none());
    }

    /// TestObserveResponseHeadersTruncatesDeterministically, for Claude: the first 64 names.
    #[test]
    fn claude_signals_cap_at_64_by_name() {
        let many: Vec<(String, String)> = (0..128)
            .map(|i| (format!("Anthropic-Ratelimit-Unified-L{i:03}-Status"), i.to_string()))
            .collect();
        let refs: Vec<(&str, &str)> = many.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let signals = claude_signals(&headers(&refs));
        assert_eq!(signals.len(), 64);
        assert!(signals.contains_key("Anthropic-Ratelimit-Unified-L000-Status"));
        assert!(signals.contains_key("Anthropic-Ratelimit-Unified-L063-Status"));
        assert!(!signals.contains_key("Anthropic-Ratelimit-Unified-L064-Status"));
    }
}
