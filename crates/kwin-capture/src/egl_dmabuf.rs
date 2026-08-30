//! Query the real DMA-BUF formats/modifiers the compositor's GPU driver supports,
//! via a throwaway EGL/Wayland connection distinct from the screencast protocol one.
//!
//! No display output or physical monitor is involved — this purely probes
//! the GPU driver behind the existing Wayland compositor connection
//! (`eglGetPlatformDisplay(EGL_PLATFORM_WAYLAND_KHR, ...)` on a real
//! `wl_display*`).
//!
//! `wayland-client` in this crate uses the pure-Rust backend (no real `wl_display*`
//! to hand to EGL), so we open our own tiny libwayland-client connection here via
//! `dlopen`.
//!
//! Deliberately takes an explicit socket path rather than connecting via the
//! `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR` env vars: those are process-global, but a
//! server handling multiple concurrent user sessions has one Wayland socket per
//! session, not one per process — same reason `CaptureSession::connect` (lib.rs)
//! takes an explicit socket path instead of relying on env vars.

use khronos_egl as egl;
use std::collections::HashMap;
use std::ffi::{c_uint, c_void};
use std::os::fd::{IntoRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const EGL_PLATFORM_WAYLAND_KHR: egl::Enum = 0x31D8;

/// A DRM fourcc format this GPU/driver stack can export as DMA-BUF, with its
/// supported modifiers (first entry is the driver's preferred one).
#[derive(Clone)]
pub struct DmabufFormat {
    pub drm_fourcc: u32,
    pub modifiers: Vec<i64>,
}

/// Returns an empty list (never an error) if EGL/Wayland DMA-BUF import isn't
/// available here — callers should treat that as "fall back to MemPtr only".
///
/// Cached per `wayland_socket_path` rather than re-queried on every call
/// (previously: every single `PipewireCapture::start()`, i.e. every video-
/// pipeline rebuild — see `SessionManager::reconcile_video_pipeline`).
/// Confirmed live via `ss -xp`: each throwaway EGL/Wayland probe here opened
/// a fresh D-Bus system-bus connection (`u_str` to `dbus-broker`) and a
/// fresh PipeWire daemon connection alongside it, neither released by
/// `eglTerminate` (which only covers what EGL itself allocated — this is a
/// separate resource, almost certainly Mesa/NVIDIA's own DRM device
/// authorization via logind, a level below the EGL API surface) — two
/// sockets leaked on every rebuild that survived the earlier `eglTerminate`
/// fix. The set of formats/modifiers a GPU driver supports doesn't change
/// between calls for the same compositor anyway, so caching is correct, not
/// just a workaround.
pub fn query_dmabuf_formats(wayland_socket_path: &Path) -> Vec<DmabufFormat> {
    static CACHE: Mutex<Option<HashMap<PathBuf, Vec<DmabufFormat>>>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap();
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some(cached) = cache.get(wayland_socket_path) {
        return cached.clone();
    }
    let formats = match try_query(wayland_socket_path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("EGL DMA-BUF modifier query unavailable: {e}");
            Vec::new()
        }
    };
    cache.insert(wayland_socket_path.to_path_buf(), formats.clone());
    formats
}

