use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO);
        let ms = now.as_millis() % 1000;
        let secs = now.as_secs();
        let h = (secs / 3600) % 24;
        let m = (secs / 60) % 60;
        let s = secs % 60;
        std::eprintln!("[{:02}:{:02}:{:02}.{:03}] {}", h, m, s, ms, format!($($arg)*));
    }};
}

use gstreamer as gst;
use gstreamer_app as gst_app;
use gst::prelude::*;

use wayland_client::{
    delegate_noop,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_registry, wl_seat},
    Connection, Dispatch, QueueHandle,
};

use winit::{
    event::{ElementState, Event, WindowEvent, MouseButton},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
    keyboard::{KeyCode, PhysicalKey},
};

use kwin_capture::CaptureSession;

mod fake_input {
    #![allow(
        dead_code, non_camel_case_types, unused_unsafe, unused_variables,
        non_upper_case_globals, non_snake_case, unused_imports, missing_docs,
        clippy::all
    )]
    pub mod client {
        use wayland_client;
        use wayland_client::protocol::*;
        use wayland_backend;
        pub mod __interfaces {
            use wayland_client::protocol::__interfaces::*;
            use wayland_backend;
            wayland_scanner::generate_interfaces!("protocols/fake-input.xml");
        }
        use self::__interfaces::*;
        wayland_scanner::generate_client_code!("protocols/fake-input.xml");
    }
}

use fake_input::client::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;

struct WaylandState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandState {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event,
             _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_noop!(WaylandState: ignore wl_seat::WlSeat);
delegate_noop!(WaylandState: ignore OrgKdeKwinFakeInput);

#[derive(Debug)]
enum UserEvent {
    NewFrame,
    FrameSizeChanged,
}

struct Frame {
    width: u32,
    height: u32,
    data: Vec<u8>,
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
        KeyCode::Escape => Some(1),
        KeyCode::Minus => Some(12),
        KeyCode::Equal => Some(13),
        KeyCode::Backspace => Some(14),
        KeyCode::Tab => Some(15),
        KeyCode::BracketLeft => Some(26),
        KeyCode::BracketRight => Some(27),
        KeyCode::Enter => Some(28),
        KeyCode::ControlLeft => Some(29),
        KeyCode::Semicolon => Some(39),
        KeyCode::Quote => Some(40),
        KeyCode::Backquote => Some(41),
        KeyCode::ShiftLeft => Some(42),
        KeyCode::Backslash => Some(43),
        KeyCode::Comma => Some(51),
        KeyCode::Period => Some(52),
        KeyCode::Slash => Some(53),
        KeyCode::ShiftRight => Some(54),
        KeyCode::NumpadMultiply => Some(55),
        KeyCode::AltLeft => Some(56),
        KeyCode::Space => Some(57),
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
        KeyCode::NumLock => Some(69),
        KeyCode::ScrollLock => Some(70),
        KeyCode::Numpad7 => Some(71),
        KeyCode::Numpad8 => Some(72),
        KeyCode::Numpad9 => Some(73),
        KeyCode::NumpadSubtract => Some(74),
        KeyCode::Numpad4 => Some(75),
        KeyCode::Numpad5 => Some(76),
        KeyCode::Numpad6 => Some(77),
        KeyCode::NumpadAdd => Some(78),
        KeyCode::Numpad1 => Some(79),
        KeyCode::Numpad2 => Some(80),
        KeyCode::Numpad3 => Some(81),
        KeyCode::Numpad0 => Some(82),
        KeyCode::NumpadDecimal => Some(83),
        KeyCode::F11 => Some(87),
        KeyCode::F12 => Some(88),
        KeyCode::NumpadEnter => Some(96),
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

fn make_pipeline(
    node_id: u32,
    frame_store: Arc<Mutex<Option<Frame>>>,
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
) -> gst::Pipeline {
    let desc = format!(
        "pipewiresrc path={node_id} do-timestamp=true \
         ! videoconvert \
         ! video/x-raw,format=BGRx \
         ! appsink name=sink sync=false"
    );
    let pipeline = gst::parse_launch(&desc)
        .expect("pipeline parse failed")
        .dynamic_cast::<gst::Pipeline>()
        .unwrap();
    let appsink = pipeline
        .by_name("sink").unwrap()
        .dynamic_cast::<gst_app::AppSink>().unwrap();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                let s = caps.structure(0).ok_or(gst::FlowError::Error)?;
                let w = s.get::<i32>("width").map_err(|_| gst::FlowError::Error)? as u32;
                let h = s.get::<i32>("height").map_err(|_| gst::FlowError::Error)? as u32;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let data = map.to_vec();
                let mut store = frame_store.lock().unwrap();
                let changed = store.as_ref().map(|f: &Frame| f.width != w || f.height != h).unwrap_or(true);
                if changed {
                    eprintln!("kwin-viewer: frame size {}x{}", w, h);
                }
                *store = Some(Frame { width: w, height: h, data });
                if changed {
                    let _ = proxy.send_event(UserEvent::FrameSizeChanged);
                } else {
                    let _ = proxy.send_event(UserEvent::NewFrame);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipeline
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("Usage: {} <headless-wayland-socket> <width> <height>", args[0]);
        std::process::exit(1);
    }
    let socket_path = std::path::Path::new(&args[1]);
    let mut width: i32  = args[2].parse().expect("invalid width");
    let mut height: i32 = args[3].parse().expect("invalid height");
    // Align to 32px grid for encoder efficiency and mode reuse
    width = ((width + 16) / 32) * 32;
    height = ((height + 16) / 32) * 32;

    let scale: f64  = std::env::var("REDFOG_SCALE").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(1.0);

    // Create virtual output and PipeWire stream via the capture library.
    let session = CaptureSession::connect(socket_path, "redfog-output", width, height, scale)
        .expect("failed to create capture session");
    let node_id = session.node_id();
    eprintln!("kwin-viewer: PipeWire node {node_id}");

    // Separate connection to headless KWin for fake_input (input injection).
    use std::os::unix::net::UnixStream;
    let fi_stream = UnixStream::connect(socket_path)?;
    let fi_conn = Connection::from_socket(fi_stream)?;
    let (globals, mut fi_queue) = registry_queue_init::<WaylandState>(&fi_conn)?;
    let fi_qh = fi_queue.handle();

    let fake_input: OrgKdeKwinFakeInput = globals
        .bind(&fi_qh, 4..=6, ())
        .map_err(|e| format!("org_kde_kwin_fake_input not available: {e}"))?;

    let mut fi_state = WaylandState;
    fake_input.authenticate(
        "redfog-viewer".to_string(),
        "input forwarding for game streaming".to_string(),
    );
    fi_conn.flush()?;
    fi_queue.roundtrip(&mut fi_state)?;
    eprintln!("kwin-viewer: fake_input authenticated");

    // Initialize GStreamer
    gst::init()?;

    let frame_store: Arc<Mutex<Option<Frame>>> = Arc::new(Mutex::new(None));
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build()?;
    let event_loop_proxy = event_loop.create_proxy();

    let mut pipeline = make_pipeline(node_id, frame_store.clone(), event_loop_proxy.clone());
    pipeline.set_state(gst::State::Playing)?;

    // Signal readiness to proto.sh (read from stdout FIFO).
    println!("ready");

    let window = Arc::new(
        WindowBuilder::new()
            .with_title("Redfog KWin Stream Viewer")
            .with_inner_size(winit::dpi::PhysicalSize::new(width as u32, height as u32))
            .build(&event_loop)?,
    );
    let context = softbuffer::Context::new(window.clone())?;
    let mut surface = softbuffer::Surface::new(&context, window.clone())?;

    let mut frame_w = 0u32;
    let mut frame_h = 0u32;
    let mut has_focus = true;
    let mut pending_resize: Option<(i32, i32, std::time::Instant)> = None;
    let mut last_pipeline_restart = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(1))
        .unwrap_or_else(std::time::Instant::now);

