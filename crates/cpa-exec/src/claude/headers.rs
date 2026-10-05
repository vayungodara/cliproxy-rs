//! Upstream header assembly (applyClaudeHeadersWithNativeProfile and the ordered
//! HTTP/1.1 writer in helps/utls_client.go).
//!
//! Headers are built with Go `http.Header` semantics (canonical keys, Set/Add/Del)
//! so every rule ports literally, then serialized in the exact wire order: on
//! first-party Anthropic the measured Claude Code order and casing come first and
//! everything else follows net/http's order (Host, User-Agent, Content-Length, then
//! byte-sorted keys).

use http::HeaderMap;

use super::betas::{self, Requested};
use super::detect::{header, header_values};
use super::profile::{self, Profile};
use super::settings::Settings;
use super::signals;
use crate::rawjson;

/// Go `textproto.CanonicalMIMEHeaderKey`.
pub(crate) fn canonical(name: &str) -> String {
    let valid = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
    if !valid || name.is_empty() {
        return name.to_owned();
    }
    let mut upper = true;
    name.chars()
        .map(|c| {
            let out = if upper {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            upper = c == '-';
            out
        })
        .collect()
}

/// A Go `http.Header`: canonical key → values, insertion order irrelevant.
#[derive(Debug, Clone, Default)]
pub(crate) struct GoHeader(Vec<(String, Vec<String>)>);

impl GoHeader {
    fn idx(&self, key: &str) -> Option<usize> {
        self.0.iter().position(|(k, _)| k == key)
    }
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        let key = canonical(name);
        match self.idx(&key) {
            Some(i) => self.0[i].1 = vec![value.into()],
            None => self.0.push((key, vec![value.into()])),
        }
    }
    pub fn add(&mut self, name: &str, value: impl Into<String>) {
        let key = canonical(name);
        match self.idx(&key) {
            Some(i) => self.0[i].1.push(value.into()),
            None => self.0.push((key, vec![value.into()])),
        }
    }
    pub fn del(&mut self, name: &str) {
        let key = canonical(name);
        self.0.retain(|(k, _)| *k != key);
    }
    pub fn get(&self, name: &str) -> &str {
        self.idx(&canonical(name))
            .and_then(|i| self.0[i].1.first())
            .map(String::as_str)
            .unwrap_or_default()
    }
    /// Raw key rename after assembly (`applyClaudeWireHeaderCasing`).
    fn rename(&mut self, from: &str, to: &str) {
        if let Some(i) = self.idx(from) {
            self.0[i].0 = to.to_owned();
        }
    }
    pub fn entries(&self) -> &[(String, Vec<String>)] {
        &self.0
    }
}

/// `misc.EnsureHeader`: incoming value, else existing, else default.
fn ensure(target: &mut GoHeader, incoming: &HeaderMap, key: &str, default: &str) {
    let value = incoming
        .get(key)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim();
    if !value.is_empty() {
        target.set(key, value);
        return;
    }
    if !target.get(key).trim().is_empty() {
        return;
    }
    if !default.trim().is_empty() {
        target.set(key, default.trim());
    }
}

pub(crate) const MESSAGES_ORDER: &[&str] = &[
    "Accept",
    "Authorization",
    "Content-Type",
    "User-Agent",
    "X-Claude-Code-Session-Id",
    "X-Stainless-Arch",
    "X-Stainless-Lang",
    "X-Stainless-OS",
    "X-Stainless-Package-Version",
    "X-Stainless-Retry-Count",
    "X-Stainless-Runtime",
    "X-Stainless-Runtime-Version",
    "X-Stainless-Timeout",
    "anthropic-beta",
    "anthropic-dangerous-direct-browser-access",
    "anthropic-version",
    "x-app",
    "x-client-request-id",
    "Connection",
    "Host",
    "Accept-Encoding",
    "Content-Length",
];

