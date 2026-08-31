//! Live GBM-based search for a usable NVIDIA block-linear (tiled) DRM
//! format modifier, when `egl_dmabuf::query_dmabuf_formats`'s
//! `eglQueryDmaBufModifiersEXT` result doesn't include a working one on
//! its own.
//!
//! Confirmed live: on a GTX 1070, that EGL query only ever reports
//! `DRM_FORMAT_MOD_LINEAR` for every format — and NVIDIA's GBM backend
//! can never actually allocate a *renderable* buffer
//! (`GBM_BO_USE_RENDERING`) with the LINEAR modifier at all, confirmed as
//! a general NVIDIA limitation, not specific to that GPU (reproduced
//! identically on a working RTX 2080, where it just doesn't matter
//! because that GPU's driver *also* advertises a real, working tiled
//! modifier as a second option). So on the 1070, KWin's own
//! `ScreenCastStream::testCreateDmaBuf`
//! (`src/plugins/screencast/screencaststream.cpp`) always fails — not
//! because the hardware can't do zero-copy DMA-BUF, but because the query
//! API that's supposed to advertise a working modifier simply never does.
//!
//! Brute-forcing the small (<20000-combination) space directly against
//! the real driver is the only way to find a working one: the "page
//! kind" field of NVIDIA's block-linear modifier encoding
//! (`DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D` in `drm_fourcc.h`) is
//! explicitly documented as GPU-model-internal, derived by the driver
//! itself from `(format, GPU model, compression type, samples per
//! pixel)` — there is no public table to look one up from. Confirmed
//! live: this search finds real, working modifiers on both a Turing
//! (`g=2, s=1, k=6`) and a Pascal (`g=0, s=1, k=254`) GPU — different per
//! GPU generation as expected, and neither ever advertised by the EGL
//! query on its own GPU. Validated first as a standalone tool
//! (`scripts/test-screencast-dmabuf-modifier-search.sh`, kept in the repo
//! for future re-diagnosis) before being wired in here.
//!
//! Cached per (render node, format, resolution) the same way
//! `query_dmabuf_formats` is cached — the search itself takes well under
//! a second, but there's no reason to repeat it every pipeline rebuild.

use khronos_egl as egl;
use std::collections::HashMap;
use std::ffi::{c_int, c_uint, c_void};
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::Mutex;

const EGL_PLATFORM_GBM_KHR: egl::Enum = 0x31D7;
const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: egl::Int = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: egl::Int = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: egl::Int = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: egl::Int = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: egl::Int = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: egl::Int = 0x3444;

const GBM_BO_USE_RENDERING: u32 = 1 << 2;
const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_NO_ERROR: u32 = 0;

/// `DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(c, s, g, k, h)` from
/// `drm_fourcc.h`, reimplemented here since it's a C preprocessor macro
/// (not something `drm_fourcc.h`'s Rust equivalents, if any were even in
/// use here, would expose as a callable function).
fn nvidia_block_linear_2d(c: u64, s: u64, g: u64, k: u64, h: u64) -> u64 {
    const DRM_FORMAT_MOD_VENDOR_NVIDIA: u64 = 0x03;
    let raw = 0x10 | (h & 0xf) | ((k & 0xff) << 12) | ((g & 0x3) << 20) | ((s & 0x1) << 22) | ((s & 0x6) << 25) | ((c & 0x7) << 23);
    (DRM_FORMAT_MOD_VENDOR_NVIDIA << 56) | raw
}

struct GbmLib {
    _lib: libloading::Library,
    create_device: unsafe extern "C" fn(c_int) -> *mut c_void,
    device_destroy: unsafe extern "C" fn(*mut c_void),
    bo_create_with_modifiers2:
        unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const u64, c_uint, u32) -> *mut c_void,
    bo_get_fd_for_plane: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    bo_get_stride_for_plane: unsafe extern "C" fn(*mut c_void, c_int) -> u32,
    bo_get_offset: unsafe extern "C" fn(*mut c_void, c_int) -> u32,
    bo_get_modifier: unsafe extern "C" fn(*mut c_void) -> u64,
    bo_destroy: unsafe extern "C" fn(*mut c_void),
}

