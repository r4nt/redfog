//! Validates that a DMA-BUF fd captured here can be
//! correctly imported by GStreamer's `vulkanupload` and converted by
//! `vulkancolorconvert` to NV12 inside Vulkan memory.

use std::os::fd::{FromRawFd, OwnedFd};
use gstreamer::prelude::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "GStreamer's vulkanupload can't consume this system's DMA_DRM/tiled-modifier \
            caps -- confirmed a real, permanent driver limitation, not a flake: fails with \
            the same pipeline error every run. vulkan_direct_import.rs is the working \
            replacement (hand-rolled ash import bypassing vulkanupload entirely). Kept \
            around, not deleted, as a live check that GStreamer's own element still can't \
            do this -- worth knowing if/when that ever changes upstream."]
async fn dmabuf_vulkan_upload_test() {
    // See capture_integration.rs's identical call for why this is here.
    redfog_test_cleanup::ensure_active();
    let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

    let runtime_dir = std::env::temp_dir().join(format!("redfog-it-dmabuf-vk-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::env::set_var("REDFOG_RUNTIME_DIR", &runtime_dir);
    std::env::set_var("REDFOG_ALWAYS_SOFTWARE", "0");

    let _dbus_session = redfog_core::ensure_private_dbus_session();
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

    let node_id = match compositor.video_source(None) {
        redfog_core::VideoSource::PipeWireNode(node) => node,
        _ => panic!("expected a PipeWireNode video source"),
    };
    let socket_path = match &compositor {
        session_backend::SpawnedCompositor::Kwin(session) => session.socket_path.clone(),
        _ => panic!("expected a Kwin-backed compositor"),
    };
    // See capture_integration.rs's identical guard for why this is here.
    struct KillCompositorOnDrop(session_backend::SpawnedCompositor);
    impl Drop for KillCompositorOnDrop {
        fn drop(&mut self) {
            self.0.kill_best_effort();
            // kill_best_effort() only signals kwin_wayland itself, not
            // Xwayland/glxgears (kwin_wayland's *own* children, spawned via
            // --exit-with-session) -- confirmed live, those survived on
            // their own otherwise. See kill_descendants_named's doc comment.
            redfog_test_cleanup::kill_descendants_named("kwin_wayland");
        }
    }
    let _compositor_guard = KillCompositorOnDrop(compositor);

    eprintln!("Starting native Pipewire capture...");
    let capture = kwin_capture::pipewire_capture::PipewireCapture::start(node_id, socket_path, _headless_runtime.pipewire_socket.to_str().unwrap(), false).unwrap();

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
    let vulkanupload = gstreamer::ElementFactory::make("vulkanupload").build().expect("vulkanupload element");
    let vulkancolorconvert = gstreamer::ElementFactory::make("vulkancolorconvert").build().expect("vulkancolorconvert element");
    let sink = gstreamer::ElementFactory::make("fakesink").build().expect("fakesink element");

    pipeline.add_many([appsrc.upcast_ref(), &vulkanupload, &vulkancolorconvert, &sink]).unwrap();
    gstreamer::Element::link_many([appsrc.upcast_ref(), &vulkanupload, &vulkancolorconvert, &sink]).unwrap();

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
    let mut caps = drm_info.to_caps().expect("VideoInfoDmaDrm::to_caps");
    caps.get_mut().unwrap().set_features(0, None);
    eprintln!("DMA_DRM caps without features: {caps}");
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
    eprintln!("vulkanupload successfully imported the DMA-BUF frame.");
}
