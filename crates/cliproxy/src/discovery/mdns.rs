//! mDNS on UDP 5353, following libp2p/zeroconf v2.2.0 (Go's dependency): the browse
//! client loop and the `RegisterProxy` responder with its probe, announcement, answer
//! and goodbye packets.
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::time::{Instant, sleep_until};

use super::dns::{
    CLASS_FLUSH, CLASS_IN, FLAGS_AUTHORITATIVE_RESPONSE, FLAGS_RESPONSE, Message, Question, RData, Record, TYPE_A,
    TYPE_AAAA, TYPE_PTR, TYPE_SRV,
};
use super::iface::Iface;
use super::{DiscoveredService, Entry, PRODUCT_CPA, ServiceSpec, entry_to_discovered, entry_within_limits};

const GROUP_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const GROUP_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
const PORT: u16 = 5353;
const TTL: u32 = 3200;
const MAX_DISCOVERED_SERVICES: usize = 256;
const MAX_BROWSE_ENTRIES: usize = 256;

fn trim_dot(s: &str) -> &str {
    s.trim_matches('.')
}

/// Go's `syscall.Errno` text: glibc's message with a lowercase first letter.
fn os_error(op: &str, e: &io::Error) -> String {
    let text = e.to_string();
    let text = text.split(" (os error").next().unwrap_or(&text);
    let mut chars = text.chars();
    let lower = chars
        .next()
        .map(|c| c.to_lowercase().chain(chars).collect::<String>())
        .unwrap_or_default();
    format!("{op}: {lower}")
}

fn names(ifaces: &[Iface]) -> String {
    // ponytail: Go prints whole net.Interface structs here; names identify them.
    let list: Vec<&str> = ifaces.iter().map(|i| i.name.as_str()).collect();
    format!("[{}]", list.join(" "))
}

/// Go `joinUdp4Multicast`: one wildcard socket on 5353 joined on each interface;
/// an error only when no join succeeds.
fn join_v4(ifaces: &[Iface]) -> Result<UdpSocket, String> {
    let ctx = "listen udp4 224.0.0.0:5353";
    let fail = |op: &'static str| move |e: io::Error| format!("{ctx}: {}", os_error(op, &e));
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(fail("socket"))?;
    socket.set_reuse_address(true).map_err(fail("setsockopt"))?;
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    socket.set_reuse_port(true).map_err(fail("setsockopt"))?;
    socket
        .bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, PORT)).into())
        .map_err(fail("bind"))?;
    let joined = ifaces
        .iter()
        .filter(|i| {
            i.ipv4()
                .is_some_and(|a| socket.join_multicast_v4(&GROUP_V4, &a).is_ok())
        })
        .count();
    if joined == 0 {
        return Err(format!(
            "udp4: failed to join any of these interfaces: {}",
            names(ifaces)
        ));
    }
    let _ = socket.set_multicast_ttl_v4(255);
    socket.set_nonblocking(true).map_err(fail("setsockopt"))?;
    UdpSocket::from_std(socket.into()).map_err(|e| format!("{ctx}: {e}"))
}

/// Go `joinUdp6Multicast`.
fn join_v6(ifaces: &[Iface]) -> Result<UdpSocket, String> {
    let ctx = "listen udp6 [ff02::]:5353";
    let fail = |op: &'static str| move |e: io::Error| format!("{ctx}: {}", os_error(op, &e));
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).map_err(fail("socket"))?;
    socket.set_only_v6(true).map_err(fail("setsockopt"))?;
    socket.set_reuse_address(true).map_err(fail("setsockopt"))?;
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    socket.set_reuse_port(true).map_err(fail("setsockopt"))?;
    socket
        .bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, PORT)).into())
        .map_err(fail("bind"))?;
    let joined = ifaces
        .iter()
        .filter(|i| socket.join_multicast_v6(&GROUP_V6, i.index).is_ok())
        .count();
    if joined == 0 {
        return Err(format!(
            "udp6: failed to join any of these interfaces: {}",
            names(ifaces)
        ));
    }
    let _ = socket.set_multicast_hops_v6(255);
    socket.set_nonblocking(true).map_err(fail("setsockopt"))?;
    UdpSocket::from_std(socket.into()).map_err(|e| format!("{ctx}: {e}"))
}

