#!/usr/bin/env bash
# Runs everything that needs real GPU/compositor hardware to actually
# execute -- these can't run in GitHub Actions (no free GPU runner tier,
# even for public repos) or in any other headless/no-GPU environment.
# Currently that's two suites:
#   - kwin-capture's own GPU/NVENC tests (CI only verifies these *build*,
#     see .github/workflows/tests.yml)
#   - connection_integration.rs's tests gated behind the "compositor-tests"
#     cargo feature -- Xwayland genuinely segfaults spawning a headless,
#     no-DRM-device KWin backend (confirmed live via `act`, including
#     --privileged mode -- a real third-party bug, not fixable in redfog's
#     own code); see redfog-moonlight/Cargo.toml's [features] section for
#     why this is a feature and not #[ignore].
#
# Run this manually on a real GPU machine before merging changes that touch
# either. Posts one commit status per suite to GitHub (context
# "gpu-tests/<suite>") for the current HEAD commit, so they show up
# alongside the "Tests" CI check on the commit/PR -- without needing a
# self-hosted Actions runner registered to this machine. Requires `gh` to
# be authenticated (`gh auth status`).
#
# Each suite's full output (including kwin_wayland/dbus-daemon/xdg-desktop-
# portal subprocess noise -- confirmed live to be substantial) goes
# straight to a log file under /tmp/redfog-test-logs/, not the terminal --
# only the pass/fail summary line and that file's path print here. Read the
# log directly if a suite fails or you want to see what actually happened.
#
# Usage:
#   bash scripts/run-gpu-tests.sh [--no-upload]

set -uo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_DIR"

UPLOAD=1
if [ "${1:-}" = "--no-upload" ]; then
    UPLOAD=0
fi

SHA="$(git rev-parse HEAD)"
OVERALL=0

LOG_DIR=/tmp/redfog-test-logs
mkdir -p "$LOG_DIR"
RUN_STAMP="$(date +%Y%m%d-%H%M%S)"

# Runs one suite, prints its result, and (unless --no-upload) posts it as a
# commit status under context "gpu-tests/$1" -- one status per suite, not
# one combined status, so it's clear on GitHub exactly which suite failed.
# Wrapped in `timeout $2` regardless of which suite: confirmed live that a
# single test can hang for hours on a resource-contention issue unrelated
# to the change actually being tested (a stale, since-fixed NVENC bug did
# this once) -- cheap insurance so this script always finishes and reports
# a clear failure/timeout status instead of blocking indefinitely.
run_suite() {
    local suite="$1"
    local context="gpu-tests/$suite"
    local timeout_seconds="$2"
    shift 2
    local log_file="$LOG_DIR/$suite-$RUN_STAMP.log"
    echo ""
    echo "=== running $context (timeout ${timeout_seconds}s) -- log: $log_file ==="
    local state="success"
    if ! timeout "$timeout_seconds" "$@" > "$log_file" 2>&1; then
        state="failure"
    fi

    # Aggregate across *every* "test result:" line, not just the last one --
    # each test binary (plus doctests) prints its own, so a single package
    # run easily produces half a dozen. Confirmed live: taking only the last
    # one previously reported "0 passed; 0 failed" for a run where multiple
    # binaries' real tests had actually passed, just because a trailing,
    # irrelevant summary (an empty doctest run) happened to print last --
    # essentially invisible now that the terminal doesn't also show the raw
    # per-binary lines as a fallback.
    local description
    description="$(awk '
        /^test result:/ {
            for (i = 1; i <= NF; i++) {
                if ($i == "passed;") passed += $(i-1)
                if ($i == "failed;") failed += $(i-1)
                if ($i == "ignored;") ignored += $(i-1)
                if ($i == "measured;") measured += $(i-1)
                if ($i == "out;" && $(i-1) == "filtered") filtered += $(i-2)
            }
            found = 1
        }
        END {
            if (found) print passed " passed; " failed " failed; " ignored " ignored; " measured " measured; " filtered " filtered out"
        }
    ' "$log_file")"
    description="${description:-no test result line found}"
    # GitHub rejects overly long status descriptions -- keep it short.
    description="${description:0:140}"

    echo "=== $context: $state: $description ==="
    if [ "$state" = "failure" ]; then
        echo "--- see $log_file for full output, including subprocess logs ---"
    fi

    if [ "$UPLOAD" -eq 1 ] && command -v gh >/dev/null 2>&1; then
        echo "Uploading commit status for $SHA ($context)..."
        gh api "repos/{owner}/{repo}/statuses/$SHA" \
            -f state="$state" \
            -f context="$context" \
            -f description="$description" \
            >/dev/null
    fi

    [ "$state" = "success" ]
}

echo "=== building workspace ==="
cargo build --workspace --features redfog-moonlight/compositor-tests

run_suite "kwin-capture" 300 cargo test -p kwin-capture -- --test-threads=1 || OVERALL=1
run_suite "connection-integration" 900 \
    cargo test -p redfog-moonlight --test connection_integration --features compositor-tests -- \
    --test-threads=1 --skip gst_wayland_display_backend_smoke_test \
    || OVERALL=1

if [ "$UPLOAD" -eq 0 ]; then
    echo ""
    echo "(--no-upload passed, skipped GitHub commit status uploads)"
elif ! command -v gh >/dev/null 2>&1; then
    echo ""
    echo "gh CLI not found -- skipped GitHub commit status uploads" >&2
fi

exit $OVERALL
