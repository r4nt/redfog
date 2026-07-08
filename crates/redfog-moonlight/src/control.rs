//! ENet control channel (47999/udp): input decode + feedback.
//!
//! Every control message (encrypted or not) is framed as `[u16 LE
//! type][u16 LE length][payload]` where `length = payload.len()`. The
//! `Encrypted` (0x0001) type wraps an AES-128-GCM-encrypted inner message:
//! `[u32 LE sequence_number][16-byte tag][ciphertext]`, IV = `[sequence_number
//! LE (4 bytes)][5 zero bytes]['C']['C']` for messages *we receive*
//! (serverbound) — clientbound (messages a server sends) uses `['H']['C']`
//! instead; confirmed against moonlight-common-rust's `ControlEncryptionMethod::
//! Sunshine` IV derivation, which is direction-dependent. Also requires the
//! server to advertise itself as Sunshine-like (`<appversion>` with a negative
//! 4th component, `x-ss-general.encryptionSupported` with `CONTROL_V2`
//! (`0x01`) set — see pairing.rs/rtsp.rs) or real clients skip this whole
//! negotiation and fall back to sending `InputData` unencrypted, keyed/framed
//! differently than what's documented here. Key = the client's `rikey` (sent
//! as a query param on `/launch`, not in the RTSP SDP — see pairing.rs).
//! `InputData` (0x0206) payloads are `[u32 LE input_event_type][event bytes]`.
//! Gamepad input is out of scope for this iteration (deferred, see plan doc).
//!
//! Layout derived from reading a known-working implementation's wire code
//! (not vendored), see the plan doc for context.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_enet::{Event, Host, HostConfig};

use crate::crypto;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    KeyDown { keycode: u32 },
    KeyUp { keycode: u32 },
    MouseMoveRelative { dx: i16, dy: i16 },
    MouseMoveAbsolute { x: i16, y: i16, screen_width: i16, screen_height: i16 },
    MouseButtonDown { button: u32 },
    MouseButtonUp { button: u32 },
    ScrollVertical { amount: i16 },
    ScrollHorizontal { amount: i16 },
}

pub trait ControlEventHandler: Send + Sync {
    fn on_input(&self, event: InputEvent);
    fn on_request_idr_frame(&self);
}

pub struct NoopControlEventHandler;
impl ControlEventHandler for NoopControlEventHandler {
    fn on_input(&self, _event: InputEvent) {}
    fn on_request_idr_frame(&self) {}
}

pub struct ControlServer {
    pub port: u16,
    /// The client's `rikey` (from `/launch`), used to decrypt incoming control
    /// messages. A shared cell rather than a fixed value: it's only known
    /// once `/launch` happens (and changes across relaunches), while this
    /// server binds once at startup.
    pub key: Arc<std::sync::Mutex<Option<[u8; 16]>>>,
    pub handler: Arc<dyn ControlEventHandler>,
    /// Bumped by `SessionManager::set_rikey` whenever a new client takes over
    /// an existing session (reconnect after a closed window, or a plain
    /// relaunch). Each connected peer is tagged (see `serve`) with whatever
    /// generation was current at its own `Connect` event; any peer whose tag
    /// falls behind the latest generation gets disconnected.
    ///
    /// This has to be generation-based rather than "disconnect whoever's
    /// connected right now when notified" — confirmed live: a blanket sweep
    /// run right after the *new* client's own peer connected disconnected
    /// that brand-new peer too (it doesn't know it's new), silently killing
    /// every reconnect's control channel before it could even request a
    /// keyframe. Tagging by generation means a peer is only ever a
    /// disconnect target once *another*, later reconnect makes it stale —
    /// never the one that triggered the sweep it's caught in.
    ///
    /// Solves a separate, real problem: a stale peer that never sent ENet's
    /// own disconnect (e.g. a closed browser tab, confirmed live) keeps
    /// sending messages encrypted with the old rikey after `key` moves on to
    /// the new client's, and those fail GCM authentication forever.
    pub rikey_generation: Arc<AtomicU64>,
}

