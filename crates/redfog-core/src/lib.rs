use std::path::{Path, PathBuf};
use std::process::{Command, Stdio, Child};
use std::sync::{Arc, Mutex};
use std::os::unix::net::UnixStream;
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
/// either a PipeWire node to connect to via `pipewiresrc` (KWin's
/// `CaptureSession`, which claims a virtual output and registers it in
/// PipeWire itself), or an already-constructed GStreamer source element,
/// for backends where the compositor *is* the GStreamer element (e.g.
/// gst-wayland-display's `waylanddisplaysrc`, which has no PipeWire
/// involvement at all — it hands raw frames straight to the pipeline).
pub enum VideoSource {
    PipeWireNode(u32),
    Element(gst::Element),
}

impl VideoSource {
    /// `client_name` must be unique per session/generation — GStreamer's
    /// `pipewiresrc` shares one underlying PipeWire core/thread-loop across
    /// every element in the process that resolves to the same client
    /// identity. Without a distinct name here, every session for the life of
    /// one `redfog-server` process reuses the same connection, so a single
    /// wedged (abandoned-on-timeout) pipeline permanently poisons it for
    /// every later session too — confirmed live via matching mutex addresses
    /// across generations.
    fn into_element(self, client_name: &str) -> gst::Element {
        match self {
            VideoSource::PipeWireNode(node_id) => gst::ElementFactory::make("pipewiresrc")
                .name("src")
                .property("path", node_id.to_string())
                .property("client-name", client_name)
                .property("do-timestamp", true)
                .build()
                .expect("pipewiresrc should always be available"),
            VideoSource::Element(el) => el,
        }
    }
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

        Self::wait_and_attach(session_type, socket_name, socket_path, width, height, scale, Some(child), &runtime, &pw_sock)
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
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let runtime = default_runtime_dir();
        let pw_sock = std::env::var("PIPEWIRE_REMOTE").unwrap_or_else(|_| "pipewire-0".to_string());
        Self::wait_and_attach(session_type, socket_name, socket_path, width, height, scale, None, &runtime, &pw_sock)
    }

    #[allow(clippy::too_many_arguments)]
    fn wait_and_attach(
        session_type: SessionType,
        socket_name: &str,
        socket_path: PathBuf,
        width: i32,
        height: i32,
        scale: f64,
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
        let capture_session = CaptureSession::connect(&socket_path, "redfog-output", width, height, scale)?;
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

    pub fn handoff(&mut self, next_session: &CompositorSession) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let new_input_forwarder = InputForwarder::connect(&next_session.socket_path)?;
        if let Some(src) = self.pipeline.by_name("src") {
            src.set_state(gst::State::Null).ok();
            src.set_property("path", next_session.pipewire_node_id.to_string());
            src.set_state(gst::State::Playing).ok();
            eprintln!("redfog-core: GStreamer source path updated to {}!", next_session.pipewire_node_id);
        }
        self.input_forwarder = new_input_forwarder;
        Ok(())
    }
}

pub fn make_pipeline<F>(
    source: VideoSource,
    client_name: &str,
    frame_store: Arc<Mutex<Option<Frame>>>,
    on_frame: F,
) -> gst::Pipeline
where
    F: Fn(bool) + Send + Sync + 'static,
{
    let src = source.into_element(client_name);
    let downstream = gst::parse_bin_from_description(
        "videoconvert ! video/x-raw,format=BGRx ! appsink name=sink sync=false",
        true,
    )
    .expect("downstream bin parse failed");

    let pipeline = gst::Pipeline::new();
    pipeline.add(&src).expect("failed to add source to pipeline");
    pipeline.add(&downstream).expect("failed to add downstream bin to pipeline");
    src.link(&downstream).expect("failed to link source to downstream bin");

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

/// Name of the `x264enc` element in the pipeline built by
/// [`make_encoder_pipeline`], so callers can address it (e.g. [`request_keyframe`]).
const ENCODER_ELEMENT_NAME: &str = "enc";

/// Capture -> H.264 encode pipeline for network streaming (as opposed to
/// [`make_pipeline`]'s raw-BGRx path for local display). Delivers Annex-B
/// access units (one per encoded frame) to `on_access_unit(bytes, is_keyframe)`.
///
/// Software `x264enc` for this iteration; `nvh264enc` is a drop-in swap once
/// hardware encoding is worth the complexity (see project notes on the
/// NVIDIA GBM issue — encoding is a separate GPU path from KWin's virtual
/// backend rendering, so it isn't necessarily blocked by the same bug).
pub fn make_encoder_pipeline<F>(
    source: VideoSource,
    client_name: &str,
    bitrate_kbps: u32,
    on_access_unit: F,
) -> gst::Pipeline
where
    F: Fn(Vec<u8>, bool) + Send + Sync + 'static,
{
    let src = source.into_element(client_name);
    let downstream_desc = format!(
        "videoconvert \
         ! video/x-raw,format=I420 \
         ! x264enc name={ENCODER_ELEMENT_NAME} tune=zerolatency speed-preset=ultrafast \
                   byte-stream=true key-int-max=300 bitrate={bitrate_kbps} \
         ! video/x-h264,stream-format=byte-stream,alignment=au \
         ! appsink name=sink sync=false"
    );
    let downstream = gst::parse_bin_from_description(&downstream_desc, true)
        .expect("downstream encoder bin parse failed");

    let pipeline = gst::Pipeline::new();
    pipeline.add(&src).expect("failed to add source to pipeline");
    pipeline.add(&downstream).expect("failed to add downstream bin to pipeline");
    src.link(&downstream).expect("failed to link source to downstream bin");

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

/// Capture -> Opus encode pipeline for network streaming: `pipewiresrc`
/// targeting an [`AudioLoopback`]'s capture side -> stereo 48kHz -> Opus.
/// Delivers one encoded Opus packet per callback invocation.
///
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
pub fn make_audio_pipeline<F>(loopback: &AudioLoopback, client_name: &str, on_packet: F) -> gst::Pipeline
where
    F: Fn(Vec<u8>) + Send + Sync + 'static,
{
    let desc = format!(
        "pipewiresrc target-object={capture_name} client-name={client_name} do-timestamp=true \
         ! audioconvert ! audioresample \
         ! audio/x-raw,format=S16LE,channels=2,rate=48000 \
         ! opusenc frame-size=5 \
         ! appsink name=sink sync=false",
        capture_name = loopback.capture_name,
    );
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
