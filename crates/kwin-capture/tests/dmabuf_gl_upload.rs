//! Validates the one genuinely new-ground piece needed to wire `PipewireCapture`
//! into redfog-server's real encoder pipeline: can a DMA-BUF fd captured here be
//! correctly imported by GStreamer's `glupload` (the element `nvh264enc` actually
//! needs upstream of it, since `nvh264enc`'s sink pad only accepts
//! `memory:CUDAMemory`/`memory:GLMemory`/system memory — confirmed via
//! `gst-inspect-1.0 nvh264enc` — never `memory:DMABuf` directly)?
//!
//! Uses `gstreamer_video::VideoInfoDmaDrm` to build correct `format=DMA_DRM,
//! drm-format=<fourcc>:<modifier>` caps (required by `glupload`'s own sink
//! template — confirmed via `gst-inspect-1.0 glupload` — a plain `format=BGRx`
//! caps won't match), and `gstreamer_allocators::DmaBufAllocator` to wrap the fd
//! as `gst::Memory` without a copy.

use std::os::fd::{FromRawFd, OwnedFd};
use gstreamer::prelude::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dmabuf_gl_upload_test() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let runtime_dir = std::env::temp_dir().join(format!("redfog-it-dmabuf-gl-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::env::set_var("REDFOG_RUNTIME_DIR", &runtime_dir);
    std::env::set_var("REDFOG_ALWAYS_SOFTWARE", "0");
    // glupload needs a headless GL context — no real display for auto-detection
    // to find (see redfog-core's doc comment on the Nvenc/PipeWireNode arm).
    std::env::set_var("GST_GL_WINDOW", "surfaceless");
    std::env::set_var("GST_GL_PLATFORM", "egl");

    redfog_core::ensure_private_dbus_session();
    let _headless_runtime = redfog_core::HeadlessRuntime::start(runtime_dir).unwrap();

    eprintln!("Spawning KWin running glxgears...");
    let compositor = session_backend::spawn_user_compositor_direct(
        session_backend::Backend::Kwin,
        "user",
        &["glxgears".to_string()],
        1280,
        720,
        60,
    ).unwrap();

    let node_id = match compositor.video_source() {
        redfog_core::VideoSource::PipeWireNode(node) => node,
        _ => panic!("expected a PipeWireNode video source"),
    };
    let socket_path = match &compositor {
        session_backend::SpawnedCompositor::Kwin(session) => session.socket_path.clone(),
        _ => panic!("expected a Kwin-backed compositor"),
    };

    eprintln!("Starting native Pipewire capture...");
    let capture = kwin_capture::pipewire_capture::PipewireCapture::start(node_id, socket_path).unwrap();

    let mut frame = None;
    for _ in 0..60 {
        if let Some(f) = capture.next_frame() {
            if f.is_dma_buf {
                frame = Some(f);
                break;
            }
            unsafe { libc::close(f.fd) };
        }
    }
    let frame = frame.expect("expected a DMA-BUF frame within 60 attempts");
    eprintln!(
        "Got DMA-BUF frame: {}x{} format={} modifier={} stride={}",
        frame.width, frame.height, frame.format, frame.modifier, frame.stride
    );

    gstreamer::init().unwrap();

    let pipeline = gstreamer::Pipeline::new();
    let appsrc = gstreamer_app::AppSrc::builder()
        .name("src")
        .format(gstreamer::Format::Time)
        .is_live(true)
        .build();
    let glupload = gstreamer::ElementFactory::make("glupload").build().expect("glupload element");
    let glcolorconvert = gstreamer::ElementFactory::make("glcolorconvert").build().expect("glcolorconvert element");
    let sink = gstreamer::ElementFactory::make("fakesink").build().expect("fakesink element");

    pipeline.add_many([appsrc.upcast_ref(), &glupload, &glcolorconvert, &sink]).unwrap();
    gstreamer::Element::link_many([appsrc.upcast_ref(), &glupload, &glcolorconvert, &sink]).unwrap();

    let gst_format = match frame.format {
        8 => gstreamer_video::VideoFormat::Bgrx,
        12 => gstreamer_video::VideoFormat::Bgra,
        other => panic!("unexpected SPA video format id {other}"),
    };

    let video_info = gstreamer_video::VideoInfo::builder(gst_format, frame.width, frame.height)
        .build()
        .expect("valid VideoInfo");
    let drm_info = gstreamer_video::VideoInfoDmaDrm::from_video_info(&video_info, frame.modifier)
        .expect("VideoInfoDmaDrm::from_video_info");
    let caps = drm_info.to_caps().expect("VideoInfoDmaDrm::to_caps");
    eprintln!("DMA_DRM caps: {caps}");
    appsrc.set_caps(Some(&caps));

    let allocator = gstreamer_allocators::DmaBufAllocator::new();
    let size = (frame.stride as u32 * frame.height) as usize;
    let owned_fd = unsafe { OwnedFd::from_raw_fd(frame.fd) };
    let memory = unsafe { allocator.alloc(owned_fd, size).expect("DmaBufAllocator::alloc") };

    let mut buffer = gstreamer::Buffer::new();
    {
        let buffer_mut = buffer.get_mut().expect("freshly allocated buffer is never shared");
        buffer_mut.append_memory(memory);
        gstreamer_video::VideoMeta::add_full(
            buffer_mut,
            gstreamer_video::VideoFrameFlags::empty(),
            gst_format,
            frame.width,
            frame.height,
            &[0],
            &[frame.stride],
        ).expect("VideoMeta::add_full");
    }

    pipeline.set_state(gstreamer::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();

    appsrc.push_buffer(buffer).expect("push_buffer failed");
    appsrc.end_of_stream().ok();

    let mut reached_eos = false;
    for msg in bus.iter_timed(gstreamer::ClockTime::from_seconds(5)) {
        match msg.view() {
            gstreamer::MessageView::Eos(_) => {
                reached_eos = true;
                break;
            }
            gstreamer::MessageView::Error(err) => {
                panic!("pipeline error: {} ({:?})", err.error(), err.debug());
            }
            _ => {}
        }
    }
    pipeline.set_state(gstreamer::State::Null).ok();
    assert!(reached_eos, "pipeline did not reach EOS within 5s");
    eprintln!("glupload successfully imported the DMA-BUF frame.");
}
