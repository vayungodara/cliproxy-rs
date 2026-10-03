//! Credential routing policy. Config parsing remains owned by cpa-core; integration
//! publishes this policy through Runtime::publish_policy.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use cpa_core::credential::Credential;
use cpa_core::exec::ExecError;
#[cfg(test)]
use cpa_core::exec::FailureScope;
use serde::Deserialize;
use serde_json::Value;

use crate::runtime::{Outcome, Selection};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Strategy {
    #[default]
    RoundRobin,
    FillFirst,
    WeightedRoundRobin,
    /// cliproxy-rs addition (`soonest-reset`, alias `reset-first`): spend the account
    /// whose weekly window resets soonest first, until it cools or a usage window runs
    /// out, then the next soonest. Go has no such strategy and reads it as round-robin.
    SoonestReset,
}

/// What `soonest-reset` knows about a credential's usage windows, from its latest
/// passive quota observation (Claude `anthropic-ratelimit-unified-*`, Codex
/// `x-codex-*`). The default is "nothing known".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Windows {
    /// When the long (weekly) window resets, while that is still ahead.
    pub weekly_reset: Option<SystemTime>,
    /// The long window's reset once it has passed: the window rolled over, so what was
    /// learned about it is stale and the credential may be probed again. Only a reset
    /// upstream stated (an absolute time, or a delay above zero) identifies a rollover;
    /// a zero delay would make every answer's observation time a new one.
    pub rolled_over: Option<SystemTime>,
    /// How far apart two rollovers must be to count as different ones: half the long
    /// window (five hours when its length is unknown), which absorbs the rounding of
    /// relative resets around a boundary.
    pub rollover_gap: Duration,
    /// A window (Claude 5-hour or 7-day, Codex primary or secondary) is used up and has
    /// not reset yet: upstream would refuse until it does.
    pub exhausted: bool,
}

/// How long a used-up window without a usable reset or length is assumed to hold.
const FALLBACK_WINDOW: Duration = Duration::from_secs(5 * 3600);

/// One usage window read from quota signals; each part may be missing.
#[derive(Debug, Clone, Copy, Default)]
struct Window {
    used_up: bool,
    reset: Option<SystemTime>,
    /// `reset` when it can identify a rollover (not synthesized from a zero delay).
    stated_reset: Option<SystemTime>,
    /// The window length, when one was reported and can be represented.
    length: Option<Duration>,
}

impl Window {
    /// Until when a used-up window holds: its reset, else its length (or five hours)
    /// after the observation. `None` when it is not used up.
    fn exhausted_until(&self, observed_at: SystemTime) -> Option<SystemTime> {
        if !self.used_up {
            return None;
        }
        self.reset.or_else(|| {
            self.length
                .and_then(|l| observed_at.checked_add(l))
                .or_else(|| observed_at.checked_add(FALLBACK_WINDOW))
        })
    }
}

/// A window length in minutes, when positive and representable as a deadline from
/// `observed_at`; otherwise the missing-length policy applies.
fn window_length(minutes: f64, observed_at: SystemTime) -> Option<Duration> {
    if !minutes.is_finite() || minutes <= 0.0 {
        return None;
    }
    let length = Duration::try_from_secs_f64(minutes * 60.0).ok()?;
    observed_at.checked_add(length).map(|_| length)
}

/// A reset signal: epoch seconds (fractions allowed) or an RFC 3339 time.
fn reset_time(raw: &str) -> Option<SystemTime> {
    let raw = raw.trim();
    if let Ok(seconds) = raw.parse::<f64>() {
        return (seconds.is_finite() && seconds > 0.0)
            .then(|| Duration::try_from_secs_f64(seconds).ok())
            .flatten()
            .and_then(|d| SystemTime::UNIX_EPOCH.checked_add(d));
    }
    chrono::DateTime::parse_from_rfc3339(raw).ok().map(SystemTime::from)
}

impl Windows {
    /// Reads a snapshot's `signals` (header name to value), observed at `observed_at`,
    /// for `provider` (`claude` or `codex`; anything else knows nothing).
    pub fn observed(
        provider: &str,
        signals: &std::collections::BTreeMap<String, String>,
        observed_at: SystemTime,
        now: SystemTime,
    ) -> Self {
        let get = |name: &str| {
            signals
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.trim())
        };
        let number = |name: &str| get(name).and_then(|v| v.parse::<f64>().ok()).filter(|n| n.is_finite());
        let codex = provider.trim().eq_ignore_ascii_case("codex");
        let (long, short) = match provider.trim().to_ascii_lowercase().as_str() {
            "claude" => {
                let window = |label: &str, minutes: f64| Window {
                    used_up: get(&format!("anthropic-ratelimit-unified-{label}-status"))
                        .is_some_and(|s| s.eq_ignore_ascii_case("rejected"))
                        || number(&format!("anthropic-ratelimit-unified-{label}-utilization"))
                            .is_some_and(|u| u >= 1.0),
                    reset: get(&format!("anthropic-ratelimit-unified-{label}-reset")).and_then(reset_time),
                    stated_reset: get(&format!("anthropic-ratelimit-unified-{label}-reset")).and_then(reset_time),
                    length: window_length(minutes, observed_at),
                };
                (Some(window("7d", 7.0 * 24.0 * 60.0)), Some(window("5h", 300.0)))
            }
            "codex" => {
                // Usage, reset and length are read independently; a window exists when
                // any of them was reported.
                let window = |label: &str| {
                    let p = format!("x-codex-{label}-");
                    let used = number(&format!("{p}used-percent"));
                    let absolute = get(&format!("{p}reset-at")).and_then(reset_time);
                    let after = number(&format!("{p}reset-after-seconds")).filter(|s| *s >= 0.0);
                    let relative = after.and_then(|a| observed_at.checked_add(Duration::try_from_secs_f64(a).ok()?));
                    let reset = absolute.or(relative);
                    // A zero delay still ends an exhausted window now, but its time is
                    // just the observation's, so it names no rollover.
                    let stated_reset = absolute.or(relative.filter(|_| after.is_some_and(|a| a > 0.0)));
                    let length = number(&format!("{p}window-minutes")).and_then(|m| window_length(m, observed_at));
                    (used.is_some() || reset.is_some() || length.is_some()).then_some(Window {
                        used_up: used.is_some_and(|u| u >= 100.0),
                        reset,
                        stated_reset,
                        length,
                    })
                };
                let day = Duration::from_secs(24 * 3600);
                match (window("primary"), window("secondary")) {
                    // The longer window is the weekly one, whichever header carries it;
                    // without lengths, secondary is weekly.
                    (Some(p), Some(s)) if p.length.unwrap_or(Duration::ZERO) > s.length.unwrap_or(Duration::MAX) => {
                        (Some(p), Some(s))
                    }
                    (Some(p), Some(s)) => (Some(s), Some(p)),
                    (Some(p), None) if p.length.is_some_and(|l| l >= day) => (Some(p), None),
                    (None, Some(s)) if s.length.is_none_or(|l| l >= day) => (Some(s), None),
                    (only, None) | (None, only) => (None, only),
                }
            }
            _ => (None, None),
        };
        let windows = [long, short];
        let until = windows
            .iter()
            .flatten()
            .filter_map(|w| w.exhausted_until(observed_at))
            .max();
        // `X-Codex-Limit-Reached` lasts as long as the used-up windows do; when none is
        // used up, until the soonest reset any window reports; with no reset at all,
        // five hours from the observation.
        let flag_until = (codex && get("x-codex-limit-reached").is_some_and(|v| v.eq_ignore_ascii_case("true")))
            .then(|| {
                until
                    .or_else(|| windows.iter().flatten().filter_map(|w| w.reset).min())
                    .or_else(|| observed_at.checked_add(FALLBACK_WINDOW))
            })
            .flatten();
        let long_reset = long.and_then(|w| w.reset);
        Self {
            weekly_reset: long_reset.filter(|r| *r > now),
            rolled_over: long.and_then(|w| w.stated_reset).filter(|r| *r <= now),
            rollover_gap: long.and_then(|w| w.length).unwrap_or(FALLBACK_WINDOW) / 2,
            exhausted: until.max(flag_until).is_some_and(|u| u > now),
        }
    }

    /// `soonest-reset` order. Usable accounts come before used-up ones. Among them, an
    /// account without a known reset that has not been probed since its long window
    /// last rolled over gets one probe request, then known resets go earliest first,
    /// then accounts whose probe taught nothing.
    fn order(&self, probed: bool) -> (bool, u8, Option<SystemTime>) {
        let class = match (self.weekly_reset, probed) {
            (Some(_), _) => 1,
            (None, false) => 0,
            (None, true) => 2,
        };
        (self.exhausted, class, self.weekly_reset)
    }
}

/// Whether a credential was already probed for what it reports now. A probe holds
/// until the long window rolls over after it: a reset that passed and is more than
/// `gap` later than the rollover the probe was made for (`None`: a probe with no
/// rollover known).
fn probed(record: Option<&Option<SystemTime>>, rolled_over: Option<SystemTime>, gap: Duration) -> bool {
    match (record, rolled_over) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(at), Some(rollover)) => at.is_some_and(|at| at.checked_add(gap).is_none_or(|limit| rollover <= limit)),
    }
}

/// A credential's [`Windows`] for one pick.
pub type Ranks<'a> = dyn Fn(&Credential) -> Windows + 'a;

#[derive(Debug, Clone)]
pub struct Policy {
    pub strategy: Strategy,
    /// routing.retry.request-retry: int, default 0 (additional rounds).
    pub request_retry: usize,
    /// routing.retry.max-retry-credentials: int, default 0 (unlimited per round).
    pub max_retry_credentials: usize,
    /// routing.retry.max-retry-interval: int seconds, default 0 (no positive waits).
    pub max_retry_interval: Duration,
    /// routing.cooldown.disable-cooling: bool, default false.
    pub disable_cooling: bool,
    /// routing.cooldown.transient-error-cooldown-seconds: int, 0 = 60s, negative disables.
    pub transient_error_cooldown_seconds: i64,
    /// routing.session-affinity: bool, default false.
    pub session_affinity: bool,
    /// routing.session-affinity-ttl: duration string, default 1h.
    pub session_affinity_ttl: Duration,
    pub session_affinity_subagents: bool,
    /// routing.cooldown.save-cooldown-status: persist cooldowns as `.cds` files in
    /// `auth-dir` (cooldown_store.rs), never in credential files.
    pub save_cooldown_status: bool,
    /// routing.force-model-prefix: bool, default false.
    pub force_model_prefix: bool,
    /// Enabled `openai-compatibility` entries in config order with their
    /// `disable-cooling` (Go `providerCoolingOverrideForAuth`). Filled from the config on
    /// every publish ([`compat_cooling`]).
    pub compat_disable_cooling: Vec<(String, Option<bool>)>,
    /// OAuth-only `oauth.request-scoped-errors` channels; API keys must not inherit these.
    pub oauth_request_scoped_errors: HashMap<String, Vec<ErrorRule>>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            strategy: Strategy::RoundRobin,
            request_retry: 0,
            max_retry_credentials: 0,
            max_retry_interval: Duration::ZERO,
            disable_cooling: false,
            transient_error_cooldown_seconds: 0,
            session_affinity: false,
            session_affinity_ttl: Duration::from_secs(3600),
            session_affinity_subagents: true,
            save_cooldown_status: false,
            force_model_prefix: false,
            compat_disable_cooling: Vec::new(),
            oauth_request_scoped_errors: HashMap::new(),
        }
    }
}

/// Raw config values, independent of the config stream's RoutingConfig type.
#[derive(Default)]
pub struct RawRouting<'a> {
    pub strategy: &'a str,
    pub force_model_prefix: bool,
    pub session_affinity: bool,
    pub session_affinity_ttl: &'a str,
    pub session_affinity_subagents: Option<bool>,
    pub request_retry: i64,
    pub max_retry_credentials: i64,
    pub max_retry_interval: i64,
    pub disable_cooling: bool,
    pub save_cooldown_status: bool,
    pub transient_error_cooldown_seconds: i64,
}

// ponytail: OAuth-provider disable-cooling and request-scoped-error maps stay empty until
// the config stream synthesizes provider overrides; per-credential metadata still applies.
impl From<&cpa_core::config::RoutingConfig> for Policy {
    fn from(r: &cpa_core::config::RoutingConfig) -> Self {
        normalize(RawRouting {
            strategy: &r.strategy,
            force_model_prefix: r.force_model_prefix,
            session_affinity: r.session_affinity,
            session_affinity_ttl: &r.session_affinity_ttl,
            session_affinity_subagents: r.session_affinity_subagents,
            request_retry: r.retry.request_retry,
            max_retry_credentials: r.retry.max_retry_credentials,
            max_retry_interval: r.retry.max_retry_interval,
            disable_cooling: r.cooldown.disable_cooling,
            save_cooldown_status: r.cooldown.save_cooldown_status,
            transient_error_cooldown_seconds: r.cooldown.transient_error_cooldown_seconds,
        })
    }
}

