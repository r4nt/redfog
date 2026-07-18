use std::path::{Path, PathBuf};
use std::process::{Command, Stdio, Child};
use std::sync::{Arc, Mutex};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gst::prelude::*;

use wayland_client::{
    delegate_noop,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_registry, wl_seat},
    Connection, Dispatch, QueueHandle,
};

pub use kwin_capture::CaptureSession;

mod environment;
pub use environment::{ensure_private_dbus_session, HeadlessRuntime};

/// Shared by `HeadlessRuntime::start` and `CompositorSession::spawn` so the
/// PipeWire runtime dir and the KWin socket dir always agree.
///
/// Overridable via `REDFOG_RUNTIME_DIR` — lets a self-contained integration
/// test run its own isolated compositor/PipeWire/paired-client-state
/// instance (see `redfog-moonlight/tests/`) without colliding with a real
/// `redfog-server` that might already be running on the same machine using
/// the default path.
pub fn default_runtime_dir() -> String {
    std::env::var("REDFOG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp/redfog-runtime".to_string())
}

// Define fake_input module generated from protocols/fake-input.xml
pub mod fake_input {
    #![allow(
        dead_code, non_camel_case_types, unused_unsafe, unused_variables,
        non_upper_case_globals, non_snake_case, unused_imports, missing_docs,
        clippy::all
    )]
    pub mod client {
        use wayland_client;
        use wayland_client::protocol::*;
        use wayland_backend;
        pub mod __interfaces {
            use wayland_client::protocol::__interfaces::*;
            use wayland_backend;
            wayland_scanner::generate_interfaces!("protocols/fake-input.xml");
        }
        use self::__interfaces::*;
        wayland_scanner::generate_client_code!("protocols/fake-input.xml");
    }
}

pub use fake_input::client::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;

pub struct WaylandState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandState {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event,
             _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_noop!(WaylandState: ignore wl_seat::WlSeat);
delegate_noop!(WaylandState: ignore OrgKdeKwinFakeInput);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionType {
    Login,
    User(String), // username
}

pub struct CompositorSession {
    pub session_type: SessionType,
    pub socket_name: String,
    pub socket_path: PathBuf,
    /// `None` for a session spawned by redfog-broker (KWin runs under a
    /// systemd unit we don't own a child handle for) — see `attach()`.
    /// `terminate()`/`try_wait()` handle that case by being a no-op; the
    /// caller is responsible for asking the broker to tear it down.
    kwin_process: Option<Child>,
    pub capture_session: CaptureSession,
    pub pipewire_node_id: u32,
}

/// A video frame source for [`make_pipeline`]/[`make_encoder_pipeline`] —
/// plain data only, deliberately never a pre-built `gst::Element`. Every
/// variant here gets embedded directly into one literal pipeline
/// description string per (source, encoder) combination — see
/// `pipewire_encoder_pipeline_description`'s doc comment for why that
/// matters (a caps bug hid for a long time specifically because a fragment
/// of the pipeline was constructed away from where the rest of it was
/// visible). `GstWaylandDisplay`/`Login` used to hold an already-constructed
/// `gst::Element` instead — that required the source's owner (`session-
/// backend`) to build it *before* any pipeline existed, which is also why
/// `input_sink()`/frame-pushing used to need special-casing. Neither
/// backend actually needs that: gst-wayland-display's Wayland socket only
/// exists once the pipeline reaches `Playing` anyway (nothing waits on it
/// before that), and Login's reader thread only needs a channel to relay
/// frames through, not a live `AppSrc` handle up front.
pub enum VideoSource {
    PipeWireNode(u32),
    /// gst-wayland-display's `waylanddisplaysrc` — the compositor *is* this
    /// element, no PipeWire involvement at all. `render_node` is either a
    /// real DRM render node path or `gst_backend::RENDER_NODE_SOFTWARE`.
    GstWaylandDisplay { render_node: String, width: i32, height: i32, fps: u32 },
    /// Login's `appsrc`, fed via `frame_rx` — `redfog-login` renders its own
    /// frames (`tiny-skia`, no GPU) and ships them over a Unix socket; a
    /// background thread (`session_backend::spawn_login_compositor`) relays
    /// each one onto this channel instead of pushing into an `AppSrc`
    /// directly, since the `AppSrc` doesn't exist until the pipeline this
    /// source ends up in is actually built.
    Login { frame_rx: std::sync::mpsc::Receiver<Vec<u8>>, width: u32, height: u32 },
}

/// What a video pipeline's captured frames end up as — the other half of
/// [`video_pipeline_description`]'s `(source, sink)` match, alongside
/// [`VideoSource`].
pub enum VideoSink {
    /// Raw BGRx frames for local display — [`make_pipeline`]'s only
    /// consumer, the `viewer` debug tool. No encoding at all.
    LocalDisplay,
    /// H.264-encoded access units for network streaming —
    /// [`make_encoder_pipeline`]'s only consumer, the real Moonlight server.
    Encode { encoder: VideoEncoder, bitrate_kbps: u32 },
}

/// Where compositor input events go — implemented differently per backend.
/// KWin's [`InputForwarder`] sends these over `org_kde_kwin_fake_input`, a
/// Wayland protocol; a gst-wayland-display backend would instead send
/// `CustomUpstream` GStreamer events (`MouseMoveRelative`, `KeyboardKey`,
/// etc. — see gst-wayland-display's `gst-plugin-wayland-display/src/
/// waylandsrc/imp.rs`) to its `waylanddisplaysrc` element. Method shapes
/// here mirror `OrgKdeKwinFakeInput`'s directly, since both backends'
/// underlying event vocabularies already match closely.
pub trait InputSink: Send {
    fn keyboard_key(&mut self, keycode: u32, pressed: bool);
    fn pointer_motion(&mut self, dx: f64, dy: f64);
    fn pointer_motion_absolute(&mut self, x: f64, y: f64);
    fn button(&mut self, button: u32, pressed: bool);
    fn axis(&mut self, axis: u32, value: f64);
    /// Apply queued events — required for Wayland's fake_input (an explicit
    /// `wl_display_flush`), a no-op for backends whose event delivery is
    /// already synchronous (e.g. `GstElement::send_event`).
    fn flush(&mut self) {}
}

impl CompositorSession {
    /// The abstracted form of `pipewire_node_id`, for callers that build
    /// pipelines via [`make_pipeline`]/[`make_encoder_pipeline`] against
    /// [`VideoSource`] rather than a raw node id.
    pub fn video_source(&self) -> VideoSource {
        VideoSource::PipeWireNode(self.pipewire_node_id)
    }

    pub fn spawn(
        session_type: SessionType,
        socket_name: &str,
        width: i32,
        height: i32,
        scale: f64,
        fps: u32,
        payload_args: &[String],
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let runtime = default_runtime_dir();
        let runtime_path = Path::new(&runtime);
        let socket_path = runtime_path.join(socket_name);

        // Clean up stale socket files if they exist
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_file(runtime_path.join(format!("{}.lock", socket_name)));

        let pw_sock = std::env::var("PIPEWIRE_REMOTE")
            .unwrap_or_else(|_| "pipewire-0".to_string());

        let mut cmd = Command::new("kwin_wayland");
        cmd.env("KWIN_PLATFORM", "virtual")
            .env("KWIN_WAYLAND_NO_PERMISSION_CHECKS", "1")
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("PIPEWIRE_REMOTE", &pw_sock)
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .arg("--virtual")
            .arg("--width")
            .arg(&width.to_string())
            .arg("--height")
            .arg(&height.to_string())
            .arg("--scale")
            .arg(&scale.to_string())
            .arg("--no-lockscreen")
            .arg("--socket")
            .arg(socket_name)
            .arg("--xwayland");

        if !payload_args.is_empty() {
            cmd.arg("--exit-with-session");
            cmd.arg(&payload_args[0]);
            if payload_args.len() > 1 {
                cmd.arg("--");
                for arg in &payload_args[1..] {
                    cmd.arg(arg);
                }
            }
        }

        let child = cmd.stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?;

        Self::wait_and_attach(session_type, socket_name, socket_path, width, height, scale, fps, Some(child), &runtime, &pw_sock)
    }

