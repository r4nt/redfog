#!/usr/bin/env bash
# Runs the full sudo-free, no-special-hardware test suite -- the whole
# workspace except kwin-capture (needs a real NVIDIA GPU; see
# scripts/run-gpu-tests.sh) and a couple of individually-gated tests that
# need real GPU/compositor hardware too (connection_integration.rs's tests
# behind the "compositor-tests" cargo feature, not enabled here -- also
# covered by run-gpu-tests.sh) or a separately-built plugin
# (gst_wayland_display_backend_smoke_test). Matches exactly what
# .github/workflows/tests.yml runs in CI.
#
# Sudo-free: connection_integration.rs uses its REDFOG_BROKER_FAKE_SPAWN
# path here (direct kwin_wayland spawn, no root) rather than the real
# cross-user broker path — see scripts/sudo-test-runner.sh for that one.
#
# --test-threads=1 throughout: this project's own history has confirmed
# real flakiness in these specific suites under full parallel runs
# (resource contention between simultaneous kwin_wayland/PipeWire
# instances), not a general cargo-test recommendation.
#
# Each suite's full output (including subprocess noise -- dbus-daemon,
# xdg-desktop-portal, etc. -- confirmed live to be substantial) goes
# straight to a log file under /tmp/redfog-test-logs/, not the terminal --
# only the pass/fail summary line and that file's path print here. Read the
# log directly if a suite fails or you want to see what actually happened.
#
# See scripts/run-all-tests.sh to also run everything that needs a real
# GPU, in one go.
#
# Usage:
#   bash scripts/run-tests.sh

set -uo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_DIR"

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

echo "=== building workspace (excluding kwin-capture) ==="
cargo build --workspace --exclude kwin-capture

FAILED=0

LOG_DIR=/tmp/redfog-test-logs
mkdir -p "$LOG_DIR"
RUN_STAMP="$(date +%Y%m%d-%H%M%S)"

# Each spawn in connection_integration.rs (ServerProcess/BrokerProcess, plus
# redfog-broker's own REDFOG_BROKER_FAKE_SPAWN kwin_wayland child) registers
# PR_SET_PDEATHSIG(SIGKILL) on itself, so the kernel kills the whole tree the
# moment its direct parent dies -- for any reason, not just a clean unwind
# (Drop only fires on that). That covers a hang followed by an external
# kill -9 on this script, Ctrl-C, or a genuine crash, without needing a
# wrapping systemd scope (which this script previously used here, and which
# doesn't work without a working `systemd --user` instance -- not available
# without either an active logind session or `loginctl enable-linger`,
# neither of which this should require just to run tests). See
# connection_integration.rs's `die_with_parent` for the actual mechanism.
run_suite() {
    local name="$1"
    shift
    local log_file="$LOG_DIR/${name// /-}-$RUN_STAMP.log"
    echo ""
    echo "=== running $name -- log: $log_file ==="
    if "$@" > "$log_file" 2>&1; then
        local summary_line
        summary_line="$(grep -E '^test result:' "$log_file" | tail -1)"
        echo "=== $name: passed: ${summary_line:-no test result line found} ==="
    else
        echo "!!! $name FAILED -- see $log_file for full output, including subprocess logs"
        FAILED=1
    fi
}

run_suite "workspace tests (excluding kwin-capture)" \
    cargo test --workspace --exclude kwin-capture -- --test-threads=1 --skip gst_wayland_display_backend_smoke_test

echo ""
if [ "$FAILED" -eq 0 ]; then
    echo "=== all suites passed ==="
else
    echo "=== one or more suites FAILED — see output above ==="
fi
exit "$FAILED"
