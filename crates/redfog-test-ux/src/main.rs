//! Minimal test-only stand-in for `redfog-login`/the real desktop session,
//! used by `redfog-moonlight`'s self-contained integration test instead of
//! driving the real login GUI or an external app like `glxgears`.
//!
//! Two modes, auto-detected via `REDFOG_LOGIN_FRAME_SOCKET`:
//! - **User stage** (that env var unset): a real eframe/Wayland client,
//!   repainting continuously (guarantees a steady stream of frames
//!   regardless of input activity — an idle desktop only redraws once a
//!   minute, useless for testing).
//! - **Login stage** (that env var set — see `session_backend::
//!   spawn_login_compositor`): the Login stage is always headless now, so
//!   this speaks `redfog_login_protocol::render` directly instead of
//!   opening any window at all — connects to the socket, sends a fixed
//!   solid-color frame on a steady cadence, and decodes `LoginInputEvent`s
//!   the same way the real `redfog-login` does.
//!
//! Both modes log every mouse/key event they receive to stdout in a format
//! the test can grep for, giving direct proof that input sent through the
//! control channel actually reached this session — not just that the
//! client's send didn't error. Both exit on 'Q' (the same
//! `--exit-with-session`-equivalent trigger a real login success uses), so
//! the test can drive the Login->User handoff deterministically instead of
//! racing on a shared global file.
//!
//! Tags its log lines: `"login"` for the (always backend-independent)
//! Login stage, or the Wayland socket name (`redfog-user-0`/`wayland-1`,
//! depending on backend — set as `WAYLAND_DISPLAY` by whichever compositor
//! owns the User stage) otherwise — so the test can tell which stage
//! produced a given line. Deliberately not a `--label=` CLI arg:
//! `kwin_wayland --exit-with-session <cmd> -- <args>` does NOT pass
//! `<args>` through to `<cmd>` — confirmed live, `--no-respawn` never
//! reached `plasmashell` either, a pre-existing silent no-op.

use eframe::egui;

/// evdev keycode for 'Q' — what a real login-success/`redfog-test-ux`'s own
/// exit trigger uses; input arrives here already translated from whatever
/// wire keycode the client sent (see `redfog_moonlight::control`'s
/// `vk_to_evdev`).
const KEY_Q: u32 = 16;

/// The Login stage is always headless now — no Wayland, no compositor, no
/// eframe/winit window at all (see `session_backend::
/// spawn_login_compositor`'s doc comment). Mirrors the real `redfog-login`'s
/// own main loop shape (a single thread, alternating a short-timeout read
/// with a frame write) closely enough to be a faithful stand-in, but sends
/// a fixed solid-color frame instead of actually rendering anything — this
/// is a test double, not a second UI to maintain.
fn run_headless_login(frame_socket_path: String) {
    use std::io::ErrorKind;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let label = "login";
    let width: u32 = std::env::var("REDFOG_LOGIN_WIDTH").ok().and_then(|v| v.parse().ok()).unwrap_or(1280);
    let height: u32 = std::env::var("REDFOG_LOGIN_HEIGHT").ok().and_then(|v| v.parse().ok()).unwrap_or(720);

    println!("TESTUX[{label}]: started");

    let mut stream =
        UnixStream::connect(&frame_socket_path).unwrap_or_else(|e| panic!("failed to connect to login frame socket {frame_socket_path}: {e}"));
    stream.set_read_timeout(Some(Duration::from_millis(16))).expect("set_read_timeout");

    let rgba = vec![80u8; (width as usize) * (height as usize) * 4];
    let mut last_pos: Option<(f64, f64)> = None;

    loop {
        use redfog_login_protocol::render::{read_message, LoginInputEvent, Message};
        match read_message(&mut stream) {
            Ok(Some(Message::Input(LoginInputEvent::MouseMoveAbsolute { x, y }))) => {
                let (dx, dy) = last_pos.map_or((0.0, 0.0), |(lx, ly)| (x - lx, y - ly));
                println!("TESTUX[{label}]: pointer_moved dx={dx} dy={dy} x={x} y={y}");
                last_pos = Some((x, y));
            }
            Ok(Some(Message::Input(LoginInputEvent::MouseMoveRelative { dx, dy }))) => {
                let (lx, ly) = last_pos.unwrap_or((0.0, 0.0));
                let (x, y) = (lx + dx, ly + dy);
                println!("TESTUX[{label}]: pointer_moved dx={dx} dy={dy} x={x} y={y}");
                last_pos = Some((x, y));
            }
            Ok(Some(Message::Input(LoginInputEvent::MouseButton { button, pressed }))) => {
                println!("TESTUX[{label}]: pointer_button button={button} pressed={pressed}");
            }
            Ok(Some(Message::Input(LoginInputEvent::KeyboardKey { keycode, pressed: true }))) => {
                println!("TESTUX[{label}]: key_pressed key={keycode}");
                if keycode == KEY_Q {
                    println!("TESTUX[{label}]: exiting on Q");
                    std::process::exit(0);
                }
            }
            Ok(Some(Message::Input(LoginInputEvent::KeyboardKey { pressed: false, .. } | LoginInputEvent::MouseAxis { .. }))) => {}
            Ok(Some(Message::Frame { .. })) => {} // wrong direction on this stream, ignore
            Ok(None) => break,                    // peer closed cleanly
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
            Err(e) => {
                eprintln!("redfog-test-ux: login frame socket read error: {e}");
                break;
            }
        }

        if redfog_login_protocol::render::write_frame(&mut stream, width, height, &rgba).is_err() {
            break; // peer gone
        }
    }
}