/// Wire casing Claude Code uses where Go canonicalization differs.
const WIRE_CASING: &[(&str, &str)] = &[
    ("X-Stainless-Os", "X-Stainless-OS"),
    ("Anthropic-Beta", "anthropic-beta"),
    ("Anthropic-Version", "anthropic-version"),
    ("X-App", "x-app"),
    ("X-Client-Request-Id", "x-client-request-id"),
    (
        "Anthropic-Dangerous-Direct-Browser-Access",
        "anthropic-dangerous-direct-browser-access",
    ),
];

/// Everything header assembly needs to know about the request.
pub(crate) struct Plan<'a> {
    pub api_key: &'a str,
    /// Bearer vs x-api-key (`claudeCredentialUsesOAuth`).
    pub bearer: bool,
    pub first_party: bool,
    pub count_tokens: bool,
    pub stream: bool,
    pub extra_betas: &'a [String],
    pub body: &'a str,
    pub incoming: &'a HeaderMap,
    pub confirmed: bool,
    pub helper: bool,
    /// `fp.ProfileClaudeCodeCLI || wirePolicy.Cloak`.
    pub cli_fingerprint: bool,
    pub use_oauth_betas: bool,
    pub session_id: &'a str,
    pub settings: &'a Settings,
    pub attributes: &'a std::collections::BTreeMap<String, String>,
    /// `$CPA-SESSION-ID` in custom credential headers: the explicit session only
    /// (`cpa_common::session::cpa_session_id`), empty when there is none.
    pub cpa_session: &'a str,
    /// The stabilized device profile (`ResolveClaudeDeviceProfileRequired`), resolved
    /// by the caller when stabilization is on and the caller is confirmed Claude Code.
    pub device_profile: Option<Profile>,
    /// The key's cached session ID (`CachedSessionIDRequired`), resolved by the caller
    /// when [`needs_cached_session`] says so.
    pub cached_session_id: &'a str,
}

/// Go `applyClaudeHeaders`: a caller that is neither fingerprinted as Claude Code nor
/// confirmed keeps its own headers, and the builder returns before it reads any
/// identity state.
fn preserves_caller(cli_fingerprint: bool, confirmed: bool) -> bool {
    !cli_fingerprint && !confirmed
}

/// Whether [`build`] reads `cached_session_id`: only past the caller-preserving
/// return, and only without an explicit session. Go looks the cached ID up there, so
/// a passthrough request never touches the session cache or Home KV.
pub(crate) fn needs_cached_session(cli_fingerprint: bool, confirmed: bool, session_id: &str) -> bool {
    !preserves_caller(cli_fingerprint, confirmed) && session_id.trim().is_empty()
}

fn new_request_id() -> String {
    super::session::new_v4()
}