impl ControlServer {
    pub async fn serve(self, bind_addr: IpAddr) -> Result<(), String> {
        let config = HostConfig {
            address: Some(std::net::SocketAddr::new(bind_addr, self.port)),
            peer_count: 4,
            // Real clients request 48 channels (confirmed live: "channel_count=48"
            // in the connection log) — keyboard/mouse/gamepad input each use
            // dedicated channel indices (see moonlight-common-rust's
            // `EnetChannel`, CHANNEL_COUNT=0x30=48), not just channel 0.
            // Capping this at 1 silently clamps the negotiated channel count,
            // corrupting/dropping anything sent on a channel we never set up.
            channel_limit: 48,
            ..Default::default()
        };
        let mut host = Host::new(config).map_err(|e| format!("failed to create enet host on port {}: {e}", self.port))?;
        // Which `rikey_generation` was current when each peer connected —
        // see the field's doc comment for why this can't just be "whoever's
        // connected right now".
        let mut peer_generations: HashMap<tokio_enet::PeerId, u64> = HashMap::new();
        let mut last_seen_generation = self.rikey_generation.load(Ordering::Acquire);

        loop {
            let current_generation = self.rikey_generation.load(Ordering::Acquire);
            if current_generation != last_seen_generation {
                let stale: Vec<_> = peer_generations
                    .iter()
                    .filter(|(_, &gen)| gen < current_generation)
                    .map(|(&id, _)| id)
                    .collect();
                for peer_id in &stale {
                    host.disconnect_now(*peer_id, 0);
                    peer_generations.remove(peer_id);
                }
                if !stale.is_empty() {
                    tracing::info!("control channel: disconnected {} stale peer(s) for session takeover", stale.len());
                }
                last_seen_generation = current_generation;
            }
            match host.service(Duration::from_millis(100)).await {
                Ok(Some(Event::Connect { peer_id, .. })) => {
                    tracing::info!("control channel: peer {peer_id:?} connected");
                    // Read fresh, not `last_seen_generation`/`current_generation`
                    // above — this event may be processed after a *later*
                    // `set_rikey` than the one this iteration observed, and
                    // under-tagging would make this peer an immediate
                    // disconnect target on the very next check.
                    peer_generations.insert(peer_id, self.rikey_generation.load(Ordering::Acquire));
                }
                Ok(Some(Event::Disconnect { peer_id, .. })) => {
                    tracing::info!("control channel: peer {peer_id:?} disconnected");
                    peer_generations.remove(&peer_id);
                }
                Ok(Some(Event::Receive { packet, .. })) => {
                    self.handle_message(packet.data());
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("control channel enet error: {e}"),
            }
        }
    }

    fn handle_message(&self, buffer: &[u8]) {
        let Some(key) = *self.key.lock().unwrap() else {
            tracing::trace!("dropping control message: no session's rikey is set yet");
            return;
        };
        match ControlMessage::parse(buffer, &key) {
            Ok(ControlMessage::InputData(payload)) => match decode_input_event(&payload) {
                Some(event) => self.handler.on_input(event),
                None => tracing::trace!("unhandled/unknown input event"),
            },
            Ok(ControlMessage::RequestIdrFrame) => self.handler.on_request_idr_frame(),
            Ok(ControlMessage::Other) => {} // Ping/LossStats/FrameStats/etc — ignored in v1.
            Err(e) => tracing::debug!("bad control message: {e}"),
        }
    }
}

const CONTROL_MSG_ENCRYPTED: u16 = 0x0001;
const CONTROL_MSG_INPUT_DATA: u16 = 0x0206;
const CONTROL_MSG_REQUEST_IDR_FRAME: u16 = 0x0302;
const CONTROL_MSG_INVALIDATE_REFERENCE_FRAMES: u16 = 0x0301;

enum ControlMessage {
    InputData(Vec<u8>),
    RequestIdrFrame,
    Other,
}

impl ControlMessage {
    /// Parse a top-level control message, transparently decrypting it if
    /// wrapped (`Encrypted`, 0x0001).
    fn parse(buffer: &[u8], key: &[u8; 16]) -> Result<Self, String> {
        if buffer.len() < 4 {
            return Err(format!("control message too short: {} bytes", buffer.len()));
        }
        let message_type = u16::from_le_bytes([buffer[0], buffer[1]]);
        let length = u16::from_le_bytes([buffer[2], buffer[3]]) as usize;
        // Real clients (confirmed live against moonlight-qt) send trailing
        // bytes past what `length` claims — e.g. `PeriodicPing` (0x0200)
        // arrives as a 10-byte packet with `length=4`, not the 8 bytes
        // moonlight-common-rust's own (buggy) serializer would produce.
        // Trust `length` and ignore anything after it, only rejecting a
        // packet that's genuinely truncated.
        if length > buffer.len() - 4 {
            return Err(format!("control message length mismatch: header says {length}, buffer has {}", buffer.len() - 4));
        }
        let payload = &buffer[4..4 + length];

        if message_type == CONTROL_MSG_ENCRYPTED {
            let decrypted = decrypt_wrapper(payload, key)?;
            if decrypted.len() < 4 {
                return Err(format!("decrypted control message too short: {} bytes", decrypted.len()));
            }
            let inner_type = u16::from_le_bytes([decrypted[0], decrypted[1]]);
            return Self::from_type_and_payload(inner_type, &decrypted[4..]);
        }
        Self::from_type_and_payload(message_type, payload)
    }

