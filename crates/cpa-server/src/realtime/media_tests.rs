//! Go media_test.go: `TestIsPublicRemoteIP` and `TestPionMediaRelayBridgesAudioAndDataChannel`
//! (a client peer, the relay and a fake upstream peer, all on loopback).

use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use rtc::rtp;
use tokio::sync::{mpsc, watch};
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceGatheringState, RTCSessionDescription, Registry, SettingEngineBuilder, register_default_interceptors,
};

use super::*;
use crate::realtime::relay::{Limiter, MediaRelay, Route};

#[test]
fn public_remote_ips_match_go() {
    for (ip, want) in [
        ("8.8.8.8", true),
        ("2001:4860::1", true),
        ("127.0.0.1", false),
        ("10.0.0.1", false),
        ("169.254.1.1", false),
        ("224.0.0.1", false),
        ("::1", false),
        ("fc00::1", false),
        ("fe80::1", false),
        ("ff02::1", false),
        ("0.0.0.0", false),
        ("::ffff:10.0.0.1", false),
        ("::ffff:8.8.8.8", true),
    ] {
        assert_eq!(is_public_remote_ip(ip.parse().unwrap()), want, "{ip}");
    }
    let offer = "v=0\r\na=candidate:1 1 udp 1 8.8.8.8 5000 typ host\r\na=candidate:2 1 udp 1 192.168.1.2 5000 typ host\r\na=candidate:3 1 udp 1 abc.local 5000 typ host\r\na=end\r\n";
    assert_eq!(
        public_candidates_only(offer),
        "v=0\r\na=candidate:1 1 udp 1 8.8.8.8 5000 typ host\r\na=end\r\n"
    );
}

/// What a test peer reports.
struct Peer {
    pc: Arc<dyn PeerConnection>,
    gathered: watch::Receiver<bool>,
    tracks: mpsc::Receiver<Arc<dyn TrackRemote>>,
    channels: mpsc::Receiver<Arc<dyn DataChannel>>,
}

struct PeerEvents {
    gathered: watch::Sender<bool>,
    tracks: mpsc::Sender<Arc<dyn TrackRemote>>,
    channels: mpsc::Sender<Arc<dyn DataChannel>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for PeerEvents {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathered.send_replace(true);
        }
    }
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let _ = self.tracks.send(track).await;
    }
    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        let _ = self.channels.send(channel).await;
    }
}

async fn loopback_peer() -> Peer {
    peer_on(false, "127.0.0.1").await
}

/// A UDP loopback peer, or with `tcp` one that only listens for ICE-TCP (a passive
/// candidate on 127.0.0.1), like an upstream reachable over TCP 443.
async fn peer_on(tcp: bool, ip: &str) -> Peer {
    let addr = std::net::SocketAddr::new(ip.parse().unwrap(), 0).to_string();
    let mut media = MediaEngine::default();
    media.register_codec(opus_parameters(), RtpCodecKind::Audio).unwrap();
    let registry = register_default_interceptors(Registry::new(), &mut media).unwrap();
    let (gathered_tx, gathered) = watch::channel(false);
    let (tracks_tx, tracks) = mpsc::channel(4);
    let (channels_tx, channels) = mpsc::channel(4);
    let mut settings = SettingEngineBuilder::new()
        .with_include_loopback_candidate(true)
        .with_multicast_dns_mode(rtc::ice::mdns::MulticastDnsMode::Disabled);
    let (udp, tcp) = if tcp {
        settings = settings.with_network_types(vec![rtc::ice::network_type::NetworkType::Tcp4]);
        (vec![], vec![addr])
    } else {
        (vec![addr], vec![])
    };
    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_setting_engine(settings.build())
        .with_handler(Arc::new(PeerEvents {
            gathered: gathered_tx,
            tracks: tracks_tx,
            channels: channels_tx,
        }))
        .with_udp_addrs(udp)
        .with_tcp_addrs(tcp)
        .build()
        .await
        .unwrap();
    Peer {
        pc: Arc::new(pc),
        gathered,
        tracks,
        channels,
    }
}

async fn complete(peer: &mut Peer, description: RTCSessionDescription) -> String {
    peer.pc.set_local_description(description).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), peer.gathered.wait_for(|g| *g))
        .await
        .unwrap()
        .unwrap();
    peer.pc.local_description().await.unwrap().sdp
}

