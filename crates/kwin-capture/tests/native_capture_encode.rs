//! End-to-end test of `VideoSource::KwinNativeDmaBuf`: a real KWin session,
//! captured via `kwin_capture::pipewire_capture::PipewireCapture` (not
//! GStreamer's `pipewiresrc`), through `redfog_core::make_encoder_pipeline`'s
//! `appsrc ! glupload ! glcolorconvert ! nvh264enc ! ...` pipeline, producing
//! real encoded H.264 access units.
//!
//! Complements `dmabuf_gl_upload.rs` (which only validates the DMA-BUF ->
//! `glupload` import in isolation) by exercising the actual production code
//! path: `CompositorSession::video_source(Some(VideoEncoder::Nvenc))`'s
//! default selection of `KwinNativeDmaBuf`, `spawn_kwin_native_frame_pusher`,
//! and the real `nvh264enc` encode.

use gstreamer::prelude::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_capture_produces_encoded_h264() {
    // See capture_integration.rs's identical call for why this is here.
    redfog_test_cleanup::ensure_active();
    let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

    let runtime_dir = std::env::temp_dir().join(format!("redfog-it-native-encode-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::env::set_var("REDFOG_RUNTIME_DIR", &runtime_dir);
    std::env::set_var("REDFOG_ALWAYS_SOFTWARE", "0");
    std::env::set_var("GST_GL_WINDOW", "surfaceless");
    std::env::set_var("GST_GL_PLATFORM", "egl");

    let _dbus_session = redfog_core::ensure_private_dbus_session();
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

    let source = compositor.video_source(Some(redfog_core::VideoEncoder::Nvenc));
    assert!(
        matches!(source, redfog_core::VideoSource::KwinNativeDmaBuf { .. }),
        "expected video_source(Some(Nvenc)) to default to VideoSource::KwinNativeDmaBuf, got a different variant"
    );
    // See capture_integration.rs's identical guard for why this is here.
    struct KillCompositorOnDrop(session_backend::SpawnedCompositor);
    impl Drop for KillCompositorOnDrop {
        fn drop(&mut self) {
            self.0.kill_best_effort();
            // kill_best_effort() only signals kwin_wayland itself, not
            // Xwayland/glxgears (kwin_wayland's *own* children, spawned via
            // --exit-with-session) -- confirmed live, those survived on
            // their own otherwise. See kill_descendants_named's doc comment.
            redfog_test_cleanup::kill_descendants_named("kwin_wayland");
        }
    }
    let _compositor_guard = KillCompositorOnDrop(compositor);

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
        redfog_core::VideoCodec::H264,
        _headless_runtime.pipewire_socket.to_str().unwrap(),
        move |data, is_keyframe, _capture_instant| {
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
