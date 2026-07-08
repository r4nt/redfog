//! Diagnostic: does `org_kde_kwin_fake_input`'s `pointer_motion` (relative)
//! request actually reach clients via `zwp_relative_pointer_v1` (the
//! protocol games use for raw mouse-look, requested via a real pointer
//! lock/grab), or only as regular `wl_pointer.motion`?
//!
//! Investigating a user report that mouse-look feels much slower than
//! desktop mouse movement in the same session — `redfog-test-ux`'s own
//! `egui::Event::PointerMoved` (backed by plain `wl_pointer.motion`) was
//! already proven perfectly linear with no scaling/latency issues, but that
//! doesn't rule out the actual relative-pointer path being different, since
//! games use `zwp_relative_pointer_v1` specifically, not the same path
//! `PointerMoved` is derived from. Reference project `gow-wolf` avoids this
//! entirely by injecting input through a real uinput kernel device rather
//! than a compositor-level fake-input protocol.
//!
//! Logs both `WindowEvent::CursorMoved` (wl_pointer.motion, absolute) and
//! `DeviceEvent::MouseMotion` (zwp_relative_pointer_v1, raw relative) so the
//! two can be compared directly against what the server was asked to send.

use winit::event::{DeviceEvent, Event, WindowEvent};
use winit::event_loop::EventLoop;
use winit::window::{CursorGrabMode, WindowBuilder};

fn main() {
    let label = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "default".to_string());
    println!("RELPTR[{label}]: started");

    let event_loop = EventLoop::new().unwrap();
    let window = WindowBuilder::new()
        .with_title("Relative Pointer Check")
        .with_inner_size(winit::dpi::LogicalSize::new(400.0, 300.0))
        .build(&event_loop)
        .unwrap();

    // Same as a game entering mouse-look mode — requests a real pointer
    // lock, which is what should make the compositor start emitting
    // zwp_relative_pointer_v1 events for this client, per protocol design.
    match window.set_cursor_grab(CursorGrabMode::Locked) {
        Ok(()) => println!("RELPTR[{label}]: cursor grab (Locked) succeeded"),
        Err(e) => {
            println!("RELPTR[{label}]: Locked grab failed ({e}), trying Confined");
            match window.set_cursor_grab(CursorGrabMode::Confined) {
                Ok(()) => println!("RELPTR[{label}]: cursor grab (Confined) succeeded"),
                Err(e2) => println!("RELPTR[{label}]: Confined grab also failed ({e2})"),
            }
        }
    }
    window.set_cursor_visible(false);

    let mut last_cursor_pos: Option<(f64, f64)> = None;
    let mut relative_count = 0u64;
    let mut cursor_moved_count = 0u64;

    let _ = event_loop.run(move |event, elwt| {
        elwt.set_control_flow(winit::event_loop::ControlFlow::Poll);
        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => elwt.exit(),
            Event::WindowEvent { event: WindowEvent::CursorMoved { position, .. }, .. } => {
                cursor_moved_count += 1;
                let (x, y) = (position.x, position.y);
                if let Some((lx, ly)) = last_cursor_pos {
                    println!("RELPTR[{label}]: CursorMoved(wl_pointer.motion) dx={} dy={} x={x} y={y}", x - lx, y - ly);
                } else {
                    println!("RELPTR[{label}]: CursorMoved(wl_pointer.motion) first x={x} y={y}");
                }
                last_cursor_pos = Some((x, y));
            }
            Event::DeviceEvent { event: DeviceEvent::MouseMotion { delta }, .. } => {
                relative_count += 1;
                println!("RELPTR[{label}]: DeviceEvent::MouseMotion(zwp_relative_pointer_v1) dx={} dy={}", delta.0, delta.1);
            }
            Event::WindowEvent { event: WindowEvent::KeyboardInput { event: key_event, .. }, .. } => {
                use winit::keyboard::{Key, NamedKey};
                if key_event.state == winit::event::ElementState::Pressed
                    && key_event.logical_key == Key::Named(NamedKey::Escape)
                {
                    println!(
                        "RELPTR[{label}]: exiting on Escape — totals: cursor_moved={cursor_moved_count} relative_motion={relative_count}"
                    );
                    elwt.exit();
                }
            }
            _ => {}
        }
    });
}
