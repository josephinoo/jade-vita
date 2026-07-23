//! NVIDIA GFN device-code login (the "Steam Deck" OAuth flow) and encrypted-at-rest token
//! storage.
//!
//! There is no viable browser-redirect OAuth flow on the Vita (no embedded browser, no way to
//! receive a `localhost` redirect), so this only implements the alternate device-code flow:
//! the app shows a short code + QR, the user completes login on any other device, and the app
//! polls until authorized. See `docs/protocol-notes.md` §1b for how this was reverse-engineered
//! from OpenNOW's `opennow-stable/src/main/gfn/auth.ts`.
//!
//! NVIDIA only appears to allow the device-code grant for a specific `client_id` that the
//! official web client associates with its Steam Deck / "console" client profile, which is why
//! the client id, headers, and user agent below all masquerade as that client even though this
//! is not a Steam Deck. This is unconfirmed reverse-engineered behavior (see protocol notes) -
//! changing these values is likely to make NVIDIA reject the request outright.

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Client;
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// "Steam Deck" OAuth client id - the only one observed to support the device-code grant.
const CLIENT_ID: &str = "q61ddeJrVt7O90Nl-P-N7I36yctih4Ml6FyXLrb6j-U";
/// Default NVIDIA login provider id (as opposed to an Alliance-partner idp).
const IDP_ID: &str = "PDiAhv2kJTFeQ7WOPqiQ2tRZ7lGhR2X11dXvM4TZSxg";
const SCOPE: &str = "openid consent email tk_client age";
const DEVICE_AUTHORIZE_ENDPOINT: &str = "https://login.nvidia.com/device/authorize";
const TOKEN_ENDPOINT: &str = "https://login.nvidia.com/token";
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; Steam Deck) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36";
const DISPLAY_NAME: &str = "Jade Vita";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Fallback poll interval if NVIDIA's response omits `interval` (should not happen in practice).
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
/// Fallback device-code validity if NVIDIA's response omits `expires_in`.
const DEFAULT_CHALLENGE_TTL_SECS: u64 = 600;

const TOKEN_STORE_DIR: &str = "ux0:data/jade-vita";
const TOKEN_STORE_PATH: &str = "ux0:data/jade-vita/gfn-auth.json";
const TOKEN_STORE_VERSION: u8 = 1;
const TOKEN_KEY_MAGIC: &[u8; 8] = b"JVATKY01";
const TOKEN_KEY_SIZE: usize = 32;
const TOKEN_KEY_RECORD_SIZE: usize = TOKEN_KEY_MAGIC.len() + TOKEN_KEY_SIZE;
/// Safe Memory offset for the token encryption key. Only consumer of Safe Memory in this app
/// today - bump this if a second consumer is ever added so their records don't overlap.
const TOKEN_KEY_OFFSET: i64 = 0;
const TOKEN_NONCE_SIZE: usize = 12;
const TOKEN_AAD: &[u8] = b"jade-vita/gfn-refresh-token/v1";

pub fn client() -> Client {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .pool_max_idle_per_host(0)
        .build()
        .unwrap_or_default()
}

/// A device code challenge in progress: what the UI shows, plus what `poll` needs to check on
/// it. `device_code` is not displayed to the user - only `user_code` is.
#[derive(Debug, Clone)]
pub struct DeviceCodeChallenge {
    pub user_code: String,
    /// Already includes the user code as a query param - what the QR/link points at. The plain
    /// `verification_uri` NVIDIA also returns is redundant for our UI (`user_code` is shown as
    /// its own big text box) and is intentionally not kept here.
    pub verification_uri_complete: String,
    device_code: String,
    pub interval: Duration,
    deadline: Instant,
}

impl DeviceCodeChallenge {
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.deadline
    }
}

#[derive(Debug, Deserialize)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
}

