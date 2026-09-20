#!/usr/bin/env python3
"""Live, manual verification that InputtinoGamepad's virtual device actually
delivers the kernel events it claims to — not just "didn't panic" (the gated
unit test in redfog-core::tests already covers that). Reads raw struct
input_event records straight off the created /dev/input/eventN node(s) while
crates/redfog-core/examples/inputtino_verify.rs sends one known state
update, and checks the values that arrive match what was requested.

Requires real /dev/uinput access (same REDFOG_INPUT_BACKEND=inputtino
requirement as production) -- not run in CI. Build first:
  cargo build -p redfog-core --example inputtino_verify
"""
import struct
import subprocess
import sys
import time
from pathlib import Path

EVENT_FORMAT = "llHHi"  # struct input_event on 64-bit Linux: timeval{long,long}, u16 type, u16 code, i32 value
EVENT_SIZE = struct.calcsize(EVENT_FORMAT)
EV_KEY, EV_ABS = 0x01, 0x03
# evdev codes -- see /usr/include/linux/input-event-codes.h. BTN_SOUTH is
# what inputtino's XboxOneJoypad actually writes for JoypadButton::A (see
# src/uinput/joypad_xbox.cpp) -- BTN_A is just an alias for the same value.
BTN_SOUTH = 0x130
ABS_X, ABS_Y, ABS_Z, ABS_RX, ABS_RY, ABS_RZ = 0x00, 0x01, 0x02, 0x03, 0x04, 0x05


def read_events(path, duration_s):
    events = []
    deadline = time.time() + duration_s
    with open(path, "rb") as f:
        import os
        import fcntl
        flags = fcntl.fcntl(f.fileno(), fcntl.F_GETFL)
        fcntl.fcntl(f.fileno(), fcntl.F_SETFL, flags | os.O_NONBLOCK)
        while time.time() < deadline:
            try:
                chunk = f.read(EVENT_SIZE)
            except BlockingIOError:
                chunk = None
            if chunk and len(chunk) == EVENT_SIZE:
                _, _, ev_type, ev_code, ev_value = struct.unpack(EVENT_FORMAT, chunk)
                events.append((ev_type, ev_code, ev_value))
            else:
                time.sleep(0.02)
    return events


def main():
    repo_root = Path(__file__).resolve().parent.parent
    binary = repo_root / "target" / "debug" / "examples" / "inputtino_verify"
    if not binary.exists():
        print(f"build first: cargo build -p redfog-core --example inputtino_verify (looked for {binary})", file=sys.stderr)
        return 1

    proc = subprocess.Popen([str(binary)], stdout=subprocess.PIPE, text=True)
    nodes = []
    for line in proc.stdout:
        line = line.strip()
        if line == "READY":
            break
        if line.startswith("NODE "):
            nodes.append(line[len("NODE "):])
    if not nodes:
        print("expected at least one device node, got none", file=sys.stderr)
        return 1

    print(f"nodes: {nodes}")

    import concurrent.futures
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(nodes)) as pool:
        futures = [pool.submit(read_events, node, 2.0) for node in nodes]
        proc.wait(timeout=5)
        events = [f.result() for f in futures]
    all_events = [e for node_events in events for e in node_events]

    ok = True

    # set_state(0x1000 /* A */, 128, 200, (-32768, 32767), (10000, -10000))
    if (EV_KEY, BTN_SOUTH, 1) not in all_events:
        print(f"FAIL: gamepad never saw BTN_SOUTH (A) press. Got: {all_events}", file=sys.stderr)
        ok = False
    else:
        print("OK: gamepad A-button press observed")

    if (EV_ABS, ABS_Z, 128) not in all_events or (EV_ABS, ABS_RZ, 200) not in all_events:
        print(f"FAIL: gamepad didn't see the expected trigger values (left=128, right=200). Got: {all_events}", file=sys.stderr)
        ok = False
    else:
        print("OK: gamepad trigger values (left=128, right=200) observed")

    # inputtino inverts the Y axis internally (src/uinput/joypad_xbox.cpp's
    # set_stick: `libevdev_uinput_write_event(controller, EV_ABS, ABS_Y, -y)`)
    # -- left_stick=(-32768, 32767) on the wire becomes ABS_X=-32768, ABS_Y=-32767.
    if (EV_ABS, ABS_X, -32768) not in all_events or (EV_ABS, ABS_Y, -32767) not in all_events:
        print(f"FAIL: gamepad didn't see the expected left stick values (x=-32768, y=-32767 after inversion). Got: {all_events}", file=sys.stderr)
        ok = False
    else:
        print("OK: gamepad left stick values observed")

    # right_stick=(10000, -10000) -> ABS_RX=10000, ABS_RY=10000 (inverted).
    if (EV_ABS, ABS_RX, 10000) not in all_events or (EV_ABS, ABS_RY, 10000) not in all_events:
        print(f"FAIL: gamepad didn't see the expected right stick values (x=10000, y=10000 after inversion). Got: {all_events}", file=sys.stderr)
        ok = False
    else:
        print("OK: gamepad right stick values observed")

    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
