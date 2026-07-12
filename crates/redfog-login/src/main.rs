//! Redfog's login screen — renders itself (no compositor, no Wayland, no
//! KWin/gst-wayland-display dependency at all — see `session_backend::
//! spawn_login_compositor`'s doc comment for the architecture this fits
//! into) and ships raw frames + receives decoded input over a single Unix
//! stream (`redfog_login_protocol::render`).
//!
//! Two threads, not one: an early version read one input message, then
//! synchronously wrote a full ~3.7MB uncompressed 1280x720 RGBA frame, in a
//! strict alternating sequence on a single thread — confirmed live, this
//! made the cursor visibly crawl and keyboard input arrive only after a
//! long delay. A real mouse produces relative-move deltas far faster than
//! that loop could cycle once each iteration was paying for a multi-
//! megabyte write, so messages queued up faster than they drained, and
//! keyboard events (much rarer) ended up stuck behind that same backlog in
//! FIFO order. Fix: a dedicated reader thread does nothing but block-read
//! `LoginInputEvent`s and apply them to `Shared` immediately, completely
//! decoupled from however long writing a frame takes; the main thread
//! renders and writes frames on its own independent cadence. `Shared`
//! bundles `LoginUiState` with the `Layout` from its last render under one
//! lock (not two) so a click can never hit-test against a layout that's
//! gone stale relative to the state it's judging (e.g. the dropdown having
//! just opened but its row rects not registered yet).
//!
//! Text input goes through a real `libxkbcommon` keymap (see `keymap`
//! module) — there's no compositor left to do this for us implicitly, so
//! it's done explicitly, the same way KWin/Sway do it internally. Layout
//! is selectable from the login screen's own dropdown, defaulting to US.

mod keymap;
mod ui;

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use redfog_login_protocol::render::{self, LoginInputEvent, Message};
use redfog_login_protocol::{LoginRequest, LoginResponse};
use ui::{Focus, Hit, Layout, LoginUiState};

/// The login screen's "Custom" entry sends this literal sentinel instead of
/// a preset name — see `SessionManager::handle_login_report`'s doc comment
/// for what it means (resolve the target user's own
/// `~/.config/redfog/session.toml` via the broker instead of a fixed
/// operator-defined preset).
const USER_CONFIGURED: &str = "user-configured";

const BTN_LEFT: u32 = 272;
const KEY_BACKSPACE: u32 = 14;
const KEY_TAB: u32 = 15;
const KEY_ENTER: u32 = 28;

/// Sends one request to `redfog-server` over `REDFOG_LOGIN_SOCKET` and
/// returns its response — shared connection/framing logic for
/// `authenticate`/`check_username`/`try_logout`. `Err` here means a
/// transport-level failure (couldn't connect, couldn't read/parse a
/// response) — a *rejected* request (bad password, no such session, ...) is
/// a successful round trip whose `LoginResponse` payload happens to be an
/// `Err`, decided by each caller's own match on the specific variant it
/// expects back.
fn send_login_request(socket_path: &str, request: &LoginRequest) -> Result<LoginResponse, String> {
    let stream = UnixStream::connect(socket_path).map_err(|e| format!("failed to reach session server: {e}"))?;
    let mut writer = stream.try_clone().map_err(|e| format!("failed to reach session server: {e}"))?;
    let mut line = serde_json::to_string(request).expect("protocol types always serialize");
    line.push('\n');
    writer.write_all(line.as_bytes()).map_err(|e| format!("failed to reach session server: {e}"))?;

    let mut response_line = String::new();
    BufReader::new(stream)
        .read_line(&mut response_line)
        .map_err(|e| format!("failed to read response from session server: {e}"))?;
    serde_json::from_str(response_line.trim_end()).map_err(|e| format!("invalid response from session server: {e}"))
}

