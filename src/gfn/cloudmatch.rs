//! CloudMatch REST API — creates, polls and stops a GFN streaming session.
#![allow(dead_code)] // Public API used in subsequent Fase 3 commits; suppress until wired.
//!
//! Reference: `opennow-stable/src/main/gfn/cloudmatch.ts` and `protocol.rs` in the OpenNOW
//! native streamer. This is a deliberate, minimal implementation for the Vita client: it does
//! not support network-test sessions, alliance partners, ad reporting, or resume/claim flows.
//!
//! Forced stream profile for Vita hardware (see `docs/protocol-notes.md` §5):
//! - Resolution 960x544, 30 fps
//! - H.264 only (hardware decoder on Vita)
//! - 8-bit 4:2:0 color quality
//! - Conservative bitrate (max ~8 Mbps)

use super::headers::{self, error_for_status_with_body};
use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

const DEFAULT_CLOUDMATCH_BASE_URL: &str = "https://prod.cloudmatchbeta.nvidiagrid.net/";
const DEFAULT_LOCALE: &str = "en_US";
const DEFAULT_KEYBOARD_LAYOUT: &str = "us";

/// Per-session client identity. CloudMatch expects a random UUID for `client_id` per session
/// and a stable `device_id` across reconnects. We keep both on `SessionInfo` so polling and
/// stop reuse the same values.
#[derive(Debug, Clone)]
pub struct SessionIdentity {
    pub client_id: String,
    pub device_id: String,
}

/// Settings chosen for the Vita's hardware limits.
#[derive(Debug, Clone)]
pub struct StreamSettings {
    pub resolution: String,
    pub fps: u32,
    pub max_bitrate_mbps: u32,
}

impl StreamSettings {
    /// Default Vita profile: 960x544@30, H.264 (forced later via `codec` field), moderate bitrate.
    pub fn for_vita() -> Self {
        Self {
            resolution: "960x544".to_owned(),
            fps: 30,
            max_bitrate_mbps: 8,
        }
    }

    fn parse_resolution(&self) -> (u32, u32) {
        let mut parts = self.resolution.split('x');
        let width = parts.next().and_then(|s| s.parse().ok()).unwrap_or(960);
        let height = parts.next().and_then(|s| s.parse().ok()).unwrap_or(544);
        (width, height)
    }
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub app_id: String,
    pub status: u32,
    pub server_ip: String,
    pub signaling_server: String,
    pub signaling_url: String,
    pub client_id: String,
    pub device_id: String,
    pub streaming_base_url: String,
    pub identity: SessionIdentity,
    pub media_connection_info: Option<MediaConnectionInfo>,
    pub ice_servers: Vec<IceServer>,
    pub negotiated_stream_profile: Option<NegotiatedStreamProfile>,
}

#[derive(Debug, Clone)]
pub struct MediaConnectionInfo {
    pub ip: String,
    pub port: u16,
}

#[derive(Debug, Clone)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NegotiatedStreamProfile {
    pub resolution: Option<String>,
    pub fps: Option<u32>,
    pub codec: Option<String>,
}

/// CloudMatch session creation request.
pub struct CreateSessionRequest<'a> {
    pub token: &'a str,
    pub app_id: &'a str,
    pub vpc_id: &'a str,
    pub settings: &'a StreamSettings,
}

/// CloudMatch session poll request.
pub struct PollSessionRequest<'a> {
    pub token: &'a str,
    pub session_id: &'a str,
    pub session: &'a SessionInfo,
}

