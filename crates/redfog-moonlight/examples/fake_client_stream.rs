//! Fake Moonlight client: pairs, launches, then does the *real* RTSP
//! handshake + video/audio/control UDP streaming via the reference
//! implementation (`moonlight-common-rust`'s `MoonlightStream`), against a
//! live, already-running `redfog-server` (with a real KWin/plasmashell
//! session). This is the strongest validation of the post-pairing streaming
//! path — it exercises real client-side frame reassembly and RTP/FEC parsing,
//! not just "did some UDP bytes arrive".
//!
//! Usage: cargo run --example fake_client_stream -- [host] [pin]
//!
//! Requires a running `redfog-server` reachable at `host` on the standard
//! ports (47989/47984/48010/47998/47999/48000).

use std::sync::Arc;
use std::time::Duration;

use moonlight_common::crypto::rustcrypto::RustCryptoBackend;
use moonlight_common::high::tokio::MoonlightHost;
use moonlight_common::http::client::tokio_hyper::TokioHyperClient;
use moonlight_common::http::pair::PairPin;
use moonlight_common::http::{ClientIdentifier, ClientSecret};
use moonlight_common::stream::audio::AudioConfig;
use moonlight_common::stream::control::ActiveGamepads;
use moonlight_common::stream::tokio::MoonlightStream;
use moonlight_common::stream::video::{ColorRange, ColorSpace, VideoCapabilities, VideoFormats};
use moonlight_common::stream::{AesIv, AesKey, EncryptionFlags, MoonlightStreamSettings, StreamingConfig};

use redfog_moonlight::tls::ServerIdentity;

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt().with_env_filter("info").init();

    let mut args = std::env::args().skip(1);
    let host_addr = args.next().unwrap_or_else(|| "127.0.0.1".to_string());

    let client_identity = ServerIdentity::generate().expect("generate client identity");
    let client_identifier = ClientIdentifier::from_pem(pem::parse(&client_identity.cert_pem).unwrap());
    let client_secret = ClientSecret::from_pem(pem::parse(&client_identity.private_key_pem).unwrap());

    let host = MoonlightHost::<TokioHyperClient>::new(host_addr, 47989, Some("fake-stream-client".to_string()))
        .expect("construct MoonlightHost");

    let pin = PairPin::new_random(&RustCryptoBackend).expect("generate pin");
    println!("pairing with PIN {pin}...");
    println!("(submit via: curl -X POST http://<host>:47989/submit-pin -d uniqueid=fake-stream-client&pin={pin})");

    host.pair(&client_identifier, &client_secret, "fake-stream-client".to_string(), pin, RustCryptoBackend)
        .await
        .expect("pairing must succeed");
    println!("paired!");

    let mut settings = MoonlightStreamSettings {
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
    };

    let server_version = host.version().await.expect("server version");
    let gfe_version = host.gfe_version().await.expect("gfe version");
    let codec_support = host.server_codec_mode_support().await.expect("codec support");
    settings
        .adjust_for_server(server_version, &gfe_version, codec_support)
        .expect("settings must be compatible with the server");

    let aes_key = AesKey::new_random(&RustCryptoBackend).expect("random aes key");
    let aes_iv = AesIv(1);

    println!("launching Desktop (app_id=1)...");
    let stream_config = host
        .start_stream(1, &settings, aes_key, aes_iv, "")
        .await
        .expect("launch must succeed");

    println!("connecting stream (RTSP handshake + video/audio/control UDP)...");
    let video_capabilities = VideoCapabilities {
        reference_frame_invalidation_h264: false,
        reference_frame_invalidation_h265: false,
        reference_frame_invalidation_av1: false,
        pull_renderer: false,
        slices_per_frame: None,
    };
    let crypto_backend = Arc::new(RustCryptoBackend);
    let stream = MoonlightStream::connect(stream_config, settings, crypto_backend, video_capabilities)
        .await
        .expect("stream must connect");
    println!("stream connected! polling for 10s...");

    let mut video_frames = 0usize;
    let mut video_bytes = 0usize;
    let mut audio_frames = 0usize;
    let mut audio_bytes = 0usize;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            frame = stream.poll_video_frame() => {
                match frame {
                    Ok(frame) => {
                        video_frames += 1;
                        video_bytes += frame.raw().len();
                    }
                    Err(e) => { println!("video stream ended: {e}"); break; }
                }
            }
            frame = stream.poll_audio_frame() => {
                match frame {
                    Ok(frame) => {
                        audio_frames += 1;
                        audio_bytes += frame.buffer.len();
                    }
                    Err(e) => { println!("audio stream ended: {e}"); break; }
                }
            }
        }
    }

    println!("\n=== RESULTS ===");
    println!("video: {video_frames} frames, {video_bytes} bytes");
    println!("audio: {audio_frames} frames, {audio_bytes} bytes");

    println!("\nsending a mouse move through the real control channel...");
    stream
        .send_input(moonlight_common::stream::proto::control::input_batcher::ClientInputEvent::MouseMoveRelative {
            delta_x: 5,
            delta_y: 0,
        })
        .expect("send_input");
    tokio::time::sleep(Duration::from_millis(200)).await;
    println!("done.");
}