/// `applyClaudeHeadersWithNativeProfile`.
pub(crate) fn build(p: &Plan<'_>) -> GoHeader {
    let mut h = GoHeader::default();
    if !p.api_key.trim().is_empty() {
        if p.first_party && !p.bearer {
            h.set("x-api-key", p.api_key);
        } else {
            h.set("Authorization", format!("Bearer {}", p.api_key));
        }
    }
    h.set("Content-Type", "application/json");
    let preserve_caller = preserves_caller(p.cli_fingerprint, p.confirmed);
    let stabilize = p.settings.header_defaults.stabilize_device_profile;
    let device_profile = p.device_profile.clone();
    let incoming_betas = header_values(p.incoming, "anthropic-beta").join(",").trim().to_owned();
    let requested: Requested = betas::requested(&incoming_betas, p.extra_betas);
    let advisor = requested.contains(betas::ADVISOR_TOOL) || betas::has_advisor_tool(p.body);

    let mut base = incoming_betas.clone();
    if !preserve_caller {
        base = betas::cli(p.body, &requested, p.use_oauth_betas);
        if p.count_tokens {
            base = betas::count_tokens(p.use_oauth_betas);
            if advisor {
                base = betas::with_advisor(&base);
            }
        }
    }
    if p.confirmed && !incoming_betas.is_empty() {
        base = incoming_betas.clone();
        if advisor {
            base = betas::with_advisor(&base);
        }
        if p.use_oauth_betas && !p.helper {
            if p.count_tokens {
                base = betas::with_count_oauth(&base);
            } else {
                let sub = signals::subagent(p.incoming, p.body);
                let probe = signals::probe_or_helper(p.body);
                let sub1h = sub && signals::subagent_requests_1h(p.incoming, p.body);
                base = betas::with_oauth_credential(&base, (!sub || sub1h) && !probe);
            }
        }
    } else if preserve_caller && p.use_oauth_betas {
        base = if p.count_tokens {
            betas::with_count_oauth(&base)
        } else {
            betas::with_oauth_credential(&base, false)
        };
    }
    if preserve_caller && advisor {
        base = betas::with_advisor(&base);
    }
    if !betas::supports_effort(p.body) {
        base = betas::without(&base, betas::EFFORT);
    }
    let mut existing: std::collections::HashSet<String> = base
        .split(',')
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_owned)
        .collect();
    let mut append = |base: &mut String, beta: &str| {
        let beta = beta.trim();
        if beta.is_empty() || existing.contains(beta) {
            return;
        }
        if base.trim().is_empty() {
            *base = beta.into();
        } else {
            base.push(',');
            base.push_str(beta);
        }
        existing.insert(beta.into());
    };
    if preserve_caller {
        if rawjson::string(p.body, "speed").trim().eq_ignore_ascii_case("fast") {
            append(&mut base, betas::FAST_MODE);
        }
        for beta in p.extra_betas {
            append(&mut base, beta);
        }
    } else {
        if !p.confirmed && !incoming_betas.is_empty() {
            for beta in incoming_betas.split(',') {
                let beta = beta.trim();
                if beta.is_empty() || (betas::managed(beta) && p.first_party) {
                    continue;
                }
                append(&mut base, beta);
            }
        }
        if !p.first_party {
            for beta in p.extra_betas {
                append(&mut base, beta);
            }
        }
    }
    let finalize_betas = |base: &str| -> String {
        let mut b = base.to_owned();
        if !betas::supports_effort(p.body) {
            b = betas::without(&b, betas::EFFORT);
        }
        let probe = signals::probe_or_helper(p.body);
        if probe && !p.helper {
            for x in [
                betas::SERVER_SIDE_FALLBACK,
                betas::THINKING_DISPLAY_UPDATES,
                betas::EXTENDED_CACHE_TTL,
            ] {
                b = betas::without(&b, x);
            }
        }
        if rawjson::string(p.body, "thinking.type") == "disabled" {
            b = betas::without(&b, betas::THINKING_DISPLAY_UPDATES);
        }
        if signals::subagent(p.incoming, p.body) && !signals::subagent_requests_1h(p.incoming, p.body) {
            b = betas::without(&b, betas::EXTENDED_CACHE_TTL);
        }
        if !probe && !p.count_tokens && signals::has_1h_ttl(p.body) {
            b = betas::with_extended_ttl(&b);
        }
        let model = rawjson::string(p.body, "model").trim().to_lowercase();
        if model.contains("haiku") && !rawjson::get(p.body, "fallbacks").exists() && !p.helper {
            b = betas::without(&b, betas::SERVER_SIDE_FALLBACK);
        }
        b
    };
    let apply_betas = |h: &mut GoHeader, b: &str| {
        if b.trim().is_empty() {
            h.del("Anthropic-Beta");
        } else {
            h.set("Anthropic-Beta", b);
        }
    };
    base = finalize_betas(&base);
    apply_betas(&mut h, &base);

    if preserve_caller {
        let (accept, encoding) = if p.stream && !p.first_party {
            ("text/event-stream", "identity")
        } else {
            ("application/json", "gzip, deflate, br, zstd")
        };
        copy_caller_fingerprint(&mut h, p.incoming, p.confirmed);
        ensure(&mut h, p.incoming, "Anthropic-Version", "2023-06-01");
        ensure(&mut h, p.incoming, "Accept", accept);
        ensure(&mut h, p.incoming, "Accept-Encoding", encoding);
        ensure(
            &mut h,
            p.incoming,
            "User-Agent",
            &format!("CLIProxyAPI/{}", crate::claude::PROXY_VERSION),
        );
        apply_betas(&mut h, &base);
        if !p.session_id.trim().is_empty() {
            h.set("X-Claude-Code-Session-Id", p.session_id.trim());
        }
        custom_headers(&mut h, p);
        let restore = |h: &mut GoHeader| {
            for (name, fallback) in [("Accept", accept), ("Accept-Encoding", encoding)] {
                let incoming = p
                    .incoming
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .trim();
                h.set(name, if incoming.is_empty() { fallback } else { incoming });
            }
        };
        if p.first_party {
            apply_betas(&mut h, &base);
            restore(&mut h);
        } else if p.stream {
            restore(&mut h);
        }
        return h;
    }

    let identity = |h: &mut GoHeader, name: &str, fallback: &str| {
        if p.confirmed {
            ensure(h, p.incoming, name, fallback);
        } else {
            h.set(name, fallback);
        }
    };
    identity(&mut h, "Anthropic-Version", "2023-06-01");
    identity(&mut h, "Anthropic-Dangerous-Direct-Browser-Access", "true");
    identity(&mut h, "X-App", "cli");
    identity(&mut h, "X-Stainless-Retry-Count", "0");
    identity(&mut h, "X-Stainless-Runtime", "node");
    identity(&mut h, "X-Stainless-Lang", "js");
    if p.confirmed && header(p.incoming, "x-stainless-async") == "async" {
        h.set("X-Stainless-Async", "async");
    }
    if !p.count_tokens {
        let timeout = &p.settings.header_defaults.timeout;
        identity(
            &mut h,
            "X-Stainless-Timeout",
            if timeout.is_empty() { "600" } else { timeout },
        );
    } else if p.confirmed {
        let t = header(p.incoming, "x-stainless-timeout");
        if !t.is_empty() {
            h.set("X-Stainless-Timeout", t);
        }
    }
    if !p.session_id.trim().is_empty() {
        h.set("X-Claude-Code-Session-Id", p.session_id.trim());
    } else {
        identity(&mut h, "X-Claude-Code-Session-Id", p.cached_session_id);
    }
    for name in [
        "X-Claude-Code-Agent-Id",
        "X-Claude-Code-Parent-Agent-Id",
        "X-Claude-Remote-Container-Id",
        "X-Claude-Remote-Session-Id",
        "X-Client-App",
        "X-Anthropic-Additional-Protection",
    ] {
        let v = header(p.incoming, name);
        if !v.is_empty() {
            h.set(name, v);
        }
    }
    if p.confirmed {
        for name in [
            "X-Claude-Code-Request-Class",
            "X-Claude-Code-Agent-Type",
            "X-Claude-Code-Prev-Tool-Durations",
            "X-Claude-Code-Compaction",
            "X-Claude-Code-Context-Compacted",
        ] {
            let v = header(p.incoming, name);
            if !v.is_empty() {
                h.set(name, v);
            }
        }
    }
    if p.first_party || (p.helper && !header(p.incoming, "x-client-request-id").is_empty()) {
        identity(&mut h, "x-client-request-id", &new_request_id());
    }
    h.set("Connection", "keep-alive");
    let transport = |h: &mut GoHeader| {
        if p.helper {
            identity(h, "Accept", "application/json");
            identity(h, "Accept-Encoding", "gzip, deflate, br, zstd");
        } else if p.stream && !p.first_party {
            h.set("Accept", "text/event-stream");
            h.set("Accept-Encoding", "identity");
        } else {
            h.set("Accept", "application/json");
            h.set("Accept-Encoding", "gzip, deflate, br, zstd");
        }
    };
    transport(&mut h);
    let set_profile = |h: &mut GoHeader, d: &Profile| {
        h.set("User-Agent", &d.user_agent);
        h.set("X-Stainless-Package-Version", &d.package_version);
        h.set("X-Stainless-Runtime-Version", &d.runtime_version);
        h.set("X-Stainless-Os", &d.os);
        h.set("X-Stainless-Arch", &d.arch);
    };
    match device_profile {
        Some(d) => set_profile(&mut h, &d),
        None if stabilize => set_profile(&mut h, &Profile::default_for(p.settings)),
        None => legacy_device_headers(&mut h, p),
    }
    custom_headers(&mut h, p);
    if p.first_party {
        h.set("Anthropic-Beta", base.as_str());
        transport(&mut h);
    } else if p.stream {
        transport(&mut h);
    }
    h
}