/// Creates a CloudMatch session and returns the initial `SessionInfo`.
///
/// The server may return a session that is not immediately ready (`status` not 2/3), so the
/// caller should then call `poll_session` until it is.
pub async fn create_session(
    client: &Client,
    request: CreateSessionRequest<'_>,
) -> Result<SessionInfo> {
    let identity = SessionIdentity {
        client_id: uuid::Uuid::new_v4().to_string(),
        device_id: stable_device_id(),
    };
    let base_url = DEFAULT_CLOUDMATCH_BASE_URL.trim_end_matches('/');
    let (width, height) = request.settings.parse_resolution();

    let body = build_session_request_body(
        request.app_id,
        &identity.device_id,
        width,
        height,
        request.settings.fps,
    );
    let url = format!(
        "{base_url}/v2/session?keyboardLayout={DEFAULT_KEYBOARD_LAYOUT}&languageCode={DEFAULT_LOCALE}"
    );

    let send_request = || async {
        let mut last_err = None;
        for _retry in 0..3 {
            let response = headers::apply_cloudmatch_headers(
                client.post(&url),
                request.token,
                &identity.client_id,
                &identity.device_id,
            )
            .header("Connection", "close")
            .json(&body)
            .send()
            .await;

            match response {
                Ok(resp) => {
                    let status = resp.status();
                    let body_text = resp.text().await.unwrap_or_default();

                    if status.is_success() {
                        let payload: CloudMatchResponse = serde_json::from_str(&body_text)
                            .context("failed to decode CloudMatch create session response")?;
                        return Ok((payload, false));
                    }

                    if status == reqwest::StatusCode::FORBIDDEN || body_text.contains("SESSION_LIMIT_EXCEEDED") {
                        if let Ok(limit_payload) = serde_json::from_str::<CloudMatchResponse>(&body_text) {
                            let mut old_ids = Vec::new();
                            if let Some(s) = &limit_payload.session {
                                if let Some(id) = &s.session_id {
                                    old_ids.push(id.as_string());
                                }
                            }
                            for s in &limit_payload.other_user_sessions {
                                if let Some(id) = &s.session_id {
                                    old_ids.push(id.as_string());
                                }
                            }
                            for old_id in old_ids {
                                if !old_id.is_empty() {
                                    stop_session_by_id(client, request.token, &old_id).await;
                                }
                            }
                            return Ok((limit_payload, true));
                        }
                    }

                    bail!("HTTP {status}: {body_text}");
                }
                Err(err) => {
                    last_err = Some(err);
                    sleep(Duration::from_millis(500)).await;
                }
            }
        }
        Err(anyhow::anyhow!("CloudMatch create session request failed: {}", last_err.unwrap()))
    };

    let (mut payload, was_limit_exceeded) = send_request().await?;
    if was_limit_exceeded {
        sleep(Duration::from_millis(1000)).await;
        let (retry_payload, _) = send_request().await?;
        payload = retry_payload;
    }

    parse_session_info(payload, base_url, identity)
}

#[derive(Debug, Clone, Default)]
pub struct QueueStatus {
    pub queue_position: u32,
    pub eta_ms: u32,
    pub attempt: usize,
}

pub type QueueProgressTracker = Arc<std::sync::Mutex<QueueStatus>>;

/// Polls a session until the server reports it is ready, or a reasonable timeout expires.
///
/// Per the reference, `status` values 2 and 3 are considered "ready" (the exact enum names are
/// not documented; only the numeric constants are known from `cloudmatch.ts`).
pub async fn poll_session(
    client: &Client,
    request: PollSessionRequest<'_>,
    tracker: Option<QueueProgressTracker>,
) -> Result<SessionInfo> {
    let base_url = request.session.streaming_base_url.trim_end_matches('/');
    let url = format!("{base_url}/v2/session/{}", request.session_id);
    let identity = &request.session.identity;
    const MAX_ATTEMPTS: usize = 1800; // 1 hour maximum queue wait time
    const POLL_INTERVAL: Duration = Duration::from_secs(2);

    for attempt in 0..MAX_ATTEMPTS {
        let response = headers::apply_cloudmatch_headers(
            client.get(&url),
            request.token,
            &identity.client_id,
            &identity.device_id,
        )
        .send()
        .await
        .with_context(|| format!("CloudMatch poll attempt {attempt} failed"))?;
        let response = error_for_status_with_body(response)
            .await
            .with_context(|| format!("CloudMatch poll attempt {attempt} rejected"))?;

        let payload: CloudMatchResponse = response
            .json()
            .await
            .context("failed to decode CloudMatch poll response")?;

        if payload.request_status.status_code != 1 {
            bail!(
                "CloudMatch poll error: {} ({})",
                payload.request_status.status_code,
                payload
                    .request_status
                    .status_message
                    .as_deref()
                    .unwrap_or("unknown")
            );
        }

        let session = payload
            .session
            .as_ref()
            .context("CloudMatch poll response had no session")?;

        if let Some(tr) = &tracker {
            if let Ok(mut st) = tr.lock() {
                st.attempt = attempt + 1;
                if let Some(seat) = &session.seat_setup_info {
                    st.queue_position = seat.queue_position;
                    st.eta_ms = seat.seat_setup_eta;
                }
            }
        }

        if is_ready_status(session.status) {
            // The zone load balancer reports itself in the connection fields; once the session
            // is seated, OpenNOW re-polls the real server directly and uses that response for
            // signaling info (the zone LB may return different data than a direct poll).
            let base_host = base_url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .split('/')
                .next()
                .and_then(|authority| authority.split(':').next())
                .unwrap_or("");
            if let Some(real_ip) = streaming_server_ip(session) {
                if is_zone_hostname(base_host) && !is_zone_hostname(&real_ip) {
                    let direct_base = format!("https://{real_ip}");
                    let direct_url = format!("{direct_base}/v2/session/{}", request.session_id);
                    if let Some(direct_payload) =
                        fetch_session_payload(client, &direct_url, request.token, identity).await
                    {
                        return parse_session_info(direct_payload, &direct_base, identity.clone());
                    }
                }
            }
            return parse_session_info(payload, base_url, identity.clone());
        }

        sleep(POLL_INTERVAL).await;
    }

    bail!("CloudMatch session did not become ready within the poll timeout")
}

