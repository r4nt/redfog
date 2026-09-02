#!/usr/bin/env bash
# Captures a real CPU flamegraph of a live redfog-server process using
# flamegraph-rs (https://github.com/flamegraph-rs/flamegraph, installed via
# `cargo install flamegraph`). Complements scripts/profile-cpu.py: that one
# gives per-thread aggregate CPU%, this one shows WHERE in the call stack
# the time actually goes (down to inlined frames, given `debug = true` in
# the root Cargo.toml's [profile.release]).
#
# The static SVG this produces is only interactive (zoom/search/hover) when
# opened as a real page in a browser — for something as deep/multi-threaded
# as redfog-server it's close to illegible any other way. This script
# already generates a Perfetto-ready .perftxt for you (see below); to
# re-derive something from the raw perf.data yourself later instead —
# e.g. `samply import` for the interactive Firefox Profiler UI — run it as
# root (`sudo samply import "$PERF_DATA"`), same reasoning as the
# kptr_restrict note further down: file ownership doesn't matter here, the
# *reading* process needs to be root to resolve kernel-space frames.
# Chrome DevTools' own Performance panel doesn't apply here — it only reads
# browser/JS traces (.cpuprofile, Chrome trace-event JSON), not perf.data.
#
# Attaches to the already-running process (`perf record -p <pid>`) rather
# than launching a fresh one under `cargo flamegraph` directly: redfog-server
# needs the full scripts/sudo-live-session.sh setup (root, PAM session,
# broker socket, env vars) to do anything meaningful, so profiling a bare
# relaunch would profile a cold/nonfunctional pipeline instead of the real
# live one.
#
# Rebuild after any code change before capturing (`cargo build --workspace
# --release`) or the flamegraph will be stale/wrong.
#
# Usage: scripts/flamegraph.sh [duration_seconds]
#   REDFOG_FLAMEGRAPH_PID: profile this pid instead of auto-detecting
#
# Run this WHILE a real client is actively streaming (see
# scripts/sudo-live-session.sh) — idle CPU tells you nothing. Needs sudo
# (perf record needs root to attach to a root-owned process and read kernel
# symbols) — will prompt, more than once (see below).
#
# Symbolization (not just recording) needs root too, separately: this
# machine has `kernel.kptr_restrict=2`, which zeroes out /proc/kallsyms for
# non-root readers, so ANY kernel-space frame (every syscall-blocked
# thread's innermost frames — futex/epoll/ioctl waits, which is most
# threads most of the time) resolves to a bare, module-less `[unknown]`
# unless whatever reads perf.data (`perf script`, or `flamegraph`'s own
# internal call to it) is ALSO root. Confirmed live: of one real capture's
# ~7,890 resolvable frames, 7,806 were kernel addresses, ALL unresolved —
# recording as root and then handing perf.data to an unprivileged `perf
# script`/`flamegraph` step (an earlier version of this script did exactly
# that) reproduces this near-total illegibility every time. So the whole
# pipeline below — record AND both symbolization steps — stays under sudo;
# only the finished output files get handed back to you at the end.
#
# That's necessary but NOT sufficient at kptr_restrict=2 specifically: that
# value hides /proc/kallsyms' real addresses from *everyone*, root included
# — unlike kptr_restrict=1, which only restricts non-root readers. Confirmed
# live: `sudo perf report` alone still showed 100% `[unknown]` for every
# kernel-space frame at =2, even though the whole pipeline was already
# root. So this script now lowers it to 1 itself for the duration of the
# capture (root can resolve symbols there; unprivileged users still can't,
# so this isn't a broader exposure than perf needing root to attach at all
# already implies) and restores whatever it was before on exit, via a trap
# — covering Ctrl-C and errors, not just a clean run.

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

