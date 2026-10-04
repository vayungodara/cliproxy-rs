//! The WebRTC media relay (media.go `pionMediaRelay`) on webrtc-rs, a pion port. Feature
//! `media-relay`.
//!
//! Each call gets two peer connections: one answering the client's offer (downstream) and
//! one offering to the Codex upstream. Opus RTP and the `oai-events` data channel are
//! forwarded between them; anything else is dropped. Data channel messages over 256 KiB,
//! a failed or remotely closed peer, or a failed data channel end the session.
//!
//! A credential with a proxy keeps the upstream peer on loopback and reaches the upstream
//! over ICE-TCP through that proxy (tunnel.rs, Go tcp_proxy.go).
//!
//! ponytail: pion behaviour webrtc-rs 0.21 lacks (more at `ice_url`, `advertise`):
//! `disable-private-remote-ips` filters the client's offered candidates, not peer-reflexive
//! ones learned from STUN.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use futures_util::future::BoxFuture;
use rtc::media_stream::MediaStreamTrack;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelMessage};
use webrtc::media_stream::Track;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceCandidateInit, RTCIceGatheringState, RTCIceServer, RTCPeerConnectionState, RTCSessionDescription, Registry,
    SettingEngineBuilder, register_default_interceptors,
};
use webrtc::rtp_transceiver::RtpSender;

use super::dialer::{self, Dial, proxy_scheme};
use super::relay::{Hold, Limiter, MediaRelay, MediaSession, NewSession, RelayConfig, RelayError, Route, Slot};
use super::tunnel::{self, Tunnel};

/// `realtimeDataChannelLabel`.
const LABEL: &str = "oai-events";
/// `mediaDataQueueSize`, `mediaDataMessageMaxSize`, `mediaDataBufferedMaxSize`.
const QUEUE: usize = 64;
const MAX_MESSAGE: usize = 256 << 10;
const BUFFERED_MAX: usize = 1 << 20;
/// Opus as Go registers it (payload type 111).
const OPUS_FMTP: &str = "minptime=10;useinbandfec=1";
/// How long gathering may run before the session goes on with the candidates it has.
/// pion gives each STUN server its 5 s `stunGatherTimeout`, then completes gathering;
/// webrtc-rs 0.21 keeps a timed-out STUN client and never completes.
// ponytail: a TURN allocation slower than this loses its relay candidate, which pion
// would still wait for.
const GATHER_BOUND: Duration = Duration::from_secs(6);

pub(super) struct Relay {
    config: RelayConfig,
    limiter: Arc<Limiter>,
    /// Bind and advertise only this address (tests use loopback).
    bind_ip: Option<IpAddr>,
}

impl Relay {
    pub fn new(config: &RelayConfig, limiter: Arc<Limiter>) -> Result<Self, String> {
        let skipped = ungathered_ice_urls(config);
        if !skipped.is_empty() {
            // ponytail: webrtc-rs 0.21 gathers relay candidates over UDP TURN only; pion
            // also uses TURN over TCP and TLS. Those servers are skipped with this warning.
            tracing::warn!(
                urls = ?skipped,
                "codex.live-media-relay: TURN over TCP or TLS is not supported by this build; these ICE servers are skipped"
            );
        }
        if !config.public_ip.trim().is_empty() && config.public_ip.trim().parse::<IpAddr>().is_err() {
            tracing::warn!(
                "codex.live-media-relay.public-ip is not an IP address; host candidates keep their addresses"
            );
        }
        Ok(Self {
            config: config.clone(),
            limiter,
            bind_ip: None,
        })
    }

