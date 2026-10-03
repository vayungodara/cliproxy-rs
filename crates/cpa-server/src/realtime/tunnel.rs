//! Go tcp_proxy.go: the upstream half of a proxied media session runs ICE-TCP through
//! the credential's proxy.
//!
//! The upstream peer only has loopback candidates. Its answer keeps nothing but TCP
//! passive host candidates on public addresses, port 443, and each one is rewritten to a
//! local listener. A connection to that listener must first send the ICE binding request
//! the session expects (user `remote:local`, signed with the remote password). Only then
//! does the tunnel dial the fixed upstream address through the proxy and splice the two.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use rtc::sdp::SessionDescription;
use rtc::stun::attributes::{ATTR_FINGERPRINT, ATTR_MESSAGE_INTEGRITY, ATTR_MESSAGE_INTEGRITY_SHA256, ATTR_USERNAME};
use rtc::stun::fingerprint::FINGERPRINT;
use rtc::stun::message::{BINDING_REQUEST, Message};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{AbortHandle, JoinSet};

use super::dialer::{Dial, quote};

/// `maxUpstreamICECandidates`, `maxProxiedTCPCandidates`, `maxUnauthenticatedTCPConns`,
/// `maxInitialSTUNFrameSize`, `stunMessageHeaderSize`.
const MAX_CANDIDATES: usize = 64;
const MAX_TUNNELS: usize = 16;
const MAX_UNAUTHENTICATED: usize = 4;
const MAX_FRAME: usize = 4096;
const STUN_HEADER: usize = 20;

