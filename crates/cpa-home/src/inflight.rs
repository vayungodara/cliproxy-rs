//! In-flight credential snapshots (Go sdk/cliproxy/auth/home_in_flight_publisher.go).
//!
//! Each tick freezes the registry and publishes the executions as bounded frames: sorted
//! aggregates per (credential, model, accounted) and, space permitting, sorted request
//! details. Frames carry Go's exact JSON bytes, so part sizes and truncation decisions
//! match Go. When the aggregates cannot fit, one `overflow` frame reports the group count.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use crate::client::Client;
use crate::config::{
    CredentialInFlight, IN_FLIGHT_MAX_AGGREGATE_GROUPS, IN_FLIGHT_MAX_DETAILS, IN_FLIGHT_MAX_PART_COUNT,
    IN_FLIGHT_MAX_REVISION_BYTES, IN_FLIGHT_MAX_STRING_BYTES,
};
use crate::dispatch::valid_concurrency_model_key;
use crate::gojson::string_into;
use crate::registry::{Freeze, Observation, Registry};

/// Go `HomeInFlightPublisherConfig`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PublisherConfig {
    pub snapshot_interval: Duration,
    pub max_part_bytes: usize,
    pub max_part_count: usize,
    pub max_revision_bytes: usize,
    pub max_aggregate_groups: usize,
    pub max_details: usize,
    pub max_string_bytes: usize,
}

impl PublisherConfig {
    /// Go `HomeInFlightPublisherConfigFromConfig`.
    pub fn from_config(cfg: &CredentialInFlight) -> anyhow::Result<Self> {
        let (snapshot_interval, _, _) = cfg.durations()?;
        let usize_of = |v: i64| usize::try_from(v).unwrap_or(0);
        let out = Self {
            snapshot_interval,
            max_part_bytes: usize_of(cfg.max_part_bytes),
            max_part_count: usize_of(cfg.max_part_count),
            max_revision_bytes: usize_of(cfg.max_revision_bytes),
            max_aggregate_groups: usize_of(cfg.max_aggregate_groups),
            max_details: usize_of(cfg.max_details),
            max_string_bytes: usize_of(cfg.max_string_bytes),
        };
        if !out.valid() || cfg.max_details < 0 {
            anyhow::bail!("credential-in-flight bounds are invalid");
        }
        Ok(out)
    }

    /// Go `validHomeInFlightPublisherConfig`.
    pub fn valid(&self) -> bool {
        let limit = |v: i64| v as usize;
        !self.snapshot_interval.is_zero()
            && self.max_part_bytes >= 1024
            && self.max_part_count > 0
            && self.max_part_count <= limit(IN_FLIGHT_MAX_PART_COUNT)
            && self.max_revision_bytes >= self.max_part_bytes
            && self.max_revision_bytes <= limit(IN_FLIGHT_MAX_REVISION_BYTES)
            && self.max_aggregate_groups > 0
            && self.max_aggregate_groups <= limit(IN_FLIGHT_MAX_AGGREGATE_GROUPS)
            && self.max_details <= limit(IN_FLIGHT_MAX_DETAILS)
            && self.max_string_bytes > 0
            && self.max_string_bytes <= limit(IN_FLIGHT_MAX_STRING_BYTES)
            && self.max_revision_bytes.div_ceil(self.max_part_bytes) <= self.max_part_count
    }

    /// Go `validHomeInFlightPublisherBounds`.
    fn bounds_valid(&self) -> bool {
        self.max_part_bytes > 0
            && self.max_part_count > 0
            && self.max_revision_bytes >= self.max_part_bytes
            && self.max_aggregate_groups > 0
            && self.max_string_bytes > 0
    }
}

/// Go `time.Time.MarshalJSON` for a UTC time: RFC 3339 with trimmed nanoseconds.
pub(crate) fn rfc3339_nano(t: DateTime<Utc>) -> String {
    let text = t.format("%Y-%m-%dT%H:%M:%S%.9f").to_string();
    let text = text.trim_end_matches('0').trim_end_matches('.');
    format!("{text}Z")
}

fn utc(t: SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(t)
}