/// `ApplyClaudeLegacyDeviceHeaders` (device-profile stabilization off, the default).
fn legacy_device_headers(h: &mut GoHeader, p: &Plan<'_>) {
    let d = Profile::default_for(p.settings);
    if p.confirmed {
        let mut ensure_valid = |name: &str, fallback: &str, valid: &dyn Fn(&str) -> bool| {
            let current = h.get(name).trim().to_owned();
            if !current.is_empty() && valid(&current) {
                return;
            }
            let incoming = p
                .incoming
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .trim();
            if !incoming.is_empty() && valid(incoming) {
                h.set(name, incoming);
            } else {
                h.set(name, fallback);
            }
        };
        let ua = p
            .incoming
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .trim();
        // A newer native release keeps its own SDK and runtime versions, so they match
        // its User-Agent (docs/DIFFERENCES-FROM-GO.md); otherwise Go's baseline values.
        let newer = profile::newer_than_baseline(ua, p.settings);
        ensure_valid("X-Stainless-Runtime-Version", &d.runtime_version, &|v| {
            v == d.runtime_version || (newer && profile::runtime_version_ok(v))
        });
        ensure_valid("X-Stainless-Package-Version", &d.package_version, &|v| {
            v == d.package_version || (newer && profile::package_version_ok(v))
        });
        ensure_valid("X-Stainless-Os", &profile::host_os(), &|_| true);
        ensure_valid("X-Stainless-Arch", &profile::host_arch(), &|_| true);
        if profile::plausible_user_agent(ua, p.settings) {
            h.set("User-Agent", ua);
            return;
        }
    }
    h.set("X-Stainless-Runtime-Version", &d.runtime_version);
    h.set("X-Stainless-Package-Version", &d.package_version);
    h.set("X-Stainless-Os", &d.os);
    h.set("X-Stainless-Arch", &d.arch);
    h.set("User-Agent", &d.user_agent);
}

