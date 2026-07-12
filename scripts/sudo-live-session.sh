#!/usr/bin/env bash
# Run this as your normal user (NOT via sudo directly — see below), leave
# it running, connect a real Moonlight client from another machine on the
# network at this host's IP:
#
#   bash scripts/sudo-live-session.sh
#
# Builds the gst-wayland-display plugin (cloning/building it if needed,
# same as scripts/run-gst-viewer.sh) and the whole workspace as YOUR user
# first, then re-execs itself under sudo for the part that actually needs
# root. Building as root would leave root-owned files in vendor/ and
# target/, breaking every future normal-user `cargo build` — that's the
# whole reason for the two-phase split, not just a style preference.
#
# Starts the REAL redfog-broker + redfog-server on the default Moonlight
# ports, using the REAL redfog-login as the Login stage (type your actual
# account's username/password) — with the gst-wayland-display plugin dir
# set, so BOTH of the login screen's default session-picker entries (KDE
# Plasma via KWin, Sway via gst-wayland-display) actually work, not just
# whichever one happened to be REDFOG_BACKEND's startup default. Pick
# either one from the real dropdown once connected.
#
# REDFOG_BROKER_PAM_SPAWN: defaults to 1, using the direct fork/PAM/setuid
# session path (crates/redfog-session-init) instead of generating systemd
# units — see design.md's "Privilege separation" section and project
# memory for the comparison between the two. Set to empty/0 in your own
# environment before invoking to use the systemd-unit path instead (still
# preserved across the sudo re-exec below).
#
# Ctrl-C stops both processes and cleans up.

set -uo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR_DIR="$REPO_DIR/vendor/gst-wayland-display"
PLUGIN_DIR="$VENDOR_DIR/install/lib/gstreamer-1.0"
SELF="$REPO_DIR/scripts/sudo-live-session.sh"

if [ "$(id -u)" -ne 0 ]; then
    # ---- Setup phase: must run as your normal user, not root (see the
    # header comment for why). ----
    if [ -n "${SUDO_USER:-}" ]; then
        echo "error: run this directly as yourself, not via 'sudo bash ...' — it escalates itself for only the part that needs root." >&2
        exit 1
    fi

    if [ ! -d "$VENDOR_DIR" ]; then
        echo "cloning gst-wayland-display (MIT) into $VENDOR_DIR..."
        mkdir -p "$REPO_DIR/vendor"
        git clone --depth 1 https://github.com/games-on-whales/gst-wayland-display.git "$VENDOR_DIR"
    fi
    if ! cargo cinstall --version >/dev/null 2>&1; then
        echo "installing cargo-c (one-time build tool for gst-wayland-display)..."
        cargo install cargo-c
    fi
    if [ ! -e "$PLUGIN_DIR/libgstwaylanddisplaysrc.so" ]; then
        echo "building gst-wayland-display plugin (cargo cinstall)..."
        (cd "$VENDOR_DIR" && cargo cinstall --prefix="$VENDOR_DIR/install")
    fi

    # --release, not a plain debug build: confirmed live, redfog-login's
    # own rendering is ~800ms/frame in debug vs ~2ms/frame in release —
    # slow enough in debug to starve input responsiveness badly enough to
    # feel completely broken (input events queue up behind frame writes).
    echo "building redfog workspace (release)..."
    (cd "$REPO_DIR" && cargo build --workspace --release)

    echo "re-executing as root for the broker/server run phase (sudo may prompt for your password)..."
    exec sudo -E env "PATH=$PATH" "$SELF" "$@"
fi

# ---- Run phase: root from here on (via the re-exec above, or a direct
# `sudo -E ... bash sudo-live-session.sh` invocation for anyone who knows
# what they're doing and wants to skip the build check). ----
: "${SUDO_USER:?must be run via sudo, not as a raw root login}"

# Move into our own systemd scope before starting anything, isolated from
# whatever cgroup this shell happens to have inherited (typically your
# desktop session's own — e.g. chrome-remote-desktop@<user>.service, if
# you're running this from a terminal inside a CRD-connected desktop, the
# common case this script was written for). Confirmed live: redfog-server
# leaked memory until the kernel OOM-killed it, and because it was still
# sitting inside that same cgroup, the kill took the whole CRD session down
# with it, not just redfog-server — see project memory for the incident.
# `systemd-run --scope` always talks to PID1's manager to create a brand
# new, top-level-slice-scoped unit regardless of the caller's own cgroup,
# so this genuinely decouples the two rather than just hoping `setsid`'s
# new session/process-group also implied a new cgroup (it doesn't).
# `MemoryMax` is a real backstop, not just cosmetic isolation: a future leak
# now gets OOM-killed within this scope specifically, long before it could
# threaten the rest of a 31G machine that also runs your desktop session.
if [ -z "${REDFOG_LIVE_SCOPED:-}" ]; then
    echo "moving into a dedicated systemd scope (redfog-live.slice), isolated from this shell's own cgroup..."
    exec systemd-run --scope --unit="redfog-live-$$" --slice=redfog-live.slice --collect \
        --property="MemoryMax=10G" \
        --setenv="REDFOG_LIVE_SCOPED=1" \
        --setenv="SUDO_USER=$SUDO_USER" \
        --setenv="PATH=$PATH" \
        --setenv="REDFOG_BROKER_PAM_SPAWN=${REDFOG_BROKER_PAM_SPAWN-1}" \
        -- "$SELF" "$@"