    /// One peer connection with Go's media and network settings. Tries each free port of
    /// `udp-port-min..=udp-port-max` until one binds. A proxied session's upstream peer
    /// stays on loopback instead (Go `newPionProxyAPI`).
    async fn peer(&self, side: Side, shared: &Arc<Shared>) -> Result<Arc<dyn PeerConnection>, String> {
        if side == Side::Up && shared.proxy.is_some() {
            return self.build(side, shared, Net::Loopback, false).await;
        }
        // pion gathers host candidates on every interface of both families.
        let ips: Vec<IpAddr> = match self.bind_ip {
            Some(ip) => vec![ip],
            None if ipv6_host() => vec![Ipv4Addr::UNSPECIFIED.into(), Ipv6Addr::UNSPECIFIED.into()],
            None => vec![Ipv4Addr::UNSPECIFIED.into()],
        };
        let (min, max) = (self.config.udp_port_min, self.config.udp_port_max);
        let ports: Vec<u16> = if min == 0 {
            vec![0]
        } else {
            let span = u32::from(max - min) + 1;
            let start = getrandom::u32().unwrap_or(0) % span;
            (0..span).map(|i| min + ((start + i) % span) as u16).collect()
        };
        let mut last = String::from("no free UDP port in the codex.live-media-relay range");
        let mut cursors = vec![0; ips.len()];
        while let Some(addrs) = next_ports(&ips, &ports, &mut cursors, port_free) {
            match self.build(side, shared, Net::Udp(addrs.clone()), true).await {
                Ok(pc) => return Ok(pc),
                // pion only logs a failed mDNS socket (no multicast interface); webrtc-rs
                // fails the peer, so retry without mDNS queries.
                Err(e) if e.contains("No such device") || e.contains("multicast") => {
                    tracing::debug!(error = %e, "codex live media: mDNS unavailable; continuing without it");
                    match self.build(side, shared, Net::Udp(addrs), false).await {
                        Ok(pc) => return Ok(pc),
                        Err(e) => last = e,
                    }
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    async fn build(
        &self,
        side: Side,
        shared: &Arc<Shared>,
        net: Net,
        mdns: bool,
    ) -> Result<Arc<dyn PeerConnection>, String> {
        let mut media = MediaEngine::default();
        media
            .register_codec(opus_parameters(), RtpCodecKind::Audio)
            .map_err(|e| format!("register Opus codec: {e}"))?;
        let registry = register_default_interceptors(Registry::new(), &mut media)
            .map_err(|e| format!("register WebRTC interceptors: {e}"))?;
        let mut settings = SettingEngineBuilder::new();
        let (udp, tcp, ice_servers) = match net {
            Net::Udp(addrs) => {
                if addrs.iter().any(|a| a.ip().is_loopback()) {
                    settings = settings.with_include_loopback_candidate(true);
                }
                let servers = self
                    .config
                    .ice_servers
                    .iter()
                    .map(|s| RTCIceServer {
                        urls: s.urls.iter().map(|u| ice_url(u.trim())).collect(),
                        username: s.username.clone(),
                        credential: s.credential.clone(),
                    })
                    .collect();
                (addrs, vec![], servers)
            }
            Net::Loopback => {
                use rtc::ice::network_type::NetworkType;
                settings = settings.with_include_loopback_candidate(true).with_network_types(vec![
                    NetworkType::Udp4,
                    NetworkType::Udp6,
                    NetworkType::Tcp4,
                    NetworkType::Tcp6,
                ]);
                (loopback(), loopback(), vec![])
            }
        };
        if !mdns {
            settings = settings.with_multicast_dns_mode(rtc::ice::mdns::MulticastDnsMode::Disabled);
        }
        let pc = PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().with_ice_servers(ice_servers).build())
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(settings.build())
            .with_handler(Arc::new(Handler {
                shared: Arc::downgrade(shared),
                side,
            }))
            .with_udp_addrs(udp)
            // TCP binds give the peer its ICE-TCP active candidate (it dials the
            // tunnel); webrtc-rs also listens there, on loopback only.
            .with_tcp_addrs(tcp)
            .with_data_channel_send_buffer_limit(BUFFERED_MAX)
            .build()
            .await
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(pc))
    }

    async fn start(&self, offer: String, shared: &Arc<Shared>) -> Result<String, RelayError> {
        let fail = |what: &str, e: &dyn std::fmt::Display| RelayError::new(format!("{what}: {e}"));
        // Each peer is registered the moment it exists, so a cancelled or failed setup
        // closes whatever was created (webrtc-rs peers only stop on an explicit close).
        let down = self
            .peer(Side::Down, shared)
            .await
            .map_err(|e| fail("create downstream PeerConnection", &e))?;
        shared.register(&down);
        let up = self
            .peer(Side::Up, shared)
            .await
            .map_err(|e| fail("create upstream PeerConnection", &e))?;
        shared.register(&up);
        let _ = shared.pcs.set([down.clone(), up.clone()]);
        let offer = if self.config.disable_private_remote_ips {
            public_candidates_only(&offer)
        } else {
            offer
        };
        let offer = RTCSessionDescription::offer(offer).map_err(|e| fail("set downstream WebRTC offer", &e))?;
        down.set_remote_description(offer)
            .await
            .map_err(|e| fail("set downstream WebRTC offer", &e))?;
        for (side, pc) in [(Side::Down, &down), (Side::Up, &up)] {
            let track = Arc::new(TrackLocalStaticRTP::new(opus_track()));
            let sender = pc.add_track(track.clone()).await.map_err(|e| {
                let what = if side == Side::Down {
                    "add downstream audio track"
                } else {
                    "add upstream audio track"
                };
                fail(what, &e)
            })?;
            let ssrc = track.ssrcs().await.first().copied().unwrap_or_default();
            let _ = shared.outputs[side as usize].set(Output {
                track,
                sender,
                ssrc,
                payload_type: OnceLock::new(),
            });
        }
        let channel = up
            .create_data_channel(LABEL, None)
            .await
            .map_err(|e| fail("create upstream DataChannel", &e))?;
        shared.attach(Side::Up, channel);
        let offer = up
            .create_offer(None)
            .await
            .map_err(|e| fail("create upstream WebRTC offer", &e))?;
        up.set_local_description(offer)
            .await
            .map_err(|e| fail("set upstream WebRTC offer", &e))?;
        shared
            .gathered(Side::Up)
            .await
            .map_err(|e| fail("gather upstream WebRTC candidates", &e))?;
        let local = match up.local_description().await {
            Some(local) if !local.sdp.trim().is_empty() => local.sdp,
            _ => return Err(RelayError::new("upstream WebRTC offer is empty")),
        };
        if shared.proxy.is_some() {
            // Loopback only: no public-ip to advertise. The answer is checked against it.
            let _ = shared.local_offer.set(local.clone());
            return Ok(local);
        }
        Ok(advertise(&local, &self.config.public_ip))
    }
}

impl MediaRelay for Relay {
    fn new_session(&self, offer: String, route: Route, hold: Hold) -> BoxFuture<'_, NewSession> {
        Box::pin(async move {
            // `proxyutil.BuildDialer` before taking a slot.
            let proxy = dialer::build(&route.proxy_url)
                .map_err(|e| RelayError::new(format!("configure Codex live remote TCP proxy: {e}")))?
                .map(|dialer| Proxied {
                    dialer: Arc::new(dialer),
                    scheme: proxy_scheme(&route.proxy_url),
                });
            let slot = self
                .limiter
                .acquire()
                .ok_or_else(|| RelayError::new("Codex live media relay capacity exhausted"))?;
            let shared = Shared::new(slot, route, proxy, self.config.public_ip.clone());
            match &shared.proxy {
                Some(proxy) => tracing::info!(
                    media_session_id = %shared.id,
                    remote_transport = "tcp",
                    proxy_scheme = %proxy.scheme,
                    "codex live WebRTC media session created"
                ),
                None => tracing::info!(media_session_id = %shared.id, "codex live WebRTC media session created"),
            }
            // Closes the session if setup fails or this future is dropped (the request
            // went away mid-negotiation), as Go's `Close` on those paths, and keeps `hold`
            // until that close finished.
            let mut guard = SetupGuard {
                shared: Some(shared.clone()),
                hold: Some(hold),
            };
            let sdp = match self.start(offer, &shared).await {
                Ok(sdp) => sdp,
                Err(e) => {
                    // Go's `Close` returns after the peers closed; so does this error, so
                    // the caller's credential outlives the session.
                    drop(guard);
                    let (done, closed) = tokio::sync::oneshot::channel();
                    shared.on_closed(Box::new(move || {
                        let _ = done.send(());
                    }));
                    let _ = closed.await;
                    return Err(e);
                }
            };
            guard.shared = None;
            Ok((Arc::new(Session(shared)) as Arc<dyn MediaSession>, sdp))
        })
    }
}

/// Where a peer's sockets live.
enum Net {
    /// UDP on these wildcards or addresses, all on one port (the configured range, or
    /// any port).
    Udp(Vec<SocketAddr>),
    /// UDP and TCP on loopback only: a proxied session's upstream peer.
    Loopback,
}

/// The loopback addresses a proxied upstream peer binds: IPv4, and IPv6 where the host
/// has it (pion gathers both families' loopback candidates).
fn loopback() -> Vec<SocketAddr> {
    static V6: OnceLock<bool> = OnceLock::new();
    let mut addrs = vec![SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)];
    if *V6.get_or_init(|| std::net::UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).is_ok()) {
        addrs.push(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 0));
    }
    addrs
}

