#!/usr/bin/env bash
# Runs the test that documents the known, currently-unfixed post-resume video
# throttling bug (crates/redfog-moonlight/tests/connection_integration.rs,
# video_throttles_after_resume_under_input_driven_damage). It's #[ignore]d
# because it's expected to fail until the underlying KWin issue is fixed —
# see project memory for the investigation history.
#
# This script's own exit code is inverted: it exits 0 when the test FAILS
# (the expected, documented state) and exits 1 if the test unexpectedly
# PASSES (which would mean the bug got fixed — worth investigating rather
# than silently discarding as a good result).
#
# Usage:
#   bash scripts/repro-post-resume-throttling.sh

set -uo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_DIR"

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

cargo test -p redfog-moonlight --test connection_integration -- \
    --test-threads=1 --ignored --nocapture \
    video_throttles_after_resume_under_input_driven_damage
RESULT=$?

echo ""
if [ "$RESULT" -ne 0 ]; then
    echo "=== test failed as expected (bug still present) ==="
    exit 0
else
    echo "=== test PASSED — the throttling bug may be fixed; investigate before trusting this ==="
    exit 1
fi
