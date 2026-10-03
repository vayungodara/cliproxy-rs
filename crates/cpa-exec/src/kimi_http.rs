//! Helpers shared by the Kimi, Meta and Devin executors: Go runtime facts, per-credential
//! custom headers and Go's credential expiry rules. Clients and Go net/http wire
//! behaviour live in crate::proxy.
//!
//! ponytail: provider-neutral (M4-0030 custom headers, M4-0027 refresh timing); hoist
//! when the shared executor helpers land. TLS is wreq's default profile, not Go
//! crypto/tls; these upstreams are not known to fingerprint ClientHello.

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, FailureScope};
use serde_json::Value;

/// Go `runtime.GOOS`.
pub(crate) fn go_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

/// Go `runtime.GOARCH`.
pub(crate) fn go_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "powerpc64" => "ppc64",
        other => other,
    }
}

/// Go `os.Hostname()`, or `None` when it fails.
pub(crate) fn hostname() -> Option<String> {
    #[cfg(target_os = "linux")]
    if let Ok(name) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let name = name.trim_end_matches('\n');
        if !name.is_empty() {
            return Some(name.to_owned());
        }
    }
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes into the provided buffer.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// Go `ApplyPayloadConfigWithRequest` (no target executor) for the device providers:
/// `requests.payload` rules for `model`, `protocol` the target format, `original` the
/// client's original request translated to that target.
// ponytail: the rules are parsed per request (the runtime has no per-snapshot slot), and
// ExecRequest has no inbound route path, so path-gated image-generation rules see "".
pub(crate) fn payload_rules(
    cfg: &cpa_core::config::Config,
    req: &cpa_core::exec::ExecRequest,
    model: &str,
    protocol: &str,
    body: Vec<u8>,
    original: &[u8],
) -> Vec<u8> {
    use cpa_common::payload;
    let rules = payload::Rules::from_config(cfg);
    // PayloadRequestedModel: the client's model, else req.Model.
    let requested = match req.requested_model.trim() {
        "" => req.model.trim(),
        requested => requested,
    };
    payload::apply(
        &rules,
        &payload::Request {
            target_executor: "",
            model,
            requested_model: requested,
            protocol,
            from_protocol: req.source_format.as_str(),
            root: "",
            original,
            request_path: &req.request_path,
            headers: Some(&req.headers),
        },
        body,
    )
}

/// `util.ApplyCustomHeadersFromAttrs` through cpa_common::headers, with Go's
/// `$CPA-SESSION-ID`: the canonical session bound to the request (`req.session`).
pub(crate) fn credential_headers(
    credential: &Credential,
    req: &cpa_core::exec::ExecRequest,
    _original: &[u8],
) -> Vec<(String, String)> {
    let session = cpa_common::session::cpa_session_id(req.session.as_deref());
    cpa_common::headers::custom_headers(&credential.attributes, &req.headers, session.as_deref())
}

/// Version reported where Go sends `buildinfo.Version`.
pub(crate) const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `statusErr{code, msg: body}`. Go's Kimi errors carry no retry hint (Retry-After is not
/// read) and are not credential-scoped, so a 429 cools only the model: the scheduler
/// reserves credential-wide quota cooldowns for credential-scoped 429s.
pub(crate) async fn status_error(upstream: crate::proxy::Upstream) -> ExecError {
    let body = crate::proxy::read_all(upstream.body, crate::proxy::MAX_ERROR_BODY, true)
        .await
        .unwrap_or_default();
    let scope = match upstream.status {
        429 => FailureScope::Model,
        401 | 402 | 403 | 408 | 500.. => FailureScope::Credential,
        _ => FailureScope::Request,
    };
    ExecError {
        status: upstream.status,
        scope,
        body,
        headers: Box::new(upstream.headers),
        retry_after: None,
        direct: false,
    }
}

/// Go `io.ReadAll` for a body whose read failure must surface: the first `keep` bytes are
/// kept and the rest is drained, so a read error anywhere in the body is returned.
// ponytail: Go keeps the whole body; bytes past `keep` are discarded to bound memory.
// Provider-neutral; hoist into crate::proxy (owner: Claude thread) if others need it.
pub(crate) async fn read_all_strict(
    mut body: futures_util::stream::BoxStream<'static, Result<bytes::Bytes, ExecError>>,
    keep: usize,
) -> Result<bytes::Bytes, ExecError> {
    use futures_util::StreamExt;
    let mut out = bytes::BytesMut::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        let room = keep.saturating_sub(out.len());
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
    Ok(out.freeze())
}