/// How a proxied session reaches the upstream.
struct Proxied {
    dialer: Arc<dyn Dial>,
    /// For logs (Go `proxyScheme`).
    scheme: String,
}

/// Closes a session whose setup did not finish, then drops `hold` once that close
/// finished. Without a session to close, `hold` drops with the guard.
struct SetupGuard {
    shared: Option<Arc<Shared>>,
    hold: Option<Hold>,
}

impl Drop for SetupGuard {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.take() {
            shared.close("closed");
            if let Some(hold) = self.hold.take() {
                shared.on_closed(Box::new(move || drop(hold)));
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Down = 0,
    Up = 1,
}

impl Side {
    fn other(self) -> Self {
        match self {
            Side::Down => Side::Up,
            Side::Up => Side::Down,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Side::Down => "downstream",
            Side::Up => "upstream",
        }
    }
}

fn opus_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: "audio/opus".into(),
        clock_rate: 48000,
        channels: 2,
        sdp_fmtp_line: OPUS_FMTP.into(),
        rtcp_feedback: vec![],
    }
}

fn opus_parameters() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: opus_codec(),
        payload_type: 111,
    }
}

fn opus_track() -> MediaStreamTrack {
    MediaStreamTrack::new(
        "codex-live".into(),
        "audio".into(),
        "audio".into(),
        RtpCodecKind::Audio,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(getrandom::u32().unwrap_or(1)),
                ..Default::default()
            },
            codec: opus_codec(),
            ..Default::default()
        }],
    )
}

