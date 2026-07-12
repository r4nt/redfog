//! Live smoke test for `spawn_login_compositor` against the *real*
//! `redfog-login` binary (not `redfog-test-ux`'s stand-in, which is all the
//! automated integration tests exercise) — spawns it exactly the way
//! production does, waits for a real frame, and dumps it to a PNG so the
//! whole chain (socket handshake, appsrc, real rendering) can be visually
//! confirmed working end to end.
//!
//! Also drives a real click-to-focus + type + switch-keyboard-layout +
//! type-again sequence through `InputSink`, dumping a frame after each step
//! — the only way to confirm the `xkbcommon` keymap wiring (see
//! `redfog-login/src/keymap.rs`) actually changes what a keypress produces
//! on the real rendered screen, not just in the `keymap` module's own unit
//! tests.
//!
//! Usage: cargo run -p session-backend --example headless_login_smoke -- \
//!   target/release/redfog-login /tmp/headless-login-smoke

use std::sync::{Arc, Mutex};

const BTN_LEFT: u32 = 272;
const KEY_Y: u32 = 21; // evdev — US "y", German QWERTZ "z"

fn wait_and_save(frame_store: &Arc<Mutex<Option<redfog_core::Frame>>>, out_path: &str) {
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
    // still-blank, or pre-input-applied) one.
    std::thread::sleep(std::time::Duration::from_millis(500));

    let frame = frame_store.lock().unwrap().take().expect("frame present");
    eprintln!("got a {}x{} frame, converting BGRx -> RGBA and saving to {out_path}...", frame.width, frame.height);

    let mut rgba = frame.data.clone();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2); // BGRx -> RGBx
        px[3] = 255; // opaque
    }
    let pixmap = tiny_skia::Pixmap::from_vec(rgba, tiny_skia::IntSize::from_wh(frame.width, frame.height).unwrap()).expect("pixmap");
    pixmap.save_png(out_path).expect("save_png");
    println!("wrote {out_path}");
}

fn click(input: &mut dyn redfog_core::InputSink, x: f64, y: f64) {
    input.pointer_motion_absolute(x, y);
    input.button(BTN_LEFT, true);
    input.button(BTN_LEFT, false);
    input.flush();
}

fn tap_key(input: &mut dyn redfog_core::InputSink, keycode: u32) {
    input.keyboard_key(keycode, true);
    input.keyboard_key(keycode, false);
    input.flush();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let login_bin = args.get(1).cloned().unwrap_or_else(|| "target/release/redfog-login".to_string());
    let out_prefix = args.get(2).cloned().unwrap_or_else(|| "/tmp/headless-login-smoke".to_string());

    gstreamer::init().expect("gstreamer::init");

    let compositor = session_backend::spawn_login_compositor(&[login_bin], 1280, 720).expect("spawn_login_compositor should succeed");
    let mut input = compositor.input_sink().expect("input_sink");

    let frame_store: Arc<Mutex<Option<redfog_core::Frame>>> = Arc::new(Mutex::new(None));
    let client_name = format!("redfog-headless-login-smoke-{}", std::process::id());
    let pipeline = redfog_core::make_pipeline(compositor.video_source(), &client_name, frame_store.clone(), |_changed| {});
    {
        use gstreamer::prelude::*;
        pipeline.set_state(gstreamer::State::Playing).expect("pipeline playing");
    }

    wait_and_save(&frame_store, &format!("{out_prefix}-initial.png"));

    // Layout coordinates below match `ui::render`'s fixed layout math for a
    // 1280x720 canvas (see that function's own comments) — not derived from
    // a `Layout` value, since this process only has the frame socket, not
    // the login process's internals.
    eprintln!("clicking username field and typing 'y' under the default US layout...");
    click(&mut *input, 640.0, 310.0); // username field
    tap_key(&mut *input, KEY_Y);
    wait_and_save(&frame_store, &format!("{out_prefix}-us-y.png"));

    eprintln!("opening the keyboard dropdown and selecting German...");
    click(&mut *input, 640.0, 222.0); // keyboard dropdown toggle
    // The reader thread applies input to `state` synchronously, but the
    // `Layout` a click hit-tests against only gets its `keyboard_options`
    // rects populated by the main thread's next render pass (~33ms cadence
    // — see main.rs's module doc comment); clicking again immediately would
    // race that and hit a stale, still-closed layout. A real mouse doesn't
    // click twice within a frame, so give it a beat.
    std::thread::sleep(std::time::Duration::from_millis(100));
    click(&mut *input, 640.0, 350.0); // "German" row (3rd of 10 — see default_keyboard_layouts)
    wait_and_save(&frame_store, &format!("{out_prefix}-de-selected.png"));

    eprintln!("typing 'y' again — should now produce 'z' (German QWERTZ)...");
    tap_key(&mut *input, KEY_Y);
    wait_and_save(&frame_store, &format!("{out_prefix}-de-z.png"));

    {
        use gstreamer::prelude::*;
        let _ = pipeline.set_state(gstreamer::State::Null);
    }
    compositor.terminate();
}
