//! `server.trusted-proxies` and client-IP resolution with Go's exact semantics:
//! config validation (internal/config/trusted_proxies.go), gin v1.10.1
//! `SetTrustedProxies` parsing and `Context.ClientIP` (X-Forwarded-For, then X-Real-IP).
//!
//! Go keeps IPv4 as 4 bytes and everything else as 16 bytes when matching, and a
//! bare IPv4-mapped IPv6 entry becomes a `/32` IPv6 prefix. Both are reproduced.
use std::net::IpAddr;

use anyhow::bail;

/// One parsed `net.IPNet`, already reduced the way `networkNumberAndMask` reduces it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Net {
    network: Vec<u8>,
    mask: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedProxies(Vec<Net>);

/// The header names gin consults, in order, when the peer is trusted.
pub const REMOTE_IP_HEADERS: [&str; 2] = ["X-Forwarded-For", "X-Real-IP"];

const V4_IN_V6: [u8; 12] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff];

fn bytes16(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
        IpAddr::V6(v6) => v6.octets(),
    }
}

/// Go `IP.To4()` on the 16-byte form, falling back to the 16 bytes.
fn go_bytes(ip: IpAddr) -> Vec<u8> {
    let b = bytes16(ip);
    if b[..12] == V4_IN_V6 {
        b[12..].to_vec()
    } else {
        b.to_vec()
    }
}

/// `net.ParseIP` (Go 1.26): no zones, no leading-zero IPv4 octets.
fn parse_ip(text: &str) -> Option<IpAddr> {
    text.parse().ok()
}

/// `net.ParseCIDR`, returning the reduced network/mask pair Go matches against.
fn parse_cidr(text: &str) -> Option<Net> {
    let (addr, bits) = text.split_once('/')?;
    let ip: IpAddr = addr.parse().ok()?;
    let len = if ip.is_ipv4() { 4 } else { 16 };
    if bits.is_empty() || !bits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let ones: usize = bits.parse().ok().filter(|n| *n <= len * 8)?;
    let mask: Vec<u8> = (0..len)
        .map(|i| {
            let set = ones.saturating_sub(i * 8).min(8);
            if set == 0 { 0 } else { 0xffu8 << (8 - set) }
        })
        .collect();
    // IP(addr16).Mask(m)
    let mut base: Vec<u8> = bytes16(ip).to_vec();
    if mask.len() == 4 && base[..12] == V4_IN_V6 {
        base = base[12..].to_vec();
    }
    if base.len() != mask.len() {
        return Some(Net {
            network: Vec::new(),
            mask: Vec::new(),
        });
    }
    let masked: Vec<u8> = base.iter().zip(&mask).map(|(a, m)| a & m).collect();
    // networkNumberAndMask
    let (network, mask) = if masked.len() == 16 && masked[..12] == V4_IN_V6 {
        (masked[12..].to_vec(), mask[12..].to_vec())
    } else {
        (masked, mask)
    };
    Some(Net { network, mask })
}

impl Net {
    fn contains(&self, ip: IpAddr) -> bool {
        let ip = go_bytes(ip);
        ip.len() == self.network.len()
            && ip
                .iter()
                .zip(&self.network)
                .zip(&self.mask)
                .all(|((a, n), m)| a & m == n & m)
    }
}

/// Config load validation. Messages match Go's `validateTrustedProxies`.
pub fn validate(entries: &[String]) -> anyhow::Result<()> {
    for entry in entries {
        if entry.is_empty() || entry.trim() != entry {
            bail!("invalid trusted-proxies entry {entry:?}: expected an IP address or CIDR");
        }
        if parse_ip(entry).is_none() && parse_cidr(entry).is_none() {
            bail!("invalid trusted-proxies entry {entry:?}: invalid CIDR address: {entry}");
        }
    }
    Ok(())
}

impl TrustedProxies {
    /// gin `SetTrustedProxies`. Invalid input disables forwarded headers entirely, as
    /// Go's server does after logging; config validation normally rejects it first.
    pub fn new(entries: &[String]) -> Self {
        let mut nets = Vec::with_capacity(entries.len());
        for entry in entries {
            let net = if entry.contains('/') {
                parse_cidr(entry)
            } else {
                parse_ip(entry).and_then(|ip| {
                    let bits = if go_bytes(ip).len() == 4 { 32 } else { 128 };
                    parse_cidr(&format!("{entry}/{bits}"))
                })
            };
            match net {
                Some(net) => nets.push(net),
                None => return Self::default(),
            }
        }
        Self(nets)
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|net| net.contains(ip))
    }

    /// gin `Context.ClientIP`. `header` returns the first value of a request header.
    /// The result is the peer's canonical text, or a validated forwarded entry verbatim
    /// (after trimming), exactly as Go compares it against "127.0.0.1" and "::1".
    pub fn client_ip<'a>(&self, peer: Option<IpAddr>, header: impl Fn(&str) -> Option<&'a str>) -> String {
        let Some(peer) = peer else {
            return String::new();
        };
        let peer = peer.to_canonical();
        if self.contains(peer) {
            for name in REMOTE_IP_HEADERS {
                if let Some(ip) = header(name).and_then(|v| self.forwarded(v)) {
                    return ip.to_owned();
                }
            }
        }
        peer.to_string()
    }

    /// gin `validateHeader`: walk right to left, skipping trusted hops.
    fn forwarded<'v>(&self, value: &'v str) -> Option<&'v str> {
        if value.is_empty() {
            return None;
        }
        let items: Vec<&str> = value.split(',').collect();
        for (i, item) in items.iter().enumerate().rev() {
            let text = item.trim();
            let ip = parse_ip(text)?;
            if i == 0 || !self.contains(ip) {
                return Some(text);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapped_entries_and_v4_cidrs_follow_go_byte_lengths() {
        let mapped = TrustedProxies::new(&["::ffff:127.0.0.1".into()]);
        // Go turns this into ::/32: IPv6 loopback matches, IPv4 never does.
        assert!(mapped.contains("::1".parse().unwrap()));
        assert!(!mapped.contains("127.0.0.1".parse().unwrap()));
        let v4 = TrustedProxies::new(&["10.0.0.0/8".into()]);
        assert!(v4.contains("::ffff:10.1.2.3".parse().unwrap()));
        assert!(!v4.contains("11.0.0.1".parse().unwrap()));
        let v4_in_v6 = TrustedProxies::new(&["::ffff:10.0.0.0/104".into()]);
        assert!(v4_in_v6.contains("10.9.9.9".parse().unwrap()));
        assert_eq!(TrustedProxies::new(&["bogus".into()]), TrustedProxies::default());
    }
}
