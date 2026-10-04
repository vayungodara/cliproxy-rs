//! Home settings: the runtime-only connection config (Go `config.HomeConfig`, filled from
//! `-home-jwt`, never from YAML) and the two Home-authoritative sections of
//! `config.yaml`, `credentials.concurrency` and `credentials.in-flight` (legacy
//! `credential-concurrency`, `credential-in-flight`).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_yaml_ng::Value;

/// Go `config.HomeConfig`. YAML never sets it: Go tags it `yaml:"-"` on `Config`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HomeConfig {
    pub enabled: bool,
    pub node_id: String,
    pub host: String,
    pub port: u16,
    pub disable_cluster_discovery: bool,
    pub tls: HomeTlsConfig,
}

/// Go `config.HomeTLSConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HomeTlsConfig {
    pub enable: bool,
    pub server_name: String,
    pub insecure_skip_verify: bool,
    pub ca_cert: Option<PathBuf>,
    pub client_cert: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
    /// Verify against the address actually dialed (cluster failover targets).
    pub use_target_server_name: bool,
}

/// Go `NormalizeHomePort`: the CPA listener port from a Home config payload.
pub fn normalize_home_port(port: u16) -> u16 {
    if port == 0 { 8317 } else { port }
}

const SECOND: i64 = 1_000_000_000;
const MILLISECOND: i64 = 1_000_000;
pub const MAX_CREDENTIAL_CONCURRENCY_LIMIT: i64 = 1_000_000;

/// Go `CredentialConcurrencyConfig`. Durations are signed nanoseconds, like Go's
/// `time.Duration`, so negative input survives to validation. Each field remembers
/// whether the YAML named it: only absent fields receive the legacy defaults, an
/// explicit `0` or `null` is rejected by validation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CredentialConcurrency {
    pub lifecycle_config_revision: i64,
    pub observation_barrier_revision: i64,
    pub cpa_heartbeat_timeout: i64,
    pub cpa_cancel_bound: i64,
    pub reclaim_grace: i64,
    pub cleanup_interval: i64,
    pub release_flush_interval: i64,
    pub release_max_backoff: i64,
    pub busy_retry_min: i64,
    pub busy_retry_max: i64,
    pub max_limit: i64,
    pub(crate) present: u16,
}

/// YAML key, presence bit, default (Go `WithDefaults`, 0 means none).
const FIELDS: [(&str, i64); 11] = [
    ("lifecycle-config-revision", 0),
    ("observation-barrier-revision", 0),
    ("cpa-heartbeat-timeout", 3 * SECOND),
    ("cpa-cancel-bound", 5 * SECOND),
    ("reclaim-grace", 5 * SECOND),
    ("cleanup-interval", 5 * SECOND),
    ("release-flush-interval", 250 * MILLISECOND),
    ("release-max-backoff", 2 * SECOND),
    ("busy-retry-min", 250 * MILLISECOND),
    ("busy-retry-max", SECOND),
    ("max-limit", MAX_CREDENTIAL_CONCURRENCY_LIMIT),
];
const DURATION_FIELDS: std::ops::Range<usize> = 2..10;

impl CredentialConcurrency {
    fn slots(&mut self) -> [&mut i64; 11] {
        [
            &mut self.lifecycle_config_revision,
            &mut self.observation_barrier_revision,
            &mut self.cpa_heartbeat_timeout,
            &mut self.cpa_cancel_bound,
            &mut self.reclaim_grace,
            &mut self.cleanup_interval,
            &mut self.release_flush_interval,
            &mut self.release_max_backoff,
            &mut self.busy_retry_min,
            &mut self.busy_retry_max,
            &mut self.max_limit,
        ]
    }

    fn present(&self, field: &str) -> bool {
        FIELDS
            .iter()
            .position(|(name, _)| *name == field)
            .is_some_and(|i| self.present & (1 << i) != 0)
    }

