#!/usr/bin/env bash
# Captures a CPU profile of a live redfog-server process with samply
# (https://github.com/mstange/samply, installed via `cargo install samply`)
# and opens it in the interactive Firefox Profiler UI (a local server +
# your default browser — no Firefox-the-application required, it's just
# the profiler frontend at https://profiler.firefox.com served locally).
#
# Use this instead of scripts/flamegraph.sh's static SVG when the SVG is
# illegible: inferno's flamegraph SVGs only get their zoom/search/hover
# interactivity when opened in an actual browser as a real page — viewed
# any other way (image viewer, thumbnail, etc.) a process this deep and
# multi-threaded is just an unreadable wall of tiny boxes. samply's output
# is inherently interactive (timeline scrubbing, per-thread tracks, category
# coloring, ctrl+F search, click-drag zoom, stack-invert toggle), which
# scales much better to redfog-server's actual shape: many GStreamer
# threads, most of them idle most of the time.
#
# Rebuild after any code change before capturing (`cargo build --workspace
# --release`) — `[profile.release] debug = true` in the root Cargo.toml is
# what gives you real function names/line numbers/inlined-frame attribution
# instead of bare addresses.
#
# Usage: scripts/profile-samply.sh [duration_seconds]
#   REDFOG_SAMPLY_PID: profile this pid instead of auto-detecting
#
# Already have a scripts/flamegraph.sh capture instead? Skip recording
# again — just run: sudo samply import /tmp/redfog-flamegraph/perf.data
# (sudo matters here, not just for reading the file — see below).
#
# This machine has `kernel.kptr_restrict=2`, which zeroes out
# /proc/kallsyms for non-root readers, so any kernel-space frame (most
# threads' innermost frame most of the time — futex/epoll/ioctl waits)
# resolves to nothing without root. `samply record`'s single sudo'd
# invocation below covers both capturing AND resolving those addresses in
# one privileged step, which is why this script (unlike an earlier, since-
# fixed version of scripts/flamegraph.sh that recorded as root but then
# symbolized after handing the file back to your own user) doesn't need a
# second, separate root step for that part.
#
# Being root for that step still isn't enough on its own at kptr_restrict=2
# specifically, though — that value hides /proc/kallsyms' real addresses
# from *everyone*, root included, unlike kptr_restrict=1 (non-root readers
# only). Confirmed live via the same issue in scripts/flamegraph.sh. So
# this script also lowers it to 1 itself for the capture and restores
# whatever it was before on exit (a trap, covering Ctrl-C and errors too).
#
# Run this WHILE a real client is actively streaming (see
# scripts/sudo-live-session.sh) — idle CPU tells you nothing. Needs sudo
# (attaching to a root-owned process needs the same privilege perf itself
# would) — will prompt.

set -euo pipefail

# See the kptr_restrict paragraph above for why this exists at all.
ORIGINAL_KPTR_RESTRICT="$(cat /proc/sys/kernel/kptr_restrict)"
restore_kptr_restrict() {
    if [ "$(cat /proc/sys/kernel/kptr_restrict 2>/dev/null)" != "$ORIGINAL_KPTR_RESTRICT" ]; then
        sudo sysctl -qw kernel.kptr_restrict="$ORIGINAL_KPTR_RESTRICT"
    fi
}
trap restore_kptr_restrict EXIT
if [ "$ORIGINAL_KPTR_RESTRICT" -gt 1 ]; then
    echo "kernel.kptr_restrict=$ORIGINAL_KPTR_RESTRICT hides kernel-space symbols even from root — lowering to 1 for this capture, restoring on exit..."
    sudo sysctl -qw kernel.kptr_restrict=1
fi

# samply supports debuginfod, but `sudo` strips DEBUGINFOD_URLS same as it
# strips PATH below — see scripts/flamegraph.sh's header comment for the
# full rationale (reading /etc/debuginfod/*.urls directly rather than
# hardcoding one distro's server, best-effort not guaranteed).
DEBUGINFOD_URLS="${DEBUGINFOD_URLS:-$(find /etc/debuginfod -name '*.urls' -exec cat {} + 2>/dev/null | tr '\n' ' ')}"

DURATION="${1:-10}"
OUT_DIR="/tmp/redfog-samply"
PROFILE="$OUT_DIR/profile-$(date +%Y%m%d-%H%M%S).json.gz"
mkdir -p "$OUT_DIR"

if [ -n "${REDFOG_SAMPLY_PID:-}" ]; then
    PID="$REDFOG_SAMPLY_PID"
else
    # See scripts/flamegraph.sh for why zombies need filtering out here.
    mapfile -t candidates < <(pgrep -x redfog-server || true)
    live=()
    for p in "${candidates[@]}"; do
        state="$(ps -o stat= -p "$p" 2>/dev/null | tr -d ' ')"
        [[ "$state" == Z* ]] && continue
        live+=("$p")
    done
    if [ "${#live[@]}" -eq 0 ]; then
        echo "error: no running (non-zombie) redfog-server process found — start one via scripts/sudo-live-session.sh first, or set REDFOG_SAMPLY_PID" >&2
        exit 1
    fi
    if [ "${#live[@]}" -gt 1 ]; then
        echo "error: multiple redfog-server processes found, set REDFOG_SAMPLY_PID explicitly: ${live[*]}" >&2
        exit 1
    fi
    PID="${live[0]}"
fi

echo "recording with samply (pid $PID) over ${DURATION}s..."
# `env "PATH=$PATH"`: samply lives in ~/.cargo/bin, which sudo's secure_path
# strips for the root command it execs — same fix as scripts/flamegraph.sh
# uses for `flamegraph` itself (see that script's header comment).
# `DEBUGINFOD_URLS` passed the same way, same reason.
sudo env "PATH=$PATH" "DEBUGINFOD_URLS=$DEBUGINFOD_URLS" samply record -p "$PID" -d "$DURATION" --save-only -o "$PROFILE"
sudo chown "$(id -u):$(id -g)" "$PROFILE"

echo "opening interactive profiler UI (local server + your default browser)..."
samply load "$PROFILE"