/// `copyClaudeCallerFingerprintHeaders`.
fn copy_caller_fingerprint(h: &mut GoHeader, incoming: &HeaderMap, confirmed: bool) {
    for name in incoming.keys() {
        let n = name.as_str();
        let fingerprint = matches!(
            n,
            "accept"
                | "accept-encoding"
                | "user-agent"
                | "x-app"
                | "x-client-request-id"
                | "x-client-app"
                | "x-anthropic-additional-protection"
        ) || n.starts_with("anthropic-")
            || n.starts_with("x-stainless-")
            || n.starts_with("x-claude-code-")
            || n.starts_with("x-claude-remote-");
        if !fingerprint || (!confirmed && (n.starts_with("x-claude-code-") || n.starts_with("x-claude-remote-"))) {
            continue;
        }
        h.del(n);
        for value in incoming.get_all(name).iter().filter_map(|v| v.to_str().ok()) {
            h.add(n, value);
        }
    }
}

/// `util.ApplyCustomHeadersFromAttrs` through the shared `cpa_common::headers`.
fn custom_headers(h: &mut GoHeader, p: &Plan<'_>) {
    let session = Some(p.cpa_session).filter(|s| !s.is_empty());
    for (name, value) in cpa_common::headers::custom_headers(p.attributes, p.incoming, session) {
        h.set(&name, value);
    }
}