/// Best-effort GET of a session payload; `None` on any transport/decode/status failure so the
/// caller can fall back to the zone load balancer response it already has.
async fn fetch_session_payload(
    client: &Client,
    url: &str,
    token: &str,
    identity: &SessionIdentity,
) -> Option<CloudMatchResponse> {
    let response = headers::apply_cloudmatch_headers(
        client.get(url),
        token,
        &identity.client_id,
        &identity.device_id,
    )
    .send()
    .await
    .ok()?;
    let response = error_for_status_with_body(response).await.ok()?;
    let payload: CloudMatchResponse = response.json().await.ok()?;
    if payload.request_status.status_code == 1 && payload.session.is_some() {
        Some(payload)
    } else {
        None
    }
}

pub async fn stop_session_by_id(client: &Client, token: &str, session_id: &str) {
    let base_url = DEFAULT_CLOUDMATCH_BASE_URL.trim_end_matches('/');
    let url = format!("{base_url}/v2/session/{session_id}");
    let client_id = uuid::Uuid::new_v4().to_string();
    let device_id = stable_device_id();

    if let Ok(response) = headers::apply_cloudmatch_headers(
        client.delete(&url),
        token,
        &client_id,
        &device_id,
    )
    .send()
    .await
    {
        let _ = response.text().await;
    }
}

/// Stops a CloudMatch session. Best-effort: errors are logged but not returned as fatal.
pub async fn stop_session(client: &Client, token: &str, session: &SessionInfo) {
    stop_session_by_id(client, token, &session.session_id).await;
}

fn is_ready_status(status: u32) -> bool {
    // Reference constants from cloudmatch.ts: READY_SESSION_STATUSES = {2, 3}
    status == 2 || status == 3
}

/// Zone load balancer hostnames (e.g. `np-lon-05.cloudmatchbeta.nvidiagrid.net`) broker
/// sessions but do not host them - signaling and ready-session polls must target the real
/// seat instead. Mirrors OpenNOW's `isZoneHostname`.
fn is_zone_hostname(host: &str) -> bool {
    host.contains("cloudmatchbeta.nvidiagrid.net") || host.contains("cloudmatch.nvidiagrid.net")
}

/// Host portion of a `rtsps://host:port/...`-style URL. Mirrors OpenNOW's `extractHostFromUrl`.
fn extract_host_from_url(url: &str) -> Option<&str> {
    let after_proto = ["rtsps://", "rtsp://", "wss://", "https://"]
        .iter()
        .find_map(|prefix| url.strip_prefix(prefix))?;
    let host = after_proto.split(':').next()?.split('/').next()?;
    if host.is_empty() || host.starts_with('.') {
        None
    } else {
        Some(host)
    }
}

