use std::collections::HashMap;
use std::sync::mpsc;

use wayland_client::{
    protocol::wl_registry,
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols_plasma::screencast::v1::client::{
    zkde_screencast_stream_unstable_v1::{self, ZkdeScreencastStreamUnstableV1},
    zkde_screencast_unstable_v1::{self, ZkdeScreencastUnstableV1},
};

// Generated client bindings for kde-output-device-v2.
mod kde_output_device_v2 {
    #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables,
             non_upper_case_globals, non_snake_case, unused_imports, missing_docs,
             clippy::all)]
    pub mod client {
        use wayland_client;
        use wayland_client::protocol::*;
        use wayland_backend;
        use bitflags;
        pub mod __interfaces {
            use wayland_client::protocol::__interfaces::*;
            use wayland_backend;
            wayland_scanner::generate_interfaces!("protocols/kde-output-device-v2.xml");
        }
        use self::__interfaces::*;
        wayland_scanner::generate_client_code!("protocols/kde-output-device-v2.xml");
    }
}

// Generated client bindings for kde-output-management-v2 (depends on device types).
mod kde_output_management_v2 {
    #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables,
             non_upper_case_globals, non_snake_case, unused_imports, missing_docs,
             clippy::all)]
    pub mod client {
        use wayland_client;
        use wayland_client::protocol::*;
        use wayland_backend;
        use bitflags;
        use super::super::kde_output_device_v2::client::*;
        pub mod __interfaces {
            use wayland_client::protocol::__interfaces::*;
            use wayland_backend;
            use super::super::super::kde_output_device_v2::client::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocols/kde-output-management-v2.xml");
        }
        use self::__interfaces::*;
        wayland_scanner::generate_client_code!("protocols/kde-output-management-v2.xml");
    }
}

use kde_output_device_v2::client::{
    kde_output_device_registry_v2::KdeOutputDeviceRegistryV2,
    kde_output_device_v2::{self as dev_ev, KdeOutputDeviceV2},
    kde_output_device_mode_v2::{self as mode_ev, KdeOutputDeviceModeV2},
};
use kde_output_management_v2::client::{
    kde_output_configuration_v2::{self as cfg_ev, KdeOutputConfigurationV2},
    kde_output_management_v2::KdeOutputManagementV2,
    kde_mode_list_v2::KdeModeListV2,
};

// ── output info ───────────────────────────────────────────────────────────────

struct ModeInfo {
    proxy: KdeOutputDeviceModeV2,
    w: i32,
    h: i32,
}

struct OutputInfo {
    proxy:  KdeOutputDeviceV2,
    name:   Option<String>,
    modes:  Vec<ModeInfo>,
}

// ── resize state machine ──────────────────────────────────────────────────────
//
// Phase 1 – AddingMode:
//   Create a mode list, call set_custom_modes + apply on config1.
//   Wait for config1.applied AND for the new mode to appear with the right size.
//
// Phase 2 – SelectingMode:
//   Create config2, call mode() + apply.
//   Wait for config2.applied → done.

#[derive(Default)]
enum ResizePhase {
    #[default]
    Idle,
    AddingMode { target_w: i32, target_h: i32, waiting_for_mode: bool },
    SelectingMode,
}

// ── state ─────────────────────────────────────────────────────────────────────

struct State {
    screencast:         Option<ZkdeScreencastUnstableV1>,
    node_id:            Option<u32>,
    stream_done:        bool,

    output_registry:    Option<KdeOutputDeviceRegistryV2>,
    output_management:  Option<KdeOutputManagementV2>,
    outputs:            HashMap<wayland_client::backend::ObjectId, OutputInfo>,

    our_output_name:    String,  // name passed to stream_virtual_output

    resize_phase:       ResizePhase,
    pending_config:     Option<KdeOutputConfigurationV2>,
}

impl State {
    fn new(name: &str) -> Self {
        Self {
            screencast: None,
            node_id: None,
            stream_done: false,
            output_registry: None,
            output_management: None,
            outputs: HashMap::new(),
            our_output_name: name.into(),
            resize_phase: ResizePhase::Idle,
            pending_config: None,
        }
    }

    fn our_output(&self) -> Option<&OutputInfo> {
        let target = format!("Virtual-{}", self.our_output_name);
        self.outputs.values().find(|o| o.name.as_deref() == Some(target.as_str()))
    }

    // ── Phase 1: add a custom mode ────────────────────────────────────────────