/// Final `(name, value)` pairs in wire order and casing, excluding Content-Length,
/// which the transport writes at the position given by `order`.
pub(crate) fn wire(mut h: GoHeader, first_party: bool, count_tokens: bool) -> (Vec<(String, String)>, Vec<String>) {
    if first_party {
        for (from, to) in WIRE_CASING {
            h.rename(from, to);
        }
    }
    // net/http: Host, User-Agent, Content-Length, then byte-sorted remaining keys.
    let mut keys: Vec<&(String, Vec<String>)> = h.entries().iter().filter(|(k, _)| k != "User-Agent").collect();
    keys.sort_by(|a, b| a.0.cmp(&b.0));
    let mut natural: Vec<String> = vec!["Host".into()];
    if !h.get("User-Agent").is_empty() {
        natural.push("User-Agent".into());
    }
    natural.push("Content-Length".into());
    natural.extend(keys.iter().map(|(k, _)| k.clone()));
    let order: Vec<String> = if first_party {
        let listed: Vec<&str> = MESSAGES_ORDER
            .iter()
            .copied()
            .filter(|n| !(count_tokens && *n == "X-Stainless-Timeout"))
            .collect();
        let present = |n: &str| natural.iter().any(|x| x.eq_ignore_ascii_case(n));
        let mut order: Vec<String> = listed.iter().filter(|n| present(n)).map(|n| (*n).to_owned()).collect();
        order.extend(
            natural
                .iter()
                .filter(|x| !listed.iter().any(|n| n.eq_ignore_ascii_case(x)))
                .cloned(),
        );
        order
    } else {
        natural
    };
    let mut pairs = Vec::new();
    for name in &order {
        // Host appears only when a custom header set it (Go mirrors it into req.Host).
        if name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        if let Some((_, values)) = h.entries().iter().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
            for v in values {
                pairs.push((name.clone(), v.clone()));
            }
        }
    }
    (pairs, order)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_matches_go() {
        assert_eq!(canonical("x-stainless-os"), "X-Stainless-Os");
        assert_eq!(canonical("anthropic-BETA"), "Anthropic-Beta");
        assert_eq!(canonical("bad header"), "bad header");
        assert_eq!(canonical("x_y"), "X_y");
    }

    #[test]
    fn passthrough_skips_the_cached_session_like_go() {
        // Neither fingerprinted nor confirmed: the builder returns before it reads
        // the cached session, so the caller must not look it up.
        assert!(!needs_cached_session(false, false, ""));
        assert!(needs_cached_session(true, false, ""));
        assert!(needs_cached_session(false, true, " "));
        assert!(!needs_cached_session(true, true, "explicit-session"));
    }

    #[test]
    fn harness_unconfirmed_oauth_messages_headers_match_go_capture() {
        let settings = Settings::default();
        let incoming: HeaderMap = [
            ("authorization", "Bearer fixture-client-key"),
            ("content-type", "application/json"),
            ("anthropic-version", "2023-06-01"),
            ("x-claude-code-session-id", "11111111-2222-4333-8444-555555555555"),
        ]
        .into_iter()
        .map(|(k, v)| (k.parse().unwrap(), v.parse().unwrap()))
        .collect();
        let attributes = Default::default();
        let body = r#"{"model":"claude-sonnet-4-6","diagnostics":{"previous_message_id":null}}"#;
        let h = build(&Plan {
            api_key: "sk-ant-oat01-FAKE",
            bearer: true,
            first_party: true,
            count_tokens: false,
            stream: false,
            extra_betas: &[],
            body,
            incoming: &incoming,
            confirmed: false,
            helper: false,
            cli_fingerprint: true,
            use_oauth_betas: true,
            session_id: "dd01238e-cdb5-5572-8f27-a28d98fe9075",
            settings: &settings,
            attributes: &attributes,
            cpa_session: "",
            device_profile: None,
            cached_session_id: "",
        });
        let (pairs, order) = wire(h, true, false);
        let names: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            [
                "Accept",
                "Authorization",
                "Content-Type",
                "User-Agent",
                "X-Claude-Code-Session-Id",
                "X-Stainless-Arch",
                "X-Stainless-Lang",
                "X-Stainless-OS",
                "X-Stainless-Package-Version",
                "X-Stainless-Retry-Count",
                "X-Stainless-Runtime",
                "X-Stainless-Runtime-Version",
                "X-Stainless-Timeout",
                "anthropic-beta",
                "anthropic-dangerous-direct-browser-access",
                "anthropic-version",
                "x-app",
                "x-client-request-id",
                "Connection",
                "Accept-Encoding"
            ]
        );
        assert_eq!(order[19], "Host");
        assert_eq!(order[21], "Content-Length");
        let get = |n: &str| pairs.iter().find(|(k, _)| k == n).unwrap().1.clone();
        assert_eq!(get("User-Agent"), "claude-cli/2.1.280 (external, cli)");
        assert_eq!(get("X-Stainless-OS"), "MacOS");
        assert_eq!(get("Accept-Encoding"), "gzip, deflate, br, zstd");
    }
}

