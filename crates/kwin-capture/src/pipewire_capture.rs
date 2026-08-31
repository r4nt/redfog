use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::time::Duration;
use pipewire as pw;
use pw::spa;
use spa::pod::Pod;

/// DRM fourcc <-> SPA video format table. Only the two non-HDR entries we
/// actually offer.
const FORMAT_MAP: &[(u32, pw::spa::param::video::VideoFormat)] = &[
    (u32::from_le_bytes(*b"XR24"), pw::spa::param::video::VideoFormat::BGRx), // DRM_FORMAT_XRGB8888
    (u32::from_le_bytes(*b"AR24"), pw::spa::param::video::VideoFormat::BGRA), // DRM_FORMAT_ARGB8888
];

fn resolve_drm_fourcc(format: pw::spa::param::video::VideoFormat) -> u32 {
    FORMAT_MAP.iter().find(|(_, f)| *f == format).map(|(fourcc, _)| *fourcc).unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub fd: RawFd,
    pub width: u32,
    pub height: u32,
    pub stride: i32,
    pub format: u32,
    pub modifier: u64,
    pub is_dma_buf: bool,
    /// Opaque per-underlying-buffer identity — PipeWire negotiates a small
    /// fixed pool of real GPU buffers and cycles through them (confirmed
    /// live: 3 distinct buffers over 303 frames of a 5s capture), so this
    /// repeats across frames rather than being unique per frame. Callers
    /// that import each buffer into a GPU API (e.g. `glupload`'s EGLImage
    /// import) can cache that import keyed on this value instead of
    /// re-importing from scratch every single frame. Derived from the
    /// pre-`dup()` fd PipeWire itself owns for this buffer's lifetime — an
    /// identity key only, never valid for I/O (unlike `fd`, which is our
    /// own freshly-`dup()`'d, uniquely-owned copy).
    pub buffer_identity: i64,
}

pub struct PipewireCapture {
    frame_rx: Receiver<CapturedFrame>,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PipewireCapture {
    /// `wayland_socket_path`: used only for the throwaway EGL DMA-BUF modifier
    /// query (see egl_dmabuf.rs) — an explicit path rather than relying on
    /// `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR` env vars, which can't disambiguate
    /// between concurrent sessions in a multi-session server.
    ///
    /// `prefer_linear`: restrict DMA-BUF negotiation to the driver's
    /// `DRM_FORMAT_MOD_LINEAR` (0) modifier alternative only, instead of
    /// whatever it prefers (usually a proprietary tiled mode — see
    /// `cuda_import.rs`'s doc comment for why the direct-CUDA-import path
    /// needs this and the GL/`glupload` path doesn't).
    pub fn start(target_node_id: u32, wayland_socket_path: PathBuf, prefer_linear: bool) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let (frame_tx, frame_rx) = channel::<CapturedFrame>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = shutdown.clone();

        let thread = std::thread::spawn(move || {
            if let Err(e) = Self::run_loop(target_node_id, wayland_socket_path, prefer_linear, frame_tx, thread_shutdown) {
                eprintln!("Pipewire background loop failed: {e}");
            }
        });

        Ok(Self { frame_rx, shutdown, thread: Some(thread) })
    }

    fn run_loop(
        target_node_id: u32,
        wayland_socket_path: PathBuf,
        prefer_linear: bool,
        frame_tx: std::sync::mpsc::Sender<CapturedFrame>,
        shutdown: Arc<AtomicBool>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        pw::init();

        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;

        let stream = pw::stream::StreamBox::new(
            &core,
            "kwin-native-capture",
            pw::properties::properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Camera",
            },
        )?;

        struct FormatState {
            width: u32,
            height: u32,
            format: u32,
            modifier: u64,
        }

        let format_state = std::sync::Arc::new(std::sync::Mutex::new(FormatState {
            width: 0,
            height: 0,
            format: 0,
            modifier: 0,
        }));

        let format_state_clone = format_state.clone();

