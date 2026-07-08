//! Diagnostic: does `org_kde_kwin_fake_input`'s `pointer_motion` reach an
//! XWayland client using SDL2's relative-mouse-mode (`SDL_SetRelativeMouseMode`)
//! — exactly the mechanism Source engine (Portal) uses for camera-look — the
//! same way it (allegedly) fails to reach a *native Wayland* client holding a
//! `zwp_pointer_constraints_v1` lock (proven earlier via
//! relative_pointer_check.rs + winit's CursorGrabMode::Locked)?
//!
//! XWayland's own X11-style pointer grab is a completely different KWin code
//! path from native Wayland's zwp_pointer_constraints_v1/zwp_relative_pointer_v1
//! — the earlier finding does not necessarily generalize to it, and Portal
//! reportedly "played fine" (just slow, not frozen) with fake_input, which
//! contradicts "zero events get through" if the two paths behaved the same.
//! This test settles it directly rather than assuming.
//!
//! Forces SDL_VIDEODRIVER=x11 so this runs through XWayland (matching how
//! Portal/Proton actually runs) rather than SDL2 picking a native Wayland
//! backend on its own.

fn main() {
    std::env::set_var("SDL_VIDEODRIVER", "x11");
    let label = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "default".to_string());
    println!("SDLRELPTR[{label}]: started, DISPLAY={:?}", std::env::var("DISPLAY"));

    let sdl_context = sdl2::init().expect("sdl2 init");
    let video = sdl_context.video().expect("sdl2 video subsystem");
    println!("SDLRELPTR[{label}]: video driver = {}", video.current_video_driver());

    let window = video
        .window("SDL2 Relative Pointer Check", 400, 300)
        .position_centered()
        .build()
        .expect("create window");

    // Exactly what Source engine calls when entering mouse-look mode.
    sdl_context.mouse().set_relative_mouse_mode(true);
    println!(
        "SDLRELPTR[{label}]: relative mouse mode = {}",
        sdl_context.mouse().relative_mouse_mode()
    );

    let mut event_pump = sdl_context.event_pump().expect("event pump");
    let mut motion_count: u64 = 0;
    let mut total_dx: i64 = 0;
    let mut total_dy: i64 = 0;

    'running: loop {
        for event in event_pump.poll_iter() {
            use sdl2::event::Event;
            use sdl2::keyboard::Keycode;
            match event {
                Event::Quit { .. } => break 'running,
                Event::KeyDown { keycode: Some(Keycode::Escape), .. } => {
                    println!(
                        "SDLRELPTR[{label}]: exiting on Escape — totals: motion_events={motion_count} total_dx={total_dx} total_dy={total_dy}"
                    );
                    break 'running;
                }
                Event::MouseMotion { xrel, yrel, .. } => {
                    motion_count += 1;
                    total_dx += xrel as i64;
                    total_dy += yrel as i64;
                    println!("SDLRELPTR[{label}]: MouseMotion xrel={xrel} yrel={yrel}");
                }
                _ => {}
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let _ = window;
}
