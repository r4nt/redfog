//! Manual, live verification for `InputtinoGamepad` — not part of the
//! automated test suite (CI has no `/dev/uinput` access at all). Creates a
//! real session's worth of virtual gamepad device(s), prints their node
//! paths (one per line, prefixed `NODE `), then sends one clearly-
//! identifiable state update and exits. Paired with
//! `scripts/verify-inputtino-devices.py`, which reads the raw `input_event`
//! records straight off those nodes to confirm the values that actually
//! land in the kernel match what was requested — not just "didn't panic"
//! (what the gated unit test in `redfog-core::tests` already covers).
use redfog_core::InputtinoGamepad;

fn main() {
    let mut gamepad = InputtinoGamepad::new("verify").expect("create gamepad (need /dev/uinput access)");
    for node in gamepad.node_paths().expect("node paths") {
        println!("NODE {}", node.display());
    }
    println!("READY");

    // Give the reader script time to open every node before any event fires
    // — an event written before a reader opens its node is simply lost.
    std::thread::sleep(std::time::Duration::from_millis(500));

    // A: bit 0x1000 (INPUTTINO_JOYPAD_BTN::A, matches Moonlight's own
    // ControllerButtons::A bit exactly — see InputtinoGamepad::set_state's
    // doc comment). Distinctive, non-zero trigger/stick values too.
    gamepad.set_state(0x1000, 128, 200, (-32768, 32767), (10000, -10000));

    std::thread::sleep(std::time::Duration::from_millis(500));
}