        let _listener = stream
            .add_local_listener_with_user_data(frame_tx)
            .state_changed(|_, _, old, new| {
                eprintln!("Pipewire stream state: {:?} -> {:?}", old, new);
            })
            .param_changed(move |stream, _, id, param| {
                let Some(param) = param else { return; };
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let (media_type, media_subtype) =
                    match pw::spa::param::format_utils::parse_format(param) {
                        Ok(v) => v,
                        Err(_) => return,
                    };
                if media_type != pw::spa::param::format::MediaType::Video
                    || media_subtype != pw::spa::param::format::MediaSubtype::Raw
                {
                    return;
                }

                let mut format_info = pw::spa::param::video::VideoInfoRaw::default();
                if format_info.parse(param).is_err() {
                    return;
                }

                let drm_format = resolve_drm_fourcc(format_info.format());
                // Authoritative "did the negotiated format actually carry a modifier" signal —
                // set by SPA only when a SPA_FORMAT_VIDEO_modifier property was present, not
                // just because the numeric modifier field happens to be nonzero.
                let has_modifier = format_info
                    .flags()
                    .contains(pw::spa::param::video::VideoFlags::MODIFIER);

                {
                    let mut state = format_state_clone.lock().unwrap();
                    state.width = format_info.size().width;
                    state.height = format_info.size().height;
                    state.format = format_info.format().as_raw();
                    state.modifier = format_info.modifier();
                }

                let use_dmabuf = has_modifier && drm_format != 0;
                eprintln!(
                    "Pipewire negotiated format: {}x{} format={} modifier={} using {}",
                    format_info.size().width, format_info.size().height, format_info.format().as_raw(),
                    format_info.modifier(), if use_dmabuf { "DMA-BUF" } else { "MemPtr" }
                );

                // React to the negotiated format by telling
                // PipeWire which single buffer type we now require — DMA-BUF only or MemPtr only,
                // never both. This must happen via update_params() *after* seeing the format, not
                // as part of the initial connect() params (which is what actually forces DMA-BUF
                // through — sending a ParamBuffers up front pushed buffer allocation onto
                // PipeWire's generic allocator, which can never produce DMA-BUF buffers).
                let buffer_type_bit = if use_dmabuf { spa::sys::SPA_DATA_DmaBuf } else { spa::sys::SPA_DATA_MemPtr };

                let buffers_obj = pw::spa::pod::object!(
                    pw::spa::utils::SpaTypes::ObjectParamBuffers,
                    pw::spa::param::ParamType::Buffers,
                    pw::spa::pod::Property {
                        key: spa::sys::SPA_PARAM_BUFFERS_dataType,
                        flags: pw::spa::pod::PropertyFlags::empty(),
                        value: pw::spa::pod::Value::Int(1i32 << buffer_type_bit),
                    },
                );

                let meta_header_obj = pw::spa::pod::object!(
                    pw::spa::utils::SpaTypes::ObjectParamMeta,
                    pw::spa::param::ParamType::Meta,
                    pw::spa::pod::Property {
                        key: spa::sys::SPA_PARAM_META_type,
                        flags: pw::spa::pod::PropertyFlags::empty(),
                        value: pw::spa::pod::Value::Id(pw::spa::utils::Id(spa::sys::SPA_META_Header)),
                    },
                    pw::spa::pod::Property {
                        key: spa::sys::SPA_PARAM_META_size,
                        flags: pw::spa::pod::PropertyFlags::empty(),
                        value: pw::spa::pod::Value::Int(std::mem::size_of::<spa::sys::spa_meta_header>() as i32),
                    },
                );

                let region_size = std::mem::size_of::<spa::sys::spa_meta_region>() as i32;
                let meta_damage_obj = pw::spa::pod::object!(
                    pw::spa::utils::SpaTypes::ObjectParamMeta,
                    pw::spa::param::ParamType::Meta,
                    pw::spa::pod::Property {
                        key: spa::sys::SPA_PARAM_META_type,
                        flags: pw::spa::pod::PropertyFlags::empty(),
                        value: pw::spa::pod::Value::Id(pw::spa::utils::Id(spa::sys::SPA_META_VideoDamage)),
                    },
                    pw::spa::pod::Property {
                        key: spa::sys::SPA_PARAM_META_size,
                        flags: pw::spa::pod::PropertyFlags::empty(),
                        value: pw::spa::pod::Value::Choice(pw::spa::pod::ChoiceValue::Int(
                            pw::spa::utils::Choice(
                                pw::spa::utils::ChoiceFlags::empty(),
                                pw::spa::utils::ChoiceEnum::Range {
                                    default: region_size * 16,
                                    min: region_size * 1,
                                    max: region_size * 16,
                                }
                            )
                        )),
                    },
                );

                let serialize = |obj: pw::spa::pod::Object| -> Vec<u8> {
                    pw::spa::pod::serialize::PodSerializer::serialize(
                        std::io::Cursor::new(Vec::new()),
                        &pw::spa::pod::Value::Object(obj),
                    )
                    .unwrap()
                    .0
                    .into_inner()
                };

                let buffers_bytes = serialize(buffers_obj);
                let meta_header_bytes = serialize(meta_header_obj);
                let meta_damage_bytes = serialize(meta_damage_obj);

                let mut response_params = [
                    Pod::from_bytes(&buffers_bytes).unwrap(),
                    Pod::from_bytes(&meta_header_bytes).unwrap(),
                    Pod::from_bytes(&meta_damage_bytes).unwrap(),
                ];
                if let Err(e) = stream.update_params(&mut response_params) {
                    eprintln!("Pipewire update_params failed: {e}");
                }
            })
            .process(move |stream, tx| {
                if let Some(mut buffer) = stream.dequeue_buffer() {
                    let datas = buffer.datas_mut();
                    if datas.is_empty() { return; }

                    let data = &datas[0];
                    if data.type_() == spa::buffer::DataType::DmaBuf || data.type_() == spa::buffer::DataType::MemFd {
                        let raw_fd = data.fd();
                        if raw_fd >= 0 {
                            let dup_fd = unsafe { libc::dup(raw_fd) };
                            if dup_fd >= 0 {
                                let state = format_state.lock().unwrap();
                                let frame = CapturedFrame {
                                    fd: dup_fd,
                                    width: state.width,
                                    height: state.height,
                                    stride: data.chunk().stride(),
                                    format: state.format,
                                    modifier: state.modifier,
                                    is_dma_buf: data.type_() == spa::buffer::DataType::DmaBuf,
                                    buffer_identity: raw_fd as i64,
                                };
                                let _ = tx.send(frame);
                            }
                        }
                    }
                }
            })
            .register()?;

