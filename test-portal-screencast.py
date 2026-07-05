#!/usr/bin/env python3
"""
Try the XDG ScreenCast portal against the running proto.sh session.
Reads DBUS_SESSION_BUS_ADDRESS from /tmp/redfog-proto/dbus-session-address.
"""
import os, sys, time, random, string
import dbus
import dbus.mainloop.glib
from gi.repository import GLib

BUS_ADDR_FILE = "/tmp/redfog-proto/dbus-session-address"

def random_token(n=8):
    return ''.join(random.choices(string.ascii_lowercase, k=n))

def main():
    if not os.path.exists(BUS_ADDR_FILE):
        sys.exit(f"ERROR: {BUS_ADDR_FILE} not found — is proto.sh running?")

    bus_addr = open(BUS_ADDR_FILE).read().strip()
    if not bus_addr:
        sys.exit("ERROR: bus address file is empty")
    print(f"Using session bus: {bus_addr}")

    dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
    bus = dbus.bus.BusConnection(bus_addr)

    portal = bus.get_object(
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
    )
    sc = dbus.Interface(portal, "org.freedesktop.portal.ScreenCast")

    sender = bus.get_unique_name().lstrip(":").replace(".", "_")
    session_token = random_token()
    request_token = random_token()

    loop = GLib.MainLoop()
    session_handle = None
    node_id = None

    def on_response(response, results, **kw):
        nonlocal session_handle, node_id
        if kw.get("path", "").endswith(session_token) or "session_handle" in results:
            if response != 0:
                print(f"CreateSession failed: response={response}")
                loop.quit()
                return
            session_handle = str(results["session_handle"])
            print(f"Session created: {session_handle}")
            # SelectSources
            sel_token = random_token()
            sel_path = f"/org/freedesktop/portal/desktop/request/{sender}/{sel_token}"
            bus.add_signal_receiver(on_response, "Response",
                "org.freedesktop.portal.Request", path=sel_path)
            sc.SelectSources(
                session_handle,
                {
                    "handle_token": dbus.String(sel_token, variant_level=1),
                    "types": dbus.UInt32(4, variant_level=1),   # Virtual
                    "multiple": dbus.Boolean(False, variant_level=1),
                    "persist_mode": dbus.UInt32(2, variant_level=1),
                },
            )
        elif "streams" in results or response != 0:
            if response != 0:
                print(f"Start failed: response={response}")
                loop.quit()
                return
            streams = results.get("streams", [])
            print(f"Streams: {streams}")
            for node, props in streams:
                print(f"  PipeWire node ID: {node}  props: {dict(props)}")
                node_id = int(node)
            loop.quit()
        else:
            # SelectSources response
            if response != 0:
                print(f"SelectSources failed: response={response}")
                loop.quit()
                return
            print("Sources selected, calling Start...")
            start_token = random_token()
            start_path = f"/org/freedesktop/portal/desktop/request/{sender}/{start_token}"
            bus.add_signal_receiver(on_response, "Response",
                "org.freedesktop.portal.Request", path=start_path)
            sc.Start(session_handle, "", {"handle_token": dbus.String(start_token, variant_level=1)})

    req_path = f"/org/freedesktop/portal/desktop/request/{sender}/{request_token}"
    bus.add_signal_receiver(on_response, "Response",
        "org.freedesktop.portal.Request", path=req_path)

    print("Calling CreateSession...")
    sc.CreateSession({
        "handle_token": dbus.String(request_token, variant_level=1),
        "session_handle_token": dbus.String(session_token, variant_level=1),
    })

    GLib.timeout_add_seconds(30, lambda: (print("Timeout"), loop.quit()) and False)
    loop.run()

    if node_id is not None:
        print(f"\nSuccess! PipeWire node ID: {node_id}")
    else:
        print("\nNo node ID obtained.")

if __name__ == "__main__":
    main()