/// Sends the entered credentials (and the chosen session name — either one
/// of the loaded presets' `name`s, or [`USER_CONFIGURED`] — see
/// `SessionManager::handle_login_report`) to `redfog-server` over
/// `REDFOG_LOGIN_SOCKET` and waits for the real PAM-backed verdict (via the
/// broker's `Authenticate` — see design.md's "Privilege separation: broker
/// vs. server"). Without this env var set (e.g. standalone use with no
/// broker configured), falls back to accepting any non-empty username, same
/// as this app's original no-op placeholder behavior — the session choice
/// is simply never reported in that case.
///
/// Deliberately doesn't take a "resume vs. start fresh" flag: which one
/// happens is decided server-side, from whether `username` already has a
/// background session at the moment this call lands — never from this
/// process's own (possibly stale) `LoginUiState::username_running`, which
/// exists purely to label the button, not to drive real behavior.
fn authenticate(username: &str, password: &str, session: &str) -> Result<(), String> {
    let Ok(socket_path) = std::env::var("REDFOG_LOGIN_SOCKET") else {
        if username.trim().is_empty() {
            return Err("Username cannot be empty".to_string());
        }
        return Ok(());
    };
    let request = LoginRequest::Authenticate { username: username.to_string(), password: password.to_string(), session: session.to_string() };
    match send_login_request(&socket_path, &request)? {
        LoginResponse::Authenticate(result) => result,
        other => Err(format!("unexpected response to Authenticate: {other:?}")),
    }
}

/// Whether `username` already has a running session — see
/// `LoginRequest::CheckUsername`'s doc comment for why this needs no
/// password. `None` (rather than defaulting to "not running") whenever the
/// question genuinely can't be answered — no session server configured, an
/// empty username, or a transport failure — so the caller shows "Log In"
/// (the safe default) rather than confidently showing the wrong label.
/// Errors are logged, not surfaced to the user: failing to answer this
/// purely cosmetic question shouldn't block them from just trying to log
/// in anyway.
fn check_username(username: &str) -> Option<bool> {
    let socket_path = std::env::var("REDFOG_LOGIN_SOCKET").ok()?;
    if username.trim().is_empty() {
        return None;
    }
    match send_login_request(&socket_path, &LoginRequest::CheckUsername { username: username.to_string() }) {
        Ok(LoginResponse::CheckUsername { running }) => Some(running),
        Ok(other) => {
            eprintln!("redfog-login: unexpected response to CheckUsername: {other:?}");
            None
        }
        Err(e) => {
            eprintln!("redfog-login: failed to check username: {e}");
            None
        }
    }
}

fn try_submit(state: &mut LoginUiState) {
    let session_name = if state.selected_session == state.sessions.len() - 1 {
        USER_CONFIGURED.to_string()
    } else {
        state.sessions[state.selected_session].clone()
    };
    match authenticate(&state.username, &state.password, &session_name) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            state.password.clear();
            state.error_msg = Some(e);
        }
    }
}

/// Unlike `try_submit`, never exits the process on success — logging
/// another session out doesn't mean *this* login screen's own job here is
/// done; it stays up, ready for an actual login attempt.
fn try_logout(state: &mut LoginUiState) {
    let Ok(socket_path) = std::env::var("REDFOG_LOGIN_SOCKET") else {
        state.error_msg = Some("Log Out requires a session server (no REDFOG_LOGIN_SOCKET configured)".to_string());
        return;
    };
    let request = LoginRequest::LogOut { username: state.username.clone(), password: state.password.clone() };
    let result = match send_login_request(&socket_path, &request) {
        Ok(LoginResponse::LogOut(result)) => result,
        Ok(other) => Err(format!("unexpected response to LogOut: {other:?}")),
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => {
            state.password.clear();
            state.error_msg = None;
            // The button/control refresh themselves immediately — no
            // separate "Logged Out" confirmation message needed; the
            // Resume/Log Out UI simply disappearing back to a plain "Log
            // In" button already communicates it worked.
            state.username_running = Some(false);
        }
        Err(e) => state.error_msg = Some(e),
    }
}

/// Re-checks `LoginUiState::username_running` for whatever's currently
/// typed — called whenever focus leaves the username field (see
/// `handle_click`/`handle_key`'s `KEY_TAB` arm), so the primary button's
/// label is correct by the time a user's about to click it, matching how
/// they'd naturally type username -> Tab/click -> password -> submit.
fn maybe_check_username(state: &mut LoginUiState) {
    state.username_running = if state.username.trim().is_empty() { None } else { check_username(&state.username) };
}

