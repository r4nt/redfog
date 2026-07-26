//! Import a DMA-BUF frame directly into CUDA device memory via
//! `cuImportExternalMemory` (wrapped safely by the `cudarc` crate, dlopen-based
//! — no CUDA toolkit install needed, only the driver's `libcuda.so.1`),
//! bypassing GL/EGL entirely.
//!
//! [`CudaImporter::import_array`] is the one that matters in practice: KWin's
//! virtual output only ever exports its preferred tiled/block-linear modifier
//! — confirmed live, restricting PipeWire negotiation to
//! `DRM_FORMAT_MOD_LINEAR` (0) doesn't get a linear DMA-BUF, it makes KWin
//! give up on DMA-BUF entirely and fall back to `MemPtr`. A tiled buffer isn't
//! a flat address range, so it has to come back as an opaque `CUarray`
//! (`cuExternalMemoryGetMappedMipmappedArray`), not a `CUdeviceptr`
//! (`cuExternalMemoryGetMappedBuffer`, [`CudaImporter::import_linear`] below)
//! — the latter would silently reinterpret tiled bytes as if they were
//! row-major, producing garbage.
//!
//! `import_array` only works on GPUs where
//! [`CudaImporter::dma_buf_array_import_supported`] is true (Ampere+ —
//! confirmed false on this dev machine's Turing RTX 2080, where
//! `cuExternalMemoryGetMappedMipmappedArray` reliably fails with
//! `CUDA_ERROR_UNKNOWN`). On older hardware, use
//! [`crate::vulkan_bridge::VulkanBridge::detile_dmabuf_to_linear_fd`] first
//! to detile the frame into a linear buffer via a plain GPU copy command,
//! then hand *that* fd to [`CudaImporter::import_linear`] instead — the
//! classical opaque-fd CUDA/Vulkan interop path, which isn't gated by that
//! capability bit.

use cudarc::driver::sys::{self, CUarray, CUcontext, CUdevice, CUdeviceptr, CUexternalMemory};
use cudarc::driver::{CudaContext, DevicePtr, DriverError, MappedBuffer};
use std::fs::File;
use std::os::fd::{FromRawFd, RawFd};
use std::sync::Arc;

/// Owns the CUDA context every frame is imported against — reused (not
/// independently re-created) by the NVENC session so both operate on the
/// exact same underlying `CUcontext`.
pub struct CudaImporter {
    ctx: Arc<CudaContext>,
}

impl CudaImporter {
    /// `CudaContext::new(0)` retains the *primary* CUDA context for device 0
    /// (`cuDevicePrimaryCtxRetain`) — the standard, refcounted, interop-
    /// friendly context every well-behaved CUDA-using library in this
    /// process is expected to share, rather than each creating its own. It's
    /// safe for a second, independent `cudarc` copy (e.g. a different semver
    /// line pulled in transitively by an encoder crate) to also call
    /// `CudaContext::new(0)`: the driver refcounts the primary context per
    /// (process, device), not per caller, so both resolve to the same
    /// underlying `CUcontext`.
    pub fn new() -> Result<Self, DriverError> {
        Ok(Self { ctx: CudaContext::new(0)? })
    }

    pub fn cu_context(&self) -> CUcontext {
        self.ctx.cu_ctx()
    }

    pub fn cu_device(&self) -> CUdevice {
        self.ctx.cu_device()
    }

    /// Whether this GPU/driver supports importing a dma-buf directly as a
    /// tiled `CUarray` via [`CudaImporter::import_array`] — `false` means
    /// callers need the [`crate::vulkan_bridge`] fallback instead. Queries
    /// `CU_DEVICE_ATTRIBUTE_DMA_BUF_SUPPORTED` (Ampere+ only).
    pub fn dma_buf_array_import_supported(&self) -> Result<bool, DriverError> {
        let supported = self.ctx.attribute(sys::CUdevice_attribute::CU_DEVICE_ATTRIBUTE_DMA_BUF_SUPPORTED)?;
        Ok(supported != 0)
    }

