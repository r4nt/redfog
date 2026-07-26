//! Production wrapper around the DMA-BUF -> CUDA -> NVENC path validated by
//! `tests/cuda_direct_nvenc.rs`: a background thread that captures via
//! `PipewireCapture`, imports each frame into CUDA (directly as a tiled
//! array on Ampere+, or via `vulkan_bridge`'s detile-to-linear-buffer bridge
//! on older GPUs — see `cuda_import`'s doc comment), and drives NVENC
//! directly, with no GStreamer involved in the video leg at all.
//!
//! Picture-type decision (`enablePTD`) is deliberately left *off* — with it
//! on, NVIDIA's docs say `NV_ENC_PIC_PARAMS::pictureType` is ignored, which
//! would make [`CudaDirectEncoderSession::request_keyframe`] a no-op. With
//! it off, we own the I/P cadence ourselves (an IDR every `gop_length`
//! frames or on request, P otherwise) — the same standard pattern
//! GStreamer's `nvh264enc`/PTD path replaces, just done by hand.
//!
//! Live bitrate changes aren't implemented: `nvidia-video-codec-sdk` 0.4
//! doesn't expose `NvEncReconfigureEncoder` through its safe API (the
//! `Encoder`/`Session` structs' raw pointer field is `pub(crate)`), so
//! [`CudaDirectEncoderSession::set_bitrate`] is currently a logged no-op.

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use nvidia_video_codec_sdk::sys::nvEncodeAPI::{
    NV_ENC_BUFFER_FORMAT,
    NV_ENC_CODEC_H264_GUID,
    NV_ENC_INPUT_RESOURCE_TYPE,
    NV_ENC_PARAMS_RC_MODE,
    NV_ENC_PIC_TYPE,
    NV_ENC_PRESET_P4_GUID,
    NV_ENC_TUNING_INFO,
};
use nvidia_video_codec_sdk::{EncodePictureParams, Encoder, EncoderInitParams, EncoderInput, RegisteredResource};

use crate::cuda_import::{CudaImporter, ImportedArray, ImportedLinear};
use crate::pipewire_capture::PipewireCapture;
use crate::vulkan_bridge::{BridgedImage, VulkanBridge};

const GOP_LENGTH: u32 = 300;

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
    thread: Option<JoinHandle<()>>,
}

