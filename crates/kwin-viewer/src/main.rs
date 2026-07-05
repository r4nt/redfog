use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};

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
    fn event(
        _state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(WaylandState: ignore wl_seat::WlSeat);
delegate_noop!(WaylandState: ignore OrgKdeKwinFakeInput);

#[derive(Debug)]
enum UserEvent {
    NewFrame,
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <pipewire-node-id> <headless-wayland-socket-path> [width height]", args[0]);
        std::process::exit(1);
    }
    let node_id: u32 = args[1].parse().expect("Invalid PipeWire node ID");
    let headless_wayland_path = &args[2];
    let preview_w: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(960);
    let preview_h: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(540);

    // Initialize GStreamer
    gst::init()?;

    // Connect to Wayland and bind fake_input
    use std::os::unix::net::UnixStream;
    let stream = UnixStream::connect(headless_wayland_path)?;
    let conn = Connection::from_socket(stream)?;
    let (globals, mut queue) = registry_queue_init::<WaylandState>(&conn)?;
    let qh = queue.handle();

    let fake_input: OrgKdeKwinFakeInput = globals
        .bind(&qh, 4..=6, ())
        .map_err(|e| format!("org_kde_kwin_fake_input not available: {e}"))?;

    let mut wayland_state = WaylandState;
    fake_input.authenticate(
        "redfog-viewer".to_string(),
        "input forwarding for game streaming".to_string(),
    );
    conn.flush()?;
    queue.roundtrip(&mut wayland_state)?;

    eprintln!("kwin-viewer: connected to Wayland & fake_input authenticated");

    // Build the Event Loop
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build()?;
    let window = Arc::new(
        WindowBuilder::new()
            .with_title("Redfog KWin Stream Viewer")
            .with_inner_size(winit::dpi::PhysicalSize::new(preview_w, preview_h))
            .build(&event_loop)?,
    );

    // Setup softbuffer context & surface
    let context = softbuffer::Context::new(window.clone())?;
    let mut surface = softbuffer::Surface::new(&context, window.clone())?;

    // Frame storage
    let frame_store = Arc::new(Mutex::new(None));
    let frame_store_clone = frame_store.clone();
    let event_loop_proxy = event_loop.create_proxy();

    // KWin's virtual output is already at preview_w x preview_h — no scaling needed.
    let pipeline_desc = format!(
        "pipewiresrc path={node_id} do-timestamp=true \
         ! videoconvert \
         ! video/x-raw,format=BGRx \
         ! appsink name=sink sync=false"
    );
    let pipeline = gst::parse_launch(&pipeline_desc)?;
    let appsink = pipeline
        .clone()
        .dynamic_cast::<gst::Pipeline>()
        .unwrap()
        .by_name("sink")
        .unwrap()
        .dynamic_cast::<gst_app::AppSink>()
        .unwrap();

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                let structure = caps.structure(0).ok_or(gst::FlowError::Error)?;
                let width = structure.get::<i32>("width").map_err(|_| gst::FlowError::Error)? as u32;
                let height = structure.get::<i32>("height").map_err(|_| gst::FlowError::Error)? as u32;

                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let data = map.to_vec();

                let mut store = frame_store_clone.lock().unwrap();
                *store = Some(Frame { width, height, data });

                let _ = event_loop_proxy.send_event(UserEvent::NewFrame);

                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.set_state(gst::State::Playing)?;

    let mut current_width = 0;
    let mut current_height = 0;
    let mut has_focus = true;

    event_loop.run(move |event, elwt| {
        elwt.set_control_flow(ControlFlow::Wait);

        match event {
            Event::UserEvent(UserEvent::NewFrame) => {
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
                    if current_width != frame.width || current_height != frame.height {
                        current_width = frame.width;
                        current_height = frame.height;
                        let _ = surface.resize(
                            NonZeroU32::new(current_width).unwrap(),
                            NonZeroU32::new(current_height).unwrap(),
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
                    current_width = size.width;
                    current_height = size.height;
                    let _ = surface.resize(
                        NonZeroU32::new(current_width).unwrap(),
                        NonZeroU32::new(current_height).unwrap(),
                    );
                }
            }
            Event::WindowEvent { event: WindowEvent::Focused(focused), .. } => {
                has_focus = focused;
            }
            Event::WindowEvent { event: WindowEvent::CursorLeft { .. }, .. } => {}
            Event::WindowEvent { event: WindowEvent::CursorMoved { position, .. }, .. } => {
                if has_focus {
                    let w_size = window.inner_size();
                    if w_size.width > 0 && w_size.height > 0 && current_width > 0 && current_height > 0 {
                        let rx = (position.x / w_size.width as f64) * current_width as f64;
                        let ry = (position.y / w_size.height as f64) * current_height as f64;
                        fake_input.pointer_motion_absolute(rx, ry);
                        let _ = conn.flush();
                    }
                }
            }
            Event::WindowEvent { event: WindowEvent::MouseInput { state, button, .. }, .. } => {
                if has_focus {
                    if let Some(evdev_btn) = winit_button_to_evdev(button) {
                        fake_input.button(
                            evdev_btn,
                            if state == ElementState::Pressed { 1 } else { 0 },
                        );
                        let _ = conn.flush();
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
                            let _ = conn.flush();
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