impl GbmLib {
    fn load() -> Result<Self, Box<dyn std::error::Error>> {
        unsafe {
            let lib = libloading::Library::new("libgbm.so.1")?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get(concat!($name, "\0").as_bytes())?
                };
            }
            Ok(GbmLib {
                create_device: sym!("gbm_create_device"),
                device_destroy: sym!("gbm_device_destroy"),
                bo_create_with_modifiers2: sym!("gbm_bo_create_with_modifiers2"),
                bo_get_fd_for_plane: sym!("gbm_bo_get_fd_for_plane"),
                bo_get_stride_for_plane: sym!("gbm_bo_get_stride_for_plane"),
                bo_get_offset: sym!("gbm_bo_get_offset"),
                bo_get_modifier: sym!("gbm_bo_get_modifier"),
                bo_destroy: sym!("gbm_bo_destroy"),
                _lib: lib,
            })
        }
    }
}

struct GlesLib {
    _lib: libloading::Library,
    gen_textures: unsafe extern "C" fn(c_int, *mut u32),
    bind_texture: unsafe extern "C" fn(u32, u32),
    delete_textures: unsafe extern "C" fn(c_int, *const u32),
    gen_framebuffers: unsafe extern "C" fn(c_int, *mut u32),
    bind_framebuffer: unsafe extern "C" fn(u32, u32),
    delete_framebuffers: unsafe extern "C" fn(c_int, *const u32),
    framebuffer_texture_2d: unsafe extern "C" fn(u32, u32, u32, u32, c_int),
    check_framebuffer_status: unsafe extern "C" fn(u32) -> u32,
    get_error: unsafe extern "C" fn() -> u32,
}

impl GlesLib {
    fn load() -> Result<Self, Box<dyn std::error::Error>> {
        unsafe {
            let lib = libloading::Library::new("libGLESv2.so.2")?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get(concat!($name, "\0").as_bytes())?
                };
            }
            Ok(GlesLib {
                gen_textures: sym!("glGenTextures"),
                bind_texture: sym!("glBindTexture"),
                delete_textures: sym!("glDeleteTextures"),
                gen_framebuffers: sym!("glGenFramebuffers"),
                bind_framebuffer: sym!("glBindFramebuffer"),
                delete_framebuffers: sym!("glDeleteFramebuffers"),
                framebuffer_texture_2d: sym!("glFramebufferTexture2D"),
                check_framebuffer_status: sym!("glCheckFramebufferStatus"),
                get_error: sym!("glGetError"),
                _lib: lib,
            })
        }
    }
}

/// Finds the first `/dev/dri/renderD*` node whose PCI vendor is NVIDIA
/// (`0x10de`) — mirrors `redfog-broker/src/session.rs`'s
/// `select_gpu_render_node`, kept as a small, independent duplicate here
/// rather than a shared crate: this is the only other place that needs
/// it, and the two call sites have different enough surrounding context
/// (this one doesn't need the sibling-card/by-path logic, config
/// override, or bwrap integration) that factoring out just this part
/// would be more abstraction than the actual shared logic (a dozen
/// lines) is worth.
fn find_nvidia_render_node() -> Option<PathBuf> {
    let entries = std::fs::read_dir("/dev/dri").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("renderD") {
            continue;
        }
        let vendor_path = format!("/sys/class/drm/{name}/device/vendor");
        if std::fs::read_to_string(&vendor_path).is_ok_and(|v| v.trim().eq_ignore_ascii_case("0x10de")) {
            return Some(PathBuf::from(format!("/dev/dri/{name}")));
        }
    }
    None
}

type ModifierCacheKey = (u32, u32, u32); // (drm_fourcc, width, height)
type ModifierCache = HashMap<ModifierCacheKey, Option<u64>>;

