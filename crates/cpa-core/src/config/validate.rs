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

/// `CodexLiveMediaRelayConfig.Validate`; only an enabled relay is checked.
fn live_media_relay(section: Option<&Value>) -> Result<()> {
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
        if max - min + 1 < sessions * 2 {
            bail!(
                "codex.live-media-relay UDP range requires at least {} ports for {sessions} sessions",
                sessions * 2
            );
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
pub(crate) fn parse_duration(s: &str) -> Option<i64> {
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
    let mut total: u128 = 0;
    while !rest.is_empty() {
        let int_len = rest.bytes().take_while(u8::is_ascii_digit).count();
        let (int_part, after) = rest.split_at(int_len);
        let (frac_part, after) = match after.strip_prefix('.') {
            Some(f) => {
                let n = f.bytes().take_while(u8::is_ascii_digit).count();
                f.split_at(n)
            }
            None => ("", after),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return None;
        }
        let unit_len = after.bytes().take_while(|b| *b != b'.' && !b.is_ascii_digit()).count();
        let (unit, after) = after.split_at(unit_len);
        let scale: u128 = match unit {
            "ns" => 1,
            "us" | "\u{b5}s" | "\u{3bc}s" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            _ => return None,
        };
        let whole: u128 = if int_part.is_empty() { 0 } else { int_part.parse().ok()? };
        total = total.checked_add(whole.checked_mul(scale)?)?;
        if !frac_part.is_empty() {
            // Go scales the fraction in float64.
            let digits = &frac_part[..frac_part.len().min(18)];
            let frac: f64 = format!("0.{digits}").parse().ok()?;
            total += (frac * scale as f64) as u128;
        }
        if total > 1 << 63 {
            return None;
        }
        rest = after;
    }
    if neg {
        return Some(-(total as i128) as i64);
    }
    i64::try_from(total).ok()
}

#[cfg(test)]
mod tests {
    use super::parse_duration;

    #[test]
    fn durations_parse_like_go() {
        assert_eq!(parse_duration("1h1m0.5s"), Some(3_660_500_000_000));
        assert_eq!(parse_duration(".5us"), Some(500));
        assert_eq!(parse_duration("-1.5h"), Some(-5_400_000_000_000));
        assert_eq!(parse_duration("0"), Some(0));
        for bad in ["", "2", "1d", ".s", "1h-1m", "9999999999h"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
    }
}