/// Go `time.Time.Format(time.RFC3339)` in UTC.
pub(crate) fn rfc3339_utc(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Go `time.Now().Format(time.RFC3339)`: local zone, `Z` only at UTC.
pub(crate) fn rfc3339_local_now() -> String {
    chrono::Local::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn normalise_unix(raw: i64) -> Option<DateTime<Utc>> {
    if raw <= 0 {
        return None;
    }
    if raw > 1_000_000_000_000 {
        Utc.timestamp_millis_opt(raw).single()
    } else {
        Utc.timestamp_opt(raw, 0).single()
    }
}

/// Go `parseTimeValue`.
pub(crate) fn parse_time(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            if let Ok(t) = DateTime::parse_from_rfc3339(s) {
                return Some(t.with_timezone(&Utc));
            }
            for layout in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
                if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, layout) {
                    return Some(t.and_utc());
                }
            }
            s.parse::<i64>().ok().and_then(normalise_unix)
        }
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .and_then(normalise_unix),
        _ => None,
    }
}

fn jwt_exp(token: &str) -> Option<DateTime<Utc>> {
    use base64::Engine;
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1].trim_end_matches('='))
        .ok()?;
    let claims: Value = serde_json::from_slice(&payload).ok()?;
    match claims.get("exp")? {
        Value::Number(n) => n.as_f64().filter(|v| *v > 0.0).and_then(|v| normalise_unix(v as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok().filter(|v| *v > 0).and_then(normalise_unix),
        _ => None,
    }
}

fn expiration_from_map(meta: &serde_json::Map<String, Value>) -> Option<DateTime<Utc>> {
    for key in ["expired", "expire", "expires_at", "expiresAt", "expiry", "expires"] {
        if let Some(t) = meta.get(key).and_then(parse_time) {
            return Some(t);
        }
    }
    let seconds = ["expires_in", "expiresIn"].iter().find_map(|k| {
        let v = meta.get(*k)?;
        let n = v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))?;
        (n > 0).then_some(n)
    });
    if let Some(seconds) = seconds
        && let Some(issued) = ["timestamp", "issued_at", "issuedAt"]
            .iter()
            .find_map(|k| meta.get(*k).and_then(parse_time))
    {
        return Some(issued + chrono::Duration::seconds(seconds));
    }
    for nested in ["token", "Token"] {
        if let Some(Value::Object(inner)) = meta.get(nested)
            && let Some(t) = expiration_from_map(inner)
        {
            return Some(t);
        }
    }
    None
}

/// Go `Auth.ExpirationTime`: access-token JWT `exp`, then metadata expiry fields.
pub(crate) fn expiration(credential: &Credential) -> Option<DateTime<Utc>> {
    let token = credential
        .str("access_token")
        .filter(|s| !s.is_empty())
        .or_else(|| credential.str("accessToken"))
        .unwrap_or_default();
    jwt_exp(token).or_else(|| expiration_from_map(&credential.metadata))
}

pub(crate) fn last_refresh(credential: &Credential) -> Option<DateTime<Utc>> {
    ["last_refresh", "lastRefresh", "last_refreshed_at", "lastRefreshedAt"]
        .iter()
        .find_map(|k| credential.metadata.get(*k).and_then(parse_time))
        .or_else(|| {
            ["last_refresh", "lastRefresh", "last_refreshed_at", "lastRefreshedAt"]
                .iter()
                .find_map(|k| {
                    credential
                        .attributes
                        .get(*k)
                        .and_then(|v| parse_time(&Value::String(v.clone())))
                })
        })
}

pub(crate) fn preferred_interval(credential: &Credential) -> Option<chrono::Duration> {
    const KEYS: [&str; 4] = [
        "refresh_interval_seconds",
        "refreshIntervalSeconds",
        "refresh_interval",
        "refreshInterval",
    ];
    let seconds = KEYS.iter().find_map(|k| match credential.metadata.get(*k)? {
        Value::Number(n) => n.as_f64().filter(|v| *v > 0.0),
        Value::String(s) => parse_duration_string(s),
        _ => None,
    });
    let seconds = seconds.or_else(|| {
        KEYS.iter()
            .find_map(|k| {
                credential
                    .attributes
                    .get(*k)
                    .and_then(|s| parse_go_duration(s).or_else(|| s.trim().parse().ok()))
            })
            .filter(|v: &f64| *v > 0.0)
    })?;
    chrono::Duration::try_milliseconds((seconds * 1000.0) as i64)
}