    /// For a session already spawned by redfog-broker (KWin running under a
    /// templated systemd unit, its Wayland socket bound via systemd socket
    /// activation — see design.md's "Cross-user socket reachability")
    /// — connects to that already-existing socket instead of spawning
    /// `kwin_wayland` ourselves.
    pub fn attach(
        session_type: SessionType,
        socket_name: &str,
        socket_path: PathBuf,
        width: i32,
        height: i32,
        scale: f64,
        fps: u32,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let runtime = default_runtime_dir();
        let pw_sock = std::env::var("PIPEWIRE_REMOTE").unwrap_or_else(|_| "pipewire-0".to_string());
        Self::wait_and_attach(session_type, socket_name, socket_path, width, height, scale, fps, None, &runtime, &pw_sock)
    }

    #[allow(clippy::too_many_arguments)]
    fn wait_and_attach(
        session_type: SessionType,
        socket_name: &str,
        socket_path: PathBuf,
        width: i32,
        height: i32,
        scale: f64,
        fps: u32,
        mut child: Option<Child>,
        runtime: &str,
        pw_sock: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // Wait for compositor Wayland socket file to appear
        let mut found = false;
        for _ in 0..60 {
            if socket_path.exists() {
                found = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }

        if !found {
            if let Some(child) = &mut child {
                child.kill().ok();
            }
            return Err(format!("KWin Wayland socket {:?} failed to appear", socket_path).into());
        }

        // Update D-Bus activation environment so services connect to this compositor socket
        Command::new("dbus-update-activation-environment")
            .arg("--systemd")
            .arg(format!("WAYLAND_DISPLAY={}", socket_name))
            .arg(format!("XDG_RUNTIME_DIR={}", runtime))
            .arg(format!("PIPEWIRE_REMOTE={}", pw_sock))
            .spawn()
            .and_then(|mut c| c.wait())
            .ok();

        // Connect CaptureSession to claim virtual output and get PipeWire node ID
        let capture_session = CaptureSession::connect(&socket_path, "redfog-output", width, height, scale, fps)?;
        let pipewire_node_id = capture_session.node_id();

        Ok(Self {
            session_type,
            socket_path,
            socket_name: socket_name.to_string(),
            kwin_process: child,
            capture_session,
            pipewire_node_id,
        })
    }


    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, std::io::Error> {
        match &mut self.kwin_process {
            Some(child) => child.try_wait(),
            None => Ok(None), // broker-owned; caller tracks liveness separately
        }
    }

    pub fn terminate(mut self) {
        if let Some(mut child) = self.kwin_process.take() {
            child.kill().ok();
            child.wait().ok();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }

    /// Non-blocking subset of `terminate()` — just the signal, no `wait()` —
    /// for use as a `Drop` safety net (see `RunningSession`'s `Drop` impl in
    /// `redfog-moonlight`), where blocking on a possibly-wedged child (the
    /// same class of hang `terminate()`'s own `wait()` can suffer from,
    /// confirmed live for the Login stage's reader-thread `join()`) would be
    /// actively harmful: `Drop` can run at unpredictable points (e.g. a
    /// `HashMap::insert` silently dropping a replaced value), and a call
    /// this deep down inside `&mut self` doesn't get to be `async` or
    /// `spawn_blocking`'d. An unreaped zombie left behind by skipping
    /// `wait()` is a tiny, harmless cost next to leaking this process's own
    /// gigabytes of GStreamer/PipeWire-mapped buffers forever — confirmed
    /// live to actually happen (see the OOM incident in project memory).
    pub fn kill_best_effort(&mut self) {
        if let Some(child) = self.kwin_process.as_mut() {
            let _ = child.kill();
        }
    }
}

pub struct InputForwarder {
    pub fake_input: OrgKdeKwinFakeInput,
    pub conn: Connection,
    pub queue: wayland_client::EventQueue<WaylandState>,
}

impl InputForwarder {
    pub fn connect(socket_path: &Path) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let stream = UnixStream::connect(socket_path)?;
        let conn = Connection::from_socket(stream)?;
        let (globals, mut queue) = registry_queue_init::<WaylandState>(&conn)?;
        let qh = queue.handle();

        let fake_input: OrgKdeKwinFakeInput = globals
            .bind(&qh, 4..=6, ())
            .map_err(|e| format!("org_kde_kwin_fake_input not available: {e}"))?;

        let mut state = WaylandState;
        fake_input.authenticate(
            "redfog-viewer".to_string(),
            "input forwarding for game streaming".to_string(),
        );
        conn.flush()?;
        queue.roundtrip(&mut state)?;
        Ok(Self { fake_input, conn, queue })
    }
}

impl InputSink for InputForwarder {
    fn keyboard_key(&mut self, keycode: u32, pressed: bool) {
        self.fake_input.keyboard_key(keycode, pressed as u32);
    }
    fn pointer_motion(&mut self, dx: f64, dy: f64) {
        self.fake_input.pointer_motion(dx, dy);
    }
    fn pointer_motion_absolute(&mut self, x: f64, y: f64) {
        self.fake_input.pointer_motion_absolute(x, y);
    }
    fn button(&mut self, button: u32, pressed: bool) {
        self.fake_input.button(button, pressed as u32);
    }
    fn axis(&mut self, axis: u32, value: f64) {
        self.fake_input.axis(axis, value);
    }
    fn flush(&mut self) {
        let _ = self.conn.flush();
    }
}

#[derive(Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

pub struct StreamingEngine {
    pub pipeline: gst::Pipeline,
    pub input_forwarder: InputForwarder,
}

impl StreamingEngine {
    pub fn new(
        initial_session: &CompositorSession,
        frame_store: Arc<Mutex<Option<Frame>>>,
        on_frame: impl Fn(bool) + Send + Sync + 'static,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let input_forwarder = InputForwarder::connect(&initial_session.socket_path)?;
        let client_name = format!("redfog-streaming-engine-{}", std::process::id());
        let pipeline = make_pipeline(initial_session.video_source(), &client_name, frame_store, on_frame);
        pipeline.set_state(gst::State::Playing)?;
        Ok(Self { pipeline, input_forwarder })
    }

