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
# NOTE: D-Bus session isolation and PipeWire/wireplumber bring-up (described
# below) now live in redfog-core::environment (ensure_private_dbus_session,
# HeadlessRuntime) since a future moonlight-style server needs the identical
# setup. kwin-viewer performs this itself on startup; proto.sh just builds
# and launches it. The discoveries are kept here for reference.
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

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Captured so cleanup() can restore it in the systemd --user environment,
# which dbus-update-activation-environment --systemd may overwrite globally
# for the user (it is not scoped to the private D-Bus session).
HOST_DISPLAY="${DISPLAY:-}"

LOG_DIR="/tmp/redfog-proto"
WIDTH=1920
HEIGHT=1080
SCALE=1.0

# ── helpers ──────────────────────────────────────────────────────────────────

die()  { echo "ERROR: $*" >&2; exit 1; }
info() { echo "==> $*"; }

# ── argument parsing ─────────────────────────────────────────────────────────

OPT_SKIP_BUILD=0
OPT_NO_PLASMASHELL=0
OPT_NO_ALACRITTY=0
OPT_APP=""
OPT_SIZE=""
OPT_KWIN_ARGS=""
OPT_PREVIEW_SCALE=2

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-build)      OPT_SKIP_BUILD=1 ;;
        --no-plasmashell)  OPT_NO_PLASMASHELL=1 ;;
        --no-alacritty)    OPT_NO_ALACRITTY=1 ;;
        --app)             OPT_APP="$2"; shift ;;
        --size)            OPT_SIZE="$2"; shift ;;
        --kwin-args)       OPT_KWIN_ARGS="$2"; shift ;;
        --preview-scale)   OPT_PREVIEW_SCALE="$2"; shift ;;
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
PREVIEW_W=$(( WIDTH / OPT_PREVIEW_SCALE ))
PREVIEW_H=$(( HEIGHT / OPT_PREVIEW_SCALE ))

CLEANED_UP=0
cleanup() {
    [[ $CLEANED_UP -eq 1 ]] && return
    CLEANED_UP=1
    info "Shutting down..."
    # Restore the host DISPLAY in the systemd user session
    if [[ -n "${HOST_DISPLAY:-}" ]]; then
        systemctl --user set-environment DISPLAY="$HOST_DISPLAY" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

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

mkdir -p "$LOG_DIR"

# PipeWire, wireplumber, and D-Bus session isolation are now brought up by
# kwin-viewer itself via redfog-core::environment (ensure_private_dbus_session,
# HeadlessRuntime) — see the note at the top of this script.

# ── 1. Build Binaries ────────────────────────────────────────────────────────

if [[ $OPT_SKIP_BUILD == 0 ]]; then
    info "Building kwin-capture, kwin-input, kwin-viewer and redfog-login..."
    cargo build --manifest-path "$SCRIPT_DIR/Cargo.toml" \
        -p kwin-capture -p kwin-input -p kwin-viewer -p redfog-login --release 2>"$LOG_DIR/cargo.log" \
        || { cat "$LOG_DIR/cargo.log" >&2; die "cargo build failed"; }
else
    info "Skipping build (--skip-build)"
fi

VIEWER_BIN="$SCRIPT_DIR/target/release/kwin-viewer"

# ── 2. Run kwin-viewer ────────────────────────────────────────────────────────

DAMAGE_APP="${OPT_APP:-alacritty}"
if [[ $OPT_NO_ALACRITTY -ne 0 ]]; then
    DAMAGE_APP=""
fi

info "Running managed kwin-viewer session..."

# kwin-viewer brings up its own private D-Bus session and PipeWire/wireplumber
# (see redfog-core::environment), so it just runs directly on the host display.
REDFOG_SCALE="$SCALE" "$VIEWER_BIN" "$PREVIEW_W" "$PREVIEW_H" "$DAMAGE_APP"
