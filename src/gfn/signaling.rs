//! NVST WebSocket signaling client - the transport carrying the SDP offer/answer and trickled
//! ICE candidates for a GFN streaming session. See `docs/protocol-notes.md` §3 for the
//! reverse-engineered protocol this implements (from
//! `opennow-stable/src/main/gfn/signaling.ts`); nothing here is documented by NVIDIA.
//!
//! Runs as a background Tokio task (`connect` spawns it) so the render loop never blocks on
//! network I/O. The caller drives it with a command channel and drains an event channel once per
//! `App::tick()`, the same non-blocking shape as the `PollJob` used for REST calls elsewhere in
//! this app - just persistent instead of one-shot.
#![allow(dead_code)] // Public API wired into AppState in the next Fase 3 commit.

use anyhow::{Context, Result};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::interval;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Matches the desktop client's own signaling user agent (see protocol notes §3) - not the
/// Steam Deck one used for login, since this request isn't gated by the device-code grant.
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

/// An ICE candidate in the JSON shape NVST's signaling channel exchanges - distinct from
/// whatever candidate type `rtc`'s ICE agent uses internally; this is only the wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceCandidate {
    pub candidate: String,
    #[serde(rename = "sdpMid", skip_serializing_if = "Option::is_none")]
    pub sdp_mid: Option<String>,
    #[serde(rename = "sdpMLineIndex", skip_serializing_if = "Option::is_none")]
    pub sdp_m_line_index: Option<u32>,
    #[serde(rename = "usernameFragment", skip_serializing_if = "Option::is_none")]
    pub username_fragment: Option<String>,
}

pub enum SignalingEvent {
    Connected,
    Offer(String),
    RemoteIce(IceCandidate),
    /// Peer sent "BYE", the signaling error `peerRemoved`, the socket closed, or the initial
    /// connection attempt failed.
    Disconnected(String),
    Error(String),
}

enum SignalingCommand {
    SendAnswer { sdp: String, nvst_sdp: String },
    SendLocalIce(IceCandidate),
    Close,
}

/// Handle to a running signaling session. Dropping this does not close the connection - call
/// `close()` explicitly (mirrors how `CoverStore`'s detached download tasks are allowed to
/// outlive a discarded `AppState`; here we'd rather be explicit since leaving a socket open to
/// NVIDIA's signaling server is a real resource, not just wasted CPU).
pub struct SignalingHandle {
    command_tx: mpsc::UnboundedSender<SignalingCommand>,
    event_rx: mpsc::UnboundedReceiver<SignalingEvent>,
}

impl SignalingHandle {
    pub fn send_answer(&self, sdp: String, nvst_sdp: String) {
        let _ = self
            .command_tx
            .send(SignalingCommand::SendAnswer { sdp, nvst_sdp });
    }

    pub fn send_local_ice(&self, candidate: IceCandidate) {
        let _ = self
            .command_tx
            .send(SignalingCommand::SendLocalIce(candidate));
    }

    pub fn close(&self) {
        let _ = self.command_tx.send(SignalingCommand::Close);
    }

    /// Non-blocking drain of whatever signaling events arrived since the last call - meant to be
    /// called once per `App::tick()`, same shape as polling a `PollJob`.
    pub fn try_recv(&mut self) -> Option<SignalingEvent> {
        self.event_rx.try_recv().ok()
    }
}

/// Connects to the session's NVST signaling WebSocket and spawns the background task that owns
/// it. `signaling_url` and `session_id` come from `cloudmatch::SessionInfo`.
pub fn connect(signaling_url: &str, session_id: &str) -> Result<SignalingHandle> {
    let peer_name = format!("peer-{}", random_peer_suffix());
    let url = build_sign_in_url(signaling_url, session_id, &peer_name);
    let protocol = format!("x-nv-sessionid.{session_id}");

    let mut request = url
        .as_str()
        .into_client_request()
        .context("invalid NVST signaling URL")?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(&protocol)
            .context("invalid session id for websocket protocol header")?,
    );
    request.headers_mut().insert(
        "Origin",
        HeaderValue::from_static("https://play.geforcenow.com"),
    );
    request
        .headers_mut()
        .insert("User-Agent", HeaderValue::from_static(USER_AGENT));

    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    tokio::spawn(run(request, peer_name, event_tx, command_rx));

    Ok(SignalingHandle {
        command_tx,
        event_rx,
    })
}