    /// Looks up `pipewiresrc` directly by name (not a wrapping "src" bin,
    /// the way this method's own `pipeline` used to be built) — matches
    /// [`make_pipeline`]'s `VideoSource::PipeWireNode` case, which no
    /// longer wraps `pipewiresrc` in an intermediate bin at all (one flat
    /// `gst::parse_launch` string now, see that function's doc comment).
    /// [`StreamingEngine`] itself is currently dead code (unused anywhere
    /// in this workspace) but kept consistent with the rest of this file's
    /// pipeline-construction approach rather than left referencing a
    /// naming scheme that no longer exists.
    pub fn handoff(&mut self, next_session: &CompositorSession) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let new_input_forwarder = InputForwarder::connect(&next_session.socket_path)?;
        if let Some(src) = self.pipeline.by_name("pipewiresrc") {
            src.set_state(gst::State::Null).ok();
            src.set_property("path", next_session.pipewire_node_id.to_string());
            src.set_state(gst::State::Playing).ok();
            eprintln!("redfog-core: GStreamer source path updated to {}!", next_session.pipewire_node_id);
        }
        self.input_forwarder = new_input_forwarder;
        Ok(())
    }
}

/// Relays frames arriving on `frame_rx` (see `VideoSource::Login`'s doc
/// comment — a background thread in `session-backend` writes to the other
/// end, forwarding what it reads off Login's Unix socket) into the
/// `login-appsrc` element a pipeline built from a [`VideoSource::Login`]
/// description always contains. Runs on its own thread for the pipeline's
/// lifetime; exits cleanly whenever either side goes away (`frame_rx`
/// disconnects when the socket reader thread exits, `push_buffer` errors
/// once the pipeline itself is torn down).
fn spawn_login_frame_pusher(pipeline: &gst::Pipeline, frame_rx: std::sync::mpsc::Receiver<Vec<u8>>) {
    let app_src = pipeline
        .by_name("login-appsrc")
        .expect("a VideoSource::Login pipeline always names its appsrc \"login-appsrc\"")
        .dynamic_cast::<gst_app::AppSrc>()
        .expect("login-appsrc is always an appsrc");
    std::thread::spawn(move || {
        for frame in frame_rx {
            let mut buffer = gst::Buffer::with_size(frame.len()).expect("buffer allocation");
            {
                let buffer_mut = buffer.get_mut().expect("freshly allocated buffer is never shared");
                buffer_mut.copy_from_slice(0, &frame).expect("buffer sized exactly for frame");
            }
            if app_src.push_buffer(buffer).is_err() {
                break; // pipeline gone/EOS
            }
        }
    });
}

/// THE single place — in this crate, and by extension this whole workspace,
/// since no other crate constructs a GStreamer element or pipeline string at
/// all anymore (see `VideoSource`'s doc comment) — that knows the actual
/// gst-launch syntax for any video pipeline this project builds. Every
/// `(source, sink)` combination is one complete, self-contained literal
/// string, spelled out in full in its own match arm right here, not
/// composed from separately-named "capture"/"downstream" helper functions
/// called from elsewhere the way earlier versions of this file did.
///
/// That composition was tried, twice, and both times caused a real bug to
/// hide: first, a shared downstream-description function taking a
/// `framerate: Option<u32>` that secretly also meant "is this a PipeWire
/// source or not" — a fixed-framerate caps request placed in the wrong half
/// didn't visibly fail, it just silently never reached `videorate`. Fixed
/// by folding capture+downstream into one string per source... which then
/// turned out to *still* be two copies of the same `pipewiresrc`/
/// `videorate` text (one here, one in [`make_pipeline`]) that had already
/// quietly drifted — only one of them had the `(ANY)` caps-features fix
/// below, since nothing forced a change to one copy to touch the other.
/// Both problems have the same root cause: pipeline construction living in
/// more than one place, however each place is organized internally.
/// Accepting the literal duplication across match arms below (e.g. the
/// `pipewiresrc`/`videorate` text appears in all three `PipeWireNode` arms)
/// is the actual fix — it trades a smaller *character* count for a
/// genuinely single, greppable, all-in-one-screen place to read and change
/// this project's entire GStreamer surface.
///
/// `client_name`/`fps` only matter for `VideoSource::PipeWireNode` (see the
/// `PipeWireNode` arms below for why) — every other source carries its own
/// `fps` as part of its own data and ignores both parameters entirely.
fn video_pipeline_description(source: &VideoSource, client_name: &str, fps: u32, sink: &VideoSink) -> String {
    match (source, sink) {
        (VideoSource::PipeWireNode(node_id), VideoSink::LocalDisplay) => {
            format!(
                "pipewiresrc name=pipewiresrc path={node_id} client-name=\"{client_name}\" \
                             do-timestamp=true keepalive-time={} \
                 ! videorate name=videorate skip-to-first=true \
                 ! video/x-raw(ANY),framerate={fps}/1 \
                 ! videoconvert \
                 ! video/x-raw,format=BGRx \
                 ! appsink name=sink sync=false",
                2000 / fps,
            )
        }
        // `client_name` must be unique per session/generation — GStreamer's
        // `pipewiresrc` shares one underlying PipeWire core/thread-loop
        // across every element in the process that resolves to the same
        // client identity. Without a distinct name here, every session for
        // the life of one `redfog-server` process reuses the same
        // connection, so a single wedged (abandoned-on-timeout) pipeline
        // permanently poisons it for every later session too — confirmed
        // live via matching mutex addresses across generations.
        //
        // `video/x-raw(ANY),framerate={fps}/1` — the `(ANY)` caps-features
        // tag is load-bearing, not decoration. A caps structure written
        // without an explicit features tag (`video/x-raw,framerate=...`, no
        // `(...)`) means *only* `memory:SystemMemory` in GStreamer's caps-
        // negotiation model — it does not mean "any memory type, this
        // format." User-caught bug: that plain form sat here through every
        // earlier version of this pipeline, silently forcing a SystemMemory
        // download right after `videorate`, on *every* encoder path —
        // including `Nvenc`'s GL/DMA-BUF arm below, which never had a real
        // chance to get a DMA-BUF from `pipewiresrc` because a hard
        // SystemMemory boundary already sat between them. That's almost
        // certainly the real explanation for `glupload` measuring a ~30fps
        // ceiling instead of the requested 120 earlier in this pipeline's
        // history: it was silently doing its own slow CPU-driven texture
        // upload from system memory the whole time, never a DMA-BUF import,
        // regardless of where the fixed-framerate caps request itself was
        // anchored (an explanation floated and disproved at the time — this
        // is the actual root cause that explanation missed). `(ANY)` fixes
        // the framerate the same way while leaving the memory feature open
        // for downstream to negotiate — `Software`'s `videoconvert` still
        // only accepts system memory so still gets exactly that, `Nvenc`'s
        // `glupload` can now actually receive DMA-BUF if `pipewiresrc`
        // offers it. `queue` is a thread boundary between capture and
        // convert/encode: without it, `pipewiresrc` pushes buffers
        // synchronously from its own PipeWire I/O thread straight through
        // conversion into the encoder, serializing all of that CPU work
        // onto one core no matter how many others are idle (confirmed live
        // via per-thread /proc sampling: ~28% of a core at 1920x1080@120fps
        // with a CPU `videoconvert` ahead of the encoder). `leaky=downstream`
        // + a small `max-size-buffers` keeps it a pure threading boundary,
        // not a latency-adding buffer.
        (VideoSource::PipeWireNode(node_id), VideoSink::Encode { encoder: VideoEncoder::Software, bitrate_kbps }) => {
            format!(
                "pipewiresrc name=pipewiresrc path={node_id} client-name=\"{client_name}\" \
                             do-timestamp=true keepalive-time={} \
                 ! videorate name=videorate skip-to-first=true \
                 ! video/x-raw(ANY),framerate={fps}/1 \
                 ! queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 \
                 ! videoconvert \
                 ! video/x-raw,format=I420 \
                 ! x264enc name={ENCODER_ELEMENT_NAME} tune=zerolatency speed-preset=ultrafast \
                           byte-stream=true key-int-max=300 bitrate={bitrate_kbps} \
                 ! video/x-h264,stream-format=byte-stream,alignment=au \
                 ! appsink name=sink sync=false",
                2000 / fps,
            )
        }
        // No CPU `videoconvert` here — `nvh264enc`'s sink pad accepts a wide
        // format list directly (confirmed via `gst-inspect-1.0 nvh264enc`).
        // `glupload`/`glcolorconvert`/DMA-BUF instead: KWin's virtual output
        // is confirmed live (`nvidia-smi --query-compute-apps` showing a
        // real C+G GPU context on `kwin_wayland --virtual`, at only ~8% CPU
        // compositing a continuously-damaged 1920x1080@120Hz scene — far too
        // cheap for software rasterization) to be genuinely GPU-composited
        // (an earlier assumption that it wasn't, based on
        // `LIBGL_ALWAYS_SOFTWARE=1` being set on every `kwin_wayland` spawn,
        // was wrong: that's a Mesa-only knob NVIDIA's driver doesn't read).
        // If `pipewiresrc` offers `memory:DMABuf` for this node, `glupload`
        // imports it directly into a GL texture (EGLImage import, no CPU
        // pixel touch at all); `nvh264enc` consumes it via CUDA-GL interop.
        // `glcolorconvert` is a cheap GPU-side no-op unless a format
        // mismatch needs fixing. Needs `GST_GL_WINDOW=surfaceless`/
        // `GST_GL_PLATFORM=egl` set on the process (see `redfog-server::
        // main`) since this process has no real display for GL context
        // auto-detection to find.
        //
        // `videorate` is load-bearing, not optional: tried putting the fixed
        // `framerate={fps}/1` request directly on `pipewiresrc`'s own output
        // instead (`pipewiresrc` has no dedicated fps *property* — confirmed
        // via `gst-inspect-1.0 pipewiresrc` — so this meant relying entirely
        // on GStreamer's pipewire plugin translating that caps request into
        // the SPA stream-format parameters it negotiates with the actual
        // PipeWire graph, hoping KWin's screencast producer could satisfy an
        // exact fixed rate directly, since its virtual output's own refresh
        // rate is already configured to this same `fps`). Confirmed live
        // that it can't: negotiation failed outright ("no more input
        // formats" / `not-negotiated` on `pipewiresrc` itself) — KWin's
        // screencast stream is fundamentally *variable*-rate, matching its
        // damage-driven rendering model (a configured refresh rate is a
        // ceiling on how often it *can* repaint when there's damage, not a
        // promise to always produce a frame every interval). `videorate`
        // downstream is what actually bridges that variable input into the
        // fixed rate `rc-mode=cbr` needs — there's no way to make the
        // source itself produce a genuinely fixed rate here.
        // ! queue leaky=downstream max-size-buffers=3 max-size-bytes=0 max-size-time=0 \
         //        ! videorate name=videorate skip-to-first=true \
         //        ! video/x-raw(memory:DMABuf),framerate={fps}/1 \
        (VideoSource::PipeWireNode(node_id), VideoSink::Encode { encoder: VideoEncoder::Nvenc, bitrate_kbps }) => {
            format!(
                 "pipewiresrc name=pipewiresrc path={node_id} client-name=\"{client_name}\" \
                             do-timestamp=true keepalive-time={} \
                 ! nvh264enc name={ENCODER_ELEMENT_NAME} zerolatency=true tune=ultra-low-latency \
                             rc-mode=cbr repeat-sequence-header=true gop-size=300 bitrate={bitrate_kbps} \
                 ! video/x-h264,stream-format=byte-stream,alignment=au \
                 ! appsink name=sink sync=false",
                2000 / fps,
            )
        }
        // `waylanddisplaysrc` — the compositor *is* this element (see
        // `VideoSource::GstWaylandDisplay`'s doc comment). The capsfilter
        // (`width`/`height`/`framerate` all fixed) is required, not
        // cosmetic: `waylanddisplaysrc` has no width/height *property*, only
        // a wide negotiable caps range with no default resolution of its
        // own — left unconstrained, it negotiates down to a literal `1x1`
        // frame (confirmed live: an unconstrained pipeline renders nothing).
        (VideoSource::GstWaylandDisplay { render_node, width, height, fps }, VideoSink::LocalDisplay) => format!(
            "waylanddisplaysrc name=waylanddisplaysrc render-node=\"{render_node}\" \
             ! video/x-raw,width={width},height={height},framerate={fps}/1 \
             ! videoconvert \
             ! video/x-raw,format=BGRx \
             ! appsink name=sink sync=false"
        ),
        // `identity name={FPS_CAP_ELEMENT_NAME}` + `install_fps_cap_probe`
        // (not `videorate`): `waylanddisplaysrc` is a damage/event-driven
        // source, not PipeWire's own steady stream — `videorate` broke on
        // exactly this kind of source (see `install_fps_cap_probe`'s doc
        // comment). No GL/DMA-BUF path even here: `waylanddisplaysrc` is a
        // comparatively simple/young plugin with no DMA-BUF export of its
        // own (unlike KWin — see the `PipeWireNode`/`Nvenc` arm above).
        (VideoSource::GstWaylandDisplay { render_node, width, height, fps }, VideoSink::Encode { encoder: VideoEncoder::Software, bitrate_kbps }) => format!(
            "waylanddisplaysrc name=waylanddisplaysrc render-node=\"{render_node}\" \
             ! video/x-raw,width={width},height={height},framerate={fps}/1 \
             ! queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 \
             ! identity name={FPS_CAP_ELEMENT_NAME} \
             ! videoconvert \
             ! video/x-raw,format=I420 \
             ! x264enc name={ENCODER_ELEMENT_NAME} tune=zerolatency speed-preset=ultrafast \
                       byte-stream=true key-int-max=300 bitrate={bitrate_kbps} \
             ! video/x-h264,stream-format=byte-stream,alignment=au \
             ! appsink name=sink sync=false"
        ),
        (VideoSource::GstWaylandDisplay { render_node, width, height, fps }, VideoSink::Encode { encoder: VideoEncoder::Nvenc, bitrate_kbps }) => format!(
            "waylanddisplaysrc name=waylanddisplaysrc render-node=\"{render_node}\" \
             ! video/x-raw,width={width},height={height},framerate={fps}/1 \
             ! queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 \
             ! identity name={FPS_CAP_ELEMENT_NAME} \
             ! video/x-raw \
             ! nvh264enc name={ENCODER_ELEMENT_NAME} zerolatency=true tune=ultra-low-latency \
                         rc-mode=cbr repeat-sequence-header=true gop-size=300 bitrate={bitrate_kbps} \
             ! video/x-h264,stream-format=byte-stream,alignment=au \
             ! appsink name=sink sync=false"
        ),
        // Login's `appsrc` — see `VideoSource::Login`'s doc comment for how
        // frames actually reach it (a background thread relays them onto a
        // channel; `spawn_login_frame_pusher` drains that channel into this
        // `appsrc`, named `login-appsrc`, once the pipeline built from this
        // description exists). `caps=` fixes `redfog-login`'s own fixed
        // `tiny-skia` canvas format/size up front (no negotiable range to
        // fall back on, same reasoning as `GstWaylandDisplay`'s capsfilter).
        (VideoSource::Login { width, height, .. }, VideoSink::LocalDisplay) => format!(
            "appsrc name=login-appsrc format=time is-live=true block=false \
             caps=video/x-raw,format=RGBA,width={width},height={height},framerate=30/1 \
             ! videoconvert \
             ! video/x-raw,format=BGRx \
             ! appsink name=sink sync=false"
        ),
        // Never a GL/DMA-BUF path: `redfog-login`'s `tiny-skia` renderer has
        // zero GPU/compositor dependency by design, always plain software-
        // rendered system memory. Confirmed live: requesting `memory:DMABuf`
        // unconditionally (back when Login and the PipeWire case shared one
        // downstream-description function) broke the Login stage outright
        // (`not-negotiated` on `GstAppSrc:login-appsrc`) before ever
        // reaching the real question of whether KWin/PipeWire has DMA-BUF.
        (VideoSource::Login { width, height, .. }, VideoSink::Encode { encoder: VideoEncoder::Software, bitrate_kbps }) => format!(
            "appsrc name=login-appsrc format=time is-live=true block=false \
             caps=video/x-raw,format=RGBA,width={width},height={height},framerate=30/1 \
             ! queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 \
             ! identity name={FPS_CAP_ELEMENT_NAME} \
             ! videoconvert \
             ! video/x-raw,format=I420 \
             ! x264enc name={ENCODER_ELEMENT_NAME} tune=zerolatency speed-preset=ultrafast \
                       byte-stream=true key-int-max=300 bitrate={bitrate_kbps} \
             ! video/x-h264,stream-format=byte-stream,alignment=au \
             ! appsink name=sink sync=false"
        ),
        (VideoSource::Login { width, height, .. }, VideoSink::Encode { encoder: VideoEncoder::Nvenc, bitrate_kbps }) => format!(
            "appsrc name=login-appsrc format=time is-live=true block=false \
             caps=video/x-raw,format=RGBA,width={width},height={height},framerate=30/1 \
             ! queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 \
             ! identity name={FPS_CAP_ELEMENT_NAME} \
             ! video/x-raw \
             ! nvh264enc name={ENCODER_ELEMENT_NAME} zerolatency=true tune=ultra-low-latency \
                         rc-mode=cbr repeat-sequence-header=true gop-size=300 bitrate={bitrate_kbps} \
             ! video/x-h264,stream-format=byte-stream,alignment=au \
             ! appsink name=sink sync=false"
        ),
    }
}

/// Delivers raw BGRx frames for local display — the `viewer` debug tool's
/// only consumer. Gets its pipeline string from [`video_pipeline_description`]
/// (the single place that owns every pipeline string this crate builds —
/// see that function's own doc comment) with [`VideoSink::LocalDisplay`],
/// same as [`make_encoder_pipeline`] does with [`VideoSink::Encode`].
pub fn make_pipeline<F>(
    source: VideoSource,
    client_name: &str,
    frame_store: Arc<Mutex<Option<Frame>>>,
    on_frame: F,
) -> gst::Pipeline
where
    F: Fn(bool) + Send + Sync + 'static,
{
    let full_desc = video_pipeline_description(&source, client_name, 60, &VideoSink::LocalDisplay);
    let pipeline = gst::parse_launch(&full_desc)
        .unwrap_or_else(|e| panic!("failed to build local-display pipeline: {e}\n(pipeline description: {full_desc:?})"))
        .dynamic_cast::<gst::Pipeline>()
        .expect("gst::parse_launch on a plain top-level description (no bin.(...)) always yields a Pipeline");
    if let VideoSource::Login { frame_rx, .. } = source {
        spawn_login_frame_pusher(&pipeline, frame_rx);
    }

    let appsink = pipeline
        .by_name("sink").unwrap()
        .dynamic_cast::<gst_app::AppSink>().unwrap();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                let s = caps.structure(0).ok_or(gst::FlowError::Error)?;
                let w = s.get::<i32>("width").map_err(|_| gst::FlowError::Error)? as u32;
                let h = s.get::<i32>("height").map_err(|_| gst::FlowError::Error)? as u32;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let data = map.to_vec();
                let mut store = frame_store.lock().unwrap();
                let changed = store.as_ref().map(|f| f.width != w || f.height != h).unwrap_or(true);
                *store = Some(Frame { width: w, height: h, data });
                on_frame(changed);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipeline
}

/// Name of the H.264 encoder element (`x264enc` or `nvh264enc`, whichever
/// [`VideoEncoder`] selected) in the pipeline built by
/// [`make_encoder_pipeline`], so callers can address it (e.g. [`request_keyframe`]).
/// Kept identical across both so `request_keyframe` stays encoder-agnostic —
/// both subclass `GstVideoEncoder`, which handles the upstream
/// force-key-unit event generically.
const ENCODER_ELEMENT_NAME: &str = "enc";

/// Name of the always-present `identity` element at the head of the
/// downstream encoder bin — see `install_fps_cap_probe`'s doc comment for
/// why capping is done via a runtime pad probe on this element rather than
/// embedded in the pipeline description string itself.
const FPS_CAP_ELEMENT_NAME: &str = "fps_cap_gate";

/// Which H.264 encoder [`make_encoder_pipeline`] builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoEncoder {
    /// `x264enc` — always available, no GPU dependency. Default: safe on
    /// any machine, including CI/dev boxes without an NVIDIA GPU.
    #[default]
    Software,
    /// `nvh264enc` (NVCODEC/NVENC) — confirmed live on an RTX 2080 to work
    /// cleanly with plain system-memory NV12 input (no explicit CUDA
    /// upload element needed; the element negotiates its own CUDA context
    /// and does the upload internally). This is a genuinely separate GPU
    /// path from KWin's virtual-output rendering (DRM/GBM), which is why
    /// it isn't blocked by the unrelated `gbm_create_device` segfault seen
    /// there — see project notes on the NVIDIA GBM issue.
    Nvenc,
}

