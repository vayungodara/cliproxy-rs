//! Go's custom checks in `ParseConfigBytes` beyond YAML shape: credential in-flight
//! bounds (internal/config/credential_in_flight.go) and the Codex Live media relay
//! (internal/config/codex_live.go). Messages are Go's.

use anyhow::{Result, bail};
use serde_yaml_ng::Value;

use super::go_url;

/// Runs both checks on the canonical v8 document.
pub(super) fn go_custom(doc: &Value) -> Result<()> {
    let at = |path: &[&str]| path.iter().try_fold(doc, |node, part| node.get(*part));
    in_flight(at(&["credentials", "in-flight"]))?;
    live_media_relay(at(&["oauth", "providers", "codex", "live-media-relay"]))
}

/// Present non-null value, else Go's pre-decode default.
fn int(section: Option<&Value>, key: &str, default: i64) -> i64 {
    section
        .and_then(|s| s.get(key))
        .filter(|v| !v.is_null())
        .and_then(Value::as_i64)
        .unwrap_or(default)
}

fn text(section: Option<&Value>, key: &str, default: &str) -> String {
    match section.and_then(|s| s.get(key)).filter(|v| !v.is_null()) {
        Some(v) => super::go_string(v),
        None => default.to_owned(),
    }
}

/// `CredentialInFlightConfig.Validate`.
fn in_flight(section: Option<&Value>) -> Result<()> {
    const MAX_PART_COUNT: i64 = 64;
    const MAX_REVISION_BYTES: i64 = 16 * 1024 * 1024;
    const MAX_AGGREGATE_GROUPS: i64 = 100_000;
    const MAX_DETAILS: i64 = 10_000;
    const MAX_STRING_BYTES: i64 = 256;
    let duration = |key: &str, default: &str| parse_duration(&text(section, key, default));
    let snapshot = duration("snapshot-interval", "2s").filter(|d| *d > 0);
    let Some(snapshot) = snapshot else {
        bail!("credential-in-flight.snapshot-interval must be positive");
    };
    if !duration("stale-after", "10s").is_some_and(|stale| stale > 0 && snapshot <= stale / 3) {
        bail!("credential-in-flight.stale-after must be at least three snapshot intervals");
    }
    if !duration("staging-retention", "1m").is_some_and(|d| d > 0) {
        bail!("credential-in-flight.staging-retention must be positive");
    }
    let part_bytes = int(section, "max-part-bytes", 256 * 1024);
    let part_count = int(section, "max-part-count", MAX_PART_COUNT);
    if part_bytes < 1024 || part_count <= 0 || part_count > MAX_PART_COUNT {
        bail!("credential-in-flight part bounds are invalid");
    }
    let revision = int(section, "max-revision-bytes", MAX_REVISION_BYTES);
    if revision < part_bytes || revision > MAX_REVISION_BYTES {
        bail!("credential-in-flight.max-revision-bytes is outside hard bounds");
    }
    if (revision + part_bytes - 1) / part_bytes > part_count {
        bail!("credential-in-flight.max-revision-bytes exceeds part capacity");
    }
    let groups = int(section, "max-aggregate-groups", MAX_AGGREGATE_GROUPS);
    if groups <= 0 || groups > MAX_AGGREGATE_GROUPS {
        bail!("credential-in-flight.max-aggregate-groups is invalid");
    }
    let details = int(section, "max-details", MAX_DETAILS);
    if !(0..=MAX_DETAILS).contains(&details) {
        bail!("credential-in-flight.max-details is invalid");
    }
    let strings = int(section, "max-string-bytes", MAX_STRING_BYTES);
    if strings <= 0 || strings > MAX_STRING_BYTES {
        bail!("credential-in-flight.max-string-bytes is invalid");
    }
    Ok(())
}

