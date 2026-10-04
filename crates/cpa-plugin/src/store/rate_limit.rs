//! The GitHub API rate limiter (internal/pluginstore/github_rate_limit.go): cooldowns
//! per request identity (network scope and credentials), shared across release
//! metadata and API asset downloads.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use super::auth::Headers;

/// A GitHub API cooldown, without response bodies or credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitError {
    pub status: u16,
    pub retry_at: SystemTime,
}

impl std::fmt::Display for RateLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let at = self
            .retry_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_secs()).ok())
            .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0));
        match at {
            Some(at) => write!(
                f,
                "GitHub API rate limited; retry after {}",
                at.format("%Y-%m-%dT%H:%M:%SZ")
            ),
            None => f.write_str("GitHub API rate limited"),
        }
    }
}

impl RateLimitError {
    /// Go `RetryAfterSeconds`: rounded up, never negative.
    pub fn retry_after_seconds(&self, now: SystemTime) -> i64 {
        match self.retry_at.duration_since(now) {
            Ok(delay) if !delay.is_zero() => {
                let secs = delay.as_secs() as i64;
                if delay.subsec_nanos() != 0 { secs + 1 } else { secs }
            }
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Cooldown {
    retry_at: Option<SystemTime>,
    status: u16,
    failures: u8,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Cooldown>,
    next_prune_at: Option<SystemTime>,
}

/// Go `GitHubRateLimiter`. Clients without their own share the process-wide one.
#[derive(Default)]
pub struct GitHubRateLimiter {
    state: Mutex<State>,
    now: Option<fn() -> SystemTime>,
}

/// 9999-12-31T23:59:59Z, the latest `X-Ratelimit-Reset` kept as sent.
const MAX_RESET: u64 = 253_402_300_799;

static DEFAULT_LIMITER: std::sync::LazyLock<GitHubRateLimiter> = std::sync::LazyLock::new(GitHubRateLimiter::default);

impl GitHubRateLimiter {
    /// A limiter on a fixed clock (tests).
    pub fn with_clock(now: fn() -> SystemTime) -> Self {
        Self {
            state: Mutex::default(),
            now: Some(now),
        }
    }

    pub(super) fn shared() -> &'static GitHubRateLimiter {
        &DEFAULT_LIMITER
    }

    fn now(&self) -> SystemTime {
        self.now.map_or_else(SystemTime::now, |f| f())
    }

    /// Go `check`: an active cooldown for the key is an error.
    pub(super) fn check(&self, key: &str) -> Result<(), RateLimitError> {
        if key.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = self.now();
        prune(&mut state, now);
        match state.entries.get(key) {
            Some(entry) if entry.retry_at.is_some_and(|at| now < at) => Err(RateLimitError {
                status: entry.status,
                retry_at: entry.retry_at.unwrap_or(now),
            }),
            _ => Ok(()),
        }
    }

    /// Go `observe`: records the response's limits; a rate-limit rejection is an
    /// error. A late success never clears another request's cooldown.
    pub(super) fn observe(
        &self,
        key: &str,
        status: u16,
        headers: &Headers,
        body: Option<&[u8]>,
    ) -> Result<(), RateLimitError> {
        if key.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = self.now();
        prune(&mut state, now);
        let mut entry = state.entries.get(key).cloned().unwrap_or_default();
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.first())
                .map(|v| v.trim().to_owned())
                .unwrap_or_default()
        };
        let remaining_zero = header("X-Ratelimit-Remaining") == "0";
        let mut retry_at = retry_after(&header("Retry-After"), now);
        let rate_limited = status == 429
            || (status == 403 && (remaining_zero || retry_at.is_some() || body.is_some_and(rate_limit_message)));
        if !rate_limited && !remaining_zero {
            if (200..300).contains(&status) && entry.retry_at.is_none_or(|at| now >= at) {
                state.entries.remove(key);
            }
            return Ok(());
        }
        if remaining_zero
            && let Ok(reset) = header("X-Ratelimit-Reset").parse::<i64>()
            && reset > 0
        {
            // Go keeps any reset; it is capped at year 9999 so the time stays
            // representable and printable.
            let reset_at = SystemTime::UNIX_EPOCH + Duration::from_secs((reset as u64).min(MAX_RESET));
            if retry_at.is_none_or(|at| reset_at > at) {
                retry_at = Some(reset_at);
            }
        }
        let retry_at = match retry_at.filter(|at| *at > now) {
            Some(at) => at,
            None => {
                if !rate_limited {
                    return Ok(());
                }
                // Headerless secondary limits: one minute, doubling to an hour.
                let delay = (Duration::from_secs(60) * (1u32 << entry.failures)).min(Duration::from_secs(3600));
                if entry.failures < 6 {
                    entry.failures += 1;
                }
                now + delay
            }
        };
        if entry.retry_at.is_none_or(|at| retry_at > at) {
            entry.retry_at = Some(retry_at);
        }
        entry.status = if rate_limited { status } else { 429 };
        let error = RateLimitError {
            status,
            retry_at: entry.retry_at.unwrap_or(retry_at),
        };
        state.entries.insert(key.to_owned(), entry);
        if rate_limited { Err(error) } else { Ok(()) }
    }
}

