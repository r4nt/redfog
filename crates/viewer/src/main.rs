//! Swiss-army-knife debug viewer: a single winit window that can exercise
//! any combination of {compositor backend} x {how the User stage gets
//! spawned}, for local interactive debugging — replaces the former
//! `kwin-viewer`/`gst-viewer`, which duplicated ~250 lines of winit/evdev
//! boilerplate between them despite both already targeting the same
//! `redfog_core::VideoSource`/`InputSink` trait boundary. The actual
//! `Backend`/`SpawnedCompositor` abstraction lives in `session-backend`,
//! shared with the real production login flow in
//! `redfog-moonlight::session` — adding a third backend, or a third way to
//! spawn the User stage, only needs to be taught there, not duplicated here
//! and in production separately.
//!
//! Usage:
//!   viewer --backend <kwin|gst> --mode <single|handoff|broker> [options] [-- payload...]
//!
//! Modes:
//!   single  - one direct-spawn compositor running `payload` for the whole
//!             lifetime, no Login stage at all — the fastest loop for
//!             iterating on a backend/payload in isolation (gst-viewer's
//!             old default behavior, now available for either backend).
//!   handoff - Login compositor (--login-app) first; once it exits
//!             successfully, hands off to a User compositor running
//!             `payload` — both direct-spawned. The old kwin-viewer's only
//!             mode, now backend-generic.
//!   broker  - like handoff, but the User-stage spawn goes through a real
//!             redfog-broker (--broker-socket, --username, --password)
//!             instead of direct-spawning — neither old viewer could do
//!             this at all.
//!
//! Run with --help for the full flag list.
//!
//! Window resizing (debounced, 200ms) calls `SpawnedCompositor::resize` —
//! a real live resize for Backend::Kwin, a documented no-op for
//! Backend::GstWaylandDisplay (its capsfilter caps are fixed at
//! construction — see `gst_backend::make_source_element`'s doc comment),
//! so this handler doesn't special-case backends itself at all.

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gstreamer as gst;
use gst::prelude::*;
use winit::{
    event::{ElementState, Event, WindowEvent, MouseButton},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
    keyboard::{KeyCode, PhysicalKey},
};

use redfog_core::{Frame, SessionType};
use session_backend::{Backend, NestedSessionConfig, SpawnedCompositor};

#[derive(Debug)]
enum UserEvent {
    NewFrame,
    FrameSizeChanged,
}