        fn serialize_obj(obj: pw::spa::pod::Object) -> Vec<u8> {
            pw::spa::pod::serialize::PodSerializer::serialize(
                std::io::Cursor::new(Vec::new()),
                &pw::spa::pod::Value::Object(obj),
            )
            .unwrap()
            .0
            .into_inner()
        }

        fn base_format_properties(width: i32, height: i32) -> Vec<pw::spa::pod::Property> {
            vec![
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::MediaType,
                    Id,
                    pw::spa::param::format::MediaType::Video
                ),
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::MediaSubtype,
                    Id,
                    pw::spa::param::format::MediaSubtype::Raw
                ),
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::VideoSize,
                    Choice,
                    Range,
                    Rectangle,
                    pw::spa::utils::Rectangle { width: width as u32, height: height as u32 },
                    pw::spa::utils::Rectangle { width: 1, height: 1 },
                    pw::spa::utils::Rectangle { width: 4096, height: 4096 }
                ),
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::VideoFramerate,
                    Choice,
                    Range,
                    Fraction,
                    pw::spa::utils::Fraction { num: 60, denom: 1 },
                    pw::spa::utils::Fraction { num: 0, denom: 1 },
                    pw::spa::utils::Fraction { num: 1000, denom: 1 }
                ),
            ]
        }

        // Real DMA-BUF formats/modifiers this compositor's GPU driver actually supports,
        // queried via EGL (see egl_dmabuf.rs). Asking for DMA-BUF without a real,
        // driver-reported modifier list doesn't work — the implicit
        // DRM_FORMAT_MOD_INVALID sentinel alone isn't enough to get KWin to commit
        // to exporting one (confirmed live: it renegotiates back down to MemPtr
        // when only the sentinel is offered).
        let dmabuf_formats = crate::egl_dmabuf::query_dmabuf_formats(&wayland_socket_path);

        let mut format_pods: Vec<Vec<u8>> = Vec::new();

        // One format entry per (SPA format, real modifier list) pair, DMA-BUF only,
        // highest priority first.
        const DRM_FORMAT_MOD_LINEAR: i64 = 0;

        for &(fourcc, spa_format) in FORMAT_MAP {
            let Some(info) = dmabuf_formats.iter().find(|f| f.drm_fourcc == fourcc) else { continue };
            let mut modifiers: Vec<i64> = if prefer_linear {
                // Restrict to LINEAR only — skip this format entirely (fall
                // through to the MemPtr fallback below) if the driver doesn't
                // offer it as an alternative at all.
                info.modifiers.iter().copied().filter(|&m| m == DRM_FORMAT_MOD_LINEAR).collect()
            } else {
                info.modifiers.clone()
            };
            // NVIDIA's eglQueryDmaBufModifiersEXT (what `dmabuf_formats`
            // came from) is known to sometimes never advertise a real,
            // working tiled modifier at all, offering only LINEAR — which
            // NVIDIA's own GBM backend can never actually allocate a
            // renderable buffer with anyway (confirmed live, a general
            // NVIDIA limitation, not GPU-specific — see
            // gbm_modifier_search's own doc comment for the full story).
            // If that's genuinely all the EGL query gave us, fall back to
            // a live GBM-based search for one it forgot to mention.
            if !prefer_linear && modifiers == [DRM_FORMAT_MOD_LINEAR] {
                if let Some(tiled) = crate::gbm_modifier_search::find_working_tiled_modifier(fourcc, 1920, 1080) {
                    modifiers.insert(0, tiled as i64);
                }
            }
            if modifiers.is_empty() {
                continue;
            }
            let mut props = base_format_properties(1280, 720);
            props.push(pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoFormat,
                Id,
                spa_format
            ));
            props.push(pw::spa::pod::Property {
                key: pw::spa::param::format::FormatProperties::VideoModifier.as_raw(),
                flags: pw::spa::pod::PropertyFlags::MANDATORY | pw::spa::pod::PropertyFlags::DONT_FIXATE,
                value: pw::spa::pod::Value::Choice(pw::spa::pod::ChoiceValue::Long(
                    pw::spa::utils::Choice(
                        pw::spa::utils::ChoiceFlags::empty(),
                        pw::spa::utils::ChoiceEnum::Enum {
                            // Driver's preferred modifier first (modifiers[0]),
                            // unless prefer_linear already restricted the list.
                            default: modifiers[0],
                            alternatives: modifiers,
                        }
                    )
                )),
            });
            format_pods.push(serialize_obj(pw::spa::pod::Object {
                type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
                id: pw::spa::param::ParamType::EnumFormat.as_raw(),
                properties: props,
            }));
        }

        // Always-present fallback: no modifier property at all, so SPA settles on MemPtr.
        // Listed last (order = priority).
        let mut mem_props = base_format_properties(1280, 720);
        mem_props.push(pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::RGBA,
            pw::spa::param::video::VideoFormat::BGRA
        ));
        format_pods.push(serialize_obj(pw::spa::pod::Object {
            type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: pw::spa::param::ParamType::EnumFormat.as_raw(),
            properties: mem_props,
        }));

        // Initial connect() params: format candidates ONLY — no ParamBuffers object here.
        // That's sent reactively from param_changed once the actual negotiated format is known.
        let mut params: Vec<&Pod> = format_pods.iter().map(|bytes| Pod::from_bytes(bytes).unwrap()).collect();

        stream.connect(
            spa::utils::Direction::Input,
            Some(target_node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )?;

        // Without this, `mainloop.run()` below never returns on its own —
        // there was previously no way to stop this thread at all short of
        // the whole process exiting. Harmless as long as a `PipewireCapture`
        // is only ever created once per process lifetime, but a *rebuild*
        // (see `SessionManager::reconcile_video_pipeline`) creates a new one
        // mid-session while the old one's underlying PipeWire connection is
        // still live: the orphaned thread's `.process()` callback below kept
        // firing forever, `dup()`-ing a fresh dma-buf fd every tick and
        // silently leaking it the moment `tx.send()` started failing (this
        // capture's `frame_rx` — the only receiver — already dropped).
        // Confirmed live: fd count climbed continuously, one per captured
        // frame, immediately following the first ever rebuild. A repeating
        // timer polling the shutdown flag every 50ms (rather than needing to
        // signal an `EventSource` cross-thread, which `pipewire-rs`'s
        // `EventSource` isn't `Send` for) is simple and fast enough — this
        // only needs to notice shutdown promptly, not react to real capture
        // traffic.
        let mainloop_for_timer = mainloop.clone();
        let timer = mainloop.loop_().add_timer(move |_| {
            if shutdown.load(Ordering::Relaxed) {
                mainloop_for_timer.quit();
            }
        });
        timer.update_timer(Some(Duration::from_millis(50)), Some(Duration::from_millis(50))).into_result()?;

        mainloop.run();

        // `pw_stream_destroy`/`pw_core_disconnect` (called by `stream`/
        // `core`'s own `Drop` below, once this function returns) send an
        // async disconnect notification to the PipeWire daemon — quitting
        // the loop the instant `shutdown` is noticed doesn't give it any
        // further chance to actually flush that out or process the
        // daemon's response before the socket disappears out from under
        // it. A few more short iterations here, after the flag is already
        // set but before anything is torn down, lets that happen instead
        // of leaving it stranded.
        for _ in 0..5 {
            mainloop.loop_().iterate(pw::loop_::Timeout::Finite(Duration::from_millis(10)));
        }

        Ok(())
    }

    /// Bounded, not a plain `recv()`: a damage-driven source (KWin's virtual
    /// output) can legitimately have nothing new to deliver for an arbitrary
    /// stretch of real time (an idle desktop, or one taken over from another
    /// client with nothing currently forcing a repaint) — an unbounded
    /// `recv()` here blocked the caller's whole loop for that entire
    /// stretch, including its `shutdown` flag check, which only happens
    /// *between* calls. That in turn made `CudaDirectEncoderSession::drop`'s
    /// `thread.join()` block just as indefinitely, since setting `shutdown`
    /// alone can't wake an in-progress `recv()` — confirmed live via a gdb
    /// thread dump on a session stuck exactly this way. `None` on timeout is
    /// already a case every caller already handles identically to a real
    /// disconnect (see `nvenc_session::run`'s own `let Some(frame) = ...
    /// else { sleep; continue }`), so this changes nothing about their
    /// control flow, just how promptly they get to re-check `shutdown`.
    pub fn next_frame(&self) -> Option<CapturedFrame> {
        self.frame_rx.recv_timeout(std::time::Duration::from_millis(200)).ok()
    }
}

impl Drop for PipewireCapture {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // `join()` returning means the background thread's `.process()`
        // callback can never fire again — safe to drain exhaustively now,
        // no race with more frames still arriving. Without this, any
        // frame(s) captured in the up-to-50ms window between the shutdown
        // flag being set and the thread's own timer noticing it (see
        // `run_loop`) sit in this channel forever: `CapturedFrame` has no
        // `Drop` of its own (its `fd` is normally closed explicitly by
        // whichever of `nvenc_session::run`'s two branches consumes it), so
        // simply letting `frame_rx` drop here would silently leak each of
        // their raw fds — a small, fixed number per rebuild rather than the
        // unbounded per-frame leak the shutdown mechanism above already
        // fixed, but a real leak all the same. Confirmed live.
        while let Ok(frame) = self.frame_rx.try_recv() {
            unsafe { libc::close(frame.fd) };
        }
    }
}