/// The real seat host, per OpenNOW's `streamingServerIp` priority chain: the usage-14
/// connection's `ip`, then the host inside its `resourcePath`, then `sessionControlInfo.ip`.
fn streaming_server_ip(session: &CloudMatchSession) -> Option<String> {
    if let Some(conn) = session
        .connection_info
        .iter()
        .find(|conn| conn.matches_usage(14))
    {
        if let Some(ip) = conn
            .ip
            .as_ref()
            .map(|ip| ip.as_string())
            .filter(|ip| !ip.is_empty())
        {
            return Some(ip);
        }
        if let Some(host) = conn
            .resource_path
            .as_deref()
            .and_then(extract_host_from_url)
        {
            return Some(host.to_owned());
        }
    }

    session
        .session_control_info
        .as_ref()
        .and_then(|ctrl| ctrl.ip.as_ref().map(|ip| ip.as_string()))
        .filter(|ip| !ip.is_empty())
}

/// Mirrors OpenNOW's `buildSignalingUrl`: `rtsps://host:port` resourcePaths become
/// `wss://{host}/nvst/`, absolute `wss://` URLs pass through, bare paths hang off the server
/// ip, and anything else falls back to `wss://{server_ip}:443/nvst/`. Returns the URL plus
/// the host it targets when one was extracted from the resourcePath itself.
fn build_signaling_url(raw: &str, server_ip: &str) -> (String, Option<String>) {
    if raw.starts_with("rtsps://") || raw.starts_with("rtsp://") {
        if let Some(host) = extract_host_from_url(raw) {
            return (format!("wss://{host}/nvst/"), Some(host.to_owned()));
        }
        return (format!("wss://{server_ip}:443/nvst/"), None);
    }

    if raw.starts_with("wss://") {
        let host = raw["wss://".len()..].split('/').next().map(str::to_owned);
        return (raw.to_owned(), host);
    }

    if server_ip.is_empty() {
        // Session not seated yet (no usage-14 connection): nothing valid to build.
        return (String::new(), None);
    }

    if raw.starts_with('/') {
        return (format!("wss://{server_ip}:443{raw}"), None);
    }

    (format!("wss://{server_ip}:443/nvst/"), None)
}

/// A stable device identifier. The reference uses a UUID persisted on disk; for the MVP we
/// derive a deterministic UUID from the console's fixed device seed. Revisit when adding
/// multi-user support.
fn stable_device_id() -> String {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, b"jade-vita-stable-device-id").to_string()
}

fn build_session_request_body(
    app_id: &str,
    device_hash_id: &str,
    width: u32,
    height: u32,
    fps: u32,
) -> serde_json::Value {
    // Values match OpenNOW's `webRtcSessionMetadata()` (cloudmatch.ts:767). The server expects
    // this exact metadata shape; omitting/changing keys here has caused 500 errors in tests.
    let sub_session_id = uuid::Uuid::new_v4().to_string();
    let metadata = json!([
        { "key": "SubSessionId", "value": sub_session_id },
        { "key": "wssignaling", "value": "1" },
        { "key": "GSStreamerType", "value": "WebRTC" },
        { "key": "networkType", "value": "Unknown" },
        { "key": "ClientImeSupport", "value": "0" },
        {
            "key": "clientPhysicalResolution",
            "value": json!({ "horizontalPixels": width, "verticalPixels": height }).to_string()
        },
        { "key": "surroundAudioInfo", "value": "2" }
    ]);

    json!({
        "sessionRequestData": {
            "appId": app_id,
            "internalTitle": null,
            "availableSupportedControllers": [],
            "networkTestSessionId": null,
            "parentSessionId": null,
            "clientIdentification": "GFN-PC",
            "deviceHashId": device_hash_id,
            "clientVersion": "30.0",
            "sdkVersion": "1.0",
            "streamerVersion": 1,
            "clientPlatformName": "windows",
            "clientRequestMonitorSettings": [
                {
                    "monitorId": 0,
                    "positionX": 0,
                    "positionY": 0,
                    "widthInPixels": width,
                    "heightInPixels": height,
                    "framesPerSecond": fps,
                    "sdrHdrMode": 0,
                    "displayData": {},
                    "hdr10PlusGamingData": null,
                    "dpi": 0,
                }
            ],
            "useOps": true,
            "audioMode": 2,
            "metaData": metadata,
            "sdrHdrMode": 0,
            "clientDisplayHdrCapabilities": null,
            "surroundAudioInfo": 0,
            "remoteControllersBitmap": 0,
            "clientTimezoneOffset": 0,
            "enhancedStreamMode": 1,
            "appLaunchMode": 0,
            "secureRTSPSupported": false,
            "partnerCustomData": "",
            "accountLinked": true,
            "enablePersistingInGameSettings": false,
            "userAge": 26,
            "requestedStreamingFeatures": {
                "reflex": false,
                "bitDepth": 0, // 0 = 8-bit, 1 = 10-bit (OpenNOW colorQualityBitDepth)
                "cloudGsync": false,
                "enabledL4S": false,
                "supportedHidDevices": 0,
                "profile": 0,
                "fallbackToLogicalResolution": false,
                "chromaFormat": 0, // 0 = 4:2:0, 1 = 4:4:4 (OpenNOW colorQualityChromaFormat)
                "prefilterMode": 0,
                "prefilterSharpness": 0,
                "prefilterNoiseReduction": 0,
                "hudStreamingMode": 0,
            }
        }
    })
}