pub async fn start_device_login(client: &Client) -> Result<DeviceCodeChallenge> {
    let response = client
        .post(DEVICE_AUTHORIZE_ENDPOINT)
        .header("Accept", "application/json, text/plain, */*")
        .header("Origin", "https://play.geforcenow.com")
        .header("Referer", "https://play.geforcenow.com/")
        .header("x-device-id", device_id())
        .header("nv-client-id", CLIENT_ID)
        .header("nv-client-streamer", "WEBRTC")
        .header("nv-client-type", "BROWSER")
        .header("nv-client-platform-name", "browser")
        .header("nv-browser-type", "CHROME")
        .header("nv-device-os", "STEAMOS")
        .header("nv-device-type", "CONSOLE")
        .header("nv-device-model", "STEAMDECK")
        .header("nv-device-make", "VALVE")
        .form(&[
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
            ("device_id", &device_id()),
            ("display_name", DISPLAY_NAME),
            ("idp_id", IDP_ID),
        ])
        .send()
        .await
        .context("device authorization request failed")?;

    let response = response
        .error_for_status()
        .context("device authorization request rejected")?;
    let payload: DeviceAuthorizationResponse = response
        .json()
        .await
        .context("failed to decode device authorization response")?;

    Ok(DeviceCodeChallenge {
        user_code: payload.user_code,
        verification_uri_complete: payload.verification_uri_complete,
        device_code: payload.device_code,
        interval: Duration::from_secs(
            payload
                .interval
                .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
                .max(1),
        ),
        deadline: Instant::now()
            + Duration::from_secs(payload.expires_in.unwrap_or(DEFAULT_CHALLENGE_TTL_SECS)),
    })
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TokenErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

pub enum DevicePollOutcome {
    /// Keep polling; the user has not finished logging in on their other device yet.
    Pending,
    /// NVIDIA asked us to slow down - the caller should widen its poll interval.
    SlowDown,
    Authorized(AuthTokens),
    /// The device code expired before the user completed login.
    Expired,
    /// The user explicitly declined the login request.
    Denied,
}

pub async fn poll_device_login(
    client: &Client,
    challenge: &DeviceCodeChallenge,
) -> Result<DevicePollOutcome> {
    let response = client
        .post(TOKEN_ENDPOINT)
        .header("Accept", "application/json, text/plain, */*")
        .header("Origin", "https://play.geforcenow.com")
        .header("Referer", "https://play.geforcenow.com/")
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", &challenge.device_code),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await
        .context("device token poll request failed")?;

    if response.status().is_success() {
        let payload: TokenResponse = response
            .json()
            .await
            .context("failed to decode device token response")?;
        return Ok(DevicePollOutcome::Authorized(AuthTokens {
            access_token: payload.access_token,
            refresh_token: payload.refresh_token,
            id_token: payload.id_token,
            expires_at_unix: expires_at_unix(payload.expires_in),
        }));
    }

    let error: TokenErrorResponse = response
        .json()
        .await
        .context("failed to decode device token error response")?;
    Ok(match error.error.as_str() {
        "authorization_pending" => DevicePollOutcome::Pending,
        "slow_down" => DevicePollOutcome::SlowDown,
        "expired_token" => DevicePollOutcome::Expired,
        "access_denied" => DevicePollOutcome::Denied,
        other => bail!(
            "device token poll rejected: {other} ({})",
            error.error_description.unwrap_or_default()
        ),
    })
}

fn expires_at_unix(expires_in: Option<u64>) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    now + expires_in.unwrap_or(86_400)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_at_unix: u64,
}

impl AuthTokens {
    /// The token GFN's own REST/GraphQL endpoints expect in `Authorization: GFNJWT <token>` -
    /// prefers `id_token` with `access_token` as a fallback, matching OpenNOW's own
    /// `session.tokens.idToken ?? session.tokens.accessToken` pattern.
    pub fn bearer(&self) -> &str {
        self.id_token.as_deref().unwrap_or(&self.access_token)
    }
}

pub struct GfnUser {
    /// The GFN account's stable subject id. Not shown in the UI today, but kept as the natural
    /// minimal identity for a signed-in user - Fase 3's CloudMatch session calls and any future
    /// multi-account support will need it.
    #[allow(dead_code)]
    pub user_id: String,
    pub display_name: String,
    pub email: Option<String>,
}

