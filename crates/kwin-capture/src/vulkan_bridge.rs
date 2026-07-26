//! Bridges KWin's tiled DMA-BUF into a *linear* buffer CUDA can import via
//! the classical, broadly-supported opaque-fd path (`cuda_import.rs`'s
//! `CudaImporter::import_linear`) — for GPUs where
//! `CU_DEVICE_ATTRIBUTE_DMA_BUF_SUPPORTED` is false (pre-Ampere; see
//! `CudaImporter::dma_buf_array_import_supported`), so
//! `cuExternalMemoryGetMappedMipmappedArray` can't consume the tiled dma-buf
//! directly.
//!
//! This is a plain, self-contained `ash` Vulkan instance/device — deliberately
//! *not* sharing GStreamer's `GstVulkanDevice` (no context negotiation, no
//! private-struct-offset reads): nothing here ever touches a GStreamer
//! element, so there's nothing to share a device with.
//!
//! The actual bridge is a single GPU copy command, not a shader:
//! `vkCmdCopyImageToBuffer` detiles KWin's DRM-modifier image into a plain
//! linear buffer using the same fixed-function copy engine any Vulkan
//! texture upload uses — no color conversion happens here (NVENC's own
//! hardware accepts packed BGRA/ARGB directly), so no shader is needed.
//!
//! [`VulkanBridge::import_persistent`] (one-time per recurring
//! `buffer_identity`) and [`VulkanBridge::refresh`] (every single frame,
//! even on a cache hit) are deliberately separate calls — this isn't just
//! zero-copy the way [`crate::cuda_import::CudaImporter::import_array`] is:
//! the detile copy is a real snapshot into memory *we* own, not a live
//! mapping of KWin's own buffer. Caching the one-time setup but skipping
//! `refresh` on a cache hit (the first, wrong version of this) fed NVENC the
//! same frozen first-frame snapshot forever — confirmed live: a real
//! session showed a static near-black frame that never updated, while the
//! encoder kept getting invoked (and producing near-empty output, since
//! nothing ever actually changed from its perspective) on every real
//! Wayland-damage event.

use ash::vk;
use std::ffi::c_void;
use std::os::fd::RawFd;

pub struct VulkanBridge {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    physical_device: vk::PhysicalDevice,
    queue: vk::Queue,
    command_pool: vk::CommandPool,
}

impl VulkanBridge {
    /// `physical_device_index` should match the CUDA device ordinal
    /// (`CudaContext::new(index)`) — on a single-GPU machine this is always
    /// correct; on a multi-GPU machine, Vulkan and CUDA don't guarantee the
    /// same device enumeration order, so the caller is responsible for
    /// picking the physical device that's actually the same GPU (matching
    /// e.g. by `VkPhysicalDeviceIDProperties::deviceUUID`, which CUDA also
    /// exposes via `cuDeviceGetUuid` — not needed on this single-GPU box).
    pub fn new(physical_device_index: u32) -> Result<Self, String> {
        let entry = unsafe { ash::Entry::load() }.map_err(|e| format!("ash::Entry::load: {e}"))?;

        let app_info = vk::ApplicationInfo { api_version: vk::API_VERSION_1_1, ..Default::default() };
        let instance_create_info = vk::InstanceCreateInfo { p_application_info: &app_info, ..Default::default() };
        let instance = unsafe { entry.create_instance(&instance_create_info, None) }
            .map_err(|e| format!("vkCreateInstance: {e:?}"))?;

        let physical_devices = unsafe { instance.enumerate_physical_devices() }
            .map_err(|e| format!("vkEnumeratePhysicalDevices: {e:?}"))?;
        let physical_device = *physical_devices
            .get(physical_device_index as usize)
            .ok_or_else(|| format!("no VkPhysicalDevice at index {physical_device_index}"))?;

        let queue_family_props =
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
        let queue_family_index = queue_family_props
            .iter()
            .position(|p| p.queue_flags.contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::TRANSFER))
            .or_else(|| queue_family_props.iter().position(|p| p.queue_flags.contains(vk::QueueFlags::TRANSFER)))
            .ok_or("no queue family supports GRAPHICS or TRANSFER")? as u32;