fn winit_key_to_evdev(code: KeyCode) -> Option<u32> {
    match code {
        KeyCode::KeyA => Some(30),
        KeyCode::KeyB => Some(48),
        KeyCode::KeyC => Some(46),
        KeyCode::KeyD => Some(32),
        KeyCode::KeyE => Some(18),
        KeyCode::KeyF => Some(33),
        KeyCode::KeyG => Some(34),
        KeyCode::KeyH => Some(35),
        KeyCode::KeyI => Some(23),
        KeyCode::KeyJ => Some(36),
        KeyCode::KeyK => Some(37),
        KeyCode::KeyL => Some(38),
        KeyCode::KeyM => Some(50),
        KeyCode::KeyN => Some(49),
        KeyCode::KeyO => Some(24),
        KeyCode::KeyP => Some(25),
        KeyCode::KeyQ => Some(16),
        KeyCode::KeyR => Some(19),
        KeyCode::KeyS => Some(31),
        KeyCode::KeyT => Some(20),
        KeyCode::KeyU => Some(22),
        KeyCode::KeyV => Some(47),
        KeyCode::KeyW => Some(17),
        KeyCode::KeyX => Some(45),
        KeyCode::KeyY => Some(21),
        KeyCode::KeyZ => Some(44),
        KeyCode::Digit1 => Some(2),
        KeyCode::Digit2 => Some(3),
        KeyCode::Digit3 => Some(4),
        KeyCode::Digit4 => Some(5),
        KeyCode::Digit5 => Some(6),
        KeyCode::Digit6 => Some(7),
        KeyCode::Digit7 => Some(8),
        KeyCode::Digit8 => Some(9),
        KeyCode::Digit9 => Some(10),
        KeyCode::Digit0 => Some(11),
        KeyCode::Enter => Some(28),
        KeyCode::Escape => Some(1),
        KeyCode::Backspace => Some(14),
        KeyCode::Tab => Some(15),
        KeyCode::Space => Some(57),
        KeyCode::Minus => Some(12),
        KeyCode::Equal => Some(13),
        KeyCode::BracketLeft => Some(26),
        KeyCode::BracketRight => Some(27),
        KeyCode::Backslash => Some(43),
        KeyCode::Semicolon => Some(39),
        KeyCode::Quote => Some(40),
        KeyCode::Backquote => Some(41),
        KeyCode::Comma => Some(51),
        KeyCode::Period => Some(52),
        KeyCode::Slash => Some(53),
        KeyCode::CapsLock => Some(58),
        KeyCode::F1 => Some(59),
        KeyCode::F2 => Some(60),
        KeyCode::F3 => Some(61),
        KeyCode::F4 => Some(62),
        KeyCode::F5 => Some(63),
        KeyCode::F6 => Some(64),
        KeyCode::F7 => Some(65),
        KeyCode::F8 => Some(66),
        KeyCode::F9 => Some(67),
        KeyCode::F10 => Some(68),
        KeyCode::F11 => Some(87),
        KeyCode::F12 => Some(88),
        KeyCode::ShiftLeft => Some(42),
        KeyCode::ControlLeft => Some(29),
        KeyCode::AltLeft => Some(56),
        KeyCode::ShiftRight => Some(54),
        KeyCode::ControlRight => Some(97),
        KeyCode::NumpadDivide => Some(98),
        KeyCode::PrintScreen => Some(99),
        KeyCode::AltRight => Some(100),
        KeyCode::Home => Some(102),
        KeyCode::ArrowUp => Some(103),
        KeyCode::PageUp => Some(104),
        KeyCode::ArrowLeft => Some(105),
        KeyCode::ArrowRight => Some(106),
        KeyCode::End => Some(107),
        KeyCode::ArrowDown => Some(108),
        KeyCode::PageDown => Some(109),
        KeyCode::Insert => Some(110),
        KeyCode::Delete => Some(111),
        KeyCode::SuperLeft => Some(125),
        KeyCode::SuperRight => Some(126),
        _ => None,
    }
}

