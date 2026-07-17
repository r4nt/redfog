#!/usr/bin/env bash
# Runs the full sudo-free test suite: workspace build, and the main moonlight
# integration suite (redfog-moonlight/tests/connection_integration.rs).
#
# Sudo-free: connection_integration.rs uses its REDFOG_BROKER_FAKE_SPAWN
# path here (direct kwin_wayland spawn, no root) rather than the real
# cross-user broker path — see scripts/sudo-test-runner.sh for that one.
#
# Tests marked #[ignore] (known-unfixed-bug documentation, or ones that
# take minutes rather than seconds) are skipped by default, matching
# `cargo test`'s own default. Pass --include-ignored to also run those.
#
# --test-threads=1 throughout: this project's own history has confirmed
# real flakiness in these specific suites under full parallel runs
# (resource contention between simultaneous kwin_wayland/PipeWire
# instances), not a general cargo-test recommendation.
#
# Usage:
#   bash scripts/run-tests.sh [--include-ignored]

set -uo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_DIR"

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

INCLUDE_IGNORED=0
if [ "${1:-}" = "--include-ignored" ]; then
    INCLUDE_IGNORED=1
fi

echo "=== building workspace ==="
cargo build --workspace

FAILED=0

run_suite() {
    local name="$1"
    shift
    echo ""
    echo "=== running $name ==="
    if ! "$@"; then
        echo "!!! $name FAILED"
        FAILED=1
    fi
}

run_suite "redfog-moonlight connection_integration" \
    cargo test -p redfog-moonlight --test connection_integration -- --test-threads=1

if [ "$INCLUDE_IGNORED" -eq 1 ]; then
    run_suite "redfog-moonlight connection_integration (--ignored)" \
        cargo test -p redfog-moonlight --test connection_integration -- --test-threads=1 --ignored
fi

echo ""
if [ "$FAILED" -eq 0 ]; then
    echo "=== all suites passed ==="
else
    echo "=== one or more suites FAILED — see output above ==="
fi
exit "$FAILED"
