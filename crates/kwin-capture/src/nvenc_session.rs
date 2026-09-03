//! Production wrapper around the DMA-BUF -> CUDA -> NVENC path validated by
//! `tests/cuda_direct_nvenc.rs`: a background thread that captures via
//! `PipewireCapture`, imports each frame into CUDA (directly as a tiled
//! array on Ampere+, or via `vulkan_bridge`'s detile-to-linear-buffer bridge
//! on older GPUs — see `cuda_import`'s doc comment), and drives NVENC
//! directly, with no GStreamer involved in the video leg at all.
//!
//! Picture-type decision (`enablePTD`) is left *off* for both codecs — we
//! own the I/P cadence ourselves (an IDR every `gop_length` frames or on
//! request, P otherwise) — the same standard pattern GStreamer's
//! `nvh264enc`/PTD path replaces, just done by hand. With PTD off,
//! [`CudaDirectEncoderSession::request_keyframe`] is a true in-place
//! operation for both codecs: the *next* captured frame just gets encoded
//! as `IDR` instead of `P` on the same, already-running session — no
//! rebuild, ~15ms end to end in practice (dominated by waiting for that
//! next frame at 60fps), not the 200+ms a full session rebuild costs.
//!
//! HEVC needs one thing H.264 doesn't for this to work at all: manually
//! setting `pictureType = P` used to make NVENC reject the very next
//! `encode_picture` call outright (`EncodeError { kind: Generic }`, no
//! further detail) on this GPU/driver — confirmed live this wasn't a
//! resource/registration bug, and briefly worked around by leaving PTD *on*
//! for HEVC only (costing `request_keyframe` entirely, and adding real
//! per-rebuild latency/CPU cost to keyframe recovery). Root-caused properly
//! by reading NVIDIA's own header docs for `NV_ENC_PIC_PARAMS_HEVC`:
//! `displayPOCSyntax` ("required to be set if client is handling the
//! picture type decision") and `refPicFlag` ("ignored if enablePTD is set
//! to 1" — i.e. it matters precisely when we need it to) were both being
//! left at their zeroed `EncodePictureParams::default()` value on every
//! frame, `codec_params: None`. NVENC was very likely rejecting the second
//! frame's `encode_picture` because its display POC (stuck at a constant
//! 0) failed a monotonicity check against the DPB state — the manually-
//! forced *first* IDR frame never triggers that check, only the first
//! P-frame after it, exactly matching what was observed. Supplying a
//! per-session POC counter (reset to 0 at every IDR, incremented per P-frame
//! — `hevc_poc` in the loop below) and `refPicFlag: 1` for HEVC's
//! `codec_params` fixed it outright: confirmed live surviving 60+
//! consecutive frames with manual picture typing, no PTD needed after all.
//! H.264's own per-picture params have no equivalent requirement — `None`
//! there is correct, not something this fix needed to touch.
//!
//! True in-place bitrate changes aren't implemented: `nvidia-video-codec-sdk`
//! 0.4 doesn't expose `NvEncReconfigureEncoder` through its safe API (the
//! `Encoder`/`Session` structs' raw pointer field is `pub(crate)`), so
//! [`CudaDirectEncoderSession::set_bitrate`] (the high-frequency adaptive-
//! bitrate hook) is still a logged no-op. [`CudaDirectEncoderSession::
//! reconfigure`] gets a coarser-grained substitute for the low-frequency,
//! reconnect-triggered case instead: rebuild the whole NVENC session (a
//! forced keyframe, same cost as a resolution change) while reusing the
//! existing `PipewireCapture` connection — see its own doc comment for why
//! that split matters (a real leak, not just an optimization).

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use nvidia_video_codec_sdk::sys::nvEncodeAPI::{
    NV_ENC_BUFFER_FORMAT,
    NV_ENC_CODEC_H264_GUID,
    NV_ENC_CODEC_HEVC_GUID,
    NV_ENC_INPUT_RESOURCE_TYPE,
    NV_ENC_PARAMS_RC_MODE,
    NV_ENC_PIC_PARAMS_HEVC,
    NV_ENC_PIC_TYPE,
    NV_ENC_PRESET_P4_GUID,
    NV_ENC_TUNING_INFO,
};
use nvidia_video_codec_sdk::{CodecPictureParams, EncodePictureParams, Encoder, EncoderInitParams, EncoderInput, RegisteredResource};