pub fn normalize(raw: RawRouting<'_>) -> Policy {
    let ttl = go_duration(raw.session_affinity_ttl.trim())
        .filter(|d| *d > 0)
        .map(|n| Duration::from_nanos(n as u64).max(Duration::from_secs(1)))
        .unwrap_or(Duration::from_secs(3600));
    Policy {
        strategy: match raw.strategy.trim().to_ascii_lowercase().as_str() {
            "weighted-round-robin" | "weightedroundrobin" | "wrr" => Strategy::WeightedRoundRobin,
            "fill-first" | "fillfirst" | "ff" => Strategy::FillFirst,
            "soonest-reset" | "soonestreset" | "reset-first" | "resetfirst" => Strategy::SoonestReset,
            _ => Strategy::RoundRobin,
        },
        force_model_prefix: raw.force_model_prefix,
        session_affinity: raw.session_affinity,
        session_affinity_ttl: ttl,
        session_affinity_subagents: !raw.session_affinity || raw.session_affinity_subagents.unwrap_or(true),
        request_retry: raw.request_retry.max(0) as usize,
        max_retry_credentials: raw.max_retry_credentials.max(0) as usize,
        max_retry_interval: Duration::from_secs(raw.max_retry_interval.max(0) as u64),
        disable_cooling: raw.disable_cooling,
        save_cooldown_status: raw.save_cooldown_status,
        transient_error_cooldown_seconds: raw.transient_error_cooldown_seconds,
        ..Policy::default()
    }
}

/// Go time.ParseDuration grammar: signed, compound decimal quantities, ns/us/µs/μs/
/// ms/s/m/h units, bare zero, nanosecond truncation and signed int64 overflow checks.
fn go_duration(mut text: &str) -> Option<i64> {
    let negative = text.starts_with('-');
    if text.starts_with(['-', '+']) {
        text = &text[1..];
    }
    if text == "0" {
        return Some(0);
    }
    if text.is_empty() {
        return None;
    }
    let mut total = 0u128;
    while !text.is_empty() {
        let n = text.bytes().take_while(u8::is_ascii_digit).count();
        let whole = if n == 0 { 0 } else { text[..n].parse::<u128>().ok()? };
        text = &text[n..];
        let mut fraction = 0u128;
        let mut scale = 1.0;
        let mut digits = 0;
        if text.starts_with('.') {
            text = &text[1..];
            digits = text.bytes().take_while(u8::is_ascii_digit).count();
            // time.leadingFraction consumes but ignores digits after signed overflow.
            let mut overflow = false;
            for digit in text.bytes().take(digits) {
                if overflow || fraction > (i64::MAX as u128) / 10 {
                    overflow = true;
                    continue;
                }
                let next = fraction * 10 + u128::from(digit - b'0');
                if next > 1u128 << 63 {
                    overflow = true;
                    continue;
                }
                fraction = next;
                scale *= 10.0;
            }
            text = &text[digits..];
        }
        if n == 0 && digits == 0 {
            return None;
        }
        let end = text
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(text.len());
        let unit = match &text[..end] {
            "ns" => 1u128,
            "us" | "µs" | "μs" => 1000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            _ => return None,
        };
        // Preserve Go's floating-point operation order, including rounding near a
        // nanosecond boundary (e.g. 0.3333333333333333333h is exactly 20m).
        let fractional_nanos = (fraction as f64 * (unit as f64 / scale)) as u128;
        total = total.checked_add(whole.checked_mul(unit)?.checked_add(fractional_nanos)?)?;
        text = &text[end..];
    }
    if negative {
        if total > 1u128 << 63 {
            return None;
        }
        Some((-(total as i128)) as i64)
    } else {
        i64::try_from(total).ok()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorRule {
    #[serde(default)]
    pub status: i64,
    #[serde(default)]
    pub r#match: Vec<String>,
    #[serde(default, rename = "match-regexr")]
    pub match_regex: Vec<String>,
    #[serde(default)]
    pub action: String,
}

#[derive(Debug, Clone, Copy)]
pub struct ErrorAction {
    /// A configured request-scoped rule decided this outcome.
    pub matched: bool,
    pub stop: bool,
    pub cooldown: bool,
    pub force_cooldown: bool,
}

fn metadata<'a>(c: &'a Credential, key: &str) -> Option<&'a Value> {
    // Canonical presence wins even for false, zero and null.
    c.metadata
        .get(key)
        .or_else(|| c.metadata.get(key.replace('_', "-").as_str()))
}

fn integer(c: &Credential, key: &str) -> Option<i64> {
    c.attributes
        .get(key)
        .filter(|_| key == "priority")
        .and_then(|s| s.trim().parse().ok())
        .or_else(|| {
            [key.to_owned(), key.replace('_', "-")].iter().find_map(|key| {
                let v = c.metadata.get(key)?;
                v.as_i64()
                    .or_else(|| v.as_str()?.trim().parse().ok())
                    .or_else(|| v.as_f64().map(|n| n as i64))
            })
        })
}

fn boolean(c: &Credential, key: &str) -> Option<bool> {
    let parse = |s: &str| match s.trim() {
        "true" | "True" | "TRUE" | "1" | "t" | "T" => Some(true),
        "false" | "False" | "FALSE" | "0" | "f" | "F" => Some(false),
        _ => None,
    };
    [key.to_owned(), key.replace('_', "-")].iter().find_map(|key| {
        let v = c.metadata.get(key)?;
        v.as_bool()
            .or_else(|| parse(v.as_str()?))
            .or_else(|| v.as_f64().map(|n| n != 0.0))
    })
}

fn oauth(c: &Credential) -> bool {
    c.str("api_key").is_none() && !c.attributes.contains_key("api_key")
}

impl Policy {
    pub fn retry_limit(&self, c: &Credential) -> usize {
        integer(c, "request_retry")
            .filter(|n| *n >= 0)
            .map(|n| n as usize)
            .unwrap_or(self.request_retry)
    }

    /// Go `quotaCooldownDisabledForAuthWithConfig`: the credential's `disable_cooling`
    /// metadata, then its OpenAI-compatibility entry's `disable-cooling`, then global.
    pub(crate) fn cooling_disabled(&self, c: &Credential) -> bool {
        boolean(c, "disable_cooling")
            .or_else(|| self.provider_cooling(c))
            .unwrap_or(self.disable_cooling)
    }

    /// Go `providerCoolingOverrideForAuth` with `resolveOpenAICompatConfig`: the first
    /// enabled entry (config order) named by the credential's `compat_name`,
    /// `provider_key` or provider.
    fn provider_cooling(&self, c: &Credential) -> Option<bool> {
        let provider = c.provider.trim().to_lowercase();
        let attr = |k: &str| c.attributes.get(k).map(|v| v.trim()).unwrap_or_default();
        let (compat_name, mut provider_key) = (attr("compat_name"), attr("provider_key"));
        if provider.is_empty()
            || (provider_key.is_empty() && compat_name.is_empty() && provider != "openai-compatibility")
        {
            return None;
        }
        if provider_key.is_empty() {
            provider_key = &provider;
        }
        let candidates = [compat_name, provider_key, provider.as_str()];
        // ponytail: ASCII case folding; Go's EqualFold also folds non-ASCII names.
        self.compat_disable_cooling
            .iter()
            .find(|(name, _)| candidates.iter().any(|c| !c.is_empty() && c.eq_ignore_ascii_case(name)))
            .and_then(|(_, disable)| *disable)
    }

    pub fn error_action(&self, c: &Credential, error: &ExecError) -> ErrorAction {
        let file_rules: Option<Vec<ErrorRule>> =
            metadata(c, "request_scoped_errors").and_then(|v| serde_json::from_value(v.clone()).ok());
        let rules = file_rules.as_ref().filter(|r| !r.is_empty()).or_else(|| {
            oauth(c)
                .then(|| self.oauth_request_scoped_errors.get(&c.provider))
                .flatten()
        });
        if let Some(rules) = rules {
            let body = crate::classify::error_text(error);
            let status = i64::from(crate::classify::go_status(error));
            for rule in rules {
                if rule.status <= 0 || rule.status != status {
                    continue;
                }
                let matches = rule.r#match.iter().any(|s| !s.is_empty() && body.contains(s))
                    || rule
                        .match_regex
                        .iter()
                        .any(|s| !s.is_empty() && regex::Regex::new(s).is_ok_and(|re| re.is_match(&body)));
                if !matches {
                    continue;
                }
                let (stop, cooldown) = match rule.action.trim().to_ascii_lowercase().as_str() {
                    "stop" => (true, false),
                    "stop-and-cooldown" => (true, true),
                    "continue" => (false, false),
                    "continue-and-cooldown" => (false, true),
                    _ => continue,
                };
                return ErrorAction {
                    matched: true,
                    stop,
                    cooldown,
                    force_cooldown: cooldown,
                };
            }
        }
        // Go `shouldSkipCredentialCooldown`: request faults and transport/lifecycle
        // faults never cool a credential.
        let request = crate::classify::is_request_invalid(error);
        ErrorAction {
            matched: false,
            stop: request,
            cooldown: !request && !crate::classify::is_transport(error),
            force_cooldown: false,
        }
    }
}

/// Go `isCredentialRetryRoundStatus`.
pub fn retry_status(status: u16) -> bool {
    matches!(status, 403 | 408 | 429 | 500 | 502 | 503 | 504)
}

fn weight(c: &Credential) -> i64 {
    let value = if let Some(s) = c.attributes.get("weight").filter(|s| !s.trim().is_empty()) {
        s.trim().parse::<i64>().ok()
    } else if let Some(v) = metadata(c, "weight") {
        v.as_i64().or_else(|| v.as_str()?.trim().parse().ok()).or_else(|| {
            let n = v.as_f64()?;
            (n.fract() == 0.0 && n <= 1_000_000.0).then_some(n as i64)
        })
    } else {
        Some(1)
    };
    value.filter(|n| *n <= 1_000_000).unwrap_or(0).max(0)
}

pub use cpa_core::registry::dynamic::canonical_model;

#[derive(Default)]
struct Rotation {
    last: String,
    weights: HashMap<String, i64>,
    current: HashMap<String, i64>,
}

pub(crate) struct Cooldown {
    pub deadline: Instant,
    pub level: u32,
    pub status: u16,
    /// Quota-style cooldown (429 or Cloudflare): reported as a model cooldown.
    pub quota: bool,
    /// The failure that set it (Go `ModelState.LastError`), for error summaries.
    pub error: String,
    /// When it was set (Go `ModelState.UpdatedAt`), for `save-cooldown-status`.
    pub since: SystemTime,
    /// A model quota inherited from a credential-wide quota (Go reason
    /// `credential_quota` on sibling model states).
    pub credential: bool,
}

#[derive(Default)]
pub(crate) struct Scheduler {
    rotations: HashMap<(String, String), Rotation>,
    /// Mixed-provider round-robin cursors (Go `mixedCursors`).
    cursors: HashMap<(String, String), usize>,
    pub(crate) cooldowns: HashMap<(String, String), Cooldown>,
    /// Session affinity bindings (Go `SessionAffinitySelector.cache`).
    affinity: crate::affinity::Cache,
    /// `soonest-reset` probes, reserved when the probe request is picked (under the
    /// scheduler lock, so concurrent picks never probe twice): the long-window rollover
    /// each credential was probed for (`None`: none known). See [`probed`].
    probes: HashMap<String, Option<SystemTime>>,
}