/// A sending audio track and the sender to read its negotiated payload type from.
async fn audio(pc: &Arc<dyn PeerConnection>) -> Output {
    let track = Arc::new(TrackLocalStaticRTP::new(opus_track()));
    let sender = pc.add_track(track.clone()).await.unwrap();
    let ssrc = track.ssrcs().await[0];
    Output {
        track,
        sender,
        ssrc,
        payload_type: OnceLock::new(),
    }
}

/// Sends `payload` every 20 ms until `track` receives an RTP packet; returns its payload.
async fn rtp_flows(from: &Output, track: &mut mpsc::Receiver<Arc<dyn TrackRemote>>, payload: &'static [u8]) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut seq = 0u16;
    let mut remote: Option<Arc<dyn TrackRemote>> = None;
    loop {
        assert!(tokio::time::Instant::now() < deadline, "no RTP relayed");
        if let Some(pt) = from.payload_type().await {
            seq = seq.wrapping_add(1);
            let packet = rtp::Packet {
                header: rtp::header::Header {
                    version: 2,
                    payload_type: pt,
                    sequence_number: seq,
                    timestamp: u32::from(seq) * 960,
                    ssrc: from.ssrc,
                    ..Default::default()
                },
                payload: bytes::Bytes::from_static(payload),
            };
            let _ = from.track.write_rtp(packet).await;
        }
        if remote.is_none() {
            remote = track.try_recv().ok();
        }
        if let Some(remote) = &remote
            && let Ok(Some(TrackRemoteEvent::OnRtpPacket(packet))) =
                tokio::time::timeout(Duration::from_millis(20), remote.poll()).await
        {
            return packet.payload.to_vec();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn next_message(channel: &Arc<dyn DataChannel>) -> String {
    loop {
        match tokio::time::timeout(Duration::from_secs(15), channel.poll()).await {
            Ok(Some(DataChannelEvent::OnMessage(m))) => return String::from_utf8(m.data.to_vec()).unwrap(),
            Ok(Some(_)) => continue,
            other => panic!("no data channel message: {:?}", other.is_ok()),
        }
    }
}

async fn wait_open(channel: &Arc<dyn DataChannel>) {
    loop {
        match tokio::time::timeout(Duration::from_secs(15), channel.poll()).await {
            Ok(Some(DataChannelEvent::OnOpen)) => return,
            Ok(Some(_)) => continue,
            _ => panic!("data channel never opened"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relays_audio_and_data_channel_between_peers() {
    let config = RelayConfig {
        enabled: true,
        max_sessions: 1,
        ..RelayConfig::default()
    };
    let limiter = Arc::new(Limiter::default());
    limiter.set_limit(config.max_sessions());
    let relay = Relay {
        config,
        limiter,
        bind_ip: Some("127.0.0.1".parse().unwrap()),
    };
    let route = || Route {
        proxy_url: String::new(),
        credential: "Voice credential".into(),
        auth_index: "auth-index".into(),
    };

    // Client: an audio track and the oai-events channel, like the Codex desktop app.
    let mut client = loopback_peer().await;
    let client_audio = audio(&client.pc).await;
    let client_channel = client.pc.create_data_channel(LABEL, None).await.unwrap();
    let offer = client.pc.create_offer(None).await.unwrap();
    let client_offer = complete(&mut client, offer).await;

    let (session, relay_offer) = relay
        .new_session(client_offer.clone(), route(), Box::new(()))
        .await
        .unwrap();
    assert_ne!(relay_offer, client_offer, "the relay offers its own session upstream");
    let busy = relay.new_session(client_offer, route(), Box::new(())).await;
    assert_eq!(
        busy.err().map(|e| e.message),
        Some("Codex live media relay capacity exhausted".to_owned()),
        "max-sessions 1"
    );

    // Upstream: answers the relay's offer with its own audio track.
    let mut upstream = loopback_peer().await;
    upstream
        .pc
        .set_remote_description(RTCSessionDescription::offer(relay_offer).unwrap())
        .await
        .unwrap();
    let upstream_audio = audio(&upstream.pc).await;
    let answer = upstream.pc.create_answer(None).await.unwrap();
    let upstream_answer = complete(&mut upstream, answer).await;

    session.set_call_id("call-relay");
    let downstream_answer = session.accept_upstream_answer(upstream_answer).await.unwrap();
    client
        .pc
        .set_remote_description(RTCSessionDescription::answer(downstream_answer).unwrap())
        .await
        .unwrap();

    // Audio both ways.
    assert_eq!(
        rtp_flows(&client_audio, &mut upstream.tracks, b"to-openai").await,
        b"to-openai"
    );
    assert_eq!(
        rtp_flows(&upstream_audio, &mut client.tracks, b"to-desktop").await,
        b"to-desktop"
    );

    // Data channel both ways, text and binary kept apart.
    let upstream_channel = tokio::time::timeout(Duration::from_secs(15), upstream.channels.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(upstream_channel.label().await.unwrap(), LABEL);
    wait_open(&client_channel).await;
    client_channel.send_text("session.update").await.unwrap();
    assert_eq!(next_message(&upstream_channel).await, "session.update");
    upstream_channel
        .send(BytesMut::from(&b"response.done"[..]))
        .await
        .unwrap();
    assert_eq!(next_message(&client_channel).await, "response.done");

    // The close handler runs when the media ends on its own; an explicit close frees the slot.
    let (closed_tx, closed_rx) = std::sync::mpsc::channel();
    session.set_close_handler(Box::new(move |reason| {
        let _ = closed_tx.send(reason);
    }));
    session.close("test_done");
    assert!(closed_rx.try_recv().is_err(), "an explicit close is not a failure");
    let (second, _) = relay
        .new_session(
            {
                let mut again = loopback_peer().await;
                let offer = again.pc.create_offer(None).await.unwrap();
                let _ = again.pc.create_data_channel(LABEL, None).await;
                let offer = again.pc.create_offer(None).await.unwrap_or(offer);
                complete(&mut again, offer).await
            },
            route(),
            Box::new(()),
        )
        .await
        .expect("closing released the session slot");
    second.close("test_done");
    let _ = client.pc.close().await;
    let _ = upstream.pc.close().await;

    // An unusable proxy fails the call before a slot is taken, with Go's message.
    let invalid = Route {
        proxy_url: "ftp://proxy:21".into(),
        ..route()
    };
    let refused = relay
        .new_session("v=0\r\n".into(), invalid, Box::new(()))
        .await
        .err()
        .unwrap();
    assert_eq!(
        (refused.status, refused.message.as_str()),
        (
            502,
            "configure Codex live remote TCP proxy: unsupported proxy scheme: ftp"
        )
    );
}

/// Go's ICE URL parsing supplies default ports; webrtc-rs needs them explicit.
#[test]
fn ice_urls_get_go_default_ports() {
    assert_eq!(ice_url("stun:stun.example.org"), "stun:stun.example.org:3478");
    assert_eq!(ice_url("stun:stun.example.org:19302"), "stun:stun.example.org:19302");
    assert_eq!(
        ice_url("turn:turn.example.org?transport=udp"),
        "turn:turn.example.org:3478?transport=udp"
    );
    assert_eq!(ice_url("turns:turn.example.org"), "turns:turn.example.org:5349");
    assert_eq!(ice_url("stun:[2001:db8::1]"), "stun:[2001:db8::1]:3478");
    assert_eq!(ice_url("stun:[2001:db8::1]:5000"), "stun:[2001:db8::1]:5000");
    let config = RelayConfig {
        ice_servers: vec![crate::realtime::relay::IceServer {
            urls: vec![
                "turn:a.example?transport=tcp".into(),
                "turns:b.example".into(),
                "turn:c.example".into(),
                "stun:d.example".into(),
            ],
            ..Default::default()
        }],
        ..RelayConfig::default()
    };
    assert_eq!(
        ungathered_ice_urls(&config),
        ["turn:a.example?transport=tcp", "turns:b.example"],
        "reported, not silently skipped"
    );
}

/// pion `SetNAT1To1IPs(public-ip, host)`: host candidates advertise the public address;
/// srflx, relay and IPv6 candidates are untouched.
#[test]
fn public_ip_replaces_ipv4_host_candidates() {
    let sdp = "v=0\r\na=candidate:1 1 udp 2130706431 10.0.0.5 50000 typ host\r\na=candidate:2 1 udp 1694498815 198.51.100.1 50000 typ srflx raddr 10.0.0.5 rport 50000\r\na=candidate:3 1 udp 2130706431 fd00::5 50001 typ host\r\na=end\r\n";
    assert_eq!(
        advertise(sdp, "203.0.113.7"),
        "v=0\r\na=candidate:1 1 udp 2130706431 203.0.113.7 50000 typ host\r\na=candidate:2 1 udp 1694498815 198.51.100.1 50000 typ srflx raddr 10.0.0.5 rport 50000\r\na=candidate:3 1 udp 2130706431 fd00::5 50001 typ host\r\na=end\r\n"
    );
    assert_eq!(advertise(sdp, ""), sdp, "unset");
    assert_eq!(
        advertise(sdp, "2001:db8::7"),
        "v=0\r\na=candidate:1 1 udp 2130706431 10.0.0.5 50000 typ host\r\na=candidate:2 1 udp 1694498815 198.51.100.1 50000 typ srflx raddr 10.0.0.5 rport 50000\r\na=candidate:3 1 udp 2130706431 2001:db8::7 50001 typ host\r\na=end\r\n",
        "an IPv6 public-ip maps only IPv6 host candidates"
    );
    assert_eq!(
        advertise(sdp, " ::ffff:203.0.113.7 "),
        advertise(sdp, "203.0.113.7"),
        "an IPv4-mapped public-ip is IPv4"
    );
    assert_eq!(advertise(sdp, "relay.example.com"), sdp, "not an IP");
}

/// Two consecutive loopback UDP ports that were free when checked.
// ponytail: the relay binds a configured port range, not sockets it is handed, so another
// test or process can take a port between this check and the relay's bind. The caller
// retries its first setup with a new pair; a port taken between a release and the next
// setup still fails it. Letting the relay take bound sockets would close both gaps.
fn free_port_pair() -> u16 {
    (20000..60000)
        .step_by(7)
        .find(|p| {
            std::net::UdpSocket::bind(("127.0.0.1", *p)).is_ok()
                && std::net::UdpSocket::bind(("127.0.0.1", *p + 1)).is_ok()
        })
        .expect("two free ports")
}

/// A request cancelled mid-negotiation (Go: request context ends while gathering) closes
/// both peers and returns the slot: with one session allowed and only two ports, a new
/// session can start again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_setup_frees_ports_and_slot() {
    // A STUN server that never answers keeps upstream gathering pending.
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let limiter = Arc::new(Limiter::default());
    limiter.set_limit(1);
    let route = || Route {
        proxy_url: String::new(),
        credential: "c".into(),
        auth_index: "i".into(),
    };
    let mut client = loopback_peer().await;
    let _ = client.pc.create_data_channel(LABEL, None).await.unwrap();
    let offer = client.pc.create_offer(None).await.unwrap();
    let offer = complete(&mut client, offer).await;

    // The relay reports a port of its range that is in use as "no free UDP port"; that
    // only means something else took one since `free_port_pair`, so try another pair.
    let mut retries = 3;
    let (relay, slot_free) = loop {
        let port = free_port_pair();
        let relay = Relay {
            config: RelayConfig {
                enabled: true,
                max_sessions: 1,
                udp_port_min: port,
                udp_port_max: port + 1,
                ice_servers: vec![crate::realtime::relay::IceServer {
                    urls: vec![format!("stun:{}", silent.local_addr().unwrap())],
                    ..Default::default()
                }],
                ..RelayConfig::default()
            },
            limiter: limiter.clone(),
            bind_ip: Some("127.0.0.1".parse().unwrap()),
        };
        let (probe, slot_free) = Probe::new(&limiter);
        let first = tokio::time::timeout(
            Duration::from_millis(300),
            relay.new_session(offer.clone(), route(), Box::new(probe)),
        )
        .await;
        let message = match first {
            Err(_) => break (relay, slot_free),
            Ok(result) => result.err().map(|e| e.message).unwrap_or_default(),
        };
        assert!(
            retries > 0 && message.contains("no free UDP port"),
            "setup is still gathering when the request goes away: {message}"
        );
        retries -= 1;
    };
    released_after_close(&slot_free).await;
    // A second attempt fails at once while the slot or a port is still held, and stays
    // pending (gathering) once both are free.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout(
            Duration::from_millis(300),
            relay.new_session(offer.clone(), route(), Box::new(())),
        )
        .await
        {
            Err(_) => break,
            Ok(result) => {
                let message = result.err().map(|e| e.message).unwrap_or_default();
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "slot or ports never released: {message}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    let _ = client.pc.close().await;
    drop(silent);
}

/// A setup hold that records, when dropped, whether the session's slot was already free.
/// The relay returns the slot only after both peers closed, so `Some(true)` means the hold
/// outlived the close.
struct Probe {
    limiter: Arc<Limiter>,
    slot_free: Arc<std::sync::Mutex<Option<bool>>>,
}

impl Probe {
    fn new(limiter: &Arc<Limiter>) -> (Self, Arc<std::sync::Mutex<Option<bool>>>) {
        let slot_free = Arc::default();
        let probe = Self {
            limiter: limiter.clone(),
            slot_free: Arc::clone(&slot_free),
        };
        (probe, slot_free)
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let free = self.limiter.acquire().is_some();
        *self.slot_free.lock().unwrap() = Some(free);
    }
}

async fn released_after_close(slot_free: &std::sync::Mutex<Option<bool>>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while slot_free.lock().unwrap().is_none() {
        assert!(tokio::time::Instant::now() < deadline, "the hold was never released");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        *slot_free.lock().unwrap(),
        Some(true),
        "the hold was released before the session finished closing"
    );
}

/// A setup that fails closes its half-built session and returns the error only after
/// the close finished; the hold is released by then, and not before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_setup_releases_its_hold_after_the_close() {
    let config = RelayConfig {
        enabled: true,
        max_sessions: 1,
        ..RelayConfig::default()
    };
    let limiter = Arc::new(Limiter::default());
    limiter.set_limit(config.max_sessions());
    let relay = Relay {
        config,
        limiter: limiter.clone(),
        bind_ip: Some("127.0.0.1".parse().unwrap()),
    };
    let route = Route {
        proxy_url: String::new(),
        credential: "c".into(),
        auth_index: "i".into(),
    };
    let (probe, slot_free) = Probe::new(&limiter);
    let result = relay.new_session("not an offer".into(), route, Box::new(probe)).await;
    assert!(result.is_err());
    assert_eq!(
        *slot_free.lock().unwrap(),
        Some(true),
        "released after the close, before the error"
    );
}

/// What the test SOCKS5 proxy saw: the targets asked for, and how many tunnels ended.
#[derive(Default)]
struct ProxyLog {
    targets: std::sync::Mutex<Vec<String>>,
    ended: std::sync::atomic::AtomicUsize,
}

/// A SOCKS5 proxy on loopback that sends every connection to `upstream` (set later).
async fn socks_proxy(upstream: Arc<OnceLock<std::net::SocketAddr>>) -> (u16, Arc<ProxyLog>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log = Arc::new(ProxyLog::default());
    let seen = log.clone();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let (seen, upstream) = (seen.clone(), upstream.clone());
            tokio::spawn(async move {
                let mut greeting = [0u8; 3];
                client.read_exact(&mut greeting).await.ok()?;
                client.write_all(&[5, 0]).await.ok()?;
                let mut request = [0u8; 10];
                client.read_exact(&mut request).await.ok()?;
                let ip = std::net::Ipv4Addr::new(request[4], request[5], request[6], request[7]);
                let port = u16::from_be_bytes([request[8], request[9]]);
                seen.targets.lock().unwrap().push(format!("{ip}:{port}"));
                let mut server = tokio::net::TcpStream::connect(*upstream.get()?).await.ok()?;
                client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.ok()?;
                let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                seen.ended.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some(())
            });
        }
    });
    (port, log)
}

/// Go `TestPionActiveTCPCandidatePassesTunnelAuthentication`, end to end: with a proxied
/// credential the relay's upstream peer has loopback candidates only, the upstream's TCP
/// passive candidate on a public address is rewritten to a local tunnel, and media flows
/// through the SOCKS5 proxy to that fixed address.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxied_credentials_relay_media_through_the_proxy() {
    let config = RelayConfig {
        enabled: true,
        max_sessions: 1,
        public_ip: "198.51.100.7".into(),
        ..RelayConfig::default()
    };
    let limiter = Arc::new(Limiter::default());
    limiter.set_limit(config.max_sessions());
    let relay = Relay {
        config,
        limiter,
        bind_ip: Some("127.0.0.1".parse().unwrap()),
    };
    let upstream_addr = Arc::new(OnceLock::new());
    let (proxy_port, proxy_log) = socks_proxy(upstream_addr.clone()).await;
    let route = Route {
        proxy_url: format!("socks5://127.0.0.1:{proxy_port}"),
        credential: "Voice credential".into(),
        auth_index: "auth-index".into(),
    };

    let mut client = loopback_peer().await;
    let client_audio = audio(&client.pc).await;
    let client_channel = client.pc.create_data_channel(LABEL, None).await.unwrap();
    let offer = client.pc.create_offer(None).await.unwrap();
    let client_offer = complete(&mut client, offer).await;
    let (session, relay_offer) = relay.new_session(client_offer, route, Box::new(())).await.unwrap();
    let offered: Vec<&str> = relay_offer.lines().filter(|l| l.starts_with("a=candidate:")).collect();
    assert!(!offered.is_empty());
    for candidate in &offered {
        let address = candidate.split(' ').nth(4).unwrap();
        assert!(
            address.parse::<IpAddr>().unwrap().is_loopback(),
            "loopback only, no public-ip, no STUN: {candidate}"
        );
    }
    assert!(
        offered
            .iter()
            .any(|c| c.contains(" tcp ") && c.contains("tcptype active"))
    );

    // The upstream answers from 127.0.0.1:P; the relay is told 20.42.0.20:443.
    let mut upstream = peer_on(true, "127.0.0.1").await;
    upstream
        .pc
        .set_remote_description(RTCSessionDescription::offer(relay_offer).unwrap())
        .await
        .unwrap();
    let upstream_audio = audio(&upstream.pc).await;
    let answer = upstream.pc.create_answer(None).await.unwrap();
    let upstream_answer = complete(&mut upstream, answer).await;
    let mut public_answer = String::new();
    for line in upstream_answer.split_inclusive("\r\n") {
        if line.starts_with("a=candidate:") && line.contains(" tcp ") && line.contains("tcptype passive") {
            let mut fields: Vec<&str> = line.trim_end().split(' ').collect();
            let _ = upstream_addr.set(format!("{}:{}", fields[4], fields[5]).parse().unwrap());
            fields[4] = "20.42.0.20";
            fields[5] = "443";
            public_answer.push_str(&fields.join(" "));
            public_answer.push_str("\r\n");
        } else {
            public_answer.push_str(line);
        }
    }
    assert!(
        upstream_addr.get().is_some(),
        "the upstream offers a TCP passive candidate"
    );

    session.set_call_id("call-proxied");
    let downstream_answer = session.accept_upstream_answer(public_answer).await.unwrap();
    client
        .pc
        .set_remote_description(RTCSessionDescription::answer(downstream_answer).unwrap())
        .await
        .unwrap();

    assert_eq!(
        rtp_flows(&client_audio, &mut upstream.tracks, b"to-openai").await,
        b"to-openai"
    );
    assert_eq!(
        rtp_flows(&upstream_audio, &mut client.tracks, b"to-desktop").await,
        b"to-desktop"
    );
    let upstream_channel = tokio::time::timeout(Duration::from_secs(15), upstream.channels.recv())
        .await
        .unwrap()
        .unwrap();
    wait_open(&client_channel).await;
    client_channel.send_text("session.update").await.unwrap();
    assert_eq!(next_message(&upstream_channel).await, "session.update");
    upstream_channel
        .send(BytesMut::from(&b"response.done"[..]))
        .await
        .unwrap();
    assert_eq!(next_message(&client_channel).await, "response.done");
    let targets = proxy_log.targets.lock().unwrap().clone();
    assert!(!targets.is_empty());
    assert!(
        targets.iter().all(|t| t == "20.42.0.20:443"),
        "the proxy only dials the fixed candidate: {targets:?}"
    );
    assert_eq!(proxy_log.ended.load(std::sync::atomic::Ordering::SeqCst), 0);

    // Closing the session ends every proxied connection while both test peers stay up.
    let (closed_tx, closed_rx) = std::sync::mpsc::channel();
    session.set_close_handler(Box::new(move |reason| {
        let _ = closed_tx.send(reason);
    }));
    session.close("test_done");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while proxy_log.ended.load(std::sync::atomic::Ordering::SeqCst) < targets.len() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "proxied connections outlived the session"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(closed_rx.try_recv().is_err(), "an explicit close is not a failure");
    let _ = client.pc.close().await;
    let _ = upstream.pc.close().await;
}

/// Go `installCandidateTunnels` and `CloseWithReason`: closing the session closes tunnels
/// nobody claimed, and tunnels installed after the close are refused and closed.
#[tokio::test]
async fn session_close_closes_unclaimed_tunnels() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/codex_live_tunnel_go.json")).unwrap();
    let case = fixtures["prepare"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "go_rewrite")
        .unwrap()
        .clone();
    struct Never;
    impl Dial for Never {
        fn dial(&self, _: SocketAddr) -> BoxFuture<'_, Result<dialer::Conn, String>> {
            Box::pin(async { Err("never".to_owned()) })
        }
    }
    let prepare = || {
        tunnel::prepare_answer(
            case["answer"].as_str().unwrap(),
            case["offer"].as_str().unwrap(),
            Arc::new(Never),
            Arc::new(|| {}),
        )
        .unwrap()
    };
    let refused = |addr: SocketAddr| async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::net::TcpStream::connect(addr).await.is_ok() {
            if tokio::time::Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        true
    };
    let limiter = Arc::new(Limiter::default());
    limiter.set_limit(1);
    let route = Route {
        proxy_url: String::new(),
        credential: "c".into(),
        auth_index: "i".into(),
    };
    let shared = Shared::new(limiter.acquire().unwrap(), route, None, String::new());
    let installed = prepare();
    let listener = installed.tunnels[0].listener;
    assert!(shared.install_tunnels(installed.tunnels));
    assert!(
        tokio::net::TcpStream::connect(listener).await.is_ok(),
        "open while the session lives"
    );
    shared.close("test_done");
    assert!(refused(listener).await, "session close closed the unclaimed listener");
    let late = prepare();
    let listener = late.tunnels[0].listener;
    assert!(
        !shared.install_tunnels(late.tunnels),
        "a closed session refuses tunnels"
    );
    assert!(refused(listener).await, "refused tunnels are closed");
}

/// Host candidates of both families: the relay negotiates and relays audio and the data
/// channel over IPv6 (`::1` here; on a host with IPv6 interfaces it also binds `[::]`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relays_over_ipv6() {
    if std::net::UdpSocket::bind((std::net::Ipv6Addr::LOCALHOST, 0)).is_err() {
        // CI sets CPA_TEST_NO_SKIP: the harness hides a passing test's output.
        assert!(
            std::env::var_os("CPA_TEST_NO_SKIP").is_none(),
            "relays_over_ipv6: this host cannot bind UDP on ::1, and CPA_TEST_NO_SKIP is set"
        );
        eprintln!("skipping relays_over_ipv6: this host cannot bind UDP on ::1");
        return;
    }
    let config = RelayConfig {
        enabled: true,
        max_sessions: 1,
        ..RelayConfig::default()
    };
    let limiter = Arc::new(Limiter::default());
    limiter.set_limit(config.max_sessions());
    let relay = Relay {
        config,
        limiter,
        bind_ip: Some("::1".parse().unwrap()),
    };
    let route = Route {
        proxy_url: String::new(),
        credential: "c".into(),
        auth_index: "i".into(),
    };
    let mut client = peer_on(false, "::1").await;
    let client_audio = audio(&client.pc).await;
    let client_channel = client.pc.create_data_channel(LABEL, None).await.unwrap();
    let offer = client.pc.create_offer(None).await.unwrap();
    let client_offer = complete(&mut client, offer).await;
    let (session, relay_offer) = relay.new_session(client_offer, route, Box::new(())).await.unwrap();
    let addresses: Vec<&str> = relay_offer
        .lines()
        .filter_map(|l| l.strip_prefix("a=candidate:"))
        .filter_map(|c| c.split(' ').nth(4))
        .collect();
    assert!(!addresses.is_empty());
    assert!(
        addresses.iter().all(|a| a.parse::<std::net::Ipv6Addr>().is_ok()),
        "{addresses:?}"
    );

    let mut upstream = peer_on(false, "::1").await;
    upstream
        .pc
        .set_remote_description(RTCSessionDescription::offer(relay_offer).unwrap())
        .await
        .unwrap();
    let upstream_audio = audio(&upstream.pc).await;
    let answer = upstream.pc.create_answer(None).await.unwrap();
    let upstream_answer = complete(&mut upstream, answer).await;
    let downstream_answer = session.accept_upstream_answer(upstream_answer).await.unwrap();
    client
        .pc
        .set_remote_description(RTCSessionDescription::answer(downstream_answer).unwrap())
        .await
        .unwrap();
    assert_eq!(
        rtp_flows(&client_audio, &mut upstream.tracks, b"to-openai").await,
        b"to-openai"
    );
    assert_eq!(
        rtp_flows(&upstream_audio, &mut client.tracks, b"to-desktop").await,
        b"to-desktop"
    );
    let upstream_channel = tokio::time::timeout(Duration::from_secs(15), upstream.channels.recv())
        .await
        .unwrap()
        .unwrap();
    wait_open(&client_channel).await;
    client_channel.send_text("session.update").await.unwrap();
    assert_eq!(next_message(&upstream_channel).await, "session.update");
    session.close("test_done");
    let _ = client.pc.close().await;
    let _ = upstream.pc.close().await;
}

