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
    pub rolled_over: Option<Rollover>,
    /// A window (Claude 5-hour or 7-day, Codex primary or secondary) is used up and has
    /// not reset yet: upstream would refuse until it does.
    pub exhausted: bool,
}

/// Which window a rollover belongs to. Probe markers are kept per window, so a
/// rollover of one window is never compared with a probe made for another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowId {
    Claude7d,
    Claude5h,
    CodexPrimary,
    CodexSecondary,
}

/// A long window that rolled over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rollover {
    pub window: WindowId,
    /// The stated reset that passed.
    pub at: SystemTime,
    /// How far apart two rollovers of this window must be to count as different ones:
    /// half its length (five hours when unknown), which absorbs the rounding of
    /// relative resets around a boundary.
    pub gap: Duration,
}

/// How long a used-up window without a usable reset or length is assumed to hold.
const FALLBACK_WINDOW: Duration = Duration::from_secs(5 * 3600);

/// One usage window read from quota signals; each part may be missing.
#[derive(Debug, Clone, Copy)]
struct Window {
    id: WindowId,
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
                let window = |id: WindowId, label: &str, minutes: f64| Window {
                    id,
                    used_up: get(&format!("anthropic-ratelimit-unified-{label}-status"))
                        .is_some_and(|s| s.eq_ignore_ascii_case("rejected"))
                        || number(&format!("anthropic-ratelimit-unified-{label}-utilization"))
                            .is_some_and(|u| u >= 1.0),
                    reset: get(&format!("anthropic-ratelimit-unified-{label}-reset")).and_then(reset_time),
                    stated_reset: get(&format!("anthropic-ratelimit-unified-{label}-reset")).and_then(reset_time),
                    length: window_length(minutes, observed_at),
                };
                (
                    Some(window(WindowId::Claude7d, "7d", 7.0 * 24.0 * 60.0)),
                    Some(window(WindowId::Claude5h, "5h", 300.0)),
                )
            }
            "codex" => {
                // Usage, reset and length are read independently; a window exists when
                // any of them was reported.
                let window = |id: WindowId, label: &str| {
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
                        id,
                        used_up: used.is_some_and(|u| u >= 100.0),
                        reset,
                        stated_reset,
                        length,
                    })
                };
                let day = Duration::from_secs(24 * 3600);
                match (
                    window(WindowId::CodexPrimary, "primary"),
                    window(WindowId::CodexSecondary, "secondary"),
                ) {
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
            rolled_over: long.and_then(|w| {
                let at = w.stated_reset.filter(|r| *r <= now)?;
                Some(Rollover {
                    window: w.id,
                    at,
                    gap: w.length.unwrap_or(FALLBACK_WINDOW) / 2,
                })
            }),
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

/// A credential's `soonest-reset` probes: present once its probe for an unknown reset
/// was spent, with the last rollover probed for, per window.
type Probes = HashMap<WindowId, SystemTime>;

/// Whether a credential was already probed for what it reports now. A probe holds
/// until its long window rolls over after it: a reset of that same window that passed
/// and is more than the window's gap later than the rollover last probed for it.
fn probed(record: Option<&Probes>, rolled_over: Option<Rollover>) -> bool {
    match (record, rolled_over) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(markers), Some(r)) => markers
            .get(&r.window)
            .is_some_and(|at| at.checked_add(r.gap).is_none_or(|limit| r.at <= limit)),
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
    /// routing.cooldown.max-trusted-cooldown (cliproxy-rs only): the first bound on a
    /// quota reset an upstream states ([`trusted`]); zero trusts every reset, as Go does.
    pub max_trusted_cooldown: Duration,
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
            max_trusted_cooldown: DEFAULT_TRUSTED_COOLDOWN,
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
    pub max_trusted_cooldown: &'a str,
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
            max_trusted_cooldown: &r.cooldown.max_trusted_cooldown,
        })
    }
}

pub fn normalize(raw: RawRouting<'_>) -> Policy {
    let ttl = cpa_core::config::parse_duration(raw.session_affinity_ttl.trim())
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
        max_trusted_cooldown: trusted_cooldown(raw.max_trusted_cooldown),
        ..Policy::default()
    }
}

const DEFAULT_TRUSTED_COOLDOWN: Duration = Duration::from_secs(3600);

/// `max-trusted-cooldown`: a Go duration (`"90m"`) or a bare number of seconds. Empty
/// means the one-hour default and zero or negative turns the bound off; a positive value
/// is at least Go's 10 s quota floor. An unreadable value (`"1d"`, `false`, `"1.5"`)
/// keeps the default and is logged once per value.
fn trusted_cooldown(raw: &str) -> Duration {
    let raw = raw.trim();
    if raw.is_empty() {
        return DEFAULT_TRUSTED_COOLDOWN;
    }
    let nanos = match raw.parse::<i64>() {
        Ok(seconds) => seconds.saturating_mul(1_000_000_000),
        Err(_) => match cpa_core::config::parse_duration(raw) {
            Some(nanos) => nanos,
            None => {
                static WARNED: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
                let mut last = WARNED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if *last != raw {
                    raw.clone_into(&mut last);
                    tracing::warn!(
                        "routing.cooldown.max-trusted-cooldown {raw:?} is not a duration (\"90m\") or a \
                         number of seconds; using 1h"
                    );
                }
                return DEFAULT_TRUSTED_COOLDOWN;
            }
        },
    };
    match nanos {
        ..=0 => Duration::ZERO,
        n => Duration::from_nanos(n as u64).max(Duration::from_secs(10)),
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
    /// Bounded trust of a stated quota reset (cliproxy-rs only); `level` stays Go's.
    pub trust: Trust,
}

/// Bounded trust of one cooldown (`max-trusted-cooldown`, docs/DIFFERENCES-FROM-GO.md).
/// Costs nothing until a stated reset is cut: then one comparison per 429 and, per
/// recorded result, a probe check on the two keys the result touches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Trust {
    /// Exponent of the latest bounded window, which lasted `cap << window`. Kept after
    /// the window ends, so the next cut window doubles; a success clears it.
    pub window: Option<u32>,
    /// The scheduler's generation when this bounded window opened or its probe was
    /// reserved ([`Scheduler::reserve_probe`]). A lease picked before it holds an older
    /// answer, which neither ends the window nor frees the reservation. Cost: four
    /// bytes here and on each lease, one comparison per key a result touches.
    pub generation: u32,
    /// The deadline is the bound's: a concurrent 429 inside it keeps it.
    pub bounded: bool,
    /// A probe the upstream accepted (a stream started) answered this ended bounded
    /// window: it no longer reserves probes, but keeps its generation, so an older
    /// lease's late answer still cannot replace it. Fits in existing padding.
    pub accepted: bool,
    /// The upstream's stated reset behind a bounded deadline (Go's recovery time).
    pub stated: Option<Instant>,
    /// An unanswered probe request holds the ended window until this instant.
    pub probe: Option<Instant>,
}

impl Trust {
    /// The window's probe was accepted ([`Scheduler::accept_probe_picked`]).
    fn accept(&mut self) {
        self.bounded = false;
        self.accepted = true;
        self.probe = None;
    }
}

/// Longest a probe reserves an ended bounded window. A 429 arrives within seconds; a
/// probe still unanswered after this is being served, and its result clears it anyway.
const PROBE_HOLD: Duration = Duration::from_secs(30);
/// Largest trust exponent: `cap << 20` is over a century for any cap of 1 h or more.
const MAX_TRUST_WINDOW: u32 = 20;

impl Cooldown {
    /// When this entry stops blocking picks: its deadline, or an unanswered probe.
    fn until(&self, now: Instant) -> Option<Instant> {
        let live = (self.deadline > now).then_some(self.deadline);
        live.max(self.trust.probe.filter(|p| *p > now))
    }

    /// A bounded window (still armed, or answered by an accepted probe) that opened, or
    /// had its probe reserved, after the lease picked at generation `picked`: that
    /// lease's answer does not answer it.
    fn newer_than(&self, picked: u32) -> bool {
        // Serial-number order, so the wrapping counter compares right while a lease is
        // fewer than 2^31 generations old.
        (self.trust.bounded || self.trust.accepted) && (self.trust.generation.wrapping_sub(picked) as i32) > 0
    }
}

#[derive(Default)]
pub(crate) struct Scheduler {
    rotations: HashMap<(String, String), Rotation>,
    /// Mixed-provider round-robin cursors (Go `mixedCursors`).
    cursors: HashMap<(String, String), usize>,
    pub(crate) cooldowns: HashMap<(String, String), Cooldown>,
    /// Session affinity bindings (Go `SessionAffinitySelector.cache`).
    affinity: crate::affinity::Cache,
    /// Bindings of requests without an explicit session (Go
    /// `SessionAffinitySelector.matcher`).
    lcp: crate::lcp::Matcher,
    /// `soonest-reset` probes by credential, reserved when the probe request is picked
    /// (under the scheduler lock, so concurrent picks never probe twice). See [`probed`].
    probes: HashMap<String, Probes>,
    /// The live policy turned session affinity off: results of leases picked while it
    /// was on no longer bind anything (Go's replacement selector has no affinity).
    affinity_off: bool,
    /// Bounded windows opened plus probes reserved; each lease carries the value at its
    /// pick ([`Trust::generation`]).
    generation: u32,
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

/// Bounded trust for a stated quota reset (docs/DIFFERENCES-FROM-GO.md): a reset longer
/// than the window's bound is cut to it, so the next ordinary request after the bound
/// probes the account and an early provider reset is noticed within the bound. The
/// first window is `cap`; each cut window after one that ended without a success is
/// twice the last (`window` is the last exponent). Returns the bound and its exponent,
/// or `None` when the stated reset is trusted as it is (it fits, or `cap` is zero).
fn bound(stated: Duration, cap: Duration, window: Option<u32>) -> Option<(Duration, u32)> {
    if cap.is_zero() {
        return None;
    }
    let step = window.map_or(0, |w| w.saturating_add(1)).min(MAX_TRUST_WINDOW);
    let bound = cap.saturating_mul(1 << step);
    (stated > bound).then_some((bound, step))
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
        self.affinity_off = !next.session_affinity;
        if previous.strategy != next.strategy
            || previous.session_affinity != next.session_affinity
            || previous.session_affinity_ttl != next.session_affinity_ttl
            || previous.session_affinity_subagents != next.session_affinity_subagents
        {
            self.rotations.clear();
            self.cursors.clear();
            self.affinity.clear();
            // Go builds a new selector, and with it a new matcher on the new TTL. Unlike
            // Go, access generations keep counting (docs/DIFFERENCES-FROM-GO.md).
            self.lcp.reset(crate::lcp::Limits {
                ttl: next.session_affinity_ttl,
                ..Default::default()
            });
            self.probes.clear();
        }
    }

