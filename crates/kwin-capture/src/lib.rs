use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO);
        let ms = now.as_millis() % 1000;
        let secs = now.as_secs();
        let h = (secs / 3600) % 24;
        let m = (secs / 60) % 60;
        let s = secs % 60;
        std::eprintln!("[{:02}:{:02}:{:02}.{:03}] {}", h, m, s, ms, format!($($arg)*));
    }};
}

use wayland_client::{
    backend::ObjectId,
    protocol::wl_registry,
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols_plasma::screencast::v1::client::{
    zkde_screencast_stream_unstable_v1::{self, ZkdeScreencastStreamUnstableV1},
    zkde_screencast_unstable_v1::{self, ZkdeScreencastUnstableV1},
};

// ── local protocol bindings from system v21 XMLs ─────────────────────────────
//
// Layout rationale:
//   - device and management XMLs each need their own __interfaces module to avoid
//     SyncWrapper/types_null conflicts when two generate_interfaces! calls share a module.
//   - management's generate_client_code! references `super::kde_output_device_v2`, so
//     the device client code must be generated one level above management's client code.
//   - Concretely: device client types live in `kde_protocols`, management types in
//     `kde_protocols::mgmt_client` where `super` == `kde_protocols` == device namespace.

// ── local protocol bindings from system v21 XMLs ─────────────────────────────
//
// Both generate_client_code! calls must be in the same module (kde_protocols).
// The management XML's generated code does `super::kde_output_device_v2::Foo`
// where super is the module containing the generated modules — i.e. kde_protocols.
// Device client code is generated there first so the reference resolves.
//
// generate_interfaces! conflicts (SyncWrapper, types_null) are avoided by putting
// each protocol's interfaces in a private sub-module that is then star-imported.

mod kde_protocols {
    #![allow(
        dead_code, non_camel_case_types, unused_unsafe, unused_variables,
        non_upper_case_globals, non_snake_case, unused_imports, missing_docs,
        clippy::all
    )]
    use wayland_client;
    use wayland_client::protocol::*;
    use wayland_backend;

    mod device_ifaces {
        use wayland_client::protocol::__interfaces::*;
        use wayland_backend;
        wayland_scanner::generate_interfaces!("protocols/kde-output-device-v2.xml");
    }
    mod management_ifaces {
        use wayland_client::protocol::__interfaces::*;
        use wayland_backend;
        pub use super::device_ifaces::*;
        wayland_scanner::generate_interfaces!("protocols/kde-output-management-v2.xml");
    }

    use self::device_ifaces::*;
    use self::management_ifaces::*;

    // Both generate_client_code! in the same module.
    // Device generates: pub mod kde_output_device_v2, pub mod kde_output_device_mode_v2
    // Management then finds them as super::kde_output_device_v2 from inside its sub-modules.
    wayland_scanner::generate_client_code!("protocols/kde-output-device-v2.xml");
    wayland_scanner::generate_client_code!("protocols/kde-output-management-v2.xml");
}

use kde_protocols::{
    kde_output_device_mode_v2::{self, KdeOutputDeviceModeV2},
    kde_output_device_v2::{self, KdeOutputDeviceV2},
    kde_output_device_registry_v2::{self, KdeOutputDeviceRegistryV2},
    kde_mode_list_v2::{self, KdeModeListV2},
    kde_output_configuration_v2::{self, KdeOutputConfigurationV2},
    kde_output_management_v2::{self, KdeOutputManagementV2},
};

// ── state ─────────────────────────────────────────────────────────────────────

struct State {
    // Screencast
    screencast:      Option<ZkdeScreencastUnstableV1>,
    _stream:         Option<ZkdeScreencastStreamUnstableV1>,
    node_id:         Option<u32>,
    stream_done:     bool,
    our_output_name: String,
    // Output management
    output_management:    Option<KdeOutputManagementV2>,
    _device_registry:     Option<KdeOutputDeviceRegistryV2>,
    _all_devices:         Vec<KdeOutputDeviceV2>, // keep-alive; devices come from device_registry
    our_device:           Option<KdeOutputDeviceV2>,
    /// (proxy, width, height) keyed by ObjectId; width/height populated on Size event
    modes:             HashMap<ObjectId, (KdeOutputDeviceModeV2, i32, i32)>,
    current_mode_id:   Option<ObjectId>,
    device_done:       bool,
    config_done:       bool,
}