impl CudaDirectEncoderSession {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        node_id: u32,
        wayland_socket_path: PathBuf,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        on_access_unit: impl Fn(Vec<u8>, bool) + Send + Sync + 'static,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let force_keyframe = Arc::new(AtomicBool::new(false));
        let thread_shutdown = shutdown.clone();
        let thread_force_keyframe = force_keyframe.clone();
        let thread = std::thread::spawn(move || {
            if let Err(e) = run(
                node_id,
                wayland_socket_path,
                width,
                height,
                fps,
                bitrate_kbps,
                &thread_shutdown,
                &thread_force_keyframe,
                &on_access_unit,
            ) {
                eprintln!("kwin-capture: CudaDirectEncoderSession thread exiting with error: {e}");
            }
        });
        Self { shutdown, force_keyframe, thread: Some(thread) }
    }

    /// See [`redfog_core::request_keyframe`]'s doc comment — same purpose
    /// (Moonlight's `RequestIdrFrame`/`InvalidateReferenceFrames`), just a
    /// flag the encode loop checks itself instead of a `GstForceKeyUnitEvent`.
    pub fn request_keyframe(&self) {
        self.force_keyframe.store(true, Ordering::SeqCst);
    }

    /// Not implemented yet — see this module's doc comment for why.
    pub fn set_bitrate(&self, bitrate_kbps: u32) {
        eprintln!(
            "kwin-capture: CudaDirectEncoderSession::set_bitrate({bitrate_kbps}) ignored — \
             live bitrate reconfiguration isn't supported yet on the CUDA-direct encode path"
        );
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

#[allow(clippy::too_many_arguments)]
fn run(
    node_id: u32,
    wayland_socket_path: PathBuf,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    shutdown: &AtomicBool,
    force_keyframe: &AtomicBool,
    on_access_unit: &(impl Fn(Vec<u8>, bool) + Send + Sync + 'static),
) -> Result<(), String> {
    let capture = PipewireCapture::start(node_id, wayland_socket_path, false)
        .map_err(|e| format!("PipewireCapture::start: {e}"))?;

    let importer = CudaImporter::new().map_err(|e| format!("CudaImporter::new: {e:?}"))?;
    let use_array_import = importer.dma_buf_array_import_supported().unwrap_or(false);
    eprintln!(
        "kwin-capture: CudaDirectEncoderSession using {} import path (CU_DEVICE_ATTRIBUTE_DMA_BUF_SUPPORTED={use_array_import})",
        if use_array_import { "direct tiled-array" } else { "Vulkan-bridge linear" }
    );
    let vulkan_bridge = if use_array_import {
        None
    } else {
        Some(VulkanBridge::new(0).map_err(|e| format!("VulkanBridge::new: {e}"))?)
    };

    // A separate cudarc line from `CudaImporter`'s (see cuda_import.rs's doc
    // comment) — both retain the same refcounted primary context for device 0.
    let nvenc_cuda_ctx =
        cudarc016::driver::CudaContext::new(0).map_err(|e| format!("CudaContext::new(0) for NVENC: {e:?}"))?;
    let encoder =
        Encoder::initialize_with_cuda(nvenc_cuda_ctx).map_err(|e| format!("Encoder::initialize_with_cuda: {e:?}"))?;

    let encode_guids = encoder.get_encode_guids().map_err(|e| format!("get_encode_guids: {e:?}"))?;
    if !encode_guids.contains(&NV_ENC_CODEC_H264_GUID) {
        return Err("NVENC on this machine doesn't support H.264".to_string());
    }
    let buffer_format = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
    let input_formats = encoder
        .get_supported_input_formats(NV_ENC_CODEC_H264_GUID)
        .map_err(|e| format!("get_supported_input_formats: {e:?}"))?;
    if !input_formats.contains(&buffer_format) {
        return Err("NVENC doesn't support ARGB input for H.264 on this machine".to_string());
    }

    let mut preset_config = encoder
        .get_preset_config(NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P4_GUID, NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY)
        .map_err(|e| format!("get_preset_config: {e:?}"))?;
    {
        let config = &mut preset_config.presetCfg;
        config.gopLength = GOP_LENGTH;
        config.frameIntervalP = 1; // no B-frames: lowest latency
        config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
        config.rcParams.averageBitRate = bitrate_kbps * 1000;
        config.rcParams.maxBitRate = bitrate_kbps * 1000;
        unsafe { config.encodeCodecConfig.h264Config.set_repeatSPSPPS(1) };
    }

    let mut init_params = EncoderInitParams::new(NV_ENC_CODEC_H264_GUID, width, height);
    init_params
        .preset_guid(NV_ENC_PRESET_P4_GUID)
        .tuning_info(NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY)
        .framerate(fps, 1)
        // deliberately not calling enable_picture_type_decision() — see
        // this module's doc comment for why we manage picture type ourselves.
        .encode_config(&mut preset_config.presetCfg);

    let session = encoder
        .start_session(buffer_format, init_params)
        .map_err(|e| format!("start_session: {e:?}"))?;
    let mut bitstream = session.create_output_bitstream().map_err(|e| format!("create_output_bitstream: {e:?}"))?;

    let mut registered: HashMap<i64, RegisteredFrame<'_>> = HashMap::new();
    let mut frame_index: u64 = 0;

    while !shutdown.load(Ordering::SeqCst) {
        let Some(frame) = capture.next_frame() else {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        };
        if !frame.is_dma_buf {
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
                        .as_ref()
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

        let input_resource = registered.get_mut(&frame.buffer_identity).unwrap();
        // Vulkan-bridge path only: re-copy fresh content every frame, not
        // just on first registration — see `vulkan_bridge`'s doc comment
        // for why a one-time copy alone would feed NVENC a frozen snapshot
        // forever. Redundant (but harmless) on the very same frame a buffer
        // was just registered, since `import_persistent` already copied
        // once — kept simple rather than special-cased.
        if let RegisteredFrame::Linear(_, bridged) = input_resource {
            unsafe {
                vulkan_bridge.as_ref().unwrap().refresh(bridged).map_err(|e| format!("VulkanBridge::refresh: {e}"))?;
            }
        }
        session
            .encode_picture(
                input_resource,
                &mut bitstream,
                EncodePictureParams { input_timestamp: frame_index, picture_type, ..Default::default() },
            )
            .map_err(|e| format!("encode_picture: {e:?}"))?;

        {
            let locked = bitstream.lock().map_err(|e| format!("bitstream lock: {e:?}"))?;
            let is_keyframe = locked.picture_type() == NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR;
            on_access_unit(locked.data().to_vec(), is_keyframe);
        }

        frame_index += 1;
    }

    Ok(())
}
