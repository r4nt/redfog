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
SOCKET="redfog-proto-0"
RUNTIME="/tmp/redfog-runtime"
PW_SOCK="$RUNTIME/pipewire-0"
LOG_DIR="/tmp/redfog-proto"
PIDS=()

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
#   --kwin-args <a>    pass extra arguments to kwin_wayland
#   --verbose          tail all logs to stdout instead of only printing errors

OPT_SKIP_BUILD=0
OPT_NO_PLASMASHELL=0
OPT_NO_ALACRITTY=0
OPT_APP=""
OPT_KWIN_ARGS=""
OPT_VERBOSE=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-build)      OPT_SKIP_BUILD=1 ;;
        --no-plasmashell)  OPT_NO_PLASMASHELL=1 ;;
        --no-alacritty)    OPT_NO_ALACRITTY=1 ;;
        --app)             OPT_APP="$2"; shift ;;
        --kwin-args)       OPT_KWIN_ARGS="$2"; shift ;;
        --verbose)         OPT_VERBOSE=1 ;;
        *) die "unknown argument: $1 (see script header for usage)" ;;
    esac
    shift
done

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

cleanup() {
    info "Shutting down..."
    for pid in "${PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
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
launch "$LOG_DIR/kwin.log" \
    env -u WAYLAND_DISPLAY -u DISPLAY \
        KWIN_PLATFORM=virtual \
        KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 \
        XDG_RUNTIME_DIR="$RUNTIME" \
        PIPEWIRE_REMOTE="$PW_SOCK" \
        kwin_wayland --no-lockscreen --socket "$SOCKET" $OPT_KWIN_ARGS
KWIN_PID=${PIDS[-1]}

wait_path "$RUNTIME/$SOCKET" 15 \
    || die "KWin Wayland socket did not appear — see $LOG_DIR/kwin.log"
info "KWin up"

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
            plasmashell --no-respawn
fi

# ── 5. Build kwin-capture ────────────────────────────────────────────────────

if [[ $OPT_SKIP_BUILD -eq 0 ]]; then
    info "Building kwin-capture..."
    cargo build --manifest-path "$SCRIPT_DIR/Cargo.toml" \
        -p kwin-capture --release 2>"$LOG_DIR/cargo.log" \
        || { cat "$LOG_DIR/cargo.log" >&2; die "cargo build failed"; }
else
    info "Skipping build (--skip-build)"
fi

CAPTURE_BIN="$SCRIPT_DIR/target/release/kwin-capture"

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
rm -f "$NODE_FIFO"
mkfifo "$NODE_FIFO"

WAYLAND_DISPLAY="$SOCKET" \
XDG_RUNTIME_DIR="$RUNTIME" \
PIPEWIRE_REMOTE="$PW_SOCK" \
    "$CAPTURE_BIN" >"$NODE_FIFO" 2>"$LOG_DIR/kwin-capture.log" &
PIDS+=($!)
# kwin-capture always logs to file so node ID can be read from the FIFO cleanly

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

# ── 7. Display via GStreamer ──────────────────────────────────────────────────
#
# pipewiresrc connects to the node by ID.  KWin negotiates BGRx (SHM path)
# or DMA_DRM (zero-copy DMA-BUF) depending on driver support.
# Watch the caps line in the output: DMA_DRM = zero-copy; BGRx = CPU copy.

info "Streaming to local window via $VIDEO_SINK (Ctrl-C to stop)..."

launch "$LOG_DIR/gst.log" \
    env PIPEWIRE_REMOTE="$PW_SOCK" \
        gst-launch-1.0 -v \
            pipewiresrc path="$NODE_ID" do-timestamp=true \
            ! videoconvert \
            ! "$VIDEO_SINK" sync=false
GST_PID=${PIDS[-1]}

# ── 8. Launch a damage source ─────────────────────────────────────────────────
#
# Without any Wayland client committing frames, KWin renders at most one black
# frame.  We need a real app drawing in the session.
# --no-alacritty skips this (useful if you're launching your own app manually).
# --app <cmd> uses a custom command instead of alacritty.

if [[ $OPT_NO_ALACRITTY -eq 0 ]]; then
    sleep 2
    DAMAGE_APP="${OPT_APP:-alacritty}"
    info "Launching $DAMAGE_APP as damage source..."
    launch "$LOG_DIR/damage-app.log" \
        env WAYLAND_DISPLAY="$SOCKET" \
            XDG_RUNTIME_DIR="$RUNTIME" \
            $DAMAGE_APP
fi

info "Preview window open. Ctrl-C to stop."
wait $GST_PID
