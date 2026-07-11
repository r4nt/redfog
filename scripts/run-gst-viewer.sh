#!/usr/bin/env bash
# Builds (if needed) and runs the unified `viewer` in single-session mode
# against the gst-wayland-display backend — the gst-backend equivalent of
# running `viewer --backend kwin`, kept as its own script since it also
# handles cloning/building gst-wayland-display itself (not on any package
# manager). Idempotent: skips cloning/building steps that are already done.
#
# Usage:
#   scripts/run-gst-viewer.sh [width] [height] [nested-command...]
#
# Defaults: 1280x720, nested command `sway`. Example with a visible test app:
#   scripts/run-gst-viewer.sh 1280 720 sway -c scripts/gst-viewer-sway-test.conf
#
# For other modes/backends (--mode handoff|broker, --backend kwin), or to
# pass flags viewer itself supports (--desktop-name, --glx-vendor, ...), run
# `cargo build -p viewer --release && target/release/viewer --help` directly
# instead of this script.
#
# Env overrides:
#   REDFOG_RUNTIME_DIR   — defaults to /tmp/gst-viewer-runtime (not the
#                           broker's /tmp/redfog-runtime, which may be
#                           root-owned from an earlier sudo-based broker
#                           test and unwritable here).
#   REDFOG_GST_RENDER_NODE — defaults to "software"; pass a real DRM render
#                           node path (e.g. /dev/dri/renderD128) if you have
#                           working GPU acceleration.

set -uo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR_DIR="$REPO_DIR/vendor/gst-wayland-display"
PLUGIN_DIR="$VENDOR_DIR/install/lib/gstreamer-1.0"

WIDTH="${1:-1280}"
HEIGHT="${2:-720}"
shift $(( $# >= 2 ? 2 : $# )) || true
NESTED_COMMAND=("$@")
if [ "${#NESTED_COMMAND[@]}" -eq 0 ]; then
    NESTED_COMMAND=(sway)
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

echo "building viewer..."
(cd "$REPO_DIR" && cargo build -p viewer)

RUNTIME_DIR="${REDFOG_RUNTIME_DIR:-/tmp/gst-viewer-runtime}"
mkdir -p "$RUNTIME_DIR"

echo "starting viewer ${WIDTH}x${HEIGHT} (backend=gst, mode=single), nested: ${NESTED_COMMAND[*]}"
# cd into the repo root first — relative paths in NESTED_COMMAND (e.g. `sway
# -c scripts/gst-viewer-sway-test.conf`) are resolved by the nested process
# against viewer's own CWD, which otherwise depends on wherever the caller
# happened to invoke this script from (confirmed live: sway silently fails
# to find its config and falls back to its own default when run from any
# directory other than the repo root).
cd "$REPO_DIR"
REDFOG_GST_WAYLAND_DISPLAY_PLUGIN_DIR="$PLUGIN_DIR" \
REDFOG_RUNTIME_DIR="$RUNTIME_DIR" \
REDFOG_GST_RENDER_NODE="${REDFOG_GST_RENDER_NODE:-software}" \
"$REPO_DIR/target/debug/viewer" --backend gst --mode single --width "$WIDTH" --height "$HEIGHT" -- "${NESTED_COMMAND[@]}"