/// `nonRoutableProxyTargetPrefixes`.
const V4_PREFIXES: [(Ipv4Addr, u8); 15] = [
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(192, 0, 2, 0), 24),
    (Ipv4Addr::new(192, 88, 99, 0), 24),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(198, 51, 100, 0), 24),
    (Ipv4Addr::new(203, 0, 113, 0), 24),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
];
const V6_PREFIXES: [(Ipv6Addr, u8); 14] = [
    (Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 96),
    // IPv4-translated (RFC 2765), not the IPv4-mapped range.
    (Ipv6Addr::new(0, 0, 0, 0, 0xffff, 0, 0, 0), 96),
    (Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96),
    (Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48),
    (Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
    (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
    (Ipv6Addr::new(0x5f00, 0, 0, 0, 0, 0, 0, 0), 16),
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
];

/// `isPublicProxyTarget` with netip semantics: the predicates (`IsGlobalUnicast`,
/// `IsPrivate`, ...) look through an IPv4-mapped address, the prefix list does not.
/// Callers unmap first, as Go does.
pub(super) fn is_public_target(ip: IpAddr) -> bool {
    let special = match ip.to_canonical() {
        IpAddr::V4(v) => {
            v.is_unspecified()
                || v.is_broadcast()
                || v.is_loopback()
                || v.is_private()
                || v.is_link_local()
                || v.is_multicast()
        }
        IpAddr::V6(v) => {
            v.is_unspecified()
                || v.is_loopback()
                || v.is_unique_local()
                || v.is_unicast_link_local()
                || v.is_multicast()
        }
    };
    let listed = match ip {
        IpAddr::V4(v) => V4_PREFIXES
            .iter()
            .any(|(net, len)| u32::from(v) >> (32 - len) == u32::from(*net) >> (32 - len)),
        IpAddr::V6(v) => V6_PREFIXES
            .iter()
            .any(|(net, len)| u128::from(v) >> (128 - len) == u128::from(*net) >> (128 - len)),
    };
    !special && !listed
}

/// One rewritten candidate and the tunnel behind its listener. Dropping it closes the
/// listener and every connection (Go `tcpCandidateTunnel.Close`).
pub(super) struct Tunnel {
    pub target: SocketAddr,
    pub listener: SocketAddr,
    /// The rewritten candidate, its media's mid and index (webrtc-rs dials a TCP passive
    /// candidate only when it is added on its own).
    pub candidate: String,
    pub mid: Option<String>,
    pub mline: u16,
    task: AbortHandle,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `prepareProxiedUpstreamAnswer`'s result.
pub(super) struct Prepared {
    pub sdp: String,
    pub tunnels: Vec<Tunnel>,
}

/// Called once a tunnel forwarded its first frame (`logForwardingStarted`).
pub(super) type OnForwarding = Arc<dyn Fn() + Send + Sync>;

struct Plan {
    media: usize,
    attribute: usize,
    fields: Vec<String>,
    target: SocketAddr,
}

/// `prepareProxiedUpstreamAnswer`: the answer restricted to proxied candidates, each
/// pointing at a new local tunnel. Must run inside a Tokio runtime.
pub(super) fn prepare_answer(
    answer: &str,
    local_offer: &str,
    dialer: Arc<dyn Dial>,
    on_forwarding: OnForwarding,
) -> Result<Prepared, String> {
    let mut remote = parse_sdp(answer).map_err(|e| format!("parse upstream WebRTC answer for TCP proxy: {e}"))?;
    let local = parse_sdp(local_offer).map_err(|e| format!("parse upstream WebRTC offer for TCP proxy: {e}"))?;
    let (remote_ufrag, remote_password) =
        credentials(&remote).map_err(|e| format!("read upstream WebRTC answer ICE credentials: {e}"))?;
    let (local_ufrag, _) =
        credentials(&local).map_err(|e| format!("read upstream WebRTC offer ICE credentials: {e}"))?;

    let mut plans = Vec::new();
    let mut count = 0;
    for (media_index, media) in remote.media_descriptions.iter_mut().enumerate() {
        let mut kept = Vec::with_capacity(media.attributes.len());
        for attribute in std::mem::take(&mut media.attributes) {
            if !attribute.is_ice_candidate() {
                kept.push(attribute);
                continue;
            }
            count += 1;
            if count > MAX_CANDIDATES {
                return Err(format!(
                    "upstream WebRTC answer exceeds the {MAX_CANDIDATES} candidate limit"
                ));
            }
            let Some((fields, target)) = candidate_plan(attribute.value.as_deref().unwrap_or_default())? else {
                continue;
            };
            if plans.len() >= MAX_TUNNELS {
                return Err(format!(
                    "upstream WebRTC answer exceeds the {MAX_TUNNELS} TCP candidate proxy limit"
                ));
            }
            plans.push(Plan {
                media: media_index,
                attribute: kept.len(),
                fields,
                target,
            });
            kept.push(attribute);
        }
        media.attributes = kept;
    }
    if plans.is_empty() {
        return Err("upstream WebRTC answer has no supported public TCP passive candidate on port 443".into());
    }

    let expected_user = format!("{remote_ufrag}:{local_ufrag}");
    let mut tunnels = Vec::with_capacity(plans.len());
    for mut plan in plans {
        // Dropping `tunnels` on an error closes the ones already open.
        let (address, task) = open(
            plan.target,
            dialer.clone(),
            &expected_user,
            &remote_password,
            on_forwarding.clone(),
        )?;
        plan.fields[4] = address.ip().to_string();
        plan.fields[5] = address.port().to_string();
        let candidate = plan.fields.join(" ");
        let media = &mut remote.media_descriptions[plan.media];
        media.attributes[plan.attribute].value = Some(candidate.clone());
        tunnels.push(Tunnel {
            target: plan.target,
            listener: address,
            candidate,
            mid: media.attribute("mid").flatten().map(str::to_owned),
            mline: plan.media as u16,
            task,
        });
    }
    Ok(Prepared {
        sdp: remote.marshal(),
        tunnels,
    })
}

fn parse_sdp(sdp: &str) -> Result<SessionDescription, String> {
    SessionDescription::unmarshal(&mut std::io::Cursor::new(sdp.as_bytes())).map_err(|e| e.to_string())
}

/// `newTCPCandidateTunnel`: a loopback listener of the target's family and the task that
/// serves it; aborting the task closes everything.
pub(super) fn open(
    target: SocketAddr,
    dialer: Arc<dyn Dial>,
    user: &str,
    password: &str,
    on_forwarding: OnForwarding,
) -> Result<(SocketAddr, AbortHandle), String> {
    if !is_public_target(target.ip()) || target.port() != 443 {
        return Err("Codex live TCP proxy target is not allowed".into());
    }
    if user.trim().is_empty() || password.trim().is_empty() {
        return Err("Codex live TCP proxy tunnel configuration is incomplete".into());
    }
    let ip: IpAddr = if target.is_ipv6() {
        Ipv6Addr::LOCALHOST.into()
    } else {
        Ipv4Addr::LOCALHOST.into()
    };
    let listener = std::net::TcpListener::bind((ip, 0))
        .and_then(|l| l.set_nonblocking(true).map(|()| l))
        .and_then(TcpListener::from_std)
        .map_err(|e| format!("listen for Codex live TCP proxy candidate: {e}"))?;
    let address = listener
        .local_addr()
        .map_err(|_| "Codex live TCP proxy listener returned an invalid address".to_owned())?;
    let task = tokio::spawn(serve(
        listener,
        target,
        dialer,
        user.to_owned(),
        password.to_owned(),
        on_forwarding,
    ))
    .abort_handle();
    Ok((address, task))
}

/// `bundledICECredentials`.
fn credentials(description: &SessionDescription) -> Result<(String, String), &'static str> {
    let session = |key| description.attribute(key).map(String::as_str).unwrap_or_default();
    let mut selected: Option<(String, String)> = None;
    for media in &description.media_descriptions {
        let value = |key| media.attribute(key).map_or(session(key), Option::unwrap_or_default);
        let (ufrag, password) = (value("ice-ufrag").trim(), value("ice-pwd").trim());
        if ufrag.is_empty() && password.is_empty() {
            continue;
        }
        if ufrag.is_empty() || password.is_empty() {
            return Err("SDP contains incomplete ICE credentials");
        }
        match &selected {
            None => selected = Some((ufrag.to_owned(), password.to_owned())),
            Some((u, p)) if u == ufrag && p == password => {}
            Some(_) => return Err("SDP contains inconsistent bundled ICE credentials"),
        }
    }
    let (ufrag, password) = selected.unwrap_or_else(|| {
        (
            session("ice-ufrag").trim().to_owned(),
            session("ice-pwd").trim().to_owned(),
        )
    });
    if ufrag.is_empty() || password.is_empty() {
        return Err("SDP is missing ICE credentials");
    }
    Ok((ufrag, password))
}

/// `proxiedTCPCandidatePlan`: the candidate's fields and fixed target when it is a TCP
/// passive host candidate for component 1; `None` drops it from the answer.
fn candidate_plan(raw: &str) -> Result<Option<(Vec<String>, SocketAddr)>, String> {
    let trimmed = raw.trim();
    let candidate = parse_candidate(trimmed).map_err(|e| format!("parse upstream WebRTC candidate: {e}"))?;
    if !candidate.tcp || !candidate.passive || candidate.component != 1 || !candidate.host {
        return Ok(None);
    }
    if candidate.port != 443 {
        return Err(format!(
            "upstream WebRTC TCP proxy candidate uses disallowed port {}",
            candidate.port
        ));
    }
    let Some(ip) = candidate.ip else {
        return Err("upstream WebRTC TCP proxy candidate address must be an IP".into());
    };
    let ip = ip.to_canonical();
    if !is_public_target(ip) {
        return Err("upstream WebRTC TCP proxy candidate address must be globally routable".into());
    }
    let fields: Vec<String> = trimmed.split_whitespace().map(str::to_owned).collect();
    if fields.len() < 8 {
        return Err("upstream WebRTC TCP proxy candidate is malformed".into());
    }
    Ok(Some((fields, SocketAddr::new(ip, candidate.port))))
}

/// What the plan reads from pion's `ice.UnmarshalCandidate`.
struct Candidate {
    tcp: bool,
    passive: bool,
    host: bool,
    component: u16,
    port: u16,
    ip: Option<IpAddr>,
}

/// pion/ice v4 `UnmarshalCandidate`, reduced to the fields above; accepts and rejects
/// what pion does, with pion's messages.
fn parse_candidate(raw: &str) -> Result<Candidate, String> {
    let raw = raw.strip_prefix("candidate:").unwrap_or(raw);
    let short = |what: &str| format!("attribute not long enough to be ICE candidate: expected {what} in {raw}");
    let mut pos = 0;
    char_token(raw, &mut pos, 32).map_err(|e| format!("failed to parse foundation: {e} in {raw}"))?;
    if pos >= raw.len() {
        return Err(short("component"));
    }
    let component = digit_token(raw, &mut pos, 5).map_err(|e| format!("failed to parse component: {e} in {raw}"))?;
    if pos >= raw.len() {
        return Err(short("transport"));
    }
    let network = string_token(raw, &mut pos);
    if pos >= raw.len() {
        return Err(short("priority"));
    }
    digit_token(raw, &mut pos, 10).map_err(|e| format!("failed to parse priority: {e} in {raw}"))?;
    if pos >= raw.len() {
        return Err(short("address"));
    }
    let address = string_token(raw, &mut pos);
    let address = address.split_once('%').map_or(address, |(before, _)| before);
    if pos >= raw.len() {
        return Err(short("port"));
    }
    let port = port_token(raw, &mut pos).map_err(|e| format!("failed to parse port: {e} in {raw}"))?;
    let key = string_token(raw, &mut pos);
    if key != "typ" {
        return Err(format!("unknown candidate typ ({key})"));
    }
    if pos >= raw.len() {
        return Err(short("candidate type"));
    }
    let typ = string_token(raw, &mut pos);
    relative_addresses(raw, &mut pos)?;
    let mut passive = false;
    if pos < raw.len() {
        let tcp_type = extensions(&raw[pos..]).map_err(|e| format!("failed to parse extension: {e}"))?;
        if !tcp_type.is_empty() {
            passive = match tcp_type.to_ascii_lowercase().as_str() {
                "passive" => true,
                "active" | "so" => false,
                _ => {
                    return Err(format!(
                        "failed to parse TCP type: invalid or unsupported TCPtype {tcp_type}"
                    ));
                }
            };
        }
    }
    let host = match typ {
        "host" => true,
        "srflx" | "prflx" | "relay" => false,
        other => return Err(format!("unknown candidate typ ({other})")),
    };
    // `NewCandidateHost` leaves mDNS names unresolved and calls them UDP4.
    if host && (address.ends_with(".local") || address.ends_with(".invalid")) {
        return Ok(Candidate {
            tcp: false,
            passive,
            host,
            component: component as u16,
            port,
            ip: None,
        });
    }
    let ip = parse_addr(address)?;
    let lower = network.to_ascii_lowercase();
    let tcp = if lower.starts_with("udp") {
        false
    } else if lower.starts_with("tcp") {
        true
    } else {
        return Err(format!(
            "unable to determine networkType from {network} {}",
            ip.to_canonical()
        ));
    };
    Ok(Candidate {
        tcp,
        passive,
        host,
        // pion stores `uint16(component)`, wrapping five-digit values.
        component: component as u16,
        port,
        ip: Some(ip),
    })
}

/// `readCandidateCharToken`.
fn char_token<'a>(raw: &'a str, pos: &mut usize, limit: usize) -> Result<&'a str, String> {
    let start = *pos;
    for (i, c) in raw[start..].char_indices() {
        if c == ' ' {
            *pos = start + i + 1;
            return Ok(&raw[start..start + i]);
        }
        if i == limit {
            return Err(format!("token too long: {} expected 1x{limit}", &raw[start..start + i]));
        }
        if !(c.is_ascii_alphanumeric() || c == '+' || c == '/') {
            return Err(format!("invalid ice-char token: {c}"));
        }
    }
    *pos = raw.len();
    Ok(&raw[start..])
}

