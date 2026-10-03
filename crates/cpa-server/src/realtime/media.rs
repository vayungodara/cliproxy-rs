//! The WebRTC media relay (media.go `pionMediaRelay`) on webrtc-rs, a pion port. Feature
//! `media-relay`.
//!
//! Each call gets two peer connections: one answering the client's offer (downstream) and
//! one offering to the Codex upstream. Opus RTP and the `oai-events` data channel are
//! forwarded between them; anything else is dropped. Data channel messages over 256 KiB,
//! a failed or remotely closed peer, or a failed data channel end the session.
//!
//! ponytail: three pion behaviours have no webrtc-rs 0.21 equivalent. (1) Upstream proxies:
//! Go tunnels the upstream media over TCP through the credential's proxy (tcp_proxy.go);
//! here a proxied credential fails the call (502) rather than sending media around the
//! proxy. (2) `disable-private-remote-ips` filters the client's offered candidates, not
//! peer-reflexive ones learned from STUN. (3) Host candidates are IPv4 only.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
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
    RTCIceCandidateType, RTCIceGatheringState, RTCIceServer, RTCPeerConnectionState, RTCSessionDescription, Registry,
    SettingEngineBuilder, register_default_interceptors,
};
use webrtc::rtp_transceiver::RtpSender;

use super::relay::{Limiter, MediaRelay, MediaSession, NewSession, RelayConfig, RelayError, Route, Slot};

/// `realtimeDataChannelLabel`.
const LABEL: &str = "oai-events";
/// `mediaDataQueueSize`, `mediaDataMessageMaxSize`, `mediaDataBufferedMaxSize`.
const QUEUE: usize = 64;
const MAX_MESSAGE: usize = 256 << 10;
const BUFFERED_MAX: usize = 1 << 20;
/// Opus as Go registers it (payload type 111).
const OPUS_FMTP: &str = "minptime=10;useinbandfec=1";
// ponytail: Go waits for ICE gathering until the request ends; a fixed bound keeps an
// unreachable STUN/TURN server from holding a session slot.
const GATHER_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct Relay {
    config: RelayConfig,
    limiter: Arc<Limiter>,
    /// Bind and advertise only this address (tests use loopback).
    bind_ip: Option<IpAddr>,
}

impl Relay {
    pub fn new(config: &RelayConfig, limiter: Arc<Limiter>) -> Result<Self, String> {
        Ok(Self {
            config: config.clone(),
            limiter,
            bind_ip: None,
        })
    }