/// Go `homeInFlightObservationModel`.
fn observation_model(observation: &Observation) -> String {
    if observation.accounted {
        return observation.model.clone();
    }
    valid_concurrency_model_key(&observation.model).unwrap_or_else(|| "unknown".to_owned())
}

/// Go `homeInFlightTruncateString`: cut to `max` bytes, then drop trailing bytes until
/// the whole string is valid UTF-8.
fn truncate(value: &[u8], max: usize) -> Vec<u8> {
    if max == 0 || value.len() <= max {
        return value.to_vec();
    }
    let mut cut = &value[..max];
    while !cut.is_empty() && std::str::from_utf8(cut).is_err() {
        cut = &cut[..cut.len() - 1];
    }
    cut.to_vec()
}

/// One request detail, already encoded.
struct Detail {
    started_at: DateTime<Utc>,
    request_id: Vec<u8>,
    credential_id: Vec<u8>,
    model: Vec<u8>,
    request_kind: Vec<u8>,
    json: Vec<u8>,
}

fn encode_detail(d: &Detail) -> Vec<u8> {
    let mut out = b"{\"request_id\":".to_vec();
    string_into(&mut out, &d.request_id);
    out.extend_from_slice(b",\"credential_id\":");
    string_into(&mut out, &d.credential_id);
    out.extend_from_slice(b",\"model\":");
    string_into(&mut out, &d.model);
    out.extend_from_slice(b",\"request_kind\":");
    string_into(&mut out, &d.request_kind);
    out.extend_from_slice(b",\"started_at\":\"");
    out.extend_from_slice(rfc3339_nano(d.started_at).as_bytes());
    out.extend_from_slice(b"\"}");
    out
}

/// Go `validHomeInFlightDetail` (strings only: start times are always set and UTC).
fn valid_detail_string(value: &[u8], max: usize) -> bool {
    std::str::from_utf8(value).is_ok_and(|s| !s.trim().is_empty()) && value.len() <= max
}

/// The shared head of every frame of one revision.
struct Head {
    revision: i64,
    observed_at: String,
    barrier: i64,
}

/// A part frame under construction: encoded aggregates and details.
struct Part<'a> {
    aggregates: Vec<&'a [u8]>,
    details: Vec<&'a [u8]>,
}

impl<'a> Part<'a> {
    fn new() -> Self {
        Self {
            aggregates: Vec::new(),
            details: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.aggregates.is_empty() && self.details.is_empty()
    }

    fn head(head: &Head, index: usize, count: usize, truncated: bool) -> Vec<u8> {
        let mut out = format!(
            "{{\"kind\":\"part\",\"revision\":{},\"observed_at\":\"{}\",\"barrier_revision\":{},\"part_index\":{index},\"part_count\":{count}",
            head.revision, head.observed_at, head.barrier
        )
        .into_bytes();
        if truncated {
            out.extend_from_slice(b",\"details_truncated\":true");
        }
        out
    }

    fn list_len(items: &[&[u8]], key_len: usize) -> usize {
        if items.is_empty() {
            return 0;
        }
        key_len + items.iter().map(|i| i.len()).sum::<usize>() + items.len() - 1 + 1
    }

    /// The exact length of `encode`, without building it.
    fn len(&self, head_len: usize) -> usize {
        head_len
            + Self::list_len(&self.aggregates, ",\"aggregates\":[".len())
            + Self::list_len(&self.details, ",\"details\":[".len())
            + 1
    }

    fn encode(&self, head: Vec<u8>) -> Vec<u8> {
        let mut out = head;
        for (key, items) in [("aggregates", &self.aggregates), ("details", &self.details)] {
            if items.is_empty() {
                continue;
            }
            out.extend_from_slice(format!(",\"{key}\":[").as_bytes());
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(item);
            }
            out.push(b']');
        }
        out.push(b'}');
        out
    }
}

fn overflow(head: &Head, groups: usize) -> Vec<Vec<u8>> {
    let mut out = format!(
        "{{\"kind\":\"overflow\",\"revision\":{},\"observed_at\":\"{}\",\"barrier_revision\":{}",
        head.revision, head.observed_at, head.barrier
    );
    if groups > 0 {
        out.push_str(&format!(",\"aggregate_group_count\":{groups}"));
    }
    out.push('}');
    vec![out.into_bytes()]
}