    /// Session affinity under `policy`, unless the live policy has since turned it off.
    fn affinity(&self, policy: &Policy) -> bool {
        policy.session_affinity && !self.affinity_off
    }

    pub fn quota_cooling(&self, c: &Credential, model: &str, now: Instant) -> bool {
        let model = canonical_model(model);
        [model, ""].into_iter().any(|m| {
            self.cooldowns
                .get(&(c.id.clone(), m.to_owned()))
                .is_some_and(|s| s.quota && s.until(now).is_some())
        })
    }

    pub fn wait(&self, c: &Credential, model: &str, now: Instant) -> Option<Duration> {
        let model = canonical_model(model);
        [model, ""]
            .into_iter()
            .filter_map(|model| {
                self.cooldowns
                    .get(&(c.id.clone(), model.to_owned()))
                    .and_then(|s| s.until(now)?.checked_duration_since(now))
                    .filter(|d| !d.is_zero())
            })
            .max()
    }

    /// Whether a cooldown deadline (not a probe reservation) is ahead for `model`.
    pub fn cooling(&self, c: &Credential, model: &str, now: Instant) -> bool {
        let model = canonical_model(model);
        [model, ""].into_iter().any(|m| {
            self.cooldowns
                .get(&(c.id.clone(), m.to_owned()))
                .is_some_and(|s| s.deadline > now)
        })
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
        // A bounded window or an unanswered probe is not waited for: the probe answers
        // for everyone, and holding requests to the window's end would release them
        // together as a burst of probes.
        [model, ""].into_iter().all(|model| {
            self.cooldowns.get(&(c.id.clone(), model.to_owned())).is_none_or(|s| {
                let probing = s.trust.probe.is_some_and(|p| p > now);
                !probing && (s.deadline <= now || (retry_status(s.status) && !s.trust.bounded))
            })
        })
    }

    /// [`Self::accept_probe_picked`] for a lease picked just now.
    #[cfg(test)]
    pub fn accept_probe(&mut self, c: &Credential, model: &str, now: Instant) {
        self.accept_probe_picked(c, model, self.generation, now);
    }

    /// The upstream accepted the attempt that probes `model` (a stream's first chunk),
    /// picked at generation `picked`: the ended bounded window has its answer, so other
    /// picks may go through while the stream runs. Only expired bounded entries that
    /// are not newer than the lease change; their count stays until the stream's own
    /// outcome is recorded. When the credential-wide window ended, the model keys that
    /// share its deadline (its copies) are disarmed too; a model's own window is not.
    /// Cost: two lookups, and a scan of the cooldown table only when the credential-wide
    /// window ended.
    pub fn accept_probe_picked(&mut self, c: &Credential, model: &str, picked: u32, now: Instant) {
        let model = canonical_model(model);
        let mut credential_window = None;
        for m in [model, ""] {
            if let Some(s) = self.cooldowns.get_mut(&(c.id.clone(), m.to_owned()))
                && s.trust.bounded
                && s.deadline <= now
                && !s.newer_than(picked)
            {
                s.trust.accept();
                if m.is_empty() {
                    credential_window = Some(s.deadline);
                }
            }
        }
        if let Some(ended) = credential_window {
            for ((id, m), s) in &mut self.cooldowns {
                if *id == c.id && !m.is_empty() && s.trust.bounded && s.deadline == ended && !s.newer_than(picked) {
                    s.trust.accept();
                }
            }
        }
    }