impl VideoEncoder {
    pub fn as_str(&self) -> &'static str {
        match self {
            VideoEncoder::Software => "software",
            VideoEncoder::Nvenc => "nvenc",
        }
    }
}

/// Wire/env-var representation — `"software"` / `"nvenc"`
/// (`REDFOG_VIDEO_ENCODER`, see `redfog-server::main`), mirroring
/// `session_backend::Backend`'s `FromStr` shape.
impl std::str::FromStr for VideoEncoder {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "software" => Ok(VideoEncoder::Software),
            "nvenc" => Ok(VideoEncoder::Nvenc),
            other => Err(format!("unknown video encoder {other:?} (expected \"software\" or \"nvenc\")")),
        }
    }
}

/// Picks `Nvenc` if the `nvh264enc` element is registered, `Software`
/// otherwise. Requires `gst::init()` to have already run (element factory
/// lookups return nothing before then) — `redfog-server::main` calls this
/// after `gstreamer::init()`, only as the fallback when `REDFOG_VIDEO_ENCODER`
/// isn't set explicitly, so the env var always wins over auto-detection in
/// either direction.
///
/// Deliberately just a factory-registration check, not a real pipeline
/// construction attempt: cheap, and doesn't open a CUDA context just to
/// answer the question. That means this can say "available" for a plugin
/// that's installed but unhealthy (wrong driver, no GPU) — a real
/// mismatch will only surface when [`make_encoder_pipeline`] actually
/// tries to build the pipeline, which is why *that* failure path needs to
/// say something useful (see its own panic message) rather than relying
/// on this check to have already ruled it out.
pub fn detect_video_encoder() -> VideoEncoder {
    if gst::ElementFactory::find("nvh264enc").is_some() {
        eprintln!("redfog-core: nvh264enc is available, defaulting to hardware video encoding");
        VideoEncoder::Nvenc
    } else {
        eprintln!("redfog-core: nvh264enc not found, defaulting to software video encoding (x264enc)");
        VideoEncoder::Software
    }
}


