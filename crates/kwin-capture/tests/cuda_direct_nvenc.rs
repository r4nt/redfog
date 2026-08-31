//! True zero-copy path: DMA-BUF -> CUDA -> NVENC, driving the encoder
//! ourselves through `nvidia-video-codec-sdk` rather than handing off to
//! GStreamer's `nvh264enc`. No GL, no `glupload`/`glcolorconvert`, no
//! GStreamer pipeline at all for the video leg — this is the thing
//! `dmabuf_gl_upload.rs`/`native_capture_encode.rs` deliberately don't do.
//!
//! Exercises `kwin_capture::nvenc_session::CudaDirectEncoderSession`
//! directly — the real production implementation (now wired into
//! `redfog-server` via `redfog_core::make_cuda_direct_encoder_session` for
//! `VideoEncoder::NvencDirect`), not a copy of its logic — so this test and
//! production can't drift apart the way they briefly did when this file
//! used to duplicate the whole capture/import/encode loop by hand.

use kwin_capture::nvenc_session::{CudaDirectEncoderSession, VideoCodec};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FPS: u32 = 60;
const BITRATE_KBPS: u32 = 5_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cuda_direct_nvenc_encode_glxgears_test() {
    // See capture_integration.rs's identical call for why this is here.
    redfog_test_cleanup::ensure_active();
    let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

    // `CudaDirectEncoderSession::spawn` isolates setup failures to its own
    // background thread rather than surfacing a `Result` here (see its own
    // doc comment), so a missing GPU wouldn't fail fast — it'd instead show
    // up as a confusing "no encoded access unit received within 2s" panic
    // below, after already paying for a real KWin/glxgears spawn. Check for
    // a CUDA-capable GPU directly first, matching
    // `nvenc_two_sessions_same_process.rs`'s equivalent guard — this crate's
    // tests otherwise can't run at all on hardware/CI without an NVIDIA GPU
    // (e.g. GH Actions' hosted runners).
    if cudarc016::driver::CudaContext::new(0).is_err() {
        eprintln!("no CUDA-capable GPU available — skipping cuda_direct_nvenc_encode_glxgears_test");
        return;
    }

    let runtime_dir = std::env::temp_dir().join(format!("redfog-it-cuda-nvenc-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::env::set_var("REDFOG_RUNTIME_DIR", &runtime_dir);
    std::env::set_var("REDFOG_ALWAYS_SOFTWARE", "0");

    let _dbus_session = redfog_core::ensure_private_dbus_session();
    let _headless_runtime = redfog_core::HeadlessRuntime::start(runtime_dir).unwrap();

    eprintln!("Spawning KWin running glxgears...");
    let compositor = session_backend::spawn_user_compositor_direct(
        session_backend::Backend::Kwin,
        "user",
        &["glxgears".to_string()],
        WIDTH,
        HEIGHT,
        FPS,
    )
    .unwrap();

    let node_id = match compositor.video_source(None) {
        redfog_core::VideoSource::PipeWireNode(node) => node,
        _ => panic!("expected a PipeWireNode video source"),
    };
    let socket_path = match &compositor {
        session_backend::SpawnedCompositor::Kwin(session) => session.socket_path.clone(),
        _ => panic!("expected a Kwin-backed compositor"),
    };
    // Kills kwin_wayland synchronously when this test function returns for
    // *any* reason, including a panic on one of the asserts below --
    // redfog-test-cleanup's watchdog is only a safety net for this whole
    // process dying abnormally; without this, a fast-panicking test can
    // finish (and cargo can start the next kwin-capture test binary)
    // before the watchdog gets scheduled, and the two collide on the
    // hardcoded "redfog-user-0" socket name (confirmed live).
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

    let (tx, rx) = std::sync::mpsc::channel::<(Vec<u8>, bool)>();
    eprintln!("Starting CudaDirectEncoderSession (DMA-BUF -> CUDA -> NVENC, no GStreamer)...");
    let session = CudaDirectEncoderSession::spawn(node_id, socket_path, WIDTH, HEIGHT, FPS, BITRATE_KBPS, VideoCodec::H264, move |data, is_keyframe, _capture_instant| {
        let _ = tx.send((data, is_keyframe));
    });

    let mut frame_count: u64 = 0;
    let mut got_keyframe = false;
    let mut window_start = std::time::Instant::now();
    let mut window_frames: u64 = 0;
    let test_start = std::time::Instant::now();

    while test_start.elapsed() < std::time::Duration::from_secs(15) {
        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok((data, is_keyframe)) => {
                assert!(!data.is_empty(), "encoded access unit was empty");
                got_keyframe |= is_keyframe;
                frame_count += 1;
                window_frames += 1;
            }
            Err(_) => panic!("no encoded access unit received within 2s"),
        }
        if window_start.elapsed() >= std::time::Duration::from_secs(5) {
            let fps = window_frames as f64 / window_start.elapsed().as_secs_f64();
            eprintln!("CUDA-DIRECT NVENC FPS: {fps:.2} ({window_frames} frames, {frame_count} access units so far)");
            window_start = std::time::Instant::now();
            window_frames = 0;
        }
    }

    drop(session);

    eprintln!("Total: {frame_count} access units encoded (keyframe seen: {got_keyframe}).");
    assert!(frame_count > 0, "no encoded access units produced");
    assert!(got_keyframe, "never received a keyframe among {frame_count} access units");
    let overall_fps = frame_count as f64 / test_start.elapsed().as_secs_f64();
    assert!(overall_fps >= 45.0, "expected at least 45 FPS, got {overall_fps:.2}");
}