/// Where relayed RTP for one side goes. pion's `TrackLocalStaticRTP` rewrites SSRC and
/// payload type to the sender's binding; webrtc-rs rejects packets that do not carry
/// them, so the relay sets both.
struct Output {
    track: Arc<TrackLocalStaticRTP>,
    sender: Arc<dyn RtpSender>,
    ssrc: u32,
    payload_type: OnceLock<u8>,
}

impl Output {
    async fn payload_type(&self) -> Option<u8> {
        if let Some(pt) = self.payload_type.get() {
            return Some(*pt);
        }
        let parameters = self.sender.get_parameters().await.ok()?;
        let pt = parameters
            .rtp_parameters
            .codecs
            .iter()
            .find(|c| c.rtp_codec.mime_type.eq_ignore_ascii_case("audio/opus"))?
            .payload_type;
        Some(*self.payload_type.get_or_init(|| pt))
    }
}

/// An ICE server URL with the default port Go's parser supplies (3478, or 5349 for the
/// TLS schemes); webrtc-rs needs it explicit. `turn:host?transport=tcp` keeps its query.
fn ice_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once(':') else {
        return url.to_owned();
    };
    let default = match scheme.to_ascii_lowercase().as_str() {
        "stun" | "turn" => 3478,
        "stuns" | "turns" => 5349,
        _ => return url.to_owned(),
    };
    let (host, query) = match rest.split_once('?') {
        Some((host, query)) => (host, Some(query)),
        None => (rest, None),
    };
    let has_port = match host.rfind(']') {
        Some(bracket) => host[bracket..].contains(':'),
        None => host.contains(':'),
    };
    let host = if has_port {
        host.to_owned()
    } else {
        format!("{host}:{default}")
    };
    match query {
        Some(query) => format!("{scheme}:{host}?{query}"),
        None => format!("{scheme}:{host}"),
    }
}

/// URLs webrtc-rs 0.21 accepts but never gathers from: TURN over TCP and TURN over TLS.
fn ungathered_ice_urls(config: &RelayConfig) -> Vec<String> {
    config
        .ice_servers
        .iter()
        .flat_map(|s| s.urls.iter())
        .map(|u| u.trim())
        .filter(|u| {
            let lower = u.to_ascii_lowercase();
            lower.starts_with("turns:") || (lower.starts_with("turn:") && lower.contains("transport=tcp"))
        })
        .map(str::to_owned)
        .collect()
}

/// The next bind addresses to try: for each family the next port of `ports` (from its
/// cursor on) that is free in that family, like pion's per-address port allocation. Each
/// family keeps its own cursor; a failed attempt advances them all. `None` once a family
/// has no free port left.
fn next_ports(
    ips: &[IpAddr],
    ports: &[u16],
    cursors: &mut [usize],
    free: impl Fn(IpAddr, u16) -> bool,
) -> Option<Vec<SocketAddr>> {
    let mut addrs = Vec::with_capacity(ips.len());
    for (ip, cursor) in ips.iter().zip(cursors.iter_mut()) {
        while ports.get(*cursor).is_some_and(|port| *port != 0 && !free(*ip, *port)) {
            *cursor += 1;
        }
        addrs.push(SocketAddr::new(*ip, *ports.get(*cursor)?));
        *cursor += 1;
    }
    Some(addrs)
}

/// Whether `port` can be bound in `ip`'s family (IPv6 checked on its own, not as a
/// dual-stack socket that would also need the IPv4 port).
fn port_free(ip: IpAddr, port: u16) -> bool {
    use socket2::{Domain, Socket, Type};
    let addr = SocketAddr::new(ip, port);
    let Ok(socket) = Socket::new(Domain::for_address(addr), Type::DGRAM, None) else {
        return false;
    };
    if ip.is_ipv6() && socket.set_only_v6(true).is_err() {
        return false;
    }
    socket.bind(&addr.into()).is_ok()
}

/// Whether webrtc-rs turns `[::]` into IPv6 host candidates: some interface address is
/// not loopback, unspecified or link-local. Without one it would bind the wildcard itself
/// and advertise `::`.
fn ipv6_host() -> bool {
    rtc::shared::ifaces::ifaces().is_ok_and(|list| {
        list.iter().filter_map(|i| i.addr).any(|a| match a.ip() {
            IpAddr::V6(v) => !(v.is_loopback() || v.is_unspecified() || v.is_unicast_link_local()),
            IpAddr::V4(_) => false,
        })
    })
}

