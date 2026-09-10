//! Regression test for the "AV1 takeover hangs forever" bug: KWin's virtual
//! output can snap/round a requested custom-mode width — confirmed live,
//! `resize(1602, 1024)` against a real `kwin_wayland --virtual` instance
//! actually produces a 1600-wide output, not 1602. `CompositorSession::
//! resize` used to store the *requested* width/height unconditionally, so
//! `video_source()` (and therefore the next `CudaDirectEncoderSession`
//! built from it) believed 1602 while the real capture stream stayed at
//! 1600 forever — `nvenc_session.rs`'s exact-match frame check then
//! dropped every single frame permanently, no recovery, no error, just
//! silence. The actual invariant that matters isn't "KWin must honor
//! exactly what we ask for" (it doesn't, and that's outside this crate's
//! control) — it's "whatever `video_source()` reports must match what the
//! capture stream actually delivers." This test checks that invariant
//! directly, live.
//!
//! Uses a private socket name (not "redfog-user-0") so it can't collide
//! with a live redfog-server session on the same machine.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compositor_session_resolution_matches_what_capture_actually_delivers_after_an_odd_resize() {
    redfog_test_cleanup::ensure_active();
    let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

    let runtime_dir = std::env::temp_dir().join(format!("redfog-it-snap-probe-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::env::set_var("REDFOG_RUNTIME_DIR", &runtime_dir);

    let _dbus_session = redfog_core::ensure_private_dbus_session();
    let _headless_runtime = redfog_core::HeadlessRuntime::start(runtime_dir).unwrap();

    eprintln!("spawning KWin on private socket 'snap-probe-socket' at 1920x1080...");
    let session = redfog_core::CompositorSession::spawn(
        redfog_core::SessionType::User("snap-probe".to_string()),
        "snap-probe-socket",
        1920,
        1080,
        1.0,
        60,
        &[],
    )
    .expect("spawn");

    eprintln!("resizing to 1602x1024 (odd width — expected to get snapped by KWin) ...");
    session.resize(1602, 1024);

    let vs = session.video_source(Some(redfog_core::VideoEncoder::NvencDirect));
    let redfog_core::VideoSource::KwinNativeDmaBuf { width: reported_width, height: reported_height, wayland_socket_path, .. } = vs else {
        panic!("expected KwinNativeDmaBuf video source");
    };
    eprintln!("CompositorSession reports (this is what the next encoder would be built for): {reported_width}x{reported_height}");
    assert_ne!(
        (reported_width, reported_height),
        (1602, 1024),
        "CompositorSession is reporting the raw *requested* resolution rather than what KWin actually \
         applied — this is the exact bug this test exists to catch, see this file's own doc comment"
    );

    // Ground truth: connect a real PipeWire capture and see what KWin
    // actually delivers, and confirm it agrees with what CompositorSession
    // just reported — not with the original 1602x1024 request.
    let capture = kwin_capture::pipewire_capture::PipewireCapture::start(
        session.pipewire_node_id,
        std::path::PathBuf::from(&wayland_socket_path),
        _headless_runtime.pipewire_socket.to_str().unwrap(),
        true,
    )
    .expect("PipewireCapture::start");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut last = None;
    while std::time::Instant::now() < deadline {
        if let Some(frame) = capture.next_frame() {
            last = Some((frame.width, frame.height));
            unsafe { libc::close(frame.fd) };
            if last == Some((reported_width, reported_height)) {
                break;
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    eprintln!("REAL captured resolution after resize(1602, 1024): {last:?} (CompositorSession reported {reported_width}x{reported_height})");
    assert_eq!(
        last,
        Some((reported_width, reported_height)),
        "CompositorSession's reported resolution doesn't match what the real capture stream delivers — \
         an encoder built from this would silently drop every frame forever"
    );
}