/// `readCandidateStringToken`.
fn string_token<'a>(raw: &'a str, pos: &mut usize) -> &'a str {
    let start = *pos;
    match raw[start..].find(' ') {
        Some(i) => {
            *pos = start + i + 1;
            &raw[start..start + i]
        }
        None => {
            *pos = raw.len();
            &raw[start..]
        }
    }
}

/// `readCandidateDigitToken`.
fn digit_token(raw: &str, pos: &mut usize, limit: usize) -> Result<u64, String> {
    let start = *pos;
    let mut value: u64 = 0;
    for (i, c) in raw[start..].char_indices() {
        if c == ' ' {
            *pos = start + i + 1;
            return Ok(value);
        }
        if i == limit {
            return Err(format!("token too long: {} expected 1x{limit}", &raw[start..start + i]));
        }
        if !c.is_ascii_digit() {
            return Err(format!("invalid digit token: {c}"));
        }
        value = value * 10 + u64::from(c as u8 - b'0');
    }
    *pos = raw.len();
    Ok(value)
}

/// `readCandidatePort`.
fn port_token(raw: &str, pos: &mut usize) -> Result<u16, String> {
    let port = digit_token(raw, pos, 5)?;
    u16::try_from(port).map_err(|_| format!("invalid RFC 4566 port {port}"))
}

