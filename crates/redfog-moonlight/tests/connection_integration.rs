//! Fully self-contained integration test using the real reference client
//! (`moonlight-common-rust`, GPL-3.0-or-later, dev-only): spawns an isolated
//! `redfog-server` subprocess (own ports, own runtime dir, own paired-client
//! state — never touches a real `redfog-server` that might already be
//! running on the default ports/runtime dir) and drives it through the exact
//! scenario that was broken and fixed earlier in this project: connect,
//! send input, hand off Login->User while streaming stays alive, close the
//! window without a clean disconnect, reconnect.
//!
//! Uses `redfog-test-ux` (a purpose-built stand-in, see that crate) as both
//! the Login and User stage instead of the real login GUI or an external app
//! like `glxgears` — it repaints continuously (guaranteeing frames without
//! waiting on screen damage from something else) and logs every mouse/key
//! event it receives to stdout in a format this test greps for, which is
//! direct proof input reached the session rather than just "the client's
//! send didn't error". It exits on 'Q', giving the test a deterministic way
//! to trigger the Login->User handoff instead of racing on the shared
//! global `/tmp/trigger-login` file `redfog-login` itself uses.
//!
//! Requires: `cargo build --workspace` first (spawns `target/debug/
//! redfog-server` and `target/debug/redfog-test-ux` directly rather than
//! depending on them as crates, since redfog-server depends on
//! redfog-moonlight, which would make it a dependency cycle).

use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonlight_common::crypto::rustcrypto::RustCryptoBackend;
use moonlight_common::high::tokio::MoonlightHost;
use moonlight_common::http::client::tokio_hyper::TokioHyperClient;
use moonlight_common::http::pair::PairPin;
use moonlight_common::http::{ClientIdentifier, ClientSecret};
use moonlight_common::stream::audio::AudioConfig;
use moonlight_common::stream::control::{
    ActiveGamepads, KeyAction, KeyCode, KeyFlags, KeyModifiers, MouseButton, MouseButtonAction,
};
use moonlight_common::stream::proto::control::input_batcher::ClientInputEvent;
use moonlight_common::stream::tokio::MoonlightStream;
use moonlight_common::stream::video::{ColorRange, ColorSpace, VideoCapabilities, VideoFormats};
use moonlight_common::stream::{AesIv, AesKey, EncryptionFlags, MoonlightStreamSettings, StreamingConfig};

use redfog_moonlight::tls::ServerIdentity;

/// Windows VK code for 'Q' — what a real client sends; our server's
/// `vk_to_evdev` translates it to the Linux evdev keycode KWin's
/// fake-input protocol expects.
const VK_Q: i16 = 0x51;

fn pick_free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn workspace_binary(name: &str) -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/redfog-moonlight; the workspace target
    // dir is two levels up. Neither redfog-server nor redfog-test-ux can be
    // a dev-dependency of this crate (redfog-server depends on
    // redfog-moonlight itself), so there's no CARGO_BIN_EXE_* env var for
    // them — locate the binaries directly instead.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../target/debug/{name}"));
    assert!(path.exists(), "{name} binary not found at {path:?} — run `cargo build --workspace` first");
    path
}

/// Kills the whole process group on drop (the child is spawned as its own
/// group leader via `process_group(0)`), so the
/// dbus-run-session/pipewire/wireplumber/kwin_wayland/redfog-test-ux tree
/// underneath doesn't leak past the test.
struct ServerProcess {
    child: Child,
    runtime_dir: PathBuf,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let pgid = nix::unistd::Pid::from_raw(-(self.child.id() as i32));
        let _ = nix::sys::signal::kill(pgid, nix::sys::signal::Signal::SIGTERM);
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
    }
}

struct TestServer {
    _process: ServerProcess,
    http_port: u16,
    stdout_lines: Arc<Mutex<Vec<String>>>,
}