/// pion allocates a port per local address: a range whose free ports differ by family
/// still serves both peers.
#[test]
fn port_ranges_are_allocated_per_family() {
    let (v4, v6): (IpAddr, IpAddr) = ("0.0.0.0".parse().unwrap(), "::".parse().unwrap());
    let ports = [40000, 40001, 40002, 40003];
    // IPv4 has the last two free, IPv6 the first two.
    let free = |ip: IpAddr, port: u16| if ip.is_ipv4() { port >= 40002 } else { port <= 40001 };
    let mut cursors = vec![0; 2];
    let pick = |cursors: &mut Vec<usize>| {
        next_ports(&[v4, v6], &ports, cursors, free).map(|a| a.iter().map(|s| s.port()).collect::<Vec<_>>())
    };
    assert_eq!(pick(&mut cursors), Some(vec![40002, 40000]), "first peer");
    assert_eq!(pick(&mut cursors), Some(vec![40003, 40001]), "second peer, or a retry");
    assert_eq!(pick(&mut cursors), None, "the range is used up");
    let mut any = vec![0];
    assert_eq!(
        next_ports(&[v4], &[0], &mut any, |_, _| false).map(|a| a[0].port()),
        Some(0),
        "an unset range binds any port without probing"
    );
    assert_eq!(
        next_ports(&[v4], &[0], &mut any, |_, _| true),
        None,
        "and is tried once"
    );
}