fn handle_click(state: &mut LoginUiState, layout: &Layout) {
    let hit = layout.hit_test(state.cursor_pos.0, state.cursor_pos.1);
    if state.session_dropdown_open || state.keyboard_dropdown_open {
        // Whichever popup is open is on top of everything else — any
        // click, hit or miss, is about that popup first. A click on one of
        // its own rows selects it; anything else (including its own
        // toggle, or a click that lands on whatever's visually underneath
        // it) just closes it, matching how real dropdown menus swallow
        // their first outside click rather than also acting on what's
        // beneath it. Both dropdowns' open flags are cleared unconditionally
        // here (not just whichever was actually open) — simplest way to
        // maintain "at most one open at a time" without extra bookkeeping,
        // since clearing an already-false flag is a no-op.
        match hit {
            Hit::SessionOption(i) => state.selected_session = i,
            Hit::KeyboardOption(i) => state.selected_layout = i,
            _ => {}
        }
        state.session_dropdown_open = false;
        state.keyboard_dropdown_open = false;
        return;
    }
    let was_username_focused = state.focus == Focus::Username;
    match hit {
        Hit::Username => state.focus = Focus::Username,
        Hit::Password => state.focus = Focus::Password,
        Hit::SessionToggle => state.session_dropdown_open = true,
        Hit::KeyboardToggle => state.keyboard_dropdown_open = true,
        Hit::SessionOption(_) | Hit::KeyboardOption(_) => {} // unreachable while closed — no rows to hit
        Hit::LoginButton => try_submit(state),
        Hit::LogoutButton => try_logout(state),
        Hit::None => {}
    }
    if was_username_focused && state.focus != Focus::Username {
        maybe_check_username(state);
    }
}

/// `text` is whatever `keymap::Keymap::key_event` resolved this press to —
/// empty for keys that don't produce text on their own (modifiers, dead
/// keys awaiting a second keystroke, ...).
fn handle_key(state: &mut LoginUiState, keycode: u32, text: &str) {
    match keycode {
        KEY_BACKSPACE => match state.focus {
            Focus::Username => {
                state.username.pop();
                state.username_running = None; // stale relative to the new (shorter) text
            }
            Focus::Password => {
                state.password.pop();
            }
            Focus::None => {}
        },
        KEY_TAB => {
            let was_username_focused = state.focus == Focus::Username;
            state.focus = match state.focus {
                Focus::Username => Focus::Password,
                Focus::Password | Focus::None => Focus::Username,
            };
            if was_username_focused {
                maybe_check_username(state);
            }
        }
        KEY_ENTER => try_submit(state),
        _ => {
            // Anything else: insert whatever the keymap resolved this
            // press to, if it's printable text — guards against control
            // characters defensively (e.g. Escape produces one on some
            // layouts), even though Backspace/Tab/Enter's own control
            // characters are already handled by the explicit arms above.
            if !text.is_empty() && !text.chars().any(|c| c.is_control()) {
                match state.focus {
                    Focus::Username => {
                        state.username.push_str(text);
                        state.username_running = None; // stale relative to the new text
                    }
                    Focus::Password => state.password.push_str(text),
                    Focus::None => {}
                }
            }
        }
    }
}

/// `LoginUiState` bundled with the `Layout` from its own last render — see
/// the module doc comment for why these live under one lock instead of two.
struct Shared {
    state: LoginUiState,
    layout: Layout,
}

/// `keymap` is owned entirely by the caller's thread (see `main`'s reader
/// thread) — it's rebuilt there, outside this function, whenever
/// `state.selected_layout` changes; this function only ever feeds it key
/// events, never swaps it out.
fn handle_input(shared: &mut Shared, keymap: &mut keymap::Keymap, event: LoginInputEvent) {
    let Shared { state, layout } = shared;
    match event {
        LoginInputEvent::MouseMoveAbsolute { x, y } => {
            state.cursor_pos = (x.clamp(0.0, state.width as f64 - 1.0), y.clamp(0.0, state.height as f64 - 1.0));
        }
        LoginInputEvent::MouseMoveRelative { dx, dy } => {
            let (x, y) = state.cursor_pos;
            state.cursor_pos = ((x + dx).clamp(0.0, state.width as f64 - 1.0), (y + dy).clamp(0.0, state.height as f64 - 1.0));
        }
        LoginInputEvent::MouseButton { button, pressed } => {
            if button == BTN_LEFT && pressed {
                handle_click(state, layout);
            }
        }
        LoginInputEvent::MouseAxis { .. } => {} // no scrollable content on this screen
        LoginInputEvent::KeyboardKey { keycode, pressed } => {
            // Always fed through, even for releases and non-text keys
            // (Shift, etc.) — XKB's internal modifier tracking silently
            // desyncs otherwise (see `Keymap::key_event`'s doc comment).
            let text = keymap.key_event(keycode, pressed);
            if pressed {
                handle_key(state, keycode, &text);
            }
        }
    }
}