    fn from_type_and_payload(message_type: u16, payload: &[u8]) -> Result<Self, String> {
        match message_type {
            CONTROL_MSG_INPUT_DATA => {
                if payload.len() < 4 {
                    return Err("input data message too short".to_string());
                }
                let event_len = u32::from_be_bytes(payload[0..4].try_into().unwrap()) as usize;
                if event_len > payload.len() - 4 {
                    return Err(format!("input event length mismatch: header says {event_len}, have {}", payload.len() - 4));
                }
                Ok(Self::InputData(payload[4..4 + event_len].to_vec()))
            }
            CONTROL_MSG_REQUEST_IDR_FRAME | CONTROL_MSG_INVALIDATE_REFERENCE_FRAMES => Ok(Self::RequestIdrFrame),
            _ => Ok(Self::Other),
        }
    }
}

/// The `Encrypted` (0x0001) wrapper's payload is `[u32 LE sequence_number][16-byte
/// tag][ciphertext]`; returns the decrypted inner `[type][length][payload]` message.
fn decrypt_wrapper(payload: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, String> {
    const MIN_LEN: usize = 4 + 16 + 4; // sequence_number + tag + minimum inner header
    if payload.len() < MIN_LEN {
        return Err(format!("encrypted control message too short: {} bytes", payload.len()));
    }
    let sequence_number = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let tag: [u8; 16] = payload[4..20].try_into().unwrap();
    let ciphertext = &payload[20..];

    let mut iv = [0u8; 12];
    iv[0..4].copy_from_slice(&sequence_number.to_le_bytes());
    // Serverbound (client -> server, i.e. everything we ever decrypt here)
    // uses 'C' at iv[10] — 'H' is the clientbound marker instead.
    iv[10] = b'C';
    iv[11] = b'C';

    crypto::gcm_decrypt(ciphertext, key, &iv, &tag)
}

/// `InputData` payloads are `[u32 LE input_event_type][type-specific bytes]`.
fn decode_input_event(payload: &[u8]) -> Option<InputEvent> {
    if payload.len() < 4 {
        return None;
    }
    let event_type = u32::from_le_bytes(payload[0..4].try_into().ok()?);
    let body = &payload[4..];

    match event_type {
        0x00000003 => Some(InputEvent::KeyDown { keycode: vk_to_evdev(key_code_from(body)?)? }),
        0x00000004 => Some(InputEvent::KeyUp { keycode: vk_to_evdev(key_code_from(body)?)? }),
        0x00000005 => {
            // MouseMoveAbsolute: x:i16, y:i16, padding:i16, width:i16, height:i16 (big-endian).
            if body.len() < 10 {
                return None;
            }
            Some(InputEvent::MouseMoveAbsolute {
                x: i16::from_be_bytes(body[0..2].try_into().ok()?),
                y: i16::from_be_bytes(body[2..4].try_into().ok()?),
                screen_width: i16::from_be_bytes(body[6..8].try_into().ok()?),
                screen_height: i16::from_be_bytes(body[8..10].try_into().ok()?),
            })
        }
        0x00000007 => {
            if body.len() < 4 {
                return None;
            }
            Some(InputEvent::MouseMoveRelative {
                dx: i16::from_be_bytes(body[0..2].try_into().ok()?),
                dy: i16::from_be_bytes(body[2..4].try_into().ok()?),
            })
        }
        0x00000008 => Some(InputEvent::MouseButtonDown { button: mouse_button_from(body)? }),
        0x00000009 => Some(InputEvent::MouseButtonUp { button: mouse_button_from(body)? }),
        0x0000000A => {
            if body.is_empty() {
                return None;
            }
            Some(InputEvent::ScrollVertical { amount: i16::from_be_bytes(body[0..2].try_into().ok()?) })
        }
        0x55000001 => {
            if body.is_empty() {
                return None;
            }
            Some(InputEvent::ScrollHorizontal { amount: i16::from_be_bytes(body[0..2].try_into().ok()?) })
        }
        _ => None, // gamepad and other event types: deferred (see plan doc)
    }
}

/// The key packet layout is `[flags:u8][key:u16 LE][modifiers:u8][padding:u16]`;
/// virtual key codes fit in a byte, so the low byte of the LE `key` field
/// (index 1) is the actual code.
fn key_code_from(body: &[u8]) -> Option<u8> {
    body.get(1).copied()
}

fn mouse_button_from(body: &[u8]) -> Option<u32> {
    match body.first()? {
        0x01 => Some(0x110), // Left
        0x02 => Some(0x112), // Middle
        0x03 => Some(0x111), // Right
        0x04 => Some(0x113), // Side
        0x05 => Some(0x114), // Extra
        _ => None,
    }
}

/// Windows virtual-key code -> Linux evdev keycode.
fn vk_to_evdev(vk: u8) -> Option<u32> {
    Some(match vk {
        0x08 => 14,  // Backspace
        0x09 => 15,  // Tab
        0x0D => 28,  // Return
        0x10 => 42,  // Shift
        0x11 => 29,  // Control
        0x12 => 56,  // Alt
        0x13 => 119, // Pause
        0x14 => 58,  // Capslock
        0x1B => 1,   // Escape
        0x20 => 57,  // Space
        0x21 => 104, // PageUp
        0x22 => 109, // PageDown
        0x23 => 107, // End
        0x24 => 102, // Home
        0x25 => 105, // Left
        0x26 => 103, // Up
        0x27 => 106, // Right
        0x28 => 108, // Down
        0x2D => 110, // Insert
        0x2E => 111, // Delete
        0x30 => 11,  // Num0
        0x31 => 2,   // Num1
        0x32 => 3,   // Num2
        0x33 => 4,   // Num3
        0x34 => 5,   // Num4
        0x35 => 6,   // Num5
        0x36 => 7,   // Num6
        0x37 => 8,   // Num7
        0x38 => 9,   // Num8
        0x39 => 10,  // Num9
        0x41 => 30,  // A
        0x42 => 48,  // B
        0x43 => 46,  // C
        0x44 => 32,  // D
        0x45 => 18,  // E
        0x46 => 33,  // F
        0x47 => 34,  // G
        0x48 => 35,  // H
        0x49 => 23,  // I
        0x4A => 36,  // J
        0x4B => 37,  // K
        0x4C => 38,  // L
        0x4D => 50,  // M
        0x4E => 49,  // N
        0x4F => 24,  // O
        0x50 => 25,  // P
        0x51 => 16,  // Q
        0x52 => 19,  // R
        0x53 => 31,  // S
        0x54 => 20,  // T
        0x55 => 22,  // U
        0x56 => 47,  // V
        0x57 => 17,  // W
        0x58 => 45,  // X
        0x59 => 21,  // Y
        0x5A => 44,  // Z
        0x5B => 125, // LeftMeta
        0x5C => 126, // RightMeta
        0x60 => 82,  // Numpad0
        0x61 => 79,  // Numpad1
        0x62 => 80,  // Numpad2
        0x63 => 81,  // Numpad3
        0x64 => 75,  // Numpad4
        0x65 => 76,  // Numpad5
        0x66 => 77,  // Numpad6
        0x67 => 71,  // Numpad7
        0x68 => 72,  // Numpad8
        0x69 => 73,  // Numpad9
        0x6A => 55,  // NumpadAsterisk
        0x6B => 78,  // NumpadPlus
        0x6D => 74,  // NumpadMinus
        0x6E => 83,  // NumpadDot
        0x6F => 98,  // NumpadSlash
        0x70 => 59,  // F1
        0x71 => 60,  // F2
        0x72 => 61,  // F3
        0x73 => 62,  // F4
        0x74 => 63,  // F5
        0x75 => 64,  // F6
        0x76 => 65,  // F7
        0x77 => 66,  // F8
        0x78 => 67,  // F9
        0x79 => 68,  // F10
        0x7A => 87,  // F11
        0x7B => 88,  // F12
        0x90 => 69,  // Numlock
        0x91 => 70,  // Scroll
        0xA0 => 42,  // LeftShift
        0xA1 => 54,  // RightShift
        0xA2 => 29,  // LeftControl
        0xA3 => 97,  // RightControl
        0xA4 => 56,  // LeftAlt
        0xA5 => 100, // RightAlt
        0xBA => 39,  // Semicolon
        0xBB => 13,  // Equal
        0xBC => 51,  // Comma
        0xBD => 12,  // Minus
        0xBE => 52,  // Dot
        0xBF => 53,  // Slash
        0xC0 => 41,  // Grave
        0xDB => 26,  // LeftBrace
        0xDC => 43,  // Backslash
        0xDD => 27,  // RightBrace
        0xDE => 40,  // Apostrophe
        0xE2 => 86,  // NonUsBackslash
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encrypt_message(key: &[u8; 16], sequence_number: u32, inner: &[u8]) -> Vec<u8> {
        let mut iv = [0u8; 12];
        iv[0..4].copy_from_slice(&sequence_number.to_le_bytes());
        // Matches decrypt_wrapper's serverbound ('C') marker — this helper
        // simulates a client sending to us.
        iv[10] = b'C';
        iv[11] = b'C';
        let (ciphertext, tag) = crypto::gcm_encrypt(inner, key, &iv).unwrap();

        let mut wrapper_payload = Vec::new();
        wrapper_payload.extend(sequence_number.to_le_bytes());
        wrapper_payload.extend(tag);
        wrapper_payload.extend(ciphertext);

        let mut buffer = Vec::new();
        buffer.extend(CONTROL_MSG_ENCRYPTED.to_le_bytes());
        buffer.extend((wrapper_payload.len() as u16).to_le_bytes());
        buffer.extend(wrapper_payload);
        buffer
    }

    /// Builds a top-level (unencrypted) `[type][length][payload]` InputData
    /// message, where the payload is `[u32 BE event_len][event_type ++ body]`.
    fn input_data_message(event_type: u32, body: &[u8]) -> Vec<u8> {
        let mut event = Vec::new();
        event.extend(event_type.to_le_bytes());
        event.extend(body);

        let mut payload = Vec::new();
        payload.extend((event.len() as u32).to_be_bytes());
        payload.extend(&event);

        let mut message = Vec::new();
        message.extend(CONTROL_MSG_INPUT_DATA.to_le_bytes());
        message.extend((payload.len() as u16).to_le_bytes());
        message.extend(payload);
        message
    }

    #[test]
    fn decrypts_and_decodes_key_down() {
        let key = [0x42u8; 16];
        // KeyDown event body: flags(1) + key(2, LE, low byte = VK code) + modifiers(1) + padding(2).
        let body = [0u8, 0x41, 0, 0, 0, 0]; // VK 'A' = 0x41
        let inner = input_data_message(0x00000003, &body);
        let encrypted = encrypt_message(&key, 7, &inner);

        match ControlMessage::parse(&encrypted, &key).unwrap() {
            ControlMessage::InputData(payload) => {
                let event = decode_input_event(&payload).unwrap();
                assert_eq!(event, InputEvent::KeyDown { keycode: 30 }); // evdev KEY_A
            }
            _ => panic!("expected InputData"),
        }
    }

    #[test]
    fn decrypt_with_wrong_key_fails() {
        let key = [0x42u8; 16];
        let wrong_key = [0x24u8; 16];
        let inner = input_data_message(0x00000003, &[0u8, 0x41, 0, 0, 0, 0]);
        let encrypted = encrypt_message(&key, 1, &inner);
        assert!(ControlMessage::parse(&encrypted, &wrong_key).is_err());
    }

    #[test]
    fn mouse_relative_move_decodes() {
        let key = [0x11u8; 16];
        let body = [0x00, 0x05, 0xFF, 0xFB]; // dx=5, dy=-5 (big-endian i16)
        let inner = input_data_message(0x00000007, &body);
        let encrypted = encrypt_message(&key, 0, &inner);

        match ControlMessage::parse(&encrypted, &key).unwrap() {
            ControlMessage::InputData(payload) => {
                assert_eq!(decode_input_event(&payload), Some(InputEvent::MouseMoveRelative { dx: 5, dy: -5 }));
            }
            _ => panic!("expected InputData"),
        }
    }

    #[test]
    fn request_idr_frame_recognized() {
        let mut buffer = Vec::new();
        buffer.extend(CONTROL_MSG_REQUEST_IDR_FRAME.to_le_bytes());
        buffer.extend(0u16.to_le_bytes());
        let key = [0u8; 16];
        assert!(matches!(ControlMessage::parse(&buffer, &key).unwrap(), ControlMessage::RequestIdrFrame));
    }
}