/// Multicasts `packet` out of every interface, ignoring send errors as Go does.
// ponytail: answers go out on every selected interface, not only the one the query
// arrived on (Go uses IP_PKTINFO); the records are the same either way.
async fn multicast(v4: Option<&UdpSocket>, v6: Option<&UdpSocket>, ifaces: &[Iface], packet: &[u8]) {
    for iface in ifaces {
        if let (Some(sock), Some(addr)) = (v4, iface.ipv4())
            && socket2::SockRef::from(sock).set_multicast_if_v4(&addr).is_ok()
        {
            let _ = sock.send_to(packet, (GROUP_V4, PORT)).await;
        }
        if let Some(sock) = v6
            && socket2::SockRef::from(sock).set_multicast_if_v6(iface.index).is_ok()
        {
            let _ = sock.send_to(packet, (GROUP_V6, PORT)).await;
        }
    }
}

async fn recv(sock: Option<&UdpSocket>, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
    match sock {
        Some(s) => s.recv_from(buf).await,
        None => std::future::pending().await,
    }
}

fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    u64::from_le_bytes(b)
}

/// An IP from an A/AAAA record; IPv4-mapped IPv6 behaves (and prints) as IPv4, like Go.
fn ip(data: &RData) -> Option<IpAddr> {
    match data {
        RData::A(a) => Some(IpAddr::V4(*a)),
        RData::Aaaa(a) => Some(IpAddr::V6(*a).to_canonical()),
        _ => None,
    }
}

/// zeroconf's browse client state.
struct Browser {
    service: String,
    domain: String,
    service_name: String,
    /// Emitted entries and their expiry (zeroconf `sentEntries`).
    sent: HashMap<String, std::time::Instant>,
}

/// zeroconf `cleanupFreq`.
const CLEANUP: Duration = Duration::from_secs(10);

impl Browser {
    /// zeroconf's cleanup tick: forget expired entries so they can be emitted again.
    fn sweep(&mut self, now: std::time::Instant) {
        self.sent.retain(|_, expiry| now <= *expiry);
    }
}

impl Browser {
    /// zeroconf `mainloop` for one message: entries assembled from this message only,
    /// emitted once each, and only with at least one address.
    fn handle(&mut self, msg: &Message) -> Vec<Entry> {
        type Slot = (Entry, std::time::Instant);
        let now = std::time::Instant::now();
        let mut order: Vec<String> = Vec::new();
        let mut entries: HashMap<String, Slot> = HashMap::new();
        let records: Vec<&Record> = msg
            .answers
            .iter()
            .chain(&msg.authority)
            .chain(&msg.additional)
            .collect();
        let (service, domain) = (&self.service, &self.domain);
        let slot = |entries: &'_ mut HashMap<String, Slot>, order: &mut Vec<String>, key: &str, instance: String| {
            if !entries.contains_key(key) {
                order.push(key.to_owned());
                let entry = Entry {
                    instance: trim_dot(&instance).to_owned(),
                    service: service.clone(),
                    domain: domain.clone(),
                    ..Default::default()
                };
                entries.insert(key.to_owned(), (entry, now));
            }
        };
        for rr in &records {
            let expiry = now + Duration::from_secs(rr.ttl.into());
            // miekg's zero-value records for empty RDATA.
            let empty_srv = RData::Srv {
                priority: 0,
                weight: 0,
                port: 0,
                target: String::new(),
            };
            let data = match &rr.data {
                RData::Empty(TYPE_PTR) => &RData::Ptr(String::new()),
                RData::Empty(TYPE_SRV) => &empty_srv,
                data => data,
            };
            let key = match data {
                RData::Ptr(ptr) if rr.name == self.service_name => {
                    slot(&mut entries, &mut order, ptr, ptr.replace(&rr.name, ""));
                    ptr
                }
                RData::Srv { .. } | RData::Txt(_) if rr.name.ends_with(&self.service_name) => {
                    slot(
                        &mut entries,
                        &mut order,
                        &rr.name,
                        rr.name.replacen(&self.service_name, "", 1),
                    );
                    &rr.name
                }
                _ => continue,
            };
            let e = entries.get_mut(key).expect("slot inserted");
            match data {
                RData::Srv { port, target, .. } => {
                    e.0.host.clone_from(target);
                    e.0.port = (*port).into();
                }
                RData::Txt(text) => e.0.text.clone_from(text),
                _ => {}
            }
            e.1 = expiry;
        }
        for rr in &records {
            if let RData::Empty(TYPE_A | TYPE_AAAA) = rr.data {
                for (e, _) in entries.values_mut() {
                    e.nil_addr |= e.host == rr.name;
                }
                continue;
            }
            let Some(addr) = ip(&rr.data) else { continue };
            for (e, _) in entries.values_mut() {
                if e.host == rr.name {
                    match rr.data {
                        RData::A(_) => e.ipv4.push(addr),
                        _ => e.ipv6.push(addr),
                    }
                }
            }
        }
        let mut out = Vec::new();
        for key in order {
            let (e, expiry) = entries.remove(&key).expect("ordered key");
            if expiry <= now {
                self.sent.remove(&key);
                continue;
            }
            if self.sent.contains_key(&key) || (e.ipv4.is_empty() && e.ipv6.is_empty() && !e.nil_addr) {
                continue;
            }
            self.sent.insert(key, expiry);
            out.push(e);
        }
        out
    }
}