    /// Decodes `credentials.concurrency` from a config document (Go's custom
    /// `UnmarshalYAML`). Durations are Go duration strings; yaml.v3 rejects integers.
    pub fn from_document(document: &Value) -> Result<Self> {
        let mut cfg = Self::default();
        let Some(section) = document.get("credentials").and_then(|c| c.get("concurrency")) else {
            return Ok(cfg);
        };
        let Value::Mapping(map) = section else {
            if section.is_null() {
                return Ok(cfg);
            }
            bail!("credentials.concurrency must be a mapping");
        };
        let mut present = 0;
        for (i, slot) in cfg.slots().into_iter().enumerate() {
            let name = FIELDS[i].0;
            let Some(value) = map.get(name) else {
                continue;
            };
            present |= 1 << i;
            *slot = match value {
                Value::Null => 0,
                Value::String(text) if DURATION_FIELDS.contains(&i) => match cpa_core::config::parse_duration(text) {
                    Some(ns) => ns,
                    None => bail!("credential-concurrency.{name}: invalid duration {text:?}"),
                },
                Value::Number(n) if !DURATION_FIELDS.contains(&i) => match (n.as_i64(), n.as_f64()) {
                    (Some(v), _) => v,
                    (None, Some(f)) if f.is_finite() => f as i64,
                    _ => bail!("credential-concurrency.{name} must be an integer"),
                },
                _ if DURATION_FIELDS.contains(&i) => bail!("credential-concurrency.{name} must be a duration string"),
                _ => bail!("credential-concurrency.{name} must be an integer"),
            };
        }
        cfg.present = present;
        Ok(cfg)
    }

    /// Go `WithDefaults`: absent zero fields get the legacy defaults.
    pub fn with_defaults(mut self) -> Self {
        let present = self.present;
        for (i, slot) in self.slots().into_iter().enumerate() {
            if present & (1 << i) == 0 && *slot == 0 {
                *slot = FIELDS[i].1;
            }
        }
        self
    }

    /// Go `ValidateCredentialConcurrency`.
    pub fn validate(&self) -> Result<()> {
        let c = self;
        if c.lifecycle_config_revision < 0
            || (c.present("lifecycle-config-revision") && c.lifecycle_config_revision == 0)
        {
            bail!("lifecycle configuration revision must be positive when present");
        }
        if c.observation_barrier_revision < 0 {
            bail!("observation barrier revision must not be negative");
        }
        if c.cpa_heartbeat_timeout <= 0 || c.cpa_cancel_bound <= 0 || c.reclaim_grace <= 0 || c.cleanup_interval <= 0 {
            bail!("credential concurrency lifecycle durations must be positive");
        }
        if c.release_flush_interval <= 0 || c.release_max_backoff <= 0 || c.busy_retry_min <= 0 || c.busy_retry_max <= 0
        {
            bail!("credential concurrency limiter durations must be positive");
        }
        if c.release_max_backoff < c.release_flush_interval {
            bail!("credential concurrency release max backoff must not be less than release flush interval");
        }
        if c.busy_retry_min % MILLISECOND != 0 || c.busy_retry_max % MILLISECOND != 0 {
            bail!("credential concurrency busy retry durations must be whole milliseconds");
        }
        if c.busy_retry_max < c.busy_retry_min {
            bail!("credential concurrency busy retry max must not be less than busy retry min");
        }
        if c.max_limit < 1 || c.max_limit > MAX_CREDENTIAL_CONCURRENCY_LIMIT {
            bail!("credential concurrency max limit must be between 1 and {MAX_CREDENTIAL_CONCURRENCY_LIMIT}");
        }
        Ok(())
    }

    /// Go `ValidateCredentialConcurrencyLifecycle`: Home reclaims a lost CPA's leases
    /// only after the CPA must already have cancelled them.
    pub fn validate_lifecycle(&self, node_heartbeat_timeout: i64) -> Result<()> {
        if node_heartbeat_timeout <= 0 {
            bail!("credential concurrency lifecycle durations must be positive");
        }
        self.validate()?;
        let (Some(left), Some(right)) = (
            node_heartbeat_timeout.checked_add(self.reclaim_grace),
            self.cpa_heartbeat_timeout.checked_add(self.cpa_cancel_bound),
        ) else {
            bail!("credential concurrency lifecycle timing safety invariant overflows");
        };
        if left <= right {
            bail!("node heartbeat timeout plus reclaim grace must exceed CPA heartbeat timeout plus cancel bound");
        }
        Ok(())
    }

    pub fn heartbeat_timeout(&self) -> Duration {
        nanos(self.cpa_heartbeat_timeout)
    }

    pub fn cancel_bound(&self) -> Duration {
        nanos(self.cpa_cancel_bound)
    }

