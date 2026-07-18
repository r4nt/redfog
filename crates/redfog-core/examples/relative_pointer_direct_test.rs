//! Direct diagnostic, bypassing the whole moonlight/RTSP/ENet stack
//! entirely: spawns a compositor running `relative_pointer_check` (which
//! requests a real pointer lock, same as a game entering mouse-look mode)
//! and calls `fake_input.pointer_motion()` directly, to see whether that
//! reaches the client via `zwp_relative_pointer_v1` (what games actually
//! use for raw mouse-look) or only as regular `wl_pointer.motion`.
//!
//! Usage: cargo run --example relative_pointer_direct_test -p redfog-core
//! (run `cargo build --example relative_pointer_check -p redfog-test-ux`
//! first, or the missing-binary assert below will explain what to build.)

use std::path::PathBuf;
use std::time::Duration;

use redfog_core::{CompositorSession, InputForwarder, SessionType};

fn main() {
    redfog_core::ensure_private_dbus_session();

    let relptr_check = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../redfog-test-ux/target/debug/examples/relative_pointer_check");
    let relptr_check = if relptr_check.exists() {
        relptr_check
    } else {
        // examples build into the workspace's shared target dir, not each
        // crate's own target/ — check there too.
        let shared = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/examples/relative_pointer_check");
        assert!(
            shared.exists(),
            "relative_pointer_check binary not found at {relptr_check:?} or {shared:?} — run \
             `cargo build --example relative_pointer_check -p redfog-test-ux` first"
        );
        shared
    };

    let _headless_runtime = redfog_core::HeadlessRuntime::start(redfog_core::default_runtime_dir()).expect("start headless runtime");

    let compositor = CompositorSession::spawn(
        SessionType::Login,
        "relptr-direct-0",
        1280,
        720,
        1.0,
        60,
        &[relptr_check.to_str().unwrap().to_string()],
    )
    .expect("spawn compositor");

    let forwarder = InputForwarder::connect(&compositor.socket_path).expect("connect input forwarder");

    // Give the client time to start up and request its pointer lock.
    std::thread::sleep(Duration::from_secs(2));

    let n = 50;
    println!("\n=== sending {n} pointer_motion(20, 0) calls, 100ms apart, over ~5s ===");
    for i in 0..n {
        forwarder.fake_input.pointer_motion(20.0, 0.0);
        let _ = forwarder.conn.flush();
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("all {n} sends issued");

    std::thread::sleep(Duration::from_secs(1));
    println!("\ndone — count RELPTR lines above: any 'DeviceEvent::MouseMotion' or 'CursorMoved' at all out of {n} sends?");

    compositor.terminate();
}