    /// One peer connection with Go's media and network settings. `port_range` tries each
    /// free port of `udp-port-min..=udp-port-max` until one binds.
    async fn peer(&self, side: Side, shared: &Arc<Shared>) -> Result<Arc<dyn PeerConnection>, String> {
        let ip = self.bind_ip.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let (min, max) = (self.config.udp_port_min, self.config.udp_port_max);
        let ports: Vec<u16> = if min == 0 {
            vec![0]
        } else {
            let span = u32::from(max - min) + 1;
            let start = getrandom::u32().unwrap_or(0) % span;
            (0..span).map(|i| min + ((start + i) % span) as u16).collect()
        };
        let mut last = String::from("no free UDP port in the codex.live-media-relay range");
        for port in ports {
            if port != 0 && std::net::UdpSocket::bind((ip, port)).is_err() {
                continue;
            }
            let addr = SocketAddr::new(ip, port);
            match self.build(side, shared, addr, true).await {
                Ok(pc) => return Ok(pc),
                // pion only logs a failed mDNS socket (no multicast interface); webrtc-rs
                // fails the peer, so retry without mDNS queries.
                Err(e) if e.contains("No such device") || e.contains("multicast") => {
                    tracing::debug!(error = %e, "codex live media: mDNS unavailable; continuing without it");
                    match self.build(side, shared, addr, false).await {
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
        addr: SocketAddr,
        mdns: bool,
    ) -> Result<Arc<dyn PeerConnection>, String> {
        let mut media = MediaEngine::default();
        media
            .register_codec(opus_parameters(), RtpCodecKind::Audio)
            .map_err(|e| format!("register Opus codec: {e}"))?;
        let registry = register_default_interceptors(Registry::new(), &mut media)
            .map_err(|e| format!("register WebRTC interceptors: {e}"))?;
        let mut settings = SettingEngineBuilder::new();
        let public_ip = self.config.public_ip.trim();
        if !public_ip.is_empty() {
            settings = settings.with_nat_1to1_ips(vec![public_ip.to_owned()], RTCIceCandidateType::Host);
        }
        if addr.ip().is_loopback() {
            settings = settings.with_include_loopback_candidate(true);
        }
        if !mdns {
            settings = settings.with_multicast_dns_mode(rtc::ice::mdns::MulticastDnsMode::Disabled);
        }
        let ice_servers = self
            .config
            .ice_servers
            .iter()
            .map(|s| RTCIceServer {
                urls: s.urls.iter().map(|u| u.trim().to_owned()).collect(),
                username: s.username.clone(),
                credential: s.credential.clone(),
            })
            .collect();
        let pc = PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().with_ice_servers(ice_servers).build())
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(settings.build())
            .with_handler(Arc::new(Handler {
                shared: Arc::downgrade(shared),
                side,
            }))
            .with_udp_addrs(vec![addr])
            .with_data_channel_send_buffer_limit(BUFFERED_MAX)
            .build()
            .await
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(pc))
    }

    async fn start(&self, offer: String, shared: &Arc<Shared>) -> Result<String, RelayError> {
        let fail = |what: &str, e: &dyn std::fmt::Display| RelayError::new(format!("{what}: {e}"));
        let down = self
            .peer(Side::Down, shared)
            .await
            .map_err(|e| fail("create downstream PeerConnection", &e))?;
        let up = self
            .peer(Side::Up, shared)
            .await
            .map_err(|e| fail("create upstream PeerConnection", &e));
        let up = match up {
            Ok(up) => up,
            Err(e) => {
                let _ = down.close().await;
                return Err(e);
            }
        };
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
        match up.local_description().await {
            Some(local) if !local.sdp.trim().is_empty() => Ok(local.sdp),
            _ => Err(RelayError::new("upstream WebRTC offer is empty")),
        }
    }
}

impl MediaRelay for Relay {
    fn new_session(&self, offer: String, route: Route) -> BoxFuture<'_, NewSession> {
        Box::pin(async move {
            match &route.proxy {
                cpa_exec::proxy::Proxy::Url(_) => {
                    return Err(RelayError::new(
                        "Codex live media relay cannot reach the upstream through a proxy in this build",
                    ));
                }
                cpa_exec::proxy::Proxy::Invalid => {
                    return Err(RelayError::new(
                        "configure Codex live remote TCP proxy: invalid proxy URL",
                    ));
                }
                _ => {}
            }
            let slot = self
                .limiter
                .acquire()
                .ok_or_else(|| RelayError::new("Codex live media relay capacity exhausted"))?;
            let shared = Shared::new(slot, route);
            tracing::info!(media_session_id = %shared.id, "codex live WebRTC media session created");
            match self.start(offer, &shared).await {
                Ok(sdp) => Ok((Arc::new(Session(shared)) as Arc<dyn MediaSession>, sdp)),
                Err(e) => {
                    shared.close("closed");
                    Err(e)
                }
            }
        })
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
    pcs: OnceLock<[Arc<dyn PeerConnection>; 2]>,
}

impl Shared {
    fn new(slot: Slot, route: Route) -> Arc<Self> {
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
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn spawn(&self, task: impl std::future::Future<Output = ()> + Send + 'static) {
        let handle = tokio::spawn(task).abort_handle();
        if self.closed.load(Ordering::SeqCst) {
            handle.abort();
            return;
        }
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner).push(handle);
    }

    async fn gathered(&self, side: Side) -> Result<(), String> {
        let mut rx = self.gathered[side as usize].subscribe();
        match tokio::time::timeout(GATHER_TIMEOUT, rx.wait_for(|done| *done)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(_)) => Err("peer connection closed".into()),
            Err(_) => Err("context deadline exceeded".into()),
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
        for task in self.tasks.lock().unwrap_or_else(PoisonError::into_inner).drain(..) {
            task.abort();
        }
        let channels: Vec<_> = self.channels.iter().filter_map(|c| c.get().cloned()).collect();
        let pcs: Vec<_> = self.pcs.get().map(|p| p.to_vec()).unwrap_or_default();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                for channel in channels {
                    let _ = channel.close().await;
                }
                for pc in pcs {
                    let _ = pc.close().await;
                }
            });
        }
        drop(self.lock().slot.take());
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
            // `logForwardingStarted`: once, with the credential and how media leaves.
            RTCPeerConnectionState::Connected
                if self.side == Side::Up && !shared.forwarding_logged.swap(true, Ordering::SeqCst) =>
            {
                tracing::info!(
                    media_session_id = %shared.id,
                    call_id = %shared.lock().call_id,
                    auth_index = %shared.route.auth_index,
                    credential = %shared.route.credential,
                    connection = "direct",
                    remote_transport = "ice",
                    "codex live remote media forwarding started"
                );
            }
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
            let answer = RTCSessionDescription::answer(answer).map_err(|e| fail("set upstream WebRTC answer", &e))?;
            up.set_remote_description(answer)
                .await
                .map_err(|e| fail("set upstream WebRTC answer", &e))?;
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
                Some(local) if !local.sdp.trim().is_empty() => Ok(local.sdp),
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
}

#[cfg(test)]
#[path = "media_tests.rs"]
mod tests;