impl TestServer {
    fn spawn() -> Self {
        let runtime_dir = std::env::temp_dir().join(format!("redfog-it-runtime-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&runtime_dir).unwrap();

        let http_port = pick_free_port();
        let https_port = pick_free_port();
        let rtsp_port = pick_free_port();
        let video_port = pick_free_port();
        let control_port = pick_free_port();
        let audio_port = pick_free_port();

        let test_ux = workspace_binary("redfog-test-ux");
        let test_ux = test_ux.to_str().unwrap();

        let mut cmd = Command::new(workspace_binary("redfog-server"));
        cmd.env("REDFOG_RUNTIME_DIR", &runtime_dir)
            .env("REDFOG_HTTP_PORT", http_port.to_string())
            .env("REDFOG_HTTPS_PORT", https_port.to_string())
            .env("REDFOG_RTSP_PORT", rtsp_port.to_string())
            .env("REDFOG_VIDEO_PORT", video_port.to_string())
            .env("REDFOG_CONTROL_PORT", control_port.to_string())
            .env("REDFOG_AUDIO_PORT", audio_port.to_string())
            .env("REDFOG_LOGIN_APP", test_ux)
            .env("REDFOG_USER_APP", test_ux)
            .env("RUST_LOG", "redfog_moonlight=debug,redfog_server=debug")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group so Drop can kill the whole tree (dbus-run-
            // session -> redfog-server -> pipewire/wireplumber/kwin_wayland
            // -> redfog-test-ux) with one signal.
            .process_group(0);

        let mut child = cmd.spawn().expect("spawn redfog-server");

        let stdout_lines = Arc::new(Mutex::new(Vec::<String>::new()));
        {
            let stdout = child.stdout.take().unwrap();
            let stdout_lines = stdout_lines.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    println!("[redfog-server] {line}");
                    stdout_lines.lock().unwrap().push(line);
                }
            });
        }
        {
            let stderr = child.stderr.take().unwrap();
            let stdout_lines = stdout_lines.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    eprintln!("[redfog-server] {line}");
                    stdout_lines.lock().unwrap().push(line);
                }
            });
        }

        // Wait for the HTTP server to actually accept connections rather
        // than sleeping a fixed guess — dbus-run-session + PipeWire bring-up
        // can take a couple of seconds.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if std::net::TcpStream::connect(("127.0.0.1", http_port)).is_ok() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "redfog-server never came up on port {http_port}");
            std::thread::sleep(Duration::from_millis(100));
        }

        TestServer {
            _process: ServerProcess { child, runtime_dir },
            http_port,
            stdout_lines,
        }
    }

    fn stdout_contains(&self, needle: &str) -> bool {
        self.stdout_lines.lock().unwrap().iter().any(|line| line.contains(needle))
    }

    fn count_stdout(&self, needle: &str) -> usize {
        self.stdout_lines.lock().unwrap().iter().filter(|line| line.contains(needle)).count()
    }

    async fn wait_for_stdout(&self, needle: &str, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        while !self.stdout_contains(needle) {
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {needle:?} in redfog-server's output");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Waits for `needle`'s occurrence count to exceed `baseline` — unlike
    /// `wait_for_stdout`, safe to reuse the same needle across multiple
    /// checkpoints in one test, since stdout is a growing, never-cleared
    /// log and a plain "does it appear" check would trivially pass on a
    /// stale match from earlier.
    async fn wait_for_new_stdout(&self, needle: &str, baseline: usize, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.count_stdout(needle) <= baseline {
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for a new {needle:?} in redfog-server's output");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn default_stream_settings() -> MoonlightStreamSettings {
    MoonlightStreamSettings {
        width: 1280,
        height: 720,
        fps: 60,
        fps_x100: 6000,
        bitrate: 10_000,
        packet_size: 1024,
        encryption_flags: EncryptionFlags::empty(),
        streaming_remotely: StreamingConfig::Local,
        sops: false,
        hdr: false,
        supported_video_formats: VideoFormats::H264,
        color_space: ColorSpace::Rec709,
        color_range: ColorRange::Limited,
        local_audio_play_mode: false,
        audio_config: AudioConfig::STEREO,
        gamepads_attached: ActiveGamepads::empty(),
        gamepads_persist_after_disconnect: false,
        enable_mic: false,
    }
}

fn video_capabilities() -> VideoCapabilities {
    VideoCapabilities {
        reference_frame_invalidation_h264: false,
        reference_frame_invalidation_h265: false,
        reference_frame_invalidation_av1: false,
        pull_renderer: false,
        slices_per_frame: None,
    }
}

/// `send_input` can fail with `NotConnected` if called before the control
/// channel's own ENet handshake (a handful of round trips, separate from
/// the RTSP handshake `MoonlightStream::connect` waits for) has finished —
/// confirmed live. Retry briefly instead of requiring callers to guess a
/// safe fixed delay.
async fn send_input_retrying(stream: &MoonlightStream, event: ClientInputEvent) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match stream.send_input(event.clone()) {
            Ok(()) => return,
            Err(e) if tokio::time::Instant::now() < deadline => {
                eprintln!("send_input not ready yet ({e}), retrying");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => panic!("send_input failed after retrying: {e}"),
        }
    }
}

async fn send_key(stream: &MoonlightStream, vk: i16, down: bool) {
    send_input_retrying(
        stream,
        ClientInputEvent::Keyboard {
            action: if down { KeyAction::Down } else { KeyAction::Up },
            flags: KeyFlags::empty(),
            key_code: KeyCode(vk),
            modifiers: KeyModifiers::empty(),
        },
    )
    .await;
}

/// Continuously polls `stream` for video/audio frames until `stop` resolves,
/// tracking the longest gap between consecutive video frames — used to
/// prove streaming never stalls across the Login->User handoff (the video-
/// continuity bug fixed earlier reset RTP state on every handoff and froze
/// the stream instead).
async fn poll_frames_tracking_gaps(stream: &MoonlightStream, stop: impl std::future::Future<Output = ()>) -> (usize, Duration) {
    tokio::pin!(stop);
    let mut video_frames = 0usize;
    let mut last_frame_at: Option<tokio::time::Instant> = None;
    let mut max_gap = Duration::ZERO;
    loop {
        tokio::select! {
            _ = &mut stop => break,
            frame = stream.poll_video_frame() => {
                if frame.is_err() {
                    break;
                }
                let now = tokio::time::Instant::now();
                if let Some(last) = last_frame_at {
                    max_gap = max_gap.max(now.duration_since(last));
                }
                last_frame_at = Some(now);
                video_frames += 1;
            }
            _ = stream.poll_audio_frame() => {}
        }
    }
    (video_frames, max_gap)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_client_connects_reconnects_and_sends_input() {
    // Our own rustls usage and moonlight-common's pull in different crypto
    // provider backends (ring vs aws-lc-rs); can't auto-pick one when both
    // are linked into the same test binary.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server = TestServer::spawn();

    let client_identity = ServerIdentity::generate().expect("generate client identity");
    let client_identifier = ClientIdentifier::from_pem(pem::parse(&client_identity.cert_pem).unwrap());
    let client_secret = ClientSecret::from_pem(pem::parse(&client_identity.private_key_pem).unwrap());

    let host = MoonlightHost::<TokioHyperClient>::new("127.0.0.1".to_string(), server.http_port, Some("it-client".to_string()))
        .expect("construct MoonlightHost");

    let pin = PairPin::new_random(&RustCryptoBackend).expect("generate pin");
    let pin_str = pin.to_string();
    let http_port = server.http_port;
    let submit_task = tokio::task::spawn_blocking(move || {
        std::thread::sleep(Duration::from_millis(300));
        ureq::post(&format!("http://127.0.0.1:{http_port}/submit-pin"))
            .send_form(&[("uniqueid", "it-client"), ("pin", &pin_str)])
            .expect("submit-pin request");
    });
    host.pair(&client_identifier, &client_secret, "connection-integration-test".to_string(), pin, RustCryptoBackend)
        .await
        .expect("pairing must succeed");
    submit_task.await.unwrap();

    let mut settings = default_stream_settings();
    let server_version = host.version().await.expect("server version");
    let gfe_version = host.gfe_version().await.expect("gfe version");
    let codec_support = host.server_codec_mode_support().await.expect("codec support");
    settings.adjust_for_server(server_version, &gfe_version, codec_support).expect("settings compatible");

    // ---- First connection: the Login stage. ----
    let stream_config = host
        .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
        .await
        .expect("first launch must succeed");
    let crypto_backend = Arc::new(RustCryptoBackend);
    let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
        .await
        .expect("first stream must connect");

    server.wait_for_stdout("TESTUX[redfog-login-0]: started", Duration::from_secs(10)).await;

    // ---- Simulated client mouse movement + key press, verified by proof
    // it actually reached the Login-stage session, not just that the
    // client's send didn't error. Absolute, targeting the window's likely
    // center — a small relative move from an unknown starting cursor
    // position may never land inside test-ux's (non-fullscreen) window at
    // all, so it'd never see the event even though the compositor correctly
    // received and forwarded it. ----
    send_input_retrying(
        &stream,
        ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
    )
    .await;
    server.wait_for_stdout("TESTUX[redfog-login-0]: pointer_moved", Duration::from_secs(5)).await;

    // A window only gets *keyboard* focus from a click, not just pointer
    // hover — confirmed live: sending a key press right after the mouse
    // move above (no click) reached fake_input and got forwarded
    // server-side, but test-ux never saw it.
    send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
    send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    send_key(&stream, VK_Q.wrapping_add(1) /* VK_R, an arbitrary non-exit key */, true).await;
    server.wait_for_stdout("TESTUX[redfog-login-0]: key_pressed", Duration::from_secs(5)).await;

    // ---- Confirm streaming works before touching the handoff, and start
    // tracking frame gaps from here through the handoff below. ----
    let (login_frames, _) = poll_frames_tracking_gaps(&stream, tokio::time::sleep(Duration::from_secs(2))).await;
    assert!(login_frames > 0, "expected video frames from the Login-stage test UX");

    // ---- Trigger the Login->User handoff deterministically (redfog-test-ux
    // exits on 'Q', same `--exit-with-session` trigger a real login success
    // uses) while continuously polling frames, proving the stream survives
    // the handoff instead of stalling — the exact bug fixed earlier, where
    // resetting RTP sequence/frame-index state on every compositor handoff
    // froze the video permanently. ----
    send_key(&stream, VK_Q, true).await;
    send_key(&stream, VK_Q, false).await;
    let (handoff_frames, max_gap) = poll_frames_tracking_gaps(&stream, async {
        server.wait_for_stdout("TESTUX[redfog-user-0]: started", Duration::from_secs(15)).await;
        // A little settle time after the User stage starts, so the next
        // input-verification step lands on a session that's fully up.
        tokio::time::sleep(Duration::from_millis(500)).await;
    })
    .await;
    assert!(handoff_frames > 0, "expected video frames to keep flowing across the Login->User handoff");
    assert!(
        max_gap < Duration::from_secs(3),
        "video stalled for {max_gap:?} across the handoff — the video-continuity bug is back"
    );

    // ---- Confirm input reaches the new, User-stage session too. Absolute,
    // not relative — this is a brand new KWin compositor instance (a
    // separate process from the Login stage's), so the cursor's position
    // here is unknown/undefined, same reasoning as the Login-stage move
    // above. ----
    send_input_retrying(
        &stream,
        ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
    )
    .await;
    server.wait_for_stdout("TESTUX[redfog-user-0]: pointer_moved", Duration::from_secs(5)).await;

    // ---- Simulate closing the window: drop the stream without any clean
    // RTSP TEARDOWN / control-channel disconnect, exactly like a closed
    // browser tab. The server has no way to know this happened yet. ----
    drop(stream);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ---- Reconnect: a brand new stream_config/AES key, same client. This
    // is the retake path (server state is still `Streaming` on the User
    // stage from the abandoned first connection) — the exact scenario that
    // was broken (stale queued PING misrouting the stream, and the new
    // peer's own control connection getting caught by its own stale-peer
    // disconnect sweep) and fixed. ----
    let stream_config = host
        .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
        .await
        .expect("reconnect launch must succeed");
    let stream = MoonlightStream::connect(stream_config, settings, crypto_backend, video_capabilities())
        .await
        .expect("reconnect stream must connect");

    let (video_frames, _) = poll_frames_tracking_gaps(&stream, tokio::time::sleep(Duration::from_secs(5))).await;
    assert!(video_frames > 0, "expected video frames after reconnect, got {video_frames} (this is the bug that was fixed)");

    // ---- Input must still work after reconnect (validates the generation-
    // based stale-peer disconnect fix in the control channel). Uses
    // wait_for_new_stdout, not wait_for_stdout — "TESTUX[redfog-user-0]: pointer_moved"
    // already appeared once above (pre-reconnect), and stdout is a growing
    // log, so a plain "does it appear" check would trivially pass without
    // this actually proving anything new arrived.
    let pointer_moved_before_reconnect = server.count_stdout("TESTUX[redfog-user-0]: pointer_moved");
    send_input_retrying(&stream, ClientInputEvent::MouseMoveRelative { delta_x: 9, delta_y: 0 }).await;
    server
        .wait_for_new_stdout("TESTUX[redfog-user-0]: pointer_moved", pointer_moved_before_reconnect, Duration::from_secs(5))
        .await;
}