/// Go `ZeroconfBrowser.Browse`'s collector: bounded, deduplicated by name/type/domain.
#[derive(Default)]
struct Collector {
    discovered: Vec<DiscoveredService>,
    seen: HashMap<(String, String, String), usize>,
    entries: usize,
}

impl Collector {
    /// Returns true once the entry budget is spent (Go cancels the browse then).
    fn add(&mut self, e: &Entry) -> bool {
        if self.entries >= MAX_BROWSE_ENTRIES {
            return true;
        }
        self.entries += 1;
        if entry_within_limits(e) {
            let svc = entry_to_discovered(e);
            if svc.port != 0 && !(svc.ipv4.is_empty() && svc.ipv6.is_empty()) {
                let key = (svc.instance_name.clone(), svc.service_type.clone(), svc.domain.clone());
                if let Some(&i) = self.seen.get(&key) {
                    super::merge_discovered(&mut self.discovered[i], &svc);
                } else if self.discovered.len() < MAX_DISCOVERED_SERVICES {
                    self.seen.insert(key, self.discovered.len());
                    self.discovered.push(svc);
                }
            }
        }
        self.entries == MAX_BROWSE_ENTRIES
    }
}

/// Go `BrowseWithFallbackServiceType` over zeroconf `Browse`: queries at once, again
/// after 4 s and then with zeroconf's jittered backoff, until `timeout`. CPA instances
/// come first. Errors carry Go's `discovery: browse query failed:` prefix.
pub async fn browse(service_type: &str, ifaces: &[Iface], timeout: Duration) -> Result<Vec<DiscoveredService>, String> {
    let fail = |e: String| format!("discovery: browse query failed: {e}");
    let service_type = if service_type.trim().is_empty() {
        super::DEFAULT_SERVICE_TYPE
    } else {
        service_type
    };
    let mut parts = service_type.split(',');
    let service = parts.next().unwrap_or_default().to_owned();
    let service_name = format!("{}.local.", trim_dot(&service));
    let query_name = parts
        .next()
        .map(|sub| format!("{}._sub.{service_name}", trim_dot(sub)))
        .unwrap_or_else(|| service_name.clone());
    let query = Message {
        questions: vec![Question {
            name: query_name,
            qtype: TYPE_PTR,
            qclass: CLASS_IN,
        }],
        ..Default::default()
    }
    .encode()
    .ok_or_else(|| fail("dns: bad domain name".into()))?;
    let v4 = join_v4(ifaces).map_err(fail)?;
    let v6 = join_v6(ifaces).map_err(fail)?;
    let mut browser = Browser {
        service,
        domain: super::DEFAULT_DOMAIN.into(),
        service_name,
        sent: HashMap::new(),
    };
    let mut collector = Collector::default();
    let start = Instant::now();
    let mut cleanup = tokio::time::interval_at(start + CLEANUP, CLEANUP);
    let deadline = start + timeout;
    let mut interval = Duration::from_secs(4);
    let mut next = start + interval;
    multicast(Some(&v4), Some(&v6), ifaces, &query).await;
    let (mut buf4, mut buf6) = (vec![0u8; 65536], vec![0u8; 65536]);
    loop {
        let packet = tokio::select! {
            _ = sleep_until(deadline) => break,
            t = cleanup.tick() => {
                browser.sweep(t.into_std());
                continue;
            }
            _ = sleep_until(next) => {
                multicast(Some(&v4), Some(&v6), ifaces, &query).await;
                let max = Duration::from_secs(60);
                if interval != max {
                    let jitter = random_u64() % interval.as_nanos() as u64;
                    interval = (interval + Duration::from_nanos(jitter) + interval / 2).min(max);
                }
                next = Instant::now() + interval;
                continue;
            }
            r = v4.recv_from(&mut buf4) => r.ok().map(|(n, _)| buf4[..n].to_vec()),
            r = v6.recv_from(&mut buf6) => r.ok().map(|(n, _)| buf6[..n].to_vec()),
        };
        let Some(msg) = packet.as_deref().and_then(Message::decode) else {
            continue;
        };
        if browser.handle(&msg).iter().any(|e| collector.add(e)) {
            break;
        }
    }
    let (cpa, other): (Vec<_>, Vec<_>) = collector.discovered.into_iter().partition(|g| g.product == PRODUCT_CPA);
    Ok(cpa.into_iter().chain(other).collect())
}