    fn start_resize(&mut self, qh: &QueueHandle<Self>, w: i32, h: i32) {
        let Some(mgmt) = &self.output_management else {
            eprintln!("resize: kde_output_management_v2 not available");
            return;
        };
        let Some(out) = self.our_output() else {
            eprintln!("resize: virtual output not found yet");
            return;
        };

        eprintln!("resize: output found: {:?}", out.name);
        eprintln!("resize: sending set_custom_modes + apply for {w}x{h}");

        let mode_list = mgmt.create_mode_list(qh, ());
        mode_list.set_resolution(w as u32, h as u32);
        mode_list.set_refresh_rate(60_000);
        mode_list.add_mode();

        let config = mgmt.create_configuration(qh, ());
        config.set_custom_modes(&out.proxy, &mode_list);
        config.apply();

        self.pending_config = Some(config);
        self.resize_phase = ResizePhase::AddingMode { target_w: w, target_h: h, waiting_for_mode: false };
    }

    // ── Phase 1 → Phase 2 transition ─────────────────────────────────────────
    //
    // Called either when config1.applied fires (and the mode may already be there)
    // or when a new mode with the right size appears (and applied may have already fired).

    fn try_advance_to_phase2(&mut self, qh: &QueueHandle<Self>) {
        let ResizePhase::AddingMode { target_w, target_h, .. } = self.resize_phase else { return };

        let Some(out) = self.our_output() else { return };
        let available: Vec<_> = out.modes.iter().map(|m| (m.w, m.h)).collect();
        eprintln!("resize: phase1 applied; available modes: {available:?}");
        let Some(mode) = out.modes.iter().find(|m| m.w == target_w && m.h == target_h) else {
            eprintln!("resize: {target_w}x{target_h} not yet visible, waiting for mode event");
            // Mode not yet visible; mark that we're waiting for it.
            if let ResizePhase::AddingMode { ref mut waiting_for_mode, .. } = self.resize_phase {
                *waiting_for_mode = true;
            }
            return;
        };

        let Some(mgmt) = &self.output_management else { return };
        let config = mgmt.create_configuration(qh, ());
        config.mode(&out.proxy, &mode.proxy);
        config.apply();

        self.pending_config = Some(config);
        self.resize_phase = ResizePhase::SelectingMode;
    }

    // ── Phase 2: configuration applied or failed ──────────────────────────────

    fn on_config_event(&mut self, ok: bool) {
        eprintln!("resize: config event ok={ok} phase={}", match self.resize_phase {
            ResizePhase::Idle => "Idle",
            ResizePhase::AddingMode { .. } => "AddingMode",
            ResizePhase::SelectingMode => "SelectingMode",
        });
        match self.resize_phase {
            ResizePhase::AddingMode { .. } => {
                self.pending_config = None;
                if !ok {
                    eprintln!("resize: set_custom_modes failed");
                    self.resize_phase = ResizePhase::Idle;
                }
            }
            ResizePhase::SelectingMode => {
                self.pending_config = None;
                self.resize_phase = ResizePhase::Idle;
                if ok { eprintln!("resize: done"); } else { eprintln!("resize: select failed"); }
            }
            ResizePhase::Idle => {}
        }
    }
}

// ── Dispatch impls ────────────────────────────────────────────────────────────

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(state: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event,
             _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        let wl_registry::Event::Global { name, interface, version } = event else { return };
        match interface.as_str() {
            "zkde_screencast_unstable_v1" =>
                state.screencast = Some(registry.bind(name, version.min(4), qh, ())),
            "kde_output_device_registry_v2" if version >= 21 =>
                state.output_registry = Some(registry.bind(name, version.min(23), qh, ())),
            "kde_output_management_v2" if version >= 18 =>
                state.output_management = Some(registry.bind(name, version.min(21), qh, ())),
            _ => {}
        }
    }
}