/// pion's `SetNAT1To1IPs(public-ip, host)`: host candidates of the public address's family
/// advertise it (the socket stays bound locally); the other family keeps its addresses.
/// webrtc-rs keeps the setting but never applies it, so the gathered SDP is rewritten.
fn advertise(sdp: &str, public_ip: &str) -> String {
    // net.IP semantics: an IPv4-mapped address is IPv4 and prints as one.
    let Ok(public) = public_ip.trim().parse::<IpAddr>().map(|ip| ip.to_canonical()) else {
        return sdp.to_owned();
    };
    let public_ip = public.to_string();
    let public_ip = public_ip.as_str();
    sdp.split_inclusive('\n')
        .map(|line| {
            let Some(rest) = line.strip_prefix("a=candidate:") else {
                return line.to_owned();
            };
            let mut fields: Vec<&str> = rest.split(' ').collect();
            let host = fields.get(6) == Some(&"typ") && fields.get(7).is_some_and(|t| t.trim_end() == "host");
            let family = |a: &&str| {
                a.parse::<IpAddr>()
                    .is_ok_and(|ip| ip.to_canonical().is_ipv4() == public.is_ipv4())
            };
            if host && fields.get(4).is_some_and(family) {
                fields[4] = public_ip;
                return format!("a=candidate:{}", fields.join(" "));
            }
            line.to_owned()
        })
        .collect()
}

/// `isPublicRemoteIP` (net.IP semantics: IPv4-mapped addresses count as IPv4).
pub(super) fn is_public_remote_ip(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v) => {
            !(v.is_unspecified() || v.is_loopback() || v.is_private() || v.is_link_local() || v.is_multicast())
        }
        IpAddr::V6(v) => {
            !(v.is_unspecified()
                || v.is_loopback()
                || v.is_unique_local()
                || v.is_unicast_link_local()
                || v.is_multicast())
        }
    }
}

/// The offer without candidates whose address is not a public IP (pion's remote IP
/// filter; mDNS names resolve to private addresses, so they go too).
fn public_candidates_only(sdp: &str) -> String {
    sdp.split_inclusive('\n')
        .filter(|line| {
            let Some(rest) = line.strip_prefix("a=candidate:") else {
                return true;
            };
            rest.split_whitespace()
                .nth(4)
                .and_then(|addr| addr.parse::<IpAddr>().ok())
                .is_some_and(is_public_remote_ip)
        })
        .collect()
}

#[derive(Default)]
struct State {
    call_id: String,
    failure: Option<String>,
    handler: Option<super::relay::CloseHandler>,
    handler_called: bool,
    slot: Option<Slot>,
}

/// Everything the two peers' callbacks and the session handle share.
struct Shared {
    id: String,
    /// Log-safe credential identity for the forwarding-started line.
    route: Route,
    forwarding_logged: AtomicBool,
    closed: AtomicBool,
    failed: AtomicBool,
    state: Mutex<State>,
    gathered: [watch::Sender<bool>; 2],
    outputs: [OnceLock<Output>; 2],
    channels: [OnceLock<Arc<dyn DataChannel>>; 2],
    /// Messages to send on each side's channel, and their receivers until it attaches.
    queues: [mpsc::Sender<RTCDataChannelMessage>; 2],
    receivers: [Mutex<Option<mpsc::Receiver<RTCDataChannelMessage>>>; 2],
    tasks: Mutex<Vec<AbortHandle>>,
    /// The two peers once both exist, for negotiation.
    pcs: OnceLock<[Arc<dyn PeerConnection>; 2]>,
    /// Every peer created so far, closed with the session.
    created: Mutex<Vec<Arc<dyn PeerConnection>>>,
    /// `public-ip`, advertised in place of host candidate addresses.
    public_ip: String,
    /// Set when the credential has a proxy: media reaches the upstream through it.
    proxy: Option<Proxied>,
    /// The upstream offer as gathered, for the proxied answer's ICE credentials.
    local_offer: OnceLock<String>,
    /// The proxied answer's candidate tunnels, closed with the session.
    tunnels: Mutex<Vec<Tunnel>>,
    /// Run once a close finished; `None` after that.
    after_close: AfterClose,
}

type AfterClose = Arc<Mutex<Option<Vec<Box<dyn FnOnce() + Send>>>>>;

fn run_after_close(hooks: &AfterClose) {
    let hooks = hooks.lock().unwrap_or_else(PoisonError::into_inner).take();
    for hook in hooks.into_iter().flatten() {
        hook();
    }
}

