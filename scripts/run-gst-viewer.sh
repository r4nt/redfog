#!/usr/bin/env bash
# Thin wrapper around `viewer --backend gst`: gst-wayland-display isn't on
# any package manager, so this clones/builds it into vendor/ (gitignored,
# same pattern as fetch-patched-deps.sh) if needed, then just runs viewer
# with its plugin dir injected via REDFOG_GST_WAYLAND_DISPLAY_PLUGIN_DIR.
# Everything else is passed straight through — see `viewer --help`.
#
# Usage:
#   scripts/run-gst-viewer.sh [viewer-args...]
#
# Only --backend gst is fixed — --mode (and everything else) is up to you,
# same as calling viewer directly; viewer's own default (--mode handoff)
# applies if you don't pass one. Examples:
#   scripts/run-gst-viewer.sh --mode single -- sway -c scripts/gst-viewer-sway-test.conf
#   scripts/run-gst-viewer.sh --mode broker --broker-socket /tmp/redfog-broker.sock --username "$USER"

set -uo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR_DIR="$REPO_DIR/vendor/gst-wayland-display"
PLUGIN_DIR="$VENDOR_DIR/install/lib/gstreamer-1.0"

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

# cd first — relative paths in a nested-command payload (e.g. `sway -c
# scripts/gst-viewer-sway-test.conf`) are resolved against viewer's own CWD
# (confirmed live: sway silently falls back to its default config otherwise).
cd "$REPO_DIR"
REDFOG_GST_WAYLAND_DISPLAY_PLUGIN_DIR="$PLUGIN_DIR" \
    cargo run -p viewer -- --backend gst "$@"