#[cfg(test)]
mod version_tests {
    use super::*;

    /// The upstream `User-Agent` and Stainless versions for a confirmed caller sending
    /// `user_agent` with its own SDK and runtime versions.
    fn sent(user_agent: &str) -> (String, String, String) {
        let settings = Settings::default();
        let incoming: HeaderMap = [
            ("user-agent", user_agent),
            ("x-stainless-package-version", "0.120.4"),
            ("x-stainless-runtime-version", "v27.0.1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.parse().unwrap(), v.parse().unwrap()))
        .collect();
        let attributes = Default::default();
        let h = build(&Plan {
            api_key: "sk-ant-oat01-FAKE",
            bearer: true,
            first_party: true,
            count_tokens: false,
            stream: false,
            extra_betas: &[],
            body: r#"{"model":"claude-sonnet-4-6"}"#,
            incoming: &incoming,
            confirmed: true,
            helper: false,
            cli_fingerprint: true,
            use_oauth_betas: true,
            session_id: "dd01238e-cdb5-5572-8f27-a28d98fe9075",
            settings: &settings,
            attributes: &attributes,
            cpa_session: "",
            device_profile: None,
            cached_session_id: "",
        });
        let (pairs, _) = wire(h, true, false);
        let get = |n: &str| pairs.iter().find(|(k, _)| k == n).unwrap().1.clone();
        (
            get("User-Agent"),
            get("X-Stainless-Package-Version"),
            get("X-Stainless-Runtime-Version"),
        )
    }

    #[test]
    fn newer_native_releases_keep_their_own_versions() {
        // A newer minor release is forwarded with its own SDK and runtime versions.
        assert_eq!(
            sent("claude-cli/2.2.3 (external, cli)"),
            (
                "claude-cli/2.2.3 (external, cli)".into(),
                "0.120.4".into(),
                "v27.0.1".into()
            )
        );
        // The baseline release itself keeps Go's baseline SDK and runtime versions.
        assert_eq!(
            sent("claude-cli/2.1.280 (external, cli)"),
            (
                "claude-cli/2.1.280 (external, cli)".into(),
                "0.112.1".into(),
                "v26.3.0".into()
            )
        );
        // Another major, or an older release, gets the whole baseline identity.
        for refused in ["claude-cli/3.0.0 (external, cli)", "claude-cli/2.1.220 (external, cli)"] {
            assert_eq!(
                sent(refused),
                (
                    "claude-cli/2.1.280 (external, cli)".into(),
                    "0.112.1".into(),
                    "v26.3.0".into()
                ),
                "{refused}"
            );
        }
    }
}
