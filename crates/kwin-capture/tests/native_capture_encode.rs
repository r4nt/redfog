//! End-to-end test of `VideoSource::KwinNativeDmaBuf`: a real KWin session,
//! captured via `kwin_capture::pipewire_capture::PipewireCapture` (not
//! GStreamer's `pipewiresrc`), through `redfog_core::make_encoder_pipeline`'s
//! `appsrc ! glupload ! glcolorconvert ! nvh264enc ! ...` pipeline, producing
//! real encoded H.264 access units.
//!
//! Complements `dmabuf_gl_upload.rs` (which only validates the DMA-BUF ->
//! `glupload` import in isolation) by exercising the actual production code
//! path: `CompositorSession::video_source()`'s `REDFOG_NATIVE_CAPTURE=1` gate,
//! `spawn_kwin_native_frame_pusher`, and the real `nvh264enc` encode.

use gstreamer::prelude::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_capture_produces_encoded_h264() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let runtime_dir = std::env::temp_dir().join(format!("redfog-it-native-encode-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::env::set_var("REDFOG_RUNTIME_DIR", &runtime_dir);
    std::env::set_var("REDFOG_ALWAYS_SOFTWARE", "0");
    std::env::set_var("REDFOG_NATIVE_CAPTURE", "1");
    std::env::set_var("GST_GL_WINDOW", "surfaceless");
    std::env::set_var("GST_GL_PLATFORM", "egl");

    redfog_core::ensure_private_dbus_session();
    let _headless_runtime = redfog_core::HeadlessRuntime::start(runtime_dir).unwrap();

    eprintln!("Spawning KWin running glxgears...");
    let compositor = session_backend::spawn_user_compositor_direct(
        session_backend::Backend::Kwin,
        "user",
        &["glxgears".to_string()],
        1280,
        720,
        60,
    ).unwrap();

    let source = compositor.video_source();
    assert!(
        matches!(source, redfog_core::VideoSource::KwinNativeDmaBuf { .. }),
        "expected REDFOG_NATIVE_CAPTURE=1 to select VideoSource::KwinNativeDmaBuf, got a different variant"
    );

    gstreamer::init().unwrap();

    if gstreamer::ElementFactory::find("nvh264enc").is_none() {
        eprintln!("nvh264enc not available on this machine — skipping (this test needs real NVENC hardware)");
        return;
    }

    let (tx, rx) = std::sync::mpsc::channel::<(Vec<u8>, bool)>();
    let pipeline = redfog_core::make_encoder_pipeline(
        source,
        "native-capture-encode-test",
        redfog_core::VideoEncoder::Nvenc,
        None,
        5000,
        move |data, is_keyframe| {
            let _ = tx.send((data, is_keyframe));
        },
    );
    pipeline.set_state(gstreamer::State::Playing).unwrap();

    let mut got_keyframe = false;
    let mut count = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline && count < 30 {
        if let Ok((data, is_keyframe)) = rx.recv_timeout(std::time::Duration::from_secs(2)) {
            assert!(!data.is_empty(), "encoded access unit was empty");
            got_keyframe |= is_keyframe;
            count += 1;
        }
    }

    pipeline.set_state(gstreamer::State::Null).ok();

    assert!(count > 0, "no encoded access units received within 10s");
    assert!(got_keyframe, "never received a keyframe among {count} access units");
    eprintln!("Received {count} encoded access units (keyframe seen: {got_keyframe}).");
}
