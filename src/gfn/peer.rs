//! Real WebRTC peer for GeForce NOW streaming, built on the sans-I/O `rtc` crate.
//!
//! Owns a dedicated OS thread running its own single-threaded tokio runtime: the sans-I/O
//! `RTCPeerConnection` is driven there (UDP socket + timers + poll loop), decrypted video RTP
//! is depacketized into H.264 access units, and those are fed to the hardware decode worker
//! (`streaming::video::VideoDecodeWorker`), which writes decoded RGB565 frames straight into
//! the SDL textures registered by the shell (green-vita's direct-texture path).
//!
//! The app talks to this through the same non-blocking channel shape as `signaling`: commands
//! in (`add_remote_ice`), events out (`try_recv` once per tick).

use crate::gfn::cloudmatch::SessionInfo;
use crate::gfn::input_protocol::{
    GAMEPAD_BITMAP_PRIMARY, GamepadInput, InputEncoder, parse_input_handshake_version,
};
use crate::gfn::signaling::IceCandidate;
use crate::streaming::video::{
    DecodedFrame, DecoderConfig, DirectVideoOutput, VideoDecodeWorker,
};
use anyhow::{Context, Result};
use bytes::BytesMut;
use rtc::peer_connection::RTCPeerConnectionBuilder;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::media_engine::MediaEngine;
use rtc::peer_connection::configuration::setting_engine::SettingEngine;
use rtc::peer_connection::event::RTCPeerConnectionEvent;
use rtc::peer_connection::message::RTCMessage;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::state::RTCPeerConnectionState;
use rtc::peer_connection::transport::{
    CandidateConfig, CandidateHostConfig, RTCDtlsRole, RTCIceCandidate, RTCIceCandidateInit,
    RTCIceServer,
};
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp::packetizer::Depacketizer;
use rtc::sansio::Protocol;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

// GFN clamps to its own supported modes regardless of what we request (960x544 is not one of
// them), so size the decoder from what CloudMatch reports and fall back to GFN's minimum.
const DEFAULT_STREAM_WIDTH: u32 = 1280;
const DEFAULT_STREAM_HEIGHT: u32 = 720;

/// The resolution NVIDIA actually streams at, per the session response.
fn stream_dimensions(session: &SessionInfo) -> (u32, u32) {
    session
        .negotiated_stream_profile
        .as_ref()
        .and_then(|profile| profile.resolution.as_deref())
        .and_then(|resolution| {
            let (width, height) = resolution.split_once('x')?;
            Some((width.parse().ok()?, height.parse().ok()?))
        })
        .filter(|(width, height)| *width > 0 && *height > 0)
        .unwrap_or((DEFAULT_STREAM_WIDTH, DEFAULT_STREAM_HEIGHT))
}

pub enum PeerEvent {
    /// Our SDP answer (plus its NVST parameter blob) is ready to go out via signaling.
    LocalAnswer {
        answer_sdp: String,
        nvst_sdp: String,
    },
    /// A local ICE candidate to trickle to the server via signaling.
    LocalIce(IceCandidate),
    /// Progress through the pipeline stages, for on-screen diagnostics.
    Status(String),
    Connected,
    Disconnected(String),
    Error(String),
}

enum PeerCommand {
    RemoteIce(IceCandidate),
    Gamepad(GamepadInput),
    Close,
}

pub struct PeerEngine {
    command_tx: mpsc::UnboundedSender<PeerCommand>,
    event_rx: mpsc::UnboundedReceiver<PeerEvent>,
    is_connected: Arc<AtomicBool>,
    video_output: Arc<DirectVideoOutput>,
    latest_frame: Arc<Mutex<Option<(u64, DecodedFrame)>>>,
}