        let queue_priorities = [1.0f32];
        let queue_create_info = vk::DeviceQueueCreateInfo {
            queue_family_index,
            queue_count: 1,
            p_queue_priorities: queue_priorities.as_ptr(),
            ..Default::default()
        };
        let extensions = [
            ash::khr::external_memory_fd::NAME,
            ash::ext::external_memory_dma_buf::NAME,
            ash::ext::image_drm_format_modifier::NAME,
            ash::ext::queue_family_foreign::NAME,
        ];
        let extension_ptrs: Vec<*const std::ffi::c_char> = extensions.iter().map(|s| s.as_ptr()).collect();
        let device_create_info = vk::DeviceCreateInfo {
            queue_create_info_count: 1,
            p_queue_create_infos: &queue_create_info,
            enabled_extension_count: extension_ptrs.len() as u32,
            pp_enabled_extension_names: extension_ptrs.as_ptr(),
            ..Default::default()
        };
        let device = unsafe { instance.create_device(physical_device, &device_create_info, None) }
            .map_err(|e| format!("vkCreateDevice: {e:?}"))?;

        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };

        let pool_create_info = vk::CommandPoolCreateInfo {
            queue_family_index,
            flags: vk::CommandPoolCreateFlags::TRANSIENT,
            ..Default::default()
        };
        let command_pool = unsafe { device.create_command_pool(&pool_create_info, None) }
            .map_err(|e| format!("vkCreateCommandPool: {e:?}"))?;

        Ok(Self {
            _entry: entry,
            instance,
            device,
            physical_device,
            queue,
            command_pool,
        })
    }

    fn find_memory_type_index(&self, type_bits: u32, required_props: vk::MemoryPropertyFlags) -> u32 {
        let props = unsafe { self.instance.get_physical_device_memory_properties(self.physical_device) };
        for i in 0..props.memory_type_count {
            let matches_type = (type_bits & (1 << i)) != 0;
            let matches_props = props.memory_types[i as usize].property_flags.contains(required_props);
            if matches_type && matches_props {
                return i;
            }
        }
        for i in 0..props.memory_type_count {
            if (type_bits & (1 << i)) != 0 {
                return i;
            }
        }
        panic!("no memory type matches bitmask {type_bits:#x}");
    }

    /// One-time setup for a recurring `buffer_identity`: imports `fd` (a
    /// KWin dma-buf, takes ownership on success — same `DMA_BUF_EXT`
    /// ownership-transfer semantics as `cuda_import.rs`) as a persistent
    /// source image, and allocates a persistent linear destination buffer,
    /// exported as a new, independently-owned fd (safe to import into CUDA
    /// once via `CudaImporter::import_linear`) — plus its size in bytes.
    /// Also does the first copy, so the returned fd's contents are already
    /// valid. Every subsequent frame on this same `buffer_identity` needs
    /// [`VulkanBridge::refresh`] instead, to pick up new content — see this
    /// module's doc comment for why.
    ///
    /// # Safety
    /// `fd` must be a valid, uniquely-owned dma-buf fd for a buffer whose
    /// format truly matches `width`/`height`/`stride`/`modifier`.
    pub unsafe fn import_persistent(
        &self,
        fd: RawFd,
        width: u32,
        height: u32,
        stride: i32,
        modifier: u64,
        vk_format: vk::Format,
    ) -> Result<(BridgedImage, RawFd), String> {
        // --- source image: KWin's tiled dma-buf, imported read-only ---
        let plane_layout = vk::SubresourceLayout {
            offset: 0,
            size: 0,
            row_pitch: stride as u64,
            array_pitch: 0,
            depth_pitch: 0,
        };
        let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT {
            drm_format_modifier: modifier,
            drm_format_modifier_plane_count: 1,
            p_plane_layouts: &plane_layout,
            ..Default::default()
        };
        let mut ext_mem_info = vk::ExternalMemoryImageCreateInfo {
            handle_types: vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            ..Default::default()
        };
        ext_mem_info.p_next = &mut modifier_info as *mut _ as *mut c_void;
        let mut src_create_info = vk::ImageCreateInfo {
            image_type: vk::ImageType::TYPE_2D,
            format: vk_format,
            extent: vk::Extent3D { width, height, depth: 1 },
            mip_levels: 1,
            array_layers: 1,
            samples: vk::SampleCountFlags::TYPE_1,
            tiling: vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT,
            usage: vk::ImageUsageFlags::TRANSFER_SRC,
            sharing_mode: vk::SharingMode::EXCLUSIVE,
            initial_layout: vk::ImageLayout::UNDEFINED,
            ..Default::default()
        };
        src_create_info.p_next = &mut ext_mem_info as *mut _ as *mut c_void;
        let src_image = self
            .device
            .create_image(&src_create_info, None)
            .map_err(|e| format!("vkCreateImage (src, DRM_FORMAT_MODIFIER_EXT): {e:?}"))?;

        let src_mem_req_info = vk::ImageMemoryRequirementsInfo2 { image: src_image, ..Default::default() };
        let mut src_mem_req2 = vk::MemoryRequirements2::default();
        self.device.get_image_memory_requirements2(&src_mem_req_info, &mut src_mem_req2);
        let src_memory_type_index = self.find_memory_type_index(
            src_mem_req2.memory_requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        );

        let mut src_import_info =
            vk::ImportMemoryFdInfoKHR { handle_type: vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT, fd, ..Default::default() };
        let mut src_dedicated_info = vk::MemoryDedicatedAllocateInfo { image: src_image, ..Default::default() };
        src_dedicated_info.p_next = &mut src_import_info as *mut _ as *mut c_void;
        let src_alloc_info = vk::MemoryAllocateInfo {
            allocation_size: src_mem_req2.memory_requirements.size,
            memory_type_index: src_memory_type_index,
            p_next: &mut src_dedicated_info as *mut _ as *mut c_void,
            ..Default::default()
        };
        let src_memory = match self.device.allocate_memory(&src_alloc_info, None) {
            Ok(memory) => memory,
            Err(err) => {
                // Per VK_EXT_external_memory_dma_buf, ownership of `fd` only
                // transfers to Vulkan on a *successful* import — this call
                // is what performs the import, so on failure it's still ours.
                libc::close(fd);
                self.device.destroy_image(src_image, None);
                return Err(format!("vkAllocateMemory (import dma-buf fd {fd}): {err:?}"));
            }
        };
        if let Err(err) = self.device.bind_image_memory2(&[vk::BindImageMemoryInfo {
            image: src_image,
            memory: src_memory,
            memory_offset: 0,
            ..Default::default()
        }]) {
            self.device.free_memory(src_memory, None);
            self.device.destroy_image(src_image, None);
            return Err(format!("vkBindImageMemory2 (src): {err:?}"));
        }

        // --- destination buffer: tightly packed, linear, exportable ---
        let dst_size = (width as u64) * (height as u64) * 4;
        let mut dst_ext_info = vk::ExternalMemoryBufferCreateInfo {
            handle_types: vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD,
            ..Default::default()
        };
        let mut dst_buffer_info = vk::BufferCreateInfo {
            size: dst_size,
            usage: vk::BufferUsageFlags::TRANSFER_DST,
            sharing_mode: vk::SharingMode::EXCLUSIVE,
            ..Default::default()
        };
        dst_buffer_info.p_next = &mut dst_ext_info as *mut _ as *mut c_void;
        let dst_buffer = match self.device.create_buffer(&dst_buffer_info, None) {
            Ok(buffer) => buffer,
            Err(err) => {
                self.device.free_memory(src_memory, None);
                self.device.destroy_image(src_image, None);
                return Err(format!("vkCreateBuffer (dst): {err:?}"));
            }
        };

        let dst_mem_req_info = vk::BufferMemoryRequirementsInfo2 { buffer: dst_buffer, ..Default::default() };
        let mut dst_mem_req2 = vk::MemoryRequirements2::default();
        self.device.get_buffer_memory_requirements2(&dst_mem_req_info, &mut dst_mem_req2);
        let dst_memory_type_index = self.find_memory_type_index(
            dst_mem_req2.memory_requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        );

        let mut dst_export_info =
            vk::ExportMemoryAllocateInfo { handle_types: vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD, ..Default::default() };
        let mut dst_dedicated_info = vk::MemoryDedicatedAllocateInfo { buffer: dst_buffer, ..Default::default() };
        dst_dedicated_info.p_next = &mut dst_export_info as *mut _ as *mut c_void;
        let dst_alloc_info = vk::MemoryAllocateInfo {
            allocation_size: dst_mem_req2.memory_requirements.size,
            memory_type_index: dst_memory_type_index,
            p_next: &mut dst_dedicated_info as *mut _ as *mut c_void,
            ..Default::default()
        };
        let dst_memory = match self.device.allocate_memory(&dst_alloc_info, None) {
            Ok(memory) => memory,
            Err(err) => {
                self.device.destroy_buffer(dst_buffer, None);
                self.device.free_memory(src_memory, None);
                self.device.destroy_image(src_image, None);
                return Err(format!("vkAllocateMemory (dst): {err:?}"));
            }
        };
        if let Err(err) = self.device.bind_buffer_memory2(&[vk::BindBufferMemoryInfo {
            buffer: dst_buffer,
            memory: dst_memory,
            memory_offset: 0,
            ..Default::default()
        }]) {
            self.device.free_memory(dst_memory, None);
            self.device.destroy_buffer(dst_buffer, None);
            self.device.free_memory(src_memory, None);
            self.device.destroy_image(src_image, None);
            return Err(format!("vkBindBufferMemory2 (dst): {err:?}"));
        }

        // First copy, so the fd we're about to export already has valid
        // contents — same barrier/copy/submit `refresh` uses later.
        if let Err(err) = self.record_and_submit_copy(src_image, dst_buffer, width, height) {
            self.device.free_memory(dst_memory, None);
            self.device.destroy_buffer(dst_buffer, None);
            self.device.free_memory(src_memory, None);
            self.device.destroy_image(src_image, None);
            return Err(err);
        }

        // Export the destination buffer's memory as a new fd, independent
        // of `dst_memory`'s Vulkan-side lifetime — safe even though we keep
        // using `dst_memory` ourselves afterward (every `refresh` call):
        // the export just adds an independent reference to the same
        // physical memory, which is exactly what we want here — Vulkan
        // (via `refresh`) keeps writing into it, CUDA (via the exported fd)
        // keeps reading whatever's currently there.
        let external_memory_fd = ash::khr::external_memory_fd::Device::new(&self.instance, &self.device);
        let get_fd_info =
            vk::MemoryGetFdInfoKHR { memory: dst_memory, handle_type: vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD, ..Default::default() };
        let exported_fd = match external_memory_fd.get_memory_fd(&get_fd_info) {
            Ok(fd) => fd,
            Err(err) => {
                self.device.free_memory(dst_memory, None);
                self.device.destroy_buffer(dst_buffer, None);
                self.device.free_memory(src_memory, None);
                self.device.destroy_image(src_image, None);
                return Err(format!("vkGetMemoryFdKHR: {err:?}"));
            }
        };

        let bridged = BridgedImage {
            device: self.device.clone(),
            src_image,
            src_memory,
            dst_buffer,
            dst_memory,
            width,
            height,
        };
        Ok((bridged, exported_fd))
    }

    /// Re-copies `bridged`'s source image into its destination buffer —
    /// call this on *every* frame for a `buffer_identity` already set up via
    /// [`VulkanBridge::import_persistent`] (including the first one after
    /// setup, which redundantly repeats that call's own first copy — cheap,
    /// and keeps this the single obvious "make sure it's fresh" call site)
    /// to pick up whatever KWin has written into the shared source image
    /// since the last call.
    ///
    /// # Safety
    /// `bridged` must have come from this same `VulkanBridge`.
    pub unsafe fn refresh(&self, bridged: &BridgedImage) -> Result<(), String> {
        self.record_and_submit_copy(bridged.src_image, bridged.dst_buffer, bridged.width, bridged.height)
    }

    unsafe fn record_and_submit_copy(&self, src_image: vk::Image, dst_buffer: vk::Buffer, width: u32, height: u32) -> Result<(), String> {
        let cmd_alloc_info = vk::CommandBufferAllocateInfo {
            command_pool: self.command_pool,
            level: vk::CommandBufferLevel::PRIMARY,
            command_buffer_count: 1,
            ..Default::default()
        };
        let cmd_buffers = self
            .device
            .allocate_command_buffers(&cmd_alloc_info)
            .map_err(|e| format!("vkAllocateCommandBuffers: {e:?}"))?;
        let cmd = cmd_buffers[0];

        let begin_info =
            vk::CommandBufferBeginInfo { flags: vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT, ..Default::default() };
        self.device
            .begin_command_buffer(cmd, &begin_info)
            .map_err(|e| format!("vkBeginCommandBuffer: {e:?}"))?;

        // `old_layout: UNDEFINED` even on repeat calls (not whatever layout
        // the previous call left it in): we never write to `src_image`
        // ourselves (TRANSFER_SRC only) and KWin's own writes into the
        // shared memory happen entirely outside this image's Vulkan-tracked
        // state — there's nothing meaningful for a "previous layout" to
        // preserve here, only a barrier telling *this* queue it's about to
        // read it as TRANSFER_SRC_OPTIMAL.
        let to_transfer_src = vk::ImageMemoryBarrier {
            src_access_mask: vk::AccessFlags::empty(),
            dst_access_mask: vk::AccessFlags::TRANSFER_READ,
            old_layout: vk::ImageLayout::UNDEFINED,
            new_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
            dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
            image: src_image,
            subresource_range: vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            },
            ..Default::default()
        };
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_transfer_src],
        );

        let region = vk::BufferImageCopy {
            buffer_offset: 0,
            buffer_row_length: 0,
            buffer_image_height: 0,
            image_subresource: vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            },
            image_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
            image_extent: vk::Extent3D { width, height, depth: 1 },
        };
        self.device.cmd_copy_image_to_buffer(
            cmd,
            src_image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            dst_buffer,
            &[region],
        );

        self.device.end_command_buffer(cmd).map_err(|e| format!("vkEndCommandBuffer: {e:?}"))?;

        let submit_info = vk::SubmitInfo { command_buffer_count: 1, p_command_buffers: &cmd, ..Default::default() };
        // `queue_wait_idle`, not a fence we return early from: the caller
        // (`nvenc_session`'s encode loop) immediately hands `dst_buffer`'s
        // CUDA-imported memory to NVENC right after this returns, so the
        // copy must be GPU-complete, not just submitted, before we do.
        let submit_result = self
            .device
            .queue_submit(self.queue, &[submit_info], vk::Fence::null())
            .and_then(|()| self.device.queue_wait_idle(self.queue));
        self.device.free_command_buffers(self.command_pool, &cmd_buffers);
        submit_result.map_err(|e| format!("vkQueueSubmit/vkQueueWaitIdle: {e:?}"))
    }
}

/// A recurring `buffer_identity`'s persistent Vulkan-side state — kept
/// alive for as long as that buffer keeps recurring, re-copied fresh via
/// [`VulkanBridge::refresh`] every frame. See this module's doc comment for
/// why a one-time copy isn't enough.
pub struct BridgedImage {
    device: ash::Device,
    src_image: vk::Image,
    src_memory: vk::DeviceMemory,
    dst_buffer: vk::Buffer,
    dst_memory: vk::DeviceMemory,
    width: u32,
    height: u32,
}

impl Drop for BridgedImage {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_buffer(self.dst_buffer, None);
            self.device.free_memory(self.dst_memory, None);
            self.device.destroy_image(self.src_image, None);
            self.device.free_memory(self.src_memory, None);
        }
    }
}

impl Drop for VulkanBridge {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
