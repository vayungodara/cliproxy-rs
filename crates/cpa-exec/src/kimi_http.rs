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

/// Which Go executor's stream usage rules a [`defer_usage`] tap follows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsageRule {
    /// Kimi chat (`StreamUsageBuffer.ObserveOpenAIStream` on every line).
    OpenAIStream,
    /// Kimi native Responses: terminal events, Codex usage with tokens, else the
    /// top-level OpenAI usage with tokens; the latest wins.
    KimiResponses,
    /// Meta: `response.completed` / `response.incomplete` with Codex usage (or a tier);
    /// the latest wins.
    MetaResponses,
}

/// Usage held back until the response completes (Go's deferred `StreamUsageBuffer.Publish`;
/// a failure publishes no usage, `PublishFailure` uses an empty detail). Call [`Self::commit`]
/// exactly when the stream ends without an error.
#[derive(Clone, Default)]
pub(crate) struct DeferredUsage {
    sink: cpa_core::exec::UsageSink,
    pending: std::sync::Arc<std::sync::Mutex<Vec<(cpa_core::format::Format, Vec<u8>)>>>,
}

impl DeferredUsage {
    /// Reports the held usage lines, in order.
    pub(crate) fn commit(&self) {
        let pending = std::mem::take(&mut *self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        for (format, line) in pending {
            self.sink.response_line(format, &line);
        }
    }
}

/// The JSON payload of a `data:` line (or a bare JSON line), trimmed.
fn line_payload(line: &[u8]) -> Option<&[u8]> {
    let trimmed = line.trim_ascii();
    let payload = trimmed.strip_prefix(b"data:").map_or(trimmed, <[u8]>::trim_ascii);
    (payload.first() == Some(&b'{')).then_some(payload)
}

/// A payload without its usage and service tier, so reporting it only feeds the
/// response model (Go `ObserveResponseModel`).
pub(crate) fn model_only(payload: &[u8]) -> Vec<u8> {
    let mut out = payload.to_vec();
    for path in [
        "usage",
        "service_tier",
        "response.usage",
        "response.service_tier",
        "interaction.service_tier",
    ] {
        if cpa_common::json::get(&out, path).exists() {
            cpa_common::json::delete(&mut out, path);
        }
    }
    out
}

/// Go's `TotalTokens > 0 || InputTokens > 0` for a parsed OpenAI-style usage node
/// (`parseOpenAIStyleUsageNode`). A `total_tokens` of exactly 0 falls back to the token
/// breakdown's total, which is input + output when both are non-negative; with input 0
/// that is the output count.
pub(crate) fn usage_has_tokens(node: &cpa_common::json::Res<'_>) -> bool {
    if !node.is_object() {
        return false;
    }
    let pick = |a: &str, b: &str| {
        let n = node.get(a);
        if n.exists() { n.int() } else { node.get(b).int() }
    };
    let total = node.get("total_tokens").int();
    let input = pick("prompt_tokens", "input_tokens");
    let output = pick("completion_tokens", "output_tokens");
    total > 0 || input > 0 || (total == 0 && input == 0 && output > 0)
}

/// Go `ParseCodexUsage(..)` `ok`: a `response.usage` with token fields, or a tier.
fn codex_usage_ok(payload: &[u8]) -> bool {
    let node = cpa_common::json::get(payload, "response.usage");
    let has_fields = node.is_object()
        && [
            "total_tokens",
            "prompt_tokens",
            "input_tokens",
            "completion_tokens",
            "output_tokens",
            "prompt_tokens_details.cached_tokens",
            "input_tokens_details.cached_tokens",
            "prompt_tokens_details.cache_write_tokens",
            "prompt_tokens_details.cache_creation_tokens",
            "input_tokens_details.cache_write_tokens",
            "input_tokens_details.cache_creation_tokens",
            "completion_tokens_details.reasoning_tokens",
            "output_tokens_details.reasoning_tokens",
        ]
        .iter()
        .any(|f| node.get(f).exists());
    has_fields
        || ["response.service_tier", "service_tier", "interaction.service_tier"]
            .iter()
            .any(|p| !cpa_common::json::get(payload, p).str().trim().is_empty())
}

/// Taps the upstream lines for usage the way `rule`'s Go executor observes them: each
/// line's response model is reported as it arrives; usage is held in the returned
/// [`DeferredUsage`] until the caller commits it on success.
pub(crate) fn defer_usage(
    lines: cpa_core::exec::ExecStream,
    usage: &cpa_core::exec::UsageSink,
    rule: UsageRule,
) -> (cpa_core::exec::ExecStream, DeferredUsage) {
    use cpa_core::format::Format;
    use futures_util::StreamExt;
    let deferred = DeferredUsage {
        sink: usage.clone(),
        pending: Default::default(),
    };
    if !usage.enabled() {
        return (lines, deferred);
    }
    let (sink, held) = (usage.clone(), deferred.clone());
    let model_format = if rule == UsageRule::OpenAIStream {
        Format::OpenAI
    } else {
        Format::Codex
    };
    let tapped = lines
        .inspect(move |item| {
            let Ok(line) = item else { return };
            // Go matches `data:` on the raw scanned line for Responses usage, and Meta
            // observes nothing else.
            let is_data = line.starts_with(b"data:");
            if rule == UsageRule::MetaResponses && !is_data {
                return;
            }
            let Some(payload) = line_payload(line) else { return };
            let mut model_line = b"data: ".to_vec();
            model_line.extend_from_slice(&model_only(payload));
            sink.response_line(model_format, &model_line);
            let mut pending = held.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let kind = cpa_common::json::get(payload, "type").bytes().into_owned();
            match rule {
                UsageRule::OpenAIStream => {
                    let has = |n: &[u8]| payload.windows(n.len()).any(|w| w == n);
                    if has(b"\"usage\"") || has(b"\"service_tier\"") {
                        pending.push((Format::OpenAI, line.to_vec()));
                    }
                }
                UsageRule::KimiResponses => {
                    if !is_data
                        || !matches!(
                            kind.as_slice(),
                            b"response.completed" | b"response.incomplete" | b"response.done"
                        )
                    {
                        return;
                    }
                    if codex_usage_ok(payload) && usage_has_tokens(&cpa_common::json::get(payload, "response.usage")) {
                        *pending = vec![(Format::Codex, line.to_vec())];
                    } else if usage_has_tokens(&cpa_common::json::get(payload, "usage")) {
                        *pending = vec![(Format::OpenAI, line.to_vec())];
                    }
                }
                UsageRule::MetaResponses => {
                    if matches!(kind.as_slice(), b"response.completed" | b"response.incomplete")
                        && codex_usage_ok(payload)
                    {
                        *pending = vec![(Format::Codex, line.to_vec())];
                    }
                }
            }
        })
        .boxed();
    (tapped, deferred)
}

/// Go `ObserveResponseModel(body)` before a translation that may still fail: only the
/// response model is reported; usage follows once the response succeeded.
pub(crate) fn report_model(sink: &cpa_core::exec::UsageSink, format: cpa_core::format::Format, body: &[u8]) {
    if !sink.enabled() {
        return;
    }
    // Go's generic model extraction needs the whole payload to be one valid JSON value,
    // so a multi-event SSE body names no model.
    match line_payload(body).filter(|p| cpa_common::json::valid(p)) {
        Some(payload) => sink.response_line(format, &model_only(payload)),
        None => sink.response_line(format, b"{}"),
    }
}

/// Go `helps.ApplyPatchRequested`: the client's original Responses request (or its
/// `request` wrapper) declares a winning custom `apply_patch` tool.
pub(crate) fn apply_patch_requested(original: &[u8]) -> bool {
    use cpa_translate::apply_patch_responses::State;
    if original.is_empty() || !cpa_common::json::valid(original) {
        return false;
    }
    let req = cpa_common::json::get(original, "request");
    let root = if req.exists() && (req.get("model").exists() || req.get("input").exists() || req.get("tools").exists())
    {
        req.raw().to_vec()
    } else {
        original.to_vec()
    };
    State::new(cpa_core::format::Format::OpenAIResponse, &root, &root).active()
}

/// Go `SetTranslatedReasoningEffort(body, provider)` for a provider name the sink's
/// `Format` cannot carry (Kimi passes `"kimi"`, whose thinking fields differ from
/// OpenAI's): the effort Go extracts, reported as an OpenAI `reasoning_effort`, which the
/// server's OpenAI extraction reads back unchanged.
// ponytail: a provider-named `UsageSink::request` would make this rewrite unnecessary.
pub(crate) fn report_effort(usage: &cpa_core::exec::UsageSink, body: &[u8], provider: &str) {
    if !usage.enabled() {
        return;
    }
    let effort = cpa_common::thinking::extract_translated_reasoning_effort(body, provider);
    let mut payload = b"{}".to_vec();
    if !effort.is_empty() {
        cpa_common::json::set_str(&mut payload, "reasoning_effort", effort);
    }
    usage.request(cpa_core::format::Format::OpenAI, &payload);
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
