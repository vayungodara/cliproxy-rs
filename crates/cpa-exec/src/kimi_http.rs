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
    platform_hostname()
}

#[cfg(unix)]
fn platform_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes into the provided buffer.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// Go's Windows `os.Hostname`: `GetComputerNameExW(ComputerNamePhysicalDnsHostname)`,
/// growing the buffer while the call answers `ERROR_MORE_DATA` with a larger size.
#[cfg(windows)]
fn platform_hostname() -> Option<String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetComputerNameExW(name_type: i32, buffer: *mut u16, size: *mut u32) -> i32;
        fn GetLastError() -> u32;
    }
    const COMPUTER_NAME_PHYSICAL_DNS_HOSTNAME: i32 = 5;
    const ERROR_MORE_DATA: u32 = 234;
    let mut n: u32 = 64;
    loop {
        let mut buf = vec![0u16; n as usize];
        // SAFETY: `buf` holds `n` UTF-16 units; the call writes at most that many and
        // stores the written (or required) length in `n`.
        let ok = unsafe { GetComputerNameExW(COMPUTER_NAME_PHYSICAL_DNS_HOSTNAME, buf.as_mut_ptr(), &mut n) };
        if ok != 0 {
            buf.truncate(n as usize);
            // syscall.UTF16ToString: up to the first NUL, invalid surrogates as U+FFFD.
            let end = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
            return Some(String::from_utf16_lossy(&buf[..end]));
        }
        // SAFETY: no preconditions.
        if unsafe { GetLastError() } != ERROR_MORE_DATA || n as usize <= buf.len() {
            return None;
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn platform_hostname() -> Option<String> {
    None
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
// Provider-neutral; hoist into crate::proxy if others need it.
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
    let from_metadata = KEYS.iter().find_map(|k| match credential.metadata.get(*k)? {
        Value::Number(n) => n.as_f64().and_then(seconds),
        Value::String(s) => parse_duration_string(s),
        _ => None,
    });
    from_metadata.or_else(|| {
        KEYS.iter()
            .find_map(|k| parse_duration_string(credential.attributes.get(*k)?))
    })
}

/// Go `time.ParseDuration`, kept to the nanosecond; non-positive is absent.
fn go_duration(s: &str) -> Option<chrono::Duration> {
    cpa_core::config::parse_duration(s)
        .filter(|nanos| *nanos > 0)
        .map(chrono::Duration::nanoseconds)
}

/// Plain seconds as Go converts them, `time.Duration(secs * float64(time.Second))`;
/// non-positive is absent.
fn seconds(value: f64) -> Option<chrono::Duration> {
    (value > 0.0).then(|| chrono::Duration::nanoseconds((value * 1e9) as i64))
}

/// `parseDurationString`: a Go duration, else plain seconds; non-positive is absent.
fn parse_duration_string(s: &str) -> Option<chrono::Duration> {
    let s = s.trim();
    go_duration(s).or_else(|| s.parse::<f64>().ok().and_then(seconds))
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
    /// Kimi native Responses: terminal `data:` events, Codex usage with tokens, else the
    /// top-level OpenAI usage with tokens.
    KimiResponses,
    /// Meta: `response.completed` / `response.incomplete` `data:` events with Codex usage
    /// (or a tier).
    MetaResponses,
}

/// Go's `StreamUsageBuffer` for the Responses rules: the merged detail as the usage node
/// it was parsed from (and that node's format) plus the response service tier.
#[derive(Default)]
struct Merged {
    ok: bool,
    usage: Option<(cpa_core::format::Format, Vec<u8>)>,
    tier: String,
}

impl Merged {
    /// `StreamUsageBuffer.Observe(detail, true)`: a detail with tokens (or without a tier)
    /// replaces the buffer, keeping the old tier when it has none; a tier-only detail
    /// updates just the tier.
    fn observe(&mut self, usage: Option<(cpa_core::format::Format, Vec<u8>)>, tier: String, nonzero: bool) {
        if tier.is_empty() || nonzero {
            self.usage = usage;
            if !tier.is_empty() {
                self.tier = tier;
            }
        } else {
            self.tier = tier;
        }
        self.ok = true;
    }

    /// The merged detail as one payload of its format, without a response model.
    fn payload(&self) -> Option<(cpa_core::format::Format, Vec<u8>)> {
        use cpa_core::format::Format;
        if !self.ok {
            return None;
        }
        let (format, node) = match &self.usage {
            Some((format, node)) => (*format, Some(node.as_slice())),
            None => (Format::Codex, None),
        };
        let mut body = b"{".to_vec();
        if let Some(node) = node {
            body.extend_from_slice(b"\"usage\":");
            body.extend_from_slice(node);
        }
        if !self.tier.is_empty() {
            if node.is_some() {
                body.push(b',');
            }
            body.extend_from_slice(b"\"service_tier\":");
            body.extend_from_slice(serde_json::to_string(&self.tier).unwrap_or_default().as_bytes());
        }
        body.push(b'}');
        let payload = match format {
            Format::OpenAI => body,
            _ => [&b"{\"type\":\"response.completed\",\"response\":"[..], &body, b"}"].concat(),
        };
        Some((format, payload))
    }
}

#[derive(Default)]
struct Held {
    /// Kimi chat: the usage and tier lines, in order, without their response models.
    lines: Vec<(cpa_core::format::Format, Vec<u8>)>,
    /// The Responses rules' merged buffer.
    merged: Merged,
}

/// Usage held back until the response completes (Go's deferred `StreamUsageBuffer.Publish`;
/// a failure publishes no usage, `PublishFailure` uses an empty detail). Call [`Self::commit`]
/// exactly when the stream ends without an error. Committed payloads carry no response
/// model, so they never replace the model observed live.
#[derive(Clone, Default)]
pub(crate) struct DeferredUsage {
    sink: cpa_core::exec::UsageSink,
    held: std::sync::Arc<std::sync::Mutex<Held>>,
}

impl DeferredUsage {
    /// Reports the held usage.
    pub(crate) fn commit(&self) {
        let held = std::mem::take(&mut *self.held.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        for (format, payload) in held.lines.into_iter().chain(held.merged.payload()) {
            self.sink.response_line(format, &[&b"data: "[..], &payload].concat());
        }
    }
}

/// Go `jsonPayload`: the JSON payload of a `data:` line (or a bare JSON line), trimmed
/// with `bytes.TrimSpace`.
fn line_payload(line: &[u8]) -> Option<&[u8]> {
    use cpa_common::gostr::trim_space;
    let trimmed = trim_space(line);
    let payload = trimmed.strip_prefix(b"data:").map_or(trimmed, trim_space);
    (payload.first() == Some(&b'{')).then_some(payload)
}

/// A payload without its usage and service tier, so reporting it only feeds the
/// response model (Go `ObserveResponseModel`).
pub(crate) fn model_only(payload: &[u8]) -> Vec<u8> {
    strip_paths(
        payload,
        &[
            "usage",
            "service_tier",
            "response.usage",
            "response.service_tier",
            "interaction.service_tier",
        ],
    )
}

/// The model fields Go's generic response-model extractor reads.
const MODEL_PATHS: [&str; 6] = [
    "response.model",
    "interaction.model",
    "modelVersion",
    "response.modelVersion",
    "message.model",
    "model",
];

fn strip_paths(payload: &[u8], paths: &[&str]) -> Vec<u8> {
    let mut out = payload.to_vec();
    for path in paths {
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
    let total = node.get("total_tokens").int();
    let input = pick(node, &["prompt_tokens", "input_tokens"]);
    let output = pick(node, &["completion_tokens", "output_tokens"]);
    total > 0 || input > 0 || (total == 0 && input == 0 && output > 0)
}

/// The first existing path's integer (Go's `Exists()` fallbacks in parseOpenAIStyleUsageNode).
fn pick(node: &cpa_common::json::Res<'_>, paths: &[&str]) -> i64 {
    paths
        .iter()
        .map(|p| node.get(p))
        .find(|r| r.exists())
        .map_or(0, |r| r.int())
}

/// Go `hasNonZeroTokenUsage` for the detail parsed from `node`.
fn usage_nonzero(node: &cpa_common::json::Res<'_>) -> bool {
    [
        &["prompt_tokens", "input_tokens"][..],
        &["completion_tokens", "output_tokens"],
        &["total_tokens"],
        &[
            "prompt_tokens_details.cached_tokens",
            "input_tokens_details.cached_tokens",
        ],
        &[
            "input_tokens_details.cache_creation_tokens",
            "input_tokens_details.cache_write_tokens",
            "prompt_tokens_details.cache_creation_tokens",
            "prompt_tokens_details.cache_write_tokens",
        ],
        &[
            "completion_tokens_details.reasoning_tokens",
            "output_tokens_details.reasoning_tokens",
        ],
    ]
    .iter()
    .any(|paths| pick(node, paths) != 0)
}

/// Go `hasOpenAIStyleUsageTokenFields`.
fn usage_has_fields(node: &cpa_common::json::Res<'_>) -> bool {
    node.is_object()
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
        .any(|f| node.get(f).exists())
}

/// Go `extractResponseServiceTier`: the first non-blank tier of a valid payload.
pub(crate) fn response_tier(payload: &[u8]) -> String {
    if !cpa_common::json::valid(payload) {
        return String::new();
    }
    ["response.service_tier", "service_tier", "interaction.service_tier"]
        .iter()
        .map(|p| cpa_common::json::get(payload, p).str().into_owned())
        .map(|t| String::from_utf8_lossy(cpa_common::gostr::trim_space(t.as_bytes())).into_owned())
        .find(|t| !t.is_empty())
        .unwrap_or_default()
}

/// One `StreamUsageBuffer.Observe` of Go `ParseCodexUsage(payload)` (format Codex) or
/// `ParseOpenAIUsage(payload)` (format OpenAI); false when Go's `ok` is false.
fn observe_parsed(merged: &mut Merged, format: cpa_core::format::Format, payload: &[u8]) -> bool {
    let path = if format == cpa_core::format::Format::OpenAI {
        "usage"
    } else {
        "response.usage"
    };
    let node = cpa_common::json::get(payload, path);
    let tier = response_tier(payload);
    if !usage_has_fields(&node) {
        if tier.is_empty() {
            return false;
        }
        merged.observe(None, tier, false);
        return true;
    }
    merged.observe(Some((format, node.raw().to_vec())), tier, usage_nonzero(&node));
    true
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
        held: Default::default(),
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
            let mut held = held.held.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let kind = cpa_common::json::get(payload, "type").bytes().into_owned();
            match rule {
                UsageRule::OpenAIStream => {
                    let has = |n: &[u8]| payload.windows(n.len()).any(|w| w == n);
                    if has(b"\"usage\"") || has(b"\"service_tier\"") {
                        let payload = if cpa_common::json::valid(payload) {
                            strip_paths(payload, &MODEL_PATHS)
                        } else {
                            payload.to_vec()
                        };
                        held.lines.push((Format::OpenAI, payload));
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
                    // Only details with tokens are observed, so each replaces the buffer.
                    if usage_has_tokens(&cpa_common::json::get(payload, "response.usage")) {
                        observe_parsed(&mut held.merged, Format::Codex, payload);
                    } else if usage_has_tokens(&cpa_common::json::get(payload, "usage")) {
                        observe_parsed(&mut held.merged, Format::OpenAI, payload);
                    }
                }
                UsageRule::MetaResponses => {
                    if matches!(kind.as_slice(), b"response.completed" | b"response.incomplete") {
                        observe_parsed(&mut held.merged, Format::Codex, payload);
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

/// Go's TTFT tracking on the response body (`usageTTFTReadCloser`): the first read that
/// returns bytes marks the first response byte, or with `packet_only` (Go
/// `TrackHTTPClientRoundTripOnly`) a first non-token frame. Call
/// `usage.round_trip_started()` before sending the request.
pub(crate) fn track_first_byte(
    body: futures_util::stream::BoxStream<'static, Result<bytes::Bytes, ExecError>>,
    usage: &cpa_core::exec::UsageSink,
    packet_only: bool,
) -> futures_util::stream::BoxStream<'static, Result<bytes::Bytes, ExecError>> {
    use futures_util::StreamExt;
    if !usage.enabled() {
        return body;
    }
    let (usage, mut marked) = (usage.clone(), false);
    body.inspect(move |item| {
        if !marked && item.as_ref().is_ok_and(|chunk| !chunk.is_empty()) {
            marked = true;
            if packet_only {
                usage.token_event(false);
            } else {
                usage.first_byte();
            }
        }
    })
    .boxed()
}

/// Go `Auth.AccountInfo()`: the credential kind and its loggable value, the email for
/// OAuth and the API key for API-key credentials (masked by the capture sink).
pub(crate) fn account_info(credential: &Credential) -> (&'static str, String) {
    match cpa_core::registry::dynamic::auth_kind(credential) {
        Some("oauth") => ("oauth", credential.str("email").unwrap_or_default().trim().to_owned()),
        Some("apikey") => (
            "api_key",
            credential
                .attributes
                .get("api_key")
                .map(|k| k.trim().to_owned())
                .unwrap_or_default(),
        ),
        _ => ("", String::new()),
    }
}

/// Go `http.Response.Header` for an HTTP/1.1 upstream response (net/http
/// `ReadResponse`): canonical names, every value in order, with `Transfer-Encoding`
/// moved out; `Connection` dropped when it carries `close`; `Trailer` and
/// `Content-Length` dropped from chunked responses (Content-Length kept for statuses
/// without a body); identical duplicate `Content-Length` values collapsed; and
/// `Cache-Control: no-cache` added for `Pragma: no-cache`.
// ponytail: the capture's view only; proxy::Upstream keeps the transport's headers for
// every other use.
pub(crate) fn go_response_headers(status: u16, headers: &http::HeaderMap) -> Vec<(String, String)> {
    use http::header::{CACHE_CONTROL, CONNECTION, CONTENT_LENGTH, PRAGMA, TRAILER, TRANSFER_ENCODING};
    let chunked = headers.contains_key(TRANSFER_ENCODING);
    let close = headers.get_all(CONNECTION).iter().any(|v| {
        v.as_bytes()
            .split(|b| *b == b',')
            .any(|t| t.trim_ascii().eq_ignore_ascii_case(b"close"))
    });
    let has_body = !(status / 100 == 1 || status == 204 || status == 304);
    let mut length_seen = false;
    let mut out: Vec<(String, String)> = headers
        .iter()
        .filter(|(name, _)| match *name {
            n if n == TRANSFER_ENCODING => false,
            n if n == CONNECTION => !close,
            n if n == TRAILER => !chunked,
            n if n == CONTENT_LENGTH => !(chunked && has_body) && !std::mem::replace(&mut length_seen, true),
            _ => true,
        })
        .map(|(name, value)| {
            (
                crate::proxy::canonical_header(name.as_str()),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    if headers.get(PRAGMA).is_some_and(|v| v.as_bytes() == b"no-cache") && !headers.contains_key(CACHE_CONTROL) {
        out.push(("Cache-Control".into(), "no-cache".into()));
    }
    out
}

/// Go `helps.RecordAPIRequest` at an executor's send site: `headers` as Go's request
/// holds them, the auth fields as that call site fills them.
pub(crate) fn capture_request(
    capture: &cpa_core::exec::CaptureSink,
    credential: &Credential,
    url: &str,
    headers: &crate::proxy::GoHeaders,
    body: &[u8],
    provider: &str,
    (auth_type, auth_value): (&str, &str),
) {
    if !capture.enabled() {
        return;
    }
    capture.record(cpa_core::exec::CaptureEvent::Request(cpa_core::exec::UpstreamRequest {
        url,
        method: "POST",
        headers: headers.pairs(),
        body,
        provider,
        auth_id: &credential.id,
        auth_label: &credential.label,
        auth_type,
        auth_value,
    }));
}

/// Go `helps.RecordAPIResponseMetadata(status, header)`.
pub(crate) fn capture_metadata(capture: &cpa_core::exec::CaptureSink, status: u16, headers: &http::HeaderMap) {
    if capture.enabled() {
        let headers = go_response_headers(status, headers);
        capture.record(cpa_core::exec::CaptureEvent::ResponseMetadata(status, &headers));
    }
}

/// Go `helps.AppendAPIResponseChunk`.
pub(crate) fn capture_chunk(capture: &cpa_core::exec::CaptureSink, chunk: &[u8]) {
    capture.record(cpa_core::exec::CaptureEvent::ResponseChunk(chunk));
}

/// Go `helps.RecordAPIResponseError(err)` with the error's text.
// ponytail: transport errors carry this crate's message, not Go's `Post "url": ...`
// wording; upgrade by keeping the client error text on ExecError.
pub(crate) fn capture_error(capture: &cpa_core::exec::CaptureSink, error: &ExecError) {
    if capture.enabled() {
        capture.record(cpa_core::exec::CaptureEvent::ResponseError(&String::from_utf8_lossy(
            &error.body,
        )));
    }
}

/// Go's per-line `AppendAPIResponseChunk(line)` in a scanner loop, and the scan error
/// it records after the loop.
// ponytail: the scan error is recorded when the line reader reports it; Go skips the
// record when an apply_patch end check already failed the stream (both failures in one
// stream).
pub(crate) fn capture_lines(
    lines: cpa_core::exec::ExecStream,
    capture: &cpa_core::exec::CaptureSink,
) -> cpa_core::exec::ExecStream {
    use futures_util::StreamExt;
    if !capture.enabled() {
        return lines;
    }
    let capture = capture.clone();
    lines
        .inspect(move |item| match item {
            Ok(line) => capture_chunk(&capture, line),
            Err(error) => capture_error(&capture, error),
        })
        .boxed()
}

/// A control-plane failure that must not echo upstream bodies (they may contain tokens).
pub(crate) fn auth_error(status: u16, message: impl Into<String>) -> ExecError {
    ExecError::local(status, FailureScope::Credential, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_headers_take_go_response_header_shape() {
        // Expected values from Go 1.26 http.ReadResponse on the same raw responses.
        let check = |status: u16, raw: &[(&str, &str)], want: &[&str]| {
            let mut h = http::HeaderMap::new();
            for (n, v) in raw {
                h.append(http::HeaderName::from_bytes(n.as_bytes()).unwrap(), v.parse().unwrap());
            }
            let mut got: Vec<String> = go_response_headers(status, &h)
                .into_iter()
                .map(|(n, v)| format!("{n}: {v}"))
                .collect();
            got.sort();
            assert_eq!(got, want, "{raw:?}");
        };
        check(
            503,
            &[
                ("transfer-encoding", "chunked"),
                ("content-length", "1"),
                ("trailer", "X-Checksum"),
                ("connection", "close"),
                ("pragma", "no-cache"),
                ("x-id", "a"),
            ],
            &["Cache-Control: no-cache", "Pragma: no-cache", "X-Id: a"],
        );
        check(
            503,
            &[
                ("content-length", "1"),
                ("content-length", "1"),
                ("connection", "keep-alive"),
                ("trailer", "X-Checksum"),
                ("pragma", "no-cache"),
                ("cache-control", "max-age=1"),
                ("set-cookie", "s=1"),
                ("set-cookie", "t=2"),
            ],
            &[
                "Cache-Control: max-age=1",
                "Connection: keep-alive",
                "Content-Length: 1",
                "Pragma: no-cache",
                "Set-Cookie: s=1",
                "Set-Cookie: t=2",
                "Trailer: X-Checksum",
            ],
        );
        check(
            204,
            &[
                ("transfer-encoding", "chunked"),
                ("content-length", "0"),
                ("connection", "Keep-Alive, Close"),
            ],
            &["Content-Length: 0"],
        );
    }

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
        let ms = chrono::Duration::milliseconds;
        assert_eq!(parse_duration_string("1h30m"), Some(chrono::Duration::seconds(5400)));
        assert_eq!(parse_duration_string("1.5h2.5s"), Some(ms(5_402_500)));
        assert_eq!(parse_duration_string("1001ms"), Some(ms(1001)));
        assert_eq!(parse_duration_string("1005ms"), Some(ms(1005)));
        assert_eq!(parse_duration_string("10"), Some(chrono::Duration::seconds(10)));
        assert_eq!(parse_duration_string("-1s"), None);
        assert_eq!(parse_duration_string(" 1h "), Some(chrono::Duration::hours(1)));
        assert_eq!(parse_duration_string("1.5"), Some(ms(1500)));
        assert_eq!(go_duration("10"), None);
        assert_eq!(go_duration("h"), None);
    }
}