/// `CodexLiveMediaRelayConfig.Validate`; only an enabled relay is checked. The UDP
/// ports are Go `uint16` fields, so decoding rejects out-of-range values first.
fn live_media_relay(section: Option<&Value>) -> Result<()> {
    for key in ["udp-port-min", "udp-port-max"] {
        if !(0..=65_535).contains(&int(section, key, 0)) {
            bail!("codex.live-media-relay.{key} must be between 0 and 65535");
        }
    }
    if !section
        .and_then(|s| s.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(());
    }
    let max_sessions = int(section, "max-sessions", 0);
    if max_sessions < 0 {
        bail!("codex.live-media-relay.max-sessions must not be negative");
    }
    let public_ip = text(section, "public-ip", "");
    let public_ip = public_ip.trim();
    if !public_ip.is_empty() && public_ip.parse::<std::net::IpAddr>().is_err() {
        bail!("codex.live-media-relay.public-ip is invalid: {public_ip:?}");
    }
    let (min, max) = (int(section, "udp-port-min", 0), int(section, "udp-port-max", 0));
    if (min == 0) != (max == 0) {
        bail!("codex.live-media-relay UDP port minimum and maximum must both be set");
    }
    if min > max {
        bail!("codex.live-media-relay.udp-port-min must not exceed udp-port-max");
    }
    if min != 0 {
        let sessions = if max_sessions > 0 { max_sessions } else { 32 };
        // Go int arithmetic wraps.
        let required = sessions.wrapping_mul(2);
        if max - min + 1 < required {
            bail!("codex.live-media-relay UDP range requires at least {required} ports for {sessions} sessions");
        }
    }
    let servers = section
        .and_then(|s| s.get("ice-servers"))
        .and_then(Value::as_sequence)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for (i, server) in servers.iter().enumerate() {
        let urls = server
            .get("urls")
            .and_then(Value::as_sequence)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if urls.is_empty() {
            bail!("codex.live-media-relay.ice-servers[{i}].urls is required");
        }
        for raw in urls {
            let parsed = go_url::parse(super::go_string(raw).trim()).filter(|u| !u.scheme.is_empty());
            let Some(parsed) = parsed else {
                bail!("codex.live-media-relay.ice-servers[{i}] contains an invalid URL");
            };
            if !matches!(parsed.scheme.as_str(), "stun" | "stuns" | "turn" | "turns") {
                bail!(
                    "codex.live-media-relay.ice-servers[{i}] uses unsupported scheme {:?}",
                    parsed.scheme
                );
            }
        }
    }
    Ok(())
}

