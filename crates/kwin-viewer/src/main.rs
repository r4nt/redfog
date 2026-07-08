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
use gst::prelude::*;
use winit::{
    event::{ElementState, Event, WindowEvent, MouseButton},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
    keyboard::{KeyCode, PhysicalKey},
};

use redfog_core::{CompositorSession, SessionType, StreamingEngine, Frame};

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
    // Must run before anything else touches D-Bus: re-execs the whole
    // process inside dbus-run-session on first launch.
    redfog_core::ensure_private_dbus_session();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <width> <height> [user-payload-command]", args[0]);
        std::process::exit(1);
    }
    let mut width: i32  = args[1].parse().expect("invalid width");
    let mut height: i32 = args[2].parse().expect("invalid height");
    let user_app = args.get(3).map(|s| s.as_str()).unwrap_or("plasmashell");

    // Align to 32px grid
    width = ((width + 16) / 32) * 32;
    height = ((height + 16) / 32) * 32;

    let scale: f64  = std::env::var("REDFOG_SCALE").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(1.0);

    // Initialize GStreamer
    gst::init()?;

    // Bring up PipeWire + wireplumber on an isolated runtime dir; exports
    // PIPEWIRE_REMOTE for CompositorSession::spawn to pick up. Kept alive
    // for the process lifetime and torn down on drop.
    eprintln!("kwin-viewer: starting headless PipeWire runtime...");
    let _headless_runtime = redfog_core::HeadlessRuntime::start(redfog_core::default_runtime_dir())
        .map_err(|e| e as Box<dyn std::error::Error>)?;

    // 1. Spawn Login Compositor
    eprintln!("kwin-viewer: spawning Login KWin compositor...");
    let login_session = CompositorSession::spawn(
        SessionType::Login,
        "redfog-login-0",
        width,
        height,
        scale,
        &["target/release/redfog-login".to_string()],
    ).map_err(|e| e as Box<dyn std::error::Error>)?;

    // 3. Initialize Streaming Engine connected to Login session
    let frame_store: Arc<Mutex<Option<Frame>>> = Arc::new(Mutex::new(None));
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build()?;
    let event_loop_proxy = event_loop.create_proxy();

    let proxy_clone = event_loop_proxy.clone();
    let mut engine = StreamingEngine::new(&login_session, frame_store.clone(), move |changed| {
        if changed {
            let _ = proxy_clone.send_event(UserEvent::FrameSizeChanged);
        } else {
            let _ = proxy_clone.send_event(UserEvent::NewFrame);
        }
    }).map_err(|e| e as Box<dyn std::error::Error>)?;

    let window = Arc::new(
        WindowBuilder::new()
            .with_title("Redfog Managed Stream Viewer")
            .with_inner_size(winit::dpi::PhysicalSize::new(width as u32, height as u32))
            .build(&event_loop)?,
    );
    let context = softbuffer::Context::new(window.clone())?;
    let mut surface = softbuffer::Surface::new(&context, window.clone())?;

    let mut login_session_opt = Some(login_session);
    let mut user_session_opt: Option<CompositorSession> = None;

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
                
                if last_pipeline_restart.elapsed() >= std::time::Duration::from_millis(500) {
                    eprintln!("kwin-viewer: restarting GStreamer pipeline for new resolution...");
                    last_pipeline_restart = std::time::Instant::now();
                    engine.pipeline.set_state(gst::State::Null).ok();
                    let proxy_clone = event_loop_proxy.clone();
                    
                    let active_node = if let Some(ref u_sess) = user_session_opt {
                        u_sess.pipewire_node_id
                    } else if let Some(ref l_sess) = login_session_opt {
                        l_sess.pipewire_node_id
                    } else {
                        0
                    };

                    engine.pipeline = redfog_core::make_pipeline(active_node, frame_store.clone(), move |changed| {
                        if changed {
                            let _ = proxy_clone.send_event(UserEvent::FrameSizeChanged);
                        } else {
                            let _ = proxy_clone.send_event(UserEvent::NewFrame);
                        }
                    });
                    engine.pipeline.set_state(gst::State::Playing).ok();
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
                // Monitor if Login KWin compositor has exited to trigger dynamic handoff
                if let Some(ref mut l_sess) = login_session_opt {
                    if let Ok(Some(status)) = l_sess.try_wait() {
                        if status.success() {
                            eprintln!("kwin-viewer: Login UI compositor exited successfully. Starting User session compositor...");
                            
                            let payload = if user_app == "plasmashell" {
                                vec!["plasmashell".to_string(), "--no-respawn".to_string()]
                            } else {
                                vec![user_app.to_string()]
                            };

                            match CompositorSession::spawn(
                                SessionType::User("user".to_string()),
                                "redfog-user-0",
                                width,
                                height,
                                scale,
                                &payload,
                            ) {
                                Ok(u_session) => {
                                    eprintln!("kwin-viewer: User session compositor spawned successfully with payload natively!");

                                    eprintln!("kwin-viewer: executing dynamic pipeline and input handoff...");
                                    if let Err(e) = engine.handoff(&u_session) {
                                        eprintln!("kwin-viewer: Handoff failed: {e}");
                                    } else {
                                        eprintln!("kwin-viewer: Handoff successful!");
                                         user_session_opt = Some(u_session);
 
                                         // Recreate the GStreamer pipeline from scratch for the new node
                                         engine.pipeline.set_state(gst::State::Null).ok();
                                         let proxy_clone = event_loop_proxy.clone();
                                         engine.pipeline = redfog_core::make_pipeline(
                                             user_session_opt.as_ref().unwrap().pipewire_node_id,
                                             frame_store.clone(),
                                             move |changed| {
                                                 if changed {
                                                     let _ = proxy_clone.send_event(UserEvent::FrameSizeChanged);
                                                 } else {
                                                     let _ = proxy_clone.send_event(UserEvent::NewFrame);
                                                 }
                                             }
                                         );
                                         engine.pipeline.set_state(gst::State::Playing).ok();
                                         eprintln!("kwin-viewer: GStreamer pipeline recreated for user session");
 
                                         login_session_opt = None; // Already exited cleanly
                                     }
                                }
                                Err(e) => {
                                    eprintln!("kwin-viewer: failed to spawn User session: {e}");
                                    elwt.exit();
                                }
                            }
                        } else {
                            eprintln!("kwin-viewer: Login UI exited with error status {:?}", status);
                            elwt.exit();
                        }
                    }
                }

                // Debounce window resize.
                if let Some((w, h, t)) = pending_resize {
                    if t.elapsed() >= std::time::Duration::from_millis(200) {
                        let snapped_w = ((w + 16) / 32) * 32;
                        let snapped_h = ((h + 16) / 32) * 32;
                        eprintln!("kwin-viewer: debounce fired — resizing to {}x{} (snapped from {}x{})", snapped_w, snapped_h, w, h);
                        
                        if let Some(ref u_sess) = user_session_opt {
                            u_sess.capture_session.resize(snapped_w, snapped_h);
                        } else if let Some(ref l_sess) = login_session_opt {
                            l_sess.capture_session.resize(snapped_w, snapped_h);
                        }
                        
                        eprintln!("kwin-viewer: KWin resize complete. Restarting GStreamer pipeline...");
                        last_pipeline_restart = std::time::Instant::now();
                        engine.pipeline.set_state(gst::State::Null).ok();
                        let proxy_clone = event_loop_proxy.clone();
                        
                        let active_node = if let Some(ref u_sess) = user_session_opt {
                            u_sess.pipewire_node_id
                        } else if let Some(ref l_sess) = login_session_opt {
                            l_sess.pipewire_node_id
                        } else {
                            0
                        };

                        engine.pipeline = redfog_core::make_pipeline(active_node, frame_store.clone(), move |changed| {
                            if changed {
                                let _ = proxy_clone.send_event(UserEvent::FrameSizeChanged);
                            } else {
                                let _ = proxy_clone.send_event(UserEvent::NewFrame);
                            }
                        });
                        engine.pipeline.set_state(gst::State::Playing).ok();
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
                    let rx = position.x.clamp(0.0, frame_w as f64 - 1.0);
                    let ry = position.y.clamp(0.0, frame_h as f64 - 1.0);
                    engine.input_forwarder.fake_input.pointer_motion_absolute(rx, ry);
                    let _ = engine.input_forwarder.conn.flush();
                }
            }
            Event::WindowEvent { event: WindowEvent::MouseInput { state, button, .. }, .. } => {
                if has_focus {
                    if let Some(evdev_btn) = winit_button_to_evdev(button) {
                        engine.input_forwarder.fake_input.button(
                            evdev_btn,
                            if state == ElementState::Pressed { 1 } else { 0 },
                        );
                        let _ = engine.input_forwarder.conn.flush();
                    }
                }
            }
            Event::WindowEvent { event: WindowEvent::KeyboardInput { event: key_event, .. }, .. } => {
                if has_focus {
                    if let PhysicalKey::Code(winit_key) = key_event.physical_key {
                        if let Some(evdev_key) = winit_key_to_evdev(winit_key) {
                            engine.input_forwarder.fake_input.keyboard_key(
                                evdev_key,
                                if key_event.state == ElementState::Pressed { 1 } else { 0 },
                            );
                            let _ = engine.input_forwarder.conn.flush();
                        }
                    }
                }
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                // Shut down any active session compositors upon viewer exit
                if let Some(u_session) = user_session_opt.take() {
                    u_session.terminate();
                }
                if let Some(l_session) = login_session_opt.take() {
                    l_session.terminate();
                }
                elwt.exit();
            }
            _ => {}
        }
    })?;

    Ok(())
}