fi

if [ ! -e "$PLUGIN_DIR/libgstwaylanddisplaysrc.so" ]; then
    echo "warning: gst-wayland-display plugin not found at $PLUGIN_DIR — the Sway session option will fail if picked." >&2
    echo "         (this shouldn't happen via the normal 'bash scripts/sudo-live-session.sh' invocation; run that instead of sudo directly.)" >&2
fi

BROKER_LOG="/tmp/redfog-live-broker.log"
SERVER_LOG="/tmp/redfog-live-server.log"
REDFOG_BROKER_PAM_SPAWN="${REDFOG_BROKER_PAM_SPAWN-1}"

# TEMPORARY debugging aids for the "resume works but video updates are
# severely throttled" investigation — see the doc comments at their
# corresponding call sites (redfog-broker/src/session.rs's spawn_via_pam,
# redfog-server/src/main.rs) for what these actually do. Override to ""
# (empty) to disable either one; remove this whole block once that
# investigation concludes.
# kwin_screencast/kwin_platform_virtual found via `strings` on the
# installed screencast.so/libkwin.so — this repo has no KWin source.
REDFOG_DEBUG_KWIN_LOGGING_RULES="${REDFOG_DEBUG_KWIN_LOGGING_RULES-kwin_screencast.debug=true;kwin_platform_virtual.debug=true}"
# GStreamer debug level 6 ("LOG") on just the pipewiresrc category — verbose
# enough to see buffer/negotiation timing without TRACE-level memory dumps.
REDFOG_DEBUG_GST_DEBUG="${REDFOG_DEBUG_GST_DEBUG-pipewiresrc:6}"

cleanup() {
    echo "stopping..."
    [ -n "${SERVER_PID:-}" ] && kill -TERM "-$SERVER_PID" 2>/dev/null
    [ -n "${BROKER_PID:-}" ] && kill -TERM "-$BROKER_PID" 2>/dev/null
    wait 2>/dev/null
    for unit in /run/systemd/system/redfog-session-*; do
        [ -e "$unit" ] || continue
        name=$(basename "$unit")
        systemctl stop "$name" 2>/dev/null
        rm -f "$unit"
    done
    systemctl daemon-reload 2>/dev/null
    echo "stopped."
}
trap cleanup EXIT INT TERM

rm -rf /tmp/redfog-runtime
rm -f /tmp/redfog-live-broker.sock

echo "starting redfog-broker (PAM_SPAWN=${REDFOG_BROKER_PAM_SPAWN:-<unset, systemd-unit path>}, real PAM auth)..."
REDFOG_BROKER_PAM_SPAWN="$REDFOG_BROKER_PAM_SPAWN" \
REDFOG_DEBUG_KWIN_LOGGING_RULES="$REDFOG_DEBUG_KWIN_LOGGING_RULES" \
RUST_LOG=redfog_broker=debug \
setsid "$REPO_DIR/target/release/redfog-broker" > "$BROKER_LOG" 2>&1 &
BROKER_PID=$!

deadline=$((SECONDS + 10))
while [ ! -S /tmp/redfog-runtime/broker.sock ]; do
    if [ $SECONDS -ge $deadline ]; then
        echo "redfog-broker never created its socket, see $BROKER_LOG"
        exit 1
    fi
    sleep 0.2
done

echo "starting redfog-server on default ports (47989/47984/48010/...)..."
REDFOG_BROKER_SOCKET=/tmp/redfog-runtime/broker.sock \
REDFOG_LOGIN_APP="$REPO_DIR/target/release/redfog-login" \
REDFOG_USER_APP="plasmashell --no-respawn" \
REDFOG_GST_WAYLAND_DISPLAY_PLUGIN_DIR="$PLUGIN_DIR" \
REDFOG_DEBUG_GST_DEBUG="$REDFOG_DEBUG_GST_DEBUG" \
RUST_LOG=redfog_moonlight=debug,redfog_server=debug,gst_backend=debug \
setsid "$REPO_DIR/target/release/redfog-server" > "$SERVER_LOG" 2>&1 &
SERVER_PID=$!

deadline=$((SECONDS + 15))
while ! (exec 3<>/dev/tcp/127.0.0.1/47989) 2>/dev/null; do
    exec 3<&- 2>/dev/null || true
    if [ $SECONDS -ge $deadline ]; then
        echo "redfog-server never came up, see $SERVER_LOG"
        exit 1
    fi
    sleep 0.2
done
exec 3<&- 2>/dev/null || true

ip=$(ip -4 addr show 2>/dev/null | grep -oP '(?<=inet\s)\d+(\.\d+){3}' | grep -v '^127\.' | head -1)
echo ""
echo "=== redfog is up ==="
echo "Point a real Moonlight client at: $ip"
echo "(pairing PIN: watch $SERVER_LOG for the pairing request, or check the client UI)"
echo "Login screen's session picker offers both KDE Plasma (kwin) and Sway (gst-wayland-display) — pick either."
echo "broker log: $BROKER_LOG"
echo "server log: $SERVER_LOG"
echo "journal for the User-stage session (systemd-unit path only): journalctl -u 'redfog-session-*' -f"
echo ""
echo "Ctrl-C to stop."

wait
