#!/usr/bin/env bash
# proto.sh — end-to-end prototype: headless KDE Plasma session captured via
# PipeWire and displayed in a local window.
#
# Run this from any session (X11 or Wayland).  KWin runs on its own Wayland
# socket; your display is never touched.
#
# Dependencies:
#   kwin_wayland, plasmashell, alacritty, pipewire, wireplumber
#   cargo (Rust toolchain)
#   gstreamer1.0-pipewire  (provides the pipewiresrc element)
#   gstreamer1.0-plugins-base, gstreamer1.0-plugins-good
#
# ── DISCOVERIES FROM BRING-UP ───────────────────────────────────────────────
#
# PipeWire socket:
#   XDG_RUNTIME_DIR alone is unreliable across tools.
#   PIPEWIRE_REMOTE=<absolute-socket-path> is the authoritative env var.
#
# Startup order matters:
#   KWin must start AFTER PipeWire (its screencast plugin connects at init).
#
# wireplumber:
#   Must be running for the PipeWire graph to advance past 'suspended'.
#   Without it, nodes never transition to running and frames never flow.
#
# zkde_screencast_unstable_v1 version cap:
#   The wayland-protocols-plasma crate bundles v4 XML.  KWin 6.7+ implements
#   v6 and sends a v6-only 'serial' event.  Binding at version.min(4) avoids
#   the "Malformed Wayland message" crash.
#
# stream_virtual_output vs stream_output:
#   stream_output requires a pre-existing wl_output, which doesn't exist in
#   virtual/headless mode.  stream_virtual_output creates both the output and
#   the PipeWire node in one call — use this for headless sessions.
#
# kwin-capture lifetime:
#   Closing the Wayland connection destroys the virtual output and its
#   PipeWire node.  kwin-capture must stay alive for the duration of capture.
#
# Stale KWin socket:
#   A leftover socket+lock from a crashed run prevents KWin from starting.
#   Always rm -f the socket and its .lock before launching.
#
# Surface damage required:
#   KWin only renders frames when Wayland clients commit new buffers.
#   Without a client drawing, the PipeWire stream sends at most one black
#   frame and then stalls.  A terminal (alacritty) is the reliable damage
#   source; plasmashell alone is not enough in a minimal headless environment.
#
# plasmashell in headless:
#   plasmashell needs the full Plasma DBus service stack to render (kded,
#   plasma-workspace scripts, etc.).  In a minimal environment it may crash
#   silently (empty log) before drawing anything.  It is kept as best-effort
#   but alacritty is the primary guaranteed damage source.
#
# D-Bus session isolation (dbus-run-session):
#   When proto.sh runs inside an existing desktop session it shares that
#   session's DBUS_SESSION_BUS_ADDRESS.  This causes two problems:
#     1. plasmashell crashes immediately because org.kde.plasmashell is already
#        owned by the desktop session.
#     2. kwin_wayland claims org.kde.KWin, confusing xdg-desktop-portal-kde
#        which tries to use it for the desktop's screencasting.
#   Fix: re-exec inside dbus-run-session to get a private bus.  The script
#   detects this via _REDFOG_INNER and re-execs itself automatically.
#   This mirrors how separate KDE login sessions work (each gets its own bus).
#
# D-Bus activation environment (dbus-update-activation-environment):
#   Services auto-activated by D-Bus (xdg-desktop-portal, xdg-desktop-portal-kde)
#   inherit the activation environment of the bus daemon, not the parent shell.
#   Must call dbus-update-activation-environment AFTER KWin is up (so
#   WAYLAND_DISPLAY is valid) and BEFORE plasmashell (so the portal backend
#   connects to our headless KWin, not the desktop compositor).
#   Key vars to push: XDG_RUNTIME_DIR, WAYLAND_DISPLAY, PIPEWIRE_REMOTE.
#
# Dynamic resize via kde_output_management_v2:
#   kwin-capture accepts "resize WxH" lines on its stdin (via a named FIFO at
#   $LOG_DIR/kwin-capture-cmd.fifo).  Two-phase protocol:
#     Phase 1: create_mode_list → set_resolution/set_refresh_rate/add_mode →
#              create_configuration → set_custom_modes → apply
#              Wait for config.applied event.
#     Phase 2: create_configuration → mode(output, new_mode) → apply
#              Wait for config.applied → done.
#   The FIFO is opened read-write (<>) in the shell to avoid blocking on open.
#   wayland-client 0.31 dispatch_pending() does NOT read from the socket;
#   must call prepare_read() + read() in the event loop to receive replies.
#
# XDG portal screencast investigation:
#   org.freedesktop.portal.ScreenCast (AvailableSourceTypes=7) does support
#   Virtual outputs (type=4).  With WAYLAND_DISPLAY pointing at our headless
#   KWin, xdg-desktop-portal-kde routes to it correctly and shows the Allow
#   dialog inside the headless session (visible through the GStreamer preview).
#   However: the dialog requires user input to dismiss; no headless bypass
#   exists (the restore_token mechanism requires a first interactive grant, and
#   pre-injecting a token would require knowledge of the internal token format —
#   deeper coupling than the protocol we already use).
#   xdg-desktop-portal-kde uses zkde_screencast_unstable_v1 internally anyway,
#   so using it directly (kwin-capture) is equivalent and avoids the dialog.
#   Decision: use the protocol directly.  The "unstable" label means "not yet
#   formally ratified by wayland-protocols", not "likely to break"; OBS, GNOME
#   Remote Desktop, and xdg-desktop-portal-kde all depend on it.
#
# NVIDIA GBM (RTX 2080, driver 610.43.02):
#   KWin's virtual backend uses GpuManager → RenderDevice::open() →
#   gbm_create_device() to find GPU render devices.  On NVIDIA proprietary,
#   gbm_create_device() on /dev/dri/renderD128 segfaults even though
#   nvidia-drm_gbm.so is present at /usr/lib/gbm/.  As a result,
#   GpuManager finds no render devices, VirtualBackend::supportedCompositors()
#   returns empty, and KWin falls back to software rendering.
#   The PipeWire stream therefore negotiates BGRx (SHM / CPU readback) rather
#   than DMA_DRM (zero-copy GPU buffer).  This is a prototype limitation; the
#   production path will use NVENC directly from the PipeWire DMA-BUF.
#   TODO: investigate GBM segfault on NVIDIA; may need kernel driver update,
#   nvidia-drm.modeset=1, or switching to EGL_PLATFORM_DEVICE_EXT instead of
#   EGL_PLATFORM_GBM_KHR in the KWin virtual backend.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ── D-Bus session isolation ───────────────────────────────────────────────────
#
# Re-exec inside a fresh D-Bus session so the headless stack (kwin_wayland,
# plasmashell) gets its own bus and doesn't collide with the desktop session.
# Without this, plasmashell crashes immediately because org.kde.plasmashell is
# already owned by the desktop, and org.kde.KWin gets stolen from the desktop
# portal by our headless kwin_wayland.
if [[ -z "${_REDFOG_INNER:-}" ]]; then
    export _REDFOG_INNER=1
    exec dbus-run-session -- bash "${BASH_SOURCE[0]}" "$@"
