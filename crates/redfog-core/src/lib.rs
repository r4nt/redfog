use std::path::{Path, PathBuf};
use std::process::{Command, Stdio, Child};
use std::sync::{Arc, Mutex};
use std::os::unix::net::UnixStream;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gst::prelude::*;

use wayland_client::{
    delegate_noop,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_registry, wl_seat},
    Connection, Dispatch, QueueHandle,
};

pub use kwin_capture::CaptureSession;

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
    kwin_process: Child,
    pub capture_session: CaptureSession,
    pub pipewire_node_id: u32,
}

impl CompositorSession {
    pub fn spawn(
        session_type: SessionType,
        socket_name: &str,
        width: i32,
        height: i32,
        scale: f64,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let runtime = "/tmp/redfog-runtime".to_string();
        let runtime_path = Path::new(&runtime);
        let socket_path = runtime_path.join(socket_name);

        // Clean up stale socket files if they exist
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_file(runtime_path.join(format!("{}.lock", socket_name)));

        let pw_sock = std::env::var("PIPEWIRE_REMOTE")
            .unwrap_or_else(|_| "pipewire-0".to_string());

        let mut child = Command::new("kwin_wayland")
            .env("KWIN_PLATFORM", "virtual")
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
            .arg("--xwayland")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?;

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
            child.kill().ok();
            return Err(format!("KWin Wayland socket {:?} failed to appear", socket_path).into());
        }

        // Connect CaptureSession to claim virtual output and get PipeWire node ID
        let capture_session = CaptureSession::connect(&socket_path, "redfog-output", width, height, scale)?;
        let pipewire_node_id = capture_session.node_id();

        Ok(Self {
            session_type,
            socket_name: socket_name.to_string(),
            socket_path,
            kwin_process: child,
            capture_session,
            pipewire_node_id,
        })
    }

    pub fn launch_payload(&self, command_args: &[&str]) -> Result<Child, Box<dyn std::error::Error + Send + Sync>> {
        if command_args.is_empty() {
            return Err("Empty command arguments".into());
        }

        let runtime = "/tmp/redfog-runtime".to_string();

        let child = Command::new(command_args[0])
            .args(&command_args[1..])
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .spawn()?;

        Ok(child)
    }

    pub fn terminate(mut self) {
        self.kwin_process.kill().ok();
        self.kwin_process.wait().ok();
        let _ = std::fs::remove_file(&self.socket_path);
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
        let pipeline = make_pipeline(initial_session.pipewire_node_id, frame_store, on_frame);
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
    node_id: u32,
    frame_store: Arc<Mutex<Option<Frame>>>,
    on_frame: F,
) -> gst::Pipeline
where
    F: Fn(bool) + Send + Sync + 'static,
{
    let desc = format!(
        "pipewiresrc name=src path={node_id} do-timestamp=true \
         ! videoconvert \
         ! video/x-raw,format=BGRx \
         ! appsink name=sink sync=false"
    );
    let pipeline = gst::parse_launch(&desc)
        .expect("pipeline parse failed")
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