/// Mirrors OpenNOW's `buildSignInUrl` (signaling.ts): keep whatever path the backend put in
/// `signaling_url`, append `sign_in` to it, and replace the query string. Never force an
/// `/nvst/` prefix - servers that hand out an explicit URL serve `sign_in` off that exact
/// path, and guessing a different one gets a 404 on the WebSocket upgrade.
fn build_sign_in_url(signaling_url: &str, session_id: &str, peer_name: &str) -> String {
    let mut url_str = signaling_url.trim().to_string();
    if url_str.starts_with("http://") {
        url_str = url_str.replacen("http://", "ws://", 1);
    } else if url_str.starts_with("https://") {
        url_str = url_str.replacen("https://", "wss://", 1);
    } else if !url_str.starts_with("ws://") && !url_str.starts_with("wss://") {
        url_str = format!("wss://{url_str}");
    }

    if let Some(query_start) = url_str.find('?') {
        url_str.truncate(query_start);
    }
    if url_str.ends_with("/sign_in") {
        url_str.truncate(url_str.len() - "sign_in".len());
    }
    let base = url_str.trim_end_matches('/');
    format!("{base}/sign_in?peer_id={peer_name}&version=2&peer_role=1&pairing_id={session_id}")
}

fn random_peer_suffix() -> u64 {
    let mut bytes = [0u8; 8];
    let _ = SystemRandom::new().fill(&mut bytes);
    u64::from_le_bytes(bytes) % 10_000_000_000
}

async fn run(
    request: tokio_tungstenite::tungstenite::handshake::client::Request,
    peer_name: String,
    event_tx: mpsc::UnboundedSender<SignalingEvent>,
    mut command_rx: mpsc::UnboundedReceiver<SignalingCommand>,
) {
    let uri = request.uri().clone();
    let (ws_stream, _response) = match tokio_tungstenite::connect_async(request).await {
        Ok(pair) => pair,
        Err(error) => {
            let _ = event_tx.send(SignalingEvent::Disconnected(format!(
                "signaling connect failed: {error} (url: {uri})"
            )));
            return;
        }
    };
    let (mut write, mut read) = ws_stream.split();

    let mut local_peer_id: u32 = 0;
    let mut remote_peer_id: u32 = 1;
    let mut ack_counter: u32 = 0;

    send_json(
        &mut write,
        &json!({
            "ackid": next_ack(&mut ack_counter),
            "peer_info": {
                "browser": "Chrome",
                "browserVersion": "131",
                "connected": true,
                "id": local_peer_id,
                "name": peer_name,
                "peerRole": 0,
                "resolution": "960x544",
                "version": 2,
            }
        }),
    )
    .await;
    let _ = event_tx.send(SignalingEvent::Connected);

    let mut heartbeat = interval(HEARTBEAT_INTERVAL);
    heartbeat.tick().await; // the first tick fires immediately; we just sent peer_info

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                send_json(&mut write, &json!({"hb": 1})).await;
            }
            command = command_rx.recv() => {
                match command {
                    Some(SignalingCommand::SendAnswer { sdp, nvst_sdp }) => {
                        let payload = json!({"type": "answer", "sdp": sdp, "nvstSdp": nvst_sdp}).to_string();
                        send_peer_msg(&mut write, local_peer_id, remote_peer_id, &payload, &mut ack_counter).await;
                    }
                    Some(SignalingCommand::SendLocalIce(candidate)) => {
                        if is_tcp_candidate(&candidate.candidate) {
                            // Matches the reference client: local TCP candidates are dropped,
                            // not sent - see protocol notes §3.
                            continue;
                        }
                        let payload = serde_json::to_string(&candidate).unwrap_or_default();
                        send_peer_msg(&mut write, local_peer_id, remote_peer_id, &payload, &mut ack_counter).await;
                    }
                    Some(SignalingCommand::Close) | None => {
                        let _ = write.close().await;
                        return;
                    }
                }
            }
            message = read.next() => {
                let should_continue = handle_incoming_message(
                    message,
                    &peer_name,
                    &mut local_peer_id,
                    &mut remote_peer_id,
                    &event_tx,
                    &mut write,
                ).await;
                if !should_continue {
                    return;
                }
            }
        }
    }
}