fi
HOST_WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-}"
HOST_DISPLAY="${DISPLAY:-}"
HOST_XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-}"

SOCKET="redfog-proto-0"
RUNTIME="/tmp/redfog-runtime"
PW_SOCK="$RUNTIME/pipewire-0"
LOG_DIR="/tmp/redfog-proto"
PIDS=()
WIDTH=1920
HEIGHT=1080
SCALE=1.0

# ── helpers ──────────────────────────────────────────────────────────────────

die()  { echo "ERROR: $*" >&2; exit 1; }
info() { echo "==> $*"; }

# ── argument parsing ─────────────────────────────────────────────────────────
#
# Usage: proto.sh [options]
#
#   --skip-build       skip cargo build (use existing binary)
#   --no-plasmashell   don't launch plasmashell
#   --no-alacritty     don't launch alacritty as damage source
#   --app <cmd>        launch <cmd> instead of alacritty as damage source
#   --size WxH         virtual display size (default: 1920x1080)
#   --kwin-args <a>    pass extra arguments to kwin_wayland
#   --preview-scale N  scale preview window by 1/N (default: 2 = half size)
#   --verbose          tail all logs to stdout instead of only printing errors

OPT_SKIP_BUILD=0
OPT_NO_PLASMASHELL=0
OPT_NO_ALACRITTY=0
OPT_APP=""
OPT_SIZE=""
OPT_KWIN_ARGS=""
OPT_PREVIEW_SCALE=2
OPT_VERBOSE=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-build)      OPT_SKIP_BUILD=1 ;;
        --no-plasmashell)  OPT_NO_PLASMASHELL=1 ;;
        --no-alacritty)    OPT_NO_ALACRITTY=1 ;;
        --app)             OPT_APP="$2"; shift ;;
        --size)            OPT_SIZE="$2"; shift ;;
        --kwin-args)       OPT_KWIN_ARGS="$2"; shift ;;
        --preview-scale)   OPT_PREVIEW_SCALE="$2"; shift ;;
        --verbose)         OPT_VERBOSE=1 ;;
        *) die "unknown argument: $1 (see script header for usage)" ;;
    esac
    shift