impl Dispatch<ZkdeScreencastUnstableV1, ()> for State {
    fn event(_: &mut Self, _: &ZkdeScreencastUnstableV1,
             _: zkde_screencast_unstable_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(state: &mut Self, _: &ZkdeScreencastStreamUnstableV1,
             event: zkde_screencast_stream_unstable_v1::Event,
             _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zkde_screencast_stream_unstable_v1::Event::Created { node } => {
                state.node_id = Some(node); state.stream_done = true;
            }
            zkde_screencast_stream_unstable_v1::Event::Failed { error } => {
                eprintln!("stream failed: {error}"); state.stream_done = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<KdeOutputDeviceRegistryV2, ()> for State {
    fn event(state: &mut Self, _: &KdeOutputDeviceRegistryV2,
             event: kde_output_device_v2::client::kde_output_device_registry_v2::Event,
             _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use kde_output_device_v2::client::kde_output_device_registry_v2::Event;
        if let Event::Output { output } = event {
            state.outputs.insert(output.id(), OutputInfo { proxy: output, name: None, modes: vec![] });
        }
    }

    wayland_client::event_created_child!(State, KdeOutputDeviceRegistryV2, [
        1 => (KdeOutputDeviceV2, ())
    ]);
}

impl Dispatch<KdeOutputDeviceV2, ()> for State {
    fn event(state: &mut Self, proxy: &KdeOutputDeviceV2, event: dev_ev::Event,
             _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let id = proxy.id();
        match event {
            dev_ev::Event::Name { name } => {
                if let Some(info) = state.outputs.get_mut(&id) { info.name = Some(name); }
            }
            dev_ev::Event::Mode { mode } => {
                if let Some(info) = state.outputs.get_mut(&id) {
                    info.modes.push(ModeInfo { proxy: mode, w: 0, h: 0 });
                }
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, KdeOutputDeviceV2, [
        2 => (KdeOutputDeviceModeV2, ())
    ]);
}

impl Dispatch<KdeOutputDeviceModeV2, ()> for State {
    fn event(state: &mut Self, proxy: &KdeOutputDeviceModeV2, event: mode_ev::Event,
             _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let mode_ev::Event::Size { width, height } = event {
            let (w, h) = (width as i32, height as i32);
            // Update stored dimensions for this mode in whichever output owns it.
            for info in state.outputs.values_mut() {
                for m in &mut info.modes {
                    if m.proxy.id() == proxy.id() { m.w = w; m.h = h; }
                }
            }
            // If we're waiting for this specific resolution to appear, advance.
            if let ResizePhase::AddingMode { target_w, target_h, waiting_for_mode: true } = state.resize_phase {
                if w == target_w && h == target_h {
                    state.try_advance_to_phase2(qh);
                }
            }
        }
    }
}

impl Dispatch<KdeOutputManagementV2, ()> for State {
    fn event(_: &mut Self, _: &KdeOutputManagementV2,
             _: kde_output_management_v2::client::kde_output_management_v2::Event,
             _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<KdeOutputConfigurationV2, ()> for State {
    fn event(state: &mut Self, _: &KdeOutputConfigurationV2, event: cfg_ev::Event,
             _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match event {
            cfg_ev::Event::Applied => {
                let was_phase1 = matches!(state.resize_phase, ResizePhase::AddingMode { .. });
                state.on_config_event(true);
                if was_phase1 { state.try_advance_to_phase2(qh); }
            }
            cfg_ev::Event::Failed => state.on_config_event(false),
            _ => {}
        }
    }
}

impl Dispatch<KdeModeListV2, ()> for State {
    fn event(_: &mut Self, _: &KdeModeListV2,
             _: kde_output_management_v2::client::kde_mode_list_v2::Event,
             _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

// ── main ──────────────────────────────────────────────────────────────────────

fn env_i32(key: &str, default: i32) -> i32 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn main() {
    let width  = env_i32("REDFOG_WIDTH",  1920);
    let height = env_i32("REDFOG_HEIGHT", 1080);
    let scale  = env_f64("REDFOG_SCALE",  1.0);

    let conn = Connection::connect_to_env().expect("failed to connect to Wayland display");
    let mut queue: EventQueue<State> = conn.new_event_queue();
    let qh = queue.handle();

    let _registry = conn.display().get_registry(&qh, ());
    let mut state = State::new("redfog-output");
    queue.roundtrip(&mut state).expect("initial roundtrip failed");

    let screencast = state.screencast.as_ref()
        .expect("zkde_screencast_unstable_v1 not available — is this KWin?");

    let _stream = screencast.stream_virtual_output(
        "redfog-output".to_string(),
        width, height, scale,
        zkde_screencast_unstable_v1::Pointer::Hidden as u32,
        &qh, (),
    );
    conn.flush().unwrap();

    while !state.stream_done {
        queue.blocking_dispatch(&mut state).expect("dispatch error");
    }
    let Some(node_id) = state.node_id else { std::process::exit(1) };

    // Announce node ID then keep the Wayland connection alive.
    println!("{node_id}");

    // Spawn stdin thread: reads "resize WxH" lines.
    let (tx, rx) = mpsc::channel::<(i32, i32)>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(std::io::stdin()).lines().flatten() {
            if let Some(dims) = line.trim().strip_prefix("resize ") {
                if let Some((ws, hs)) = dims.split_once('x') {
                    if let (Ok(w), Ok(h)) = (ws.trim().parse(), hs.trim().parse()) {
                        let _ = tx.send((w, h));
                    }
                }
            }
        }
    });

    loop {
        // Read any new events from the Wayland socket (non-blocking).
        // prepare_read() returns None if there are already events pending dispatch,
        // in which case we skip straight to dispatch_pending.
        if let Some(guard) = queue.prepare_read() {
            if let Err(e) = guard.read() {
                use wayland_client::backend::WaylandError;
                if !matches!(&e, WaylandError::Io(io_err) if io_err.kind() == std::io::ErrorKind::WouldBlock) {
                    panic!("Wayland read error: {e}");
                }
            }
        }
        queue.dispatch_pending(&mut state).expect("dispatch error");
        conn.flush().unwrap();

        while let Ok((w, h)) = rx.try_recv() {
            eprintln!("resize: requested {w}x{h}");
            state.start_resize(&qh, w, h);
            conn.flush().unwrap();
        }

        std::thread::sleep(std::time::Duration::from_millis(8));
    }
}