/// `tryReadRelativeAddrs`; only advances when the next token is `raddr`.
fn relative_addresses(raw: &str, pos: &mut usize) -> Result<(), String> {
    let mut next = *pos;
    if string_token(raw, &mut next) != "raddr" {
        return Ok(());
    }
    let fail = |what: &str| format!("failed to parse related addresses: expected {what} in {raw}");
    if next >= raw.len() {
        return Err(fail("raddr value"));
    }
    string_token(raw, &mut next);
    if next >= raw.len() {
        return Err(fail("rport"));
    }
    if string_token(raw, &mut next) != "rport" {
        return Err(fail("rport"));
    }
    if next >= raw.len() {
        return Err(fail("rport value"));
    }
    port_token(raw, &mut next).map_err(|e| format!("failed to parse related addresses: {e}"))?;
    *pos = next;
    Ok(())
}

/// `unmarshalCandidateExtensions`; returns the raw `tcptype` value.
fn extensions(raw: &str) -> Result<String, String> {
    if raw.starts_with(' ') {
        return Err(format!("failed to parse extension: unexpected space {raw}"));
    }
    let mut tcp_type = String::new();
    let mut i = 0;
    while i < raw.len() {
        let key = byte_string(raw, &mut i).map_err(|e| format!("failed to parse extension: failed to read key {e}"))?;
        let mut value = "";
        if i < raw.len() {
            value =
                byte_string(raw, &mut i).map_err(|e| format!("failed to parse extension: failed to read value {e}"))?;
        }
        if key == "tcptype" {
            value.clone_into(&mut tcp_type);
        }
    }
    Ok(tcp_type)
}