/// Returns `false` when the caller should stop the connection loop (socket closed, transport
/// error, or the peer told us the session is over).
#[allow(clippy::too_many_arguments)]
async fn handle_incoming_message(
    message: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    peer_name: &str,
    local_peer_id: &mut u32,
    remote_peer_id: &mut u32,
    event_tx: &mpsc::UnboundedSender<SignalingEvent>,
    write: &mut WsSink,
) -> bool {
    let Some(message) = message else {
        let _ = event_tx.send(SignalingEvent::Disconnected("socket closed".to_owned()));
        return false;
    };
    let message = match message {
        Ok(message) => message,
        Err(error) => {
            let _ = event_tx.send(SignalingEvent::Error(format!("websocket error: {error}")));
            return false;
        }
    };
    let Message::Text(text) = message else {
        return true;
    };

    let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
        let _ = event_tx.send(SignalingEvent::Error(
            "ignored non-JSON signaling packet".to_owned(),
        ));
        return true;
    };

    if let Some(peer_info) = parsed.get("peer_info")
        && let (Some(id), Some(name)) = (
            peer_info.get("id").and_then(Value::as_u64),
            peer_info.get("name").and_then(Value::as_str),
        )
        && name == peer_name
    {
        *local_peer_id = id as u32;
    }

    if let Some(ackid) = parsed.get("ackid").and_then(Value::as_u64) {
        let is_our_own_echo = parsed
            .get("peer_info")
            .and_then(|info| info.get("id"))
            .and_then(Value::as_u64)
            == Some(u64::from(*local_peer_id));
        if !is_our_own_echo {
            send_json(write, &json!({"ack": ackid})).await;
        }
    }

    if parsed.get("hb").is_some() {
        send_json(write, &json!({"hb": 1})).await;
        return true;
    }

    if parsed.get("error").and_then(Value::as_str) == Some("peerRemoved") {
        let _ = event_tx.send(SignalingEvent::Disconnected("peerRemoved".to_owned()));
        return false;
    }

    let Some(peer_msg) = parsed.get("peer_msg") else {
        return true;
    };
    if let Some(from) = peer_msg.get("from").and_then(Value::as_u64) {
        *remote_peer_id = from as u32;
    }
    let Some(msg) = peer_msg.get("msg").and_then(Value::as_str) else {
        return true;
    };
    let trimmed = msg.trim();
    if trimmed == "BYE" {
        let _ = event_tx.send(SignalingEvent::Disconnected("BYE".to_owned()));
        return false;
    }

    let Ok(payload) = serde_json::from_str::<Value>(trimmed) else {
        let _ = event_tx.send(SignalingEvent::Error(
            "received non-JSON peer payload".to_owned(),
        ));
        return true;
    };

    if payload.get("type").and_then(Value::as_str) == Some("offer") {
        if let Some(sdp) = payload.get("sdp").and_then(Value::as_str) {
            let _ = event_tx.send(SignalingEvent::Offer(sdp.to_owned()));
        }
        return true;
    }

    if let Some(candidate) = payload.get("candidate").and_then(Value::as_str) {
        let ice = IceCandidate {
            candidate: candidate.to_owned(),
            sdp_mid: payload.get("sdpMid").and_then(Value::as_str).map(str::to_owned),
            sdp_m_line_index: payload
                .get("sdpMLineIndex")
                .and_then(Value::as_u64)
                .map(|value| value as u32),
            username_fragment: payload
                .get("usernameFragment")
                .and_then(Value::as_str)
                .map(str::to_owned),
        };
        let _ = event_tx.send(SignalingEvent::RemoteIce(ice));
    }

    true
}

async fn send_json(write: &mut WsSink, value: &Value) {
    let _ = write.send(Message::Text(value.to_string().into())).await;
}

async fn send_peer_msg(write: &mut WsSink, from: u32, to: u32, msg: &str, ack_counter: &mut u32) {
    send_json(
        write,
        &json!({
            "peer_msg": {"from": from, "to": to, "msg": msg},
            "ackid": next_ack(ack_counter),
        }),
    )
    .await;
}

fn next_ack(counter: &mut u32) -> u32 {
    *counter += 1;
    *counter
}

/// SDP/local ICE candidates encode transport as the 3rd space-separated token
/// (`candidate:<foundation> <component> <transport> ...`) - matches the reference client's own
/// `isTcpIceCandidate`.
fn is_tcp_candidate(candidate: &str) -> bool {
    candidate
        .split_whitespace()
        .nth(2)
        .is_some_and(|token| token.eq_ignore_ascii_case("tcp"))
}
