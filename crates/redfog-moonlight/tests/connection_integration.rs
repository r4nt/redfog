//! Fully self-contained integration test using the real reference client
//! (`moonlight-common-rust`, GPL-3.0-or-later, dev-only): spawns an isolated
//! `redfog-server` subprocess (own ports, own runtime dir, own paired-client
//! state — never touches a real `redfog-server` that might already be
//! running on the default ports/runtime dir) and drives it through the exact
//! scenario that was broken and fixed earlier in this project: connect,
//! close the window without a clean disconnect, reconnect.
//!
//! Uses `glxgears` as the "Desktop" app instead of `plasmashell` — it
//! renders continuously, guaranteeing a steady stream of frames without
//! waiting on screen damage from anything else (an idle desktop only
//! redraws once a minute — see this project's KSplash/damage-source
//! investigation for why that matters for testing).
//!
//! Requires: `cargo build --workspace` first (spawns
//! `target/debug/redfog-server` directly rather than depending on it as a
//! crate, since that would be a dependency cycle).

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
use moonlight_common::stream::control::ActiveGamepads;
use moonlight_common::stream::proto::control::input_batcher::ClientInputEvent;
use moonlight_common::stream::tokio::MoonlightStream;
use moonlight_common::stream::video::{ColorRange, ColorSpace, VideoCapabilities, VideoFormats};
use moonlight_common::stream::{AesIv, AesKey, EncryptionFlags, MoonlightStreamSettings, StreamingConfig};

use redfog_moonlight::tls::ServerIdentity;

fn pick_free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn redfog_server_binary() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/redfog-moonlight; the workspace target
    // dir is two levels up. redfog-server can't be a dev-dependency of this
    // crate (it depends on redfog-moonlight itself), so there's no
    // CARGO_BIN_EXE_* env var for it — locate the binary directly instead.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/redfog-server");
    assert!(
        path.exists(),
        "redfog-server binary not found at {path:?} — run `cargo build --workspace` first"
    );
    path
}

/// Kills the whole process group on drop (the child is spawned as its own
/// group leader via `process_group(0)`), so the
/// dbus-run-session/pipewire/wireplumber/kwin_wayland/glxgears tree
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
    fn stdout_contains(&self, needle: &str) -> bool {
        self.stdout_lines.lock().unwrap().iter().any(|line| line.contains(needle))
    }
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

        let mut cmd = Command::new(redfog_server_binary());
        cmd.env("REDFOG_RUNTIME_DIR", &runtime_dir)
            .env("REDFOG_HTTP_PORT", http_port.to_string())
            .env("REDFOG_HTTPS_PORT", https_port.to_string())
            .env("REDFOG_RTSP_PORT", rtsp_port.to_string())
            .env("REDFOG_VIDEO_PORT", video_port.to_string())
            .env("REDFOG_CONTROL_PORT", control_port.to_string())
            .env("REDFOG_AUDIO_PORT", audio_port.to_string())
            .env("REDFOG_USER_APP", "glxgears")
            .env("RUST_LOG", "redfog_moonlight=debug,redfog_server=debug")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group so Drop can kill the whole tree (dbus-run-
            // session -> redfog-server -> pipewire/wireplumber/kwin_wayland
            // -> glxgears/redfog-login) with one signal.
            .process_group(0);

        let mut child = cmd.spawn().expect("spawn redfog-server");

        // Mirror both streams into this test's own output (visible on
        // failure / with `cargo test -- --nocapture`) and keep a copy of
        // stdout lines so assertions can grep it later (e.g. to confirm
        // mouse input actually reached the input forwarder).
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

/// Polls `stream` for up to `duration`, returning (video_frames, video_bytes,
/// audio_frames, audio_bytes).
async fn collect_frames(stream: &MoonlightStream, duration: Duration) -> (usize, usize, usize, usize) {
    let mut video_frames = 0usize;
    let mut video_bytes = 0usize;
    let mut audio_frames = 0usize;
    let mut audio_bytes = 0usize;

    let deadline = tokio::time::Instant::now() + duration;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            frame = stream.poll_video_frame() => {
                match frame {
                    Ok(frame) => {
                        video_frames += 1;
                        video_bytes += frame.raw().len();
                    }
                    Err(_) => break,
                }
            }
            frame = stream.poll_audio_frame() => {
                match frame {
                    Ok(frame) => {
                        audio_frames += 1;
                        audio_bytes += frame.buffer.len();
                    }
                    Err(_) => break,
                }
            }
        }
    }
    (video_frames, video_bytes, audio_frames, audio_bytes)
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

    // ---- First connection ----
    let stream_config = host
        .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
        .await
        .expect("first launch must succeed");
    let crypto_backend = Arc::new(RustCryptoBackend);
    let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
        .await
        .expect("first stream must connect");

    // The "Desktop" app starts on the Login compositor (redfog-login);
    // simulate a successful login via the same headless test-automation
    // trigger redfog-login itself supports, rather than driving its GUI.
    std::fs::write("/tmp/trigger-login", "").expect("write trigger-login");
    // Give the Login->User handoff (compositor teardown + glxgears spawn)
    // time to complete before expecting glxgears' frames specifically.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let (video_frames, video_bytes, audio_frames, _audio_bytes) = collect_frames(&stream, Duration::from_secs(5)).await;
    assert!(video_frames > 0, "expected glxgears video frames on the first connection, got {video_frames}");
    assert!(video_bytes > 0, "expected nonzero video bytes on the first connection");
    assert!(audio_frames > 0, "expected audio frames on the first connection, got {audio_frames}");

    // ---- Simulate closing the window: drop the stream without any clean
    // RTSP TEARDOWN / control-channel disconnect, exactly like a closed
    // browser tab. The server has no way to know this happened yet. ----
    drop(stream);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ---- Reconnect: a brand new stream_config/AES key, same client. This
    // is the retake path (server state is still `Streaming` from the
    // abandoned first connection) — the exact scenario that was broken
    // (stale queued PING misrouting the stream) and fixed. ----
    let stream_config = host
        .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
        .await
        .expect("reconnect launch must succeed");
    let stream = MoonlightStream::connect(stream_config, settings, crypto_backend, video_capabilities())
        .await
        .expect("reconnect stream must connect");

    let (video_frames, video_bytes, audio_frames, _audio_bytes) = collect_frames(&stream, Duration::from_secs(5)).await;
    assert!(video_frames > 0, "expected glxgears video frames after reconnect, got {video_frames} (this is the bug that was fixed: a stale queued PING from the abandoned connection misrouting the stream)");
    assert!(video_bytes > 0, "expected nonzero video bytes after reconnect");
    assert!(audio_frames > 0, "expected audio frames after reconnect, got {audio_frames}");

    // ---- Simulated client mouse movement over the reconnected control
    // channel — not just "the client's send didn't error", but that the
    // event actually reached our input forwarder server-side. ----
    stream
        .send_input(ClientInputEvent::MouseMoveRelative { delta_x: 5, delta_y: 0 })
        .expect("send_input must succeed on the reconnected control channel");
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        server.stdout_contains("forwarding MouseMoveRelative dx=5 dy=0"),
        "expected the server to log forwarding the simulated mouse move"
    );
}