/// `readCandidateByteString`.
fn byte_string<'a>(raw: &'a str, pos: &mut usize) -> Result<&'a str, String> {
    let start = *pos;
    for (i, c) in raw[start..].char_indices() {
        if c == ' ' {
            *pos = start + i + 1;
            return Ok(&raw[start..start + i]);
        }
        if matches!(c, '\0' | '\n' | '\r') || u32::from(c) > 0xff {
            return Err(format!("invalid byte-string character: {c}"));
        }
    }
    *pos = raw.len();
    Ok(&raw[start..])
}

/// Go 1.26 `netip.ParseAddr`, with its error messages (they reach the client's 502).
fn parse_addr(input: &str) -> Result<IpAddr, String> {
    let error = |(msg, at): (&str, Option<&str>)| match at {
        Some(at) => format!(
            "ParseAddr({}): {msg} (at {})",
            quote(input.as_bytes()),
            quote(at.as_bytes())
        ),
        None => format!("ParseAddr({}): {msg}", quote(input.as_bytes())),
    };
    match input.bytes().find(|b| matches!(b, b'.' | b':' | b'%')) {
        Some(b'.') => ipv4_fields(input, 0, input.len())
            .map(|f| IpAddr::V4(f.into()))
            .map_err(error),
        Some(b':') => parse_ipv6(input).map(IpAddr::V6).map_err(error),
        Some(_) => Err(error(("missing IPv6 address", None))),
        None => Err(error(("unable to parse IP", None))),
    }
}