/// Reads `sub`/`email`/`preferred_username` out of the `id_token` JWT payload without verifying
/// its signature - the token just came from NVIDIA's token endpoint over TLS, so there is
/// nothing to gain from also checking the signature client-side (mirrors OpenNOW's own
/// `parseJwtPayload`/`fetchUserInfo` fallback logic).
pub fn user_from_tokens(tokens: &AuthTokens) -> Result<GfnUser> {
    let jwt = tokens.id_token.as_deref().unwrap_or(&tokens.access_token);
    let payload = decode_jwt_payload(jwt)?;
    let user_id = payload
        .get("sub")
        .and_then(|value| value.as_str())
        .context("JWT payload missing 'sub'")?
        .to_owned();
    let email = payload
        .get("email")
        .and_then(|value| value.as_str())
        .map(str::to_owned);
    let display_name = payload
        .get("preferred_username")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .or_else(|| {
            email
                .as_deref()
                .and_then(|email| email.split('@').next())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "Usuario".to_owned());

    Ok(GfnUser {
        user_id,
        display_name,
        email,
    })
}

fn decode_jwt_payload(token: &str) -> Result<serde_json::Value> {
    let mut segments = token.split('.');
    segments.next().context("JWT missing header segment")?;
    let payload_segment = segments.next().context("JWT missing payload segment")?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload_segment)
        .context("JWT payload is not valid base64url")?;
    serde_json::from_slice(&bytes).context("JWT payload is not valid JSON")
}

/// A random id persisted alongside the tokens, standing in for the hostname+username hash
/// OpenNOW's desktop client derives its `device_id` from - the Vita has neither concept in a
/// way that is stable and meaningful here.
fn device_id() -> String {
    if let Some(existing) = load_device_id() {
        return existing;
    }
    let mut bytes = [0u8; 16];
    let _ = SystemRandom::new().fill(&mut bytes);
    let id = encode_hex(&bytes);
    let _ = save_device_id(&id);
    id
}

const DEVICE_ID_PATH: &str = "ux0:data/jade-vita/device-id.txt";

fn load_device_id() -> Option<String> {
    std::fs::read_to_string(DEVICE_ID_PATH)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn save_device_id(id: &str) -> Result<()> {
    ensure_token_store_dir()?;
    write_file_truncating(DEVICE_ID_PATH, id)
}

// --- Encrypted-at-rest token persistence -----------------------------------------------------
//
// The same approach green-vita uses for its Xbox refresh token: the ciphertext lives in a plain
// JSON file on the memory card (`ux0:data`, world-readable on a jailbroken Vita), but the
// ChaCha20-Poly1305 key lives in Safe Memory instead, which is not casually readable by other
// homebrew. See THIRD_PARTY_NOTICES.md.

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EncryptedTokenStore {
    version: u8,
    nonce: String,
    ciphertext: String,
}

pub fn save_tokens(tokens: &AuthTokens) -> Result<()> {
    let plaintext = serde_json::to_vec(tokens).context("failed to serialize GFN tokens")?;
    let key = load_or_create_token_key()?;
    let mut nonce_bytes = [0u8; TOKEN_NONCE_SIZE];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| anyhow::anyhow!("failed to generate GFN token nonce"))?;
    let cipher = token_cipher(&key)?;
    let mut ciphertext = plaintext;
    cipher
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::from(TOKEN_AAD),
            &mut ciphertext,
        )
        .map_err(|_| anyhow::anyhow!("failed to encrypt GFN tokens"))?;

    let store = EncryptedTokenStore {
        version: TOKEN_STORE_VERSION,
        nonce: encode_hex(&nonce_bytes),
        ciphertext: encode_hex(&ciphertext),
    };
    ensure_token_store_dir()?;
    write_file_truncating(
        TOKEN_STORE_PATH,
        serde_json::to_string_pretty(&store).context("failed to serialize GFN token store")?,
    )
}

pub fn load_tokens() -> Option<AuthTokens> {
    load_tokens_inner().ok()
}