    /// Imports `fd` (takes ownership) as a tiled/opaque CUDA array — the path
    /// that actually matters, see this module's doc comment. `width`/`height`
    /// are in pixels; `channels` is the per-pixel component count (4 for
    /// BGRx/BGRA). `size` is the dma-buf's real allocated size in bytes
    /// (query via `fstat`, not `stride * height` — a tiled allocation's real
    /// size doesn't necessarily match that naive calculation).
    ///
    /// # Safety
    /// `fd` must be a valid, uniquely-owned dma-buf fd for a buffer whose
    /// format truly matches `width`/`height`/`channels`.
    pub unsafe fn import_array(
        &self,
        fd: RawFd,
        size: u64,
        width: usize,
        height: usize,
        channels: u32,
    ) -> Result<ImportedArray, DriverError> {
        self.ctx.bind_to_thread()?;

        // cuImportExternalMemory's opaque-fd handle takes ownership of `fd` on
        // success (CUDA's own docs: "Ownership of the file descriptor is
        // transferred to the CUDA driver when the handle is imported
        // successfully"), so on success we must not touch it again — not even
        // via a `File`/`OwnedFd` we construct and then `mem::forget`: Rust's
        // io-safety hardening (stable since 1.80) tracks those regardless of
        // `forget`, and asserting a raw fd is "ours" while CUDA is
        // concurrently using/closing it on another thread aborts the process.
        // Plain `libc::close` on the failure path avoids that bookkeeping
        // entirely, and is correct since import failure means ownership was
        // never transferred.
        let handle_description = sys::CUDA_EXTERNAL_MEMORY_HANDLE_DESC {
            type_: sys::CUexternalMemoryHandleType::CU_EXTERNAL_MEMORY_HANDLE_TYPE_DMABUF_FD,
            handle: sys::CUDA_EXTERNAL_MEMORY_HANDLE_DESC_st__bindgen_ty_1 { fd },
            size,
            flags: 0,
            reserved: [0; 16],
        };
        let mut external = std::ptr::null_mut();
        let result = unsafe { sys::cuImportExternalMemory(&mut external, &handle_description) };
        let external = if result == sys::CUresult::CUDA_SUCCESS {
            external
        } else {
            unsafe { libc::close(fd) };
            return Err(map_driver_error(result));
        };

        let array_desc = sys::CUDA_ARRAY3D_DESCRIPTOR {
            Width: width,
            Height: height,
            Depth: 0,
            Format: sys::CUarray_format::CU_AD_FORMAT_UNSIGNED_INT8,
            NumChannels: channels,
            // Marks the array as usable by the hardware video encode/decode
            // engines — required for NVENC's CUDAARRAY input resource type.
            Flags: sys::CUDA_ARRAY3D_VIDEO_ENCODE_DECODE,
        };
        let mipmap_desc = sys::CUDA_EXTERNAL_MEMORY_MIPMAPPED_ARRAY_DESC {
            offset: 0,
            arrayDesc: array_desc,
            numLevels: 1,
            reserved: [0; 16],
        };

        let mut mipmap: sys::CUmipmappedArray = std::ptr::null_mut();
        let result = unsafe {
            sys::cuExternalMemoryGetMappedMipmappedArray(&mut mipmap, external, &mipmap_desc)
        };
        if result != sys::CUresult::CUDA_SUCCESS {
            unsafe { sys::cuDestroyExternalMemory(external) };
            return Err(map_driver_error(result));
        }

        let mut array: CUarray = std::ptr::null_mut();
        let result = unsafe { sys::cuMipmappedArrayGetLevel(&mut array, mipmap, 0) };
        if result != sys::CUresult::CUDA_SUCCESS {
            unsafe { sys::cuDestroyExternalMemory(external) };
            return Err(map_driver_error(result));
        }

        Ok(ImportedArray { external, array })
    }

    /// Imports `fd` (takes ownership) as external memory and maps it as a
    /// flat linear buffer. Only correct for a genuinely linear (modifier=0)
    /// dma-buf — see this module's doc comment for why that's not what KWin
    /// actually hands out here. Kept for reference/potential reuse elsewhere.
    ///
    /// # Safety
    /// `fd` must be a valid, uniquely-owned linear-layout dma-buf fd, and
    /// `size` must not exceed the buffer's real allocated size.
    pub unsafe fn import_linear(&self, fd: RawFd, size: u64) -> Result<ImportedLinear, DriverError> {
        let file = unsafe { File::from_raw_fd(fd) };
        let external = unsafe { self.ctx.import_external_memory(file, size) }?;
        let mapped = external.map_all()?;
        let stream = self.ctx.default_stream();
        let device_ptr = {
            let (device_ptr, _sync) = mapped.device_ptr(&stream);
            device_ptr
        };
        Ok(ImportedLinear { _mapped: mapped, device_ptr })
    }
}

fn map_driver_error(result: sys::CUresult) -> DriverError {
    DriverError(result)
}

/// An imported tiled/opaque CUDA array — the real handoff point for feeding
/// NVENC (`NV_ENC_INPUT_RESOURCE_TYPE_CUDAARRAY`) without any GL/copy step.
/// Dropping this destroys the external memory registration (which, per CUDA's
/// docs, also invalidates the derived array/mipmap — they're never destroyed
/// separately) and, since the opaque-fd import took ownership of it, closes
/// the underlying fd.
pub struct ImportedArray {
    external: CUexternalMemory,
    array: CUarray,
}

unsafe impl Send for ImportedArray {}

impl ImportedArray {
    pub fn array(&self) -> CUarray {
        self.array
    }
}

impl Drop for ImportedArray {
    fn drop(&mut self) {
        unsafe {
            let _ = sys::cuDestroyExternalMemory(self.external);
        }
    }
}

/// See [`CudaImporter::import_linear`].
pub struct ImportedLinear {
    _mapped: MappedBuffer,
    device_ptr: CUdeviceptr,
}

impl ImportedLinear {
    pub fn device_ptr(&self) -> CUdeviceptr {
        self.device_ptr
    }
}