use crate::cuda_import::{CudaImporter, ImportedArray, ImportedLinear};
use crate::pipewire_capture::PipewireCapture;
use crate::vulkan_bridge::{BridgedImage, VulkanBridge};

const GOP_LENGTH: u32 = 300;

/// Which codec NVENC actually produces. Deliberately just the two variants
/// this GPU generation (Turing) can encode at all — `encode_guids` is
/// checked live against whichever this resolves to, so an unsupported
/// choice fails with a clear error rather than silently encoding the wrong
/// thing. A third `Av1` variant would extend `codec_guid`/the `run()` match
/// below the same way once it's actually needed on hardware that supports
/// it (Ada Lovelace+) — not added speculatively now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoCodec {
    #[default]
    H264,
    Hevc,
}

impl VideoCodec {
    fn guid(self) -> nvidia_video_codec_sdk::sys::nvEncodeAPI::GUID {
        match self {
            VideoCodec::H264 => NV_ENC_CODEC_H264_GUID,
            VideoCodec::Hevc => NV_ENC_CODEC_HEVC_GUID,
        }
    }
}

/// fps/bitrate/codec, requested via [`CudaDirectEncoderSession::reconfigure`]
/// — deliberately NOT width/height. NVENC (like any hardware encoder) needs
/// a brand-new session for any of these, same as a resolution change would,
/// but *unlike* a resolution change, none of them require the underlying
/// [`PipewireCapture`] connection to change at all — the capture stream
/// doesn't care what bitrate the encoder downstream of it uses. Reusing the
/// same capture connection instead of tearing it down and reconnecting is
/// the entire point: see `run`'s doc comment for the leak this avoids. A
/// resolution change still goes through the full
/// [`CudaDirectEncoderSession::spawn`] path (a new capture connection is
/// unavoidable there — the compositor's own output size changed).
struct PendingReconfig {
    fps: u32,
    bitrate_kbps: u32,
    codec: VideoCodec,
}

enum RegisteredFrame<'a> {
    Array(RegisteredResource<'a, ImportedArray>),
    /// `BridgedImage` alongside the NVENC registration (not just the
    /// registration alone, unlike `Array`): re-copied via
    /// `VulkanBridge::refresh` on *every* frame, not just when this
    /// `buffer_identity` is first seen — see `vulkan_bridge`'s doc comment
    /// for why the one-time setup alone isn't enough on this path.
    Linear(RegisteredResource<'a, ImportedLinear>, BridgedImage),
}

impl EncoderInput for RegisteredFrame<'_> {
    fn pitch(&self) -> u32 {
        match self {
            RegisteredFrame::Array(r) => r.pitch(),
            RegisteredFrame::Linear(r, _) => r.pitch(),
        }
    }

    fn handle(&mut self) -> *mut c_void {
        match self {
            RegisteredFrame::Array(r) => r.handle(),
            RegisteredFrame::Linear(r, _) => r.handle(),
        }
    }
}

/// A running direct-NVENC encode session for one [`crate::pipewire_capture`]
/// stream. Dropping this signals the background thread to stop and joins
/// it — no explicit shutdown call needed.
pub struct CudaDirectEncoderSession {
    shutdown: Arc<AtomicBool>,
    force_keyframe: Arc<AtomicBool>,
    reconfig: Arc<Mutex<Option<PendingReconfig>>>,
    thread: Option<JoinHandle<()>>,
}