    /// Reserves the probe of `c` for `model` when its pick ends a bounded window: until
    /// the probe's result is recorded (or [`PROBE_HOLD`] passes), the window's keys keep
    /// cooling, so concurrent requests do not each go upstream. Returns the generation
    /// the pick's lease carries; a reservation takes a new one, so only its own lease's
    /// result frees it. Cost per pick: two lookups.
    pub fn reserve_probe(&mut self, c: &Credential, model: &str, now: Instant) -> u32 {
        let model = canonical_model(model);
        let next = self.generation.wrapping_add(1);
        for m in [model, ""] {
            if let Some(s) = self.cooldowns.get_mut(&(c.id.clone(), m.to_owned()))
                && s.trust.bounded
                && s.deadline <= now
            {
                s.trust.probe = now.checked_add(PROBE_HOLD);
                s.trust.generation = next;
                self.generation = next;
            }
        }
        self.generation
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
    #[cfg(test)]
    pub fn pick_ranked<'a>(
        &mut self,
        candidates: &[(&'a Credential, &str)],
        selection: &Selection,
        policy: &Policy,
        ranks: &Ranks<'_>,
        now: Instant,
    ) -> Option<&'a Credential> {
        self.pick_session(candidates, selection, policy, ranks, now)
            .map(|(c, _)| c)
    }

    /// [`Self::pick_ranked`], with the LCP binding of a request without an explicit
    /// session (Go `pickLCP`): its session identity, lineage and access generation.
    pub fn pick_session<'a>(
        &mut self,
        candidates: &[(&'a Credential, &str)],
        selection: &Selection,
        policy: &Policy,
        ranks: &Ranks<'_>,
        now: Instant,
    ) -> Option<(&'a Credential, Option<crate::lcp::Match>)> {
        if candidates.is_empty() {
            return None;
        }
        if let Some(request) = selection.lcp.as_deref().filter(|_| self.affinity(policy)) {
            let namespace = request.namespace(&selection.model);
            // A known trajectory stays on its credential while that one is available.
            if let Some(found) = self.lcp.find(&namespace, request.prepared(), now)
                && let Some((c, _)) = candidates.iter().find(|(c, _)| c.id == found.auth)
            {
                return Some((c, Some(found)));
            }
            let picked = self.pick_unbound(candidates, selection, policy, ranks, now)?;
            let bound = self.lcp.bind(&namespace, request.prepared(), &picked.id, now);
            return Some((picked, bound));
        }
        self.pick_legacy(candidates, selection, policy, ranks, now)
            .map(|c| (c, None))
    }

    /// Go `SessionAffinitySelector.Pick` after the LCP step.
    fn pick_legacy<'a>(
        &mut self,
        candidates: &[(&'a Credential, &str)],
        selection: &Selection,
        policy: &Policy,
        ranks: &Ranks<'_>,
        now: Instant,
    ) -> Option<&'a Credential> {
        let ttl = policy.session_affinity_ttl;
        let Some(keys) = self.affinity(policy).then(|| session_keys(selection)).flatten() else {
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
    ///
    /// A request without an explicit session refreshes its LCP sequence on success; a
    /// credential failure removes that exact sequence unless a newer request refreshed
    /// it after `lcp`'s generation. Such a request never touches the session cache.
    pub fn session_result(
        &mut self,
        c: &Credential,
        selection: &Selection,
        outcome: &Outcome,
        policy: &Policy,
        lcp: Option<&crate::lcp::Match>,
        now: Instant,
    ) {
        if !self.affinity(policy) {
            return;
        }
        let success = match outcome {
            Outcome::Success => true,
            Outcome::Failure(error) if policy.error_action(c, error).cooldown => false,
            _ => return,
        };
        if let Some(request) = selection.lcp.as_deref() {
            let namespace = request.namespace(&selection.model);
            if success {
                self.lcp.touch(&namespace, request.prepared(), &c.id, now);
            } else {
                let generation = lcp.map_or(0, |m| m.access);
                self.lcp
                    .remove(&namespace, &request.prepared().fingerprints, &c.id, generation, now);
            }
            if lcp.is_some() {
                return;
            }
        }
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
                        let order = windows.order(probed(probes.get(&c.id), windows.rolled_over));
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
                    let markers = probes.entry(picked.id.clone()).or_default();
                    if let Some(r) = rolled_over {
                        markers.insert(r.window, r.at);
                    }
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

    /// [`Self::record_picked`] for an attempt picked just now.
    #[cfg(test)]
    pub fn record(&mut self, c: &Credential, model: &str, outcome: &Outcome, policy: &Policy, now: Instant) {
        self.record_picked(c, model, outcome, policy, self.generation, now);
    }

    /// Applies one attempt's outcome (Go `MarkResult`). `picked` is the generation its
    /// lease was picked at ([`Self::reserve_probe`]): a bounded window or probe
    /// reservation newer than that is not answered by it.
    pub fn record_picked(
        &mut self,
        c: &Credential,
        model: &str,
        outcome: &Outcome,
        policy: &Policy,
        picked: u32,
        now: Instant,
    ) {
        let model = canonical_model(model);
        let key = (c.id.clone(), model.to_owned());
        // Any answer ends a probe reservation on the keys it touches, except one taken
        // after its lease was picked (a later probe's, once this one's hold lapsed).
        for m in [model, ""] {
            if let Some(s) = self.cooldowns.get_mut(&(c.id.clone(), m.to_owned()))
                && !s.newer_than(picked)
            {
                s.trust.probe = None;
            }
        }
        let error = match outcome {
            Outcome::Success => {
                // Active credential quota survives success on an in-flight sibling.
                let global = (c.id.clone(), String::new());
                if self.cooldowns.get(&global).is_some_and(|s| s.quota && s.deadline > now) {
                    return;
                }
                // A request that started before a bounded window opened leaves it for the
                // window's own probe.
                if let Some(ended) = self
                    .cooldowns
                    .get(&global)
                    .filter(|s| s.deadline <= now && !s.newer_than(picked))
                    .map(|s| s.deadline)
                {
                    self.cooldowns.remove(&global);
                    // The credential recovered: model keys that a credential-wide window
                    // escalated forget it too (their Go state is left as Go keeps it). A
                    // model's own ended bounded window still waits for its probe, and one
                    // a newer probe reserved or answered keeps its state.
                    for ((id, _), s) in &mut self.cooldowns {
                        if *id == c.id
                            && s.deadline <= now
                            && (!s.trust.bounded || s.deadline == ended)
                            && !s.newer_than(picked)
                        {
                            s.trust = Trust::default();
                        }
                    }
                }
                if !self.cooldowns.get(&key).is_some_and(|s| s.newer_than(picked)) {
                    self.cooldowns.remove(&key);
                }
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
        // An ended bounded window that a newer probe took over, or that opened after this
        // attempt was picked, waits for its own probe: this older answer neither renews
        // nor replaces it. Inside a live window the handling below applies.
        if prev.is_some_and(|s| s.deadline <= now && s.newer_than(picked)) {
            return;
        }
        let prev_deadline = prev.map(|s| s.deadline);
        let prev_live = prev.filter(|s| s.deadline > now).map(|s| s.deadline);
        let prev_trust = prev.map(|s| s.trust).unwrap_or_default();
        let mut level = prev.filter(|s| s.quota).map(|s| s.level).unwrap_or(0);
        // Causes other than a cut reset keep the trust count: nothing recovered.
        let mut trust = Trust {
            window: prev_trust.window,
            ..Trust::default()
        };
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
                        let stated = hint.max(Duration::from_secs(10));
                        let cap = policy.max_trusted_cooldown;
                        if !cap.is_zero()
                            && let Some(window) = prev.filter(|s| s.trust.bounded && s.deadline > now)
                        {
                            // A concurrent answer inside a live bounded window: the probe
                            // deadline and the count stand, but an earlier stated reset
                            // moves the probe up to it.
                            let reset = now.checked_add(stated);
                            let old = window.deadline;
                            let deadline = reset.map_or(old, |r| old.min(r));
                            let mut window_trust = window.trust;
                            if let Some(slot) = self.cooldowns.get_mut(&key) {
                                slot.deadline = deadline;
                                slot.error = text;
                                slot.trust.stated = reset.or(slot.trust.stated);
                                window_trust = slot.trust;
                            }
                            if credential_quota {
                                // Model keys that inherited this credential window (same
                                // deadline, bounded) follow it when it moves up; longer
                                // independent cooldowns stand.
                                for ((id, m), state) in &mut self.cooldowns {
                                    if *id == c.id && !m.is_empty() && state.trust.bounded && state.deadline == old {
                                        state.deadline = deadline;
                                        state.trust.stated = window_trust.stated;
                                        state.trust.window = state.trust.window.max(window_trust.window);
                                    }
                                }
                                self.extend_siblings(c, deadline, None, now);
                            }
                            return;
                        }
                        Some(match bound(stated, cap, prev_trust.window) {
                            Some((bound, window)) => {
                                self.generation = self.generation.wrapping_add(1);
                                trust = Trust {
                                    window: Some(window),
                                    generation: self.generation,
                                    bounded: true,
                                    accepted: false,
                                    stated: now.checked_add(stated),
                                    probe: None,
                                };
                                bound
                            }
                            None => stated,
                        })
                    } else if let Some(prev) = prev.filter(|s| s.quota && s.deadline > now) {
                        // Go `quotaCooldownAfterFailure`: an active quota deadline is
                        // reused, not extended.
                        let deadline = prev.deadline;
                        if let Some(slot) = self.cooldowns.get_mut(&key) {
                            slot.error = text;
                        }
                        if credential_quota {
                            self.extend_siblings(c, deadline, None, now);
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
        // An answer from a request already in flight (a 503, a 401) inside a live bounded
        // quota window that outlasts it: the window stands as it is, quota and all, so
        // its saved record keeps the stated reset and count. Only bounded windows; Go's
        // own cooldowns merge as Go does.
        if prev_live == Some(deadline)
            && let Some(slot) = self.cooldowns.get_mut(&key).filter(|s| s.quota && s.trust.bounded)
        {
            slot.error = text;
            // Go's level still counts a Cloudflare challenge (unchanged for other causes).
            slot.level = level;
            return;
        }
        // Provenance follows the deadline: a longer earlier cooldown that stands keeps
        // its own bounded flag and stated reset.
        let provenance = |deadline: Instant| {
            if deadline == next {
                trust
            } else if prev_live == Some(deadline) {
                Trust {
                    probe: None,
                    ..prev_trust
                }
            } else {
                Trust {
                    window: trust.window,
                    ..Trust::default()
                }
            }
        };
        let state = |deadline| Cooldown {
            deadline,
            level,
            status: error.status,
            quota,
            error: text.clone(),
            since: SystemTime::now(),
            credential: false,
            trust: provenance(deadline),
        };
        self.cooldowns.insert(key, state(deadline));
        if credential_quota {
            self.extend_siblings(c, deadline, prev_deadline, now);
            // Go also records the failing model's own quota state (reason `quota`).
            if !model.is_empty() {
                let own = (c.id.clone(), model.to_owned());
                // `extend_siblings` above already took a live own key to the credential's
                // deadline, unless it is longer, the model's own bounded window, or the
                // credential's window is bounded: then the own deadline stands.
                let own_deadline = self
                    .cooldowns
                    .get(&own)
                    .filter(|s| s.deadline > now)
                    .map_or(deadline, |s| s.deadline);
                let own_trust = self.cooldowns.get(&own).map(|s| s.trust).unwrap_or_default();
                let own_window = own_trust.window;
                let mut own_state = state(own_deadline);
                if own_deadline != deadline {
                    // The model's own window (longer, or bounded) wins: its provenance
                    // goes with it.
                    own_state.trust = Trust {
                        probe: None,
                        ..own_trust
                    };
                } else {
                    own_state.trust = provenance(deadline);
                }
                own_state.trust.window = own_state.trust.window.max(own_window);
                self.cooldowns.insert(own, own_state);
            }
        }
    }

    /// Go's credential-scoped 429: live sibling model states become quota cooldowns
    /// (`credential_quota`) lasting at least as long as the credential. Under a bounded
    /// credential window, and for a model's own bounded window under any, a model's own
    /// deadline is left as it is: the credential's key holds the model until the
    /// credential's probe anyway, and a copied deadline would follow the credential's
    /// when an earlier reset moves it up, before the model's own deadline (Go never
    /// shortens one). Only copies take the credential's deadline: the copies of the
    /// window this one `renews` (the credential's previous deadline) move onto it even
    /// when they ended, so no copy outlives the account's recovery.
    fn extend_siblings(&mut self, c: &Credential, deadline: Instant, renews: Option<Instant>, now: Instant) {
        let (level, trust) = self
            .cooldowns
            .get(&(c.id.clone(), String::new()))
            .map_or((0, Trust::default()), |s| (s.level, s.trust));
        for ((id, model), state) in &mut self.cooldowns {
            if *id != c.id || model.is_empty() {
                continue;
            }
            let copy = state.trust.bounded && (state.deadline == deadline || Some(state.deadline) == renews);
            if state.deadline > now || copy {
                let window = state.trust.window.max(trust.window);
                if copy || !(state.trust.bounded || trust.bounded) {
                    if deadline >= state.deadline {
                        state.trust = Trust { probe: None, ..trust };
                    }
                    state.deadline = state.deadline.max(deadline);
                }
                // A model further along its own escalation keeps its count.
                state.trust.window = window;
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
        self.lcp.invalidate(id);
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
                // Go's recovery time is the stated reset; the probe time is the retry.
                let recover = state
                    .trust
                    .stated
                    .filter(|r| state.trust.bounded && *r > state.deadline)
                    .map_or(at, |r| wall + (r - now));
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
                            next_recover_at: Some(recover),
                            backoff_level: state.level,
                            observed_at: None,
                            trust_windows: Some(state.trust.window.map_or(0, |w| w.saturating_add(1))),
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
        cap: Duration,
        now: Instant,
        wall: SystemTime,
    ) -> bool {
        // Go blocks until the later of the retry deadline and, for a quota state, the
        // quota recovery time (`availabilityBlock`); one deadline here.
        // Go restores only records whose retry deadline is still ahead.
        let Some(retry) = record.next_retry_after.filter(|at| *at > wall) else {
            return false;
        };
        let recover = record.quota.next_recover_at.filter(|_| record.quota.exceeded);
        let saved = record.quota.trust_windows.map(|n| n.min(MAX_TRUST_WINDOW + 1));
        let model = record.model.trim();
        if model.is_empty() && has_model_records && record.quota.reason != "credential_quota" {
            return false;
        }
        let since = |at: SystemTime| at.duration_since(wall).unwrap_or_default();
        let (remaining, trust) = match (record.quota.exceeded && !cap.is_zero(), saved) {
            // Written by cliproxy-rs: the saved retry is the deadline, as it is, whatever
            // set it (a bounded window, a trusted reset, or a longer cooldown from another
            // cause that a credential-wide quota marked), so a restart never cuts or
            // escalates it. A stated reset after the retry marks a bounded window.
            (true, Some(spent)) => {
                let stated = recover.filter(|r| *r > retry).map(|r| now + since(r));
                let trust = Trust {
                    window: spent.checked_sub(1),
                    generation: 0,
                    bounded: stated.is_some(),
                    accepted: false,
                    stated,
                    probe: None,
                };
                (since(retry), trust)
            }
            // A Go record, or one from before the bound: Go's deadline gets the first
            // bound. Go's `backoff_level` keeps its meaning and does not raise it.
            (true, None) => {
                let at = recover.map_or(retry, |r| retry.max(r));
                match bound(since(at), cap, None) {
                    Some((bound, window)) => (
                        bound,
                        Trust {
                            window: Some(window),
                            generation: 0,
                            bounded: true,
                            accepted: false,
                            stated: Some(now + since(at)),
                            probe: None,
                        },
                    ),
                    None => (since(at), Trust::default()),
                }
            }
            (false, _) => (since(recover.map_or(retry, |r| retry.max(r))), Trust::default()),
        };
        if remaining.is_zero() {
            return false;
        }
        let deadline = now + remaining;
        let error = record.last_error.clone().unwrap_or_default();
        let status = match error.http_status {
            0 if record.quota.exceeded => 429,
            s => u16::try_from(s).unwrap_or(0),
        };
        let key = (c.id.clone(), canonical_model(model).to_owned());
        let prev = self.cooldowns.get(&key).filter(|prev| prev.deadline > now);
        let prev_wins = prev.is_some_and(|p| p.deadline > deadline);
        let deadline = prev.map_or(deadline, |prev| prev.deadline.max(deadline));
        let mut quota = (
            record.quota.exceeded,
            status,
            !model.is_empty() && record.quota.reason == "credential_quota",
        );
        let mut trust = trust;
        // With bounded trust on either side, provenance and the quota classification come
        // from the entry that supplies the deadline, and the counts merge. Without it the
        // merge is Go's.
        if let Some(prev) = prev.filter(|p| p.trust != Trust::default() || trust != Trust::default()) {
            let window = prev.trust.window.max(trust.window);
            if prev_wins {
                trust = Trust {
                    probe: None,
                    ..prev.trust
                };
                quota = (prev.quota, prev.status, prev.credential);
            }
            trust.window = window;
        }
        // A bounded window this restore installs is newer than every lease in flight (a
        // restore also runs while serving, when `save-cooldown-status` turns on or its
        // directory moves); a live window it keeps keeps its generation.
        if trust.bounded && !prev_wins {
            self.generation = self.generation.wrapping_add(1);
            trust.generation = self.generation;
        }
        let (quota, status, credential) = quota;
        self.cooldowns.insert(
            key,
            Cooldown {
                deadline,
                level: record.quota.backoff_level,
                status,
                quota,
                error: if error.message.is_empty() {
                    record.reason.clone()
                } else {
                    error.message
                },
                since: record.updated_at.unwrap_or(wall),
                credential,
                trust,
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
        self.lcp.retain(|id| credentials.iter().any(|c| c.id == id));
        self.probes.retain(|id, _| credentials.iter().any(|c| c.id == *id));
    }
}

/// Time left until a bounded cooldown's stated reset, while it is ahead.
fn recover_after(s: &Cooldown, now: Instant) -> Option<Duration> {
    let stated = s
        .trust
        .stated
        .filter(|r| s.trust.bounded && *r > now && *r > s.deadline)?;
    Some(stated - now)
}

/// One active cooldown for management views (additive read API). `model` is empty for
/// a credential-wide cooldown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CooldownState {
    pub model: String,
    /// Until the next attempt (zero once a bounded window ended and the next request
    /// probes the account).
    pub remaining: Duration,
    /// Until the upstream's stated reset, when the bound cut it (cliproxy-rs only).
    pub recover_in: Option<Duration>,
    pub level: u32,
    pub status: u16,
    pub quota: bool,
}

impl Scheduler {
    /// Live session-affinity bindings whose key `matches` (see [`crate::affinity::Cache::bound`]).
    pub(crate) fn affinity_bound(&self, now: Instant, matches: impl Fn(&crate::affinity::Key) -> bool) -> Vec<String> {
        self.affinity.bound(now, matches)
    }

    /// Unexpired cooldowns of one credential, model keys sorted, credential-wide first.
    pub(crate) fn cooldowns_of(&self, id: &str, now: Instant) -> Vec<CooldownState> {
        let mut out: Vec<CooldownState> = self
            .cooldowns
            .iter()
            .filter(|((cid, _), s)| cid == id && (s.deadline > now || recover_after(s, now).is_some()))
            .map(|((_, model), s)| CooldownState {
                model: model.clone(),
                remaining: s.deadline.saturating_duration_since(now),
                recover_in: recover_after(s, now),
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
            assert_eq!(cpa_core::config::parse_duration(raw), Some(expected), "{raw}");
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
            assert_eq!(cpa_core::config::parse_duration(raw), None, "{raw}");
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
    fn max_trusted_cooldown_reads_durations_seconds_and_off() {
        let read = |raw| {
            normalize(RawRouting {
                max_trusted_cooldown: raw,
                ..Default::default()
            })
            .max_trusted_cooldown
        };
        assert_eq!(read(""), Duration::from_secs(3600));
        assert_eq!(read("90m"), Duration::from_secs(5400));
        assert_eq!(read("120"), Duration::from_secs(120));
        assert_eq!(read("0"), Duration::ZERO);
        assert_eq!(read("0s"), Duration::ZERO);
        assert_eq!(read("-5"), Duration::ZERO);
        for unreadable in ["soon", "1d", "false", "1.5"] {
            assert_eq!(read(unreadable), Duration::from_secs(3600), "{unreadable}");
        }
        assert_eq!(read("3"), Duration::from_secs(10), "clamped to Go's floor");
        assert_eq!(read("2s"), Duration::from_secs(10), "clamped to Go's floor");
    }

    const H: u64 = 3600;
    const MIN: u64 = 60;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// A 429 whose stated reset is `stated` from `at`.
    fn hinted(scope: FailureScope, stated: Duration) -> Outcome {
        let mut e = ExecError::local(429, scope, "usage_limit_reached");
        e.retry_after = Some(stated);
        Outcome::Failure(e)
    }

    fn unhinted(scope: FailureScope) -> Outcome {
        Outcome::Failure(ExecError::local(429, scope, "rate limited"))
    }

    fn entry<'a>(s: &'a Scheduler, c: &Credential, model: &str) -> &'a Cooldown {
        &s.cooldowns[&(c.id.clone(), model.to_owned())]
    }

    /// A stated reset six days away is trusted for one hour, then for doubling windows
    /// while the account keeps answering 429, and never past the stated reset. Success
    /// clears the escalation; `0` trusts the stated reset as Go does.
    #[test]
    fn quota_resets_are_trusted_up_to_a_doubling_bound() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let start = Instant::now();
        let reset = start + secs(6 * 24 * H);
        for (scope, key) in [(FailureScope::Model, "m"), (FailureScope::Credential, "")] {
            let mut s = Scheduler::default();
            let mut now = start;
            for window in [1, 2, 4, 8, 16, 32, 64] {
                s.record(&c, "m", &hinted(scope, reset - now), &p, now);
                assert_eq!(s.wait(&c, "m", now), Some(secs(window * H)), "{scope:?}");
                assert_eq!(entry(&s, &c, key).trust.stated, Some(reset), "{scope:?}: stated kept");
                assert_eq!(entry(&s, &c, key).level, 0, "{scope:?}: Go's level untouched");
                now += secs(window * H);
                assert!(s.retry_eligible(&c, "m", now));
            }
            // 127 hours have passed; the next window would be 128 hours, past the reset.
            s.record(&c, "m", &hinted(scope, reset - now), &p, now);
            assert_eq!(
                s.wait(&c, "m", now),
                Some(reset - now),
                "{scope:?}: capped by the stated reset"
            );
            // The account came back early: success clears the escalation.
            let mut s = Scheduler::default();
            s.record(&c, "m", &hinted(scope, reset - start), &p, start);
            let then = start + secs(H);
            s.record(&c, "m", &hinted(scope, reset - then), &p, then);
            assert_eq!(s.wait(&c, "m", then), Some(secs(2 * H)));
            let later = start + secs(3 * H);
            s.record(&c, "m", &Outcome::Success, &p, later);
            s.record(&c, "m", &hinted(scope, reset - later), &p, later);
            assert_eq!(
                s.wait(&c, "m", later),
                Some(secs(H)),
                "{scope:?}: back to the first bound"
            );
        }
        // A reset shorter than the bound is kept as stated.
        let mut s = Scheduler::default();
        s.record(&c, "m", &hinted(FailureScope::Model, secs(30 * MIN)), &p, start);
        assert_eq!(s.wait(&c, "m", start), Some(secs(30 * MIN)));
        assert!(!entry(&s, &c, "m").trust.bounded);
        // `0` is Go: the whole stated reset.
        let go = Policy {
            max_trusted_cooldown: Duration::ZERO,
            ..Policy::default()
        };
        let mut s = Scheduler::default();
        s.record(&c, "m", &hinted(FailureScope::Model, reset - start), &go, start);
        assert_eq!(s.wait(&c, "m", start), Some(reset - start));
    }

    /// The trust count is its own: Go's no-hint backoff level neither raises the first
    /// bound nor is changed by a cut window.
    #[test]
    fn trust_count_and_go_backoff_level_are_separate() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let days = secs(6 * 24 * H);
        // No-hint 429s first: Go's level climbs (1 s, 2 s, 4 s, ...).
        let mut s = Scheduler::default();
        let mut now = Instant::now();
        for _ in 0..9 {
            s.record(&c, "m", &unhinted(FailureScope::Model), &p, now);
            now += secs(600);
        }
        assert_eq!(entry(&s, &c, "m").level, 9);
        s.record(&c, "m", &hinted(FailureScope::Model, days), &p, now);
        assert_eq!(s.wait(&c, "m", now), Some(secs(H)), "the first bound, not 2^9 hours");
        assert_eq!(entry(&s, &c, "m").level, 9, "Go's level keeps its meaning");
        // Hinted first, then no-hint 429s after the window: Go's backoff runs from its
        // own level, and the next cut window still doubles.
        let mut s = Scheduler::default();
        let mut now = Instant::now();
        s.record(&c, "m", &hinted(FailureScope::Model, days), &p, now);
        now += secs(H);
        s.record(&c, "m", &unhinted(FailureScope::Model), &p, now);
        assert_eq!(s.wait(&c, "m", now), Some(secs(1)), "Go's first backoff step");
        now += secs(1);
        s.record(&c, "m", &hinted(FailureScope::Model, days), &p, now);
        assert_eq!(s.wait(&c, "m", now), Some(secs(2 * H)), "the trust count survived");
    }

    /// A stated reset that lands during a cooldown the bound did not set (a 5xx, a
    /// 401, a no-hint 429 backoff) opens a bounded window instead of probing at once.
    #[test]
    fn a_reset_inside_another_cooldown_gets_the_bound() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let days = secs(6 * 24 * H);
        let start = Instant::now();
        let first = |status: u16, scope| {
            let mut s = Scheduler::default();
            s.record(
                &c,
                "m",
                &Outcome::Failure(ExecError::local(status, scope, "earlier")),
                &p,
                start,
            );
            s
        };
        for (status, scope, earlier) in [
            (503, FailureScope::Credential, secs(60)),
            (429, FailureScope::Model, secs(1)),
        ] {
            let mut s = first(status, scope);
            assert_eq!(s.wait(&c, "m", start), Some(earlier));
            let at = start + Duration::from_millis(500);
            s.record(&c, "m", &hinted(FailureScope::Model, days), &p, at);
            assert_eq!(s.wait(&c, "m", at), Some(secs(H)), "after {status}");
            assert!(entry(&s, &c, "m").trust.bounded, "after {status}");
            // A concurrent 6-day answer inside that window neither extends nor escalates.
            let later = at + secs(60);
            s.record(&c, "m", &hinted(FailureScope::Model, days), &p, later);
            assert_eq!(s.wait(&c, "m", at), Some(secs(H)), "after {status}");
            assert_eq!(entry(&s, &c, "m").trust.window, Some(0), "after {status}");
        }
        // A longer cooldown from another cause stands, and is not marked bounded.
        let mut s = first(401, FailureScope::Credential);
        let cap = Policy {
            max_trusted_cooldown: secs(600),
            ..Policy::default()
        };
        s.record(&c, "m", &hinted(FailureScope::Model, days), &cap, start);
        assert_eq!(s.wait(&c, "m", start), Some(secs(30 * MIN)));
        assert!(!entry(&s, &c, "m").trust.bounded);
    }

    /// Inside a live bounded window, a concurrent stated reset keeps the probe deadline
    /// unless it is earlier; with the setting off Go's merge applies.
    #[test]
    fn concurrent_resets_keep_or_shorten_the_window() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let start = Instant::now();
        let window = |s: &mut Scheduler, policy: &Policy| {
            s.record(&c, "m", &hinted(FailureScope::Model, secs(6 * 24 * H)), policy, start);
        };
        // A 45-minute reset at minute 30 does not push the probe to minute 75.
        let mut s = Scheduler::default();
        window(&mut s, &p);
        let at = start + secs(30 * MIN);
        s.record(&c, "m", &hinted(FailureScope::Model, secs(45 * MIN)), &p, at);
        assert_eq!(s.wait(&c, "m", start), Some(secs(H)));
        // A 30-minute reset at minute 1 moves it up to minute 31.
        let mut s = Scheduler::default();
        window(&mut s, &p);
        let at = start + secs(MIN);
        s.record(&c, "m", &hinted(FailureScope::Model, secs(30 * MIN)), &p, at);
        assert_eq!(s.wait(&c, "m", start), Some(secs(31 * MIN)));
        assert_eq!(entry(&s, &c, "m").trust.window, Some(0), "not a new window");
        // Setting off: Go keeps the later deadline.
        let go = Policy {
            max_trusted_cooldown: Duration::ZERO,
            ..Policy::default()
        };
        let mut s = Scheduler::default();
        s.record(&c, "m", &hinted(FailureScope::Model, secs(H)), &go, start);
        let at = start + secs(30 * MIN);
        s.record(&c, "m", &hinted(FailureScope::Model, secs(45 * MIN)), &go, at);
        assert_eq!(s.wait(&c, "m", start), Some(secs(75 * MIN)));
    }

    /// The first pick after a bounded window reserves the probe: the window keeps
    /// cooling until the probe's answer, and is never waited for by retries.
    #[test]
    fn one_probe_per_ended_window() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let start = Instant::now();
        for (scope, key) in [(FailureScope::Model, "m"), (FailureScope::Credential, "")] {
            let mut s = Scheduler::default();
            s.record(&c, "m", &hinted(scope, secs(6 * 24 * H)), &p, start);
            let near = start + secs(H) - secs(5);
            assert!(
                !s.retry_eligible(&c, "m", near),
                "{scope:?}: a bounded window is not waited for"
            );
            let end = start + secs(H);
            assert_eq!(s.wait(&c, "m", end), None, "{scope:?}: the probe is due");
            s.reserve_probe(&c, "m", end);
            assert_eq!(s.wait(&c, "m", end), Some(PROBE_HOLD), "{scope:?}: reserved");
            assert!(s.quota_cooling(&c, "m", end), "{scope:?}");
            assert!(!s.retry_eligible(&c, "m", end), "{scope:?}");
            // An unrelated model of the same credential waits too when the window is the
            // credential's.
            assert_eq!(s.wait(&c, "other", end).is_some(), key.is_empty(), "{scope:?}");
            // The probe's 429 opens the next window; its success would clear all.
            s.record(&c, "m", &hinted(scope, secs(5 * 24 * H)), &p, end + secs(2));
            assert_eq!(s.wait(&c, "m", end + secs(2)), Some(secs(2 * H)), "{scope:?}");
            assert_eq!(entry(&s, &c, key).trust.probe, None);
            // A cancelled probe frees the window at once; a lost one after PROBE_HOLD.
            let next = end + secs(2) + secs(2 * H);
            s.reserve_probe(&c, "m", next);
            s.record(&c, "m", &Outcome::Cancelled, &p, next + secs(1));
            assert_eq!(s.wait(&c, "m", next + secs(1)), None, "{scope:?}");
            s.reserve_probe(&c, "m", next + secs(1));
            assert_eq!(s.wait(&c, "m", next + secs(1) + PROBE_HOLD), None, "{scope:?}");
        }
        // Only bounded windows reserve: an ended Go backoff does not.
        let mut s = Scheduler::default();
        s.record(&c, "m", &unhinted(FailureScope::Model), &p, start);
        s.reserve_probe(&c, "m", start + secs(2));
        assert_eq!(s.wait(&c, "m", start + secs(2)), None);
    }

    /// A credential-wide window escalates the model keys too; when the credential
    /// recovers through a success on another model, they forget the count.
    #[test]
    fn a_success_on_another_model_clears_the_escalation() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let start = Instant::now();
        let days = secs(6 * 24 * H);
        let mut s = Scheduler::default();
        s.record(&c, "a", &hinted(FailureScope::Credential, days), &p, start);
        assert_eq!(entry(&s, &c, "a").trust.window, Some(0));
        let end = start + secs(H);
        s.record(&c, "b", &Outcome::Success, &p, end);
        s.record(&c, "a", &hinted(FailureScope::Model, days), &p, end);
        assert_eq!(s.wait(&c, "a", end), Some(secs(H)), "the first bound again");
    }

    /// A probe the upstream accepted (a stream started) ends the window's question at
    /// once: the next pick goes through while the stream runs, and a later hinted 429
    /// still doubles. A credential-wide window's copies on model keys are disarmed too;
    /// a newer live window is left alone.
    #[test]
    fn an_accepted_probe_frees_the_account_and_keeps_the_count() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let days = secs(6 * 24 * H);
        let start = Instant::now();
        let end = start + secs(H);
        // Model scope.
        let mut s = Scheduler::default();
        s.record(&c, "m", &hinted(FailureScope::Model, days), &p, start);
        s.reserve_probe(&c, "m", end);
        assert!(s.wait(&c, "m", end).is_some(), "reserved");
        s.accept_probe(&c, "m", end);
        assert_eq!(s.wait(&c, "m", end), None, "the next pick goes through");
        s.reserve_probe(&c, "m", end + secs(1));
        assert_eq!(s.wait(&c, "m", end + secs(1)), None, "no second reservation");
        let later = end + secs(60);
        s.record(&c, "m", &hinted(FailureScope::Model, days), &p, later);
        assert_eq!(s.wait(&c, "m", later), Some(secs(2 * H)), "the count survived");
        // A live window (another model's newer one) is untouched.
        s.accept_probe(&c, "m", later + secs(1));
        assert!(entry(&s, &c, "m").trust.bounded);
        // Credential scope: B's accepted probe disarms A's copy as well.
        let mut s = Scheduler::default();
        s.record(&c, "a", &hinted(FailureScope::Credential, days), &p, start);
        s.reserve_probe(&c, "b", end);
        s.accept_probe(&c, "b", end);
        assert_eq!(s.wait(&c, "b", end), None);
        s.reserve_probe(&c, "a", end);
        assert_eq!(s.wait(&c, "a", end), None, "A's copy does not reserve again");
        assert_eq!(entry(&s, &c, "a").trust.window, Some(0), "count kept");
    }

    /// The credential's probe answers its window and the window's copies on model keys,
    /// not a model's own window that ended unprobed before it: that model still gets a
    /// probe of its own, after a streamed probe and after a plain success alike, and the
    /// credential probe's later success leaves that model's newer probe state alone.
    #[test]
    fn a_credential_probe_leaves_a_models_own_ended_window() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let days = secs(6 * 24 * H);
        let start = Instant::now();
        for streamed in [true, false] {
            let mut s = Scheduler::default();
            s.record(&c, "m", &hinted(FailureScope::Model, days), &p, start);
            let opened = start + secs(H) + secs(MIN);
            s.record(&c, "a", &hinted(FailureScope::Credential, days), &p, opened);
            let end = opened + secs(H);
            s.reserve_probe(&c, "b", end);
            if streamed {
                s.accept_probe(&c, "b", end);
            } else {
                s.record(&c, "b", &Outcome::Success, &p, end);
            }
            s.reserve_probe(&c, "a", end);
            assert_eq!(s.wait(&c, "a", end), None, "streamed {streamed}: A's copy is answered");
            s.reserve_probe(&c, "m", end);
            assert_eq!(
                s.wait(&c, "m", end),
                Some(PROBE_HOLD),
                "streamed {streamed}: m reserves its own probe"
            );
        }
        // The credential's stream started, then m's probe was taken over by a later one
        // that was accepted too. The credential stream's success comes from an older
        // lease: it leaves m's accepted window alone, so the superseded m probe's late
        // 429 is still ignored and the later probe's success frees m.
        let mut s = Scheduler::default();
        s.record(&c, "m", &hinted(FailureScope::Model, days), &p, start);
        let opened = start + secs(H) + secs(MIN);
        s.record(&c, "a", &hinted(FailureScope::Credential, days), &p, opened);
        let end = opened + secs(H);
        let credential_probe = s.reserve_probe(&c, "b", end);
        s.accept_probe_picked(&c, "b", credential_probe, end);
        let first = s.reserve_probe(&c, "m", end);
        let lapsed = end + PROBE_HOLD + secs(1);
        let second = s.reserve_probe(&c, "m", lapsed);
        assert!(credential_probe != first && first != second, "real generations");
        s.accept_probe_picked(&c, "m", second, lapsed);
        let done = lapsed + secs(1);
        s.record_picked(&c, "b", &Outcome::Success, &p, credential_probe, done);
        s.record_picked(&c, "m", &hinted(FailureScope::Model, days), &p, first, done);
        assert_eq!(s.wait(&c, "m", done), None, "no window from the superseded probe");
        s.record_picked(&c, "m", &Outcome::Success, &p, second, done + secs(1));
        assert!(
            !s.cooldowns.contains_key(&(c.id.clone(), "m".to_owned())),
            "m recovered"
        );
    }

    /// A probe whose hold lapsed was taken over by a later probe: its late 429 neither
    /// opens the next window nor touches the later probe's reservation, also once the
    /// later probe was accepted, so the later probe's success frees the account.
    #[test]
    fn a_superseded_probes_429_leaves_the_window_to_the_current_probe() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let days = secs(6 * 24 * H);
        let start = Instant::now();
        for scope in [FailureScope::Model, FailureScope::Credential] {
            // The second probe is still unanswered, or already accepted (its stream's
            // first event arrived) when the first one's 429 comes back.
            for accepted in [false, true] {
                let mut s = Scheduler::default();
                s.record(&c, "m", &hinted(scope, days), &p, start);
                let end = start + secs(H);
                let first = s.reserve_probe(&c, "m", end);
                let lapsed = end + PROBE_HOLD + secs(1);
                let second = s.reserve_probe(&c, "m", lapsed);
                if accepted {
                    s.accept_probe_picked(&c, "m", second, lapsed);
                }
                let late = lapsed + secs(1);
                s.record_picked(&c, "m", &hinted(scope, days - secs(H)), &p, first, late);
                assert_eq!(
                    s.wait(&c, "m", late),
                    (!accepted).then(|| PROBE_HOLD - secs(1)),
                    "{scope:?}, accepted {accepted}: still the second probe's"
                );
                s.record_picked(&c, "m", &Outcome::Success, &p, second, late + secs(1));
                assert_eq!(
                    s.wait(&c, "m", late + secs(1)),
                    None,
                    "{scope:?}, accepted {accepted}: recovered"
                );
            }
        }
    }

    /// A credential-wide window that its probe's 429 renews carries its copies on model
    /// keys along, ended ones included: when the next probe recovers the account, no copy
    /// is left to ask for a probe of its own or to keep the count.
    #[test]
    fn a_renewed_credential_window_carries_its_ended_copies() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let days = secs(6 * 24 * H);
        let start = Instant::now();
        for streamed in [true, false] {
            let mut s = Scheduler::default();
            s.record(&c, "a", &hinted(FailureScope::Credential, days), &p, start);
            let first_end = start + secs(H);
            s.reserve_probe(&c, "b", first_end);
            let renewed = first_end + secs(1);
            s.record(&c, "b", &hinted(FailureScope::Credential, days - secs(H)), &p, renewed);
            let second_end = renewed + secs(2 * H);
            s.reserve_probe(&c, "c", second_end);
            if streamed {
                s.accept_probe(&c, "c", second_end);
            } else {
                s.record(&c, "c", &Outcome::Success, &p, second_end);
            }
            s.reserve_probe(&c, "a", second_end);
            assert_eq!(s.wait(&c, "a", second_end), None, "streamed {streamed}: no probe for A");
            if !streamed {
                s.record(&c, "a", &hinted(FailureScope::Model, days), &p, second_end);
                assert_eq!(s.wait(&c, "a", second_end), Some(secs(H)), "the count is cleared");
            }
        }
    }

    /// Generations compare in serial-number order: a window opened just after the counter
    /// wraps is newer than a lease picked just before, and older than one picked after.
    #[test]
    fn generations_compare_across_the_wrap() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let start = Instant::now();
        let mut s = Scheduler {
            generation: u32::MAX,
            ..Scheduler::default()
        };
        let before = s.reserve_probe(&c, "m", start);
        assert_eq!(before, u32::MAX);
        s.record(&c, "m", &hinted(FailureScope::Model, secs(6 * 24 * H)), &p, start);
        assert_eq!(entry(&s, &c, "m").trust.generation, 0, "wrapped");
        let at = start + secs(MIN);
        s.record_picked(&c, "m", &Outcome::Success, &p, before, at);
        assert_eq!(s.wait(&c, "m", at), Some(secs(H - MIN)), "an older success leaves it");
        s.record_picked(&c, "m", &Outcome::Success, &p, 0, at);
        assert_eq!(s.wait(&c, "m", at), None, "a newer success ends it");
    }

    /// An earlier credential-wide reset moves up the model keys that inherited the
    /// window; an independent longer cooldown stands.
    #[test]
    fn an_earlier_credential_reset_shortens_inherited_model_windows() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let start = Instant::now();
        let mut s = Scheduler::default();
        let support = ExecError::local(400, FailureScope::Model, "model is not supported");
        s.record(&c, "support", &Outcome::Failure(support), &p, start);
        s.record(&c, "sibling", &unhinted(FailureScope::Model), &p, start);
        s.record(&c, "a", &hinted(FailureScope::Credential, secs(6 * 24 * H)), &p, start);
        let at = start + secs(MIN);
        s.record(&c, "a", &hinted(FailureScope::Credential, secs(30 * MIN)), &p, at);
        for model in ["a", "sibling", "unrelated"] {
            assert_eq!(s.wait(&c, model, start), Some(secs(31 * MIN)), "{model}");
        }
        assert_eq!(s.wait(&c, "support", start), Some(secs(12 * H)));
    }