/// Go `cfg.OpenAICompatibility` for cooling: enabled entries with a base URL (Go
/// `SanitizeOpenAICompatibility`), in config order, with their `disable-cooling`.
pub fn compat_cooling(cfg: &cpa_core::config::Config) -> Vec<(String, Option<bool>)> {
    cfg.document
        .get("api-keys")
        .and_then(|k| k.get("openai-compatibility"))
        .and_then(serde_yaml_ng::Value::as_sequence)
        .into_iter()
        .flatten()
        .filter(|g| {
            g.get("base-url")
                .and_then(serde_yaml_ng::Value::as_str)
                .is_some_and(|b| !b.trim().is_empty())
                && g.get("disabled").and_then(serde_yaml_ng::Value::as_bool) != Some(true)
        })
        .map(|g| {
            (
                g.get("name")
                    .and_then(serde_yaml_ng::Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
                g.get("disable-cooling").and_then(serde_yaml_ng::Value::as_bool),
            )
        })
        .collect()
}

/// A selection's affinity keys: the session, and its parent or alias when distinct.
struct SessionKeys {
    primary: crate::affinity::Key,
    fallback: Option<crate::affinity::Key>,
    /// The raw parent/alias session, for Go `isSubagentSession`.
    parent: String,
}

fn session_keys(selection: &Selection) -> Option<SessionKeys> {
    let primary = selection.session.as_deref().filter(|s| !s.is_empty())?;
    let scope = selection.provider_keys().join(",");
    let model = canonical_model(&selection.model).to_owned();
    let parent = selection.session_parent.clone().unwrap_or_default();
    let fallback = (!parent.is_empty() && parent != primary).then(|| (scope.clone(), model.clone(), parent.clone()));
    Some(SessionKeys {
        primary: (scope, model, primary.to_owned()),
        fallback,
        parent,
    })
}

/// Go `nextQuotaCooldown`: 1s doubling to 30m.
fn quota_backoff(level: u32) -> (Duration, u32) {
    let seconds = (1u64 << level.min(11)).min(1800);
    if seconds >= 1800 {
        (Duration::from_secs(1800), level)
    } else {
        (Duration::from_secs(seconds), level + 1)
    }
}

/// Go `recoverableFailureRetryAfterWithHint`.
fn transient(policy: &Policy, hint: Option<Duration>, disabled: bool) -> Option<Duration> {
    if disabled || policy.transient_error_cooldown_seconds < 0 {
        return None;
    }
    if let Some(hint) = hint.filter(|d| !d.is_zero()) {
        return Some(hint);
    }
    Some(Duration::from_secs(match policy.transient_error_cooldown_seconds {
        0 => 60,
        s => s as u64,
    }))
}

impl Scheduler {
    pub fn configure(&mut self, previous: &Policy, next: &Policy) {
        if previous.strategy != next.strategy
            || previous.session_affinity != next.session_affinity
            || previous.session_affinity_ttl != next.session_affinity_ttl
            || previous.session_affinity_subagents != next.session_affinity_subagents
        {
            self.rotations.clear();
            self.cursors.clear();
            self.affinity.clear();
            self.probes.clear();
        }
    }

    pub fn quota_cooling(&self, c: &Credential, model: &str, now: Instant) -> bool {
        let model = canonical_model(model);
        [model, ""].into_iter().any(|m| {
            self.cooldowns
                .get(&(c.id.clone(), m.to_owned()))
                .is_some_and(|s| s.quota && s.deadline > now)
        })
    }

    pub fn wait(&self, c: &Credential, model: &str, now: Instant) -> Option<Duration> {
        let model = canonical_model(model);
        [model, ""]
            .into_iter()
            .filter_map(|model| {
                self.cooldowns
                    .get(&(c.id.clone(), model.to_owned()))
                    .and_then(|s| s.deadline.checked_duration_since(now))
                    .filter(|d| !d.is_zero())
            })
            .max()
    }

    /// The last failure recorded for this credential and model, if any.
    pub fn last_error(&self, c: &Credential, model: &str) -> Option<&Cooldown> {
        let model = canonical_model(model);
        self.cooldowns
            .get(&(c.id.clone(), model.to_owned()))
            .or_else(|| self.cooldowns.get(&(c.id.clone(), String::new())))
    }

    pub fn retry_eligible(&self, c: &Credential, model: &str, now: Instant) -> bool {
        let model = canonical_model(model);
        [model, ""].into_iter().all(|model| {
            self.cooldowns
                .get(&(c.id.clone(), model.to_owned()))
                .is_none_or(|s| s.deadline <= now || retry_status(s.status))
        })
    }

    /// Picks among ready candidates, each paired with its provider key (Go
    /// `SessionAffinitySelector.Pick` over the configured selector).
    ///
    /// An established binding outranks credential priority; a session's parent (or
    /// prompt-cache conversation alias) binding is inherited by forks and, when
    /// `session-affinity-subagents` allows, by subagents.
    #[cfg(test)]
    pub fn pick<'a>(
        &mut self,
        candidates: &[(&'a Credential, &str)],
        selection: &Selection,
        policy: &Policy,
        now: Instant,
    ) -> Option<&'a Credential> {
        self.pick_ranked(candidates, selection, policy, &|_| Windows::default(), now)
    }

    /// [`Self::pick`] with each credential's usage windows, for `soonest-reset`.
    pub fn pick_ranked<'a>(
        &mut self,
        candidates: &[(&'a Credential, &str)],
        selection: &Selection,
        policy: &Policy,
        ranks: &Ranks<'_>,
        now: Instant,
    ) -> Option<&'a Credential> {
        if candidates.is_empty() {
            return None;
        }
        let ttl = policy.session_affinity_ttl;
        let Some(keys) = policy.session_affinity.then(|| session_keys(selection)).flatten() else {
            return self.pick_unbound(candidates, selection, policy, ranks, now);
        };
        self.affinity.sweep(now, ttl);
        let fork = selection.session_fork;
        let subagent = !fork && cpa_common::session::is_subagent_session(&keys.primary.2, &keys.parent);
        let bind_keys = match &keys.fallback {
            Some(fallback) if !subagent && !fork => vec![keys.primary.clone(), fallback.clone()],
            _ => vec![keys.primary.clone()],
        };
        let find = |id: &str| candidates.iter().find(|(c, _)| c.id == id).map(|(c, _)| *c);
        let reuse = match self.affinity.get_and_refresh(&keys.primary, now, ttl) {
            // A bound credential that is no longer available is replaced, not inherited.
            Some(id) => find(&id),
            None => keys
                .fallback
                .as_ref()
                .and_then(|fallback| self.affinity.get(fallback, now))
                .filter(|_| !subagent || policy.session_affinity_subagents)
                .and_then(|id| find(&id)),
        };
        let picked = match reuse {
            Some(c) => c,
            None => self.pick_unbound(candidates, selection, policy, ranks, now)?,
        };
        self.affinity.bind(&picked.id, &bind_keys, now, ttl);
        Some(picked)
    }

    /// Go `SessionAffinitySelector.OnResult`: success refreshes the session's bindings to
    /// this credential, a credential-attributed failure releases them. Request-scoped
    /// and transport failures leave them alone.
    pub fn session_result(
        &mut self,
        c: &Credential,
        selection: &Selection,
        outcome: &Outcome,
        policy: &Policy,
        now: Instant,
    ) {
        if !policy.session_affinity {
            return;
        }
        let success = match outcome {
            Outcome::Success => true,
            Outcome::Failure(error) if policy.error_action(c, error).cooldown => false,
            _ => return,
        };
        let Some(keys) = session_keys(selection) else {
            return;
        };
        let mut targets = vec![keys.primary.clone()];
        if let Some(fallback) = keys.fallback
            && !cpa_common::session::is_subagent_session(&keys.primary.2, &keys.parent)
        {
            targets.push(fallback);
        }
        for key in &targets {
            if success {
                self.affinity.touch(key, &c.id, now, policy.session_affinity_ttl);
            } else {
                self.affinity.compare_and_delete(key, &c.id);
            }
        }
    }

    fn pick_unbound<'a>(
        &mut self,
        candidates: &[(&'a Credential, &str)],
        selection: &Selection,
        policy: &Policy,
        ranks: &Ranks<'_>,
        _now: Instant,
    ) -> Option<&'a Credential> {
        let model = canonical_model(&selection.model).to_owned();
        let provider_keys = selection.provider_keys();
        let providers_key = provider_keys.join(",");
        let tier = candidates
            .iter()
            .map(|(c, _)| integer(c, "priority").unwrap_or(0))
            .max()?;
        let mut ready: Vec<(&Credential, &str)> = candidates
            .iter()
            .copied()
            .filter(|(c, _)| integer(c, "priority").unwrap_or(0) == tier)
            .collect();
        ready.sort_by(|a, b| a.0.id.cmp(&b.0.id));
        // Providers that have ready candidates, in the request's provider order.
        let mut groups: Vec<(&str, Vec<&Credential>)> = Vec::new();
        for provider in provider_keys
            .iter()
            .map(String::as_str)
            .chain(ready.iter().map(|(_, p)| *p))
        {
            if groups.iter().any(|(p, _)| *p == provider) {
                continue;
            }
            let members: Vec<&Credential> = ready.iter().filter(|(_, p)| *p == provider).map(|(c, _)| *c).collect();
            if !members.is_empty() {
                groups.push((provider, members));
            }
        }
        if self.rotations.len() >= 4096 {
            self.rotations.clear();
            self.cursors.clear();
        }
        let picked = if groups.len() == 1 {
            let (provider, members) = &groups[0];
            self.pick_within(provider, &model, members, policy.strategy, ranks)
        } else {
            match policy.strategy {
                Strategy::FillFirst => groups[0].1[0],
                // Like fill-first: the first provider group, then its soonest reset.
                Strategy::SoonestReset => {
                    let (provider, members) = &groups[0];
                    self.pick_within(provider, &model, members, Strategy::SoonestReset, ranks)
                }
                Strategy::WeightedRoundRobin => {
                    let all: Vec<&Credential> = ready.iter().map(|(c, _)| *c).collect();
                    self.pick_within(&providers_key, &model, &all, Strategy::WeightedRoundRobin, ranks)
                }
                Strategy::RoundRobin => {
                    // Go `pickMixed`: a cursor over provider segments sized by their ready
                    // counts, then that provider's own round-robin.
                    let total: usize = groups.iter().map(|(_, m)| m.len()).sum();
                    let cursor = self.cursors.entry((providers_key.clone(), model.clone())).or_default();
                    let slot = *cursor % total;
                    *cursor = slot + 1;
                    let mut start = 0;
                    let mut index = 0;
                    for (i, (_, members)) in groups.iter().enumerate() {
                        if slot < start + members.len() {
                            index = i;
                            break;
                        }
                        start += members.len();
                    }
                    let (provider, members) = &groups[index];
                    self.pick_within(provider, &model, members, Strategy::RoundRobin, ranks)
                }
            }
        };
        Some(picked)
    }

    fn pick_within<'a>(
        &mut self,
        scope: &str,
        model: &str,
        members: &[&'a Credential],
        strategy: Strategy,
        ranks: &Ranks<'_>,
    ) -> &'a Credential {
        let state = self.rotations.entry((scope.to_owned(), model.to_owned())).or_default();
        match strategy {
            Strategy::FillFirst => members[0],
            Strategy::SoonestReset => {
                let probes = &mut self.probes;
                let ranked: Vec<_> = members
                    .iter()
                    .map(|c| {
                        let windows = ranks(c);
                        let order = windows.order(probed(probes.get(&c.id), windows.rolled_over, windows.rollover_gap));
                        (order, windows.rolled_over, *c)
                    })
                    .collect();
                let best = ranked.iter().map(|(order, ..)| *order).min().expect("members");
                // Equal ranks take turns.
                let tied: Vec<_> = ranked.iter().filter(|(o, ..)| *o == best).collect();
                let (order, rolled_over, picked) = *tied
                    .iter()
                    .find(|(.., c)| c.id > state.last)
                    .copied()
                    .unwrap_or(tied[0]);
                if order.1 == 0 {
                    // Its one probe, spent whatever the answer: the next pick ranks it
                    // by what the answer reports, if anything.
                    probes.insert(picked.id.clone(), rolled_over);
                }
                state.last.clone_from(&picked.id);
                picked
            }
            Strategy::RoundRobin => {
                let picked = members
                    .iter()
                    .find(|c| c.id > state.last)
                    .copied()
                    .unwrap_or(members[0]);
                state.last.clone_from(&picked.id);
                picked
            }
            Strategy::WeightedRoundRobin => {
                if members
                    .iter()
                    .any(|c| state.weights.get(&c.id).is_some_and(|w| *w != weight(c)))
                {
                    state.current.clear();
                }
                if state.weights.len() > 1024 || state.current.len() > 1024 {
                    state.weights.retain(|id, _| members.iter().any(|c| c.id == *id));
                    state.current.retain(|id, _| members.iter().any(|c| c.id == *id));
                }
                let mut total = 0i64;
                let mut best = i64::MIN;
                let mut picked = members[0];
                for c in members {
                    let w = weight(c);
                    state.weights.insert(c.id.clone(), w);
                    let current = state.current.entry(c.id.clone()).or_default();
                    *current = current.saturating_add(w);
                    total = total.saturating_add(w);
                    if *current > best {
                        best = *current;
                        picked = c;
                    }
                }
                let current = state.current.get_mut(&picked.id).unwrap();
                *current = current.saturating_sub(total);
                picked
            }
        }
    }

    pub fn admits(&self, c: &Credential, policy: &Policy) -> bool {
        policy.strategy != Strategy::WeightedRoundRobin || weight(c) > 0
    }

    /// Applies one attempt's outcome (Go `MarkResult`).
    pub fn record(&mut self, c: &Credential, model: &str, outcome: &Outcome, policy: &Policy, now: Instant) {
        let model = canonical_model(model);
        let key = (c.id.clone(), model.to_owned());
        let error = match outcome {
            Outcome::Success => {
                // Active credential quota survives success on an in-flight sibling.
                let global = (c.id.clone(), String::new());
                if self.cooldowns.get(&global).is_some_and(|s| s.quota && s.deadline > now) {
                    return;
                }
                if self.cooldowns.get(&global).is_some_and(|s| s.deadline <= now) {
                    self.cooldowns.remove(&global);
                }
                self.cooldowns.remove(&key);
                return;
            }
            Outcome::Cancelled | Outcome::Neutral(_) => return,
            Outcome::Failure(error) => error,
        };
        let action = policy.error_action(c, error);
        if !action.cooldown {
            return;
        }
        let credential_quota = crate::classify::credential_scoped(error);
        let key = if credential_quota {
            (c.id.clone(), String::new())
        } else {
            key
        };
        // A forced cooldown applies the normal policy even when cooling is disabled.
        let disabled = policy.cooling_disabled(c) && !action.force_cooldown;
        let status = crate::classify::go_status(error);
        let text = crate::classify::error_text(error);
        let prev = self.cooldowns.get(&key);
        let prev_live = prev.filter(|s| s.deadline > now).map(|s| s.deadline);
        let mut level = prev.filter(|s| s.quota).map(|s| s.level).unwrap_or(0);
        let hint = error.retry_after.filter(|d| !d.is_zero());
        let mut quota = false;
        let duration = if crate::classify::is_model_support(status, &text) {
            (!disabled).then(|| hint.unwrap_or(Duration::from_secs(43200)))
        } else if crate::classify::is_cloudflare(status, &text) {
            quota = true;
            (!disabled).then(|| {
                let (d, next) = quota_backoff(level);
                level = next;
                d.max(Duration::from_secs(10))
            })
        } else if crate::classify::is_invalid_grant(status, &text) {
            (!disabled).then_some(Duration::from_secs(1800))
        } else {
            match status {
                401..=403 => (!disabled).then_some(Duration::from_secs(1800)),
                404 => (!disabled).then(|| hint.unwrap_or(Duration::from_secs(43200))),
                429 => {
                    quota = true;
                    if credential_quota && prev.is_none_or(|s| !s.quota) {
                        level = 0;
                    }
                    if disabled {
                        None
                    } else if let Some(hint) = error.retry_after {
                        // A present hint, even zero, gets the 10s floor (Go
                        // minQuotaCooldownFloor); only an absent one backs off.
                        Some(hint.max(Duration::from_secs(10)))
                    } else if let Some(prev) = prev.filter(|s| s.quota && s.deadline > now) {
                        // Go `quotaCooldownAfterFailure`: an active quota deadline is
                        // reused, not extended.
                        let deadline = prev.deadline;
                        if let Some(slot) = self.cooldowns.get_mut(&key) {
                            slot.error = text;
                        }
                        if credential_quota {
                            self.extend_siblings(c, deadline, now);
                        }
                        return;
                    } else {
                        let (d, next) = quota_backoff(level);
                        level = next;
                        Some(d)
                    }
                }
                408 | 500 | 502..=504 | 520..=526 => transient(policy, hint, disabled),
                _ => transient(policy, None, disabled),
            }
        };
        // Go falls back to the one-minute transient cooldown when a forced cooldown's
        // policy produced none (for example transient cooldowns disabled).
        let duration = match duration {
            None if action.force_cooldown => Some(Duration::from_secs(60)),
            d => d,
        };
        let Some(next) = duration.and_then(|d| now.checked_add(d)) else {
            self.cooldowns.remove(&key);
            return;
        };
        let deadline = prev_live.map_or(next, |p| p.max(next));
        let state = |deadline| Cooldown {
            deadline,
            level,
            status: error.status,
            quota,
            error: text.clone(),
            since: SystemTime::now(),
            credential: false,
        };
        self.cooldowns.insert(key, state(deadline));
        if credential_quota {
            self.extend_siblings(c, deadline, now);
            // Go also records the failing model's own quota state (reason `quota`).
            if !model.is_empty() {
                let own = (c.id.clone(), model.to_owned());
                let own_deadline = self
                    .cooldowns
                    .get(&own)
                    .filter(|s| s.deadline > now)
                    .map_or(deadline, |s| s.deadline.max(deadline));
                self.cooldowns.insert(own, state(own_deadline));
            }
        }
    }

    /// Go's credential-scoped 429: live sibling model states become quota cooldowns
    /// (`credential_quota`) lasting at least as long as the credential.
    fn extend_siblings(&mut self, c: &Credential, deadline: Instant, now: Instant) {
        let level = self
            .cooldowns
            .get(&(c.id.clone(), String::new()))
            .map_or(0, |s| s.level);
        for ((id, model), state) in &mut self.cooldowns {
            if *id == c.id && !model.is_empty() && state.deadline > now {
                state.deadline = state.deadline.max(deadline);
                state.quota = true;
                state.credential = true;
                state.level = level;
            }
        }
    }

    /// Clears every cooldown of one credential (Go `ResetQuota`), returning the models
    /// that were cooling.
    pub fn reset(&mut self, id: &str) -> Vec<String> {
        let mut models: Vec<String> = self
            .cooldowns
            .keys()
            .filter(|(cid, _)| cid == id)
            .map(|(_, m)| m.clone())
            .collect();
        self.cooldowns.retain(|(cid, _), _| cid != id);
        self.affinity.invalidate(id);
        self.probes.remove(id);
        models.sort();
        models
    }

    /// Go `cooldownStateRecordsForAuthLocked`: one record per live cooldown of `c`, by
    /// model (the credential-wide quota is the model-less record).
    pub fn records(&self, c: &Credential, now: Instant, wall: SystemTime) -> Vec<crate::cooldown_store::Record> {
        use crate::cooldown_store::{LastError, Quota, Record};
        let mut out: Vec<Record> = self
            .cooldowns
            .iter()
            .filter(|((id, _), state)| *id == c.id && state.deadline > now)
            .map(|((_, model), state)| {
                let at = wall + (state.deadline - now);
                let reason = match (model.is_empty() || state.credential, state.quota, state.status) {
                    (true, true, _) => "credential_quota".to_owned(),
                    (false, true, 429) => "quota".to_owned(),
                    (false, true, _) => "cloudflare challenge".to_owned(),
                    (_, false, _) => state.error.clone(),
                };
                Record {
                    provider: c.provider.trim().to_owned(),
                    auth_id: c.id.clone(),
                    model: model.clone(),
                    status: "cooling".into(),
                    next_retry_after: Some(at),
                    quota: if state.quota {
                        Quota {
                            exceeded: true,
                            reason: reason.clone(),
                            next_recover_at: Some(at),
                            backoff_level: state.level,
                            observed_at: None,
                        }
                    } else {
                        Quota::default()
                    },
                    reason,
                    last_error: Some(LastError {
                        message: state.error.clone(),
                        http_status: u32::from(state.status),
                        ..LastError::default()
                    }),
                    updated_at: Some(state.since),
                    auth_file: match &c.source {
                        cpa_core::credential::Source::File(path) => Some(path.clone()),
                        _ => None,
                    },
                }
            })
            .collect();
        out.sort_by(|a, b| a.model.cmp(&b.model));
        out
    }

    /// Go `restoreCooldownRecordLocked` for one live record of `c`. A model-less record
    /// restores the credential-wide quota; other model-less records aggregate the model
    /// records (Go recomputes them) and apply only when `c` has no model records.
    pub fn restore(
        &mut self,
        c: &Credential,
        record: &crate::cooldown_store::Record,
        has_model_records: bool,
        now: Instant,
        wall: SystemTime,
    ) -> bool {
        // Go blocks until the later of the retry deadline and, for a quota state, the
        // quota recovery time (`availabilityBlock`); one deadline here.
        // Go restores only records whose retry deadline is still ahead.
        let Some(retry) = record.next_retry_after.filter(|at| *at > wall) else {
            return false;
        };
        let at = match record.quota.next_recover_at.filter(|_| record.quota.exceeded) {
            Some(recover) => retry.max(recover),
            None => retry,
        };
        let Some(remaining) = at.duration_since(wall).ok() else {
            return false;
        };
        if remaining.is_zero() {
            return false;
        }
        let model = record.model.trim();
        if model.is_empty() && has_model_records && record.quota.reason != "credential_quota" {
            return false;
        }
        let deadline = now + remaining;
        let error = record.last_error.clone().unwrap_or_default();
        let status = match error.http_status {
            0 if record.quota.exceeded => 429,
            s => u16::try_from(s).unwrap_or(0),
        };
        let key = (c.id.clone(), canonical_model(model).to_owned());
        let deadline = self
            .cooldowns
            .get(&key)
            .filter(|prev| prev.deadline > now)
            .map_or(deadline, |prev| prev.deadline.max(deadline));
        self.cooldowns.insert(
            key,
            Cooldown {
                deadline,
                level: record.quota.backoff_level,
                status,
                quota: record.quota.exceeded,
                error: if error.message.is_empty() {
                    record.reason.clone()
                } else {
                    error.message
                },
                since: record.updated_at.unwrap_or(wall),
                credential: !model.is_empty() && record.quota.reason == "credential_quota",
            },
        );
        true
    }

    /// Go `clearDisabledCooldownStates`: drops cooldowns of credentials that are disabled
    /// or whose cooling the policy disables. Returns whether anything was cleared.
    pub fn clear_disabled(&mut self, credentials: &[std::sync::Arc<Credential>], policy: &Policy) -> bool {
        let before = self.cooldowns.len();
        self.cooldowns.retain(|(id, _), _| {
            credentials
                .iter()
                .find(|c| c.id == *id)
                .is_none_or(|c| !c.disabled && !policy.cooling_disabled(c))
        });
        self.cooldowns.len() != before
    }

    pub fn reconcile(&mut self, credentials: &[std::sync::Arc<Credential>]) {
        self.cooldowns
            .retain(|(id, _), _| credentials.iter().any(|c| c.id == *id));
        self.affinity.retain(|id| credentials.iter().any(|c| c.id == id));
        self.probes.retain(|id, _| credentials.iter().any(|c| c.id == *id));
    }
}