/// The names and records zeroconf's `RegisterProxy` serves.
#[derive(Clone)]
struct Service {
    service_name: String,
    instance_name: String,
    type_name: String,
    subtypes: Vec<String>,
    host: String,
    port: u16,
    text: Vec<String>,
    ips: Vec<IpAddr>,
}

impl Service {
    fn ptr(&self, name: &str, target: &str, ttl: u32) -> Record {
        Record {
            name: name.into(),
            class: CLASS_IN,
            ttl,
            data: RData::Ptr(target.into()),
        }
    }

    fn srv(&self, ttl: u32, class: u16) -> Record {
        Record {
            name: self.instance_name.clone(),
            class,
            ttl,
            data: RData::Srv {
                priority: 0,
                weight: 0,
                port: self.port,
                target: self.host.clone(),
            },
        }
    }

    fn txt(&self, ttl: u32, class: u16) -> Record {
        Record {
            name: self.instance_name.clone(),
            class,
            ttl,
            data: RData::Txt(self.text.clone()),
        }
    }

    /// zeroconf `appendAddrs`: A/AAAA records live 120 s (0 for goodbyes).
    fn addrs(&self, ttl: u32, flush: bool) -> impl Iterator<Item = Record> + '_ {
        let ttl = if ttl > 0 { 120 } else { 0 };
        let class = if flush { CLASS_IN | CLASS_FLUSH } else { CLASS_IN };
        let v4 = self.ips.iter().filter(|i| i.is_ipv4());
        let v6 = self.ips.iter().filter(|i| i.is_ipv6());
        v4.chain(v6).map(move |ip| Record {
            name: self.host.clone(),
            class,
            ttl,
            data: match ip {
                IpAddr::V4(a) => RData::A(*a),
                IpAddr::V6(a) => RData::Aaaa(*a),
            },
        })
    }

    /// zeroconf `composeBrowsingAnswers`.
    fn browsing(&self, resp: &mut Message) {
        resp.answers
            .push(self.ptr(&self.service_name, &self.instance_name, TTL));
        resp.additional.push(self.srv(TTL, CLASS_IN));
        resp.additional.push(self.txt(TTL, CLASS_IN));
        resp.additional.extend(self.addrs(TTL, false));
    }

    /// zeroconf `composeLookupAnswers`.
    fn lookup(&self, resp: &mut Message, ttl: u32, flush: bool) {
        resp.answers.push(self.srv(ttl, CLASS_IN | CLASS_FLUSH));
        resp.answers.push(self.txt(ttl, CLASS_IN | CLASS_FLUSH));
        resp.answers
            .push(self.ptr(&self.service_name, &self.instance_name, ttl));
        resp.answers.push(self.ptr(&self.type_name, &self.service_name, ttl));
        for sub in &self.subtypes {
            resp.answers.push(self.ptr(sub, &self.instance_name, ttl));
        }
        resp.answers.extend(self.addrs(ttl, flush));
    }

    /// zeroconf `handleQuestion` plus known-answer suppression. Subtype queries are
    /// answered with the browsing set; zeroconf v2.2.0 builds the subtype name twice
    /// (`_x._sub.<type>._sub.<type>`) and never matches them.
    fn answer(&self, q: &Question, query: &Message) -> Option<Message> {
        let mut resp = Message {
            id: query.id,
            flags: FLAGS_AUTHORITATIVE_RESPONSE | (query.flags & 0x7810),
            ..Default::default()
        };
        if q.name == self.type_name {
            resp.answers.push(self.ptr(&self.type_name, &self.service_name, TTL));
        } else if q.name == self.service_name || self.subtypes.contains(&q.name) {
            self.browsing(&mut resp);
        } else if q.name == self.instance_name {
            self.lookup(&mut resp, TTL, false);
            return Some(resp);
        } else {
            return None;
        }
        let known = match resp.answers.first() {
            Some(Record {
                data: RData::Ptr(ptr),
                ttl,
                ..
            }) => query
                .answers
                .iter()
                .any(|k| matches!(&k.data, RData::Ptr(p) if p == ptr && k.ttl >= ttl / 2)),
            _ => false,
        };
        (!known).then_some(resp)
    }
}