fn try_query(wayland_socket_path: &Path) -> Result<Vec<DmabufFormat>, Box<dyn std::error::Error>> {
    // Throwaway libwayland-client connection, purely to hand a real wl_display* to EGL.
    // wl_display_connect_to_fd takes ownership of the fd (closed by wl_display_disconnect).
    let socket = UnixStream::connect(wayland_socket_path)?;
    let socket_fd: RawFd = socket.into_raw_fd();

    let wayland_lib = unsafe { libloading::Library::new("libwayland-client.so.0")? };
    let wl_display_connect_to_fd: libloading::Symbol<unsafe extern "C" fn(RawFd) -> *mut c_void> =
        unsafe { wayland_lib.get(b"wl_display_connect_to_fd\0")? };
    let wl_display_disconnect: libloading::Symbol<unsafe extern "C" fn(*mut c_void)> =
        unsafe { wayland_lib.get(b"wl_display_disconnect\0")? };

    let wl_display = unsafe { wl_display_connect_to_fd(socket_fd) };
    if wl_display.is_null() {
        // wl_display_connect_to_fd failed without taking ownership of the fd — close it ourselves.
        unsafe { libc::close(socket_fd) };
        return Err("wl_display_connect_to_fd returned null".into());
    }
    // Ensure we always disconnect, even on an early `?` return below.
    struct WlGuard<'a>(*mut c_void, libloading::Symbol<'a, unsafe extern "C" fn(*mut c_void)>);
    impl Drop for WlGuard<'_> {
        fn drop(&mut self) {
            unsafe { (self.1)(self.0) }
        }
    }
    let _wl_guard = WlGuard(wl_display, wl_display_disconnect);

    let egl = unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required()? };

    let display = unsafe {
        egl.get_platform_display(EGL_PLATFORM_WAYLAND_KHR, wl_display, &[egl::ATTRIB_NONE])?
    };
    egl.initialize(display)?;
    // No matching `eglTerminate` existed anywhere below (several early `?`
    // returns after this point) — every call leaked the display's
    // underlying GPU driver connection (confirmed live: `/dev/dri/
    // renderD128` growing by a fixed amount on every `PipewireCapture::
    // start()`, i.e. every video-pipeline rebuild). Same drop-guard
    // pattern as `WlGuard` above, for the same reason.
    struct EglGuard<'a>(&'a egl::DynamicInstance<egl::EGL1_5>, egl::Display);
    impl Drop for EglGuard<'_> {
        fn drop(&mut self) {
            let _ = self.0.terminate(self.1);
        }
    }
    let _egl_guard = EglGuard(&egl, display);

    type QueryFormatsFn =
        unsafe extern "C" fn(egl::EGLDisplay, egl::Int, *mut egl::Int, *mut egl::Int) -> egl::Boolean;
    type QueryModifiersFn = unsafe extern "C" fn(
        egl::EGLDisplay,
        egl::Int,
        egl::Int,
        *mut i64,
        *mut egl::Boolean,
        *mut egl::Int,
    ) -> egl::Boolean;

    let query_formats: QueryFormatsFn = unsafe {
        std::mem::transmute(
            egl.get_proc_address("eglQueryDmaBufFormatsEXT")
                .ok_or("eglQueryDmaBufFormatsEXT not available")?,
        )
    };
    let query_modifiers: QueryModifiersFn = unsafe {
        std::mem::transmute(
            egl.get_proc_address("eglQueryDmaBufModifiersEXT")
                .ok_or("eglQueryDmaBufModifiersEXT not available")?,
        )
    };

    const MAX_FORMATS: usize = 200;
    const MAX_MODIFIERS: usize = 200;

    let mut num_formats: egl::Int = 0;
    let mut formats = vec![0i32; MAX_FORMATS];
    let ok = unsafe {
        query_formats(display.as_ptr(), MAX_FORMATS as egl::Int, formats.as_mut_ptr(), &mut num_formats)
    };
    if ok == 0 {
        return Err("eglQueryDmaBufFormatsEXT failed".into());
    }
    formats.truncate(num_formats.max(0) as usize);

    let mut results = Vec::new();
    for &fourcc in &formats {
        let mut num_mods: egl::Int = 0;
        let mut mods = vec![0i64; MAX_MODIFIERS];
        let mut external_only = vec![0 as c_uint; MAX_MODIFIERS];
        let ok = unsafe {
            query_modifiers(
                display.as_ptr(),
                fourcc,
                MAX_MODIFIERS as egl::Int,
                mods.as_mut_ptr(),
                external_only.as_mut_ptr(),
                &mut num_mods,
            )
        };
        if ok == 0 {
            continue;
        }
        mods.truncate(num_mods.max(0) as usize);
        if !mods.is_empty() {
            results.push(DmabufFormat { drm_fourcc: fourcc as u32, modifiers: mods });
        }
    }

    Ok(results)
}