fn parse_session_info(
    payload: CloudMatchResponse,
    streaming_base_url: &str,
    identity: SessionIdentity,
) -> Result<SessionInfo> {
    if payload.request_status.status_code != 1 {
        bail!(
            "CloudMatch error: {} ({})",
            payload.request_status.status_code,
            payload
                .request_status
                .status_message
                .as_deref()
                .unwrap_or("unknown")
        );
    }

    let session = payload
        .session
        .as_ref()
        .context("CloudMatch response had no session")?;

    let session_id = session
        .session_id
        .as_ref()
        .map(|s| s.as_string())
        .unwrap_or_default();

    let server_ip = streaming_server_ip(session).unwrap_or_default();

    // OpenNOW's resolveSignaling: the signaling endpoint comes from the usage-14 connection's
    // resourcePath, never from the top-level serverIp field (that is the zone load balancer,
    // which does not serve /nvst/sign_in and answers 404).
    let signaling_connection = session
        .connection_info
        .iter()
        .find(|conn| conn.matches_usage(14) && conn.ip.is_some())
        .or_else(|| session.connection_info.iter().find(|conn| conn.ip.is_some()));
    let resource_path = signaling_connection
        .and_then(|conn| conn.resource_path.as_deref())
        .unwrap_or("/nvst/");

    let (signaling_url, signaling_host) = build_signaling_url(resource_path, &server_ip);
    let effective_host = signaling_host.unwrap_or_else(|| server_ip.clone());
    let signaling_server = if effective_host.contains(':') || effective_host.is_empty() {
        effective_host
    } else {
        format!("{effective_host}:443")
    };

    let media_connection_info = session
        .connection_info
        .iter()
        .find(|conn| conn.matches_usage(2) || conn.matches_usage(17) || conn.matches_usage(14))
        .and_then(|conn| match (&conn.ip, conn.port) {
            (Some(ip), Some(port)) => Some(MediaConnectionInfo {
                ip: ip.as_string(),
                port,
            }),
            (None, Some(port)) if !server_ip.is_empty() => Some(MediaConnectionInfo {
                ip: server_ip.clone(),
                port,
            }),
            _ => None,
        });

    let ice_servers = session
        .ice_server_configuration
        .as_ref()
        .map(|cfg| {
            cfg.ice_servers
                .iter()
                .map(|server| IceServer {
                    urls: server.urls.to_vec(),
                    username: server.username.clone(),
                    credential: server.credential.clone(),
                })
                .collect()
        })
        .unwrap_or_default();

    let negotiated_stream_profile = session
        .session_request_data
        .as_ref()
        .and_then(|data| data.requested_streaming_features.as_ref())
        .map(|_features| NegotiatedStreamProfile {
            resolution: Some(format!("{}x{}", session.width, session.height)),
            fps: Some(session.fps),
            codec: Some("H264".to_owned()),
        });

    Ok(SessionInfo {
        session_id,
        app_id: session
            .session_request_data
            .as_ref()
            .and_then(|data| data.app_id.as_ref().map(|id| id.as_string()))
            .unwrap_or_default(),
        status: session.status,
        server_ip,
        signaling_server,
        signaling_url,
        streaming_base_url: streaming_base_url.to_owned(),
        client_id: identity.client_id.clone(),
        device_id: identity.device_id.clone(),
        identity,
        media_connection_info,
        ice_servers,
        negotiated_stream_profile,
    })
}

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + serde::Deserialize<'de>,
{
    let opt = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

// --- serde DTOs for CloudMatch responses ---

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloudMatchResponse {
    #[serde(rename = "requestStatus")]
    request_status: CloudMatchRequestStatus,
    #[serde(default)]
    session: Option<CloudMatchSession>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    other_user_sessions: Vec<CloudMatchSession>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloudMatchRequestStatus {
    status_code: u32,
    #[serde(default)]
    status_message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum FlexibleString {
    Str(String),
    Num(i64),
    // Alliance servers report `ip` as an array of addresses; the reference takes the first.
    List(Vec<FlexibleString>),
}

impl FlexibleString {
    fn as_string(&self) -> String {
        match self {
            FlexibleString::Str(s) => s.clone(),
            FlexibleString::Num(n) => n.to_string(),
            FlexibleString::List(items) => {
                items.first().map(|item| item.as_string()).unwrap_or_default()
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloudMatchSession {
    #[serde(default)]
    session_id: Option<FlexibleString>,
    #[serde(default)]
    status: u32,
    #[serde(default)]
    session_control_info: Option<CloudMatchControlInfo>,
    #[serde(default)]
    seat_setup_info: Option<SeatSetupInfo>,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    fps: u32,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    connection_info: Vec<CloudMatchConnectionInfo>,
    #[serde(default)]
    ice_server_configuration: Option<IceServerConfiguration>,
    #[serde(default)]
    session_request_data: Option<CloudMatchSessionRequestData>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SeatSetupInfo {
    #[serde(default)]
    pub queue_position: u32,
    #[serde(default)]
    pub seat_setup_step: u32,
    #[serde(default)]
    pub seat_setup_eta: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloudMatchControlInfo {
    #[serde(default)]
    ip: Option<FlexibleString>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloudMatchConnectionInfo {
    #[serde(default)]
    ip: Option<FlexibleString>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    usage: Option<UsageValue>,
    #[serde(default)]
    resource_path: Option<String>,
}

impl CloudMatchConnectionInfo {
    fn matches_usage(&self, code: u64) -> bool {
        match &self.usage {
            Some(UsageValue::Num(n)) => *n == code,
            Some(UsageValue::Str(s)) => s.parse::<u64>().map_or(false, |n| n == code),
            None => false,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum UsageValue {
    Str(String),
    Num(u64),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IceServerConfiguration {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    ice_servers: Vec<IceServerDto>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IceServerDto {
    #[serde(default)]
    urls: IceUrls,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    credential: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IceUrls {
    Single(String),
    Many(Vec<String>),
}

impl Default for IceUrls {
    fn default() -> Self {
        IceUrls::Many(Vec::new())
    }
}

impl IceUrls {
    fn to_vec(&self) -> Vec<String> {
        match self {
            IceUrls::Single(s) => vec![s.clone()],
            IceUrls::Many(v) => v.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloudMatchSessionRequestData {
    #[serde(default)]
    app_id: Option<FlexibleString>,
    #[serde(default)]
    requested_streaming_features: Option<CloudMatchStreamingFeatures>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloudMatchStreamingFeatures {
    #[serde(default)]
    bit_depth: Option<u8>,
    #[serde(default)]
    chroma_format: Option<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_device_id_is_deterministic() {
        let a = stable_device_id();
        let b = stable_device_id();
        assert_eq!(a, b);
        assert!(a.len() > 10);
    }

    #[test]
    fn parse_resolution_splits() {
        let settings = StreamSettings {
            resolution: "1280x720".to_owned(),
            fps: 30,
            max_bitrate_mbps: 5,
        };
        assert_eq!(settings.parse_resolution(), (1280, 720));
    }

    #[test]
    fn is_ready_status_values() {
        assert!(is_ready_status(2));
        assert!(is_ready_status(3));
        assert!(!is_ready_status(1));
        assert!(!is_ready_status(4));
    }
}