impl CudaDirectEncoderSession {
    /// Returns immediately — the actual capture/CUDA/NVENC setup (and
    /// anything that can fail: no CUDA device, driver too old, ...) all
    /// happens on the spawned background thread, isolated from the caller.
    /// A failure there just logs and lets the thread exit; it degrades to
    /// "no video for this session" rather than propagating a panic/error
    /// back to whoever called `spawn`. This matters now that
    /// `detect_video_encoder` defaults to this path whenever `nvh264enc` is
    /// merely *registered* (not a real capability check — see its own doc
    /// comment) — a box where that registration is stale/unhealthy fails
    /// safe here instead of taking the process down.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        node_id: u32,
        wayland_socket_path: PathBuf,
        pipewire_socket_path: String,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        codec: VideoCodec,
        // Third argument: when `capture.next_frame()` returned the frame
        // this access unit was encoded from -- lets the caller measure real
        // end-to-end latency (capture -> encode -> packetize -> actually
        // sent), not just encode time.
        on_access_unit: impl Fn(Vec<u8>, bool, std::time::Instant) + Send + Sync + 'static,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let force_keyframe = Arc::new(AtomicBool::new(false));
        let reconfig = Arc::new(Mutex::new(None));
        let thread_shutdown = shutdown.clone();
        let thread_force_keyframe = force_keyframe.clone();
        let thread_reconfig = reconfig.clone();
        let thread = std::thread::spawn(move || {
            if let Err(e) = run(
                node_id,
                wayland_socket_path,
                &pipewire_socket_path,
                width,
                height,
                fps,
                bitrate_kbps,
                codec,
                &thread_shutdown,
                &thread_force_keyframe,
                &thread_reconfig,
                &on_access_unit,
            ) {
                eprintln!("kwin-capture: CudaDirectEncoderSession thread exiting with error: {e}");
            }
        });
        Self { shutdown, force_keyframe, reconfig, thread: Some(thread) }
    }

    /// See [`redfog_core::request_keyframe`]'s doc comment — same purpose
    /// (Moonlight's `RequestIdrFrame`/`InvalidateReferenceFrames`), just a
    /// flag the encode loop checks itself instead of a `GstForceKeyUnitEvent`.
    /// A true in-place operation for both codecs — see this module's doc
    /// comment.
    pub fn request_keyframe(&self) {
        self.force_keyframe.store(true, Ordering::SeqCst);
    }

    /// Not implemented yet — see this module's doc comment for why. Kept
    /// distinct from [`Self::reconfigure`], which *is* implemented: this one
    /// is the high-frequency adaptive-bitrate hook (`on_loss_stats`, capable
    /// of firing every loss report), where a full NVENC session rebuild per
    /// call would mean a fresh forced keyframe (far larger than a P-frame)
    /// on every small adjustment — not what that mechanism wants.
    /// `reconfigure` is the coarser, reconnect-triggered path instead, where
    /// a rebuild (and the one keyframe it costs) is expected and rare.
    pub fn set_bitrate(&self, bitrate_kbps: u32) {
        eprintln!(
            "kwin-capture: CudaDirectEncoderSession::set_bitrate({bitrate_kbps}) ignored — \
             live bitrate reconfiguration isn't supported yet on the CUDA-direct encode path"
        );
    }

    /// Rebuilds the NVENC encoder in place — new fps/bitrate/codec — while
    /// leaving the underlying [`PipewireCapture`] connection completely
    /// untouched. Applied asynchronously, on the background thread's own
    /// next loop iteration (same "fire and forget" shape as `spawn` itself
    /// already has) — no resolution change is possible through this path,
    /// see `PendingReconfig`'s doc comment for why. Overwrites any
    /// previously-requested-but-not-yet-applied reconfigure.
    pub fn reconfigure(&self, fps: u32, bitrate_kbps: u32, codec: VideoCodec) {
        *self.reconfig.lock().unwrap() = Some(PendingReconfig { fps, bitrate_kbps, codec });
    }
}