/// One active cooldown for management views (additive read API). `model` is empty for
/// a credential-wide cooldown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CooldownState {
    pub model: String,
    pub remaining: Duration,
    pub level: u32,
    pub status: u16,
    pub quota: bool,
}

impl Scheduler {
    /// Unexpired cooldowns of one credential, model keys sorted, credential-wide first.
    pub(crate) fn cooldowns_of(&self, id: &str, now: Instant) -> Vec<CooldownState> {
        let mut out: Vec<CooldownState> = self
            .cooldowns
            .iter()
            .filter(|((cid, _), s)| cid == id && s.deadline > now)
            .map(|((_, model), s)| CooldownState {
                model: model.clone(),
                remaining: s.deadline - now,
                level: s.level,
                status: s.status,
                quota: s.quota,
            })
            .collect();
        out.sort_by(|a, b| a.model.cmp(&b.model));
        out
    }

    /// Clears every cooldown of one credential; returns the model keys that had one.
    pub(crate) fn reset_cooldowns(&mut self, id: &str) -> Vec<String> {
        let mut models: Vec<String> = self
            .cooldowns
            .keys()
            .filter(|(cid, model)| cid == id && !model.is_empty())
            .map(|(_, model)| model.clone())
            .collect();
        models.sort();
        self.cooldowns.retain(|(cid, _), _| cid != id);
        // A reset credential is probed again under `soonest-reset`; affinity is kept.
        self.probes.remove(id);
        models
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn tag<'a>(creds: &[&'a Credential]) -> Vec<(&'a Credential, &'static str)> {
        creds.iter().map(|c| (*c, "claude")).collect()
    }

    fn cred(id: &str, extra: Value) -> Credential {
        let mut metadata = extra.as_object().unwrap().clone();
        metadata.insert("type".into(), "claude".into());
        Credential::from_file(Path::new("/mock"), &Path::new("/mock").join(id), metadata).unwrap()
    }

    fn selection(model: &str) -> Selection {
        Selection {
            provider: "claude".into(),
            model: model.into(),
            ..Default::default()
        }
    }

    #[test]
    fn normalization_matches_go_runtime_not_example_config() {
        for (raw, expected) in [
            (" WRR ", Strategy::WeightedRoundRobin),
            ("weightedroundrobin", Strategy::WeightedRoundRobin),
            ("weighted-round-robin", Strategy::WeightedRoundRobin),
            (" FF", Strategy::FillFirst),
            ("fillfirst", Strategy::FillFirst),
            ("fill-first", Strategy::FillFirst),
            ("soonest-reset", Strategy::SoonestReset),
            (" Reset-First ", Strategy::SoonestReset),
            ("resetfirst", Strategy::SoonestReset),
            ("", Strategy::RoundRobin),
            ("anything", Strategy::RoundRobin),
        ] {
            assert_eq!(
                normalize(RawRouting {
                    strategy: raw,
                    ..Default::default()
                })
                .strategy,
                expected
            );
        }
        for (ttl, seconds) in [
            ("", 3600),
            ("bad", 3600),
            ("-1h", 3600),
            ("0", 3600),
            ("0.5s", 1),
            ("0.0000000001s", 3600),
            ("1h30m", 5400),
            (" 2m ", 120),
        ] {
            assert_eq!(
                normalize(RawRouting {
                    session_affinity_ttl: ttl,
                    ..Default::default()
                })
                .session_affinity_ttl,
                Duration::from_secs(seconds),
                "{ttl}"
            );
        }
        let policy = normalize(RawRouting {
            request_retry: -1,
            max_retry_credentials: -5,
            max_retry_interval: -3,
            session_affinity_subagents: Some(false),
            ..Default::default()
        });
        assert_eq!(policy.request_retry, 0);
        assert_eq!(policy.max_retry_credentials, 0);
        assert_eq!(policy.max_retry_interval, Duration::ZERO);
        assert!(policy.session_affinity_subagents, "ignored when affinity disabled");
        assert!(
            !normalize(RawRouting {
                session_affinity: true,
                session_affinity_subagents: Some(false),
                ..Default::default()
            })
            .session_affinity_subagents
        );
        assert!(
            normalize(RawRouting {
                session_affinity: true,
                ..Default::default()
            })
            .session_affinity_subagents
        );
    }

    #[test]
    fn go_duration_decimal_compound_units_and_overflow() {
        for (raw, expected) in [
            ("1h2m3.4s", 3_723_400_000_000),
            (".5s", 500_000_000),
            ("+1.000000001s", 1_000_000_001),
            ("1.s", 1_000_000_000),
            ("1µs2μs3us", 6000),
            ("0.3333333333333333333h", 1_200_000_000_000),
            ("0.100000000000000000000h", 360_000_000_000),
            ("-9223372036854775808ns", i64::MIN),
            ("9223372036854775807ns", i64::MAX),
            ("0.9ns", 0),
        ] {
            assert_eq!(go_duration(raw), Some(expected), "{raw}");
        }
        for raw in [
            "1d",
            ".s",
            "1",
            "1h 2m",
            "1s-2s",
            "1e3s",
            "9223372036854775808ns",
            "-9223372036854775809ns",
        ] {
            assert_eq!(go_duration(raw), None, "{raw}");
        }
    }

    #[test]
    fn rotation_tracks_previous_identity_and_provider_model_keys() {
        let a = cred("a", serde_json::json!({}));
        let b = cred("b", serde_json::json!({}));
        let c = cred("c", serde_json::json!({}));
        let p = Policy::default();
        let now = Instant::now();
        let mut s = Scheduler::default();
        assert_eq!(s.pick(&tag(&[&c, &b, &a]), &selection("m"), &p, now).unwrap().id, "a");
        assert_eq!(s.pick(&tag(&[&a, &c]), &selection("m"), &p, now).unwrap().id, "c");
        assert_eq!(s.pick(&tag(&[&a, &b, &c]), &selection("m"), &p, now).unwrap().id, "a");
        assert_eq!(s.pick(&tag(&[&a, &b]), &selection("other"), &p, now).unwrap().id, "a");
        assert_eq!(s.pick(&tag(&[&a, &b]), &selection("m(high)"), &p, now).unwrap().id, "b");
    }

    #[test]
    fn smooth_weights_keep_signed_credits_across_temporary_exclusions() {
        let a = cred("a", serde_json::json!({"weight":5}));
        let b = cred("b", serde_json::json!({"weight":1}));
        let c = cred("c", serde_json::json!({"weight":1}));
        let p = Policy {
            strategy: Strategy::WeightedRoundRobin,
            ..Default::default()
        };
        let now = Instant::now();
        let mut s = Scheduler::default();
        let picks: Vec<_> = (0..7)
            .map(|_| {
                s.pick(&tag(&[&a, &b, &c]), &selection("m"), &p, now)
                    .unwrap()
                    .id
                    .clone()
            })
            .collect();
        assert_eq!(picks, ["a", "a", "b", "a", "c", "a", "a"]);
        let mut s = Scheduler::default();
        assert_eq!(s.pick(&tag(&[&a, &b, &c]), &selection("m"), &p, now).unwrap().id, "a");
        assert_eq!(s.pick(&tag(&[&b, &c]), &selection("m"), &p, now).unwrap().id, "b");
        assert_eq!(s.pick(&tag(&[&a, &b, &c]), &selection("m"), &p, now).unwrap().id, "a");
        assert_eq!(s.pick(&tag(&[&a, &b, &c]), &selection("m"), &p, now).unwrap().id, "c");
        for v in [
            serde_json::json!(0),
            serde_json::json!(-3),
            serde_json::json!(1_000_001),
            serde_json::json!(1.5),
            serde_json::json!("bad"),
        ] {
            assert!(!s.admits(&cred("off", serde_json::json!({"weight":v})), &p));
        }
        assert_eq!(weight(&cred("max", serde_json::json!({"weight":1_000_000}))), 1_000_000);
        assert_eq!(weight(&cred("float", serde_json::json!({"weight":2.0}))), 2);
    }

    #[test]
    fn affinity_beats_recovered_priority_until_unusable_or_expired() {
        let low = cred("a", serde_json::json!({"priority":-1}));
        let high = cred("b", serde_json::json!({"priority":9}));
        let p = Policy {
            strategy: Strategy::FillFirst,
            session_affinity: true,
            session_affinity_ttl: Duration::from_secs(10),
            ..Default::default()
        };
        let sel = Selection {
            session: Some("session".into()),
            ..selection("m")
        };
        let now = Instant::now();
        let mut s = Scheduler::default();
        assert_eq!(s.pick(&tag(&[&low]), &sel, &p, now).unwrap().id, "a");
        assert_eq!(
            s.pick(&tag(&[&low, &high]), &sel, &p, now + Duration::from_secs(5))
                .unwrap()
                .id,
            "a"
        );
        assert_eq!(
            s.pick(&tag(&[&low, &high]), &sel, &p, now + Duration::from_secs(15))
                .unwrap()
                .id,
            "b"
        );
        assert_eq!(
            s.pick(&tag(&[&low]), &sel, &p, now + Duration::from_secs(16))
                .unwrap()
                .id,
            "a"
        );
        assert_eq!(s.pick(&tag(&[&low, &high]), &selection("m"), &p, now).unwrap().id, "b");
    }

    #[test]
    fn cooldown_deadlines_floor_backoff_and_sibling_success() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let now = Instant::now();
        let mut s = Scheduler::default();
        let fail = |status, scope, hint| {
            let mut e = ExecError::local(status, scope, "failure");
            e.retry_after = hint;
            Outcome::Failure(e)
        };
        s.record(&c, "sibling", &fail(404, FailureScope::Model, None), &p, now);
        s.record(
            &c,
            "m",
            &fail(429, FailureScope::Credential, Some(Duration::from_secs(2))),
            &p,
            now,
        );
        assert_eq!(s.wait(&c, "m", now), Some(Duration::from_secs(10)));
        assert_eq!(s.wait(&c, "new", now), Some(Duration::from_secs(10)));
        assert_eq!(
            s.wait(&c, "sibling", now),
            Some(Duration::from_secs(43200)),
            "quota must not shorten sibling"
        );
        s.record(&c, "sibling", &Outcome::Success, &p, now + Duration::from_secs(1));
        assert_eq!(
            s.wait(&c, "sibling", now),
            Some(Duration::from_secs(43200)),
            "inflight success cannot clear live quota"
        );
        assert_eq!(
            s.wait(&c, "new", now + Duration::from_secs(10)),
            None,
            "deadline boundary"
        );
        s.record(&c, "new", &Outcome::Success, &p, now + Duration::from_secs(10));
        assert!(s.wait(&c, "sibling", now + Duration::from_secs(10)).is_some());
        let mut s = Scheduler::default();
        s.record(&c, "m", &fail(429, FailureScope::Model, None), &p, now);
        assert_eq!(s.wait(&c, "m(high)", now), Some(Duration::from_secs(1)));
        s.record(&c, "m", &fail(429, FailureScope::Model, None), &p, now);
        assert_eq!(
            s.wait(&c, "m", now),
            Some(Duration::from_secs(1)),
            "active quota reused"
        );
        s.record(
            &c,
            "m",
            &fail(429, FailureScope::Model, None),
            &p,
            now + Duration::from_secs(1),
        );
        assert_eq!(
            s.wait(&c, "m", now + Duration::from_secs(1)),
            Some(Duration::from_secs(2))
        );
        assert_eq!(s.wait(&c, "other", now), None, "model failure stays local");
        for (status, seconds) in [
            (401, 1800),
            (402, 1800),
            (403, 1800),
            (404, 43200),
            (408, 60),
            (500, 60),
            (526, 60),
        ] {
            let mut s = Scheduler::default();
            s.record(&c, "m", &fail(status, FailureScope::Model, None), &p, now);
            assert_eq!(
                s.wait(&c, "m", now),
                Some(Duration::from_secs(seconds)),
                "status {status}"
            );
        }
    }

    /// Go `Manager.MarkResult` cooldowns: goldens from tests/reference/server/main.go,
    /// replayed with upstream-shaped errors (headers present, executor scopes).
    #[test]
    fn cooldowns_match_go_mark_result() {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/server_go.json")).unwrap();
        let cases = fixture["cooldown"].as_array().unwrap();
        assert_eq!(cases.len(), 25);
        let policy = Policy::default();
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let c = cred(&format!("{name}.json"), serde_json::json!({}));
            let mut s = Scheduler::default();
            let now = Instant::now();
            for step in case["steps"].as_array().unwrap() {
                let status = step["status"].as_u64().unwrap() as u16;
                let scope = if step["credential_scope"].as_bool().unwrap() {
                    FailureScope::Credential
                } else {
                    match status {
                        429 => FailureScope::Model,
                        401..=403 | 408 | 500.. => FailureScope::Credential,
                        _ => FailureScope::Request,
                    }
                };
                let mut error = ExecError::local(status, scope, step["message"].as_str().unwrap());
                error
                    .headers
                    .insert("content-type", "application/json".parse().unwrap());
                let hint = step["retry_after_ms"].as_i64().unwrap();
                error.retry_after = (hint >= 0).then(|| Duration::from_millis(hint as u64));
                s.record(
                    &c,
                    step["model"].as_str().unwrap(),
                    &Outcome::Failure(error),
                    &policy,
                    now,
                );
            }
            for (model, seconds) in case["seconds"].as_object().unwrap() {
                let wait = s.wait(&c, model, now).unwrap_or_default();
                assert_eq!(
                    wait.as_secs_f64().round() as u64,
                    seconds.as_u64().unwrap(),
                    "{name} {model} wait"
                );
                let quota = case["quota"][model].as_bool().unwrap();
                // Go keeps `Quota.Exceeded` on an expired state; only a live one matters.
                assert_eq!(
                    s.quota_cooling(&c, model, now),
                    quota && wait > Duration::ZERO,
                    "{name} {model} quota"
                );
            }
        }
    }

    /// Go `SessionAffinitySelector` (`Enrich`, `Pick` with fill-first, `OnResult`):
    /// goldens from tests/reference/server/main.go.
    #[test]
    fn session_affinity_matches_go_selector() {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/server_go.json")).unwrap();
        let cases = fixture["affinity"].as_array().unwrap();
        assert_eq!(cases.len(), 11);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let creds: Vec<Credential> = ["a", "b", "c"]
                .iter()
                .map(|id| {
                    let mut c = cred(id, serde_json::json!({}));
                    if let Some(p) = case["priorities"][*id].as_str() {
                        c.attributes.insert("priority".into(), p.into());
                    }
                    c
                })
                .collect();
            let policy = Policy {
                strategy: Strategy::FillFirst,
                session_affinity: true,
                session_affinity_subagents: case["subagent_affinity"].as_bool().unwrap(),
                ..Default::default()
            };
            let mut s = Scheduler::default();
            let now = Instant::now();
            for (i, step) in case["steps"].as_array().unwrap().iter().enumerate() {
                let mut headers = axum::http::HeaderMap::new();
                for pair in step["headers"].as_array().into_iter().flatten() {
                    headers.append(
                        axum::http::HeaderName::from_bytes(pair[0].as_str().unwrap().as_bytes()).unwrap(),
                        pair[1].as_str().unwrap().parse().unwrap(),
                    );
                }
                let payload = step["payload"].as_str().unwrap_or_default().as_bytes();
                let session = crate::session::resolve(cpa_core::format::Format::OpenAI, &headers, payload, None, "");
                let sel = Selection {
                    session: session.id,
                    session_parent: session.parent,
                    session_fork: session.fork,
                    ..selection("m")
                };
                let find = |id: &str| creds.iter().find(|c| c.id == id).unwrap();
                match step["op"].as_str().unwrap() {
                    "pick" => {
                        let available: Vec<&Credential> = step["available"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|id| find(id.as_str().unwrap()))
                            .collect();
                        let picked = s.pick(&tag(&available), &sel, &policy, now).unwrap();
                        assert_eq!(picked.id, step["picked"].as_str().unwrap(), "{name} step {i}");
                    }
                    op => {
                        let outcome = if op == "ok" {
                            Outcome::Success
                        } else {
                            let status = step["status"].as_u64().unwrap() as u16;
                            let scope = if status >= 500 {
                                FailureScope::Credential
                            } else {
                                FailureScope::Request
                            };
                            let mut e = ExecError::local(status, scope, step["message"].as_str().unwrap());
                            e.headers.insert("content-type", "text/plain".parse().unwrap());
                            Outcome::Failure(e)
                        };
                        s.session_result(find(step["auth"].as_str().unwrap()), &sel, &outcome, &policy, now);
                    }
                }
            }
        }
    }

    #[test]
    fn policy_change_resets_rotation_and_affinity_but_keeps_cooldowns() {
        let a = cred("a", serde_json::json!({}));
        let b = cred("b", serde_json::json!({}));
        let p = Policy {
            session_affinity: true,
            ..Default::default()
        };
        let now = Instant::now();
        let mut s = Scheduler::default();
        let mut selection = selection("m");
        selection.session = Some("session".into());
        assert_eq!(s.pick(&tag(&[&a, &b]), &selection, &p, now).unwrap().id, "a");
        s.record(
            &a,
            "other",
            &Outcome::Failure(ExecError::local(429, FailureScope::Model, "quota")),
            &p,
            now,
        );
        let mut next = p.clone();
        next.request_retry = 2;
        s.configure(&p, &next);
        assert!(!s.rotations.is_empty(), "retry-only change preserves selector state");
        assert!(!s.affinity.is_empty());
        next.session_affinity_ttl = Duration::from_secs(5);
        s.configure(&p, &next);
        assert!(s.rotations.is_empty());
        assert!(s.affinity.is_empty());
        assert_eq!(s.wait(&a, "other", now), Some(Duration::from_secs(1)));
    }

    #[test]
    fn forced_cooling_uses_fallback_and_quota_does_not_reuse_transient_state() {
        let now = Instant::now();
        // Go: a forced cooldown re-enables the normal policy despite disable_cooling
        // (conductor_cooldown.go MarkResult), so the status decides the duration.
        for (status, seconds) in [(401, 1800), (404, 43200), (429, 1), (503, 60)] {
            let c = cred(
                "a",
                serde_json::json!({"disable_cooling":true,
                "request_scoped_errors":[{"status":status,"match":["forced"],"action":"stop-and-cooldown"}]}),
            );
            let mut s = Scheduler::default();
            s.record(
                &c,
                "m",
                &Outcome::Failure(ExecError::local(status, FailureScope::Credential, "forced")),
                &Policy::default(),
                now,
            );
            assert_eq!(
                s.wait(&c, "m", now),
                Some(Duration::from_secs(seconds)),
                "status {status}"
            );
        }
        // With transient cooldowns disabled, the forced cooldown falls back to one minute.
        let c = cred(
            "a",
            serde_json::json!({"request_scoped_errors":[{"status":503,"match":["forced"],"action":"continue-and-cooldown"}]}),
        );
        let no_transient = Policy {
            transient_error_cooldown_seconds: -1,
            ..Policy::default()
        };
        let mut s = Scheduler::default();
        let forced = Outcome::Failure(ExecError::local(503, FailureScope::Credential, "forced"));
        s.record(&c, "m", &forced, &no_transient, now);
        assert_eq!(s.wait(&c, "m", now), Some(Duration::from_secs(60)));
        let plain = Outcome::Failure(ExecError::local(503, FailureScope::Credential, "plain"));
        let mut s = Scheduler::default();
        s.record(&c, "m", &plain, &no_transient, now);
        assert_eq!(
            s.wait(&c, "m", now),
            None,
            "negative transient seconds disable 503 cooldowns"
        );
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let mut s = Scheduler::default();
        s.record(
            &c,
            "m",
            &Outcome::Failure(ExecError::local(503, FailureScope::Model, "transient")),
            &p,
            now,
        );
        s.record(
            &c,
            "m",
            &Outcome::Failure(ExecError::local(429, FailureScope::Model, "quota")),
            &p,
            now,
        );
        assert!(
            s.quota_cooling(&c, "m", now),
            "quota must replace transient classification"
        );
        assert_eq!(
            s.wait(&c, "m", now),
            Some(Duration::from_secs(60)),
            "live deadline never shortened"
        );
        let mut hint = ExecError::local(418, FailureScope::Model, "other status");
        hint.retry_after = Some(Duration::from_secs(900));
        s.record(&c, "other", &Outcome::Failure(hint), &p, now);
        assert_eq!(
            s.wait(&c, "other", now),
            Some(Duration::from_secs(60)),
            "unlisted statuses ignore hint"
        );
    }

    #[test]
    fn rules_are_ordered_status_and_body_matches_with_canonical_precedence() {
        let c = cred(
            "a",
            serde_json::json!({"disable_cooling":true, "request_scoped_errors":[
            {"status":429,"match":[],"action":"stop"},
            {"status":500,"match":["quota"],"action":"stop"},
            {"status":429,"match-regexr":["[", "quota [0-9]+"],"action":" CONTINUE-AND-COOLDOWN "},
            {"status":429,"match":["quota"],"action":"stop"}]}),
        );
        let p = Policy::default();
        let e = ExecError::local(429, FailureScope::Request, "quota 12");
        let action = p.error_action(&c, &e);
        assert!(!action.stop);
        assert!(action.cooldown && action.force_cooldown);
        let now = Instant::now();
        let mut s = Scheduler::default();
        s.record(&c, "m", &Outcome::Failure(e), &p, now);
        assert!(
            s.wait(&c, "m", now).is_some(),
            "force cooling bypasses disabled cooling"
        );
        let c = cred(
            "a",
            serde_json::json!({"request_retry":0, "request-retry":5,
            "disable_cooling":false,"disable-cooling":true}),
        );
        assert_eq!(p.retry_limit(&c), 0);
        assert!(!p.cooling_disabled(&c));
        let legacy = cred(
            "legacy",
            serde_json::json!({"request_retry":null,"request-retry":2,
            "disable_cooling":"TrUe","disable-cooling":1}),
        );
        assert_eq!(
            p.retry_limit(&legacy),
            2,
            "invalid canonical override permits legacy fallback"
        );
        assert!(
            p.cooling_disabled(&legacy),
            "Go ParseBool rejects mixed case; numeric legacy accepted"
        );
        assert!(!p.cooling_disabled(&cred("numeric", serde_json::json!({"disable_cooling":0}))));
        assert_eq!(
            Policy {
                request_retry: 3,
                ..Default::default()
            }
            .retry_limit(&cred("b", serde_json::json!({"request_retry":"-1"}))),
            3
        );
        // Go `providerCoolingOverrideForAuth`: the first enabled compat entry named by
        // compat_name, provider_key or provider; credential metadata still wins.
        let cfg = cpa_core::config::Config::parse(
            "openai-compatibility:\n  - {name: skipped, base-url: '', disable-cooling: false}\n  - {name: Router, base-url: http://r.invalid, disable-cooling: true}\n  - {name: plain, base-url: http://p.invalid}\n",
        )
        .unwrap();
        let mut p = Policy {
            compat_disable_cooling: compat_cooling(&cfg),
            ..Default::default()
        };
        assert_eq!(
            p.compat_disable_cooling,
            [("Router".to_owned(), Some(true)), ("plain".to_owned(), None)],
            "legacy openai-compatibility is read through the v8 document"
        );
        let compat = |attrs: &[(&str, &str)], meta: Value| {
            let mut c = cred("compat", meta);
            c.provider = "openai-compatibility".into();
            for (k, v) in attrs {
                c.attributes.insert((*k).into(), (*v).into());
            }
            c
        };
        assert!(p.cooling_disabled(&compat(&[("compat_name", "router")], serde_json::json!({}))));
        assert!(p.cooling_disabled(&compat(&[("provider_key", "ROUTER")], serde_json::json!({}))));
        assert!(!p.cooling_disabled(&compat(
            &[("compat_name", "router")],
            serde_json::json!({"disable_cooling": false})
        )));
        p.disable_cooling = true;
        assert!(
            p.cooling_disabled(&compat(&[("compat_name", "plain")], serde_json::json!({}))),
            "an entry without disable-cooling defers to global"
        );
        p.disable_cooling = false;
        assert!(
            !p.cooling_disabled(&cred("claude-file", serde_json::json!({}))),
            "credentials outside OpenAI compatibility ignore entries"
        );
        for scope in [FailureScope::Request, FailureScope::Transport] {
            let mut s = Scheduler::default();
            s.record(
                &c,
                "m",
                &Outcome::Failure(ExecError::local(502, scope, "fault")),
                &p,
                now,
            );
            assert_eq!(s.wait(&c, "m", now), None);
        }
    }

    // ---- soonest-reset (cliproxy-rs addition) ----

    const DAY: u64 = 24 * 3600;

    fn epoch(t: SystemTime) -> String {
        t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs().to_string()
    }

    /// Claude's measured headers observed at `now`: the 7-day reset `weekly` seconds
    /// later and the 5-hour window's status and reset.
    fn claude(now: SystemTime, weekly: u64, five_hour: &str, five_hour_reset: SystemTime) -> Windows {
        claude_at(now, now, weekly, five_hour, five_hour_reset)
    }

    /// [`claude`] observed at `observed` and read at `now`.
    fn claude_at(
        observed: SystemTime,
        now: SystemTime,
        weekly: u64,
        five_hour: &str,
        five_hour_reset: SystemTime,
    ) -> Windows {
        let signals: std::collections::BTreeMap<String, String> = [
            ("Anthropic-Ratelimit-Unified-7d-Status", "allowed".to_owned()),
            ("Anthropic-Ratelimit-Unified-7d-Utilization", "0.40".to_owned()),
            (
                "Anthropic-Ratelimit-Unified-7d-Reset",
                epoch(observed + Duration::from_secs(weekly)),
            ),
            ("Anthropic-Ratelimit-Unified-5h-Status", five_hour.to_owned()),
            ("Anthropic-Ratelimit-Unified-5h-Reset", epoch(five_hour_reset)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        Windows::observed("claude", &signals, observed, now)
    }

    fn soonest() -> Policy {
        Policy {
            strategy: Strategy::SoonestReset,
            ..Default::default()
        }
    }

    /// The asymmetric case: `a` resets in four days and `b` in one, so fill-first (id
    /// order) would take `a` and round-robin would alternate.
    #[test]
    fn soonest_reset_spends_the_earliest_weekly_window_first() {
        let wall = SystemTime::now();
        let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
        let in_an_hour = wall + Duration::from_secs(3600);
        // Both observed at `wall`, read at `now`; `b`'s 5-hour window in `b_five_hour`.
        let ranks_at = |b_five_hour: &'static str, now: SystemTime| {
            let (wa, wb) = (
                claude_at(wall, now, 4 * DAY, "allowed", in_an_hour),
                claude_at(wall, now, DAY, b_five_hour, in_an_hour),
            );
            move |cred: &Credential| if cred.id == "a" { wa } else { wb }
        };
        let policy = soonest();
        let mut s = Scheduler::default();
        let now = Instant::now();
        let ranks = ranks_at("allowed", wall);
        for _ in 0..3 {
            let picked = s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &policy, &ranks, now);
            assert_eq!(picked.unwrap().id, "b", "the sooner weekly reset is used up first");
        }
        // `b` cools on a 429: the next soonest takes over.
        s.record(
            &b,
            "m",
            &Outcome::Failure(ExecError::local(429, FailureScope::Credential, "quota")),
            &policy,
            now,
        );
        let ready: Vec<&Credential> = [&a, &b].into_iter().filter(|x| s.wait(x, "m", now).is_none()).collect();
        assert_eq!(ready.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["a"]);
        assert_eq!(
            s.pick_ranked(&tag(&ready), &selection("m"), &policy, &ranks, now)
                .unwrap()
                .id,
            "a"
        );
        // Without the cooldown, `b`'s used-up 5-hour window also hands over to `a`...
        let mut s = Scheduler::default();
        let exhausted = ranks_at("rejected", wall);
        for _ in 0..2 {
            assert_eq!(
                s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &policy, &exhausted, now)
                    .unwrap()
                    .id,
                "a"
            );
        }
        // ...until that window resets, an hour later.
        let reset = ranks_at("rejected", in_an_hour + Duration::from_secs(1));
        assert_eq!(
            s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &policy, &reset, now)
                .unwrap()
                .id,
            "b"
        );
    }

    /// Accounts with no known reset get one probe request each, then rank by what they
    /// reported; one that reported nothing comes after the known resets.
    #[test]
    fn soonest_reset_probes_unknown_accounts_once_then_ranks_them() {
        let wall = SystemTime::now();
        let creds: Vec<Credential> = ["a", "b", "c", "d"]
            .iter()
            .map(|id| cred(id, serde_json::json!({})))
            .collect();
        let [a, b, c, d] = [&creds[0], &creds[1], &creds[2], &creds[3]];
        let in_an_hour = wall + Duration::from_secs(3600);
        // `d` resets in 3 days, `c` is used up, `a` and `b` were never observed.
        let known = |x: &Credential| match x.id.as_str() {
            "c" => claude(wall, DAY, "rejected", in_an_hour),
            "d" => claude(wall, 3 * DAY, "allowed", in_an_hour),
            _ => Windows::default(),
        };
        let policy = soonest();
        let mut s = Scheduler::default();
        let now = Instant::now();
        let pick = |s: &mut Scheduler, set: &[&Credential], ranks: &Ranks<'_>| {
            s.pick_ranked(&tag(set), &selection("m"), &policy, ranks, now)
                .unwrap()
                .id
                .clone()
        };
        // One probe each for the unknown accounts, then the known reset.
        let order: Vec<String> = (0..4).map(|_| pick(&mut s, &[a, b, c, d], &known)).collect();
        assert_eq!(order, ["a", "b", "d", "d"]);
        // `a`'s probe answered with a reset in 12 hours: it now ranks by it, ahead of
        // `d`; `b`'s answer reported nothing, so it stays behind the known resets.
        let learned = |x: &Credential| match x.id.as_str() {
            "a" => claude(wall + Duration::from_secs(1), 12 * 3600, "allowed", in_an_hour),
            _ => known(x),
        };
        assert_eq!(pick(&mut s, &[a, b, c, d], &learned), "a");
        assert_eq!(pick(&mut s, &[b, c, d], &learned), "d");
        assert_eq!(
            pick(&mut s, &[b, c], &learned),
            "b",
            "probed and unknown, still before used-up"
        );
        assert_eq!(
            pick(&mut s, &[c], &learned),
            "c",
            "a used-up account is the last resort"
        );
        // When `d`'s weekly reset passes, it is unknown again and gets a new probe.
        let passed = |x: &Credential| match x.id.as_str() {
            "d" => claude_at(
                wall,
                wall + Duration::from_secs(3 * DAY + 1),
                3 * DAY,
                "allowed",
                in_an_hour,
            ),
            _ => learned(x),
        };
        assert_eq!(pick(&mut s, &[a, b, d], &passed), "d");
        assert_eq!(pick(&mut s, &[a, b, d], &passed), "a");
    }

    #[test]
    fn soonest_reset_rotates_equal_ranks() {
        let wall = SystemTime::now();
        let creds: Vec<Credential> = ["a", "b"].iter().map(|id| cred(id, serde_json::json!({}))).collect();
        let in_an_hour = wall + Duration::from_secs(3600);
        // The same reset for both.
        let ranks = |_: &Credential| claude(wall, 2 * DAY, "allowed", in_an_hour);
        let mut s = Scheduler::default();
        let now = Instant::now();
        let turns: Vec<String> = (0..4)
            .map(|_| {
                s.pick_ranked(&tag(&[&creds[0], &creds[1]]), &selection("m"), &soonest(), &ranks, now)
                    .unwrap()
                    .id
                    .clone()
            })
            .collect();
        assert_eq!(turns, ["a", "b", "a", "b"]);
    }

    #[test]
    fn soonest_reset_never_moves_a_session_pin() {
        let wall = SystemTime::now();
        let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
        let in_an_hour = wall + Duration::from_secs(3600);
        let ranks = |x: &Credential| claude(wall, if x.id == "a" { 4 * DAY } else { DAY }, "allowed", in_an_hour);
        let policy = Policy {
            session_affinity: true,
            ..soonest()
        };
        let mut s = Scheduler::default();
        let now = Instant::now();
        let mut pinned = selection("m");
        pinned.session = Some("claude:thread-1".into());
        // The thread starts while only `a` is available, and keeps `a` afterwards.
        assert_eq!(
            s.pick_ranked(&tag(&[&a]), &pinned, &policy, &ranks, now).unwrap().id,
            "a"
        );
        for _ in 0..2 {
            assert_eq!(
                s.pick_ranked(&tag(&[&a, &b]), &pinned, &policy, &ranks, now)
                    .unwrap()
                    .id,
                "a"
            );
        }
        // A new thread starts on the soonest reset.
        let mut other = selection("m");
        other.session = Some("claude:thread-2".into());
        assert_eq!(
            s.pick_ranked(&tag(&[&a, &b]), &other, &policy, &ranks, now).unwrap().id,
            "b"
        );
    }

    #[test]
    fn default_strategy_stays_round_robin_and_ignores_resets() {
        assert_eq!(Policy::default().strategy, Strategy::RoundRobin);
        let wall = SystemTime::now();
        let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
        let ranks = |x: &Credential| claude(wall, if x.id == "a" { 4 * DAY } else { DAY }, "allowed", wall);
        let mut s = Scheduler::default();
        let now = Instant::now();
        let turns: Vec<String> = (0..4)
            .map(|_| {
                s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &Policy::default(), &ranks, now)
                    .unwrap()
                    .id
                    .clone()
            })
            .collect();
        assert_eq!(turns, ["a", "b", "a", "b"]);
    }

    #[test]
    fn soonest_reset_in_mixed_providers_stays_in_the_first_group() {
        let wall = SystemTime::now();
        let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
        let x = cred("x", serde_json::json!({}));
        let in_an_hour = wall + Duration::from_secs(3600);
        // The second provider's `x` resets soonest, but fill-first order picks the group.
        let ranks = |c: &Credential| {
            let days = match c.id.as_str() {
                "a" => 4,
                "b" => 2,
                _ => 1,
            };
            claude(wall, days * DAY, "allowed", in_an_hour)
        };
        let sel = Selection {
            providers: vec!["claude".into(), "codex".into()],
            model: "m".into(),
            ..Default::default()
        };
        let candidates = [(&a, "claude"), (&b, "claude"), (&x, "codex")];
        let mut s = Scheduler::default();
        let picked = s.pick_ranked(&candidates, &sel, &soonest(), &ranks, Instant::now());
        assert_eq!(picked.unwrap().id, "b");
    }

    #[test]
    fn windows_read_claude_and_codex_signals() {
        let now = SystemTime::now();
        let map = |pairs: &[(&str, String)]| -> std::collections::BTreeMap<String, String> {
            pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
        };
        // A weekly reset already behind is unknown, not "soonest".
        let stale = map(&[(
            "Anthropic-Ratelimit-Unified-7d-Reset",
            epoch(now - Duration::from_secs(60)),
        )]);
        let w = Windows::observed("claude", &stale, now, now);
        assert_eq!(
            (w.weekly_reset, w.rolled_over.map(epoch), w.exhausted),
            (None, Some(epoch(now - Duration::from_secs(60))), false)
        );
        // Utilization 1.0 exhausts the 5-hour window until its reset.
        let full = map(&[
            ("Anthropic-Ratelimit-Unified-5h-Utilization", "1.0".into()),
            (
                "Anthropic-Ratelimit-Unified-5h-Reset",
                epoch(now + Duration::from_secs(600)),
            ),
        ]);
        assert!(Windows::observed("claude", &full, now, now).exhausted);
        assert!(!Windows::observed("claude", &full, now, now + Duration::from_secs(601)).exhausted);
        // Codex: the longer window is weekly whichever header carries it; reset-after
        // counts from the observation.
        let observed = now - Duration::from_secs(100);
        let codex = map(&[
            ("X-Codex-Primary-Used-Percent", "100".into()),
            ("X-Codex-Primary-Window-Minutes", "300".into()),
            ("X-Codex-Primary-Reset-After-Seconds", "3600".into()),
            ("X-Codex-Secondary-Used-Percent", "40".into()),
            ("X-Codex-Secondary-Window-Minutes", "10080".into()),
            ("X-Codex-Secondary-Reset-At", epoch(now + Duration::from_secs(2 * DAY))),
        ]);
        let w = Windows::observed("codex", &codex, observed, now);
        assert!(w.exhausted, "the 5-hour primary is used up");
        assert_eq!(
            w.weekly_reset.map(epoch),
            Some(epoch(now + Duration::from_secs(2 * DAY)))
        );
        assert!(!Windows::observed("codex", &codex, observed, observed + Duration::from_secs(3601)).exhausted);
        let weekly_primary = map(&[
            ("X-Codex-Primary-Used-Percent", "51".into()),
            ("X-Codex-Primary-Window-Minutes", "10080".into()),
            ("X-Codex-Primary-Reset-At", epoch(now + Duration::from_secs(3 * DAY))),
        ]);
        let w = Windows::observed("codex", &weekly_primary, now, now);
        assert_eq!(
            (w.weekly_reset.map(epoch), w.exhausted),
            (Some(epoch(now + Duration::from_secs(3 * DAY))), false)
        );
        let reached = map(&[("X-Codex-Limit-Reached", "True".into())]);
        assert!(Windows::observed("codex", &reached, now, now).exhausted);
        // Another provider's headers mean nothing here.
        let w = Windows::observed("gemini", &codex, now, now);
        assert_eq!((w.weekly_reset, w.exhausted), (None, false));
    }

    // ---- soonest-reset review fixes ----

    fn signals(pairs: &[(&str, String)]) -> std::collections::BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
    }

    /// P1: an account answering with quota signals but no weekly reset is probed once;
    /// each new answer (a newer observation) must not re-arm the probe, or it would take
    /// every request ahead of an account with a known reset.
    #[test]
    fn soonest_reset_answers_without_a_reset_do_not_re_arm_the_probe() {
        let wall = SystemTime::now();
        let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
        let policy = soonest();
        let mut s = Scheduler::default();
        let now = Instant::now();
        let b_known = claude(wall, 2 * DAY, "allowed", wall + Duration::from_secs(3600));
        let mut picks = Vec::new();
        for i in 0..5u64 {
            // `a` has answered `i` times, each time with a fresh 5-hour status only.
            let a_seen = (i > 0).then(|| {
                let seen = wall + Duration::from_secs(i);
                Windows::observed(
                    "claude",
                    &signals(&[("Anthropic-Ratelimit-Unified-5h-Status", "allowed".into())]),
                    seen,
                    seen,
                )
            });
            let ranks = |x: &Credential| {
                if x.id == "a" {
                    a_seen.unwrap_or_default()
                } else {
                    b_known
                }
            };
            picks.push(
                s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &policy, &ranks, now)
                    .unwrap()
                    .id
                    .clone(),
            );
        }
        assert_eq!(picks, ["a", "b", "b", "b", "b"]);
    }

    /// The probe is reserved when picked: one that fails before any header arrives (no
    /// new observation) is spent, so the next picks go to the known reset.
    #[test]
    fn soonest_reset_a_probe_that_fails_before_headers_is_spent() {
        let wall = SystemTime::now();
        let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
        let b_known = claude(wall, 2 * DAY, "allowed", wall + Duration::from_secs(3600));
        let ranks = |x: &Credential| if x.id == "a" { Windows::default() } else { b_known };
        let mut s = Scheduler::default();
        let now = Instant::now();
        let mut pick = || {
            s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &soonest(), &ranks, now)
                .unwrap()
                .id
                .clone()
        };
        assert_eq!(pick(), "a", "the probe");
        assert_eq!([pick(), pick()], ["b", "b"]);
    }

    /// A known reset that passes rolls the window over and re-arms exactly one probe.
    #[test]
    fn soonest_reset_re_arms_only_on_a_rollover() {
        let wall = SystemTime::now();
        let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
        let b_known = claude(wall, 2 * DAY, "allowed", wall + Duration::from_secs(3600));
        let a_reset = wall + Duration::from_secs(60);
        // `a` reported a weekly reset in 60 s; read 61 s later it has rolled over.
        let a_rolled = |seen: SystemTime| {
            Windows::observed(
                "claude",
                &signals(&[("Anthropic-Ratelimit-Unified-7d-Reset", epoch(a_reset))]),
                seen,
                wall + Duration::from_secs(61),
            )
        };
        let mut s = Scheduler::default();
        let now = Instant::now();
        let mut picks = Vec::new();
        for i in 0..3u64 {
            let a_now = a_rolled(wall + Duration::from_secs(i));
            let ranks = |x: &Credential| if x.id == "a" { a_now } else { b_known };
            picks.push(
                s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &soonest(), &ranks, now)
                    .unwrap()
                    .id
                    .clone(),
            );
        }
        assert_eq!(picks, ["a", "b", "b"], "one probe per rollover");
        // The next weekly rollover re-arms it once more.
        let next = a_reset + Duration::from_secs(7 * DAY);
        let later = Windows::observed(
            "claude",
            &signals(&[("Anthropic-Ratelimit-Unified-7d-Reset", epoch(next))]),
            wall,
            next + Duration::from_secs(1),
        );
        let ranks = |x: &Credential| if x.id == "a" { later } else { b_known };
        let mut pick = || {
            s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &soonest(), &ranks, now)
                .unwrap()
                .id
                .clone()
        };
        assert_eq!([pick(), pick()], ["a", "b"]);
    }

    /// Codex answers with `Reset-After-Seconds: 0` and no `Reset-At`: the reset is the
    /// observation time itself, which must not count as a new rollover on every answer.
    #[test]
    fn soonest_reset_zero_relative_resets_do_not_re_arm_the_probe() {
        let wall = SystemTime::now();
        let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
        let b_known = claude(wall, 2 * DAY, "allowed", wall + Duration::from_secs(3600));
        let zero = signals(&[
            ("X-Codex-Secondary-Used-Percent", "0".into()),
            ("X-Codex-Secondary-Window-Minutes", "10080".into()),
            ("X-Codex-Secondary-Reset-After-Seconds", "0".into()),
        ]);
        let mut s = Scheduler::default();
        let now = Instant::now();
        let mut picks = Vec::new();
        for i in 0..4u64 {
            // `a` has answered `i` times, each identical answer observed a second later
            // and read a second after that.
            let a_now = (i > 0).then(|| {
                let seen = wall + Duration::from_secs(2 * i);
                Windows::observed("codex", &zero, seen, seen + Duration::from_secs(1))
            });
            let ranks = |x: &Credential| {
                if x.id == "a" {
                    a_now.unwrap_or_default()
                } else {
                    b_known
                }
            };
            picks.push(
                s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &soonest(), &ranks, now)
                    .unwrap()
                    .id
                    .clone(),
            );
        }
        assert_eq!(picks, ["a", "b", "b", "b"]);
        // The zero delay still ends a used-up window now.
        let used_up = signals(&[
            ("X-Codex-Primary-Used-Percent", "100".into()),
            ("X-Codex-Primary-Reset-After-Seconds", "0".into()),
        ]);
        assert!(!Windows::observed("codex", &used_up, wall, wall).exhausted);
    }

    /// Relative resets read around a boundary differ by rounding; rollovers less than
    /// half a window apart are the same one, so they never re-arm the probe twice.
    #[test]
    fn soonest_reset_rollovers_within_half_a_window_are_one_rollover() {
        let wall = SystemTime::now();
        let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
        let b_known = claude(wall, 2 * DAY, "allowed", wall + Duration::from_secs(3600));
        // `a` reports its weekly reset `after` seconds from an observation at `seen`.
        let codex = |seen: SystemTime, after: u64, read: SystemTime| {
            Windows::observed(
                "codex",
                &signals(&[
                    ("X-Codex-Secondary-Window-Minutes", "10080".into()),
                    ("X-Codex-Secondary-Reset-After-Seconds", after.to_string()),
                ]),
                seen,
                read,
            )
        };
        let mut s = Scheduler::default();
        let now = Instant::now();
        let mut pick = |a_now: Windows| {
            let ranks = |x: &Credential| if x.id == "a" { a_now } else { b_known };
            s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &soonest(), &ranks, now)
                .unwrap()
                .id
                .clone()
        };
        let boundary = wall + Duration::from_secs(600);
        // Reset reached: probed once.
        assert_eq!(pick(codex(wall, 600, boundary + Duration::from_secs(1))), "a");
        // Lagging answers keep reporting one second left, each from a later observation.
        for i in 1..4u64 {
            let seen = boundary + Duration::from_secs(i);
            assert_eq!(pick(codex(seen, 1, seen + Duration::from_secs(2))), "b", "answer {i}");
        }
        // A week later the window really rolls over again: one more probe.
        let next = boundary + Duration::from_secs(7 * DAY);
        assert_eq!(
            pick(codex(next - Duration::from_secs(5), 5, next + Duration::from_secs(1))),
            "a"
        );
        assert_eq!(
            pick(codex(next - Duration::from_secs(5), 5, next + Duration::from_secs(1))),
            "b"
        );
    }

    /// P2: a management reset clears the probe record (the account is probed again) and
    /// leaves session affinity alone.
    #[test]
    fn soonest_reset_management_reset_re_arms_the_probe_and_keeps_affinity() {
        let wall = SystemTime::now();
        let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
        let b_known = claude(wall, 2 * DAY, "allowed", wall + Duration::from_secs(3600));
        let ranks = |x: &Credential| if x.id == "a" { Windows::default() } else { b_known };
        let policy = Policy {
            session_affinity: true,
            ..soonest()
        };
        let mut s = Scheduler::default();
        let now = Instant::now();
        let mut thread = selection("m");
        thread.session = Some("claude:thread-1".into());
        let mut pick = |sel: &Selection, set: &[&Credential]| {
            s.pick_ranked(&tag(set), sel, &policy, &ranks, now).unwrap().id.clone()
        };
        // The thread starts on `a` while it is the only account: that was `a`'s probe.
        assert_eq!(pick(&thread, &[&a]), "a");
        let fresh = selection("m");
        assert_eq!(pick(&fresh, &[&a, &b]), "b");
        s.reset_cooldowns("a");
        let mut pick = |sel: &Selection, set: &[&Credential]| {
            s.pick_ranked(&tag(set), sel, &policy, &ranks, now).unwrap().id.clone()
        };
        assert_eq!(pick(&fresh, &[&a, &b]), "a", "probed again");
        assert_eq!(pick(&fresh, &[&a, &b]), "b");
        assert_eq!(pick(&thread, &[&a, &b]), "a", "the pin is kept");
    }

    /// P2: an oversized finite window length neither panics nor holds a used-up window
    /// past the missing-length policy (five hours).
    #[test]
    fn windows_survive_unrepresentable_window_lengths() {
        let now = SystemTime::now();
        for (used, exhausted) in [("0", false), ("100", true)] {
            let huge = signals(&[
                ("X-Codex-Primary-Used-Percent", used.into()),
                ("X-Codex-Primary-Window-Minutes", "1e308".into()),
            ]);
            assert_eq!(
                Windows::observed("codex", &huge, now, now).exhausted,
                exhausted,
                "{used}%"
            );
            let later = now + Duration::from_secs(5 * 3600 + 1);
            assert!(
                !Windows::observed("codex", &huge, now, later).exhausted,
                "{used}% after five hours"
            );
        }
        // Long but representable lengths still count.
        let long = signals(&[
            ("X-Codex-Primary-Used-Percent", "100".into()),
            ("X-Codex-Primary-Window-Minutes", "1e9".into()),
        ]);
        assert!(Windows::observed("codex", &long, now, now + Duration::from_secs(DAY)).exhausted);
    }

    /// P2: `X-Codex-Limit-Reached` ends with the used-up window's own reset; the
    /// five-hour fallback applies only when no window reports a reset.
    #[test]
    fn codex_limit_reached_expires_with_the_window_reset() {
        let now = SystemTime::now();
        let flagged = signals(&[
            ("X-Codex-Limit-Reached", "true".into()),
            ("X-Codex-Primary-Used-Percent", "100".into()),
            ("X-Codex-Primary-Window-Minutes", "300".into()),
            ("X-Codex-Primary-Reset-At", epoch(now + Duration::from_secs(60))),
        ]);
        assert!(Windows::observed("codex", &flagged, now, now).exhausted);
        assert!(!Windows::observed("codex", &flagged, now, now + Duration::from_secs(61)).exhausted);
        // No window used up: the flag holds until the soonest reported reset.
        let below = signals(&[
            ("X-Codex-Limit-Reached", "true".into()),
            ("X-Codex-Primary-Used-Percent", "40".into()),
            ("X-Codex-Primary-Reset-At", epoch(now + Duration::from_secs(600))),
        ]);
        assert!(Windows::observed("codex", &below, now, now + Duration::from_secs(599)).exhausted);
        assert!(!Windows::observed("codex", &below, now, now + Duration::from_secs(601)).exhausted);
        // No reset anywhere: five hours.
        let bare = signals(&[("X-Codex-Limit-Reached", "true".into())]);
        assert!(Windows::observed("codex", &bare, now, now + Duration::from_secs(5 * 3600 - 1)).exhausted);
        assert!(!Windows::observed("codex", &bare, now, now + Duration::from_secs(5 * 3600 + 1)).exhausted);
    }

    /// P2: Codex usage, reset and length are parsed independently, so a window whose
    /// usage is missing still reports its reset.
    #[test]
    fn codex_windows_parse_usage_reset_and_length_independently() {
        let now = SystemTime::now();
        let weekly = now + Duration::from_secs(3 * DAY);
        let no_usage = signals(&[
            ("X-Codex-Secondary-Window-Minutes", "10080".into()),
            ("X-Codex-Secondary-Reset-At", epoch(weekly)),
        ]);
        let w = Windows::observed("codex", &no_usage, now, now);
        assert_eq!((w.weekly_reset.map(epoch), w.exhausted), (Some(epoch(weekly)), false));
        // Reset only, no usage or length: secondary is weekly.
        let reset_only = signals(&[("X-Codex-Secondary-Reset-After-Seconds", "7200".into())]);
        let w = Windows::observed("codex", &reset_only, now, now);
        assert_eq!(w.weekly_reset.map(epoch), Some(epoch(now + Duration::from_secs(7200))));
    }

    /// Both Codex windows, the primary the longer one: the primary is weekly.
    #[test]
    fn codex_primary_longer_than_secondary_is_the_weekly_window() {
        let now = SystemTime::now();
        let weekly = now + Duration::from_secs(3 * DAY);
        let both = signals(&[
            ("X-Codex-Primary-Used-Percent", "51".into()),
            ("X-Codex-Primary-Window-Minutes", "10080".into()),
            ("X-Codex-Primary-Reset-At", epoch(weekly)),
            ("X-Codex-Secondary-Used-Percent", "100".into()),
            ("X-Codex-Secondary-Window-Minutes", "300".into()),
            ("X-Codex-Secondary-Reset-After-Seconds", "3600".into()),
        ]);
        let w = Windows::observed("codex", &both, now, now);
        assert_eq!((w.weekly_reset.map(epoch), w.exhausted), (Some(epoch(weekly)), true));
        let w = Windows::observed("codex", &both, now, now + Duration::from_secs(3601));
        assert_eq!((w.weekly_reset.map(epoch), w.exhausted), (Some(epoch(weekly)), false));
    }
}