/// Installs a buffer probe on `element`'s `src` pad that drops any buffer
/// arriving less than `1/fps` seconds (measured in real wall-clock time,
/// not buffer PTS/pipeline clock) after the last one it let through.
///
/// Deliberately not `videorate`: confirmed live that inserting `videorate
/// max-rate={fps}` ahead of the encoder broke streaming completely —
/// input buffers arrived continuously, but it pushed exactly one output
/// buffer ever and dropped everything after, root cause not fully
/// understood (see `make_encoder_pipeline`'s git history for the
/// investigation). This is a deliberately much simpler mechanism: a
/// stateless-per-buffer decision with no lookahead and no internal
/// scheduling clock — nothing here can get stuck waiting for a "next"
/// buffer, because it never looks at anything but the single buffer
/// currently in hand and a plain `Instant` recorded from the last one it
/// allowed through. Wall-clock time, not the buffer's own PTS, specifically
/// so it can't be confused by whatever pipeline segment/base-time
/// weirdness broke `videorate` in the first place. Same "ceiling, not a
/// forced rate" property as `videorate max-rate` was meant to have: only
/// ever drops excess buffers, never invents/duplicates ones, so a source
/// producing frames slower than the cap is untouched.
fn install_fps_cap_probe(element: &gst::Element, fps: u32) {
    let min_interval = Duration::from_secs_f64(1.0 / fps as f64);
    let last_allowed: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let pad = element.static_pad("src").expect("identity element always has a src pad");
    pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
        let now = Instant::now();
        let mut last = last_allowed.lock().unwrap();
        let allow = match *last {
            None => true,
            Some(t) => now.duration_since(t) >= min_interval,
        };
        if allow {
            *last = Some(now);
            gst::PadProbeReturn::Ok
        } else {
            gst::PadProbeReturn::Drop
        }
    });
}