# perf has debuginfod support built in (confirmed via `perf version
# --build-options`), but `sudo` strips DEBUGINFOD_URLS same as it strips
# PATH -- explicitly passed through below (`sudo env DEBUGINFOD_URLS=...`),
# same fix as the existing `PATH` one. Read from /etc/debuginfod/*.urls
# directly (matching /etc/profile.d/debuginfod.sh's own logic) rather than
# hardcoding a specific distro's server: confirmed live on an Arch+CachyOS
# machine that the *stock* Arch debuginfod server alone doesn't cover
# CachyOS's own optimized rebuilds (different build-ids from upstream Arch
# even at the "same" version -- glibc and libopus both 404'd there) while
# CachyOS's own server (also listed in that same directory) does. Best
# effort only, not a guaranteed fix like the kptr_restrict one above: even
# with the right server, coverage isn't universal (confirmed via nsys's own
# earlier debuginfod use in this same investigation, ~33% query success
# rate) -- misses just stay `[unknown]`, same as without this at all,
# they don't error the capture out.
DEBUGINFOD_URLS="${DEBUGINFOD_URLS:-$(find /etc/debuginfod -name '*.urls' -exec cat {} + 2>/dev/null | tr '\n' ' ')}"

DURATION="${1:-10}"
OUT_DIR="/tmp/redfog-flamegraph"
PERF_DATA="$OUT_DIR/perf.data"
STAMP="$(date +%Y%m%d-%H%M%S)"
SVG="$OUT_DIR/flamegraph-$STAMP.svg"
PERFTEXT="$OUT_DIR/perfetto-$STAMP.perftxt"
mkdir -p "$OUT_DIR"

if [ -n "${REDFOG_FLAMEGRAPH_PID:-}" ]; then
    PID="$REDFOG_FLAMEGRAPH_PID"
else
    # Filter out defunct/zombie processes (a prior run's unreaped child can
    # linger under the same name — confirmed seen live on this machine) so
    # we don't silently error out trying to attach perf to one, or (worse,
    # with more than one live candidate) silently pick the wrong one.
    mapfile -t candidates < <(pgrep -x redfog-server || true)
    live=()
    for p in "${candidates[@]}"; do
        state="$(ps -o stat= -p "$p" 2>/dev/null | tr -d ' ')"
        [[ "$state" == Z* ]] && continue
        live+=("$p")
    done
    if [ "${#live[@]}" -eq 0 ]; then
        echo "error: no running (non-zombie) redfog-server process found — start one via scripts/sudo-live-session.sh first, or set REDFOG_FLAMEGRAPH_PID" >&2
        exit 1
    fi
    if [ "${#live[@]}" -gt 1 ]; then
        echo "error: multiple redfog-server processes found, set REDFOG_FLAMEGRAPH_PID explicitly: ${live[*]}" >&2
        exit 1
    fi
    PID="${live[0]}"
fi

echo "recording perf data for redfog-server (pid $PID) over ${DURATION}s..."
# `dwarf,16384`: the default 8K stack-dump size sometimes truncates the
# unwind through Rust's closure-heavy/monomorphized call chains (GStreamer
# callbacks, tracing spans, etc.) before it reaches main() — confirmed
# empirically to be a real problem with the default in similarly-shaped
# Rust services, not a hypothetical concern.
sudo perf record -F 997 -g --call-graph dwarf,16384 -p "$PID" -o "$PERF_DATA" -- sleep "$DURATION"

echo "generating flamegraph (root, for kernel-frame symbols — see header comment)..."
# `env "PATH=$PATH"`: `flamegraph`/`cargo-flamegraph` live in ~/.cargo/bin,
# which sudo's secure_path strips for the root command it execs — without
# this, plain `sudo flamegraph` fails with "command not found" even though
# it resolves fine for you unprivileged (confirmed live). `DEBUGINFOD_URLS`
# passed the same way — see this script's own header comment.
sudo env "PATH=$PATH" "DEBUGINFOD_URLS=$DEBUGINFOD_URLS" flamegraph --perfdata "$PERF_DATA" -o "$SVG" --title "redfog-server pid $PID, ${DURATION}s"

echo "generating Perfetto-compatible text (drag into https://ui.perfetto.dev)..."
sudo env "DEBUGINFOD_URLS=$DEBUGINFOD_URLS" perf script -i "$PERF_DATA" > "$PERFTEXT"

sudo chown "$(id -u):$(id -g)" "$PERF_DATA" "$SVG" "$PERFTEXT"

echo "done:"
echo "  $PERF_DATA   (raw — also: samply import \"$PERF_DATA\")"
echo "  $SVG   (static, open in a browser for zoom/search)"
echo "  $PERFTEXT   (drag into https://ui.perfetto.dev)"