impl Shared {
    fn new(slot: Slot, route: Route, proxy: Option<Proxied>, public_ip: String) -> Arc<Self> {
        let (down_tx, down_rx) = mpsc::channel(QUEUE);
        let (up_tx, up_rx) = mpsc::channel(QUEUE);
        Arc::new(Self {
            id: crate::dispatch::request_id(),
            route,
            forwarding_logged: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            state: Mutex::new(State {
                slot: Some(slot),
                ..State::default()
            }),
            gathered: [watch::channel(false).0, watch::channel(false).0],
            outputs: [OnceLock::new(), OnceLock::new()],
            channels: [OnceLock::new(), OnceLock::new()],
            queues: [down_tx, up_tx],
            receivers: [Mutex::new(Some(down_rx)), Mutex::new(Some(up_rx))],
            tasks: Mutex::default(),
            pcs: OnceLock::new(),
            created: Mutex::default(),
            public_ip,
            proxy,
            local_offer: OnceLock::new(),
            tunnels: Mutex::default(),
            after_close: Arc::new(Mutex::new(Some(Vec::new()))),
        })
    }

    /// Runs `then` once a started close finished, or at once after that.
    fn on_closed(&self, then: Box<dyn FnOnce() + Send>) {
        let mut hooks = self.after_close.lock().unwrap_or_else(PoisonError::into_inner);
        match hooks.as_mut() {
            Some(pending) => pending.push(then),
            None => {
                drop(hooks);
                then();
            }
        }
    }

    /// `installCandidateTunnels`: false (and the tunnels closed) once the session closed.
    /// `close` sets `closed` before it drains, so no tunnel outlives the session.
    fn install_tunnels(&self, tunnels: Vec<Tunnel>) -> bool {
        let mut installed = self.tunnels.lock().unwrap_or_else(PoisonError::into_inner);
        if self.closed.load(Ordering::SeqCst) {
            return false;
        }
        installed.extend(tunnels);
        true
    }

    fn close_tunnels(&self) {
        drop(std::mem::take(
            &mut *self.tunnels.lock().unwrap_or_else(PoisonError::into_inner),
        ));
    }

    /// `logForwardingStarted`: once per session, from the first tunnel that forwarded or
    /// the upstream peer connecting, whichever comes first.
    fn forwarding_started(&self) {
        if self.forwarding_logged.swap(true, Ordering::SeqCst) {
            return;
        }
        let call_id = self.lock().call_id.clone();
        let (id, auth_index, credential) = (&self.id, &self.route.auth_index, &self.route.credential);
        match &self.proxy {
            Some(proxy) => tracing::info!(
                media_session_id = %id,
                call_id,
                auth_index = %auth_index,
                credential = %credential,
                connection = %format!("via {} proxy", proxy.scheme),
                remote_transport = "tcp",
                proxy_scheme = %proxy.scheme,
                "codex live remote media forwarding started"
            ),
            None => tracing::info!(
                media_session_id = %id,
                call_id,
                auth_index = %auth_index,
                credential = %credential,
                connection = "direct",
                remote_transport = "ice",
                "codex live remote media forwarding started"
            ),
        }
    }

    /// Registers a new peer; one created after the session closed is closed at once.
    fn register(&self, pc: &Arc<dyn PeerConnection>) {
        let mut created = self.created.lock().unwrap_or_else(PoisonError::into_inner);
        if self.closed.load(Ordering::SeqCst) {
            let pc = pc.clone();
            tokio::spawn(async move {
                let _ = pc.close().await;
            });
            return;
        }
        created.push(pc.clone());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs a relay task until the session closes. The closed check and the registration
    /// happen under the task-list lock that `close` drains, so none escapes.
    fn spawn(&self, task: impl std::future::Future<Output = ()> + Send + 'static) {
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        tasks.push(tokio::spawn(task).abort_handle());
    }

    /// Waits for ICE gathering to complete, at most [`GATHER_BOUND`]; past it the local
    /// description carries the candidates gathered so far. A session closed meanwhile
    /// fails at once instead of going on with negotiation.
    async fn gathered(&self, side: Side) -> Result<(), String> {
        let closed = || self.closed.load(Ordering::SeqCst);
        let mut rx = self.gathered[side as usize].subscribe();
        let waited = tokio::time::timeout(GATHER_BOUND, rx.wait_for(|done| *done || closed())).await;
        if closed() {
            return Err("peer connection closed".into());
        }
        match waited {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(_)) => Err("peer connection closed".into()),
            Err(_) => {
                tracing::debug!(
                    media_session_id = %self.id,
                    peer = side.name(),
                    "codex live WebRTC gathering incomplete; continuing with the gathered candidates"
                );
                Ok(())
            }
        }
    }