enum Packed<'a> {
    /// Aggregates alone do not fit: publish an overflow frame.
    Overflow,
    /// Frames (indices unassigned) and how many details made it in.
    Frames(Vec<Part<'a>>, usize),
}

/// Go `packHomeInFlightFrames`. Sizes during packing use part index 0 and part count
/// `max_part_count`, as Go's placeholder frames do.
fn pack<'a>(
    head: &Head,
    cfg: &PublisherConfig,
    aggregates: &'a [Vec<u8>],
    details: &'a [Vec<u8>],
    truncated: bool,
) -> Packed<'a> {
    let head_len = Part::head(head, 0, cfg.max_part_count, truncated).len();
    let fits = |part: &Part| part.len(head_len) <= cfg.max_part_bytes;
    let mut frames: Vec<Part<'a>> = Vec::new();
    let mut current = Part::new();
    for aggregate in aggregates {
        current.aggregates.push(aggregate);
        if fits(&current) {
            continue;
        }
        current.aggregates.pop();
        if current.is_empty() || frames.len() >= cfg.max_part_count {
            return Packed::Overflow;
        }
        frames.push(std::mem::replace(&mut current, Part::new()));
        current.aggregates.push(aggregate);
        if !fits(&current) {
            return Packed::Overflow;
        }
    }
    let mut included = 0;
    for detail in details {
        current.details.push(detail);
        if fits(&current) {
            included += 1;
            continue;
        }
        current.details.pop();
        if current.is_empty() {
            return Packed::Frames(frames, included);
        }
        if frames.len() >= cfg.max_part_count {
            return Packed::Frames(frames, included - current.details.len());
        }
        frames.push(std::mem::replace(&mut current, Part::new()));
        current.details.push(detail);
        if !fits(&current) {
            return Packed::Frames(frames, included);
        }
        included += 1;
    }
    if !current.is_empty() || frames.is_empty() {
        if frames.len() >= cfg.max_part_count {
            if !current.aggregates.is_empty() {
                return Packed::Overflow;
            }
            return Packed::Frames(frames, included - current.details.len());
        }
        frames.push(current);
    }
    Packed::Frames(frames, included)
}

