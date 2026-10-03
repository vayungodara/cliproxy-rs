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
    let mut media = MediaEngine::default();
    media.register_codec(opus_parameters(), RtpCodecKind::Audio).unwrap();
    let registry = register_default_interceptors(Registry::new(), &mut media).unwrap();
    let (gathered_tx, gathered) = watch::channel(false);
    let (tracks_tx, tracks) = mpsc::channel(4);
    let (channels_tx, channels) = mpsc::channel(4);
    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_setting_engine(
            SettingEngineBuilder::new()
                .with_include_loopback_candidate(true)
                .with_multicast_dns_mode(rtc::ice::mdns::MulticastDnsMode::Disabled)
                .build(),
        )
        .with_handler(Arc::new(PeerEvents {
            gathered: gathered_tx,
            tracks: tracks_tx,
            channels: channels_tx,
        }))
        .with_udp_addrs(vec!["127.0.0.1:0".to_owned()])
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
        proxy: cpa_exec::proxy::Proxy::Inherit,
        credential: "Voice credential".into(),
        auth_index: "auth-index".into(),
    };

    // Client: an audio track and the oai-events channel, like the Codex desktop app.
    let mut client = loopback_peer().await;
    let client_audio = audio(&client.pc).await;
    let client_channel = client.pc.create_data_channel(LABEL, None).await.unwrap();
    let offer = client.pc.create_offer(None).await.unwrap();
    let client_offer = complete(&mut client, offer).await;

    let (session, relay_offer) = relay.new_session(client_offer.clone(), route()).await.unwrap();
    assert_ne!(relay_offer, client_offer, "the relay offers its own session upstream");
    let busy = relay.new_session(client_offer, route()).await;
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
        )
        .await
        .expect("closing released the session slot");
    second.close("test_done");
    let _ = client.pc.close().await;
    let _ = upstream.pc.close().await;

    // Proxied credentials fail closed instead of sending media around the proxy.
    let proxied = Route {
        proxy: cpa_exec::proxy::Proxy::Url("socks5://127.0.0.1:9".into()),
        ..route()
    };
    let refused = relay.new_session("v=0\r\n".into(), proxied).await;
    assert_eq!(refused.err().map(|e| e.status), Some(502));
}