fn winit_button_to_evdev(btn: MouseButton) -> Option<u32> {
    match btn {
        MouseButton::Left => Some(272),
        MouseButton::Right => Some(273),
        MouseButton::Middle => Some(274),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Single,
    Handoff,
    Broker,
}

struct Args {
    backend: Backend,
    mode: Mode,
    width: i32,
    height: i32,
    login_app: Vec<String>,
    payload: Vec<String>,
    broker_socket: Option<PathBuf>,
    username: String,
    password: String,
    desktop_name: String,
    glx_vendor: Option<String>,
}

fn usage(program: &str) -> String {
    format!(
        "Usage: {program} --backend <kwin|gst> --mode <single|handoff|broker> [options] [-- payload...]\n\n\
         Options:\n\
         \x20 --backend <kwin|gst>        compositor backend (default: kwin)\n\
         \x20 --mode <single|handoff|broker>  (default: handoff)\n\
         \x20 --width <px>                (default: 1280)\n\
         \x20 --height <px>               (default: 720)\n\
         \x20 --login-app \"<cmd...>\"      Login-stage command for handoff/broker modes,\n\
         \x20                             space-separated (default: \"target/release/redfog-login\")\n\
         \x20 --broker-socket <path>      redfog-broker's Unix socket (required for --mode broker)\n\
         \x20 --username <user>           target user for --mode broker (required for --mode broker)\n\
         \x20 --password <pass>           password for --mode broker's Authenticate (default: empty)\n\
         \x20 --desktop-name <name>       XDG_SESSION_DESKTOP/XDG_CURRENT_DESKTOP for the gst backend's\n\
         \x20                             nested payload (default: sway)\n\
         \x20 --glx-vendor <vendor>       __GLX_VENDOR_LIBRARY_NAME for the gst backend's nested payload\n\
         \x20                             (default: unset — see session-backend::NestedSessionConfig)\n\
         \x20 -- <payload...>             the User-stage (or single-mode) command\n\
         \x20                             (default: \"plasmashell --no-respawn\" for kwin, \"sway\" for gst)\n\n\
         Env:\n\
         \x20 REDFOG_GST_WAYLAND_DISPLAY_PLUGIN_DIR  gst backend's plugin dir (see scripts/run-gst-viewer.sh)\n\
         \x20 REDFOG_GST_RENDER_NODE                 gst backend's render-node (default: software)\n\
         \x20 REDFOG_RUNTIME_DIR                     override the runtime dir (default: /tmp/redfog-runtime)"
    )
}

fn next_arg(argv: &[String], i: &mut usize, flag: &str) -> String {
    *i += 1;
    argv.get(*i).cloned().unwrap_or_else(|| {
        eprintln!("{flag} requires a value");
        std::process::exit(1);
    })
}

fn parse_args() -> Args {
    let argv: Vec<String> = std::env::args().collect();
    let mut backend = Backend::Kwin;
    let mut mode = Mode::Handoff;
    let mut width = 1280i32;
    let mut height = 720i32;
    let mut login_app = vec!["target/release/redfog-login".to_string()];
    let mut broker_socket: Option<PathBuf> = None;
    let mut username = String::new();
    let mut password = String::new();
    let mut desktop_name = "sway".to_string();
    let mut glx_vendor: Option<String> = None;
    let mut payload: Vec<String> = Vec::new();

    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--" => {
                payload = argv[i + 1..].to_vec();
                break;
            }
            "--backend" => {
                backend = match next_arg(&argv, &mut i, "--backend").as_str() {
                    "kwin" => Backend::Kwin,
                    "gst" => Backend::GstWaylandDisplay,
                    other => {
                        eprintln!("invalid --backend {other:?} (expected \"kwin\" or \"gst\")\n\n{}", usage(&argv[0]));
                        std::process::exit(1);
                    }
                };
            }
            "--mode" => {
                mode = match next_arg(&argv, &mut i, "--mode").as_str() {
                    "single" => Mode::Single,
                    "handoff" => Mode::Handoff,
                    "broker" => Mode::Broker,
                    other => {
                        eprintln!("invalid --mode {other:?} (expected \"single\", \"handoff\", or \"broker\")\n\n{}", usage(&argv[0]));
                        std::process::exit(1);
                    }
                };
            }
            "--width" => {
                let v = next_arg(&argv, &mut i, "--width");
                width = v.parse().unwrap_or_else(|_| {
                    eprintln!("invalid --width {v:?}");
                    std::process::exit(1);
                });
            }
            "--height" => {
                let v = next_arg(&argv, &mut i, "--height");
                height = v.parse().unwrap_or_else(|_| {
                    eprintln!("invalid --height {v:?}");
                    std::process::exit(1);
                });
            }
            "--login-app" => {
                login_app = next_arg(&argv, &mut i, "--login-app").split_whitespace().map(str::to_string).collect();
            }
            "--broker-socket" => broker_socket = Some(PathBuf::from(next_arg(&argv, &mut i, "--broker-socket"))),
            "--username" => username = next_arg(&argv, &mut i, "--username"),
            "--password" => password = next_arg(&argv, &mut i, "--password"),
            "--desktop-name" => desktop_name = next_arg(&argv, &mut i, "--desktop-name"),
            "--glx-vendor" => glx_vendor = Some(next_arg(&argv, &mut i, "--glx-vendor")),
            "--help" | "-h" => {
                println!("{}", usage(&argv[0]));
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument {other:?}\n\n{}", usage(&argv[0]));
                std::process::exit(1);
            }
        }
        i += 1;
    }

    if mode == Mode::Broker && (broker_socket.is_none() || username.is_empty()) {
        eprintln!("--mode broker requires --broker-socket and --username\n\n{}", usage(&argv[0]));
        std::process::exit(1);
    }

    Args { backend, mode, width, height, login_app, payload, broker_socket, username, password, desktop_name, glx_vendor }
}