/// Go `time.ParseDuration`, in seconds.
pub(crate) fn parse_go_duration(s: &str) -> Option<f64> {
    let mut rest = s;
    let negative = rest.starts_with('-');
    rest = rest.strip_prefix(['-', '+']).unwrap_or(rest);
    if rest == "0" {
        return Some(0.0);
    }
    if rest.is_empty() {
        return None;
    }
    let mut total = 0.0;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let number = &rest[..digits];
        if number.is_empty() || number == "." || number.matches('.').count() > 1 {
            return None;
        }
        rest = &rest[digits..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let scale = match &rest[..unit_len] {
            "ns" => 1e-9,
            "us" | "\u{b5}s" | "\u{3bc}s" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
        rest = &rest[unit_len..];
        total += number.parse::<f64>().ok()? * scale;
    }
    Some(if negative { -total } else { total })
}

/// `parseDurationString`: a Go duration, else plain seconds; non-positive is absent.
fn parse_duration_string(s: &str) -> Option<f64> {
    let s = s.trim();
    parse_go_duration(s)
        .filter(|v| *v > 0.0)
        .or_else(|| s.parse::<f64>().ok().filter(|v| *v > 0.0))
}

/// Go `shouldRefresh` timing for one credential (backoff and lifecycle gates belong to the
/// runtime). `lead` is the provider's SDK refresh lead; `None` means no scheduled refresh.
pub(crate) fn refresh_due(credential: &Credential, lead: Option<chrono::Duration>, now: DateTime<Utc>) -> bool {
    let expiry = expiration(credential);
    if let Some(interval) = preferred_interval(credential) {
        if let Some(expiry) = expiry
            && (expiry <= now || expiry - now <= interval)
        {
            return true;
        }
        return last_refresh(credential).is_none_or(|last| now - last >= interval);
    }
    let Some(lead) = lead else {
        return false;
    };
    if let Some(expiry) = expiry {
        return expiry - now <= lead;
    }
    last_refresh(credential).is_none_or(|last| now - last >= lead)
}

/// A control-plane failure that must not echo upstream bodies (they may contain tokens).
pub(crate) fn auth_error(status: u16, message: impl Into<String>) -> ExecError {
    ExecError::local(status, FailureScope::Credential, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(meta: Value) -> Credential {
        Credential::from_file(
            std::path::Path::new("/fake"),
            std::path::Path::new("/fake/a.json"),
            meta.as_object().unwrap().clone(),
        )
        .unwrap()
    }

    #[test]
    fn refresh_due_uses_jwt_exp_before_metadata_and_lead() {
        let now = Utc.timestamp_opt(1_790_000_000, 0).unwrap();
        let lead = Some(chrono::Duration::minutes(5));
        use base64::Engine;
        let exp = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":1790000100}"#);
        let jwt = format!("h.{exp}.s");
        // JWT exp (100s away) wins over a far metadata expiry.
        let c = credential(serde_json::json!({"type":"kimi","access_token":jwt,"expired":"2099-01-01T00:00:00Z"}));
        assert!(refresh_due(&c, lead, now));
        let c = credential(serde_json::json!({"type":"kimi","access_token":"opaque","expired":"2099-01-01T00:00:00Z"}));
        assert!(!refresh_due(&c, lead, now));
        // No expiry: last refresh decides; none recorded means refresh now.
        let c = credential(serde_json::json!({"type":"kimi","access_token":"opaque"}));
        assert!(refresh_due(&c, lead, now));
        let c = credential(serde_json::json!({"type":"kimi","access_token":"opaque","last_refresh":1_789_999_900}));
        assert!(!refresh_due(&c, lead, now));
        assert!(!refresh_due(&c, None, now), "no lead means no scheduled refresh");
        // expires_in is relative to timestamp; values above 1e12 are milliseconds.
        let c = credential(
            serde_json::json!({"type":"kimi","access_token":"o","expires_in":200,"timestamp":1_789_999_900_000u64}),
        );
        assert!(refresh_due(&c, lead, now));
        let c = credential(
            serde_json::json!({"type":"kimi","access_token":"o","expires_in":900,"timestamp":1_789_999_900_000u64}),
        );
        assert!(!refresh_due(&c, lead, now));
        // refresh_interval accepts Go compound durations and plain seconds.
        let c = credential(
            serde_json::json!({"type":"kimi","access_token":"o","refresh_interval":"1h30m","last_refresh":1_790_000_000 - 1200}),
        );
        assert!(!refresh_due(&c, lead, now));
        let c = credential(
            serde_json::json!({"type":"kimi","access_token":"o","refresh_interval":"600","last_refresh":1_790_000_000 - 1200}),
        );
        assert!(refresh_due(&c, lead, now));
        assert_eq!(parse_go_duration("1h30m"), Some(5400.0));
        assert_eq!(parse_go_duration("1.5h2.5s"), Some(5402.5));
        assert_eq!(parse_go_duration("300ms"), Some(0.3));
        assert_eq!(parse_go_duration("10"), None);
        assert_eq!(parse_go_duration("h"), None);
    }
}