/// Go `pruneLocked`: entries an hour past their cooldown go, at most hourly.
fn prune(state: &mut State, now: SystemTime) {
    if state.next_prune_at.is_some_and(|at| now < at) {
        return;
    }
    let hour = Duration::from_secs(3600);
    state.entries.retain(|_, e| {
        e.retry_at
            .is_some_and(|at| at.checked_add(hour).is_none_or(|end| now < end))
    });
    state.next_prune_at = now.checked_add(hour);
}

/// Go `githubRetryAfter`: delta seconds or an HTTP date.
fn retry_after(value: &str, now: SystemTime) -> Option<SystemTime> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<i64>()
        && (0..=i64::MAX / 1_000_000_000).contains(&seconds)
    {
        return Some(now + Duration::from_secs(seconds as u64));
    }
    http_date(value)
}

/// `http.ParseTime`: RFC 1123 (`GMT`), RFC 850, or ANSI C asctime.
fn http_date(value: &str) -> Option<SystemTime> {
    for layout in [
        "%a, %d %b %Y %H:%M:%S GMT",
        "%A, %d-%b-%y %H:%M:%S GMT",
        "%a %b %e %H:%M:%S %Y",
    ] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(value, layout) {
            return Some(t.and_utc().into());
        }
    }
    None
}

crate::go_struct! {
    /// The body Go's `githubRateLimitMessage` decodes.
    pub struct RateLimitBody("pluginstore.githubRateLimitBody") {
        "message" => message: String,
    }
}

/// Go `githubRateLimitMessage`: `json.Unmarshal` into the struct (keys matched case
/// insensitively; any decode error means no).
fn rate_limit_message(body: &[u8]) -> bool {
    let Ok(decoded) = crate::gojson::from_slice::<RateLimitBody>(body) else {
        return false;
    };
    let text = decoded.message.to_lowercase();
    text.contains("secondary rate limit")
        || text.contains("api rate limit exceeded")
        || text.contains("abuse detection")
}

/// Go `githubRateLimitKey`: GitHub API requests only, by request identity.
pub(super) fn github_rate_limit_key(
    request_url: &str,
    network_scope: &str,
    headers: &Headers,
    authenticated: bool,
) -> String {
    let Some(parsed) = super::url::parse(request_url) else {
        return String::new();
    };
    if !parsed.scheme.eq_ignore_ascii_case("https")
        || !parsed.hostname.eq_ignore_ascii_case("api.github.com")
        || !(parsed.port.is_empty() || parsed.port == "443")
    {
        return String::new();
    }
    format!(
        "api.github.com/{}",
        super::client::request_identity(network_scope, headers, authenticated)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn cooldowns_follow_go() {
        let limiter = GitHubRateLimiter::with_clock(|| SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
        let h = |pairs: &[(&str, &str)]| -> Headers {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), vec![(*v).to_owned()]))
                .collect()
        };
        // Headerless secondary limit: one minute.
        let err = limiter
            .observe(
                "k",
                403,
                &h(&[]),
                Some(br#"{"message":"You have exceeded a secondary rate limit"}"#),
            )
            .unwrap_err();
        assert_eq!(err.retry_at, at(1_060));
        assert!(limiter.check("k").is_err());
        // The reset header extends a remaining=0 answer, even a successful one.
        assert!(
            limiter
                .observe(
                    "j",
                    200,
                    &h(&[("X-Ratelimit-Remaining", "0"), ("X-Ratelimit-Reset", "5000")]),
                    None
                )
                .is_ok()
        );
        assert_eq!(limiter.check("j").unwrap_err().retry_at, at(5_000));
        assert_eq!(limiter.check("j").unwrap_err().status, 429);
        let err = limiter
            .observe("r", 429, &h(&[("Retry-After", "30")]), None)
            .unwrap_err();
        assert_eq!(err.retry_after_seconds(at(1_000)), 30);
        assert_eq!(
            err.to_string(),
            "GitHub API rate limited; retry after 1970-01-01T00:17:10Z"
        );
    }

    /// An absurd reset header neither panics now nor on the requests after it.
    #[test]
    fn extreme_resets_stay_usable() {
        let limiter = GitHubRateLimiter::with_clock(|| SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
        for (key, reset) in [("a", "10000000000000"), ("b", "9223372036854775807")] {
            let headers: Headers = [
                ("X-Ratelimit-Remaining".to_owned(), vec!["0".to_owned()]),
                ("X-Ratelimit-Reset".to_owned(), vec![reset.to_owned()]),
            ]
            .into_iter()
            .collect();
            let err = limiter.observe(key, 403, &headers, None).unwrap_err();
            assert_eq!(err.retry_at, at(MAX_RESET));
            assert_eq!(
                err.to_string(),
                "GitHub API rate limited; retry after 9999-12-31T23:59:59Z"
            );
            assert_eq!(limiter.check(key).unwrap_err().retry_at, at(MAX_RESET));
        }
        // Later requests prune past the stored entries without overflowing.
        let mut state = limiter.state.lock().unwrap();
        state.next_prune_at = None;
        prune(&mut state, at(1_000));
        assert_eq!(state.entries.len(), 2);
        drop(state);
        assert!(limiter.check("other").is_ok());
        assert!(limiter.observe("other", 200, &Headers::new(), None).is_ok());
        // A time past chrono's range still prints.
        let far = RateLimitError {
            status: 429,
            retry_at: at(1 << 50),
        };
        assert_eq!(far.to_string(), "GitHub API rate limited");
    }
}