fn default_payload(backend: Backend) -> Vec<String> {
    match backend {
        Backend::Kwin => vec!["plasmashell".to_string(), "--no-respawn".to_string()],
        Backend::GstWaylandDisplay => vec!["sway".to_string()],
    }
}

/// No-op for `SpawnedCompositor::Kwin`/`HeadlessLogin` (their payload is
/// already running by construction — see `SpawnedCompositor`'s doc
/// comment; the Login stage is always `HeadlessLogin` now regardless of
/// `--backend`, so this is a no-op for every Login-stage call). For
/// `GstWaylandDisplay`, blocks (via `runtime.block_on`) waiting for the
/// compositor's Wayland socket to appear and then spawns `command` as its
/// nested payload — directly, or via the broker if `via_broker` (only
/// meaningful for `Mode::Broker`'s User stage — see `session_backend::
/// spawn_gst_payload`'s doc comment).
fn spawn_gst_payload_for(
    runtime: &tokio::runtime::Runtime,
    compositor: &mut SpawnedCompositor,
    args: &Args,
    command: Vec<String>,
    via_broker: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let SpawnedCompositor::GstWaylandDisplay { runtime_dir, socket_path, socket_name, payload_process, .. } = compositor else {
        return Ok(());
    };
    let nested = NestedSessionConfig { command, desktop_name: args.desktop_name.clone(), glx_vendor: args.glx_vendor.clone() };
    let broker = if via_broker {
        let broker_socket_path = args.broker_socket.as_deref().expect("--mode broker requires --broker-socket, checked in parse_args");
        Some((broker_socket_path, "viewer-0".to_string(), args.username.clone()))
    } else {
        None
    };
    eprintln!("viewer: waiting for gst-wayland-display socket under {runtime_dir}...");
    let child = runtime
        .block_on(session_backend::spawn_gst_payload(runtime_dir, socket_path, socket_name, &nested, broker, Duration::from_secs(10)))
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    *payload_process = child;
    Ok(())
}