/// A STUN server that never answers: pion abandons it after its STUN deadline and the call
/// goes on with the other candidates; so does the relay, after `GATHER_BOUND`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_stun_server_does_not_fail_the_call() {
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let config = RelayConfig {
        enabled: true,
        max_sessions: 1,
        ice_servers: vec![crate::realtime::relay::IceServer {
            urls: vec![format!("stun:{}", silent.local_addr().unwrap())],
            ..Default::default()
        }],
        ..RelayConfig::default()
    };
    let limiter = Arc::new(Limiter::default());
    limiter.set_limit(config.max_sessions());
    let relay = Relay {
        config,
        limiter,
        bind_ip: Some("127.0.0.1".parse().unwrap()),
    };
    let route = Route {
        proxy_url: String::new(),
        credential: "c".into(),
        auth_index: "i".into(),
    };
    let mut client = loopback_peer().await;
    let _channel = client.pc.create_data_channel(LABEL, None).await.unwrap();
    let offer = client.pc.create_offer(None).await.unwrap();
    let client_offer = complete(&mut client, offer).await;
    let started = tokio::time::Instant::now();
    let (session, relay_offer) = tokio::time::timeout(
        Duration::from_secs(15),
        relay.new_session(client_offer, route, Box::new(())),
    )
    .await
    .expect("setup finished")
    .expect("setup succeeded");
    assert!(
        started.elapsed() >= Duration::from_secs(5),
        "waited for the STUN server first"
    );
    assert!(
        relay_offer
            .lines()
            .any(|l| l.starts_with("a=candidate:") && l.contains(" 127.0.0.1 ")),
        "the host candidates are offered: {relay_offer}"
    );
    session.close("test_done");
    let _ = client.pc.close().await;
    drop(silent);
}

/// Closing a session wakes a setup waiting for gathering, which then fails instead of
/// waiting out `GATHER_BOUND` and going on with negotiation.
#[tokio::test]
async fn close_ends_a_pending_gathering_wait() {
    let limiter = Arc::new(Limiter::default());
    limiter.set_limit(1);
    let route = Route {
        proxy_url: String::new(),
        credential: "c".into(),
        auth_index: "i".into(),
    };
    let shared = Shared::new(limiter.acquire().unwrap(), route, None, String::new());
    let waiting = tokio::spawn({
        let shared = shared.clone();
        async move { shared.gathered(Side::Up).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiting.is_finished(), "gathering has not completed");
    shared.close("closed");
    let result = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("woken by the close")
        .unwrap();
    assert_eq!(result, Err("peer connection closed".to_owned()));
    // A wait that starts after the close fails at once too.
    let late = tokio::time::timeout(Duration::from_secs(1), shared.gathered(Side::Down)).await;
    assert_eq!(
        late.expect("no timeout fallback"),
        Err("peer connection closed".to_owned())
    );
}
