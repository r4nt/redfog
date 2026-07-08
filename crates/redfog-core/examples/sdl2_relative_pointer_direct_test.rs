//! Same diagnostic as relative_pointer_direct_test.rs, but against
//! sdl2_relative_pointer_check (XWayland + SDL2 relative-mouse-mode, the
//! actual mechanism Portal/Source engine uses) instead of a native Wayland
//! client — checking whether the earlier "fake_input drops motion under an
//! active pointer lock" finding (proven for native Wayland only) actually
//! generalizes to XWayland's completely different pointer-grab code path.
//!
//! Usage: cargo run --example sdl2_relative_pointer_direct_test -p redfog-core
//! (run `cargo build --example sdl2_relative_pointer_check -p redfog-test-ux`
//! first.)

use std::path::PathBuf;
use std::time::Duration;

use redfog_core::{CompositorSession, InputForwarder, SessionType};

fn main() {
    redfog_core::ensure_private_dbus_session();

    let sdl_check = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../redfog-test-ux/target/debug/examples/sdl2_relative_pointer_check");
    let sdl_check = if sdl_check.exists() {
        sdl_check
    } else {
        let shared = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/examples/sdl2_relative_pointer_check");
        assert!(
            shared.exists(),
            "sdl2_relative_pointer_check binary not found at {sdl_check:?} or {shared:?} — run \
             `cargo build --example sdl2_relative_pointer_check -p redfog-test-ux` first"
        );
        shared
    };

    let _headless_runtime = redfog_core::HeadlessRuntime::start(redfog_core::default_runtime_dir()).expect("start headless runtime");

    let compositor = CompositorSession::spawn(
        SessionType::Login,
        "sdlrelptr-direct-0",
        1280,
        720,
        1.0,
        &[sdl_check.to_str().unwrap().to_string()],
    )
    .expect("spawn compositor");

    let forwarder = InputForwarder::connect(&compositor.socket_path).expect("connect input forwarder");

    // Give XWayland + the SDL2 client time to start up and engage relative
    // mouse mode.
    std::thread::sleep(Duration::from_secs(3));

    let n = 50;
    println!("\n=== sending {n} pointer_motion(20, 0) calls, 100ms apart, over ~5s ===");
    for i in 0..n {
        forwarder.fake_input.pointer_motion(20.0, 0.0);
        let _ = forwarder.conn.flush();
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("all {n} sends issued");

    std::thread::sleep(Duration::from_secs(1));
    println!("\ndone — count SDLRELPTR 'MouseMotion' lines above out of {n} sends, and whether total_dx approaches {}", n * 20);

    compositor.terminate();
}
