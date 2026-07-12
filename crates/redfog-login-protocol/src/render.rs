//! The frame/input wire protocol between `redfog-login` and whatever spawns
//! it as its Login-stage payload (`session_backend::spawn_login_compositor`)
//! — see that function's doc comment for the architecture this replaces
//! (no compositor, no Wayland, no KWin/gst-wayland-display for the Login
//! stage at all: `redfog-login` renders its own frames in-process via
//! `tiny-skia`/`embedded-graphics` — see its own `ui` module — and ships
//! them directly over this one Unix stream instead of through a Wayland
//! socket).
//!
//! Deliberately plain blocking `std::io`, not async: `redfog-login` isn't a
//! tokio app (see this crate's top-level doc comment for why), and the
//! `session-backend` side runs this from a background `std::thread`, not an
//! async task — see `spawn_login_compositor`'s doc comment.
//!
//! Framing: `[u8 tag][u32 LE length][length bytes payload]`, repeated for
//! the life of the connection. Two message kinds share the one duplex
//! stream, flowing in opposite directions — frames one way, input the
//! other — since a login form is low-bandwidth enough that multiplexing
//! them onto separate sockets would just be needless bookkeeping.

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};

const TAG_FRAME: u8 = 1;
const TAG_INPUT: u8 = 2;

/// What the control channel decodes into and forwards here — see
/// `redfog_core::InputSink`'s methods, which this mirrors closely (this
/// crate can't depend on `redfog_core` for the trait itself: same
/// heavy-dependency-graph reasoning as `SessionPreset::backend` being a
/// plain `String` rather than `session_backend::Backend`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LoginInputEvent {
    KeyboardKey { keycode: u32, pressed: bool },
    MouseMoveRelative { dx: f64, dy: f64 },
    MouseMoveAbsolute { x: f64, y: f64 },
    MouseButton { button: u32, pressed: bool },
    MouseAxis { axis: u32, value: f64 },
}

/// A framed message read off the stream — see the module doc comment for
/// the wire format.
pub enum Message {
    /// A rendered frame, login -> spawner. `rgba` is exactly
    /// `width * height * 4` bytes, straight (non-premultiplied at full
    /// opacity, which is all this ever produces) RGBA8, row-major.
    Frame { width: u32, height: u32, rgba: Vec<u8> },
    /// A decoded input event, spawner -> login.
    Input(LoginInputEvent),
}

fn write_message(w: &mut impl Write, tag: u8, payload: &[u8]) -> std::io::Result<()> {
    w.write_all(&[tag])?;
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Writes one [`Message::Frame`]. `rgba` must be exactly
/// `width * height * 4` bytes (row-major RGBA8) — the caller (the `ui`
/// module's renderer) already guarantees this via `Pixmap`'s own sizing.
pub fn write_frame(w: &mut impl Write, width: u32, height: u32, rgba: &[u8]) -> std::io::Result<()> {
    let mut payload = Vec::with_capacity(8 + rgba.len());
    payload.extend_from_slice(&width.to_le_bytes());
    payload.extend_from_slice(&height.to_le_bytes());
    payload.extend_from_slice(rgba);
    write_message(w, TAG_FRAME, &payload)
}

/// Writes one [`Message::Input`].
pub fn write_input(w: &mut impl Write, event: &LoginInputEvent) -> std::io::Result<()> {
    let payload = serde_json::to_vec(event).expect("LoginInputEvent always serializes");
    write_message(w, TAG_INPUT, &payload)
}

/// Reads one framed message. `Ok(None)` means the peer closed the
/// connection cleanly (EOF exactly at a message boundary) — anything else,
/// including EOF *inside* a message, is an error.
pub fn read_message(r: &mut impl Read) -> std::io::Result<Option<Message>> {
    let mut tag_buf = [0u8; 1];
    match r.read_exact(&mut tag_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    match tag_buf[0] {
        TAG_FRAME => {
            if payload.len() < 8 {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame payload too short"));
            }
            let width = u32::from_le_bytes(payload[0..4].try_into().unwrap());
            let height = u32::from_le_bytes(payload[4..8].try_into().unwrap());
            let rgba = payload[8..].to_vec();
            Ok(Some(Message::Frame { width, height, rgba }))
        }
        TAG_INPUT => {
            let event: LoginInputEvent =
                serde_json::from_slice(&payload).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            Ok(Some(Message::Input(event)))
        }
        other => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("unknown login-render message tag {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips() {
        let rgba = vec![1u8, 2, 3, 255, 4, 5, 6, 255]; // 2x1 pixels
        let mut buf = Vec::new();
        write_frame(&mut buf, 2, 1, &rgba).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        match read_message(&mut cursor).unwrap() {
            Some(Message::Frame { width, height, rgba: got }) => {
                assert_eq!((width, height), (2, 1));
                assert_eq!(got, rgba);
            }
            other => panic!("expected Frame, got {other:?}", other = matches_desc(&other)),
        }
    }

    #[test]
    fn input_round_trips() {
        let event = LoginInputEvent::MouseMoveAbsolute { x: 12.5, y: 7.0 };
        let mut buf = Vec::new();
        write_input(&mut buf, &event).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        match read_message(&mut cursor).unwrap() {
            Some(Message::Input(LoginInputEvent::MouseMoveAbsolute { x, y })) => {
                assert_eq!((x, y), (12.5, 7.0));
            }
            other => panic!("expected Input(MouseMoveAbsolute), got {other:?}", other = matches_desc(&other)),
        }
    }

    #[test]
    fn clean_eof_at_boundary_is_none() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        assert!(read_message(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn multiple_messages_in_sequence() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 1, 1, &[9, 9, 9, 255]).unwrap();
        write_input(&mut buf, &LoginInputEvent::KeyboardKey { keycode: 30, pressed: true }).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        assert!(matches!(read_message(&mut cursor).unwrap(), Some(Message::Frame { .. })));
        assert!(matches!(read_message(&mut cursor).unwrap(), Some(Message::Input(LoginInputEvent::KeyboardKey { .. }))));
        assert!(read_message(&mut cursor).unwrap().is_none());
    }

    fn matches_desc(m: &Option<Message>) -> &'static str {
        match m {
            Some(Message::Frame { .. }) => "Frame",
            Some(Message::Input(_)) => "Input",
            None => "None",
        }
    }
}
