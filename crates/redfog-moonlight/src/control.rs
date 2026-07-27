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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio_enet::{Event, Host, HostConfig, PeerId};

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

/// `rikey` identifies which session a message came from — matched by
/// actually decrypting with it (see `ControlServer::serve`'s doc comment),
/// not by source address. `SessionManager` scans its sessions for whichever
/// one's own `origin.rikey` equals this to find which one to act on.
pub trait ControlEventHandler: Send + Sync {
    fn on_input(&self, rikey: [u8; 16], event: InputEvent);
    fn on_request_idr_frame(&self, rikey: [u8; 16]);
    /// `LossStats` (0x0201) — sent regularly by every real client (not a
    /// Sunshine/Foundation extension, base protocol), carrying the frame
    /// index of the last frame it fully received. Real Sunshine already
    /// uses this signal to drive server-side adaptive bitrate; see
    /// `redfog_core::set_encoder_bitrate`'s doc comment for why bitrate
    /// specifically (unlike resolution/fps) needs no client cooperation
    /// beyond this existing report.
    fn on_loss_stats(&self, rikey: [u8; 16], last_good_frame: u64);
}

pub struct NoopControlEventHandler;
impl ControlEventHandler for NoopControlEventHandler {
    fn on_input(&self, _rikey: [u8; 16], _event: InputEvent) {}
    fn on_request_idr_frame(&self, _rikey: [u8; 16]) {}
    fn on_loss_stats(&self, _rikey: [u8; 16], _last_good_frame: u64) {}
}

/// `epoch` is bumped on every `register()` call for a `rikey` (whether it's
/// a fresh registration or a re-registration of the same value) —
/// `ControlServer::serve`'s loop compares each currently-matched peer's
/// remembered epoch against the live one and disconnects it the moment
/// they diverge. This is what actually disconnects a stale ENet peer on a
/// genuine retake or session end — scoped per rikey rather than a single
/// global sweep, so one session's takeover never touches another's
/// still-live connection.
///
/// Registered rikeys (not addresses): matching works by actually
/// decrypting an incoming message with each candidate key and seeing which
/// one authenticates (GCM's tag either verifies or it doesn't) — see
/// `ControlServer::serve`'s own doc comment for why this replaced
/// address-based matching (can't tell two concurrent clients sharing a
/// source address apart) and is, unlike that, genuinely cryptographic: an
/// attacker without the right rikey cannot produce a message that
/// authenticates, regardless of where it's sent from.
#[derive(Default)]
struct RegistryState {
    registrations: HashMap<[u8; 16], u64>,
}

/// Shared between `SessionManager` (writes, via `register`/`forget` — see
/// `on_play`/`launch`/`take_active_session`) and `ControlServer::serve`
/// (reads, via `snapshot`) — the control-channel analogue of
/// `MultiClientUdpSender`: one ENet host bound once at server startup,
/// concurrent sessions distinguished by their own registration rather than
/// a single shared key.
pub struct ControlRegistry {
    state: Mutex<RegistryState>,
    next_epoch: AtomicU64,
}

impl Default for ControlRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlRegistry {
    pub fn new() -> Self {
        Self { state: Mutex::new(RegistryState::default()), next_epoch: AtomicU64::new(0) }
    }

    /// Registers (or re-registers, for a retake — see `RegistryState`'s doc
    /// comment) `rikey` as a currently-valid control-channel key. Idempotent
    /// to call repeatedly with the same value (each call still bumps the
    /// epoch and thus still triggers a reconnect-sweep of the previously-
    /// matched peer, if any — harmless, just a peer re-matching itself on
    /// the next loop tick).
    pub fn register(&self, rikey: [u8; 16]) {
        let epoch = self.next_epoch.fetch_add(1, Ordering::AcqRel);
        self.state.lock().unwrap().registrations.insert(rikey, epoch);
    }

    /// Removes `rikey`'s registration entirely — its ENet peer (if matched)
    /// gets disconnected on the `serve` loop's next tick, same as any other
    /// now-stale registration.
    pub fn forget(&self, rikey: [u8; 16]) {
        self.state.lock().unwrap().registrations.remove(&rikey);
    }

    fn snapshot(&self) -> HashMap<[u8; 16], u64> {
        self.state.lock().unwrap().registrations.clone()
    }
}

pub struct ControlServer {
    pub port: u16,
    /// Bound once at server startup (not per-launch, not per-session) —
    /// same reasoning as `SessionManager::video_sender`/`audio_sender`: one
    /// shared ENet host for the whole server's lifetime, with concurrent
    /// sessions distinguished by their own registered rikey rather than a
    /// single shared key.
    pub registry: Arc<ControlRegistry>,
    pub handler: Arc<dyn ControlEventHandler>,
}