impl Drop for CudaDirectEncoderSession {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Owns the [`PipewireCapture`] connection for this session's whole
/// lifetime — created once, reused across every `reconfigure` call — and
/// drives an inner encoder loop ([`run_encoder`]) that gets torn down and
/// rebuilt in place whenever a `PendingReconfig` arrives, or exits for good
/// on `shutdown`.
///
/// Splitting it this way (rather than one flat loop that rebuilds
/// *everything*, capture included, on any parameter change) is what fixes a
/// real leak: repeatedly reconnecting to the compositor's PipeWire daemon
/// leaked two sockets every time (a fresh D-Bus system-bus connection plus
/// the PipeWire connection itself, per `ss -xp` — not from anything in this
/// crate's own EGL/Vulkan code, both already ruled out and fixed
/// separately; most likely PipeWire's own client library or wireplumber
/// doing DRM device authorization via `logind` on every new stream
/// connection). A bitrate/fps/codec-only change — the common case for
/// `SessionManager::reconcile_video_pipeline`, e.g. a different client
/// connecting with a different requested bitrate — never needs a new
/// capture connection at all; only resolution changes still do (see
/// `PendingReconfig`'s doc comment), and those already go through a fresh
/// `CudaDirectEncoderSession::spawn` instead of `reconfigure`.
#[allow(clippy::too_many_arguments)]
fn run(
    node_id: u32,
    wayland_socket_path: PathBuf,
    pipewire_socket_path: &str,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    codec: VideoCodec,
    shutdown: &AtomicBool,
    force_keyframe: &AtomicBool,
    reconfig: &Mutex<Option<PendingReconfig>>,
    on_access_unit: &(impl Fn(Vec<u8>, bool, std::time::Instant) + Send + Sync + 'static),
) -> Result<(), String> {
    let capture = PipewireCapture::start(node_id, wayland_socket_path, pipewire_socket_path, false)
        .map_err(|e| format!("PipewireCapture::start: {e}"))?;

    let mut fps = fps;
    let mut bitrate_kbps = bitrate_kbps;
    let mut codec = codec;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(());
        }
        match run_encoder(&capture, width, height, fps, bitrate_kbps, codec, shutdown, force_keyframe, reconfig, on_access_unit)? {
            EncoderOutcome::Shutdown => return Ok(()),
            EncoderOutcome::Reconfigure(new) => {
                eprintln!("kwin-capture: reconfiguring encoder in place (fps={} bitrate={}kbps codec={:?}) — capture connection untouched", new.fps, new.bitrate_kbps, new.codec);
                fps = new.fps;
                bitrate_kbps = new.bitrate_kbps;
                codec = new.codec;
            }
        }
    }
}

enum EncoderOutcome {
    Shutdown,
    Reconfigure(PendingReconfig),
}