impl State {
    fn new(name: &str) -> Self {
        Self {
            screencast: None,
            _stream: None,
            node_id: None,
            stream_done: false,
            our_output_name: name.into(),
            output_management: None,
            _device_registry: None,
            _all_devices: Vec::new(),
            our_device: None,
            modes: HashMap::new(),
            current_mode_id: None,
            device_done: false,
            config_done: false,
        }
    }
}

// ── Dispatch impls ────────────────────────────────────────────────────────────

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(state: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event,
             _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        let wl_registry::Event::Global { name, interface, version } = event else { return };
        eprintln!("capture: registry global: {interface} v{version}");
        match interface.as_str() {
            "zkde_screencast_unstable_v1" => {
                state.screencast = Some(registry.bind(name, version.min(4), qh, ()));
            }
            "kde_output_management_v2" => {
                eprintln!("capture: binding kde_output_management_v2 v{}", version.min(18));
                state.output_management = Some(registry.bind(name, version.min(18), qh, ()));
            }
            // In KWin v21+, devices come via kde_output_device_registry_v2.output() events,
            // not as individual Wayland globals. Bind the registry to receive them.
            "kde_output_device_registry_v2" => {
                eprintln!("capture: binding kde_output_device_registry_v2 v{}", version.min(21));
                state._device_registry = Some(registry.bind(name, version.min(21), qh, ()));
            }
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
                eprintln!("capture: stream created, node={node}");
                state.node_id = Some(node);
                state.stream_done = true;
            }
            zkde_screencast_stream_unstable_v1::Event::Failed { error } => {
                eprintln!("capture: stream failed: {error}");
                state.stream_done = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<KdeOutputManagementV2, ()> for State {
    fn event(_: &mut Self, _: &KdeOutputManagementV2,
             _: kde_output_management_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<KdeOutputDeviceRegistryV2, ()> for State {
    fn event(state: &mut Self, _: &KdeOutputDeviceRegistryV2,
             event: kde_output_device_registry_v2::Event,
             _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            kde_output_device_registry_v2::Event::Output { output } => {
                eprintln!("capture: device_registry output event → new device");
                state._all_devices.push(output);
            }
            _ => {}
        }
    }

    // output event (opcode 1) creates a kde_output_device_v2 child object.
    wayland_client::event_created_child!(State, KdeOutputDeviceRegistryV2, [
        1 => (KdeOutputDeviceV2, ()),
    ]);
}

impl Dispatch<KdeOutputDeviceV2, ()> for State {
    fn event(state: &mut Self, proxy: &KdeOutputDeviceV2, event: kde_output_device_v2::Event,
             _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            kde_output_device_v2::Event::Name { name } => {
                eprintln!("capture: device Name={name}");
                // KWin prefixes "Virtual-" to stream_virtual_output names.
                if name == state.our_output_name || name.contains(&*state.our_output_name) {
                    eprintln!("capture: matched our device");
                    state.our_device = Some(proxy.clone());
                }
            }
            kde_output_device_v2::Event::Mode { mode } => {
                let id = mode.id();
                eprintln!("capture: device Mode id={id:?}");
                state.modes.insert(id, (mode, 0, 0));
            }
            kde_output_device_v2::Event::CurrentMode { mode } => {
                eprintln!("capture: device CurrentMode id={:?}", mode.id());
                state.current_mode_id = Some(mode.id());
            }
            kde_output_device_v2::Event::Done => {
                eprintln!("capture: device Done (is_ours={})",
                    state.our_device.as_ref().map(|d| d.id() == proxy.id()).unwrap_or(false));
                if state.our_device.as_ref().map(|d| d.id() == proxy.id()).unwrap_or(false) {
                    state.device_done = true;
                }
            }
            _ => {}
        }
    }

    // mode event (opcode 2) creates a kde_output_device_mode_v2 child object.
    wayland_client::event_created_child!(State, KdeOutputDeviceV2, [
        2 => (KdeOutputDeviceModeV2, ()),
    ]);
}

impl Dispatch<KdeOutputDeviceModeV2, ()> for State {
    fn event(state: &mut Self, proxy: &KdeOutputDeviceModeV2,
             event: kde_output_device_mode_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            kde_output_device_mode_v2::Event::Size { width, height } => {
                if let Some(entry) = state.modes.get_mut(&proxy.id()) {
                    entry.1 = width;
                    entry.2 = height;
                }
            }
            kde_output_device_mode_v2::Event::Removed => {
                eprintln!("capture: mode Removed {:?} (size was {:?})",
                    proxy.id(),
                    state.modes.get(&proxy.id()).map(|(_, w, h)| (*w, *h)));
                state.modes.remove(&proxy.id());
                if state.current_mode_id == Some(proxy.id()) {
                    state.current_mode_id = None;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<KdeOutputConfigurationV2, ()> for State {
    fn event(state: &mut Self, _: &KdeOutputConfigurationV2,
             event: kde_output_configuration_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            kde_output_configuration_v2::Event::Applied => { state.config_done = true; }
            kde_output_configuration_v2::Event::Failed => {
                eprintln!("capture: output configuration failed");
                state.config_done = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<KdeModeListV2, ()> for State {
    fn event(_: &mut Self, _: &KdeModeListV2,
             _: kde_mode_list_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn pump(state: &mut State, queue: &mut wayland_client::EventQueue<State>, conn: &Connection) {
    conn.flush().ok();
    queue.blocking_dispatch(state).ok();
}

/// Find a mode whose dimensions are within 8px of (w, h). KDE may snap widths.
fn find_mode(modes: &HashMap<ObjectId, (KdeOutputDeviceModeV2, i32, i32)>, w: i32, h: i32)
    -> Option<KdeOutputDeviceModeV2>
{
    modes.values()
        .filter(|(_, mw, mh)| *mw > 0 && *mh > 0)
        .find(|(_, mw, mh)| (*mw - w).abs() <= 8 && (*mh - h).abs() <= 8)
        .map(|(proxy, _, _)| proxy.clone())
}

/// Apply a mode change on the virtual output. Two-step when the mode doesn't exist yet:
/// set_custom_modes (replaces any prior custom mode) then mode() (activates it).
fn set_output_mode(
    state: &mut State,
    queue: &mut wayland_client::EventQueue<State>,
    qh: &QueueHandle<State>,
    conn: &Connection,
    w: i32,
    h: i32,
) {
    let (Some(device), Some(mgmt)) = (state.our_device.clone(), state.output_management.clone())
    else {
        eprintln!("capture: set_output_mode: no device or management object");
        return;
    };

    // Skip if current mode already matches.
    let already_correct = state.current_mode_id
        .as_ref()
        .and_then(|id| state.modes.get(id))
        .map(|(_, mw, mh)| (*mw - w).abs() <= 8 && (*mh - h).abs() <= 8)
        .unwrap_or(false);
    if already_correct {
        return;
    }

    // If no existing mode matches, add a custom one (set_custom_modes replaces any prior).
    let target = if let Some(m) = find_mode(&state.modes, w, h) {
        m
    } else {
        let mode_list = mgmt.create_mode_list(qh, ());
        mode_list.set_resolution(w as u32, h as u32);
        mode_list.set_refresh_rate(60_000);
        mode_list.add_mode();

        let config = mgmt.create_configuration(qh, ());
        config.set_custom_modes(&device, &mode_list);
        config.apply();
        conn.flush().ok();

        // Wait for applied; device updates (Removed + new Mode + Size events) arrive first.
        state.config_done = false;
        while !state.config_done { pump(state, queue, conn); }

        // Extra roundtrip to ensure all Size events are fully processed.
        queue.roundtrip(state).ok();

        match find_mode(&state.modes, w, h) {
            Some(m) => m,
            None => {
                eprintln!("capture: custom mode {w}x{h} not found after set_custom_modes. Available modes:");
                for (id, (_, mw, mh)) in &state.modes {
                    eprintln!("  - {id:?}: {mw}x{mh}");
                }
                // Try finding the closest mode as a fallback
                if let Some((proxy, mw, mh)) = state.modes.values()
                    .filter(|(_, mw, mh)| *mw > 0 && *mh > 0)
                    .min_by_key(|(_, mw, mh)| (*mw - w).abs() + (*mh - h).abs())
                {
                    eprintln!("capture: falling back to closest mode {mw}x{mh}");
                    proxy.clone()
                } else {
                    return;
                }
            }
        }
    };

    // Activate the target mode.
    let config = mgmt.create_configuration(qh, ());
    config.mode(&device, &target);
    config.apply();
    conn.flush().ok();

    state.config_done = false;
    while !state.config_done { pump(state, queue, conn); }
}

fn create_stream(
    state: &mut State,
    queue: &mut wayland_client::EventQueue<State>,
    qh: &QueueHandle<State>,
    conn: &Connection,
    w: i32,
    h: i32,
    scale: f64,
) -> Option<u32> {
    state._stream = None;
    conn.flush().ok();
    queue.roundtrip(state).ok()?;

    eprintln!("capture: calling stream_virtual_output({}, {w}x{h})", state.our_output_name);
    let stream = {
        let screencast = state.screencast.as_ref()?;
        screencast.stream_virtual_output(
            state.our_output_name.clone(),
            w, h, scale,
            zkde_screencast_unstable_v1::Pointer::Hidden as u32,
            qh, (),
        )
    };
    state._stream = Some(stream);
    state.node_id = None;
    state.stream_done = false;
    state.our_device = None;
    state.device_done = false;
    state.current_mode_id = None;
    conn.flush().ok();

    eprintln!("capture: waiting for stream+device (stream_done={} device={} device_done={})",
        state.stream_done, state.our_device.is_some(), state.device_done);
    // Wait for PipeWire node AND output device to be fully described.
    while !state.stream_done || state.our_device.is_none() || !state.device_done {
        pump(state, queue, conn);
    }
    // Size events arrive after Mode events in a subsequent roundtrip; flush them now
    // so find_mode sees populated sizes and doesn't needlessly call set_custom_modes.
    queue.roundtrip(state).ok();
    eprintln!("capture: stream+device ready, node={:?}", state.node_id);

    set_output_mode(state, queue, qh, conn, w, h);

    state.node_id
}

// ── public API ────────────────────────────────────────────────────────────────

pub struct CaptureSession {
    node_id:   u32,
    resize_tx: mpsc::Sender<(i32, i32, mpsc::Sender<()>)>,
}

impl CaptureSession {
    pub fn connect(
        socket_path: &Path,
        output_name: &str,
        width: i32,
        height: i32,
        scale: f64,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        use std::os::unix::net::UnixStream;

        let stream = UnixStream::connect(socket_path)?;
        let conn = Connection::from_socket(stream)?;
        let mut queue = conn.new_event_queue::<State>();
        let qh = queue.handle();

        conn.display().get_registry(&qh, ());
        let mut state = State::new(output_name);
        queue.roundtrip(&mut state)?;

        let node_id = create_stream(&mut state, &mut queue, &qh, &conn, width, height, scale)
            .ok_or("virtual output stream failed")?;

        let (resize_tx, resize_rx) = mpsc::channel::<(i32, i32, mpsc::Sender<()>)>();

        std::thread::spawn(move || {
            use std::os::fd::{AsFd, AsRawFd};
            let fd = conn.as_fd().as_raw_fd();
            loop {
                if let Some(guard) = conn.prepare_read() {
                    let mut fds = [libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    }];
                    let ret = unsafe { libc::poll(fds.as_mut_ptr(), 1, 8) };
                    if ret > 0 && (fds[0].revents & libc::POLLIN) != 0 {
                        if let Err(e) = guard.read() {
                            eprintln!("capture: error reading from Wayland socket: {e}");
                            break;
                        }
                    }
                }

                conn.flush().ok();
                queue.dispatch_pending(&mut state).ok();

                while let Ok((w, h, reply_tx)) = resize_rx.try_recv() {
                    eprintln!("capture: resizing to {w}x{h}");
                    set_output_mode(&mut state, &mut queue, &qh, &conn, w, h);
                    let _ = reply_tx.send(());
                }
            }
        });

        Ok(CaptureSession { node_id, resize_tx })
    }

    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    /// Request the virtual output to change resolution.
    /// This blocks until the compositor has successfully applied the mode change.
    pub fn resize(&self, w: i32, h: i32) {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self.resize_tx.send((w, h, reply_tx)).is_ok() {
            let _ = reply_rx.recv();
        }
    }
}