    pub fn flush_interval(&self) -> Duration {
        nanos(self.release_flush_interval)
    }

    pub fn max_backoff(&self) -> Duration {
        nanos(self.release_max_backoff)
    }

    pub fn busy_retry(&self) -> (Duration, Duration) {
        (nanos(self.busy_retry_min), nanos(self.busy_retry_max))
    }
}

/// A validated, non-negative Go duration.
pub(crate) fn nanos(ns: i64) -> Duration {
    Duration::from_nanos(ns.max(0) as u64)
}

pub const IN_FLIGHT_MAX_PART_BYTES: i64 = 256 * 1024;
pub const IN_FLIGHT_MAX_PART_COUNT: i64 = 64;
pub const IN_FLIGHT_MAX_REVISION_BYTES: i64 = 16 * 1024 * 1024;
pub const IN_FLIGHT_MAX_AGGREGATE_GROUPS: i64 = 100_000;
pub const IN_FLIGHT_MAX_DETAILS: i64 = 10_000;
pub const IN_FLIGHT_MAX_STRING_BYTES: i64 = 256;

/// Go `CredentialInFlightConfig`. `Config::parse` already ran Go's `Validate`; this
/// only reads the values, with Go's loader defaults for absent or null fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialInFlight {
    pub snapshot_interval: String,
    pub stale_after: String,
    pub max_part_bytes: i64,
    pub max_part_count: i64,
    pub max_revision_bytes: i64,
    pub max_aggregate_groups: i64,
    pub max_details: i64,
    pub max_string_bytes: i64,
    pub staging_retention: String,
}

impl Default for CredentialInFlight {
    fn default() -> Self {
        Self {
            snapshot_interval: "2s".into(),
            stale_after: "10s".into(),
            max_part_bytes: IN_FLIGHT_MAX_PART_BYTES,
            max_part_count: IN_FLIGHT_MAX_PART_COUNT,
            max_revision_bytes: IN_FLIGHT_MAX_REVISION_BYTES,
            max_aggregate_groups: IN_FLIGHT_MAX_AGGREGATE_GROUPS,
            max_details: IN_FLIGHT_MAX_DETAILS,
            max_string_bytes: IN_FLIGHT_MAX_STRING_BYTES,
            staging_retention: "1m".into(),
        }
    }
}

impl CredentialInFlight {
    pub fn from_document(document: &Value) -> Self {
        let mut cfg = Self::default();
        let Some(section) = document.get("credentials").and_then(|c| c.get("in-flight")) else {
            return cfg;
        };
        let get = |key: &str| section.get(key).filter(|v| !v.is_null());
        let text = |key: &str, slot: &mut String| {
            if let Some(v) = get(key) {
                *slot = cpa_core::config::go_string(v);
            }
        };
        text("snapshot-interval", &mut cfg.snapshot_interval);
        text("stale-after", &mut cfg.stale_after);
        text("staging-retention", &mut cfg.staging_retention);
        let int = |key: &str, slot: &mut i64| {
            if let Some(v) = get(key).and_then(Value::as_i64) {
                *slot = v;
            }
        };
        int("max-part-bytes", &mut cfg.max_part_bytes);
        int("max-part-count", &mut cfg.max_part_count);
        int("max-revision-bytes", &mut cfg.max_revision_bytes);
        int("max-aggregate-groups", &mut cfg.max_aggregate_groups);
        int("max-details", &mut cfg.max_details);
        int("max-string-bytes", &mut cfg.max_string_bytes);
        cfg
    }