/// Capture -> H.264 encode pipeline for network streaming (as opposed to
/// [`make_pipeline`]'s raw-BGRx path for local display). Delivers Annex-B
/// access units (one per encoded frame) to `on_access_unit(bytes, is_keyframe)`.
///
/// `fps_cap`: `None` leaves capture fully dynamic/damage-driven, byte-
/// identical to the pipeline before fps capping existed at all (the
/// `identity` gate element is still there, but nothing ever attaches a
/// probe to it, so it's a true no-op). `Some(fps)` attaches a buffer-drop
/// probe via `install_fps_cap_probe` — see its doc comment for why that
/// mechanism (not `videorate`, which broke this pipeline outright — see
/// git history) was chosen.
///
/// Gets its pipeline string from [`video_pipeline_description`] — the
/// single place that owns every pipeline string this crate builds, see that
/// function's own doc comment — with `VideoSink::Encode { encoder,
/// bitrate_kbps }`. No source is ever a pre-built `gst::Element` glued on
/// with `.link()` — see `VideoSource`'s doc comment for why that used to be
/// true for two of the three variants, and why it turned out not to
/// actually be necessary.
///
/// `fps_cap`: for the damage-driven sources (`GstWaylandDisplay`/`Login`),
/// `None` leaves capture fully dynamic, byte-identical to the pipeline
/// before fps capping existed at all (the `identity` gate element is still
/// there, but nothing ever attaches a probe to it, so it's a true no-op);
/// `Some(fps)` attaches a buffer-drop probe via `install_fps_cap_probe` —
/// see its doc comment for why that mechanism (not `videorate`, which broke
/// this pipeline outright for a damage-driven source — see git history) was
/// chosen. PipeWire's own steady stream paces itself via `videorate`
/// instead — no probe involved there at all.
pub fn make_encoder_pipeline<F>(
    source: VideoSource,
    client_name: &str,
    encoder: VideoEncoder,
    fps_cap: Option<u32>,
    bitrate_kbps: u32,
    on_access_unit: F,
) -> gst::Pipeline
where
    F: Fn(Vec<u8>, bool) + Send + Sync + 'static,
{
    let fps = fps_cap.unwrap_or(60);
    let full_desc = video_pipeline_description(&source, client_name, fps, &VideoSink::Encode { encoder, bitrate_kbps });
    // Named/self-contained panic message (not a bare `.expect()`) so a
    // missing/broken encoder plugin says exactly that, rather than a
    // generic "parse failed" with no indication of *which* encoder or
    // *why* — this is the failure mode `detect_video_encoder`'s doc
    // comment warns about (plugin registered but unhealthy driver/no GPU),
    // and it's much more common in practice than a typo in the pipeline
    // description string.
    let pipeline = gst::parse_launch(&full_desc)
        .unwrap_or_else(|e| {
            panic!(
                "failed to build the {encoder:?} video pipeline: {e}\n\
                 (pipeline description: {full_desc:?})\n\
                 If this is Nvenc, force REDFOG_VIDEO_ENCODER=software to rule out a broken/mismatched NVENC driver install."
            )
        })
        .dynamic_cast::<gst::Pipeline>()
        .expect("gst::parse_launch on a plain top-level description (no bin.(...)) always yields a Pipeline");

    if !matches!(source, VideoSource::PipeWireNode(_)) {
        if let Some(fps) = fps_cap.filter(|&fps| fps > 0) {
            let gate = pipeline.by_name(FPS_CAP_ELEMENT_NAME).expect("identity gate element always present for a damage-driven source");
            install_fps_cap_probe(&gate, fps);
        }
    }
    if let VideoSource::Login { frame_rx, .. } = source {
        spawn_login_frame_pusher(&pipeline, frame_rx);
    }

    let appsink = pipeline
        .by_name("sink").unwrap()
        .dynamic_cast::<gst_app::AppSink>().unwrap();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let is_keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                on_access_unit(map.to_vec(), is_keyframe);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipeline
}


/// Force the next frame out of a [`make_encoder_pipeline`] pipeline to be a
/// keyframe — used to honor Moonlight's `RequestIdrFrame`/
/// `InvalidateReferenceFrames` control messages after packet loss.
pub fn request_keyframe(pipeline: &gst::Pipeline) {
    let Some(encoder) = pipeline.by_name(ENCODER_ELEMENT_NAME) else {
        return;
    };
    let event = gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
    encoder.send_event(event);
}

/// Live-adjust a [`make_encoder_pipeline`] pipeline's target bitrate — no
/// rebuild/reconnect needed: `bitrate` is `changeable in NULL, READY,
/// PAUSED or PLAYING state` on both `x264enc` and `nvh264enc` (confirmed
/// via `gst-inspect-1.0`), and an H.264 bitstream doesn't need the decoder
/// told anything when it changes — a decoder just decodes whatever NAL
/// units arrive, whatever size they are. For server-side adaptive
/// bitrate, reacting to the client's `LossStats` control-channel reports
/// (see `control::ControlEventHandler::on_loss_stats`) — unlike a
/// resolution/fps change, this needs no client-side protocol support at
/// all, which is why it's the tractable half of "live renegotiation" (see
/// project notes on the Foundation-Sunshine dynamic-stream-param-change
/// extension, which bundles resolution/fps in too — those genuinely do
/// need the client to know, since decoder output geometry isn't something
/// downstream rendering can just shrug off).
pub fn set_encoder_bitrate(pipeline: &gst::Pipeline, bitrate_kbps: u32) {
    let Some(encoder) = pipeline.by_name(ENCODER_ELEMENT_NAME) else {
        return;
    };
    encoder.set_property("bitrate", bitrate_kbps);
}