    /// Binds a data channel to its side: a reader that forwards its messages to the other
    /// side's queue, and a writer that sends this side's queue once the channel is open.
    fn attach(self: &Arc<Self>, side: Side, channel: Arc<dyn DataChannel>) {
        if self.channels[side as usize].set(channel.clone()).is_err() {
            tokio::spawn(async move {
                let _ = channel.close().await;
            });
            return;
        }
        let rx = self.receivers[side as usize]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let (open_tx, mut open_rx) = watch::channel(false);
        let me = Arc::downgrade(self);
        let to_other = self.queues[side.other() as usize].clone();
        let reader = channel.clone();
        let name = format!("{}-to-{}", side.name(), side.other().name());
        self.spawn(async move {
            while let Some(event) = reader.poll().await {
                match event {
                    DataChannelEvent::OnOpen => {
                        let _ = open_tx.send_replace(true);
                    }
                    DataChannelEvent::OnMessage(message) => {
                        if message.data.len() > MAX_MESSAGE {
                            fail(&me, &format!("{name} DataChannel message exceeds {MAX_MESSAGE} bytes"));
                            return;
                        }
                        if to_other.send(message).await.is_err() {
                            return;
                        }
                    }
                    DataChannelEvent::OnError => {
                        fail(&me, &format!("{name} DataChannel error"));
                        return;
                    }
                    DataChannelEvent::OnClose => break,
                    _ => {}
                }
            }
            fail(&me, &format!("{name} DataChannel closed"));
        });
        let Some(mut rx) = rx else { return };
        let me = Arc::downgrade(self);
        self.spawn(async move {
            if open_rx.wait_for(|open| *open).await.is_err() {
                return;
            }
            while let Some(message) = rx.recv().await {
                let sent = match (message.is_string, std::str::from_utf8(&message.data)) {
                    (true, Ok(text)) => channel.send_text(text).await,
                    _ => channel.send(message.data).await,
                };
                if let Err(e) = sent {
                    fail(&me, &format!("send DataChannel message: {e}"));
                    return;
                }
            }
        });
    }

    /// `CloseWithReason`: idempotent; stops every relay task, closes both peers and
    /// releases the session slot.
    fn close(&self, reason: &str) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let call_id = self.lock().call_id.clone();
        tracing::info!(media_session_id = %self.id, call_id, reason, "codex live WebRTC media session closing");
        // Wakes a setup waiting for gathering; it sees `closed` and fails.
        for gathered in &self.gathered {
            gathered.send_modify(|_| {});
        }
        self.close_tunnels();
        for task in self.tasks.lock().unwrap_or_else(PoisonError::into_inner).drain(..) {
            task.abort();
        }
        let channels: Vec<_> = self.channels.iter().filter_map(|c| c.get().cloned()).collect();
        let pcs = std::mem::take(&mut *self.created.lock().unwrap_or_else(PoisonError::into_inner));
        // The slot is released only after both peers closed and freed their ports.
        let slot = self.lock().slot.take();
        let hooks = self.after_close.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    for channel in channels {
                        let _ = channel.close().await;
                    }
                    for pc in pcs {
                        let _ = pc.close().await;
                    }
                    drop(slot);
                    run_after_close(&hooks);
                });
            }
            Err(_) => run_after_close(&hooks),
        }
    }
}

/// A data channel failure (`dataChannelPipe.reportError`).
fn fail(shared: &Weak<Shared>, detail: &str) {
    let Some(shared) = shared.upgrade() else { return };
    fail_shared(&shared, "data_channel_failed", detail);
}

/// `pionMediaSession.fail`: closes the session once with `reason` and tells the call.
fn fail_shared(shared: &Shared, reason: &str, detail: &str) {
    if shared.failed.swap(true, Ordering::SeqCst) {
        return;
    }
    tracing::warn!(media_session_id = %shared.id, reason, detail, "codex live WebRTC media session failed");
    let code = reason.to_owned();
    shared.close(&code);
    let handler = {
        let mut state = shared.lock();
        state.failure = Some(code.clone());
        if state.handler_called {
            None
        } else {
            state.handler_called = state.handler.is_some();
            state.handler.take()
        }
    };
    if let Some(handler) = handler {
        handler(code);
    }
}

struct Handler {
    shared: Weak<Shared>,
    side: Side,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete
            && let Some(shared) = self.shared.upgrade()
        {
            shared.gathered[self.side as usize].send_replace(true);
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let Some(shared) = self.shared.upgrade() else { return };
        tracing::debug!(media_session_id = %shared.id, peer = self.side.name(), %state, "codex live WebRTC peer state changed");
        match state {
            RTCPeerConnectionState::Connected if self.side == Side::Up => shared.forwarding_started(),
            RTCPeerConnectionState::Failed => {
                fail_shared(
                    &shared,
                    &format!("{}_failed", self.side.name()),
                    "PeerConnection failed",
                );
            }
            RTCPeerConnectionState::Closed if !shared.closed.load(Ordering::SeqCst) => {
                fail_shared(
                    &shared,
                    &format!("{}_closed", self.side.name()),
                    "PeerConnection closed",
                );
            }
            _ => {}
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let Some(shared) = self.shared.upgrade() else { return };
        if track.kind().await != RtpCodecKind::Audio {
            return;
        }
        let me = Arc::downgrade(&shared);
        let to = self.side.other();
        shared.spawn(async move {
            while let Some(event) = track.poll().await {
                let TrackRemoteEvent::OnRtpPacket(mut packet) = event else {
                    continue;
                };
                let Some(shared) = me.upgrade() else { return };
                let Some(output) = shared.outputs[to as usize].get() else {
                    continue;
                };
                let Some(payload_type) = output.payload_type().await else {
                    continue;
                };
                // `normalizeRTPPacket`: header extensions are not negotiated across legs.
                packet.header.extension = false;
                packet.header.extension_profile = 0;
                packet.header.extensions.clear();
                packet.header.ssrc = output.ssrc;
                packet.header.payload_type = payload_type;
                if output.track.write_rtp(packet).await.is_err() {
                    return;
                }
            }
        });
    }

    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        let Some(shared) = self.shared.upgrade() else { return };
        if self.side != Side::Down {
            return;
        }
        if channel.label().await.ok().as_deref() != Some(LABEL) {
            let _ = channel.close().await;
            return;
        }
        shared.attach(Side::Down, channel);
    }
}