/// See this module's own doc comment. Returns `None` if no working
/// modifier was found (including: no NVIDIA render node at all, or
/// `libgbm`/`libGLESv2` unavailable) — callers should treat that as "no
/// improvement available, keep whatever `query_dmabuf_formats` already
/// found."
pub fn find_working_tiled_modifier(drm_fourcc: u32, width: u32, height: u32) -> Option<u64> {
    static CACHE: Mutex<Option<ModifierCache>> = Mutex::new(None);
    let key: ModifierCacheKey = (drm_fourcc, width, height);
    let mut cache = CACHE.lock().unwrap();
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some(cached) = cache.get(&key) {
        return *cached;
    }
    let found = match try_search(drm_fourcc, width, height) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("kwin-capture: GBM tiled-modifier search unavailable: {e}");
            None
        }
    };
    if let Some(modifier) = found {
        eprintln!("kwin-capture: found working NVIDIA tiled modifier {modifier:#x} for format {drm_fourcc:#x} ({width}x{height}) via live GBM search");
    }
    cache.insert(key, found);
    found
}

fn try_search(drm_fourcc: u32, width: u32, height: u32) -> Result<Option<u64>, Box<dyn std::error::Error>> {
    let node = find_nvidia_render_node().ok_or("no NVIDIA render node found")?;
    let gbm_lib = GbmLib::load()?;
    let gles_lib = GlesLib::load()?;

    let fd = std::fs::OpenOptions::new().read(true).write(true).open(&node)?;
    let raw_fd: RawFd = std::os::fd::AsRawFd::as_raw_fd(&fd);
    let gbm = unsafe { (gbm_lib.create_device)(raw_fd) };
    if gbm.is_null() {
        return Err("gbm_create_device failed".into());
    }
    struct GbmGuard<'a>(&'a GbmLib, *mut c_void);
    impl Drop for GbmGuard<'_> {
        fn drop(&mut self) {
            unsafe { (self.0.device_destroy)(self.1) }
        }
    }
    let _gbm_guard = GbmGuard(&gbm_lib, gbm);

    let egl = unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required()? };
    let get_platform_display: unsafe extern "C" fn(egl::Enum, *mut c_void, *const egl::Int) -> egl::EGLDisplay = unsafe {
        std::mem::transmute(
            egl.get_proc_address("eglGetPlatformDisplayEXT").ok_or("eglGetPlatformDisplayEXT not available")?,
        )
    };
    let dpy = unsafe { get_platform_display(EGL_PLATFORM_GBM_KHR, gbm, std::ptr::null()) };
    if dpy.is_null() {
        return Err("eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR) returned null".into());
    }
    let display = unsafe { egl::Display::from_ptr(dpy) };
    egl.initialize(display)?;
    struct EglGuard<'a>(&'a egl::DynamicInstance<egl::EGL1_5>, egl::Display);
    impl Drop for EglGuard<'_> {
        fn drop(&mut self) {
            let _ = self.0.terminate(self.1);
        }
    }
    let _egl_guard = EglGuard(&egl, display);

    egl.bind_api(egl::OPENGL_ES_API)?;
    let config_attribs = [
        egl::SURFACE_TYPE,
        egl::PBUFFER_BIT as egl::Int,
        egl::RENDERABLE_TYPE,
        egl::OPENGL_ES2_BIT as egl::Int,
        egl::RED_SIZE,
        8,
        egl::GREEN_SIZE,
        8,
        egl::BLUE_SIZE,
        8,
        egl::NONE,
    ];
    let config = egl.choose_first_config(display, &config_attribs)?.ok_or("no usable EGL config found")?;
    let ctx_attribs = [egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE];
    let context = egl.create_context(display, config, None, &ctx_attribs)?;
    egl.make_current(display, None, None, Some(context))?;

    let create_image: unsafe extern "C" fn(egl::EGLDisplay, egl::EGLContext, egl::Enum, *mut c_void, *const egl::Int) -> *mut c_void =
        unsafe { std::mem::transmute(egl.get_proc_address("eglCreateImageKHR").ok_or("eglCreateImageKHR not available")?) };
    let destroy_image: unsafe extern "C" fn(egl::EGLDisplay, *mut c_void) -> egl::Boolean =
        unsafe { std::mem::transmute(egl.get_proc_address("eglDestroyImageKHR").ok_or("eglDestroyImageKHR not available")?) };
    let image_target_texture_2d: unsafe extern "C" fn(u32, *mut c_void) = unsafe {
        std::mem::transmute(egl.get_proc_address("glEGLImageTargetTexture2DOES").ok_or("glEGLImageTargetTexture2DOES not available")?)
    };

    // c=0 (no compression) fixed -- see scripts/test-screencast-dmabuf-modifier-search.sh's
    // header comment for the full rationale (matches this search exactly,
    // validated there first). g, h, s, k sweep their full
    // documented/possible ranges.
    for g in 0u64..=2 {
        for h in 0u64..=5 {
            for s in 0u64..=3 {
                for k in 0u64..=255 {
                    let modifier = nvidia_block_linear_2d(0, s, g, k, h);
                    let bo = unsafe {
                        (gbm_lib.bo_create_with_modifiers2)(gbm, width, height, drm_fourcc, &modifier, 1, GBM_BO_USE_RENDERING)
                    };
                    if bo.is_null() {
                        continue;
                    }
                    let real_modifier = unsafe { (gbm_lib.bo_get_modifier)(bo) };
                    let dmabuf_fd = unsafe { (gbm_lib.bo_get_fd_for_plane)(bo, 0) };
                    let mut complete = false;
                    if dmabuf_fd >= 0 {
                        let stride = unsafe { (gbm_lib.bo_get_stride_for_plane)(bo, 0) };
                        let offset = unsafe { (gbm_lib.bo_get_offset)(bo, 0) };
                        let image_attribs = [
                            egl::WIDTH,
                            width as egl::Int,
                            egl::HEIGHT,
                            height as egl::Int,
                            EGL_LINUX_DRM_FOURCC_EXT,
                            drm_fourcc as egl::Int,
                            EGL_DMA_BUF_PLANE0_FD_EXT,
                            dmabuf_fd,
                            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
                            offset as egl::Int,
                            EGL_DMA_BUF_PLANE0_PITCH_EXT,
                            stride as egl::Int,
                            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                            (real_modifier & 0xffffffff) as egl::Int,
                            EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                            (real_modifier >> 32) as egl::Int,
                            egl::NONE,
                        ];
                        let image = unsafe {
                            create_image(display.as_ptr(), std::ptr::null_mut(), EGL_LINUX_DMA_BUF_EXT, std::ptr::null_mut(), image_attribs.as_ptr())
                        };
                        if !image.is_null() {
                            let mut tex = 0u32;
                            let mut fbo = 0u32;
                            unsafe {
                                (gles_lib.gen_textures)(1, &mut tex);
                                (gles_lib.bind_texture)(GL_TEXTURE_2D, tex);
                                image_target_texture_2d(GL_TEXTURE_2D, image);
                                if (gles_lib.get_error)() == GL_NO_ERROR {
                                    (gles_lib.gen_framebuffers)(1, &mut fbo);
                                    (gles_lib.bind_framebuffer)(GL_FRAMEBUFFER, fbo);
                                    (gles_lib.framebuffer_texture_2d)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
                                    complete = (gles_lib.check_framebuffer_status)(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE;
                                    (gles_lib.delete_framebuffers)(1, &fbo);
                                }
                                (gles_lib.delete_textures)(1, &tex);
                                destroy_image(display.as_ptr(), image);
                            }
                        }
                        unsafe { libc::close(dmabuf_fd) };
                    }
                    unsafe { (gbm_lib.bo_destroy)(bo) };
                    if complete {
                        return Ok(Some(real_modifier));
                    }
                }
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Needs a real NVIDIA GPU — no #[ignore], matching this whole crate's
    // existing convention of being excluded wholesale from the sudo-free
    // CI suite via `-p`/`--exclude` rather than per-test #[ignore].
    #[test]
    fn finds_a_working_tiled_modifier_on_a_real_nvidia_gpu() {
        const DRM_FORMAT_ARGB8888: u32 = 0x34325241; // "AR24" — matches FORMAT_MAP
        let found = find_working_tiled_modifier(DRM_FORMAT_ARGB8888, 1920, 1080);
        let modifier = found.expect("expected to find a working tiled modifier on a real NVIDIA GPU");
        let vendor = modifier >> 56;
        assert_eq!(vendor, 0x03, "modifier {modifier:#x} doesn't look like an NVIDIA (vendor 0x03) modifier");
        println!("found working modifier: {modifier:#x}");
    }
}