    /// A model's own bounded window that a credential-wide window covers keeps its own
    /// probe time: an earlier credential reset moves the credential's probe up, never
    /// the model's before its own bound, whether the credential's 429 came from another
    /// model or from that one.
    #[test]
    fn an_earlier_credential_reset_never_moves_a_models_own_probe_earlier() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let days = secs(6 * 24 * H);
        let start = Instant::now();
        let own_end = start + secs(H);
        for failing in ["a", "m"] {
            let mut s = Scheduler::default();
            s.record(&c, "m", &hinted(FailureScope::Model, days), &p, start);
            let opened = start + secs(30 * MIN);
            s.record(&c, failing, &hinted(FailureScope::Credential, days), &p, opened);
            let at = opened + secs(MIN);
            s.record(&c, failing, &hinted(FailureScope::Credential, secs(10 * MIN)), &p, at);
            let moved = at + secs(10 * MIN);
            assert_eq!(
                s.wait(&c, "other", moved),
                None,
                "{failing}: the credential's probe is due"
            );
            assert_eq!(
                s.wait(&c, "m", moved),
                Some(own_end - moved),
                "{failing}: m waits for its own bound"
            );
        }
    }

    /// A model's own cooldown of another cause (a 401's 30 minutes) under a bounded
    /// credential window keeps its own deadline, as Go keeps the later one: an earlier
    /// credential reset brings the credential's probe forward, never the model back
    /// before its own deadline, whether the credential's 429 came from another model or
    /// from that one.
    #[test]
    fn an_earlier_credential_reset_never_shortens_a_models_own_cooldown() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let start = Instant::now();
        let unauthorized = Outcome::Failure(ExecError::local(401, FailureScope::Credential, "unauthorized"));
        for failing in ["a", "m"] {
            let mut s = Scheduler::default();
            s.record(&c, "m", &unauthorized, &p, start);
            assert_eq!(s.wait(&c, "m", start), Some(secs(30 * MIN)));
            let opened = start + secs(MIN);
            s.record(
                &c,
                failing,
                &hinted(FailureScope::Credential, secs(6 * 24 * H)),
                &p,
                opened,
            );
            let at = start + secs(2 * MIN);
            s.record(&c, failing, &hinted(FailureScope::Credential, secs(MIN)), &p, at);
            let moved = start + secs(3 * MIN);
            assert_eq!(
                s.wait(&c, "other", moved),
                None,
                "{failing}: the credential's probe is due"
            );
            assert_eq!(
                s.wait(&c, "m", moved),
                Some(secs(27 * MIN)),
                "{failing}: m keeps its own 30 minutes"
            );
        }
    }

    /// Provenance follows the winning deadline: the failing model's own longer bounded
    /// window keeps its flag and stated reset through a credential-wide merge.
    #[test]
    fn the_own_key_keeps_its_window_through_a_credential_merge() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let days = secs(6 * 24 * H);
        let start = Instant::now();
        let mut s = Scheduler::default();
        s.record(&c, "m", &hinted(FailureScope::Model, days), &p, start);
        s.record(
            &c,
            "m",
            &hinted(FailureScope::Model, days - secs(H)),
            &p,
            start + secs(H),
        );
        let own_end = start + secs(3 * H);
        let at = start + secs(H) + secs(MIN);
        s.record(&c, "m", &hinted(FailureScope::Credential, days), &p, at);
        let own = entry(&s, &c, "m");
        assert_eq!(own.deadline, own_end);
        assert!(own.trust.bounded && own.trust.stated == Some(start + days));
        s.reserve_probe(&c, "m", own_end);
        assert!(
            entry(&s, &c, "m").trust.probe.is_some(),
            "it reserves a probe at its end"
        );
    }

    /// A restore into a scheduler that already holds a later deadline keeps that
    /// entry's provenance and classification, merging the counts.
    #[test]
    fn a_restore_keeps_the_winner_trust() {
        use crate::cooldown_store::{Quota, Record};
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let (now, wall) = (Instant::now(), SystemTime::now());
        let days = secs(6 * 24 * H);
        let mut s = Scheduler::default();
        s.record(&c, "m", &hinted(FailureScope::Model, days), &p, now);
        let live = entry(&s, &c, "m").trust;
        let shorter = Record {
            auth_id: "a".into(),
            model: "m".into(),
            next_retry_after: Some(wall + secs(10 * MIN)),
            quota: Quota {
                exceeded: true,
                reason: "quota".into(),
                next_recover_at: Some(wall + secs(10 * MIN)),
                trust_windows: Some(3),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(s.restore(&c, &shorter, true, secs(H), now, wall));
        let merged = entry(&s, &c, "m");
        assert_eq!(merged.deadline, now + secs(H));
        assert!(merged.trust.bounded && merged.trust.stated == live.stated);
        assert_eq!(merged.trust.window, Some(2), "counts merge");
        assert_eq!(merged.trust.generation, live.generation, "the live window's generation");
    }

    /// A restore while serving (`save-cooldown-status` turned on) that reinstalls a
    /// bounded window gives it a fresh generation: a stream picked before the window
    /// opened still cannot end it.
    #[test]
    fn a_restore_while_serving_keeps_the_window_newer_than_older_leases() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let (now, wall) = (Instant::now(), SystemTime::now());
        let mut s = Scheduler::default();
        let stream = s.reserve_probe(&c, "m", now);
        s.record(&c, "m", &hinted(FailureScope::Model, secs(6 * 24 * H)), &p, now);
        let saved = s.records(&c, now, wall);
        assert!(s.restore(&c, &saved[0], true, secs(H), now, wall));
        assert!(entry(&s, &c, "m").trust.bounded);
        let at = now + secs(MIN);
        s.record_picked(&c, "m", &Outcome::Success, &p, stream, at);
        assert_eq!(s.wait(&c, "m", at), Some(secs(H - MIN)), "the window stands");
    }

    /// A 503 from a request already in flight inside a live bounded window: the window
    /// stands, and its saved record keeps the stated reset and count.
    #[test]
    fn a_503_inside_a_bounded_window_keeps_it() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let (now, wall) = (Instant::now(), SystemTime::now());
        let days = secs(6 * 24 * H);
        let mut s = Scheduler::default();
        s.record(&c, "m", &hinted(FailureScope::Model, days), &p, now);
        let at = now + secs(30 * MIN);
        let unavailable = ExecError::local(503, FailureScope::Model, "overloaded");
        s.record(&c, "m", &Outcome::Failure(unavailable), &p, at);
        let saved = &s.records(&c, at, wall + secs(30 * MIN))[0];
        assert!(saved.quota.exceeded);
        assert_eq!(saved.quota.next_recover_at, Some(wall + days));
        assert_eq!(saved.quota.trust_windows, Some(1));
        assert_eq!(saved.reason, "quota");
    }

    /// A Cloudflare challenge from a request already in flight inside a live bounded
    /// window leaves the window as it is but still raises Go's level, as Go does.
    #[test]
    fn a_cloudflare_challenge_inside_a_bounded_window_raises_the_level() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let start = Instant::now();
        let mut s = Scheduler::default();
        s.record(&c, "m", &hinted(FailureScope::Model, secs(6 * 24 * H)), &p, start);
        let challenge = Outcome::Failure(ExecError::local(403, FailureScope::Model, "cloudflare challenge"));
        for (n, level) in [(1, 1), (2, 2)] {
            s.record(&c, "m", &challenge, &p, start + secs(n * MIN));
            assert_eq!(entry(&s, &c, "m").level, level);
            assert_eq!(s.wait(&c, "m", start), Some(secs(H)), "the window stands");
            assert!(entry(&s, &c, "m").trust.bounded);
        }
    }

    /// `.cds` records: the probe time is the retry, the stated reset Go's recovery time,
    /// and the count is its own field. Restores never escalate, and Go's records get
    /// the first bound whatever their backoff level.
    #[test]
    fn restored_quota_resets_are_bounded_too() {
        use crate::cooldown_store::{Quota, Record};
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let (now, wall) = (Instant::now(), SystemTime::now());
        let cap = secs(H);
        let days = secs(6 * 24 * H);
        let mut s = Scheduler::default();
        s.record(&c, "m", &hinted(FailureScope::Model, days), &p, now);
        let saved = s.records(&c, now, wall);
        assert_eq!(saved[0].next_retry_after, Some(wall + secs(H)));
        assert_eq!(saved[0].quota.next_recover_at, Some(wall + days));
        assert_eq!(
            (saved[0].quota.trust_windows, saved[0].quota.backoff_level),
            (Some(1), 0)
        );
        // Our own record restores as it was: one hour, the same window.
        let mut back = Scheduler::default();
        assert!(back.restore(&c, &saved[0], true, cap, now, wall));
        assert_eq!(back.wait(&c, "m", now), Some(secs(H)));
        assert_eq!(back.records(&c, now, wall), saved);
        let record_at = |retry: Duration, trust_windows, backoff_level| Record {
            auth_id: "a".into(),
            model: "m".into(),
            next_retry_after: Some(wall + retry),
            quota: Quota {
                exceeded: true,
                reason: "quota".into(),
                next_recover_at: Some(wall + days),
                backoff_level,
                trust_windows,
                ..Default::default()
            },
            ..Default::default()
        };
        // Go's record with backoff level 2: the first bound; the level stays Go's.
        let mut s = Scheduler::default();
        assert!(s.restore(&c, &record_at(days, None, 2), true, cap, now, wall));
        assert_eq!(s.wait(&c, "m", now), Some(secs(H)));
        assert_eq!(entry(&s, &c, "m").level, 2);
        // Our record in its third window (4 h, count 2): restored as it is, and the
        // count survives, so the next cut window is 8 h.
        let mut s = Scheduler::default();
        assert!(s.restore(&c, &record_at(secs(4 * H), Some(3), 0), true, cap, now, wall));
        assert_eq!(s.wait(&c, "m", now), Some(secs(4 * H)));
        let trust = entry(&s, &c, "m").trust;
        assert!(trust.bounded && trust.window == Some(2));
        assert_eq!(trust.stated, Some(now + days));
        let later = now + secs(4 * H);
        s.record(&c, "m", &hinted(FailureScope::Model, days - secs(4 * H)), &p, later);
        assert_eq!(s.wait(&c, "m", later), Some(secs(8 * H)));
        // A reset trusted as stated restores as it was, unbounded, keeping the count.
        let ninety = secs(90 * MIN);
        let trusted = Record {
            quota: Quota {
                next_recover_at: Some(wall + ninety),
                ..record_at(ninety, Some(1), 0).quota
            },
            ..record_at(ninety, Some(1), 0)
        };
        let mut s = Scheduler::default();
        assert!(s.restore(&c, &trusted, true, cap, now, wall));
        assert_eq!(s.wait(&c, "m", now), Some(ninety));
        let back = &s.records(&c, now, wall)[0];
        assert_eq!(
            (
                back.next_retry_after,
                back.quota.next_recover_at,
                back.quota.trust_windows
            ),
            (trusted.next_retry_after, trusted.quota.next_recover_at, Some(1))
        );
        assert!(!entry(&s, &c, "m").trust.bounded);
        // An absurd saved count is clamped, not overflowed.
        let mut s = Scheduler::default();
        assert!(s.restore(&c, &record_at(secs(H), Some(u32::MAX), 0), true, cap, now, wall));
        assert_eq!(entry(&s, &c, "m").trust.window, Some(MAX_TRUST_WINDOW));
        // Setting off: Go's six days.
        let mut s = Scheduler::default();
        assert!(s.restore(&c, &record_at(days, Some(3), 0), true, Duration::ZERO, now, wall));
        assert_eq!(s.wait(&c, "m", now), Some(days));
    }

    /// A longer cooldown of another cause that a credential-wide quota marked as quota
    /// (model B's 12 h model-support cooldown during a six-day 429 on model A) keeps its
    /// own deadline through a save and a restore: it is cliproxy-rs's record, not Go's.
    #[test]
    fn a_marked_sibling_restores_its_own_deadline() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let (now, wall) = (Instant::now(), SystemTime::now());
        let mut s = Scheduler::default();
        let support = ExecError::local(400, FailureScope::Model, "model is not supported");
        s.record(&c, "b", &Outcome::Failure(support), &p, now);
        assert_eq!(s.wait(&c, "b", now), Some(secs(12 * H)), "Go's model-support cooldown");
        s.record(&c, "a", &hinted(FailureScope::Credential, secs(6 * 24 * H)), &p, now);
        let records = s.records(&c, now, wall);
        let b = records.iter().find(|r| r.model == "b").unwrap();
        assert!(b.quota.exceeded && b.quota.trust_windows.is_some(), "{b:?}");
        let mut back = Scheduler::default();
        for r in &records {
            back.restore(&c, r, true, secs(H), now, wall);
        }
        assert_eq!(back.wait(&c, "b", now), Some(secs(12 * H)));
        assert!(!entry(&back, &c, "b").trust.bounded, "not cut, so not a bounded window");
    }

    /// A credential-wide window does not lower a model's own count: a model already in
    /// its fourth window (8 h) keeps it when a credential-wide 1 h window outlasts its
    /// own, so its next cut window is 16 h, not 2 h. Both the sibling and the failing
    /// model's own key.
    #[test]
    fn credential_wide_windows_keep_the_larger_model_count() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let days = secs(6 * 24 * H);
        for failing in ["other", "m"] {
            let mut now = Instant::now();
            let mut s = Scheduler::default();
            for window in [1, 2, 4, 8] {
                s.record(&c, "m", &hinted(FailureScope::Model, days), &p, now);
                assert_eq!(s.wait(&c, "m", now), Some(secs(window * H)));
                if window < 8 {
                    now += secs(window * H);
                }
            }
            // Ten minutes before m's 8 h window ends, a credential-wide hint whose 1 h
            // window outlasts it.
            let late = now + secs(8 * H) - secs(10 * MIN);
            s.record(&c, failing, &hinted(FailureScope::Credential, days), &p, late);
            assert_eq!(entry(&s, &c, "m").trust.window, Some(3), "{failing}: count kept");
            let end = late + secs(H);
            s.record(&c, "m", &hinted(FailureScope::Model, days), &p, end);
            assert_eq!(s.wait(&c, "m", end), Some(secs(16 * H)), "{failing}");
        }
    }

    /// Go's level climbs under Cloudflare challenges too, even inside a live window; a
    /// stated reset there still gets the first bound.
    #[test]
    fn a_reset_inside_a_cloudflare_window_gets_the_first_bound() {
        let c = cred("a", serde_json::json!({}));
        let p = Policy::default();
        let mut now = Instant::now();
        let mut s = Scheduler::default();
        for _ in 0..5 {
            let challenge = ExecError::local(403, FailureScope::Model, "cloudflare challenge");
            s.record(&c, "m", &Outcome::Failure(challenge), &p, now);
            now += secs(1);
        }
        assert!(entry(&s, &c, "m").level >= 5);
        assert!(s.wait(&c, "m", now).is_some(), "inside the challenge window");
        s.record(&c, "m", &hinted(FailureScope::Model, secs(6 * 24 * H)), &p, now);
        assert_eq!(s.wait(&c, "m", now), Some(secs(H)));
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
        // Go trusts every stated reset (docs/DIFFERENCES-FROM-GO.md).
        let policy = Policy {
            max_trusted_cooldown: Duration::ZERO,
            ..Policy::default()
        };
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
        assert_eq!(cases.len(), 25);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let lcp_case = case["lcp"].as_bool().unwrap_or_default();
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
                let format = cpa_core::format::Format::parse(case["format"].as_str().unwrap_or("openai")).unwrap();
                let caller = match step["caller"].as_str() {
                    Some(c) => c,
                    None => case["caller"].as_str().unwrap_or_default(),
                };
                let caller = if lcp_case { caller } else { "" };
                let session = crate::session::resolve(format, &headers, payload, None, caller);
                // dispatch::run's LCP step.
                let lcp = (lcp_case && !session.explicit)
                    .then(|| crate::lcp::Request::new(format.as_str(), payload, caller))
                    .flatten()
                    .map(std::sync::Arc::new);
                let sel = Selection {
                    session: session.id,
                    session_parent: session.parent,
                    session_fork: session.fork,
                    lcp,
                    ..selection("m")
                };
                let find = |id: &str| creds.iter().find(|c| c.id == id).unwrap();
                let outcome = |op: &str| {
                    if op.ends_with("ok") {
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
                    }
                };
                match step["op"].as_str().unwrap() {
                    op @ ("pick" | "pick_ok" | "pick_fail") => {
                        let available: Vec<&Credential> = step["available"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|id| find(id.as_str().unwrap()))
                            .collect();
                        let (picked, bound) = s
                            .pick_session(&tag(&available), &sel, &policy, &|_| Windows::default(), now)
                            .unwrap();
                        assert_eq!(picked.id, step["picked"].as_str().unwrap(), "{name} step {i}");
                        // The metadata Go's pickLCP wrote, as the attempt reports it.
                        let got = bound.as_ref().map(|m| {
                            let (node_kind, fork, compaction) = m.node();
                            serde_json::json!({
                                "session": m.session, "parent": m.parent, "node_kind": node_kind,
                                "fork": fork, "compaction": compaction, "generation": m.access,
                            })
                        });
                        assert_eq!(got.as_ref(), step.get("lcp"), "{name} step {i} lcp");
                        if op != "pick" {
                            let picked = picked.clone();
                            s.session_result(&picked, &sel, &outcome(op), &policy, bound.as_ref(), now);
                        }
                    }
                    op => {
                        s.session_result(
                            find(step["auth"].as_str().unwrap()),
                            &sel,
                            &outcome(op),
                            &policy,
                            None,
                            now,
                        );
                    }
                }
            }
        }
    }

    /// The failure of a request picked before a policy change removes nothing bound
    /// after it, though the change replaced the matcher.
    #[test]
    fn delayed_failure_keeps_binding_made_after_policy_change() {
        let a = cred("a", serde_json::json!({}));
        let p = Policy {
            session_affinity: true,
            ..Default::default()
        };
        let next = Policy {
            session_affinity_ttl: Duration::from_secs(600),
            ..p.clone()
        };
        let now = Instant::now();
        let mut s = Scheduler::default();
        let body = br#"{"messages":[{"role":"user","content":"hello"}]}"#;
        let request = crate::lcp::Request::new("openai", body, "client-key").unwrap();
        let sel = Selection {
            lcp: Some(std::sync::Arc::new(request.clone())),
            ..selection("m")
        };
        let ranks = |_: &Credential| Windows::default();
        let mut old = None;
        for _ in 0..5 {
            old = s.pick_session(&tag(&[&a]), &sel, &p, &ranks, now).unwrap().1;
        }
        let old = old.unwrap();
        s.configure(&p, &next);
        let fresh = s
            .pick_session(&tag(&[&a]), &sel, &next, &ranks, now)
            .unwrap()
            .1
            .unwrap();
        assert!(fresh.access > old.access, "{} after {}", fresh.access, old.access);
        let failure = Outcome::Failure(ExecError::local(500, FailureScope::Credential, "boom"));
        s.session_result(&a, &sel, &failure, &p, Some(&old), now);
        let namespace = request.namespace("m");
        let kept = s.lcp.find(&namespace, request.prepared(), now);
        assert_eq!(kept.map(|m| m.auth), Some("a".to_owned()));
    }

    /// After session affinity is turned off, a request picked while it was on binds
    /// nothing: neither its result nor a retry under its policy.
    #[test]
    fn leases_from_before_disabling_affinity_bind_nothing() {
        let a = cred("a", serde_json::json!({}));
        let on = Policy {
            session_affinity: true,
            ..Default::default()
        };
        let off = Policy::default();
        let now = Instant::now();
        let mut s = Scheduler::default();
        s.configure(&off, &on);
        let body = br#"{"messages":[{"role":"user","content":"hello"}]}"#;
        let request = crate::lcp::Request::new("openai", body, "client-key").unwrap();
        let namespace = request.namespace("m");
        let lcp = Selection {
            lcp: Some(std::sync::Arc::new(request.clone())),
            ..selection("m")
        };
        let explicit = Selection {
            session: Some("s1".into()),
            ..selection("m")
        };
        let ranks = |_: &Credential| Windows::default();
        let bound = s.pick_session(&tag(&[&a]), &lcp, &on, &ranks, now).unwrap().1;
        assert!(bound.is_some());
        s.configure(&on, &off);
        assert!(s.lcp.find(&namespace, request.prepared(), now).is_none());
        s.session_result(&a, &lcp, &Outcome::Success, &on, bound.as_ref(), now);
        s.session_result(&a, &explicit, &Outcome::Success, &on, None, now);
        let retry = s.pick_session(&tag(&[&a]), &lcp, &on, &ranks, now).unwrap().1;
        assert!(retry.is_none());
        s.pick_session(&tag(&[&a]), &explicit, &on, &ranks, now);
        assert!(s.lcp.find(&namespace, request.prepared(), now).is_none());
        assert!(s.affinity.get(&session_keys(&explicit).unwrap().primary, now).is_none());
        // Turning it back on binds again.
        s.configure(&off, &on);
        assert!(s.pick_session(&tag(&[&a]), &lcp, &on, &ranks, now).unwrap().1.is_some());
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
            (
                w.weekly_reset,
                w.rolled_over.map(|r| (r.window, epoch(r.at))),
                w.exhausted
            ),
            (
                None,
                Some((WindowId::Claude7d, epoch(now - Duration::from_secs(60)))),
                false
            )
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

    /// The oracle's sequence: a probe made for one window's rollover must not suppress a
    /// later rollover of another window.
    #[test]
    fn soonest_reset_probe_markers_are_kept_per_window() {
        let wall = SystemTime::now();
        let (a, b) = (cred("a", serde_json::json!({})), cred("b", serde_json::json!({})));
        let b_known = claude(wall, 5 * DAY, "allowed", wall + Duration::from_secs(3600));
        let r = wall + Duration::from_secs(600);
        let after = |t: SystemTime, secs: u64| t + Duration::from_secs(secs);
        // 1. Only `Secondary-Reset-At: R` and no lengths: secondary is taken as weekly.
        let partial = signals(&[("X-Codex-Secondary-Reset-At", epoch(r))]);
        // 2. The probe's answer is complete: the weekly window is primary, resetting at
        //    R + 1 day; secondary is the 5-hour window.
        let complete = signals(&[
            ("X-Codex-Primary-Used-Percent", "10".into()),
            ("X-Codex-Primary-Window-Minutes", "10080".into()),
            ("X-Codex-Primary-Reset-At", epoch(after(r, DAY))),
            ("X-Codex-Secondary-Used-Percent", "10".into()),
            ("X-Codex-Secondary-Window-Minutes", "300".into()),
            ("X-Codex-Secondary-Reset-At", epoch(after(r, 5 * 3600))),
        ]);
        let mut s = Scheduler::default();
        let now = Instant::now();
        let mut pick = |a_now: Windows| {
            let ranks = |x: &Credential| if x.id == "a" { a_now } else { b_known };
            s.pick_ranked(&tag(&[&a, &b]), &selection("m"), &soonest(), &ranks, now)
                .unwrap()
                .id
                .clone()
        };
        // Just after R, `a`'s secondary rolled over: its probe.
        let w = Windows::observed("codex", &partial, wall, after(r, 1));
        assert_eq!(w.rolled_over.map(|r| r.window), Some(WindowId::CodexSecondary));
        assert_eq!(pick(w), "a");
        // The answer reports primary resetting in a day, sooner than `b`: `a` keeps it.
        assert_eq!(
            pick(Windows::observed("codex", &complete, after(r, 1), after(r, 2))),
            "a"
        );
        // 3. Just after R + 1 day, primary rolls over: one probe, not suppressed by the
        //    secondary's marker R (R + 1 day is within half of primary's week of R).
        let w = Windows::observed("codex", &complete, after(r, 1), after(r, DAY + 1));
        assert_eq!(w.rolled_over.map(|r| r.window), Some(WindowId::CodexPrimary));
        assert_eq!(pick(w), "a");
        assert_eq!(pick(w), "b", "probed for that rollover");
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