    /// Go `Durations`: snapshot interval, stale-after and staging retention.
    pub fn durations(&self) -> Result<(Duration, Duration, Duration)> {
        let parse = |s: &str| cpa_core::config::parse_duration(s);
        let Some(snapshot) = parse(&self.snapshot_interval).filter(|d| *d > 0) else {
            bail!("credential-in-flight.snapshot-interval must be positive");
        };
        let Some(stale) = parse(&self.stale_after).filter(|d| *d > 0 && snapshot <= *d / 3) else {
            bail!("credential-in-flight.stale-after must be at least three snapshot intervals");
        };
        let Some(staging) = parse(&self.staging_retention).filter(|d| *d > 0) else {
            bail!("credential-in-flight.staging-retention must be positive");
        };
        Ok((nanos(snapshot), nanos(stale), nanos(staging)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<CredentialConcurrency> {
        let doc: Value = serde_yaml_ng::from_str(yaml).unwrap();
        CredentialConcurrency::from_document(&doc)
    }

    #[test]
    fn defaults_match_go_limiter_config() {
        // Go TestCredentialConcurrencyLimiterConfig.
        let got = CredentialConcurrency::default().with_defaults();
        assert_eq!(
            (got.lifecycle_config_revision, got.observation_barrier_revision),
            (0, 0)
        );
        assert_eq!(got.heartbeat_timeout(), Duration::from_secs(3));
        assert_eq!(got.cancel_bound(), Duration::from_secs(5));
        assert_eq!(got.reclaim_grace, 5 * SECOND);
        assert_eq!(got.cleanup_interval, 5 * SECOND);
        assert_eq!(got.flush_interval(), Duration::from_millis(250));
        assert_eq!(got.max_backoff(), Duration::from_secs(2));
        assert_eq!(got.busy_retry(), (Duration::from_millis(250), Duration::from_secs(1)));
        assert_eq!(got.max_limit, 1_000_000);
        got.validate_lifecycle(20 * SECOND).unwrap();
        assert!(got.validate_lifecycle(2 * SECOND).is_err());
    }

    /// Go `TestValidateCredentialConcurrencyAcceptsHomeAuthoritativeHeartbeat`.
    #[test]
    fn home_authoritative_heartbeat_passes_intrinsic_but_not_lifecycle_validation() {
        let mut cfg = CredentialConcurrency::default().with_defaults();
        cfg.cpa_heartbeat_timeout = 20 * SECOND;
        cfg.validate().unwrap();
        assert!(cfg.validate_lifecycle(20 * SECOND).is_err());
    }

    #[test]
    fn explicit_zero_null_and_negative_values_are_not_defaulted() {
        // Go TestCredentialConcurrencyConfigDefaultsOnlyMissingFields.
        let base = "  cpa-cancel-bound: 5s\n  reclaim-grace: 5s\n  cleanup-interval: 5s\n";
        for head in [
            "  lifecycle-config-revision: 0\n  cpa-heartbeat-timeout: 3s\n",
            "  lifecycle-config-revision: 1\n  cpa-heartbeat-timeout: 0s\n",
            "  lifecycle-config-revision: 1\n  cpa-heartbeat-timeout: null\n",
            "  lifecycle-config-revision: 1\n  observation-barrier-revision: -1\n  cpa-heartbeat-timeout: 3s\n",
        ] {
            let cfg = parse(&format!("credentials:\n concurrency:\n{head}{base}"))
                .unwrap()
                .with_defaults();
            assert!(cfg.validate_lifecycle(20 * SECOND).is_err(), "{head}");
        }
        // Absent fields still default.
        let cfg = parse(&format!(
            "credentials:\n concurrency:\n  lifecycle-config-revision: 4\n{base}"
        ))
        .unwrap()
        .with_defaults();
        assert_eq!(cfg.lifecycle_config_revision, 4);
        assert_eq!(cfg.cpa_heartbeat_timeout, 3 * SECOND);
        cfg.validate_lifecycle(20 * SECOND).unwrap();
    }

    #[test]
    fn invalid_limiter_values_are_rejected() {
        // Go TestCredentialConcurrencyConfigRejectsInvalidLimiter.
        let ms = MILLISECOND;
        for (flush, backoff, min, max, limit) in [
            (SECOND, 500 * ms, ms, ms, 1),
            (ms, ms, 1500 * 1000, 2 * ms, 1),
            (ms, ms, ms, ms, 1_000_001),
        ] {
            let cfg = CredentialConcurrency {
                release_flush_interval: flush,
                release_max_backoff: backoff,
                busy_retry_min: min,
                busy_retry_max: max,
                max_limit: limit,
                cpa_heartbeat_timeout: 3 * SECOND,
                cpa_cancel_bound: 5 * SECOND,
                reclaim_grace: 5 * SECOND,
                cleanup_interval: 5 * SECOND,
                ..Default::default()
            };
            assert!(cfg.validate_lifecycle(20 * SECOND).is_err());
        }
    }

    /// Go `TestValidateCredentialConcurrencyLifecycleRejectsSafetyOverflow`.
    #[test]
    fn lifecycle_sum_overflow_is_rejected() {
        let cfg = CredentialConcurrency {
            lifecycle_config_revision: 1,
            cpa_heartbeat_timeout: i64::MAX,
            cpa_cancel_bound: 1,
            reclaim_grace: SECOND,
            cleanup_interval: SECOND,
            ..Default::default()
        }
        .with_defaults();
        let err = cfg.validate_lifecycle(SECOND).unwrap_err();
        assert!(err.to_string().contains("overflows"), "{err}");
    }

    #[test]
    fn legacy_and_v8_layouts_read_the_same_section() {
        let legacy = cpa_core::config::Config::parse("credential-concurrency:\n  busy-retry-min: 300ms\n").unwrap();
        let v8 = cpa_core::config::Config::parse("credentials:\n  concurrency:\n    busy-retry-min: 300ms\n").unwrap();
        for cfg in [legacy, v8] {
            let parsed = CredentialConcurrency::from_document(&cfg.document)
                .unwrap()
                .with_defaults();
            assert_eq!(parsed.busy_retry_min, 300 * MILLISECOND);
        }
    }

    #[test]
    fn in_flight_reads_defaults_and_overrides() {
        let cfg = cpa_core::config::Config::parse(
            "credential-in-flight:\n  snapshot-interval: 1s\n  max-details: 5\n  max-part-bytes: null\n",
        )
        .unwrap();
        let in_flight = CredentialInFlight::from_document(&cfg.document);
        assert_eq!(in_flight.snapshot_interval, "1s");
        assert_eq!(in_flight.max_details, 5);
        assert_eq!(in_flight.max_part_bytes, IN_FLIGHT_MAX_PART_BYTES);
        let (snapshot, stale, staging) = in_flight.durations().unwrap();
        assert_eq!(
            (snapshot, stale, staging),
            (Duration::from_secs(1), Duration::from_secs(10), Duration::from_secs(60))
        );
    }

    /// Go `TestCredentialConcurrencyLifecycleFixture`: the shared lifecycle fixture
    /// (hot durations given as YAML strings) decodes to the defaults with revision 1 and
    /// validates; with a node heartbeat timeout of 3s, or a zero CPA heartbeat under a
    /// 20s node timeout, the lifecycle invariant fails.
    #[test]
    fn the_lifecycle_fixture_validates_like_go() {
        let fixture = |heartbeat: &str, revision: &str| {
            parse(&format!(
                "credentials:\n concurrency:\n{revision}  cpa-heartbeat-timeout: {heartbeat}\n  cpa-cancel-bound: 5s\n  reclaim-grace: 5s\n  cleanup-interval: 5s\n  release-flush-interval: 250ms\n  release-max-backoff: 2s\n  busy-retry-min: 250ms\n  busy-retry-max: 1s\n  max-limit: 1000000\n"
            ))
            .unwrap()
        };
        let defaults = fixture("3s", "  lifecycle-config-revision: 1\n");
        let mut expected = CredentialConcurrency::default().with_defaults();
        expected.lifecycle_config_revision = 1;
        let fields = |c: &CredentialConcurrency| {
            (
                c.lifecycle_config_revision,
                c.observation_barrier_revision,
                c.cpa_heartbeat_timeout,
                c.cpa_cancel_bound,
                c.reclaim_grace,
                c.cleanup_interval,
                c.release_flush_interval,
                c.release_max_backoff,
                c.busy_retry_min,
                c.busy_retry_max,
                c.max_limit,
            )
        };
        assert_eq!(fields(&defaults), fields(&expected));
        defaults.validate().unwrap();
        assert!(fixture("3s", "").validate_lifecycle(3 * SECOND).is_err());
        let zero = fixture("0s", "");
        assert_eq!(zero.cpa_heartbeat_timeout, 0, "an explicit zero is kept");
        assert!(zero.with_defaults().validate_lifecycle(20 * SECOND).is_err());
    }

    #[test]
    fn home_port_defaults_like_go() {
        // Go TestNormalizeHomePort.
        assert_eq!(normalize_home_port(0), 8317);
        assert_eq!(normalize_home_port(8327), 8327);
        assert_eq!(normalize_home_port(9090), 9090);
    }
}