fn main() -> Result<(), eframe::Error> {
    if let Ok(frame_socket_path) = std::env::var("REDFOG_LOGIN_FRAME_SOCKET") {
        run_headless_login(frame_socket_path);
        return Ok(());
    }

    let label = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "default".to_string());

    println!("TESTUX[{label}]: started");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Redfog Test UX")
            .with_inner_size([400.0, 300.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Redfog Test UX",
        options,
        Box::new(|_cc| Box::new(TestUxApp { label, last_pos: None })),
    )
}

struct TestUxApp {
    label: String,
    last_pos: Option<egui::Pos2>,
}

impl eframe::App for TestUxApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // egui only repaints on input by default. Streaming needs a steady
        // stream of Wayland surface commits regardless of user interaction —
        // KWin's screencast only pushes a PipeWire frame when a client
        // commits a new buffer, so without this the capture pipeline sends
        // one frame and then stalls (see redfog-login's identical comment).
        ctx.request_repaint_after(std::time::Duration::from_millis(33));

        let label = &self.label;
        ctx.input(|i| {
            for event in &i.events {
                match event {
                    egui::Event::PointerMoved(pos) => {
                        let (dx, dy) = match self.last_pos {
                            Some(last) => (pos.x - last.x, pos.y - last.y),
                            // No prior position to diff against (the very
                            // first move) — still log it, just as an
                            // absolute position with a nominal zero delta,
                            // rather than silently swallowing it.
                            None => (0.0, 0.0),
                        };
                        println!("TESTUX[{label}]: pointer_moved dx={dx} dy={dy} x={} y={}", pos.x, pos.y);
                        self.last_pos = Some(*pos);
                    }
                    egui::Event::PointerButton { button, pressed, .. } => {
                        println!("TESTUX[{label}]: pointer_button button={button:?} pressed={pressed}");
                    }
                    egui::Event::Key { key, pressed: true, repeat: false, .. } => {
                        println!("TESTUX[{label}]: key_pressed key={key:?}");
                        if *key == egui::Key::Q {
                            println!("TESTUX[{label}]: exiting on Q");
                            std::process::exit(0);
                        }
                    }
                    _ => {}
                }
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(format!("Test UX [{label}]"));
            ui.label("Press Q to exit (simulates login success / session end)");
        });
    }
}