type AddrError<'a> = (&'static str, Option<&'a str>);

/// `parseIPv4Fields` over `input[off..end]`.
fn ipv4_fields(input: &str, off: usize, end: usize) -> Result<[u8; 4], AddrError<'_>> {
    let s = &input[off..end];
    let bytes = s.as_bytes();
    let mut fields = [0u8; 4];
    let (mut value, mut pos, mut digits) = (0u32, 0, 0);
    for (i, &c) in bytes.iter().enumerate() {
        match c {
            b'0'..=b'9' => {
                if digits == 1 && value == 0 {
                    return Err(("IPv4 field has octet with leading zero", None));
                }
                value = value * 10 + u32::from(c - b'0');
                digits += 1;
                if value > 255 {
                    return Err(("IPv4 field has value >255", None));
                }
            }
            b'.' => {
                if i == 0 || i == bytes.len() - 1 || bytes[i - 1] == b'.' {
                    return Err(("IPv4 field must have at least one digit", Some(&s[i..])));
                }
                if pos == 3 {
                    return Err(("IPv4 address too long", None));
                }
                fields[pos] = value as u8;
                pos += 1;
                value = 0;
                digits = 0;
            }
            _ => return Err(("unexpected character", Some(&s[i..]))),
        }
    }
    if pos < 3 {
        return Err(("IPv4 address too short", None));
    }
    fields[3] = value as u8;
    Ok(fields)
}

/// `parseIPv6` (a zone is accepted and dropped; candidates arrive without one).
fn parse_ipv6(input: &str) -> Result<Ipv6Addr, AddrError<'_>> {
    let (mut s, zone) = match input.find('%') {
        Some(i) if i + 1 == input.len() => return Err(("zone must be a non-empty string", None)),
        Some(i) => (&input[..i], &input[i + 1..]),
        None => (input, ""),
    };
    let mut ip = [0u8; 16];
    let mut ellipsis: Option<usize> = None;
    if let Some(rest) = s.strip_prefix("::") {
        ellipsis = Some(0);
        s = rest;
        if s.is_empty() {
            return Ok(Ipv6Addr::UNSPECIFIED);
        }
    }
    let mut i = 0;
    while i < 16 {
        let bytes = s.as_bytes();
        let (mut off, mut acc) = (0, 0u32);
        while off < bytes.len() {
            let digit = match bytes[off] {
                c @ b'0'..=b'9' => c - b'0',
                c @ b'a'..=b'f' => c - b'a' + 10,
                c @ b'A'..=b'F' => c - b'A' + 10,
                _ => break,
            };
            acc = (acc << 4) + u32::from(digit);
            if off > 3 {
                return Err(("each group must have 4 or less digits", Some(s)));
            }
            if acc > 0xffff {
                return Err(("IPv6 field has value >=2^16", Some(s)));
            }
            off += 1;
        }
        if off == 0 {
            return Err(("each colon-separated field must have at least one digit", Some(s)));
        }
        if bytes.get(off) == Some(&b'.') {
            if ellipsis.is_none() && i != 12 {
                return Err((
                    "embedded IPv4 address must replace the final 2 fields of the address",
                    Some(s),
                ));
            }
            if i + 4 > 16 {
                return Err((
                    "too many hex fields to fit an embedded IPv4 at the end of the address",
                    Some(s),
                ));
            }
            let end = input.len() - if zone.is_empty() { 0 } else { zone.len() + 1 };
            ip[i..i + 4].copy_from_slice(&ipv4_fields(input, end - s.len(), end)?);
            s = "";
            i += 4;
            break;
        }
        ip[i] = (acc >> 8) as u8;
        ip[i + 1] = acc as u8;
        i += 2;
        s = &s[off..];
        if s.is_empty() {
            break;
        }
        if !s.starts_with(':') {
            return Err(("unexpected character, want colon", Some(s)));
        }
        if s.len() == 1 {
            return Err(("colon must be followed by more characters", Some(s)));
        }
        s = &s[1..];
        if s.starts_with(':') {
            if ellipsis.is_some() {
                return Err(("multiple :: in address", Some(s)));
            }
            ellipsis = Some(i);
            s = &s[1..];
            if s.is_empty() {
                break;
            }
        }
    }
    if !s.is_empty() {
        return Err(("trailing garbage after address", Some(s)));
    }
    match ellipsis {
        _ if i == 16 && ellipsis.is_some() => {
            return Err(("the :: must expand to at least one field of zeros", None));
        }
        None if i < 16 => return Err(("address string too short", None)),
        Some(at) if i < 16 => {
            let n = 16 - i;
            ip.copy_within(at..i, at + n);
            ip[at..at + n].fill(0);
        }
        _ => {}
    }
    Ok(Ipv6Addr::from(ip))
}