impl PeerEngine {
    pub fn new(offer_sdp: &str, session: &SessionInfo) -> Result<Self> {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let is_connected = Arc::new(AtomicBool::new(false));
        let (stream_width, stream_height) = stream_dimensions(session);
        let video_output = Arc::new(DirectVideoOutput::new(stream_width, stream_height));
        let latest_frame: Arc<Mutex<Option<(u64, DecodedFrame)>>> = Arc::new(Mutex::new(None));

        let setup = PeerSetup {
            offer_sdp: offer_sdp.to_owned(),
            server_ip: session.server_ip.clone(),
            ice_servers: session
                .ice_servers
                .iter()
                .map(|server| RTCIceServer {
                    urls: server.urls.clone(),
                    username: server.username.clone().unwrap_or_default(),
                    credential: server.credential.clone().unwrap_or_default(),
                })
                .collect(),
        };

        let thread_events = event_tx.clone();
        let thread_connected = is_connected.clone();
        let thread_output = video_output.clone();
        let thread_frames = latest_frame.clone();
        std::thread::Builder::new()
            .name("jade-vita-peer".to_owned())
            .spawn(move || {
                // The sans-I/O peer loop gets its own runtime so its socket/timer waits never
                // touch the single-threaded runtime driving the UI (see main.rs).
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = thread_events
                            .send(PeerEvent::Error(format!("peer runtime failed: {error}")));
                        return;
                    }
                };
                let result = runtime.block_on(run_peer(
                    setup,
                    command_rx,
                    thread_events.clone(),
                    thread_connected,
                    thread_output,
                    thread_frames,
                ));
                if let Err(error) = result {
                    let _ = thread_events
                        .send(PeerEvent::Disconnected(format!("peer loop ended: {error:#}")));
                }
            })
            .context("failed to spawn peer thread")?;

        Ok(Self {
            command_tx,
            event_rx,
            is_connected,
            video_output,
            latest_frame,
        })
    }

    pub fn is_connected(&self) -> bool {
        self.is_connected.load(Ordering::Relaxed)
    }

    pub fn try_recv(&mut self) -> Option<PeerEvent> {
        self.event_rx.try_recv().ok()
    }

    pub fn add_remote_ice(&self, candidate: IceCandidate) {
        let _ = self.command_tx.send(PeerCommand::RemoteIce(candidate));
    }

    /// Ships one controller snapshot to the game (timestamped inside the peer thread on the
    /// session clock). Dropped silently until the input channel handshake completes.
    pub fn send_gamepad(&self, input: GamepadInput) {
        let _ = self.command_tx.send(PeerCommand::Gamepad(input));
    }

    pub fn direct_video_output(&self) -> Arc<DirectVideoOutput> {
        self.video_output.clone()
    }

    pub fn video_frame(&self) -> Option<(u64, DecodedFrame)> {
        *self.latest_frame.lock().ok()?
    }
}

impl Drop for PeerEngine {
    fn drop(&mut self) {
        let _ = self.command_tx.send(PeerCommand::Close);
        // Wake anything parked waiting for a free texture.
        self.video_output.clear_targets();
    }
}

struct PeerSetup {
    offer_sdp: String,
    server_ip: String,
    ice_servers: Vec<RTCIceServer>,
}