/// Go `time.ParseDuration` in nanoseconds; `None` where Go returns an error.
pub fn parse_duration(s: &str) -> Option<i64> {
    const LIMIT: u64 = 1 << 63;
    let (neg, mut rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    if rest == "0" {
        return Some(0);
    }
    if rest.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    while !rest.is_empty() {
        if !rest.starts_with(|c: char| c == '.' || c.is_ascii_digit()) {
            return None;
        }
        // Go `leadingInt`: overflow past 1<<63 is an error.
        let int_len = rest.bytes().take_while(u8::is_ascii_digit).count();
        let mut whole: u64 = 0;
        for d in rest[..int_len].bytes() {
            if whole > LIMIT / 10 {
                return None;
            }
            whole = whole * 10 + u64::from(d - b'0');
            if whole > LIMIT {
                return None;
            }
        }
        rest = &rest[int_len..];
        // Go `leadingFraction`: digits past the overflow point are consumed, ignored.
        let (mut frac, mut scale, mut has_frac) = (0u64, 1f64, false);
        if let Some(after) = rest.strip_prefix('.') {
            let n = after.bytes().take_while(u8::is_ascii_digit).count();
            has_frac = n > 0;
            let mut overflow = false;
            for d in after[..n].bytes() {
                if overflow || frac > (LIMIT - 1) / 10 {
                    overflow = true;
                    continue;
                }
                let next = frac * 10 + u64::from(d - b'0');
                if next > LIMIT {
                    overflow = true;
                    continue;
                }
                frac = next;
                scale *= 10.0;
            }
            rest = &after[n..];
        }
        if int_len == 0 && !has_frac {
            return None;
        }
        let unit_len = rest.bytes().take_while(|b| *b != b'.' && !b.is_ascii_digit()).count();
        let (unit, after) = rest.split_at(unit_len);
        let unit: u64 = match unit {
            "ns" => 1,
            "us" | "\u{b5}s" | "\u{3bc}s" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            _ => return None,
        };
        if whole > LIMIT / unit {
            return None;
        }
        let mut value = whole * unit;
        if frac > 0 {
            value += (frac as f64 * (unit as f64 / scale)) as u64;
            if value > LIMIT {
                return None;
            }
        }
        total = total.checked_add(value).filter(|t| *t <= LIMIT)?;
        rest = after;
    }
    if neg {
        return Some((total as i64).wrapping_neg());
    }
    i64::try_from(total).ok()
}

#[cfg(test)]
mod tests {
    use super::{Value, in_flight, parse_duration};

    /// Go `TestCredentialInFlightConfigDurationBounds` and
    /// `TestCredentialInFlightConfigRejectsUnsafeBounds`.
    #[test]
    fn in_flight_bounds_follow_go() {
        let valid = |yaml: &str| in_flight(Some(&serde_yaml_ng::from_str::<Value>(yaml).unwrap())).is_ok();
        assert!(valid(""), "the defaults");
        assert!(
            valid("snapshot-interval: 1s\nstale-after: 3s\n"),
            "exactly three intervals"
        );
        assert!(!valid("snapshot-interval: 1s\nstale-after: 2999999999ns\n"));
        // time.Duration(math.MaxInt64 / 2).String() and time.Duration(math.MaxInt64).String().
        assert!(!valid(
            "snapshot-interval: 1281023h53m38.427387903s\nstale-after: 2562047h47m16.854775807s\n"
        ));
        assert!(!valid("stale-after: 5s\n"), "below three default intervals");
        assert!(!valid(&format!("max-revision-bytes: {}\n", 16 * 1024 * 1024 + 1)));
        assert!(!valid(&format!("max-part-bytes: {}\n", i64::MAX)));
    }

    /// Values that overflowed before: Go rejects out-of-range uint16 ports at decode
    /// and wraps `max-sessions * 2` (here to i64::MIN, so the range check passes).
    #[test]
    fn relay_arithmetic_never_overflows() {
        use crate::config::Config;
        let relay = |body: &str| {
            Config::parse(&format!(
                "oauth: {{providers: {{codex: {{live-media-relay: {body}}}}}}}\n"
            ))
        };
        assert!(relay("{enabled: true, udp-port-min: -1, udp-port-max: 9223372036854775807}").is_err());
        assert!(relay("{enabled: false, udp-port-min: -64, udp-port-max: -1}").is_err());
        assert!(relay("{enabled: true, udp-port-min: 1, udp-port-max: 64, max-sessions: 4611686018427387904}").is_ok());
        assert!(relay("{enabled: true, udp-port-min: 1, udp-port-max: 64, max-sessions: 33}").is_err());
    }

    #[test]
    fn durations_parse_like_go() {
        assert_eq!(parse_duration("1h1m0.5s"), Some(3_660_500_000_000));
        assert_eq!(parse_duration(".5us"), Some(500));
        assert_eq!(parse_duration("-1.5h"), Some(-5_400_000_000_000));
        assert_eq!(parse_duration("0"), Some(0));
        assert_eq!(parse_duration("-2562047h47m16.854775808s"), Some(i64::MIN));
        assert_eq!(parse_duration("9223372036.854775807s"), Some(i64::MAX));
        for bad in [
            "",
            "2",
            "1d",
            ".s",
            "1h-1m",
            "9999999999h",
            "9223372036.854775808s",
            "340282366920938463463374607431768211455.999999999999999999ns1s",
            "18446744073709551615ns",
        ] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
    }
}
