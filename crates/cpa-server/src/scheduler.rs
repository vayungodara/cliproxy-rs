//! Credential routing policy. Config parsing remains owned by cpa-core; integration
//! publishes this policy through Runtime::publish_policy.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, FailureScope};
use serde::Deserialize;
use serde_json::Value;

use crate::runtime::{Outcome, Selection};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Strategy {
    #[default]
    RoundRobin,
    FillFirst,
    WeightedRoundRobin,
}

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
    /// ponytail: reserved for integration; separate cooldown persistence/restore
    /// remains M4-0026, not credential JSON write-back.
    pub save_cooldown_status: bool,
    /// routing.force-model-prefix: bool, default false.
    pub force_model_prefix: bool,
    /// OAuth-only provider overrides; API keys must not inherit these.
    pub oauth_disable_cooling: HashMap<String, bool>,
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
            oauth_disable_cooling: HashMap::new(),
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

    pub(crate) fn cooling_disabled(&self, c: &Credential) -> bool {
        boolean(c, "disable_cooling")
            .or_else(|| {
                oauth(c)
                    .then(|| self.oauth_disable_cooling.get(&c.provider).copied())
                    .flatten()
            })
            .unwrap_or(self.disable_cooling)
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
            let body = String::from_utf8_lossy(&error.body);
            for rule in rules {
                if rule.status <= 0 || rule.status != i64::from(error.status) {
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
                    stop,
                    cooldown,
                    force_cooldown: cooldown,
                };
            }
        }
        // ponytail: compatibility for the baseline executor's untyped transport error.
        // Remove the string check when all executors report FailureScope::Transport.
        let transport = error.scope == FailureScope::Transport
            || (error.headers.is_empty() && error.body.starts_with(b"upstream request failed:"));
        // ponytail: generic scopes/statuses only; Go's provider-specific model-support,
        // invalid_grant, Cloudflare and compact classifiers remain M4-0024.
        ErrorAction {
            stop: error.scope == FailureScope::Request,
            cooldown: error.scope != FailureScope::Request && !transport,
            force_cooldown: false,
        }
    }
}

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

pub fn canonical_model(model: &str) -> &str {
    // Go's thinking.ParseSuffix removes the terminal parenthesized thinking suffix.
    let model = model.trim();
    if model.ends_with(')') {
        model
            .rsplit_once('(')
            .map(|(base, _)| base.trim())
            .filter(|base| !base.is_empty())
            .unwrap_or(model)
    } else {
        model
    }
}

fn wildcard(pattern: &str, model: &str) -> bool {
    // '*' is the only wildcard in Go's excluded-model matcher.
    let pattern = format!("^{}$", regex::escape(pattern).replace("\\*", ".*"));
    regex::Regex::new(&pattern).is_ok_and(|re| re.is_match(model))
}

pub fn execution_model(c: &Credential, model: &str, policy: &Policy) -> Option<String> {
    let requested = model.trim();
    let model = canonical_model(requested);
    let suffix = requested.strip_prefix(model).unwrap_or("");
    let prefix = c
        .attributes
        .get("prefix")
        .map(String::as_str)
        .or_else(|| c.str("prefix"))
        .unwrap_or("");
    let model = if prefix.is_empty() {
        model
    } else if let Some(model) = model.strip_prefix(&format!("{prefix}/")) {
        model
    } else if policy.force_model_prefix || model.contains('/') {
        return None;
    } else {
        model
    };
    // ponytail: local aliases only; registry listings and global OAuth aliases remain
    // the model-registry stream's responsibility.
    let mut resolved = model;
    if let Some(aliases) = metadata(c, "model_aliases").and_then(Value::as_array) {
        for alias in aliases {
            if alias.get("alias").and_then(Value::as_str) == Some(model) {
                resolved = alias.get("name").and_then(Value::as_str)?;
                break;
            }
        }
    }
    let excluded = metadata(c, "excluded_models").and_then(Value::as_array);
    if excluded.is_some_and(|list| list.iter().filter_map(Value::as_str).any(|p| wildcard(p, resolved))) {
        return None;
    }
    Some(if resolved.ends_with(')') {
        resolved.to_owned()
    } else {
        format!("{resolved}{suffix}")
    })
}