/// One NVENC session's whole lifetime: builds it fresh, then feeds it
/// frames from the *already-connected* `capture` until either `shutdown` or
/// a `reconfig` request ends this particular encoder (not the capture
/// connection, which outlives every call to this function — see `run`'s
/// doc comment). All NVENC/CUDA-import state below is local to one call:
/// returning drops it all, so the caller rebuilding fresh on the next call
/// is a completely clean start, same as it always was per-`run` before this
/// was split out — just without paying for a new capture connection too.
#[allow(clippy::too_many_arguments)]
fn run_encoder(
    capture: &PipewireCapture,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    codec: VideoCodec,
    shutdown: &AtomicBool,
    force_keyframe: &AtomicBool,
    reconfig: &Mutex<Option<PendingReconfig>>,
    on_access_unit: &(impl Fn(Vec<u8>, bool, std::time::Instant) + Send + Sync + 'static),
) -> Result<EncoderOutcome, String> {
    let importer = CudaImporter::new().map_err(|e| format!("CudaImporter::new: {e:?}"))?;
    let use_array_import = importer.dma_buf_array_import_supported().unwrap_or(false);
    eprintln!(
        "kwin-capture: CudaDirectEncoderSession using {} import path (CU_DEVICE_ATTRIBUTE_DMA_BUF_SUPPORTED={use_array_import})",
        if use_array_import { "direct tiled-array" } else { "Vulkan-bridge linear" }
    );
    let vulkan_bridge = if use_array_import { None } else { Some(VulkanBridge::shared()?) };

    // A separate cudarc line from `CudaImporter`'s (see cuda_import.rs's doc
    // comment) — both retain the same refcounted primary context for device 0.
    let nvenc_cuda_ctx =
        cudarc016::driver::CudaContext::new(0).map_err(|e| format!("CudaContext::new(0) for NVENC: {e:?}"))?;
    let encoder =
        Encoder::initialize_with_cuda(nvenc_cuda_ctx).map_err(|e| format!("Encoder::initialize_with_cuda: {e:?}"))?;

    let codec_guid = codec.guid();
    let encode_guids = encoder.get_encode_guids().map_err(|e| format!("get_encode_guids: {e:?}"))?;
    if !encode_guids.contains(&codec_guid) {
        return Err(format!("NVENC on this machine doesn't support {codec:?}"));
    }
    let buffer_format = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
    let input_formats = encoder
        .get_supported_input_formats(codec_guid)
        .map_err(|e| format!("get_supported_input_formats: {e:?}"))?;
    if !input_formats.contains(&buffer_format) {
        return Err(format!("NVENC doesn't support ARGB input for {codec:?} on this machine"));
    }

    let mut preset_config = encoder
        .get_preset_config(codec_guid, NV_ENC_PRESET_P4_GUID, NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY)
        .map_err(|e| format!("get_preset_config: {e:?}"))?;
    {
        let config = &mut preset_config.presetCfg;
        config.gopLength = GOP_LENGTH;
        config.frameIntervalP = 1; // no B-frames: lowest latency
        config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
        config.rcParams.averageBitRate = bitrate_kbps * 1000;
        config.rcParams.maxBitRate = bitrate_kbps * 1000;
        // Spatial adaptive quantization: redistributes bits *within* each
        // frame toward detailed/high-motion regions instead of spreading
        // them evenly, at the same CBR average bitrate — doesn't touch
        // preset/tuning/GOP, so it costs no extra latency, just NVENC's own
        // (fixed-function, not host-CPU) per-frame analysis. `aqStrength`
        // deliberately left unset (0 = auto) — NVIDIA's own recommended
        // default rather than a guessed fixed value.
        config.rcParams.set_enableAQ(1);
        // repeatSPSPPS lives on a codec-specific union member (h264Config vs
        // hevcConfig) — same field name/semantics on both, just a different
        // struct.
        match codec {
            VideoCodec::H264 => unsafe { config.encodeCodecConfig.h264Config.set_repeatSPSPPS(1) },
            VideoCodec::Hevc => unsafe { config.encodeCodecConfig.hevcConfig.set_repeatSPSPPS(1) },
        }
    }

    let mut init_params = EncoderInitParams::new(codec_guid, width, height);
    init_params
        .preset_guid(NV_ENC_PRESET_P4_GUID)
        .tuning_info(NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY)
        .framerate(fps, 1)
        // deliberately not calling enable_picture_type_decision() — see this
        // module's doc comment for why we manage picture type ourselves for
        // both codecs, and why HEVC additionally needs displayPOCSyntax/
        // refPicFlag supplied per frame to do that (H.264's own per-picture
        // params tolerate defaults fine, hence the codec split below).
        .encode_config(&mut preset_config.presetCfg);

    let session = encoder
        .start_session(buffer_format, init_params)
        .map_err(|e| format!("start_session: {e:?}"))?;
    let mut bitstream = session.create_output_bitstream().map_err(|e| format!("create_output_bitstream: {e:?}"))?;

    let mut registered: HashMap<i64, RegisteredFrame<'_>> = HashMap::new();
    let mut frame_index: u64 = 0;
    // HEVC-only — see this module's doc comment: NVENC requires a
    // client-supplied, monotonically-increasing display POC per frame when
    // handling picture type decisions manually, resetting at every IDR
    // (standard video-coding convention: an IDR flushes the reference
    // picture buffer, so POC continuity across one has no meaning).
    let mut hevc_poc: u32 = 0;

    // Diagnostic-only: the *actual* delivered/encoded frame rate,
    // independent of the `fps` parameter above (which only feeds NVENC's
    // own rate-control math via `.framerate()` a few lines up — this loop
    // itself has no pacing/throttle beyond the "no frame yet" sleep below,
    // so it encodes every frame PipeWire/KWin's damage-driven output
    // actually delivers, whatever that real rate turns out to be). Logged
    // once a second so a live session can be checked directly against the
    // client-requested fps, without needing an external profiler.
    let mut fps_window_start = std::time::Instant::now();
    let mut fps_window_count: u32 = 0;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(EncoderOutcome::Shutdown);
        }
        if let Some(new) = reconfig.lock().unwrap().take() {
            return Ok(EncoderOutcome::Reconfigure(new));
        }
        let Some(frame) = capture.next_frame() else {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        };
        // Captured as early as possible after `next_frame()` returns — the
        // "capture" end of the end-to-end (capture -> encode -> packetize
        // -> actually sent) latency the caller measures.
        let capture_instant = std::time::Instant::now();
        if !frame.is_dma_buf {
            unsafe { libc::close(frame.fd) };
            continue;
        }
        if frame.width != width || frame.height != height {
            // A compositor resize (`SessionManager::reconcile_video_pipeline`'s
            // resolution-change path) is asynchronous: PipeWire can still
            // deliver a frame or two at the old resolution, or at an
            // intermediate size from a mid-transition renegotiation, after
            // this encoder was already spawned for the new target
            // `width`/`height` above — NVENC's init params are fixed for
            // this whole `run_encoder` call, so registering a differently-
            // sized buffer against it fails. That used to be fatal (the
            // failure propagated out via `?` and ended the whole background
            // thread, killing video for the rest of the session); dropping
            // the mismatched frame and waiting for capture to settle on the
            // resolution this encoder actually expects costs at most a
            // couple of frames during a resize, which is a non-issue.
            eprintln!(
                "kwin-capture: dropping frame at {}x{} — waiting for capture to settle on {width}x{height} after a resolution change",
                frame.width, frame.height
            );
            unsafe { libc::close(frame.fd) };
            continue;
        }

        if let std::collections::hash_map::Entry::Vacant(entry) = registered.entry(frame.buffer_identity) {
            let resource = if use_array_import {
                let mut stat: libc::stat = unsafe { std::mem::zeroed() };
                let rc = unsafe { libc::fstat(frame.fd, &mut stat) };
                if rc != 0 {
                    unsafe { libc::close(frame.fd) };
                    return Err("fstat on dma-buf fd failed".to_string());
                }
                let size = stat.st_size as u64;
                let imported = unsafe {
                    importer
                        .import_array(frame.fd, size, frame.width as usize, frame.height as usize, 4)
                        .map_err(|e| format!("cuExternalMemoryGetMappedMipmappedArray: {e:?}"))?
                };
                let array_ptr = imported.array() as *mut c_void;
                let r = session
                    .register_generic_resource(
                        imported,
                        NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDAARRAY,
                        array_ptr,
                        frame.stride as u32,
                    )
                    .map_err(|e| format!("NVENC array resource registration: {e:?}"))?;
                RegisteredFrame::Array(r)
            } else {
                let linear_size = (frame.width as u64) * (frame.height as u64) * 4;
                let (bridged, linear_fd) = unsafe {
                    vulkan_bridge
                        .unwrap()
                        .lock()
                        .unwrap()
                        .import_persistent(
                            frame.fd,
                            frame.width,
                            frame.height,
                            frame.stride,
                            frame.modifier,
                            ash::vk::Format::B8G8R8A8_UNORM,
                        )
                        .map_err(|e| format!("VulkanBridge::import_persistent: {e}"))?
                };
                let imported = unsafe {
                    importer
                        .import_linear(linear_fd, linear_size)
                        .map_err(|e| format!("cuExternalMemoryGetMappedBuffer: {e:?}"))?
                };
                let device_ptr = imported.device_ptr() as *mut c_void;
                let pitch = frame.width * 4; // tightly packed, see VulkanBridge
                let r = session
                    .register_generic_resource(
                        imported,
                        NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
                        device_ptr,
                        pitch,
                    )
                    .map_err(|e| format!("NVENC linear resource registration: {e:?}"))?;
                RegisteredFrame::Linear(r, bridged)
            };
            entry.insert(resource);
            eprintln!(
                "kwin-capture: CudaDirectEncoderSession registered new NVENC input resource for buffer_identity={} ({} distinct buffers so far)",
                frame.buffer_identity,
                registered.len()
            );
        } else {
            // Same underlying GPU buffer we've already imported/registered —
            // its contents changed under us (that's the whole point of
            // zero-copy), but the CUDA/NVENC-side mapping is still valid.
            unsafe { libc::close(frame.fd) };
        }

        let want_keyframe = frame_index % (GOP_LENGTH as u64) == 0 || force_keyframe.swap(false, Ordering::SeqCst);
        let picture_type =
            if want_keyframe { NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR } else { NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_P };
        if want_keyframe {
            hevc_poc = 0;
        } else if codec == VideoCodec::Hevc {
            hevc_poc += 1;
        }

        let input_resource = registered.get_mut(&frame.buffer_identity).unwrap();
        // Vulkan-bridge path only: re-copy fresh content every frame, not
        // just on first registration — see `vulkan_bridge`'s doc comment
        // for why a one-time copy alone would feed NVENC a frozen snapshot
        // forever. Redundant (but harmless) on the very same frame a buffer
        // was just registered, since `import_persistent` already copied
        // once — kept simple rather than special-cased.
        if let RegisteredFrame::Linear(_, bridged) = input_resource {
            unsafe {
                vulkan_bridge.unwrap().lock().unwrap().refresh(bridged).map_err(|e| format!("VulkanBridge::refresh: {e}"))?;
            }
        }
        // HEVC-only: NVENC's own docs (`NV_ENC_PIC_PARAMS_HEVC`) say
        // `displayPOCSyntax` is "required to be set if client is handling
        // the picture type decision" and `refPicFlag` matters whenever PTD
        // is off — see this module's doc comment for how omitting these
        // (an all-zero `codecPicParams`, `displayPOCSyntax` stuck at 0
        // forever) was the real cause of `encode_picture` rejecting every
        // manually-typed HEVC P-frame. H.264's own per-picture params have
        // no such requirement — `None` there is correct, not an oversight.
        let codec_params = (codec == VideoCodec::Hevc).then(|| {
            CodecPictureParams::Hevc(NV_ENC_PIC_PARAMS_HEVC { displayPOCSyntax: hevc_poc, refPicFlag: 1, ..Default::default() })
        });
        session
            .encode_picture(
                input_resource,
                &mut bitstream,
                EncodePictureParams { input_timestamp: frame_index, picture_type, codec_params },
            )
            .map_err(|e| format!("encode_picture: {e:?}"))?;

        {
            let locked = bitstream.lock().map_err(|e| format!("bitstream lock: {e:?}"))?;
            let is_keyframe = locked.picture_type() == NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR;
            on_access_unit(locked.data().to_vec(), is_keyframe, capture_instant);
        }

        fps_window_count += 1;
        let fps_window_elapsed = fps_window_start.elapsed();
        if fps_window_elapsed >= std::time::Duration::from_secs(1) {
            eprintln!(
                "kwin-capture: actual encoded frame rate: {:.1} fps (client-requested fps={fps})",
                fps_window_count as f64 / fps_window_elapsed.as_secs_f64()
            );
            fps_window_start = std::time::Instant::now();
            fps_window_count = 0;
        }

        frame_index += 1;
    }
}