/// A running advertisement; [`Responder::shutdown`] sends the goodbye.
pub struct Responder {
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Responder {
    /// zeroconf `RegisterProxy` for `spec`, host name `host` (`.local.` is appended).
    pub fn register(spec: &ServiceSpec, host: &str) -> Result<Responder, String> {
        let domain = if spec.domain.is_empty() { "local" } else { &spec.domain };
        let service_type = if spec.service_type.is_empty() {
            super::DEFAULT_SERVICE_TYPE
        } else {
            &spec.service_type
        };
        let service_name = format!("{}.{}.", trim_dot(service_type), trim_dot(domain));
        let host = if trim_dot(host).ends_with(domain) {
            host.to_owned()
        } else {
            format!("{}.{}.", trim_dot(host), trim_dot(domain))
        };
        let service = Service {
            instance_name: format!("{}.{service_name}", trim_dot(&spec.instance_name)),
            type_name: format!("_services._dns-sd._udp.{}.", trim_dot(domain)),
            subtypes: spec
                .subtypes
                .iter()
                .map(|s| format!("{}._sub.{service_name}", trim_dot(s)))
                .collect(),
            service_name,
            host,
            port: spec.port,
            text: spec.text_records.clone(),
            ips: spec.advertised_ips.clone(),
        };
        let ifaces = spec.interfaces.clone();
        let v4 = join_v4(&ifaces).inspect_err(|e| tracing::debug!("[zeroconf] no suitable IPv4 interface: {e}"));
        let v6 = join_v6(&ifaces).inspect_err(|e| tracing::debug!("[zeroconf] no suitable IPv6 interface: {e}"));
        if v4.is_err() && v6.is_err() {
            return Err("no supported interface".into());
        }
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(serve(service, ifaces, v4.ok(), v6.ok(), stopped));
        Ok(Responder { stop, task })
    }

    /// zeroconf `Shutdown`: goodbye records (TTL 0) on every interface, then close.
    pub async fn shutdown(self) {
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}

async fn serve(
    service: Service,
    ifaces: Vec<Iface>,
    v4: Option<UdpSocket>,
    v6: Option<UdpSocket>,
    mut stopped: tokio::sync::oneshot::Receiver<()>,
) {
    let (v4, v6) = (v4.as_ref(), v6.as_ref());
    // zeroconf `probe`: three probes 250 ms apart after a 0-250 ms delay, then two
    // announcements one and two seconds apart. Conflicts are not resolved (as in Go).
    let start = Instant::now() + Duration::from_millis(random_u64() % 250);
    let mut schedule: Vec<(Instant, bool)> = (0..3u32)
        .map(|i| (start + Duration::from_millis(250) * i, false))
        .chain([
            (start + Duration::from_millis(750), true),
            (start + Duration::from_millis(1750), true),
        ])
        .collect();
    schedule.reverse();
    let probe = Message {
        questions: vec![Question {
            name: service.instance_name.clone(),
            qtype: TYPE_PTR,
            qclass: CLASS_IN,
        }],
        authority: vec![service.srv(TTL, CLASS_IN), service.txt(TTL, CLASS_IN)],
        ..Default::default()
    }
    .encode();
    let announce = |ttl| {
        let mut m = Message {
            flags: FLAGS_RESPONSE,
            ..Default::default()
        };
        service.lookup(&mut m, ttl, true);
        m.encode()
    };
    let (mut buf4, mut buf6) = (vec![0u8; 65536], vec![0u8; 65536]);
    loop {
        let next = schedule.last().map(|s| s.0);
        let (packet, from) = tokio::select! {
            _ = &mut stopped => break,
            _ = async { sleep_until(next.unwrap()).await }, if next.is_some() => {
                let (_, is_announcement) = schedule.pop().expect("scheduled");
                let packet = if is_announcement { announce(TTL) } else { probe.clone() };
                if let Some(packet) = packet {
                    multicast(v4, v6, &ifaces, &packet).await;
                }
                continue;
            }
            r = recv(v4, &mut buf4) => match r { Ok((n, from)) => (buf4[..n].to_vec(), from), Err(_) => continue },
            r = recv(v6, &mut buf6) => match r { Ok((n, from)) => (buf6[..n].to_vec(), from), Err(_) => continue },
        };
        let Some(query) = Message::decode(&packet) else {
            continue;
        };
        if !query.authority.is_empty() {
            continue;
        }
        for q in &query.questions {
            let Some(packet) = service.answer(q, &query).and_then(|r| r.encode()) else {
                continue;
            };
            if q.qclass & CLASS_FLUSH != 0 {
                let sock = if from.is_ipv4() { v4 } else { v6 };
                if let Some(sock) = sock {
                    let _ = sock.send_to(&packet, from).await;
                }
            } else {
                multicast(v4, v6, &ifaces, &packet).await;
            }
        }
    }
    if let Some(goodbye) = announce(0) {
        multicast(v4, v6, &ifaces, &goodbye).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec {
            instance_name: "My Box-8F3B".into(),
            service_type: "_ai-gateway._tcp".into(),
            domain: "local.".into(),
            port: 8317,
            subtypes: vec!["_responses".into()],
            text_records: vec!["version=1".into(), "product=cliproxyapi".into()],
            interfaces: vec![],
            advertised_ips: vec!["192.0.2.7".parse().unwrap(), "fe80::7".parse().unwrap()],
        }
    }

    fn service() -> Service {
        let s = spec();
        Service {
            service_name: "_ai-gateway._tcp.local.".into(),
            instance_name: format!("{}._ai-gateway._tcp.local.", s.instance_name),
            type_name: "_services._dns-sd._udp.local.".into(),
            subtypes: vec!["_responses._sub._ai-gateway._tcp.local.".into()],
            host: "box.local.".into(),
            port: s.port,
            text: s.text_records,
            ips: s.advertised_ips,
        }
    }

    fn question(name: &str, qclass: u16) -> Message {
        Message {
            questions: vec![Question {
                name: name.into(),
                qtype: TYPE_PTR,
                qclass,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn responder_answers_round_trip_through_the_browser() {
        let svc = service();
        let query = question("_ai-gateway._tcp.local.", CLASS_IN);
        let resp = svc.answer(&query.questions[0], &query).unwrap();
        assert_eq!(resp.flags, FLAGS_AUTHORITATIVE_RESPONSE);
        let wire = resp.encode().unwrap();
        let mut browser = Browser {
            service: "_ai-gateway._tcp".into(),
            domain: "local.".into(),
            service_name: "_ai-gateway._tcp.local.".into(),
            sent: HashMap::new(),
        };
        let entries = browser.handle(&Message::decode(&wire).unwrap());
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        // miekg presentation form: the space in the instance label is escaped.
        assert_eq!(e.instance, "My\\ Box-8F3B");
        assert_eq!((e.host.as_str(), e.port), ("box.local.", 8317));
        assert_eq!(e.ipv4, vec!["192.0.2.7".parse::<IpAddr>().unwrap()]);
        assert_eq!(e.ipv6, vec!["fe80::7".parse::<IpAddr>().unwrap()]);
        // Emitted once per browse; a goodbye forgets it.
        assert!(browser.handle(&Message::decode(&wire).unwrap()).is_empty());
        let mut bye = Message::default();
        svc.lookup(&mut bye, 0, true);
        assert!(
            browser
                .handle(&Message::decode(&bye.encode().unwrap()).unwrap())
                .is_empty()
        );
        assert_eq!(browser.handle(&Message::decode(&wire).unwrap()).len(), 1);
    }

    /// Packets captured from Go's zeroconf advertiser on a dummy interface (README.md).
    #[test]
    fn decodes_compressed_packets_from_go_zeroconf() {
        let packets: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/discovery_go_packets.json")).unwrap();
        let decode = |k: &str| {
            let hex = packets[k].as_str().unwrap();
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            Message::decode(&bytes).unwrap_or_else(|| panic!("{k} decodes"))
        };
        let mut browser = Browser {
            service: "_ai-gateway._tcp".into(),
            domain: "local.".into(),
            service_name: "_ai-gateway._tcp.local.".into(),
            sent: HashMap::new(),
        };
        let entries = browser.handle(&decode("answer"));
        assert_eq!(entries.len(), 1);
        let svc = entry_to_discovered(&entries[0]);
        assert_eq!(svc.instance_name, "Go\\ Box-4DE2");
        assert_eq!((svc.host.as_str(), svc.port), ("e2b.local.", 18317));
        assert_eq!(svc.ipv4, vec!["10.77.0.1".parse::<IpAddr>().unwrap()]);
        assert_eq!(svc.raw_txt["instance_id"], "4DE2");
        assert_eq!(decode("announce").answers.len(), 11);
        assert!(decode("goodbye").answers.iter().all(|r| r.ttl == 0));
        assert_eq!(decode("probe").authority.len(), 2);
        // Our own encoding of the same announcement decodes to the same records.
        let announce = decode("announce");
        assert_eq!(Message::decode(&announce.encode().unwrap()).unwrap(), announce);
    }

    #[test]
    fn expired_entries_are_swept_and_emitted_again() {
        let svc = service();
        let mut resp = Message::default();
        svc.browsing(&mut resp);
        let wire = Message::decode(&resp.encode().unwrap()).unwrap();
        let mut browser = Browser {
            service: "_ai-gateway._tcp".into(),
            domain: "local.".into(),
            service_name: "_ai-gateway._tcp.local.".into(),
            sent: HashMap::new(),
        };
        assert_eq!(browser.handle(&wire).len(), 1);
        browser.sweep(std::time::Instant::now());
        assert!(browser.handle(&wire).is_empty(), "TTL 3200 has not expired");
        browser.sweep(std::time::Instant::now() + Duration::from_secs(3201));
        assert_eq!(browser.handle(&wire).len(), 1);
        // A zero-length A record resolves the entry (Go's nil IP) without an address.
        let mut nil_only = wire.clone();
        nil_only
            .additional
            .retain(|r| !matches!(r.data, RData::A(_) | RData::Aaaa(_)));
        nil_only.additional.push(Record {
            name: "box.local.".into(),
            class: CLASS_IN,
            ttl: 120,
            data: RData::Empty(TYPE_A),
        });
        browser.sent.clear();
        let entries = browser.handle(&nil_only);
        assert!(entries[0].nil_addr && entries[0].ipv4.is_empty());
        assert!(entry_to_discovered(&entries[0]).ipv4.is_empty());
    }

    #[test]
    fn known_answers_suppress_and_unknown_names_are_ignored() {
        let svc = service();
        let mut query = question("_ai-gateway._tcp.local.", CLASS_IN);
        query
            .answers
            .push(svc.ptr(&svc.service_name, &svc.instance_name, TTL / 2));
        assert!(svc.answer(&query.questions[0], &query).is_none());
        query.answers[0].ttl = TTL / 2 - 1;
        assert!(svc.answer(&query.questions[0], &query).is_some());
        let other = question("_other._tcp.local.", CLASS_IN);
        assert!(svc.answer(&other.questions[0], &other).is_none());
        // Go compares the unescaped instance name with the escaped presentation name
        // it decoded, so an instance with a space never matches; neither do we.
        let lookup = question("My\\ Box-8F3B._ai-gateway._tcp.local.", CLASS_IN);
        assert!(svc.answer(&lookup.questions[0], &lookup).is_none());
        let sub = question("_responses._sub._ai-gateway._tcp.local.", CLASS_IN);
        assert_eq!(svc.answer(&sub.questions[0], &sub).unwrap().answers.len(), 1);
        let enumerate = question("_services._dns-sd._udp.local.", CLASS_IN);
        let resp = svc.answer(&enumerate.questions[0], &enumerate).unwrap();
        assert_eq!(resp.answers[0].data, RData::Ptr("_ai-gateway._tcp.local.".into()));
    }

    #[test]
    fn lookup_answers_match_zeroconf_order_and_flush_bits() {
        let svc = service();
        let mut m = Message::default();
        svc.lookup(&mut m, TTL, true);
        let kinds: Vec<(u16, u32)> = m.answers.iter().map(|r| (r.class, r.ttl)).collect();
        assert_eq!(
            kinds,
            vec![
                (CLASS_IN | CLASS_FLUSH, TTL),
                (CLASS_IN | CLASS_FLUSH, TTL),
                (CLASS_IN, TTL),
                (CLASS_IN, TTL),
                (CLASS_IN, TTL),
                (CLASS_IN | CLASS_FLUSH, 120),
                (CLASS_IN | CLASS_FLUSH, 120),
            ]
        );
        let mut bye = Message::default();
        svc.lookup(&mut bye, 0, true);
        assert!(bye.answers.iter().all(|r| r.ttl == 0));
    }

    #[test]
    fn collector_stops_after_256_entries_and_merges_duplicates() {
        let mut c = Collector::default();
        let e = Entry {
            instance: "a".into(),
            service: "_ai-gateway._tcp".into(),
            domain: "local.".into(),
            port: 1,
            ipv4: vec!["192.0.2.1".parse().unwrap()],
            text: vec!["product=x".into()],
            ..Default::default()
        };
        let mut second = e.clone();
        second.ipv4 = vec!["192.0.2.2".parse().unwrap()];
        assert!(!c.add(&e));
        assert!(!c.add(&second));
        assert_eq!(c.discovered.len(), 1);
        assert_eq!(c.discovered[0].ipv4.len(), 2);
        let stops: Vec<bool> = (2..256).map(|_| c.add(&e)).collect();
        assert_eq!(stops.iter().filter(|s| **s).count(), 1);
        assert!(stops[253]);
    }
}
