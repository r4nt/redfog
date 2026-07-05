use std::io::{self, BufRead};

use wayland_client::{
    delegate_noop,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_registry, wl_seat},
    Connection, Dispatch, QueueHandle,
};

mod fake_input {
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

use fake_input::client::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore OrgKdeKwinFakeInput);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();

    let fake_input: OrgKdeKwinFakeInput = globals
        .bind(&qh, 4..=6, ())
        .map_err(|e| format!("org_kde_kwin_fake_input not available: {e}"))?;

    let mut state = State;

    fake_input.authenticate(
        "redfog".to_string(),
        "input forwarding for game streaming".to_string(),
    );
    conn.flush()?;
    queue.roundtrip(&mut state)?;

    eprintln!("kwin-input: connected, fake_input authenticated");
    eprintln!("kwin-input: commands: key <evdev> <1|0>, rel <dx> <dy>, button <btn> <1|0>");

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.as_slice() {
            // key <evdev_keycode> <1|0>  — linux evdev keycode
            ["key", code, pressed] => {
                if let Ok(k) = code.parse::<u32>() {
                    fake_input.keyboard_key(k, if *pressed == "1" { 1 } else { 0 });
                    conn.flush()?;
                }
            }
            // rel <dx> <dy>  — relative pointer motion (pixels as f64 fixed-point)
            ["rel", dx, dy] => {
                if let (Ok(x), Ok(y)) = (dx.parse::<f64>(), dy.parse::<f64>()) {
                    fake_input.pointer_motion(x, y);
                    conn.flush()?;
                }
            }
            // button <evdev_button> <1|0>  — BTN_LEFT=272 BTN_RIGHT=273 BTN_MIDDLE=274
            ["button", code, pressed] => {
                if let Ok(b) = code.parse::<u32>() {
                    fake_input.button(b, if *pressed == "1" { 1 } else { 0 });
                    conn.flush()?;
                }
            }
            _ => {
                eprintln!("kwin-input: unknown: {line}");
            }
        }
    }

    Ok(())
}
