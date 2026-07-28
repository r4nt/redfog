//! Isolates one variable from the "Login stage fails to open an NVENC
//! session while a backgrounded User session is still running" live bug:
//! does opening a *second* NVENC session from the *same process* fail, even
//! when the driver clearly supports multiple concurrent sessions overall
//! (confirmed live: 3 concurrent `ffmpeg -c:v h264_nvenc` processes run fine
//! simultaneously — but each is a separate OS process, unlike redfog-server,
//! which tries to hold two sessions itself: one via
//! `kwin_capture::nvenc_session::CudaDirectEncoderSession`, one via
//! GStreamer's `nvh264enc` for the new Login stage).
//!
//! No KWin/PipeWire/GStreamer at all — just two independent
//! `Encoder::initialize_with_cuda` + `start_session` calls in this one
//! process, each against the primary CUDA context for device 0 (same as
//! both real consumers use), each encoding a few synthetic black frames.

use nvidia_video_codec_sdk::sys::nvEncodeAPI::{
    NV_ENC_BUFFER_FORMAT, NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P4_GUID, NV_ENC_TUNING_INFO,
};
use nvidia_video_codec_sdk::{EncodePictureParams, Encoder, EncoderInitParams};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const DATA_LEN: usize = (WIDTH * HEIGHT * 4) as usize;

/// Returns `false` (having already printed a skip message) if there's no
/// CUDA-capable GPU on this machine, so callers can skip gracefully instead
/// of hard-failing — this whole file otherwise can't run at all in CI/on
/// hardware without an NVIDIA GPU (e.g. GH Actions' hosted runners), unlike
/// the `nvh264enc`-availability check `nvenc_session_plus_gstreamer_nvh264enc`
/// already does below.
fn open_session_and_encode_a_frame(label: &str) -> bool {
    eprintln!("[{label}] retaining primary CUDA context for device 0...");
    // `cudarc016`, matching `nvidia-video-codec-sdk`'s own (semver-
    // incompatible) cudarc dependency — see `cuda_import.rs`'s doc comment.
    let cuda_ctx = match cudarc016::driver::CudaContext::new(0) {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("[{label}] no CUDA-capable GPU available ({e}) — skipping");
            return false;
        }
    };

    eprintln!("[{label}] Encoder::initialize_with_cuda...");
    let encoder = Encoder::initialize_with_cuda(cuda_ctx).unwrap_or_else(|e| {
        panic!("[{label}] Encoder::initialize_with_cuda failed: {e:?}");
    });

    let buffer_format = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
    let mut init_params = EncoderInitParams::new(NV_ENC_CODEC_H264_GUID, WIDTH, HEIGHT);
    init_params
        .preset_guid(NV_ENC_PRESET_P4_GUID)
        .tuning_info(NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY)
        .framerate(30, 1)
        .enable_picture_type_decision();

    eprintln!("[{label}] start_session (this is the call that fails live: NvEncOpenEncodeSessionEx)...");
    let session = encoder.start_session(buffer_format, init_params).unwrap_or_else(|e| {
        panic!("[{label}] start_session (NvEncOpenEncodeSessionEx) failed: {e:?}");
    });
    eprintln!("[{label}] session opened successfully.");

    let mut input_buffer = session.create_input_buffer().expect("create_input_buffer");
    let mut output_bitstream = session.create_output_bitstream().expect("create_output_bitstream");
    unsafe { input_buffer.lock().unwrap().write(&[0u8; DATA_LEN]) };
    session
        .encode_picture(&mut input_buffer, &mut output_bitstream, EncodePictureParams::default())
        .unwrap_or_else(|e| panic!("[{label}] encode_picture failed: {e:?}"));
    let data_len = output_bitstream.lock().unwrap().data().len();
    eprintln!("[{label}] encoded {data_len} bytes successfully. Holding session open...");

    // Deliberately leak everything (never call end_of_stream/Drop) so the
    // caller controls how long this session stays open, matching how a
    // backgrounded `CudaDirectEncoderSession` stays open indefinitely. Order
    // matters: `input_buffer`/`output_bitstream` borrow from `session`
    // (their `Drop` impls use it), so they must be forgotten first.
    std::mem::forget(input_buffer);
    std::mem::forget(output_bitstream);
    std::mem::forget(session);
    true
}

#[test]
fn two_nvenc_sessions_same_process() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    if !open_session_and_encode_a_frame("session-1") {
        return;
    }
    assert!(open_session_and_encode_a_frame("session-2"), "second session failed to open even though the first one succeeded");
    eprintln!("Both sessions opened and encoded successfully in the same process.");
}

/// The variable the test above *doesn't* isolate: the live failure is our
/// own raw `nvidia-video-codec-sdk` session plus GStreamer's own `nvh264enc`
/// (which may create its own, separate CUDA context rather than sharing the
/// primary one) — not two raw sessions both using the primary context.
#[test]
fn nvenc_session_plus_gstreamer_nvh264enc() {
    use gstreamer::prelude::*;

    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    if !open_session_and_encode_a_frame("our-own-session") {
        return;
    }
    eprintln!("Our own session is open and held (leaked). Now starting a real GStreamer nvh264enc pipeline...");

    gstreamer::init().unwrap();
    if gstreamer::ElementFactory::find("nvh264enc").is_none() {
        eprintln!("nvh264enc not available on this machine — skipping");
        return;
    }
    let pipeline = gstreamer::parse_launch(
        "videotestsrc num-buffers=10 ! video/x-raw,width=1280,height=720,framerate=30/1 ! nvh264enc ! fakesink",
    )
    .expect("parse_launch")
    .downcast::<gstreamer::Pipeline>()
    .unwrap();

    pipeline.set_state(gstreamer::State::Playing).expect("set_state(Playing)");
    let bus = pipeline.bus().unwrap();
    let mut got_error = None;
    let mut got_eos = false;
    for msg in bus.iter_timed(gstreamer::ClockTime::from_seconds(10)) {
        match msg.view() {
            gstreamer::MessageView::Eos(_) => {
                got_eos = true;
                break;
            }
            gstreamer::MessageView::Error(err) => {
                got_error = Some(format!("{} ({:?})", err.error(), err.debug()));
                break;
            }
            _ => {}
        }
    }
    pipeline.set_state(gstreamer::State::Null).ok();

    if let Some(err) = got_error {
        panic!(
            "GStreamer nvh264enc pipeline FAILED while our own raw NVENC session was open: {err}\n\
             This confirms the conflict is specifically between our session and GStreamer's own \
             CUDA/NVENC context creation, not a generic same-process/session-count limit."
        );
    }
    assert!(got_eos, "pipeline did not reach EOS within 10s (and no error was reported either)");
    eprintln!("GStreamer nvh264enc pipeline succeeded while our own raw NVENC session was open.");
}