fn load_tokens_inner() -> Result<AuthTokens> {
    let data = std::fs::read_to_string(TOKEN_STORE_PATH).context("no saved GFN login")?;
    let store: EncryptedTokenStore =
        serde_json::from_str(&data).context("failed to parse GFN token store")?;
    if store.version != TOKEN_STORE_VERSION {
        bail!("unsupported GFN token store version {}", store.version);
    }
    let nonce = decode_hex(&store.nonce).context("invalid GFN token nonce")?;
    let nonce: [u8; TOKEN_NONCE_SIZE] = nonce
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid GFN token nonce length"))?;
    let mut ciphertext = decode_hex(&store.ciphertext).context("invalid GFN ciphertext")?;
    let key = load_token_key()?;
    let cipher = token_cipher(&key)?;
    let plaintext = cipher
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(TOKEN_AAD),
            &mut ciphertext,
        )
        .map_err(|_| anyhow::anyhow!("GFN token authentication failed"))?;
    serde_json::from_slice(plaintext).context("decrypted GFN token payload is not valid JSON")
}

pub fn clear_tokens() {
    let _ = std::fs::remove_file(TOKEN_STORE_PATH);
    let _ = safe_memory_save(TOKEN_KEY_OFFSET, &[0u8; TOKEN_KEY_RECORD_SIZE]);
}

fn token_cipher(key: &[u8; TOKEN_KEY_SIZE]) -> Result<LessSafeKey> {
    let key = UnboundKey::new(&aead::CHACHA20_POLY1305, key)
        .map_err(|_| anyhow::anyhow!("failed to initialize GFN token cipher"))?;
    Ok(LessSafeKey::new(key))
}

fn load_token_key() -> Result<[u8; TOKEN_KEY_SIZE]> {
    let record: [u8; TOKEN_KEY_RECORD_SIZE] = safe_memory_load(TOKEN_KEY_OFFSET)?;
    if &record[..TOKEN_KEY_MAGIC.len()] != TOKEN_KEY_MAGIC {
        bail!("GFN token key is missing from Safe Memory");
    }
    let mut key = [0u8; TOKEN_KEY_SIZE];
    key.copy_from_slice(&record[TOKEN_KEY_MAGIC.len()..]);
    Ok(key)
}

fn load_or_create_token_key() -> Result<[u8; TOKEN_KEY_SIZE]> {
    if let Ok(key) = load_token_key() {
        return Ok(key);
    }
    let mut key = [0u8; TOKEN_KEY_SIZE];
    SystemRandom::new()
        .fill(&mut key)
        .map_err(|_| anyhow::anyhow!("failed to generate GFN token key"))?;
    let mut record = [0u8; TOKEN_KEY_RECORD_SIZE];
    record[..TOKEN_KEY_MAGIC.len()].copy_from_slice(TOKEN_KEY_MAGIC);
    record[TOKEN_KEY_MAGIC.len()..].copy_from_slice(&key);
    safe_memory_save(TOKEN_KEY_OFFSET, &record)?;
    Ok(key)
}

fn safe_memory_load<const N: usize>(offset: i64) -> Result<[u8; N]> {
    crate::safe_memory::load::<N>(offset)
}

fn safe_memory_save(offset: i64, data: &[u8]) -> Result<()> {
    crate::safe_memory::save(offset, data)
}

fn ensure_token_store_dir() -> Result<()> {
    std::fs::create_dir_all(TOKEN_STORE_DIR).context("failed to create GFN token store directory")
}

fn write_file_truncating(path: &str, data: impl AsRef<[u8]>) -> Result<()> {
    // std::fs::write alone doesn't reliably truncate an existing file on the Vita's newlib
    // filesystem (same caveat green-vita's fs_utils documents).
    let _ = std::fs::remove_file(path);
    std::fs::write(path, data).with_context(|| format!("failed to write {path}"))
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_hex(encoded: &str) -> Result<Vec<u8>> {
    if !encoded.len().is_multiple_of(2) {
        bail!("hex value has an odd length");
    }
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = decode_hex_digit(pair[0])?;
            let low = decode_hex_digit(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn decode_hex_digit(digit: u8) -> Result<u8> {
    match digit {
        b'0'..=b'9' => Ok(digit - b'0'),
        b'a'..=b'f' => Ok(digit - b'a' + 10),
        b'A'..=b'F' => Ok(digit - b'A' + 10),
        _ => bail!("invalid hex digit"),
    }
}