/// Go `encodeHomeInFlightFreeze`: the marshalled frames of one snapshot.
pub fn encode(freeze: &Freeze, observed_at: SystemTime, cfg: &PublisherConfig) -> Vec<Vec<u8>> {
    let head = Head {
        revision: freeze.revision,
        observed_at: rfc3339_nano(utc(observed_at)),
        barrier: freeze.barrier_revision,
    };
    let mut counts: BTreeMap<(Vec<u8>, Vec<u8>, bool), i64> = BTreeMap::new();
    let mut keys_valid = true;
    for observation in &freeze.executions {
        let model = observation_model(observation);
        if observation.credential_id.len() > cfg.max_string_bytes || model.len() > cfg.max_string_bytes {
            keys_valid = false;
        }
        // Keyed on `unaccounted` so BTreeMap order is Go's sort: credential, model,
        // then status, where "accounted" < "unaccounted".
        *counts
            .entry((
                observation.credential_id.clone().into_bytes(),
                model.into_bytes(),
                !observation.accounted,
            ))
            .or_default() += 1;
    }
    let aggregates: Vec<Vec<u8>> = counts
        .iter()
        .map(|((credential, model, unaccounted), count)| {
            let accounted = !unaccounted;
            let mut out = b"{\"credential_id\":".to_vec();
            string_into(&mut out, credential);
            out.extend_from_slice(b",\"model\":");
            string_into(&mut out, model);
            out.extend_from_slice(if accounted {
                b",\"status\":\"accounted\""
            } else {
                b",\"status\":\"unaccounted\""
            });
            out.extend_from_slice(format!(",\"count\":{count}}}").as_bytes());
            out
        })
        .collect();
    if !cfg.bounds_valid() || !keys_valid || aggregates.len() > cfg.max_aggregate_groups {
        return overflow(&head, aggregates.len());
    }

    let mut truncated = false;
    let mut details: Vec<Detail> = Vec::with_capacity(freeze.executions.len());
    for observation in &freeze.executions {
        let max = cfg.max_string_bytes;
        let model = observation_model(observation);
        let fields = [
            observation.request_id.as_bytes(),
            observation.credential_id.as_bytes(),
            model.as_bytes(),
            observation.request_kind.as_bytes(),
        ];
        let bounded: Vec<Vec<u8>> = fields.iter().map(|f| truncate(f, max)).collect();
        let changed = bounded.iter().zip(fields.iter()).any(|(b, f)| b.as_slice() != *f);
        if !bounded.iter().all(|f| valid_detail_string(f, max)) {
            truncated = true;
            continue;
        }
        truncated |= changed;
        let mut fields = bounded.into_iter();
        let mut detail = Detail {
            started_at: utc(observation.started_at),
            request_id: fields.next().unwrap_or_default(),
            credential_id: fields.next().unwrap_or_default(),
            model: fields.next().unwrap_or_default(),
            request_kind: fields.next().unwrap_or_default(),
            json: Vec::new(),
        };
        detail.json = encode_detail(&detail);
        details.push(detail);
    }
    details.sort_by(|a, b| {
        (a.started_at, &a.request_id, &a.credential_id, &a.model, &a.request_kind).cmp(&(
            b.started_at,
            &b.request_id,
            &b.credential_id,
            &b.model,
            &b.request_kind,
        ))
    });
    if details.len() > cfg.max_details {
        details.truncate(cfg.max_details);
        truncated = true;
    }
    let mut detail_json: Vec<Vec<u8>> = details.into_iter().map(|d| d.json).collect();

    loop {
        let (frames, included) = match pack(&head, cfg, &aggregates, &detail_json, truncated) {
            Packed::Overflow => return overflow(&head, aggregates.len()),
            Packed::Frames(frames, included) => (frames, included),
        };
        if included < detail_json.len() {
            detail_json.truncate(included);
            truncated = true;
            continue;
        }
        let count = frames.len();
        let encoded: Vec<Vec<u8>> = frames
            .iter()
            .enumerate()
            .map(|(index, part)| part.encode(Part::head(&head, index, count, truncated)))
            .collect();
        // Go `homeInFlightFramesWithinBounds`, with the real part indices.
        let within = !encoded.is_empty()
            && encoded.len() <= cfg.max_part_count
            && encoded.iter().all(|f| f.len() <= cfg.max_part_bytes)
            && encoded.iter().map(Vec::len).sum::<usize>() <= cfg.max_revision_bytes;
        if within {
            return encoded;
        }
        if detail_json.is_empty() {
            return overflow(&head, aggregates.len());
        }
        detail_json.pop();
        truncated = true;
    }
}

/// The publisher config Home last sent (Go `Manager.homeInFlightPublisherConfig`).
#[derive(Clone, Default)]
pub struct PublisherSettings(Arc<RwLock<Option<PublisherConfig>>>);

impl PublisherSettings {
    /// Go `ApplyHomeInFlightPublisherConfig`: invalid settings are ignored.
    pub fn apply(&self, cfg: PublisherConfig) {
        if cfg.valid() {
            *self.0.write().unwrap_or_else(PoisonError::into_inner) = Some(cfg);
        }
    }