/// One tunnel's life: authenticate the first connection that proves it belongs to this
/// session, then dial the fixed target through the proxy and splice.
async fn serve(
    listener: TcpListener,
    target: SocketAddr,
    dialer: Arc<dyn Dial>,
    user: String,
    password: String,
    on_forwarding: OnForwarding,
) {
    let mut validating: JoinSet<Option<(TcpStream, Vec<u8>)>> = JoinSet::new();
    let (client, frame) = loop {
        tokio::select! {
            biased;
            joined = validating.join_next(), if !validating.is_empty() => {
                if let Some(Ok(Some(found))) = joined {
                    break found;
                }
            }
            accepted = listener.accept() => {
                let Ok((conn, _)) = accepted else {
                    tracing::warn!("codex live TCP proxy: accept candidate connection failed");
                    return;
                };
                if validating.len() >= MAX_UNAUTHENTICATED {
                    tracing::warn!("codex live TCP proxy: rejected excess unauthenticated candidate connection");
                    continue;
                }
                let (user, password) = (user.clone(), password.clone());
                validating.spawn(async move {
                    let mut conn = conn;
                    match read_validated_frame(&mut conn, &user, &password).await {
                        Ok(frame) => Some((conn, frame)),
                        Err(e) => {
                            tracing::warn!(error = %e, "codex live TCP proxy: rejected unauthenticated candidate connection");
                            None
                        }
                    }
                });
            }
        }
    };
    // Claimed: one connection per tunnel. The listener and the other unauthenticated
    // connections close here.
    drop(listener);
    drop(validating);
    let mut upstream = match dialer.dial(target).await {
        Ok(upstream) => upstream,
        Err(e) => {
            tracing::warn!(error = %e, "codex live TCP proxy: connect fixed upstream candidate failed");
            return;
        }
    };
    if let Err(e) = upstream.write_all(&frame).await {
        tracing::warn!(error = %e, "codex live TCP proxy: forward authenticated ICE frame failed");
        return;
    }
    on_forwarding();
    let (mut client_read, mut client_write) = client.into_split();
    let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);
    // Either direction ending closes both (Go closes both connections).
    tokio::select! {
        _ = tokio::io::copy(&mut client_read, &mut upstream_write) => {}
        _ = tokio::io::copy(&mut upstream_read, &mut client_write) => {}
    }
}

