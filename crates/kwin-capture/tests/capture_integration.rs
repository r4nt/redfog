#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kwin_native_pipewire_capture_glxgears_test() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    // 1. Create a temporary runtime dir
    let runtime_dir = std::env::temp_dir().join(format!("redfog-it-pw-capture-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::env::set_var("REDFOG_RUNTIME_DIR", &runtime_dir);
    std::env::set_var("REDFOG_ALWAYS_SOFTWARE", "0");

    // 2. Start headless D-Bus and PipeWire runtime
    redfog_core::ensure_private_dbus_session();
    let _headless_runtime = redfog_core::HeadlessRuntime::start(runtime_dir).unwrap();

    // 3. Spawn KWin running glxgears
    eprintln!("Spawning KWin running glxgears...");
    let compositor = session_backend::spawn_user_compositor_direct(
        session_backend::Backend::Kwin,
        "user",
        &["glxgears".to_string()],
        1280,
        720,
        60,
    ).unwrap();

    let node_id = match compositor.video_source(None) {
        redfog_core::VideoSource::PipeWireNode(node) => node,
        _ => panic!("Expected PipeWireNode"),
    };
    eprintln!("KWin screencast PipeWire node ID: {node_id}");

    let socket_path = match &compositor {
        session_backend::SpawnedCompositor::Kwin(session) => session.socket_path.clone(),
        _ => panic!("expected a Kwin-backed compositor"),
    };

    // 4. Start our native PipeWire capture
    eprintln!("Starting native Pipewire capture...");
    let capture = kwin_capture::pipewire_capture::PipewireCapture::start(node_id, socket_path, false).unwrap();

    // 5. Measure FPS over 5 seconds
    eprintln!("Measuring FPS for 5 seconds...");
    // Drain any initial frames
    for _ in 0..5 {
        if let Some(frame) = capture.next_frame() {
            assert!(frame.is_dma_buf, "Expected frame buffer datatype to be DmaBuf, but got MemFd! Verify GPU render node availability.");
            unsafe { libc::close(frame.fd) };
        }
    }
    let mut frame_count = 0;
    let measurement_start = std::time::Instant::now();
    while measurement_start.elapsed() < std::time::Duration::from_secs(5) {
        if let Some(frame) = capture.next_frame() {
            assert!(frame.is_dma_buf, "Expected frame buffer datatype to be DmaBuf, but got MemFd! Verify GPU render node availability.");
            unsafe { libc::close(frame.fd) };
            frame_count += 1;
        }
    }
    let elapsed = measurement_start.elapsed().as_secs_f64();
    let fps = frame_count as f64 / elapsed;
    eprintln!("NATIVE CAPTURE FPS: {:.2} ({} frames in {:.2}s)", fps, frame_count, elapsed);
    assert!(fps >= 45.0, "Expected at least 45 FPS, got {:.2}", fps);
}
