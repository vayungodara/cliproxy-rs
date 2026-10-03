//! Network interfaces and Go's `FilterInterfaces` rules for picking LAN adapters.
use std::net::IpAddr;

/// Go `net.Interface` plus its `Addrs()`, in interface index order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Iface {
    pub index: u32,
    pub name: String,
    pub up: bool,
    pub loopback: bool,
    pub point_to_point: bool,
    pub multicast: bool,
    pub addrs: Vec<IpAddr>,
}

impl Iface {
    pub fn ipv4(&self) -> Option<std::net::Ipv4Addr> {
        self.addrs.iter().find_map(|a| match a {
            IpAddr::V4(v4) => Some(*v4),
            IpAddr::V6(_) => None,
        })
    }
}

/// Go `net.ParseIP` (IPv4-mapped IPv6 behaves as IPv4, as Go's methods treat it).
pub fn parse_go_ip(s: &str) -> Option<IpAddr> {
    s.parse::<IpAddr>().ok().map(|ip| ip.to_canonical())
}

/// Go `net.IP.Equal`.
pub fn ip_equal(a: IpAddr, b: IpAddr) -> bool {
    a.to_canonical() == b.to_canonical()
}

/// Go `extractInterfaceIPs`: every non-loopback, specified address, interface order.
pub fn usable_ips(ifaces: &[Iface]) -> Vec<IpAddr> {
    ifaces
        .iter()
        .flat_map(|i| &i.addrs)
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
        .copied()
        .collect()
}

/// Go `IgnoredInterfacePrefixes`.
const IGNORED_PREFIXES: [&str; 15] = [
    "docker",
    "veth",
    "utun",
    "tailscale",
    "wg",
    "tun",
    "tap",
    "br-",
    "cni",
    "flannel",
    "virbr",
    "vmnet",
    "vboxnet",
    "awdl",
    "llw",
];
const PHYSICAL_PREFIXES: [&str; 12] = [
    "en", "eth", "em", "igb", "ix", "re", "wl", "wlan", "wifi", "wi-fi", "ethernet", "bond",
];

pub fn is_virtual_or_tunnel(name: &str) -> bool {
    IGNORED_PREFIXES.iter().any(|p| name.starts_with(p))
}

pub fn is_likely_physical_lan(name: &str) -> bool {
    PHYSICAL_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Go `matchesAny`: case-insensitive names, `prefix*` wildcards.
pub fn matches_any(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| {
        let p = cpa_common::gostr::lower_bytes(super::go_trim(p).as_bytes());
        match p.strip_suffix('*') {
            _ if p.is_empty() => false,
            Some(prefix) => name.starts_with(prefix),
            None => name == p,
        }
    })
}

/// Go `FilterInterfaces` over a given interface list.
pub fn filter_from(all: Vec<Iface>, include: &[String], exclude: &[String]) -> Vec<Iface> {
    all.into_iter()
        .filter(|i| i.up && !i.loopback && !i.point_to_point && i.multicast)
        .filter(|i| {
            let name = cpa_common::gostr::lower_bytes(i.name.as_bytes());
            if !exclude.is_empty() && matches_any(&name, exclude) {
                return false;
            }
            if include.is_empty() {
                !is_virtual_or_tunnel(&name) && is_likely_physical_lan(&name)
            } else {
                matches_any(&name, include)
            }
        })
        .filter(|i| i.addrs.iter().any(|a| !a.is_loopback() && !a.is_unspecified()))
        .collect()
}

/// Go `FilterInterfaces` on this host.
pub fn filter(include: &[String], exclude: &[String]) -> Result<Vec<Iface>, String> {
    Ok(filter_from(interfaces()?, include, exclude))
}

/// Go `net.Interfaces()` with addresses.
#[cfg(unix)]
pub fn interfaces() -> Result<Vec<Iface>, String> {
    use std::collections::BTreeMap;
    use std::ffi::CStr;

    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list we free below with freeifaddrs.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(format!("route ip+net: {}", std::io::Error::last_os_error()));
    }
    let mut by_index: BTreeMap<u32, Iface> = BTreeMap::new();
    let mut cursor = head;
    while !cursor.is_null() {
        // SAFETY: `cursor` walks the list getifaddrs returned; entries stay valid until
        // freeifaddrs.
        let entry = unsafe { &*cursor };
        cursor = entry.ifa_next;
        if entry.ifa_name.is_null() {
            continue;
        }
        // SAFETY: ifa_name is a NUL-terminated interface name.
        let cname = unsafe { CStr::from_ptr(entry.ifa_name) };
        // SAFETY: if_nametoindex reads the same NUL-terminated name.
        let index = unsafe { libc::if_nametoindex(entry.ifa_name) };
        if index == 0 {
            continue;
        }
        let flags = entry.ifa_flags as libc::c_int;
        let iface = by_index.entry(index).or_insert_with(|| Iface {
            index,
            name: cname.to_string_lossy().into_owned(),
            up: flags & libc::IFF_UP != 0,
            loopback: flags & libc::IFF_LOOPBACK != 0,
            point_to_point: flags & libc::IFF_POINTOPOINT != 0,
            multicast: flags & libc::IFF_MULTICAST != 0,
            addrs: Vec::new(),
        });
        if entry.ifa_addr.is_null() {
            continue;
        }
        // SAFETY: ifa_addr points at a sockaddr whose family says how to read it.
        let addr = unsafe {
            match (*entry.ifa_addr).sa_family as libc::c_int {
                libc::AF_INET => {
                    let sin = &*(entry.ifa_addr as *const libc::sockaddr_in);
                    Some(IpAddr::from(u32::from_be(sin.sin_addr.s_addr).to_be_bytes()))
                }
                libc::AF_INET6 => {
                    let sin6 = &*(entry.ifa_addr as *const libc::sockaddr_in6);
                    Some(IpAddr::from(sin6.sin6_addr.s6_addr))
                }
                _ => None,
            }
        };
        if let Some(addr) = addr {
            iface.addrs.push(addr);
        }
    }
    // SAFETY: `head` came from a successful getifaddrs.
    unsafe { libc::freeifaddrs(head) };
    Ok(by_index.into_values().collect())
}

// ponytail: LAN discovery is Unix-only; Windows needs GetAdaptersAddresses.
#[cfg(not(unix))]
pub fn interfaces() -> Result<Vec<Iface>, String> {
    Err("network interface enumeration is not supported on this platform".into())
}