/// `readValidatedICEBindingFrame`: the first RFC 4571 frame, which must be the session's
/// ICE binding request; returned with its length prefix.
pub(super) async fn read_validated_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    user: &str,
    password: &str,
) -> Result<Vec<u8>, String> {
    let mut header = [0u8; 2];
    reader
        .read_exact(&mut header)
        .await
        .map_err(|e| format!("read ICE-TCP frame header: {e}"))?;
    let size = usize::from(u16::from_be_bytes(header));
    if !(STUN_HEADER..=MAX_FRAME).contains(&size) {
        return Err(format!("invalid initial ICE-TCP STUN frame size {size}"));
    }
    let mut frame = vec![0u8; 2 + size];
    frame[..2].copy_from_slice(&header);
    reader
        .read_exact(&mut frame[2..])
        .await
        .map_err(|e| format!("read ICE-TCP STUN frame: {e}"))?;
    check_binding(&frame[2..], user, password)?;
    Ok(frame)
}

fn check_binding(payload: &[u8], user: &str, password: &str) -> Result<(), String> {
    let mut message = Message::new();
    message
        .unmarshal_binary(payload)
        .map_err(|e| format!("decode initial ICE-TCP STUN message: {e}"))?;
    strict(&mut message);
    if payload.len() != STUN_HEADER + message.length as usize {
        return Err("initial ICE-TCP STUN message contains trailing data".into());
    }
    if message.typ != BINDING_REQUEST {
        return Err(format!(
            "initial ICE-TCP STUN message has unexpected type {}",
            message.typ
        ));
    }
    let username = message
        .get(ATTR_USERNAME)
        .map_err(|e| format!("read initial ICE-TCP STUN username: {e}"))?;
    if username != user.as_bytes() {
        return Err("initial ICE-TCP STUN username does not match the media session".into());
    }
    check_integrity(&message, password.as_bytes())
        .map_err(|e| format!("verify initial ICE-TCP STUN integrity: {e}"))?;
    FINGERPRINT
        .check(&message)
        .map_err(|e| format!("verify initial ICE-TCP STUN fingerprint: {e}"))?;
    Ok(())
}

/// pion/stun v3.1.6 `MessageIntegrity.Check`: HMAC-SHA1 over the message up to the
/// MESSAGE-INTEGRITY attribute, the header length covering the attributes up to and
/// including it. (rtc-stun's older check also counts attributes after it.)
fn check_integrity(message: &Message, key: &[u8]) -> Result<(), String> {
    let tag = message.get(ATTR_MESSAGE_INTEGRITY).map_err(|e| e.to_string())?;
    let mut length = 0;
    for attribute in &message.attributes.0 {
        length += 4 + usize::from(attribute.length).div_ceil(4) * 4;
        if attribute.typ == ATTR_MESSAGE_INTEGRITY {
            break;
        }
    }
    let start = STUN_HEADER + length - 24;
    let mut header = [0u8; STUN_HEADER];
    header.copy_from_slice(&message.raw[..STUN_HEADER]);
    header[2..4].copy_from_slice(&(length as u16).to_be_bytes());
    let provider = rtc::crypto::default_provider().map_err(|e| e.to_string())?;
    let mut mac = provider
        .crypto()
        .new_hmac(rtc::crypto::HmacAlgorithm::Sha1, key)
        .map_err(|e| e.to_string())?;
    mac.verify(&[&header, &message.raw[STUN_HEADER..start]], &tag)
        .map_err(|_| "integrity check failed".to_owned())
}

/// pion/stun `WithStrict(true)` decoding: attributes after MESSAGE-INTEGRITY other than
/// MESSAGE-INTEGRITY-SHA256 and FINGERPRINT are dropped (RFC 8489).
fn strict(message: &mut Message) {
    let (mut seen_mi, mut seen_mi256) = (false, false);
    message.attributes.0.retain(|attribute| {
        let (mi, mi256, fingerprint) = (
            attribute.typ == ATTR_MESSAGE_INTEGRITY,
            attribute.typ == ATTR_MESSAGE_INTEGRITY_SHA256,
            attribute.typ == ATTR_FINGERPRINT,
        );
        let after = (seen_mi && !mi256 && !fingerprint) || (!seen_mi && seen_mi256 && !fingerprint);
        if after {
            return false;
        }
        seen_mi |= mi;
        seen_mi256 |= mi256;
        true
    });
}

#[cfg(test)]
#[path = "tunnel_tests.rs"]
mod tests;