fn build_and_play_pipeline(
    compositor: &SpawnedCompositor,
    frame_store: Arc<Mutex<Option<Frame>>>,
    event_loop_proxy: winit::event_loop::EventLoopProxy<UserEvent>,
) -> Result<gst::Pipeline, Box<dyn std::error::Error>> {
    let client_name = format!("redfog-viewer-{}", std::process::id());
    let pipeline = redfog_core::make_pipeline(compositor.video_source(), &client_name, frame_store, move |changed| {
        if changed {
            let _ = event_loop_proxy.send_event(UserEvent::FrameSizeChanged);
        } else {
            let _ = event_loop_proxy.send_event(UserEvent::NewFrame);
        }
    });
    pipeline.set_state(gst::State::Playing)?;
    Ok(pipeline)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args();

    if args.backend == Backend::Kwin {
        // Must run before anything else touches D-Bus: re-execs the whole
        // process inside dbus-run-session on first launch.
        redfog_core::ensure_private_dbus_session();
    }

    // For Backend::GstWaylandDisplay: waylanddisplaysrc isn't installed
    // system-wide, so it won't be on GStreamer's default plugin search path.
    if let Ok(plugin_dir) = std::env::var("REDFOG_GST_WAYLAND_DISPLAY_PLUGIN_DIR") {
        let existing = std::env::var("GST_PLUGIN_PATH").unwrap_or_default();
        let combined = if existing.is_empty() { plugin_dir } else { format!("{plugin_dir}:{existing}") };
        std::env::set_var("GST_PLUGIN_PATH", combined);
    }
    gst::init()?;

    // Only Backend::Kwin's capture goes through PipeWire (kwin-capture's
    // screencast protocol) — Backend::GstWaylandDisplay hands frames
    // straight to the pipeline via its own GStreamer element, no PipeWire
    // involved at all.
    let _headless_runtime = if args.backend == Backend::Kwin {
        eprintln!("viewer: starting headless PipeWire runtime...");
        Some(redfog_core::HeadlessRuntime::start(redfog_core::default_runtime_dir()).map_err(|e| e as Box<dyn std::error::Error>)?)
    } else {
        None
    };

    // Only needed for the broker-touching async calls (Mode::Broker's User
    // spawn, and Backend::GstWaylandDisplay's payload spawn) — everything
    // else here is synchronous, matching the winit event loop it all runs
    // inside of.
    let runtime = tokio::runtime::Runtime::new()?;

    let payload = if args.payload.is_empty() { default_payload(args.backend) } else { args.payload.clone() };

    let (mut stage, mut compositor) = match args.mode {
        Mode::Single => {
            eprintln!("viewer: spawning compositor directly (single mode)...");
            (SessionType::User("user".to_string()), session_backend::spawn_user_compositor_direct(args.backend, "user", &payload, args.width as u32, args.height as u32, 60)?)
        }
        Mode::Handoff | Mode::Broker => {
            eprintln!("viewer: spawning Login compositor...");
            (SessionType::Login, session_backend::spawn_login_compositor(&args.login_app, args.width as u32, args.height as u32)?)
        }
    };

    let frame_store: Arc<Mutex<Option<Frame>>> = Arc::new(Mutex::new(None));
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build()?;
    let event_loop_proxy = event_loop.create_proxy();

    // Must come before the payload spawn below: for Backend::GstWaylandDisplay
    // the compositor's Wayland socket doesn't exist until its pipeline
    // actually reaches Playing (see spawn_gst_compositor's doc comment) —
    // spawn_gst_payload_for waits for that socket, so calling it first would
    // just hang until its own 10s timeout.
    let mut pipeline = build_and_play_pipeline(&compositor, frame_store.clone(), event_loop_proxy.clone())?;
    let initial_payload = if matches!(stage, SessionType::Login) { args.login_app.clone() } else { payload.clone() };
    // Login always spawns directly, never via the broker — it doesn't run
    // as any particular target user (see design.md's "Authentication: a
    // real graphical login screen"), matching production
    // (session_backend::spawn_gst_payload's callers).
    spawn_gst_payload_for(&runtime, &mut compositor, &args, initial_payload, false)?;
    let mut input_sink = compositor.input_sink()?;
    // `Option` so `CloseRequested`/handoff can `.take()` an owned value out
    // to call `terminate(self)` on, without needing a throwaway placeholder
    // compositor just to satisfy `mem::replace` — only ever `None` for the
    // instant between taking the old one and storing the new one.
    let mut compositor = Some(compositor);

    let window = Arc::new(
        WindowBuilder::new()
            .with_title("Redfog Viewer")
            .with_inner_size(winit::dpi::PhysicalSize::new(args.width as u32, args.height as u32))
            .build(&event_loop)?,
    );
    let context = softbuffer::Context::new(window.clone())?;
    let mut surface = softbuffer::Surface::new(&context, window.clone())?;

    let mut frame_w = 0u32;
    let mut frame_h = 0u32;
    let mut has_focus = true;
    let mut pending_resize: Option<(i32, i32, std::time::Instant)> = None;

    event_loop.run(move |event, elwt| {
        elwt.set_control_flow(ControlFlow::Wait);

        match event {
            Event::UserEvent(UserEvent::NewFrame) => {
                window.request_redraw();
            }
            Event::UserEvent(UserEvent::FrameSizeChanged) => {
                // Fires once unconditionally on the very first frame (the
                // frame-size-changed check in redfog_core::make_pipeline
                // compares against "no frame yet"), and again on a genuine
                // resolution change. Just resize the window to match —
                // rebuilding the pipeline isn't needed (videoconvert/appsink
                // adapt to new caps on their own) and would actively break
                // Backend::GstWaylandDisplay anyway: unlike KWin's
                // PipeWireNode (a fresh pipewiresrc element every rebuild),
                // its VideoSource::Element is the *same* gst::Element every
                // time, and GStreamer refuses to add an element to a second
                // bin while it's still parented to the first (confirmed
                // live: "Failed to add element").
                let size = {
                    let store = frame_store.lock().unwrap();
                    store.as_ref().map(|f| (f.width, f.height))
                };
                let Some((w, h)) = size else { return };
                let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(w, h));
                window.request_redraw();
            }
            Event::WindowEvent { event: WindowEvent::RedrawRequested, .. } => {
                let frame_opt = {
                    let store = frame_store.lock().unwrap();
                    store.as_ref().map(|f| Frame { width: f.width, height: f.height, data: f.data.clone() })
                };
                if let Some(frame) = frame_opt {
                    if frame.width != frame_w || frame.height != frame_h {
                        frame_w = frame.width;
                        frame_h = frame.height;
                        let _ = surface.resize(NonZeroU32::new(frame_w).unwrap(), NonZeroU32::new(frame_h).unwrap());
                    }
                    if let Ok(mut buffer) = surface.buffer_mut() {
                        let pixels = unsafe { std::slice::from_raw_parts(frame.data.as_ptr() as *const u32, frame.data.len() / 4) };
                        buffer.copy_from_slice(pixels);
                        let _ = buffer.present();
                    }
                }
            }
            Event::WindowEvent { event: WindowEvent::Resized(size), .. } => {
                // Debounced — dragging a window edge fires many Resized
                // events per second; only act once the user has settled on
                // a size. compositor.resize() itself is a documented no-op
                // for backends that can't support it (see its doc comment),
                // so this handler doesn't need to know which backend it's
                // running against at all.
                if size.width > 0 && size.height > 0 {
                    pending_resize = Some((size.width as i32, size.height as i32, std::time::Instant::now()));
                    elwt.set_control_flow(ControlFlow::WaitUntil(std::time::Instant::now() + std::time::Duration::from_millis(200)));
                }
            }
            Event::AboutToWait => {
                if let Some((w, h, t)) = pending_resize {
                    if t.elapsed() >= std::time::Duration::from_millis(200) {
                        pending_resize = None;
                        if compositor.as_ref().unwrap().resize(w, h) {
                            eprintln!("viewer: resizing compositor to {w}x{h}...");
                            let _ = pipeline.set_state(gst::State::Null);
                            match build_and_play_pipeline(compositor.as_ref().unwrap(), frame_store.clone(), event_loop_proxy.clone()) {
                                Ok(p) => pipeline = p,
                                Err(e) => eprintln!("viewer: failed to rebuild pipeline after resize: {e}"),
                            }
                        }
                    } else {
                        elwt.set_control_flow(ControlFlow::WaitUntil(t + std::time::Duration::from_millis(200)));
                    }
                }

                // Only Mode::Handoff/Mode::Broker's Login stage is watched
                // for exit — Mode::Single never has one.
                if !matches!(stage, SessionType::Login) {
                    return;
                }
                let Ok(Some(status)) = compositor.as_mut().unwrap().try_wait() else { return };
                if !status.success() {
                    eprintln!("viewer: Login compositor exited unexpectedly ({status:?}), giving up");
                    elwt.exit();
                    return;
                }
                eprintln!("viewer: Login compositor exited successfully, spawning User compositor...");

                // Build the new compositor before touching the old one/the
                // pipeline — on failure below, this leaves everything as it
                // was (already-exited Login compositor, still-Null-able
                // pipeline) rather than half torn down.
                let spawn_result = match args.mode {
                    Mode::Handoff => session_backend::spawn_user_compositor_direct(args.backend, "user", &payload, args.width as u32, args.height as u32, 60),
                    Mode::Broker => runtime.block_on(session_backend::spawn_user_compositor_via_broker(
                        args.backend,
                        args.broker_socket.as_deref().expect("--mode broker requires --broker-socket, checked in parse_args"),
                        "viewer-0".to_string(),
                        &args.username,
                        &args.password,
                        false,
                        &payload,
                        args.width as u32,
                        args.height as u32,
                        60,
                    )),
                    Mode::Single => unreachable!("Mode::Single has no Login stage to hand off from"),
                };
                let mut new_compositor = match spawn_result {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("viewer: failed to spawn User compositor: {e}");
                        elwt.exit();
                        return;
                    }
                };
                // Must come before the payload spawn below — see the
                // matching comment on the initial spawn earlier in main().
                let new_pipeline = match build_and_play_pipeline(&new_compositor, frame_store.clone(), event_loop_proxy.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("viewer: failed to build User-stage pipeline: {e}");
                        elwt.exit();
                        return;
                    }
                };
                if let Err(e) = spawn_gst_payload_for(&runtime, &mut new_compositor, &args, payload.clone(), args.mode == Mode::Broker) {
                    eprintln!("viewer: failed to spawn User-stage payload: {e}");
                    let _ = new_pipeline.set_state(gst::State::Null);
                    elwt.exit();
                    return;
                }
                input_sink = match new_compositor.input_sink() {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("viewer: failed to connect input sink to User compositor: {e}");
                        let _ = new_pipeline.set_state(gst::State::Null);
                        elwt.exit();
                        return;
                    }
                };

                // Everything about the new compositor/pipeline succeeded —
                // only now tear down the old (already-exited) Login one.
                let _ = pipeline.set_state(gst::State::Null);
                compositor.take().unwrap().terminate();

                pipeline = new_pipeline;
                compositor = Some(new_compositor);
                stage = SessionType::User("user".to_string());
                eprintln!("viewer: handoff to User compositor complete");
            }
            Event::WindowEvent { event: WindowEvent::Focused(focused), .. } => {
                has_focus = focused;
            }
            Event::WindowEvent { event: WindowEvent::CursorMoved { position, .. }, .. } => {
                if has_focus && frame_w > 0 && frame_h > 0 {
                    let rx = position.x.clamp(0.0, frame_w as f64 - 1.0);
                    let ry = position.y.clamp(0.0, frame_h as f64 - 1.0);
                    input_sink.pointer_motion_absolute(rx, ry);
                    input_sink.flush();
                }
            }
            Event::WindowEvent { event: WindowEvent::MouseInput { state, button, .. }, .. } => {
                if has_focus {
                    if let Some(evdev_btn) = winit_button_to_evdev(button) {
                        input_sink.button(evdev_btn, state == ElementState::Pressed);
                        input_sink.flush();
                    }
                }
            }
            Event::WindowEvent { event: WindowEvent::KeyboardInput { event: key_event, .. }, .. } => {
                if has_focus {
                    if let PhysicalKey::Code(winit_key) = key_event.physical_key {
                        if let Some(evdev_key) = winit_key_to_evdev(winit_key) {
                            input_sink.keyboard_key(evdev_key, key_event.state == ElementState::Pressed);
                            input_sink.flush();
                        }
                    }
                }
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                let _ = pipeline.set_state(gst::State::Null);
                if let Some(c) = compositor.take() {
                    c.terminate();
                }
                elwt.exit();
            }
            _ => {}
        }
    })?;

    Ok(())
}