#[derive(Default)]
struct Rotation {
    last: String,
    weights: HashMap<String, i64>,
    current: HashMap<String, i64>,
}

struct Cooldown {
    deadline: Instant,
    level: u32,
    status: u16,
    quota: bool,
}

#[derive(Default)]
pub(crate) struct Scheduler {
    rotations: HashMap<(String, String), Rotation>,
    cooldowns: HashMap<(String, String), Cooldown>,
    bindings: HashMap<(String, String, String), (String, Instant)>,
}

impl Scheduler {
    pub fn configure(&mut self, previous: &Policy, next: &Policy) {
        if previous.strategy != next.strategy
            || previous.session_affinity != next.session_affinity
            || previous.session_affinity_ttl != next.session_affinity_ttl
            || previous.session_affinity_subagents != next.session_affinity_subagents
        {
            self.rotations.clear();
            self.bindings.clear();
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

    pub fn retry_eligible(&self, c: &Credential, model: &str, now: Instant) -> bool {
        let model = canonical_model(model);
        [model, ""].into_iter().all(|model| {
            self.cooldowns
                .get(&(c.id.clone(), model.to_owned()))
                .is_none_or(|s| s.deadline <= now || retry_status(s.status))
        })
    }

    pub fn pick<'a>(
        &mut self,
        candidates: &[&'a Credential],
        selection: &Selection,
        policy: &Policy,
        now: Instant,
    ) -> Option<&'a Credential> {
        if candidates.is_empty() {
            return None;
        }
        self.bindings.retain(|_, (_, deadline)| *deadline > now);
        let binding_key = selection.session.as_ref().map(|s| {
            (
                selection.provider.clone(),
                canonical_model(&selection.model).to_owned(),
                s.clone(),
            )
        });
        if policy.session_affinity
            && let Some((id, deadline)) = binding_key.as_ref().and_then(|key| self.bindings.get_mut(key))
            && let Some(c) = candidates.iter().find(|c| c.id == *id)
        {
            *deadline = now + policy.session_affinity_ttl;
            return Some(c);
        }
        let tier = candidates.iter().map(|c| integer(c, "priority").unwrap_or(0)).max()?;
        let mut candidates: Vec<_> = candidates
            .iter()
            .copied()
            .filter(|c| integer(c, "priority").unwrap_or(0) == tier)
            .collect();
        candidates.sort_by(|a, b| a.id.cmp(&b.id));
        let key = (selection.provider.clone(), canonical_model(&selection.model).to_owned());
        if !self.rotations.contains_key(&key) && self.rotations.len() >= 4096 {
            self.rotations.clear();
        }
        let state = self.rotations.entry(key).or_default();
        let picked = match policy.strategy {
            Strategy::FillFirst => candidates[0],
            Strategy::RoundRobin => {
                let picked = candidates
                    .iter()
                    .find(|c| c.id > state.last)
                    .copied()
                    .unwrap_or(candidates[0]);
                state.last.clone_from(&picked.id);
                picked
            }
            Strategy::WeightedRoundRobin => {
                if candidates
                    .iter()
                    .any(|c| state.weights.get(&c.id).is_some_and(|w| *w != weight(c)))
                {
                    state.current.clear();
                }
                if state.weights.len() > 1024 || state.current.len() > 1024 {
                    state.weights.retain(|id, _| candidates.iter().any(|c| c.id == *id));
                    state.current.retain(|id, _| candidates.iter().any(|c| c.id == *id));
                }
                let mut total = 0i64;
                let mut best = i64::MIN;
                let mut picked = candidates[0];
                for c in candidates {
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
        };
        if policy.session_affinity
            && let Some(key) = binding_key
        {
            // ponytail: bounded clear instead of Go's LRU at capacity. Replace with
            // eviction order when pools approach 65536 simultaneous sessions.
            if self.bindings.len() >= 65536 {
                self.bindings.clear();
            }
            self.bindings
                .insert(key, (picked.id.clone(), now + policy.session_affinity_ttl));
        }
        Some(picked)
    }

    pub fn admits(&self, c: &Credential, policy: &Policy) -> bool {
        policy.strategy != Strategy::WeightedRoundRobin || weight(c) > 0
    }

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
            Outcome::Cancelled => return,
            Outcome::Failure(error) => error,
        };
        let action = policy.error_action(c, error);
        if !action.cooldown {
            return;
        }
        let credential_quota = error.status == 429 && error.scope == FailureScope::Credential;
        let key = if credential_quota {
            (c.id.clone(), String::new())
        } else {
            key
        };
        // Go applies the normal policy first; force-cooldown supplies a 1m fallback
        // only if that policy produced no deadline (including disable-cooling).
        let cooling_disabled = policy.cooling_disabled(c);
        let transient_disabled =
            policy.transient_error_cooldown_seconds < 0 && !matches!(error.status, 401..=404 | 429);
        if cooling_disabled || transient_disabled {
            if !action.force_cooldown {
                self.cooldowns.remove(&key);
                return;
            }
            let next = now + Duration::from_secs(60);
            let deadline = self
                .cooldowns
                .get(&key)
                .filter(|s| s.deadline > now)
                .map(|s| s.deadline.max(next))
                .unwrap_or(next);
            self.cooldowns.insert(
                key,
                Cooldown {
                    deadline,
                    level: 0,
                    status: error.status,
                    quota: error.status == 429,
                },
            );
            if credential_quota {
                self.extend_siblings(c, deadline, now);
            }
            return;
        }
        let prev = self.cooldowns.get(&key);
        let mut level = prev.filter(|s| s.quota).map(|s| s.level).unwrap_or(0);
        let duration = match error.status {
            401..=403 => Duration::from_secs(1800),
            404 => error
                .retry_after
                .filter(|d| !d.is_zero())
                .unwrap_or(Duration::from_secs(43200)),
            429 => match error.retry_after {
                Some(d) => d.max(Duration::from_secs(10)),
                None => {
                    // An active quota deadline is reused, not exponentially extended.
                    if let Some(prev) = prev.filter(|s| s.quota && s.deadline > now) {
                        let deadline = prev.deadline;
                        if credential_quota {
                            self.extend_siblings(c, deadline, now);
                        }
                        return;
                    }
                    let seconds = (1u64 << level.min(11)).min(1800);
                    if seconds < 1800 {
                        level += 1;
                    }
                    Duration::from_secs(seconds)
                }
            },
            _ => {
                let seconds = policy.transient_error_cooldown_seconds;
                // Only the explicitly transient status set honors Retry-After.
                error
                    .retry_after
                    .filter(|_| matches!(error.status, 408 | 500 | 502..=504 | 520..=526))
                    .filter(|d| !d.is_zero())
                    .unwrap_or(Duration::from_secs(if seconds > 0 { seconds as u64 } else { 60 }))
            }
        };
        let Some(next) = now.checked_add(duration) else {
            return;
        };
        let deadline = prev
            .filter(|s| s.deadline > now)
            .map(|s| s.deadline.max(next))
            .unwrap_or(next);
        self.cooldowns.insert(
            key,
            Cooldown {
                deadline,
                level,
                status: error.status,
                quota: error.status == 429,
            },
        );
        if credential_quota {
            self.extend_siblings(c, deadline, now);
        }
    }

    fn extend_siblings(&mut self, c: &Credential, deadline: Instant, now: Instant) {
        for ((id, _), state) in &mut self.cooldowns {
            if *id == c.id && state.deadline > now {
                state.deadline = state.deadline.max(deadline);
            }
        }
    }

    pub fn reconcile(&mut self, credentials: &[std::sync::Arc<Credential>]) {
        self.cooldowns
            .retain(|(id, _), _| credentials.iter().any(|c| c.id == *id));
        self.bindings
            .retain(|_, (id, _)| credentials.iter().any(|c| c.id == *id));
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
        models
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

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
        assert_eq!(s.pick(&[&c, &b, &a], &selection("m"), &p, now).unwrap().id, "a");
        assert_eq!(s.pick(&[&a, &c], &selection("m"), &p, now).unwrap().id, "c");
        assert_eq!(s.pick(&[&a, &b, &c], &selection("m"), &p, now).unwrap().id, "a");
        assert_eq!(s.pick(&[&a, &b], &selection("other"), &p, now).unwrap().id, "a");
        assert_eq!(s.pick(&[&a, &b], &selection("m(high)"), &p, now).unwrap().id, "b");
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
            .map(|_| s.pick(&[&a, &b, &c], &selection("m"), &p, now).unwrap().id.clone())
            .collect();
        assert_eq!(picks, ["a", "a", "b", "a", "c", "a", "a"]);
        let mut s = Scheduler::default();
        assert_eq!(s.pick(&[&a, &b, &c], &selection("m"), &p, now).unwrap().id, "a");
        assert_eq!(s.pick(&[&b, &c], &selection("m"), &p, now).unwrap().id, "b");
        assert_eq!(s.pick(&[&a, &b, &c], &selection("m"), &p, now).unwrap().id, "a");
        assert_eq!(s.pick(&[&a, &b, &c], &selection("m"), &p, now).unwrap().id, "c");
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
        assert_eq!(s.pick(&[&low], &sel, &p, now).unwrap().id, "a");
        assert_eq!(
            s.pick(&[&low, &high], &sel, &p, now + Duration::from_secs(5))
                .unwrap()
                .id,
            "a"
        );
        assert_eq!(
            s.pick(&[&low, &high], &sel, &p, now + Duration::from_secs(15))
                .unwrap()
                .id,
            "b"
        );
        assert_eq!(
            s.pick(&[&low], &sel, &p, now + Duration::from_secs(16)).unwrap().id,
            "a"
        );
        assert_eq!(s.pick(&[&low, &high], &selection("m"), &p, now).unwrap().id, "b");
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
        assert_eq!(s.pick(&[&a, &b], &selection, &p, now).unwrap().id, "a");
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
        assert!(!s.bindings.is_empty());
        next.session_affinity_ttl = Duration::from_secs(5);
        s.configure(&p, &next);
        assert!(s.rotations.is_empty());
        assert!(s.bindings.is_empty());
        assert_eq!(s.wait(&a, "other", now), Some(Duration::from_secs(1)));
    }

    #[test]
    fn forced_cooling_uses_fallback_and_quota_does_not_reuse_transient_state() {
        let now = Instant::now();
        for status in [401, 404, 429, 503] {
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
            assert_eq!(s.wait(&c, "m", now), Some(Duration::from_secs(60)), "status {status}");
        }
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
        let mut hint = ExecError::local(409, FailureScope::Model, "other status");
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
        let mut p = Policy::default();
        p.oauth_disable_cooling.insert("claude".into(), true);
        assert!(p.cooling_disabled(&cred("oauth", serde_json::json!({}))));
        assert!(!p.cooling_disabled(&cred("key", serde_json::json!({"api_key":"fake"}))));
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

    #[test]
    fn prefixes_aliases_exclusions_and_thinking_suffixes() {
        let mut c = cred(
            "a",
            serde_json::json!({"prefix":"team", "model_aliases":[{"alias":"friendly","name":"claude-sonnet"}],
            "excluded_models":["claude-opus*", "*haiku*"]}),
        );
        let mut p = Policy::default();
        assert_eq!(
            execution_model(&c, "team/friendly(high)", &p),
            Some("claude-sonnet(high)".into())
        );
        assert_eq!(execution_model(&c, "claude-opus-5", &p), None);
        assert_eq!(execution_model(&c, "other/friendly", &p), None);
        p.force_model_prefix = true;
        assert_eq!(execution_model(&c, "friendly", &p), None);
        assert_eq!(execution_model(&c, "team/friendly", &p), Some("claude-sonnet".into()));
        c.metadata
            .insert("excluded_models".into(), serde_json::json!(["claude-sonnet"]));
        assert_eq!(
            execution_model(&c, "team/friendly", &p),
            None,
            "execution model exclusions"
        );
    }
}