/// Discover the local IP the OS routes toward the server - classic connected-UDP trick.
fn local_ip_toward(server_ip: &str) -> IpAddr {
    let target = crate::gfn::sdp::extract_public_ip(server_ip)
        .and_then(|ip| ip.parse::<Ipv4Addr>().ok())
        .map(IpAddr::V4)
        .unwrap_or(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)));
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect(SocketAddr::new(target, 443))?;
            socket.local_addr()
        })
        .map(|addr| addr.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

async fn run_peer(
    setup: PeerSetup,
    mut command_rx: mpsc::UnboundedReceiver<PeerCommand>,
    event_tx: mpsc::UnboundedSender<PeerEvent>,
    is_connected: Arc<AtomicBool>,
    video_output: Arc<DirectVideoOutput>,
    latest_frame: Arc<Mutex<Option<(u64, DecodedFrame)>>>,
) -> Result<()> {
    // --- Hardware decode worker (sets `decoder_ready` so the shell creates the textures) ---
    let decode_worker = match VideoDecodeWorker::spawn(
        DecoderConfig {
            decode_width: video_output.width,
            decode_height: video_output.height,
            output_width: video_output.width,
            output_height: video_output.height,
        },
        video_output.clone(),
        latest_frame.clone(),
    ) {
        Ok(worker) => Some(worker),
        Err(error) => {
            // Non-fatal: negotiation still proceeds so the network path can be validated on
            // targets without the hardware decoder.
            let _ = event_tx.send(PeerEvent::Error(format!(
                "hardware decoder unavailable: {error:#}"
            )));
            None
        }
    };

    // --- Peer connection from NVIDIA's (sanitized) offer ---
    let sanitized_offer = crate::gfn::sdp::sanitize_offer(&setup.offer_sdp, &setup.server_ip);
    // Negotiation dumps for offline protocol debugging (world-readable like the rest of
    // ux0:data/jade-vita; the SDP holds only per-session credentials).
    let _ = std::fs::write("ux0:data/jade-vita/offer-raw.sdp", &setup.offer_sdp);
    let _ = std::fs::write("ux0:data/jade-vita/offer-sanitized.sdp", &sanitized_offer);
    let video_payload_types = crate::gfn::sdp::h264_payload_types(&sanitized_offer);

    let mut media_engine = MediaEngine::default();
    media_engine
        .register_default_codecs()
        .context("failed to register codecs")?;
    // NVIDIA's server is ICE-lite, which makes us the ICE controlling agent; rtc's Auto rule
    // (controlling → DTLS server) would answer `a=setup:passive` and then both sides sit
    // waiting for the other's ClientHello. GFN servers never act as DTLS client, so force the
    // standard browser behavior: answer `active` and initiate the handshake ourselves.
    let mut setting_engine = SettingEngine::default();
    setting_engine
        .set_answering_dtls_role(RTCDtlsRole::Client)
        .context("failed to force DTLS client role")?;
    let mut pc = RTCPeerConnectionBuilder::new()
        .with_configuration(
            RTCConfigurationBuilder::new()
                .with_ice_servers(setup.ice_servers.clone())
                .build(),
        )
        .with_media_engine(media_engine)
        .with_setting_engine(setting_engine)
        .build()
        .context("failed to build peer connection")?;

    let offer = RTCSessionDescription::offer(sanitized_offer)
        .context("NVIDIA offer SDP was rejected by the SDP parser")?;
    pc.set_remote_description(offer)
        .context("failed to apply NVIDIA offer")?;

    // --- Input data channel (must exist before the answer so its SCTP stream is negotiated;
    //     the offer already carries the m=application section) ---
    let input_channel_id = match pc.create_data_channel("input_channel_v1", None) {
        Ok(channel) => Some(channel.id()),
        Err(error) => {
            let _ = event_tx.send(PeerEvent::Error(format!(
                "input channel creation failed: {error}"
            )));
            None
        }
    };
    let mut input_encoder = InputEncoder::default();
    let mut input_ready = false;
    let session_clock = Instant::now();

    // --- UDP socket + local host candidate ---
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0")
        .await
        .context("failed to bind media UDP socket")?;
    let bound_port = socket.local_addr()?.port();
    let local_ip = local_ip_toward(&setup.server_ip);
    let local_addr = SocketAddr::new(local_ip, bound_port);

    let host_candidate = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: local_ip.to_string(),
            port: bound_port,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()
    .context("failed to create host candidate")?;
    let local_candidate_init: RTCIceCandidateInit = RTCIceCandidate::from(&host_candidate)
        .to_json()
        .context("failed to serialize host candidate")?;
    pc.add_local_candidate(local_candidate_init.clone())
        .context("failed to add local candidate")?;

    // --- Answer ---
    let answer = pc.create_answer(None).context("failed to create answer")?;
    pc.set_local_description(answer.clone())
        .context("failed to set local description")?;
    let answer_sdp = answer.sdp.clone();
    let _ = std::fs::write("ux0:data/jade-vita/answer.sdp", &answer_sdp);
    let nvst_sdp = crate::gfn::sdp::build_nvst_sdp_from_answer(&answer_sdp);
    let our_ufrag = crate::gfn::sdp::extract_ice_credentials(&answer_sdp).ufrag;
    let _ = event_tx.send(PeerEvent::LocalAnswer {
        answer_sdp,
        nvst_sdp,
    });
    let _ = event_tx.send(PeerEvent::LocalIce(IceCandidate {
        candidate: local_candidate_init.candidate.clone(),
        sdp_mid: Some("0".to_owned()),
        sdp_m_line_index: Some(0),
        username_fragment: Some(our_ufrag),
    }));

    // --- Sans-I/O event loop ---
    let mut depacketizer = H264Packet::default();
    let mut access_unit: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut buf = vec![0u8; 2000];
    let mut first_rtp_seen = false;
    let mut first_au_submitted = false;
    // Raw pipeline counters surfaced on-screen every few seconds - the fastest way to see
    // which stage a stalled stream died at without console access on the Vita. In/out packet
    // classes tell apart "our DTLS ClientHello never leaves" from "NVIDIA never answers it".
    let mut in_stun: u64 = 0;
    let mut in_dtls: u64 = 0;
    let mut in_media: u64 = 0;
    let mut out_stun: u64 = 0;
    let mut out_dtls: u64 = 0;
    let mut out_media: u64 = 0;
    let mut rtp_packets: u64 = 0;
    let mut access_units_sent: u64 = 0;
    let mut frames_decoded_last: u64 = 0;
    // First byte of a UDP payload: 0-3 STUN, 20-63 DTLS records, 128-191 RTP/RTCP.
    fn classify(first_byte: Option<&u8>) -> usize {
        match first_byte {
            Some(0..=3) => 0,
            Some(20..=63) => 1,
            Some(128..=191) => 2,
            _ => 2,
        }
    }
    let mut stats_interval = tokio::time::interval(Duration::from_secs(3));
    stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(2));
    heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pending_commands: Vec<PeerCommand> = Vec::new();
    const IDLE_TIMEOUT: Duration = Duration::from_secs(86400);

    loop {
        while let Some(msg) = pc.poll_write() {
            match classify(msg.message.first()) {
                0 => out_stun += 1,
                1 => out_dtls += 1,
                _ => out_media += 1,
            }
            let _ = socket.send_to(&msg.message, msg.transport.peer_addr).await;
        }

        while let Some(event) = pc.poll_event() {
            match event {
                RTCPeerConnectionEvent::OnConnectionStateChangeEvent(state) => match state {
                    RTCPeerConnectionState::Connected => {
                        is_connected.store(true, Ordering::Relaxed);
                        let _ = event_tx.send(PeerEvent::Connected);
                    }
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                        let _ = event_tx.send(PeerEvent::Disconnected(format!(
                            "peer connection state: {state}"
                        )));
                        return Ok(());
                    }
                    other => {
                        let _ = event_tx.send(PeerEvent::Status(format!("Conexión: {other}")));
                    }
                },
                RTCPeerConnectionEvent::OnIceConnectionStateChangeEvent(state) => {
                    let _ = event_tx.send(PeerEvent::Status(format!("ICE: {state}")));
                }
                RTCPeerConnectionEvent::OnTrack(_) => {
                    let _ = event_tx.send(PeerEvent::Status("Track de media abierto".to_owned()));
                }
                _ => {}
            }
        }

        while let Some(message) = pc.poll_read() {
            if let RTCMessage::DataChannelMessage(_channel_id, dc_message) = &message {
                if !input_ready
                    && let Some(version) = parse_input_handshake_version(&dc_message.data)
                {
                    input_ready = true;
                    input_encoder.set_protocol_version(version.min(u8::MAX as u16) as u8);
                    let _ = event_tx.send(PeerEvent::Status(format!(
                        "Canal de input listo (protocolo v{version})"
                    )));
                }
                continue;
            }
            if let RTCMessage::RtpPacket(_track_id, packet) = message {
                rtp_packets += 1;
                if !first_rtp_seen {
                    first_rtp_seen = true;
                    let _ = event_tx.send(PeerEvent::Status(format!(
                        "Recibiendo RTP (payload type {})",
                        packet.header.payload_type
                    )));
                }
                let is_video = video_payload_types.is_empty()
                    || video_payload_types.contains(&packet.header.payload_type);
                if !is_video {
                    continue;
                }
                match depacketizer.depacketize(&packet.payload) {
                    Ok(nal_bytes) => {
                        access_unit.extend_from_slice(&nal_bytes);
                        if packet.header.marker && !access_unit.is_empty() {
                            let au = std::mem::take(&mut access_unit);
                            access_units_sent += 1;
                            if !first_au_submitted {
                                first_au_submitted = true;
                                let _ = event_tx.send(PeerEvent::Status(format!(
                                    "Decodificando H.264 ({} bytes/AU)",
                                    au.len()
                                )));
                            }
                            if let Some(worker) = &decode_worker {
                                worker.submit_access_unit(au);
                            }
                        }
                    }
                    Err(_) => {
                        // Mid-fragment loss; drop the partial AU and wait for the next one.
                        access_unit.clear();
                    }
                }
            }
        }

        let timeout = pc
            .poll_timeout()
            .unwrap_or_else(|| Instant::now() + IDLE_TIMEOUT);
        let delay = timeout.saturating_duration_since(Instant::now());
        if delay.is_zero() {
            pc.handle_timeout(Instant::now())?;
            continue;
        }

        let timer = tokio::time::sleep(delay);
        tokio::pin!(timer);

        tokio::select! {
            biased;

            _ = &mut timer => {
                pc.handle_timeout(Instant::now())?;
            }
            _ = heartbeat_interval.tick() => {
                if input_ready && let Some(id) = input_channel_id {
                    let heartbeat = input_encoder.encode_heartbeat();
                    if let Some(mut channel) = pc.data_channel(id) {
                        let _ = channel.send(BytesMut::from(&heartbeat[..]));
                    }
                }
            }
            _ = stats_interval.tick() => {
                let frames = latest_frame
                    .lock()
                    .ok()
                    .and_then(|slot| slot.map(|(id, _)| id))
                    .unwrap_or(0);
                // Only worth showing while the picture hasn't appeared or has frozen.
                if frames == frames_decoded_last {
                    let _ = event_tx.send(PeerEvent::Status(format!(
                        "IN s:{in_stun} d:{in_dtls} m:{in_media} | OUT s:{out_stun} d:{out_dtls} m:{out_media} | RTP:{rtp_packets} AU:{access_units_sent} F:{frames}"
                    )));
                }
                frames_decoded_last = frames;
            }
            command = command_rx.recv() => {
                match command {
                    Some(command) => pending_commands.push(command),
                    None => pending_commands.push(PeerCommand::Close),
                }
            }
            received = socket.recv_from(&mut buf) => {
                if let Ok((n, peer_addr)) = received {
                    match classify(buf.first()) {
                        0 => in_stun += 1,
                        1 => in_dtls += 1,
                        _ => in_media += 1,
                    }
                    pc.handle_read(TaggedBytesMut {
                        now: Instant::now(),
                        transport: TransportContext {
                            local_addr,
                            peer_addr,
                            ecn: None,
                            transport_protocol: TransportProtocol::UDP,
                        },
                        message: BytesMut::from(&buf[..n]),
                    })?;
                }
            }
        }

        // Latency control: drain everything already queued before the next poll cycle.
        // Handling one datagram/command per wakeup lets the OS socket buffer (and with it,
        // glass-to-glass delay) grow without bound during video bursts.
        while let Ok((n, peer_addr)) = socket.try_recv_from(&mut buf) {
            match classify(buf.first()) {
                0 => in_stun += 1,
                1 => in_dtls += 1,
                _ => in_media += 1,
            }
            pc.handle_read(TaggedBytesMut {
                now: Instant::now(),
                transport: TransportContext {
                    local_addr,
                    peer_addr,
                    ecn: None,
                    transport_protocol: TransportProtocol::UDP,
                },
                message: BytesMut::from(&buf[..n]),
            })?;
        }
        while let Ok(command) = command_rx.try_recv() {
            pending_commands.push(command);
        }

        // Coalesce queued gamepad snapshots down to the newest one - the game only cares
        // about current stick/button state, and replaying a backlog adds input latency.
        let mut latest_gamepad = None;
        for command in pending_commands.drain(..) {
            match command {
                PeerCommand::RemoteIce(candidate) => {
                    let init = RTCIceCandidateInit {
                        candidate: candidate.candidate,
                        sdp_mid: candidate.sdp_mid,
                        sdp_mline_index: candidate.sdp_m_line_index.map(|index| index as u16),
                        username_fragment: candidate.username_fragment,
                        ..Default::default()
                    };
                    if let Err(error) = pc.add_remote_candidate(init) {
                        let _ = event_tx.send(PeerEvent::Error(format!(
                            "remote ICE candidate rejected: {error}"
                        )));
                    }
                }
                PeerCommand::Gamepad(input) => latest_gamepad = Some(input),
                PeerCommand::Close => {
                    let _ = pc.close();
                    return Ok(());
                }
            }
        }
        if let Some(mut input) = latest_gamepad
            && input_ready
            && let Some(id) = input_channel_id
        {
            input.timestamp_us = session_clock.elapsed().as_micros() as u64;
            let packet = input_encoder.encode_gamepad_state(GAMEPAD_BITMAP_PRIMARY, input);
            if let Some(mut channel) = pc.data_channel(id) {
                let _ = channel.send(BytesMut::from(&packet[..]));
            }
        }
    }
}
