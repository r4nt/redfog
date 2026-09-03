#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kwin_native_pipewire_capture_glxgears_test() {
    // Not what actually installs the protection (that already ran, via
    // redfog-test-cleanup's own #[ctor], before this binary's main() even
    // started) -- keeps the linker from stripping the crate as unused. See
    // that crate's doc comment for why PR_SET_PDEATHSIG alone can't
    // protect kwin_wayland's own spawn (file capabilities clear it at
    // exec time), confirmed live after this test's spawned sessions leaked
    // repeatedly.
    redfog_test_cleanup::ensure_active();
    let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

    // 1. Create a temporary runtime dir
    let runtime_dir = std::env::temp_dir().join(format!("redfog-it-pw-capture-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::env::set_var("REDFOG_RUNTIME_DIR", &runtime_dir);
    std::env::set_var("REDFOG_ALWAYS_SOFTWARE", "0");

    // 2. Start headless D-Bus and PipeWire runtime
    let _dbus_session = redfog_core::ensure_private_dbus_session();
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

    // 4. Start our native PipeWire capture
    eprintln!("Starting native Pipewire capture...");
    let capture = kwin_capture::pipewire_capture::PipewireCapture::start(node_id, socket_path, _headless_runtime.pipewire_socket.to_str().unwrap(), false).unwrap();

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