done

if [[ -n "$OPT_SIZE" ]]; then
    WIDTH="${OPT_SIZE%x*}"
    HEIGHT="${OPT_SIZE#*x}"
    [[ "$WIDTH" =~ ^[0-9]+$ && "$HEIGHT" =~ ^[0-9]+$ ]] \
        || die "--size must be WxH (e.g. 1280x720), got: $OPT_SIZE"
fi

# The KWin virtual output runs at the preview resolution directly — no scaling.
# This avoids coordinate translation issues and keeps CPU usage low.
PREVIEW_W=$(( WIDTH / OPT_PREVIEW_SCALE ))
PREVIEW_H=$(( HEIGHT / OPT_PREVIEW_SCALE ))
WIDTH=$PREVIEW_W
HEIGHT=$PREVIEW_H

# launch <logfile> <cmd> [args...] — start a background process, redirect its
# output to logfile unless --verbose, then append its PID to PIDS.
launch() {
    local logfile="$1"; shift
    if [[ $OPT_VERBOSE -eq 1 ]]; then
        "$@" &
    else
        "$@" &>"$logfile" &
    fi
    PIDS+=($!)
}

CLEANED_UP=0
cleanup() {
    [[ $CLEANED_UP -eq 1 ]] && return
    CLEANED_UP=1
    info "Shutting down..."
    for pid in "${PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    # Restore the host DISPLAY in the systemd user session
    if [[ -n "${HOST_DISPLAY:-}" ]]; then
        systemctl --user set-environment DISPLAY="$HOST_DISPLAY" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

wait_path() {
    local path="$1" secs="${2:-10}"
    for ((i = 0; i < secs * 2; i++)); do
        [[ -e "$path" ]] && return 0
        sleep 0.5
    done
    return 1
}

check_cmd() {
    for cmd in "$@"; do
        command -v "$cmd" &>/dev/null || die "required command not found: $cmd"
    done
}

# ── preflight ────────────────────────────────────────────────────────────────

# Source Rust toolchain if available (needed for cargo build).
[[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"

check_cmd kwin_wayland plasmashell pipewire wireplumber gst-launch-1.0
[[ $OPT_SKIP_BUILD -eq 1 ]] || check_cmd cargo

gst-inspect-1.0 pipewiresrc &>/dev/null \
    || die "GStreamer pipewiresrc not found — install gstreamer1.0-pipewire"

# Pick a video sink: prefer ximagesink (X11) or waylandsink.
if [[ -n "${DISPLAY:-}" ]] && gst-inspect-1.0 ximagesink &>/dev/null; then
    VIDEO_SINK="ximagesink"
elif [[ -n "${WAYLAND_DISPLAY:-}" ]] && gst-inspect-1.0 waylandsink &>/dev/null; then
    VIDEO_SINK="waylandsink"
else
    die "no usable video sink found (need ximagesink or waylandsink)"
fi
info "Video sink: $VIDEO_SINK"

mkdir -p "$RUNTIME" "$LOG_DIR"
chmod 700 "$RUNTIME"

# Write the session bus address to a known file so external tools can reach it.
echo "$DBUS_SESSION_BUS_ADDRESS" > "$LOG_DIR/dbus-session-address"

# ── 1. PipeWire ──────────────────────────────────────────────────────────────
#
# Clean up any stale sockets from a previous run so PipeWire can bind fresh.
# We use a private RUNTIME dir so we never collide with the system PipeWire.
# PIPEWIRE_REMOTE (absolute socket path) is the reliable way to point clients
# at a non-default PipeWire instance; XDG_RUNTIME_DIR alone is not enough.

info "Starting PipeWire..."
rm -f "$PW_SOCK" "$PW_SOCK.lock" "$RUNTIME/pipewire-0-manager" "$RUNTIME/pipewire-0-manager.lock"

launch "$LOG_DIR/pipewire.log" \
    env XDG_RUNTIME_DIR="$RUNTIME" pipewire

wait_path "$PW_SOCK" 10 || die "PipeWire socket did not appear — see $LOG_DIR/pipewire.log"
PIPEWIRE_REMOTE="$PW_SOCK" pw-cli info 0 &>/dev/null \
    || die "PipeWire started but not responding"
info "PipeWire up"

export PIPEWIRE_REMOTE="$PW_SOCK"

# ── 2. Wireplumber ───────────────────────────────────────────────────────────
#
# Wireplumber is the PipeWire session manager.  Without it the graph stays
# in 'suspended' and nodes never transition to 'running'.

info "Starting wireplumber..."
launch "$LOG_DIR/wireplumber.log" \
    env XDG_RUNTIME_DIR="$RUNTIME" PIPEWIRE_REMOTE="$PW_SOCK" wireplumber
sleep 1

# ── 3. KWin in virtual / headless mode ───────────────────────────────────────
#
# KWIN_PLATFORM=virtual: headless backend, no DRM/KMS, no dummy EDID.
# KWin renders via OpenGL/Vulkan into GPU memory when a GBM-capable render
# device is found (GpuManager scans /dev/dri/ via udev).  On NVIDIA with the
# proprietary driver, gbm_create_device() segfaults, so KWin falls back to
# software rendering — frames are exported via SHM (BGRx) instead of DMA-BUF.
# Must start after PipeWire so the screencast plugin can connect.

info "Starting KWin (virtual platform) on WAYLAND_DISPLAY=$SOCKET..."
rm -f "$RUNTIME/$SOCKET" "$RUNTIME/$SOCKET.lock"

# Snapshot existing X11 sockets so we can detect the one XWayland creates.
X11_BEFORE=$(ls /tmp/.X11-unix/ 2>/dev/null | sort)

launch "$LOG_DIR/kwin.log" \
    env -u WAYLAND_DISPLAY -u DISPLAY \
        KWIN_PLATFORM=virtual \
        KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 \
        XDG_RUNTIME_DIR="$RUNTIME" \
        PIPEWIRE_REMOTE="$PW_SOCK" \
        kwin_wayland --no-lockscreen --socket "$SOCKET" --xwayland $OPT_KWIN_ARGS
KWIN_PID=${PIDS[-1]}

wait_path "$RUNTIME/$SOCKET" 15 \
    || die "KWin Wayland socket did not appear — see $LOG_DIR/kwin.log"
info "KWin up"

# Detect the XWayland display KWin just started (new socket in /tmp/.X11-unix/).
XWAYLAND_DISPLAY=""
for ((i = 0; i < 40; i++)); do
    NEW=$(comm -13 <(echo "$X11_BEFORE") <(ls /tmp/.X11-unix/ 2>/dev/null | sort) | head -1)
    if [[ -n "$NEW" ]]; then
        XWAYLAND_DISPLAY=":${NEW#X}"
        info "KWin XWayland on DISPLAY=$XWAYLAND_DISPLAY"
        break
    fi
    sleep 0.25
done
[[ -n "$XWAYLAND_DISPLAY" ]] || info "Warning: XWayland display not detected"

# Point D-Bus-activated services at our headless session, not the desktop.
# xdg-desktop-portal-kde uses WAYLAND_DISPLAY to connect to KWin for screencasting;
# without this it falls back to the host display and fails.
export XDG_RUNTIME_DIR="$RUNTIME"
export WAYLAND_DISPLAY="$SOCKET"
# Push DISPLAY into the systemd user session environment so fish (and other shells)
# that read from systemctl --user show-environment pick up KWin's XWayland, not CRD.
if [[ -n "$XWAYLAND_DISPLAY" ]]; then
    systemctl --user set-environment DISPLAY="$XWAYLAND_DISPLAY" 2>/dev/null || true
fi
dbus-update-activation-environment \
    XDG_RUNTIME_DIR WAYLAND_DISPLAY PIPEWIRE_REMOTE DISPLAY 2>/dev/null || true

# ── 4. Plasmashell ───────────────────────────────────────────────────────────
#
# Without any Wayland clients there is no surface damage and KWin does not
# render frames.  We try plasmashell first (best-effort: it needs Plasma DBus
# services to render anything useful), and in any case launch a plain terminal
# as a guaranteed damage source so there's always visible content.

if [[ $OPT_NO_PLASMASHELL -eq 0 ]]; then
    info "Starting plasmashell..."
    launch "$LOG_DIR/plasmashell.log" \
        env WAYLAND_DISPLAY="$SOCKET" \
            XDG_RUNTIME_DIR="$RUNTIME" \
            PIPEWIRE_REMOTE="$PW_SOCK" \
            DISPLAY="${XWAYLAND_DISPLAY:-}" \
            plasmashell --no-respawn
fi

# ── 5. Build kwin-capture ────────────────────────────────────────────────────

if [[ $OPT_SKIP_BUILD -eq 0 ]]; then
    info "Building kwin-capture, kwin-input and kwin-viewer..."
    cargo build --manifest-path "$SCRIPT_DIR/Cargo.toml" \
        -p kwin-capture -p kwin-input -p kwin-viewer --release 2>"$LOG_DIR/cargo.log" \
        || { cat "$LOG_DIR/cargo.log" >&2; die "cargo build failed"; }
else
    info "Skipping build (--skip-build)"
fi

CAPTURE_BIN="$SCRIPT_DIR/target/release/kwin-capture"
INPUT_BIN="$SCRIPT_DIR/target/release/kwin-input"
VIEWER_BIN="$SCRIPT_DIR/target/release/kwin-viewer"

# ── 6. Request a PipeWire stream from KWin ───────────────────────────────────
#
# kwin-capture connects to KWin as a Wayland client and calls
# zkde_screencast_unstable_v1.stream_virtual_output, which:
#   - creates a 1920×1080 virtual output inside KWin
#   - starts a PipeWire stream for it
#   - returns the PipeWire node ID via the 'created' event
#
# kwin-capture prints the node ID and then loops forever.  It must stay alive:
# when the Wayland connection closes KWin destroys the virtual output and its
# PipeWire node.

info "Requesting PipeWire stream from KWin..."
NODE_FIFO="$LOG_DIR/node-id.fifo"
CMD_FIFO="$LOG_DIR/kwin-capture-cmd.fifo"
rm -f "$NODE_FIFO" "$CMD_FIFO"
mkfifo "$NODE_FIFO" "$CMD_FIFO"
# Keep CMD_FIFO open in this shell so kwin-capture doesn't see EOF.
# Open read+write (<>) so the open(2) doesn't block waiting for a reader.
exec 4<>"$CMD_FIFO"

WAYLAND_DISPLAY="$SOCKET" \
XDG_RUNTIME_DIR="$RUNTIME" \
PIPEWIRE_REMOTE="$PW_SOCK" \
REDFOG_WIDTH="$WIDTH" \
REDFOG_HEIGHT="$HEIGHT" \
REDFOG_SCALE="$SCALE" \
    "$CAPTURE_BIN" <"$CMD_FIFO" >"$NODE_FIFO" 2>"$LOG_DIR/kwin-capture.log" &
PIDS+=($!)
# kwin-capture always logs to file so node ID can be read from the FIFO cleanly
# Send resize commands: echo "resize 1280x720" > /tmp/redfog-proto/kwin-capture-cmd.fifo

NODE_ID=$(head -n1 "$NODE_FIFO")
[[ -n "$NODE_ID" ]] || die "kwin-capture did not return a node ID — see $LOG_DIR/kwin-capture.log"
kill -0 "$KWIN_PID" 2>/dev/null || die "KWin died before stream was created — see $LOG_DIR/kwin.log"
info "PipeWire node ID: $NODE_ID"

# Wait for the node to appear in the PipeWire graph.
for ((i = 0; i < 20; i++)); do
    PIPEWIRE_REMOTE="$PW_SOCK" pw-dump 2>/dev/null | python3 -c "
import sys, json
nodes = json.load(sys.stdin)
ids = [str(n.get('id','')) for n in nodes if n.get('type') == 'PipeWire:Interface:Node']
sys.exit(0 if '$NODE_ID' in ids else 1)" && break
    sleep 0.5
done || info "Warning: node $NODE_ID not yet visible in pw-dump, proceeding anyway"

# ── 7. Input forwarding via org_kde_kwin_fake_input ──────────────────────────
#
# kwin-input binds org_kde_kwin_fake_input on the headless Wayland socket
# (same socket kwin-capture uses) and calls authenticate().  This is KWin's
# direct input injection interface — no EIS sessions, no portal dialogs, no
# timing dependency on focused windows.
#
# Send commands to the FIFO:
#   echo "key 28 1"        > /tmp/redfog-proto/kwin-input-cmd.fifo  # Enter down
#   echo "key 28 0"        > /tmp/redfog-proto/kwin-input-cmd.fifo  # Enter up
#   echo "rel 100 50"      > /tmp/redfog-proto/kwin-input-cmd.fifo  # mouse move
#   echo "button 272 1"    > /tmp/redfog-proto/kwin-input-cmd.fifo  # left click
#   echo "button 272 0"    > /tmp/redfog-proto/kwin-input-cmd.fifo
# Keycodes are Linux evdev keycodes (not X11/USB HID). Common codes:
#   1=ESC 28=Enter 57=Space 30=A 48=B ... 272=BTN_LEFT 273=BTN_RIGHT

INPUT_FIFO="$LOG_DIR/kwin-input-cmd.fifo"
rm -f "$INPUT_FIFO"
mkfifo "$INPUT_FIFO"
exec 5<>"$INPUT_FIFO"

# ── 8. Display via kwin-viewer ────────────────────────────────────────────────
#
# kwin-viewer connects to the PipeWire stream by ID and displays it in a window
# while forwarding keyboard and mouse events back to the headless KWin compositor.

info "Streaming to local window via kwin-viewer at ${PREVIEW_W}x${PREVIEW_H} (Ctrl-C to stop)..."

launch "$LOG_DIR/kwin-viewer.log" \
    env -u WAYLAND_DISPLAY -u XDG_RUNTIME_DIR \
        WAYLAND_DISPLAY="$HOST_WAYLAND_DISPLAY" \
        DISPLAY="$HOST_DISPLAY" \
        XDG_RUNTIME_DIR="$HOST_XDG_RUNTIME_DIR" \
        PIPEWIRE_REMOTE="$PW_SOCK" \
        "$VIEWER_BIN" "$NODE_ID" "$RUNTIME/$SOCKET" "$PREVIEW_W" "$PREVIEW_H"
VIEWER_PID=${PIDS[-1]}

# ── 9. Launch a damage source ─────────────────────────────────────────────────
#
# Without any Wayland client committing frames, KWin renders at most one black
# frame.  We need a real app drawing in the session.
# --no-alacritty skips this (useful if you're launching your own app manually).
# --app <cmd> uses a custom command instead of alacritty.

info "Starting kwin-input (fake_input)..."
WAYLAND_DISPLAY="$SOCKET" \
XDG_RUNTIME_DIR="$RUNTIME" \
    "$INPUT_BIN" <"$INPUT_FIFO" 2>"$LOG_DIR/kwin-input.log" &
PIDS+=($!)
sleep 0.3
grep -q "authenticated" "$LOG_DIR/kwin-input.log" 2>/dev/null \
    && info "kwin-input ready" \
    || info "kwin-input started (see $LOG_DIR/kwin-input.log)"

if [[ $OPT_NO_ALACRITTY -eq 0 ]]; then
    sleep 2
    DAMAGE_APP="${OPT_APP:-alacritty}"
    info "Launching $DAMAGE_APP as damage source..."
    launch "$LOG_DIR/damage-app.log" \
        env WAYLAND_DISPLAY="$SOCKET" \
            XDG_RUNTIME_DIR="$RUNTIME" \
            DISPLAY="${XWAYLAND_DISPLAY:-}" \
            $DAMAGE_APP
fi

info "Preview window open. Ctrl-C to stop."
wait $VIEWER_PID
