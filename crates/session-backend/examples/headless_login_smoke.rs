//! Live smoke test for `spawn_login_compositor` against the *real*
//! `redfog-login` binary (not `redfog-test-ux`'s stand-in, which is all the
//! automated integration tests exercise) — spawns it exactly the way
//! production does, waits for a real frame, and dumps it to a PNG so the
//! whole chain (socket handshake, appsrc, real rendering) can be visually
//! confirmed working end to end.
//!
//! Usage: cargo run -p session-backend --example headless_login_smoke -- \
//!   target/release/redfog-login /tmp/headless-login-smoke.png

use std::sync::{Arc, Mutex};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let login_bin = args.get(1).cloned().unwrap_or_else(|| "target/release/redfog-login".to_string());
    let out_path = args.get(2).cloned().unwrap_or_else(|| "/tmp/headless-login-smoke.png".to_string());

    gstreamer::init().expect("gstreamer::init");

    let compositor = session_backend::spawn_login_compositor(&[login_bin], 1280, 720).expect("spawn_login_compositor should succeed");

    let frame_store: Arc<Mutex<Option<redfog_core::Frame>>> = Arc::new(Mutex::new(None));
    let pipeline = redfog_core::make_pipeline(compositor.video_source(), frame_store.clone(), |_changed| {});
    {
        use gstreamer::prelude::*;
        pipeline.set_state(gstreamer::State::Playing).expect("pipeline playing");
    }

    eprintln!("waiting for a frame...");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if frame_store.lock().unwrap().is_some() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "no frame arrived within 10s");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // A couple more frames so it's not literally the first (possibly
    // still-blank) one.
    std::thread::sleep(std::time::Duration::from_millis(500));

    let frame = frame_store.lock().unwrap().take().expect("frame present");
    eprintln!("got a {}x{} frame, converting BGRx -> RGBA and saving to {out_path}...", frame.width, frame.height);

    let mut rgba = frame.data.clone();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2); // BGRx -> RGBx
        px[3] = 255; // opaque
    }
    let pixmap = tiny_skia::Pixmap::from_vec(rgba, tiny_skia::IntSize::from_wh(frame.width, frame.height).unwrap()).expect("pixmap");
    pixmap.save_png(&out_path).expect("save_png");
    println!("wrote {out_path}");

    {
        use gstreamer::prelude::*;
        let _ = pipeline.set_state(gstreamer::State::Null);
    }
    compositor.terminate();
}
