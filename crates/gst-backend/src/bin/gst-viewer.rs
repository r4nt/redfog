//! Standalone local prover for the gst-wayland-display backend — the
//! equivalent of `kwin-viewer`'s role for the KWin backend: spawns the
//! compositor, renders it in a local window, forwards local mouse/keyboard
//! into it, so the backend can be validated end-to-end without any broker
//! or Moonlight client involved. No Login->User handoff (that's a
//! redfog-moonlight/broker concern, not this backend's) — just one nested
//! session for the session's lifetime.
//!
//! Usage: gst-viewer <width> <height> [nested-command...]
//! Defaults to `sway` if no nested command is given. Requires
//! REDFOG_GST_WAYLAND_DISPLAY_PLUGIN_DIR to point at gst-wayland-display's
//! built `gstreamer-1.0` plugin directory (see its README's `cargo cinstall`
//! step) unless it's already on GST_PLUGIN_PATH/installed system-wide.
//!
//! REDFOG_GST_GLX_VENDOR: sets __GLX_VENDOR_LIBRARY_NAME for the nested
//! session (e.g. "nvidia") — needed on machines with more than one GLVND
//! EGL vendor file installed, where auto-detection can otherwise silently
//! pick Mesa's software rasterizer for nested GLX apps even though a real
//! GPU is available (confirmed live via glxgears + nvidia-smi). Unset by
//! default since it's not universal — see gst_backend::NestedSessionConfig.

use std::num::NonZeroU32;
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

use redfog_core::{Frame, InputSink, VideoSource};

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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <width> <height> [nested-command...]", args[0]);
        std::process::exit(1);
    }
    let width: i32 = args[1].parse().expect("invalid width");
    let height: i32 = args[2].parse().expect("invalid height");
    let nested_command: Vec<String> = if args.len() > 3 { args[3..].to_vec() } else { vec!["sway".to_string()] };

    if let Ok(plugin_dir) = std::env::var("REDFOG_GST_WAYLAND_DISPLAY_PLUGIN_DIR") {
        let existing = std::env::var("GST_PLUGIN_PATH").unwrap_or_default();
        let combined = if existing.is_empty() { plugin_dir } else { format!("{plugin_dir}:{existing}") };
        std::env::set_var("GST_PLUGIN_PATH", combined);
    }
    gst::init()?;

    let render_node = std::env::var("REDFOG_GST_RENDER_NODE").unwrap_or_else(|_| gst_backend::RENDER_NODE_SOFTWARE.to_string());
    eprintln!("gst-viewer: building waylanddisplaysrc (render-node={render_node})...");
    let src = gst_backend::make_source_element(&render_node, width, height)?;

    let frame_store: Arc<Mutex<Option<Frame>>> = Arc::new(Mutex::new(None));
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build()?;
    let event_loop_proxy = event_loop.create_proxy();

    // waylanddisplaysrc reads XDG_RUNTIME_DIR when its Smithay compositor
    // starts up, which happens synchronously inside `set_state(Playing)`
    // below — confirmed live: setting this after that call is too late
    // (`RuntimeDirNotSet` panic deep in wayland-display-core).
    let runtime_dir = redfog_core::default_runtime_dir();
    std::fs::create_dir_all(&runtime_dir)?;
    std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir);
    let socket_name = "wayland-1";

    let proxy_clone = event_loop_proxy.clone();
    let pipeline = redfog_core::make_pipeline(VideoSource::Element(src.clone()), frame_store.clone(), move |changed| {
        if changed {
            let _ = proxy_clone.send_event(UserEvent::FrameSizeChanged);
        } else {
            let _ = proxy_clone.send_event(UserEvent::NewFrame);
        }
    });
    pipeline.set_state(gst::State::Playing)?;

    // Its Wayland socket doesn't exist until the above actually takes
    // effect — the nested session can only be spawned after this.
    eprintln!("gst-viewer: waiting for Wayland socket under {runtime_dir}...");
    gst_backend::wait_for_wayland_socket(&runtime_dir, socket_name, Duration::from_secs(10))?;

    eprintln!("gst-viewer: spawning nested session {nested_command:?}...");
    let glx_vendor = std::env::var("REDFOG_GST_GLX_VENDOR").ok();
    let mut nested_child = gst_backend::spawn_nested_session(
        &gst_backend::NestedSessionConfig { command: nested_command, desktop_name: "sway".to_string(), glx_vendor },
        &runtime_dir,
        socket_name,
    )?;

    let mut input_sink = gst_backend::GstInputSink::new(src);

    let window = Arc::new(
        WindowBuilder::new()
            .with_title("Redfog gst-wayland-display Viewer")
            .with_inner_size(winit::dpi::PhysicalSize::new(width as u32, height as u32))
            .build(&event_loop)?,
    );
    let context = softbuffer::Context::new(window.clone())?;
    let mut surface = softbuffer::Surface::new(&context, window.clone())?;

    let mut frame_w = 0u32;
    let mut frame_h = 0u32;
    let mut has_focus = true;

    event_loop.run(move |event, elwt| {
        elwt.set_control_flow(ControlFlow::Wait);

        match event {
            Event::UserEvent(UserEvent::NewFrame) => {
                window.request_redraw();
            }
            Event::UserEvent(UserEvent::FrameSizeChanged) => {
                let store = frame_store.lock().unwrap();
                if let Some(f) = store.as_ref() {
                    let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(f.width, f.height));
                }
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
                        let pixels = unsafe {
                            std::slice::from_raw_parts(frame.data.as_ptr() as *const u32, frame.data.len() / 4)
                        };
                        buffer.copy_from_slice(pixels);
                        let _ = buffer.present();
                    }
                }
            }
            Event::WindowEvent { event: WindowEvent::Focused(focused), .. } => {
                has_focus = focused;
            }
            Event::WindowEvent { event: WindowEvent::CursorMoved { position, .. }, .. } => {
                if has_focus && frame_w > 0 && frame_h > 0 {
                    let rx = position.x.clamp(0.0, frame_w as f64 - 1.0);
                    let ry = position.y.clamp(0.0, frame_h as f64 - 1.0);
                    input_sink.pointer_motion_absolute(rx, ry);
                }
            }
            Event::WindowEvent { event: WindowEvent::MouseInput { state, button, .. }, .. } => {
                if has_focus {
                    if let Some(evdev_btn) = winit_button_to_evdev(button) {
                        input_sink.button(evdev_btn, state == ElementState::Pressed);
                    }
                }
            }
            Event::WindowEvent { event: WindowEvent::KeyboardInput { event: key_event, .. }, .. } => {
                if has_focus {
                    if let PhysicalKey::Code(winit_key) = key_event.physical_key {
                        if let Some(evdev_key) = winit_key_to_evdev(winit_key) {
                            input_sink.keyboard_key(evdev_key, key_event.state == ElementState::Pressed);
                        }
                    }
                }
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                let _ = nested_child.kill();
                let _ = nested_child.wait();
                elwt.exit();
            }
            _ => {}
        }
    })?;

    Ok(())
}