fn main() {
    let width: u32 = std::env::var("REDFOG_LOGIN_WIDTH").ok().and_then(|v| v.parse().ok()).unwrap_or(1920);
    let height: u32 = std::env::var("REDFOG_LOGIN_HEIGHT").ok().and_then(|v| v.parse().ok()).unwrap_or(1080);
    let frame_socket_path =
        std::env::var("REDFOG_LOGIN_FRAME_SOCKET").expect("REDFOG_LOGIN_FRAME_SOCKET must be set (see session_backend::spawn_login_compositor)");

    let sessions_path = std::env::var("REDFOG_SESSIONS_CONFIG").unwrap_or_else(|_| redfog_login_protocol::DEFAULT_SESSIONS_CONFIG_PATH.to_string());
    let presets = match redfog_login_protocol::load_presets(&sessions_path) {
        Ok(presets) => presets,
        Err(e) => {
            eprintln!("redfog-login: failed to load {sessions_path}: {e} — falling back to built-in defaults");
            redfog_login_protocol::default_presets()
        }
    };
    let mut session_names: Vec<String> = presets.iter().map(|p| p.name.clone()).collect();
    session_names.push("Custom".to_string());

    let stream =
        UnixStream::connect(&frame_socket_path).unwrap_or_else(|e| panic!("failed to connect to login frame socket {frame_socket_path}: {e}"));

    let state = LoginUiState::new(width, height, session_names);
    let initial_layout_code = state.keyboard_layouts[state.selected_layout].0.clone();
    let (_pixmap, layout) = ui::render(&state);
    let shared = Arc::new(Mutex::new(Shared { state, layout }));

    // Reader thread: does nothing but block-read input messages and apply
    // them — never blocked by, or blocking, frame writes (see the module
    // doc comment). Owns the `Keymap` outright (never shared across
    // threads, so no locking needed for it) and rebuilds it whenever the
    // login screen's own keyboard dropdown picks a different layout.
    let reader_stream = stream.try_clone().expect("failed to clone login frame stream for the reader thread");
    {
        let shared = shared.clone();
        std::thread::spawn(move || {
            let mut reader_stream = reader_stream;
            let mut keymap = keymap::Keymap::new(&initial_layout_code);
            let mut current_layout_code = initial_layout_code;
            loop {
                match render::read_message(&mut reader_stream) {
                    Ok(Some(Message::Input(event))) => {
                        let mut shared = shared.lock().unwrap();
                        handle_input(&mut shared, &mut keymap, event);
                        let new_layout_code = &shared.state.keyboard_layouts[shared.state.selected_layout].0;
                        if *new_layout_code != current_layout_code {
                            current_layout_code = new_layout_code.clone();
                            keymap = keymap::Keymap::new(&current_layout_code);
                        }
                    }
                    Ok(Some(Message::Frame { .. })) => {} // wrong direction on this stream, ignore
                    Ok(None) => std::process::exit(0),    // peer closed cleanly
                    Err(e) => {
                        eprintln!("redfog-login: frame socket read error: {e}");
                        std::process::exit(1);
                    }
                }
            }
        });
    }

    // Main thread: render/write frames on its own fixed cadence, entirely
    // independent of however fast input happens to be arriving. Clones
    // `LoginUiState` out and releases the lock *before* calling
    // `ui::render` — even though render is fast now (a couple of ms in
    // release builds, tens of ms in debug — see `fill_cached_background`'s
    // doc comment for how much slower it was before that fix), holding a
    // lock the reader thread also needs for the length of a render call is
    // still needless contention the reader thread shouldn't have to wait
    // through, and debug builds in particular are still meaningfully
    // slower than release ones.
    let mut write_stream = stream;
    let mut last_blink = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(33)); // ~30fps

        let state_snapshot = {
            let mut shared = shared.lock().unwrap();
            if last_blink.elapsed() >= Duration::from_millis(500) {
                shared.state.caret_blink_on = !shared.state.caret_blink_on;
                last_blink = Instant::now();
            }
            shared.state.clone()
        };

        let (pixmap, layout) = ui::render(&state_snapshot);
        shared.lock().unwrap().layout = layout;

        if render::write_frame(&mut write_stream, width, height, pixmap.data()).is_err() {
            break; // peer gone
        }
    }

    let _ = write_stream.shutdown(Shutdown::Both);
}
