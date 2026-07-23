//! NVST input-channel binary protocol - the wire format for controller state sent over the
//! `input_channel_v1` WebRTC data channel. Ported from OpenNOW's
//! `native/opennow-streamer/src/input.rs` (gamepad + heartbeat subset; keyboard/mouse can be
//! added later from the same reference).

const INPUT_HEARTBEAT: u32 = 2;
const INPUT_GAMEPAD: u32 = 12;

const WRAPPER_LEGACY_INPUT: u8 = 0x21;
const WRAPPER_VERSION_MARKER: u8 = 0x23;
const GAMEPAD_PAYLOAD_SIZE: u16 = 26;
const GAMEPAD_INNER_SIZE: u16 = 20;
const GAMEPAD_RESERVED_MARKER: u16 = 85;

/// Bitmap of connected controllers - the Vita is always exactly one, in slot 0.
pub const GAMEPAD_BITMAP_PRIMARY: u16 = 1;

/// One controller snapshot in XInput conventions: bitmask per `XINPUT_GAMEPAD_*`, stick axes
/// -32768..32767 with +Y up, triggers 0-255.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GamepadInput {
    pub controller_id: u8,
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub left_stick_x: i16,
    pub left_stick_y: i16,
    pub right_stick_x: i16,
    pub right_stick_y: i16,
    /// Microseconds on the session clock (time since the peer connected).
    pub timestamp_us: u64,
}

pub struct InputEncoder {
    protocol_version: u8,
}

impl Default for InputEncoder {
    fn default() -> Self {
        Self {
            protocol_version: 2,
        }
    }
}

impl InputEncoder {
    pub fn set_protocol_version(&mut self, protocol_version: u8) {
        self.protocol_version = protocol_version;
    }

    /// Keepalive the server expects every ~2 seconds once the channel is up.
    pub fn encode_heartbeat(&self) -> Vec<u8> {
        INPUT_HEARTBEAT.to_le_bytes().to_vec()
    }

    pub fn encode_gamepad_state(&self, bitmap: u16, input: GamepadInput) -> Vec<u8> {
        let mut payload = Vec::with_capacity(38);
        payload.extend_from_slice(&INPUT_GAMEPAD.to_le_bytes());
        payload.extend_from_slice(&GAMEPAD_PAYLOAD_SIZE.to_le_bytes());
        payload.extend_from_slice(&(input.controller_id as u16).to_le_bytes());
        payload.extend_from_slice(&bitmap.to_le_bytes());
        payload.extend_from_slice(&GAMEPAD_INNER_SIZE.to_le_bytes());
        payload.extend_from_slice(&input.buttons.to_le_bytes());
        payload.extend_from_slice(
            &(input.left_trigger as u16 | ((input.right_trigger as u16) << 8)).to_le_bytes(),
        );
        payload.extend_from_slice(&input.left_stick_x.to_le_bytes());
        payload.extend_from_slice(&input.left_stick_y.to_le_bytes());
        payload.extend_from_slice(&input.right_stick_x.to_le_bytes());
        payload.extend_from_slice(&input.right_stick_y.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&GAMEPAD_RESERVED_MARKER.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&input.timestamp_us.to_le_bytes());
        self.wrap_legacy_input(input.timestamp_us, &payload)
    }

    /// Protocol v3 frames every event as `[0x23][u64 BE timestamp][0x21][u16 BE len][payload]`;
    /// v2 sends the bare payload.
    fn wrap_legacy_input(&self, timestamp_us: u64, payload: &[u8]) -> Vec<u8> {
        if self.protocol_version < 3 {
            return payload.to_vec();
        }

        let mut bytes = Vec::with_capacity(12 + payload.len());
        bytes.push(WRAPPER_VERSION_MARKER);
        bytes.extend_from_slice(&timestamp_us.to_be_bytes());
        bytes.push(WRAPPER_LEGACY_INPUT);
        bytes.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }
}

/// The server's first message on the input channel announces the protocol version: either
/// `[526 u16 LE][version u16 LE]` or a leading `0x0e` byte with the version word itself.
pub fn parse_input_handshake_version(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < 2 {
        return None;
    }

    let first_word = u16::from_le_bytes([bytes[0], bytes[1]]);
    if first_word == 526 {
        return Some(if bytes.len() >= 4 {
            u16::from_le_bytes([bytes[2], bytes[3]])
        } else {
            2
        });
    }

    if bytes[0] == 0x0e {
        return Some(first_word);
    }

    None
}