/// How long an ENet peer is allowed to sit connected without ever sending a
/// message that authenticates against some registered rikey before it's
/// given up on and disconnected. Real clients always connect the control
/// channel only after RTSP `PLAY` (whose handling registers the rikey this
/// peer needs to match — see `SessionManager::on_play`), but that
/// registration happens on a different task than this loop, so there's an
/// inherent, normally brief race — this timeout only guards against it
/// never resolving at all (an abandoned/bogus connection, or one that only
/// ever sends unencrypted messages — see `is_encrypted_message`'s doc
/// comment for why those can't be used to match at all).
const PENDING_MATCH_TIMEOUT: Duration = Duration::from_secs(10);

impl ControlServer {
    /// Matches each new ENet peer to a session by *decrypting* its first
    /// matchable message against every currently-registered rikey and
    /// seeing which one authenticates (GCM's 16-byte tag either verifies or
    /// it doesn't — there's no meaningful chance of a false match). This
    /// replaced an earlier version that matched by the peer's source
    /// address instead: confirmed live that plain IP can't tell two
    /// concurrent clients apart when they share one (same NAT, or literally
    /// the same machine) — decrypt-based matching sidesteps that entirely,
    /// and is genuinely cryptographic besides (see `RegistryState`'s doc
    /// comment). Once a peer matches, its rikey is cached (`peer_rikey`) so
    /// only its *first* message ever needs the O(sessions) trial — every
    /// later message decrypts directly against the one already-known key.
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
        // Peers matched to a rikey, and the epoch they were matched under
        // (see `RegistryState`'s doc comment for why epoch, not rikey
        // alone, is what decides staleness).
        let mut peer_rikey: HashMap<PeerId, ([u8; 16], u64)> = HashMap::new();
        // Connected but not yet matched to any rikey, with when they
        // connected — see `PENDING_MATCH_TIMEOUT`.
        let mut pending: HashMap<PeerId, Instant> = HashMap::new();