    pub fn get(&self) -> Option<PublisherConfig> {
        *self.0.read().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Go `StartHomeInFlightPublisher`: one snapshot per interval while the heartbeat holds.
pub async fn run_publisher(
    client: Client,
    registry: Registry,
    settings: PublisherSettings,
    shutdown: CancellationToken,
) {
    let mut next = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep_until(next) => {}
        }
        let observed_at = SystemTime::now();
        // Go reads a zero config before Home sent one: its bounds are invalid, so the
        // snapshot is a single overflow frame.
        let cfg = settings.get().unwrap_or_default();
        let interval = if cfg.snapshot_interval.is_zero() {
            Duration::from_secs(2)
        } else {
            cfg.snapshot_interval
        };
        next = tokio::time::Instant::now() + interval;
        if !client.heartbeat_ok() {
            continue;
        }
        let freeze = registry.freeze();
        for frame in encode(&freeze, observed_at, &cfg) {
            // Go passes the publisher context to every push: a cancelled one fails.
            if shutdown.is_cancelled() || client.lpush_in_flight_snapshot(&frame).await.is_err() {
                tracing::warn!("failed to publish in-flight snapshot frame");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value as Json;

    fn time(nanos: i64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_nanos(nanos as u64)
    }

    /// Go's frames byte for byte (go_auth_golden.json), part and overflow alike: the
    /// field order, tags and required keys Go `TestCredentialInFlightWireContractFixture`
    /// pins. (`TestCredentialInFlightWireContractRejectsInvalidJSON` checks Home's
    /// decoder; a node only writes these frames.) The golden also runs the inputs of
    /// Go's encoder tests, named after them:
    /// `TestEncodeHomeInFlightFreezePreservesPartitionsAndBarrier`,
    /// `…UsesOverflowWithoutPartialAggregates`, `…UsesDeterministicBoundedMultipartFrames`,
    /// `…OverflowsWhenFinalAggregatePartExceedsPartCount`,
    /// `…TruncatesDetailsBeforeTotalOverflow`, `…BoundsStringsAndExcludesSensitiveFields`,
    /// `…OverflowsForRawAggregateKey`, `…KeepsRawAggregateGroupsDistinct`,
    /// `…DropsInvalidDetailsWithoutDiscardingAggregates`,
    /// `…CanonicalizesUnaccountedModelsWithFallback` and
    /// `…SetsGlobalDetailTruncationMetadata` (their zero start times become the epoch:
    /// a Rust scope always records its start).
    #[test]
    fn frames_match_go_bytes() {
        let doc: Json = serde_json::from_str(include_str!("../tests/fixtures/go_auth_golden.json")).unwrap();
        for case in doc["inflight"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let c = &case["cfg"];
            let n = |k: &str| c[k].as_u64().unwrap() as usize;
            let cfg = PublisherConfig {
                snapshot_interval: Duration::from_secs(2),
                max_part_bytes: n("max_part_bytes"),
                max_part_count: n("max_part_count"),
                max_revision_bytes: n("max_revision_bytes"),
                max_aggregate_groups: n("max_aggregate_groups"),
                max_details: n("max_details"),
                max_string_bytes: n("max_string_bytes"),
            };
            let executions = case["observations"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .map(|o| Observation {
                            request_id: o["request_id"].as_str().unwrap().into(),
                            credential_id: o["credential_id"].as_str().unwrap().into(),
                            model: o["model"].as_str().unwrap().into(),
                            request_kind: o["request_kind"].as_str().unwrap().into(),
                            started_at: time(o["started_unix_nano"].as_i64().unwrap()),
                            accounted: o["accounted"].as_bool().unwrap(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let freeze = Freeze {
                revision: case["revision"].as_i64().unwrap(),
                barrier_revision: case["barrier"].as_i64().unwrap(),
                executions,
            };
            let got: Vec<String> = encode(&freeze, time(case["observed_unix_nano"].as_i64().unwrap()), &cfg)
                .into_iter()
                .map(|f| String::from_utf8(f).unwrap())
                .collect();
            let want: Vec<String> = serde_json::from_value(case["frames"].clone()).unwrap();
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn fixture_frames_decode_into_the_contract() {
        // internal/home/testdata/credential_in_flight_contract.json: the part frame's
        // aggregates and details re-encode byte for byte through this module's writers.
        let doc: Json =
            serde_json::from_str(include_str!("../tests/fixtures/credential_in_flight_contract.json")).unwrap();
        let part = &doc["part"];
        let observed = DateTime::parse_from_rfc3339(part["observed_at"].as_str().unwrap()).unwrap();
        assert_eq!(rfc3339_nano(observed.with_timezone(&Utc)), "2026-07-21T12:00:00Z");
        let cfg = PublisherConfig::from_config(&CredentialInFlight::default()).unwrap();
        let freeze = Freeze {
            revision: 7,
            barrier_revision: 11,
            executions: vec![
                Observation {
                    request_id: "req-1".into(),
                    credential_id: "cred-a".into(),
                    model: "gpt-5".into(),
                    request_kind: "sse".into(),
                    started_at: SystemTime::from(DateTime::parse_from_rfc3339("2026-07-21T11:59:58Z").unwrap()),
                    accounted: true,
                },
                Observation {
                    request_id: "req-2".into(),
                    credential_id: "cred-a".into(),
                    model: "gpt-5".into(),
                    request_kind: "sse".into(),
                    started_at: SystemTime::from(DateTime::parse_from_rfc3339("2026-07-21T11:59:59Z").unwrap()),
                    accounted: true,
                },
            ],
        };
        let frames = encode(&freeze, SystemTime::from(observed), &cfg);
        let frame: Json = serde_json::from_slice(&frames[0]).unwrap();
        assert_eq!(frame["kind"], "part");
        assert_eq!(
            frame["aggregates"][0],
            part["aggregates"][0]
                .clone()
                .as_object()
                .map(|a| {
                    let mut a = a.clone();
                    a.insert("count".into(), 2.into());
                    Json::Object(a)
                })
                .unwrap()
        );
        assert_eq!(frame["details"][0], part["details"][0]);
    }

    #[test]
    fn rfc3339_nano_trims_like_go() {
        let t = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        assert_eq!(
            rfc3339_nano(t("2026-07-21T12:00:00.120000000Z")),
            "2026-07-21T12:00:00.12Z"
        );
        assert_eq!(
            rfc3339_nano(t("2026-07-21T12:00:00.000000001Z")),
            "2026-07-21T12:00:00.000000001Z"
        );
        assert_eq!(rfc3339_nano(t("2026-07-21T12:00:00Z")), "2026-07-21T12:00:00Z");
    }

    /// Go `TestHomeInFlightPublisherConfigFromConfigValidatesAndUpdates`.
    #[test]
    fn publisher_config_from_defaults_is_valid() {
        let fast = CredentialInFlight {
            snapshot_interval: "25ms".into(),
            ..CredentialInFlight::default()
        };
        let fast = PublisherConfig::from_config(&fast).unwrap();
        assert_eq!(fast.snapshot_interval, Duration::from_millis(25));
        let cfg = PublisherConfig::from_config(&CredentialInFlight::default()).unwrap();
        assert!(cfg.valid());
        assert_eq!(cfg.snapshot_interval, Duration::from_secs(2));
        let settings = PublisherSettings::default();
        settings.apply(PublisherConfig {
            max_part_bytes: 10,
            ..cfg
        });
        assert_eq!(settings.get(), None);
        settings.apply(cfg);
        assert_eq!(settings.get(), Some(cfg));
    }

    /// A publisher on a fake Home, recording when each snapshot frame arrived.
    struct Published {
        home: crate::fake::FakeHome,
        client: Client,
        arrivals: Arc<std::sync::Mutex<Vec<(tokio::time::Instant, Json)>>>,
    }

    impl Published {
        async fn start(heartbeat: bool) -> Self {
            let arrivals: Arc<std::sync::Mutex<Vec<(tokio::time::Instant, Json)>>> = Arc::default();
            let seen = arrivals.clone();
            let home = crate::fake::FakeHome::start(move |args| {
                if args[0].eq_ignore_ascii_case("lpush") && args[1] == "in-flight-snapshot" {
                    let frame = serde_json::from_str(&args[2]).unwrap();
                    seen.lock().unwrap().push((tokio::time::Instant::now(), frame));
                }
                crate::fake::raw(":1\r\n")
            })
            .await;
            let client = Client::new(home.config());
            crate::fake::set_heartbeat(&client, heartbeat);
            Self { home, client, arrivals }
        }

        fn arrivals(&self) -> Vec<(tokio::time::Instant, Json)> {
            self.arrivals.lock().unwrap().clone()
        }

        async fn wait_for(&self, count: usize) -> Vec<(tokio::time::Instant, Json)> {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let arrivals = self.arrivals();
                    if arrivals.len() >= count {
                        return arrivals;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{count} snapshots within 2s"))
        }
    }

    /// Go `homeInFlightPublisherTestConfig`.
    fn test_settings(interval: Duration) -> PublisherSettings {
        let settings = PublisherSettings::default();
        settings.apply(PublisherConfig {
            snapshot_interval: interval,
            max_part_bytes: 1024,
            max_part_count: 2,
            max_revision_bytes: 2048,
            max_aggregate_groups: 2,
            max_details: 1,
            max_string_bytes: 32,
        });
        assert!(settings.get().is_some());
        settings
    }

    /// Go `TestHomeInFlightPublisherPinsLifetimeRegistry` and
    /// `TestHomeInFlightPublisherReplacementStopsOldLifetimeAndPinsDependencies`: a
    /// publisher snapshots at once from its own lifetime's registry, and once stopped
    /// sends nothing while its replacement publishes the new registry.
    #[tokio::test]
    async fn a_publisher_pins_its_lifetime_and_stops_when_replaced() {
        let settings = test_settings(Duration::from_millis(10));
        let old = Published::start(true).await;
        let old_registry = Registry::new();
        old_registry.observe_barrier(11);
        let stop_old = CancellationToken::new();
        let old_task = tokio::spawn(run_publisher(
            old.client.clone(),
            old_registry,
            settings.clone(),
            stop_old.clone(),
        ));
        assert_eq!(old.wait_for(1).await[0].1["barrier_revision"], 11);
        stop_old.cancel();
        tokio::time::timeout(Duration::from_secs(1), old_task)
            .await
            .unwrap()
            .unwrap();
        let sent_by_old = old.arrivals().len();

        let new = Published::start(true).await;
        let new_registry = Registry::new();
        new_registry.observe_barrier(22);
        let stop_new = CancellationToken::new();
        tokio::spawn(run_publisher(
            new.client.clone(),
            new_registry.clone(),
            settings,
            stop_new.clone(),
        ));
        assert_eq!(new.wait_for(1).await[0].1["barrier_revision"], 22);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(old.arrivals().len(), sent_by_old, "the replaced publisher stopped");
        assert_eq!(new_registry.freeze().barrier_revision, 22);
        stop_new.cancel();
        drop(old.home);
    }

    /// Go `TestHomeInFlightPublisherSkipsFreezeAndPublishWithoutHeartbeat` and
    /// `TestHomeInFlightPublisherCancellationExits`: without a heartbeat nothing is
    /// frozen or sent, and cancelling ends the publisher at once.
    #[tokio::test]
    async fn without_a_heartbeat_nothing_is_frozen_or_sent() {
        for interval in [Duration::from_millis(10), Duration::from_secs(3600)] {
            let published = Published::start(false).await;
            let registry = Registry::new();
            let stop = CancellationToken::new();
            let task = tokio::spawn(run_publisher(
                published.client.clone(),
                registry.clone(),
                test_settings(interval),
                stop.clone(),
            ));
            tokio::time::sleep(Duration::from_millis(30)).await;
            stop.cancel();
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap();
            assert!(published.arrivals().is_empty());
            assert_eq!(registry.freeze().revision, 1, "the publisher never froze the registry");
        }
    }

    /// Go `TestHomeInFlightPublisherAppliesConfigUpdateAtNextTimerCycle`: a new interval
    /// takes effect after the pending timer fires, not before. Paused time: the timers
    /// and arrival stamps share one controlled clock, so scheduler delay cannot skew them.
    #[tokio::test(start_paused = true)]
    async fn a_new_interval_applies_at_the_next_timer_cycle() {
        let settings = test_settings(Duration::from_millis(60));
        let published = Published::start(true).await;
        let stop = CancellationToken::new();
        tokio::spawn(run_publisher(
            published.client.clone(),
            Registry::new(),
            settings.clone(),
            stop.clone(),
        ));
        published.wait_for(1).await;
        let updated = tokio::time::Instant::now();
        settings.apply(PublisherConfig {
            snapshot_interval: Duration::from_millis(10),
            ..settings.get().unwrap()
        });
        let arrivals = published.wait_for(3).await;
        assert!(
            arrivals[1].0.duration_since(updated) >= Duration::from_millis(30),
            "the hot interval waited for the pending timer"
        );
        assert!(
            arrivals[2].0.duration_since(arrivals[1].0) <= Duration::from_millis(35),
            "then the new interval applies"
        );
        stop.cancel();
    }
}
