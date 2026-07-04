use wayland_client::{
    protocol::wl_registry,
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols_plasma::screencast::v1::client::{
    zkde_screencast_stream_unstable_v1::{self, ZkdeScreencastStreamUnstableV1},
    zkde_screencast_unstable_v1::{self, ZkdeScreencastUnstableV1},
};

const WIDTH: i32 = 1920;
const HEIGHT: i32 = 1080;
const SCALE: f64 = 1.0;

#[derive(Default)]
struct State {
    screencast: Option<ZkdeScreencastUnstableV1>,
    node_id: Option<u32>,
    stream_done: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global { name, interface, version } = event else {
            return;
        };
        if interface == "zkde_screencast_unstable_v1" {
            // Bind at v4: the wayland-protocols-plasma crate bundles the v4 XML.
            // KWin 6.7+ uses v6, which adds a `serial` event our generated code
            // doesn't know about. Capping at 4 avoids the unknown event.
            // TODO: generate bindings from the system XML at build time.
            state.screencast = Some(registry.bind(name, version.min(4), qh, ()));
        }
    }
}

impl Dispatch<ZkdeScreencastUnstableV1, ()> for State {
    fn event(
        _: &mut Self, _: &ZkdeScreencastUnstableV1,
        _: zkde_screencast_unstable_v1::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        event: zkde_screencast_stream_unstable_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zkde_screencast_stream_unstable_v1::Event::Created { node } => {
                state.node_id = Some(node);
                state.stream_done = true;
            }
            zkde_screencast_stream_unstable_v1::Event::Failed { error } => {
                eprintln!("stream failed: {error}");
                state.stream_done = true;
            }
            _ => {}
        }
    }
}

fn main() {
    let conn = Connection::connect_to_env().expect("failed to connect to Wayland display");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();

    let _registry = conn.display().get_registry(&qh, ());

    let mut state = State::default();
    queue.roundtrip(&mut state).expect("initial roundtrip failed");

    let screencast = state
        .screencast
        .as_ref()
        .expect("zkde_screencast_unstable_v1 not available — is this KWin?");

    // stream_virtual_output creates a headless virtual output sized to our
    // spec and immediately starts a PipeWire stream for it.  No pre-existing
    // wl_output needed — ideal for a headless server session.
    let _stream = screencast.stream_virtual_output(
        "redfog-output".to_string(),
        WIDTH,
        HEIGHT,
        SCALE,
        zkde_screencast_unstable_v1::Pointer::Hidden as u32,
        &qh,
        (),
    );
    conn.flush().unwrap();

    while !state.stream_done {
        queue.blocking_dispatch(&mut state).expect("dispatch error");
    }

    match state.node_id {
        Some(id) => {
            // Print the node ID for the caller, then keep the Wayland
            // connection alive — dropping it would destroy the stream.
            println!("{id}");
            loop {
                queue.blocking_dispatch(&mut state).expect("dispatch error");
            }
        }
        None => std::process::exit(1),
    }
}