/// A per-session virtual audio sink: apps in the compositor session play
/// audio to `sink_name`, which we then capture from `capture_name`. Backed
/// by `pw-loopback` rather than PipeWire's own graph, since nothing creates
/// a session-specific sink in `HeadlessRuntime`'s isolated PipeWire instance
/// otherwise.
///
/// `HeadlessRuntime`'s PipeWire instance is isolated in D-Bus/socket
/// namespace only — `/dev/snd` itself isn't namespaced, so wireplumber's
/// ALSA monitor still sees and claims the host's *real* hardware sink there
/// too, and by default picks it (not our loopback) as `default.audio.sink`.
/// Confirmed live: without forcing the default below, an app's audio linked
/// straight to `alsa_output.<real-card>` — playing out the host's actual
/// speakers, completely bypassing capture, while this pipeline still
/// happily encoded/sent real (just near-silent) packets the whole time, no
/// error anywhere in that chain.
pub struct AudioLoopback {
    pub sink_name: String,
    pub capture_name: String,
    process: Child,
}

impl AudioLoopback {
    /// Spawn a loopback named after `session_name` (e.g. the compositor's
    /// socket name, to keep it unique per session).
    pub fn spawn(session_name: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let sink_name = format!("redfog-audio-sink-{session_name}");
        let capture_name = format!("redfog-audio-capture-{session_name}");

        let process = Command::new("pw-loopback")
            .arg("-n")
            .arg(format!("redfog-audio-{session_name}"))
            .arg("--capture-props")
            .arg(format!("media.class=Audio/Sink node.name={sink_name}"))
            .arg("--playback-props")
            .arg(format!("media.class=Audio/Source node.name={capture_name}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to spawn pw-loopback: {e}"))?;

        // Force this session's sink to be the default target for new audio
        // streams — see the struct doc comment for why this can't be left
        // to wireplumber's own default-node policy. Setting
        // `default.configured.audio.sink` on the "default" metadata object
        // (not the separate "settings" one — that name looks right but
        // doesn't actually drive default-node selection, confirmed live)
        // is picked up by wireplumber's already-running default-nodes
        // module, including re-routing streams that linked to the old
        // default *before* this ran — no restart of the app or of
        // wireplumber itself needed. Best-effort: a session should still
        // work (just possibly without audio) rather than fail outright if
        // `pw-metadata` is missing or this particular PipeWire build wires
        // default-node selection differently.
        match Command::new("pw-metadata")
            .args(["-n", "default", "0", "default.configured.audio.sink", &format!(r#"{{"name":"{sink_name}"}}"#)])
            .output()
        {
            Ok(output) if output.status.success() => {
                eprintln!("redfog-core: set default.configured.audio.sink to {sink_name}");
            }
            Ok(output) => eprintln!(
                "redfog-core: pw-metadata set default.configured.audio.sink to {sink_name} exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(e) => eprintln!("redfog-core: failed to run pw-metadata to set default.configured.audio.sink to {sink_name}: {e}"),
        }

        Ok(Self {
            sink_name,
            capture_name,
            process,
        })
    }
}

impl Drop for AudioLoopback {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// `frame-size=5`, NOT the more common VoIP default of 20ms: Moonlight's
/// wire protocol hardcodes a 5ms audio packet duration on the *client* side
/// (confirmed by reading moonlight-common-rust — not vendored into git, see
/// scripts/fetch-patched-deps.sh — `stream/proto/mod.rs`'s
/// `audio_packet_duration = Duration::from_millis(5)`, used to compute
/// `samples_per_frame` for `OpusMultistreamConfig`). A downstream client
/// (e.g. a WebRTC relay) that paces playback using that negotiated
/// `samples_per_frame` value has no way to know we're actually sending 4x
/// as much audio per packet as that implies — confirmed live: with
/// `frame-size=20`, a WebRTC-based client's presentation clock advanced 4x
/// slower than real audio arrived, causing a deterministic (not
/// random-packet-loss-driven) queue-up-then-flush every few seconds —
/// silence, then a fast, garbled burst, on a perfectly regular cycle.
/// Split out from `make_audio_pipeline` purely so this literal is
/// unit-testable without needing a live PipeWire capture behind it.
fn audio_pipeline_description(capture_name: &str, client_name: &str) -> String {
    format!(
        "pipewiresrc target-object={capture_name} client-name={client_name} do-timestamp=true \
         ! audioconvert ! audioresample \
         ! audio/x-raw,format=S16LE,channels=2,rate=48000 \
         ! opusenc frame-size=5 \
         ! appsink name=sink sync=false"
    )
}

/// Capture -> Opus encode pipeline for network streaming: `pipewiresrc`
/// targeting an [`AudioLoopback`]'s capture side -> stereo 48kHz -> Opus.
/// Delivers one encoded Opus packet per callback invocation.
pub fn make_audio_pipeline<F>(loopback: &AudioLoopback, client_name: &str, on_packet: F) -> gst::Pipeline
where
    F: Fn(Vec<u8>) + Send + Sync + 'static,
{
    let desc = audio_pipeline_description(&loopback.capture_name, client_name);
    let pipeline = gst::parse_launch(&desc)
        .expect("audio pipeline parse failed")
        .dynamic_cast::<gst::Pipeline>()
        .unwrap();
    let appsink = pipeline
        .by_name("sink").unwrap()
        .dynamic_cast::<gst_app::AppSink>().unwrap();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                on_packet(map.to_vec());
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipeline
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards against silently reverting to the more common VoIP default of
    /// `frame-size=20` — the exact regression that caused a real, live
    /// symptom (a WebRTC-relaying client's playback clock running 4x too
    /// slow, since it paces against a `samples_per_frame` value derived
    /// from Moonlight's own hardcoded 5ms assumption). Deliberately a plain
    /// string check, not a constructed `gst::Pipeline` — this doesn't need
    /// GStreamer initialized or a live PipeWire capture behind it at all.
    #[test]
    fn audio_pipeline_requests_5ms_opus_frames() {
        let desc = audio_pipeline_description("some-capture-node", "some-client");
        assert!(desc.contains("opusenc frame-size=5"), "pipeline description: {desc}");
    }

    /// Can't assert *which* encoder without depending on the test machine
    /// having (or not having) an NVENC-capable GPU — this just guards the
    /// detection logic itself: it must agree with a direct factory lookup,
    /// not e.g. always return `Software` regardless of what's installed.
    #[test]
    fn detect_video_encoder_matches_element_factory_lookup() {
        gst::init().expect("gst::init");
        let expected = if gst::ElementFactory::find("nvh264enc").is_some() {
            VideoEncoder::Nvenc
        } else {
            VideoEncoder::Software
        };
        assert_eq!(detect_video_encoder(), expected);
    }

    /// Cheap sanity check that the fps-cap gate element is present for the
    /// damage-driven sources' `Encode` arms — no GStreamer pipeline needed,
    /// just the description string. `videorate`-based pacing (`PipeWireNode`)
    /// never has this gate at all.
    #[test]
    fn damage_driven_sources_have_the_fps_cap_gate() {
        let gst_wayland_display = video_pipeline_description(
            &VideoSource::GstWaylandDisplay { render_node: "software".to_string(), width: 1280, height: 720, fps: 60 },
            "test-client",
            60,
            &VideoSink::Encode { encoder: VideoEncoder::Software, bitrate_kbps: 10_000 },
        );
        assert!(gst_wayland_display.contains(&format!("identity name={FPS_CAP_ELEMENT_NAME}")), "pipeline description: {gst_wayland_display}");
        assert!(!gst_wayland_display.contains("videorate"), "pipeline description: {gst_wayland_display}");

        let (login_tx, login_rx) = std::sync::mpsc::channel();
        drop(login_tx);
        let login = video_pipeline_description(
            &VideoSource::Login { frame_rx: login_rx, width: 1280, height: 720 },
            "test-client",
            60,
            &VideoSink::Encode { encoder: VideoEncoder::Software, bitrate_kbps: 10_000 },
        );
        assert!(login.contains(&format!("identity name={FPS_CAP_ELEMENT_NAME}")), "pipeline description: {login}");
        assert!(!login.contains("videorate"), "pipeline description: {login}");
    }

    /// Neither `VideoSource::GstWaylandDisplay` nor `VideoSource::Login` can
    /// ever satisfy a `memory:DMABuf` caps request (see `video_pipeline_
    /// description`'s doc comment). Confirmed live for Login specifically:
    /// requesting it unconditionally (back when Login and the PipeWire case
    /// shared one downstream-description function) broke the Login stage
    /// outright (`not-negotiated` on `GstAppSrc:login-appsrc`) before the
    /// pipeline ever got as far as the real PipeWire capture this was meant
    /// to test — exactly the kind of mistake having every combination
    /// spelled out explicitly, in one match, should make structurally
    /// harder to make again.
    #[test]
    fn only_the_pipewire_pipeline_requests_dmabuf() {
        let (login_tx, login_rx) = std::sync::mpsc::channel();
        drop(login_tx);
        let login = video_pipeline_description(
            &VideoSource::Login { frame_rx: login_rx, width: 1280, height: 720 },
            "test-client",
            60,
            &VideoSink::Encode { encoder: VideoEncoder::Nvenc, bitrate_kbps: 10_000 },
        );
        assert!(!login.contains("DMABuf"), "Login pipeline description: {login}");
        assert!(!login.contains("glupload"), "Login pipeline description: {login}");

        let gst_wayland_display = video_pipeline_description(
            &VideoSource::GstWaylandDisplay { render_node: "software".to_string(), width: 1280, height: 720, fps: 60 },
            "test-client",
            60,
            &VideoSink::Encode { encoder: VideoEncoder::Nvenc, bitrate_kbps: 10_000 },
        );
        assert!(!gst_wayland_display.contains("DMABuf"), "gst-wayland-display pipeline description: {gst_wayland_display}");
        assert!(!gst_wayland_display.contains("glupload"), "gst-wayland-display pipeline description: {gst_wayland_display}");

        let pipewire = video_pipeline_description(
            &VideoSource::PipeWireNode(56),
            "test-client",
            120,
            &VideoSink::Encode { encoder: VideoEncoder::Nvenc, bitrate_kbps: 10_000 },
        );
        assert!(pipewire.contains("DMABuf"), "PipeWire pipeline description: {pipewire}");
        assert!(pipewire.contains("glupload"), "PipeWire pipeline description: {pipewire}");
    }

    /// Real `gst::parse_launch` syntax/structure check for the `Software`
    /// encoder — string-content assertions (like the test above) can't
    /// catch a genuine grammar mistake (unbalanced parens, a bad property
    /// name, etc.) in the generated description. Doesn't need a live
    /// PipeWire node behind it: constructing/parsing a `pipewiresrc`
    /// element doesn't itself connect to anything — that only happens on
    /// the state change to `READY`/`PAUSED`, which this test never makes.
    ///
    /// `Nvenc` deliberately isn't checked here, and can't be with a fake
    /// node id — confirmed while writing this test: `pipewiresrc`'s static
    /// pad template caps are `ANY` (`gst-inspect-1.0 pipewiresrc`; it only
    /// knows its *real* caps once actually connected to a live node), so
    /// `gst_parse_launch`'s structural link check for the `Nvenc` arm's
    /// explicit `video/x-raw(memory:DMABuf)` caps filter fails immediately
    /// against a non-existent node ("queue1 can't handle caps
    /// video/x-raw(memory:DMABuf)") — not because the pipeline description
    /// is wrong, but because there's nothing live upstream to negotiate
    /// concrete caps against. That's a real, load-bearing gap in what this
    /// crate can verify offline: whether DMA-BUF is actually negotiable at
    /// all is only knowable against a real, running KWin/PipeWire capture
    /// (confirmed live, separately, that it does successfully negotiate
    /// there).
    #[test]
    fn pipewire_pipeline_description_parses_for_software_encoder() {
        gst::init().expect("gst::init");
        let desc = video_pipeline_description(
            &VideoSource::PipeWireNode(999_999),
            "parse-check",
            120,
            &VideoSink::Encode { encoder: VideoEncoder::Software, bitrate_kbps: 10_000 },
        );
        let el = gst::parse_launch(&desc).unwrap_or_else(|e| panic!("pipeline failed to parse: {e}\n{desc}"));
        assert!(el.dynamic_cast::<gst::Pipeline>().is_ok(), "pipeline description: {desc}");
    }

    /// Real, live pipeline test — the whole reason `videorate` shipped
    /// broken is that it was only ever tested against a well-behaved
    /// continuous source, not a rapid-burst pattern (exactly what real
    /// damage-driven capture produces). This deliberately exercises both
    /// halves: a burst gets throttled to one buffer, AND — critically,
    /// the part a naive test would have missed — a later buffer arriving
    /// after the cap's interval has elapsed still gets through, proving
    /// this can't get permanently stuck the way `videorate` did.
    #[test]
    fn fps_cap_probe_throttles_bursts_without_stalling() {
        gst::init().expect("gst::init");

        let appsrc = gst_app::AppSrc::builder()
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "RGBx")
                    .field("width", 4)
                    .field("height", 4)
                    .field("framerate", gst::Fraction::new(0, 1))
                    .build(),
            )
            .build();
        let identity = gst::ElementFactory::make("identity").name(FPS_CAP_ELEMENT_NAME).build().unwrap();
        let appsink = gst_app::AppSink::builder().sync(false).build();

        let pipeline = gst::Pipeline::new();
        pipeline.add_many([appsrc.upcast_ref(), &identity, appsink.upcast_ref()]).unwrap();
        gst::Element::link_many([appsrc.upcast_ref(), &identity, appsink.upcast_ref()]).unwrap();

        install_fps_cap_probe(&identity, 10); // 10fps cap => 100ms minimum spacing

        let received = Arc::new(Mutex::new(0u32));
        let received_cb = received.clone();
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let _ = sink.pull_sample();
                    *received_cb.lock().unwrap() += 1;
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        pipeline.set_state(gst::State::Playing).expect("pipeline should reach Playing");

        // A rapid burst — matches the "20 buffers in a fraction of a
        // second" pattern confirmed live to permanently break videorate.
        for _ in 0..20 {
            let buffer = gst::Buffer::with_size(16 * 4 * 4).unwrap();
            appsrc.push_buffer(buffer).expect("push_buffer should succeed");
        }
        std::thread::sleep(Duration::from_millis(50));
        let after_burst = *received.lock().unwrap();
        assert_eq!(after_burst, 1, "expected only the first buffer of a rapid burst to pass, got {after_burst}");

        // The critical assertion videorate would have failed: does this
        // recover, or is it stuck forever after the first buffer?
        std::thread::sleep(Duration::from_millis(150));
        let buffer = gst::Buffer::with_size(16 * 4 * 4).unwrap();
        appsrc.push_buffer(buffer).expect("push_buffer should succeed");
        std::thread::sleep(Duration::from_millis(50));
        let after_wait = *received.lock().unwrap();
        assert_eq!(after_wait, 2, "expected a buffer arriving after the cap interval to pass (not stuck), got {after_wait}");

        let _ = pipeline.set_state(gst::State::Null);
    }
}