    event_loop.run(move |event, elwt| {
        elwt.set_control_flow(ControlFlow::Wait);

        match event {
            Event::UserEvent(UserEvent::NewFrame) => {
                window.request_redraw();
            }
            Event::UserEvent(UserEvent::FrameSizeChanged) => {
                // Snap window to whatever size KDE settled on, read directly from
                // frame_store to avoid the RedrawRequested → Resized feedback loop.
                let mut size = (0, 0);
                {
                    let store = frame_store.lock().unwrap();
                    if let Some(f) = store.as_ref() {
                        size = (f.width, f.height);
                        let _ = window.request_inner_size(
                            winit::dpi::PhysicalSize::new(f.width, f.height)
                        );
                    }
                }
                eprintln!("kwin-viewer: FrameSizeChanged event received (new size: {}x{})", size.0, size.1);
                // Restart the pipeline so GStreamer renegotiates a fresh buffer pool
                // with PipeWire at the new resolution; without this, old-size buffers
                // block PipeWire from delivering new-size frames.
                // Cooldown prevents a double-restart race when the output settles
                // through two intermediate sizes (e.g., stale kscreen → correct size).
                if last_pipeline_restart.elapsed() >= std::time::Duration::from_millis(500) {
                    eprintln!("kwin-viewer: restarting GStreamer pipeline for new resolution...");
                    last_pipeline_restart = std::time::Instant::now();
                    pipeline.set_state(gst::State::Null).ok();
                    pipeline = make_pipeline(node_id, frame_store.clone(), event_loop_proxy.clone());
                    pipeline.set_state(gst::State::Playing).ok();
                    eprintln!("kwin-viewer: pipeline restarted and set to PLAYING");
                } else {
                    eprintln!("kwin-viewer: pipeline restart throttled (cooldown)");
                }
                window.request_redraw();
            }
            Event::WindowEvent { event: WindowEvent::RedrawRequested, .. } => {
                let frame_opt = {
                    let store = frame_store.lock().unwrap();
                    store.as_ref().map(|f| Frame {
                        width: f.width,
                        height: f.height,
                        data: f.data.clone(),
                    })
                };
                if let Some(frame) = frame_opt {
                    if frame.width != frame_w || frame.height != frame_h {
                        frame_w = frame.width;
                        frame_h = frame.height;
                        let _ = surface.resize(
                            NonZeroU32::new(frame_w).unwrap(),
                            NonZeroU32::new(frame_h).unwrap(),
                        );
                    }
                    if let Ok(mut buffer) = surface.buffer_mut() {
                        let pixels = unsafe {
                            std::slice::from_raw_parts(
                                frame.data.as_ptr() as *const u32,
                                frame.data.len() / 4,
                            )
                        };
                        buffer.copy_from_slice(pixels);
                        let _ = buffer.present();
                    }
                }
            }
            Event::WindowEvent { event: WindowEvent::Resized(size), .. } => {
                if size.width > 0 && size.height > 0 {
                    let w = size.width as i32;
                    let h = size.height as i32;
                    let already_matching = {
                        let store = frame_store.lock().unwrap();
                        store.as_ref().map(|f| f.width == size.width && f.height == size.height).unwrap_or(false)
                    };
                    if !already_matching {
                        eprintln!("kwin-viewer: window Resized {}x{} → queuing resize {}x{}", size.width, size.height, w, h);
                        pending_resize = Some((w, h, std::time::Instant::now()));
                        elwt.set_control_flow(ControlFlow::WaitUntil(
                            std::time::Instant::now() + std::time::Duration::from_millis(50),
                        ));
                    } else {
                        eprintln!("kwin-viewer: window Resized {}x{} matches frame size, ignoring", size.width, size.height);
                    }
                }
            }
            Event::AboutToWait => {
                // Debounce window resize.
                if let Some((w, h, t)) = pending_resize {
                    if t.elapsed() >= std::time::Duration::from_millis(200) {
                        let snapped_w = ((w + 16) / 32) * 32;
                        let snapped_h = ((h + 16) / 32) * 32;
                        eprintln!("kwin-viewer: debounce fired — resizing to {}x{} (snapped from {}x{})", snapped_w, snapped_h, w, h);
                        session.resize(snapped_w, snapped_h);
                        
                        // Restart pipeline immediately since KWin has applied the change
                        eprintln!("kwin-viewer: KWin resize complete. Restarting GStreamer pipeline...");
                        last_pipeline_restart = std::time::Instant::now();
                        pipeline.set_state(gst::State::Null).ok();
                        pipeline = make_pipeline(node_id, frame_store.clone(), event_loop_proxy.clone());
                        pipeline.set_state(gst::State::Playing).ok();
                        eprintln!("kwin-viewer: pipeline restarted and set to PLAYING");
                        
                        pending_resize = None;
                    }
                    elwt.set_control_flow(ControlFlow::WaitUntil(
                        std::time::Instant::now() + std::time::Duration::from_millis(50),
                    ));
                }
            }
            Event::WindowEvent { event: WindowEvent::Focused(focused), .. } => {
                has_focus = focused;
            }
            Event::WindowEvent { event: WindowEvent::CursorLeft { .. }, .. } => {}
            Event::WindowEvent { event: WindowEvent::CursorMoved { position, .. }, .. } => {
                if has_focus && frame_w > 0 && frame_h > 0 {
                    // Clamp to frame bounds — window may briefly be larger than the
                    // frame if request_inner_size hasn't been honored by the compositor yet.
                    let rx = position.x.clamp(0.0, frame_w as f64 - 1.0);
                    let ry = position.y.clamp(0.0, frame_h as f64 - 1.0);
                    fake_input.pointer_motion_absolute(rx, ry);
                    let _ = fi_conn.flush();
                }
            }
            Event::WindowEvent { event: WindowEvent::MouseInput { state, button, .. }, .. } => {
                if has_focus {
                    if let Some(evdev_btn) = winit_button_to_evdev(button) {
                        fake_input.button(
                            evdev_btn,
                            if state == ElementState::Pressed { 1 } else { 0 },
                        );
                        let _ = fi_conn.flush();
                    }
                }
            }
            Event::WindowEvent { event: WindowEvent::KeyboardInput { event: key_event, .. }, .. } => {
                if has_focus {
                    if let PhysicalKey::Code(winit_key) = key_event.physical_key {
                        if let Some(evdev_key) = winit_key_to_evdev(winit_key) {
                            fake_input.keyboard_key(
                                evdev_key,
                                if key_event.state == ElementState::Pressed { 1 } else { 0 },
                            );
                            let _ = fi_conn.flush();
                        }
                    }
                }
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                elwt.exit();
            }
            _ => {}
        }
    })?;

    Ok(())
}