/// The handle the call keeps (`pionMediaSession`).
struct Session(Arc<Shared>);

impl MediaSession for Session {
    fn accept_upstream_answer(&self, answer: String) -> BoxFuture<'_, Result<String, RelayError>> {
        Box::pin(async move {
            let fail = |what: &str, e: &dyn std::fmt::Display| RelayError::new(format!("{what}: {e}"));
            let Some([down, up]) = self.0.pcs.get().cloned() else {
                return Err(RelayError::new("Codex live media session unavailable"));
            };
            let mut dial = Vec::new();
            let answer = match &self.0.proxy {
                None => answer,
                Some(proxy) => {
                    let shared = Arc::downgrade(&self.0);
                    let prepared = tunnel::prepare_answer(
                        &answer,
                        self.0.local_offer.get().map_or("", String::as_str),
                        proxy.dialer.clone(),
                        Arc::new(move || {
                            if let Some(shared) = shared.upgrade() {
                                shared.forwarding_started();
                            }
                        }),
                    )
                    .map_err(RelayError::new)?;
                    for tunnel in &prepared.tunnels {
                        tracing::debug!(
                            media_session_id = %self.0.id,
                            target = %tunnel.target,
                            listener = %tunnel.listener,
                            "codex live TCP proxy: candidate tunnel ready"
                        );
                        dial.push(RTCIceCandidateInit {
                            candidate: format!("candidate:{}", tunnel.candidate),
                            sdp_mid: tunnel.mid.clone(),
                            sdp_mline_index: Some(tunnel.mline),
                            ..Default::default()
                        });
                    }
                    if !self.0.install_tunnels(prepared.tunnels) {
                        return Err(RelayError::new(
                            "Codex live media session closed while configuring TCP proxy",
                        ));
                    }
                    prepared.sdp
                }
            };
            let applied = async {
                let answer = RTCSessionDescription::answer(answer)?;
                up.set_remote_description(answer).await?;
                // webrtc-rs dials a remote TCP passive candidate only when it is added on
                // its own; the copy in the answer is a duplicate it ignores.
                for candidate in dial {
                    up.add_ice_candidate(candidate).await?;
                }
                Ok::<_, webrtc::error::Error>(())
            };
            if let Err(e) = applied.await {
                self.0.close_tunnels();
                return Err(fail("set upstream WebRTC answer", &e));
            }
            let answer = down
                .create_answer(None)
                .await
                .map_err(|e| fail("create downstream WebRTC answer", &e))?;
            down.set_local_description(answer)
                .await
                .map_err(|e| fail("set downstream WebRTC answer", &e))?;
            self.0
                .gathered(Side::Down)
                .await
                .map_err(|e| fail("gather downstream WebRTC candidates", &e))?;
            match down.local_description().await {
                Some(local) if !local.sdp.trim().is_empty() => Ok(advertise(&local.sdp, &self.0.public_ip)),
                _ => Err(RelayError::new("downstream WebRTC answer is empty")),
            }
        })
    }

    fn set_call_id(&self, call_id: &str) {
        self.0.lock().call_id = call_id.trim().to_owned();
    }

    /// Called at once when the session already failed (Go `SetCloseHandler`).
    fn set_close_handler(&self, handler: super::relay::CloseHandler) {
        let mut state = self.0.lock();
        match state.failure.clone() {
            Some(reason) if !state.handler_called => {
                state.handler_called = true;
                drop(state);
                handler(reason);
            }
            Some(_) => {}
            None => state.handler = Some(handler),
        }
    }

    fn close(&self, reason: &str) {
        self.0.close(reason);
    }

    fn after_close(&self, then: Box<dyn FnOnce() + Send>) {
        self.0.on_closed(then);
    }
}

#[cfg(test)]
#[path = "media_tests.rs"]
mod tests;