        loop {
            let regs = self.registry.snapshot();

            // Disconnect any peer whose matched rikey has since been
            // superseded (a retake re-registered it, bumping its epoch) or
            // removed entirely (the session ended).
            let stale: Vec<PeerId> = peer_rikey.iter().filter(|(_, &(rikey, epoch))| regs.get(&rikey) != Some(&epoch)).map(|(&id, _)| id).collect();
            for peer_id in &stale {
                host.disconnect_now(*peer_id, 0);
                peer_rikey.remove(peer_id);
            }
            if !stale.is_empty() {
                tracing::info!("control channel: disconnected {} stale peer(s) for session takeover", stale.len());
            }

            // Give up on any peer that's been connected too long without
            // ever sending a message we could match (see
            // `PENDING_MATCH_TIMEOUT`'s doc comment).
            pending.retain(|&peer_id, &mut connected_at| {
                if connected_at.elapsed() > PENDING_MATCH_TIMEOUT {
                    tracing::warn!("control channel: peer {peer_id:?} never matched a session within {PENDING_MATCH_TIMEOUT:?}, disconnecting");
                    host.disconnect_now(peer_id, 0);
                    false
                } else {
                    true
                }
            });

            match host.service(Duration::from_millis(100)).await {
                Ok(Some(Event::Connect { peer_id, .. })) => {
                    tracing::info!("control channel: peer {peer_id:?} connected");
                    pending.insert(peer_id, Instant::now());
                }
                Ok(Some(Event::Disconnect { peer_id, .. })) => {
                    tracing::info!("control channel: peer {peer_id:?} disconnected");
                    peer_rikey.remove(&peer_id);
                    pending.remove(&peer_id);
                }
                Ok(Some(Event::Receive { peer_id, packet, .. })) => {
                    if let Some(&(rikey, _)) = peer_rikey.get(&peer_id) {
                        self.handle_message(rikey, packet.data());
                    } else if is_encrypted_message(packet.data()) {
                        match regs.iter().find(|(rikey, _)| ControlMessage::parse(packet.data(), rikey).is_ok()) {
                            Some((&rikey, &epoch)) => {
                                tracing::info!("control channel: peer {peer_id:?} matched");
                                peer_rikey.insert(peer_id, (rikey, epoch));
                                pending.remove(&peer_id);
                                self.handle_message(rikey, packet.data());
                            }
                            None => tracing::debug!("control channel: peer {peer_id:?} sent an encrypted message that didn't authenticate against any registered session"),
                        }
                    } else {
                        // Unencrypted messages (base-protocol keepalives
                        // etc.) don't even read the key they're "decrypted"
                        // with — see `is_encrypted_message`'s doc comment —
                        // so there's nothing here to match on. Wait for a
                        // later, actually-encrypted message instead.
                        tracing::trace!("control channel: ignoring unencrypted message from unmatched peer {peer_id:?}");
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("control channel enet error: {e}"),
            }
        }
    }

    fn handle_message(&self, rikey: [u8; 16], buffer: &[u8]) {
        match ControlMessage::parse(buffer, &rikey) {
            Ok(ControlMessage::InputData(payload)) => match decode_input_event(&payload) {
                Some(event) => self.handler.on_input(rikey, event),
                None => tracing::trace!("unhandled/unknown input event"),
            },
            Ok(ControlMessage::RequestIdrFrame) => self.handler.on_request_idr_frame(rikey),
            Ok(ControlMessage::LossStats { last_good_frame }) => self.handler.on_loss_stats(rikey, last_good_frame),
            Ok(ControlMessage::Other) => {} // Ping/FrameStats/etc — ignored in v1.
            Err(e) => tracing::debug!("bad control message: {e}"),
        }
    }
}

/// Whether `buffer`'s top-level message type is `Encrypted` (0x0001) — the
/// only kind that actually reads the key it's parsed with (see
/// `ControlMessage::parse`: any other type ignores the `key` argument
/// entirely). Used to decide whether an unmatched peer's message is even
/// usable for decrypt-based session matching (see `ControlServer::serve`'s
/// doc comment) — trying every registered key against an unencrypted
/// message would "succeed" identically for all of them (nothing to
/// authenticate), which would match to an arbitrary, likely wrong, session.
fn is_encrypted_message(buffer: &[u8]) -> bool {
    buffer.len() >= 4 && u16::from_le_bytes([buffer[0], buffer[1]]) == CONTROL_MSG_ENCRYPTED
}

const CONTROL_MSG_ENCRYPTED: u16 = 0x0001;
const CONTROL_MSG_LOSS_STATS: u16 = 0x0201;
const CONTROL_MSG_INPUT_DATA: u16 = 0x0206;
const CONTROL_MSG_REQUEST_IDR_FRAME: u16 = 0x0302;
const CONTROL_MSG_INVALIDATE_REFERENCE_FRAMES: u16 = 0x0301;

enum ControlMessage {
    InputData(Vec<u8>),
    RequestIdrFrame,
    LossStats { last_good_frame: u64 },
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
            CONTROL_MSG_LOSS_STATS => {
                // Layout (confirmed against moonlight-common-rust's own
                // serialize/deserialize, not vendored into git — see
                // scripts/fetch-patched-deps.sh):
                // [u32 LE unknown][u32 LE loss_report_interval_ms][u32 LE unknown]
                // [u64 LE last_good_frame][u32 LE unknown][u32 LE unknown][u32 LE unknown],
                // 32 bytes total — we only need the 8 bytes at offset 12.
                if payload.len() < 20 {
                    return Err(format!("loss stats message too short: {} bytes", payload.len()));
                }
                let last_good_frame = u64::from_le_bytes(payload[12..20].try_into().unwrap());
                Ok(Self::LossStats { last_good_frame })
            }
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

    #[test]
    fn loss_stats_extracts_last_good_frame() {
        let mut payload = Vec::new();
        payload.extend(0u32.to_le_bytes()); // unknown1
        payload.extend(1000u32.to_le_bytes()); // loss_report_interval_ms
        payload.extend(1000u32.to_le_bytes()); // unknown2
        payload.extend(424242u64.to_le_bytes()); // last_good_frame
        payload.extend(0u32.to_le_bytes()); // unknown3
        payload.extend(0u32.to_le_bytes()); // unknown4
        payload.extend(0x14u32.to_le_bytes()); // unknown5

        let mut buffer = Vec::new();
        buffer.extend(CONTROL_MSG_LOSS_STATS.to_le_bytes());
        buffer.extend((payload.len() as u16).to_le_bytes());
        buffer.extend(&payload);

        let key = [0u8; 16];
        match ControlMessage::parse(&buffer, &key).unwrap() {
            ControlMessage::LossStats { last_good_frame } => assert_eq!(last_good_frame, 424242),
            _ => panic!("expected LossStats"),
        }
    }

    #[test]
    fn loss_stats_too_short_is_rejected() {
        let mut buffer = Vec::new();
        buffer.extend(CONTROL_MSG_LOSS_STATS.to_le_bytes());
        buffer.extend(8u16.to_le_bytes());
        buffer.extend([0u8; 8]);
        let key = [0u8; 16];
        assert!(ControlMessage::parse(&buffer, &key).is_err());
    }
}
