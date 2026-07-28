#!/usr/bin/env bash
# Runs kwin-capture's GPU-dependent test suite for real, on real NVIDIA
# hardware. These can't run in GitHub Actions -- there's no free GPU runner
# tier, even for public repos (see .github/workflows/tests.yml's own
# comment, which only verifies kwin-capture *builds* there, never that it
# actually works). Run this manually on a real GPU machine before merging
# changes that touch kwin-capture.
#
# Posts the result as a commit status on GitHub (context
# "gpu-tests/kwin-capture") for the current HEAD commit, so it shows up
# alongside the "Tests" CI check on the commit/PR -- without needing a
# self-hosted Actions runner registered to this machine. Requires `gh` to
# be authenticated (`gh auth status`).
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

echo "=== building kwin-capture tests ==="
cargo build -p kwin-capture --tests

echo ""
echo "=== running kwin-capture GPU tests ==="
OUTPUT_FILE="$(mktemp)"
if cargo test -p kwin-capture -- --test-threads=1 2>&1 | tee "$OUTPUT_FILE"; then
    STATE="success"
else
    STATE="failure"
fi

SUMMARY_LINE="$(grep -E '^test result:' "$OUTPUT_FILE" | tail -1)"
DESCRIPTION="${SUMMARY_LINE:-no test result line found}"
# GitHub rejects overly long status descriptions -- keep it short.
DESCRIPTION="${DESCRIPTION:0:140}"
rm -f "$OUTPUT_FILE"

echo ""
echo "=== $STATE: $DESCRIPTION ==="

if [ "$UPLOAD" -eq 0 ]; then
    echo "(--no-upload passed, skipping GitHub commit status)"
    [ "$STATE" = "success" ]
    exit $?
fi

if ! command -v gh >/dev/null 2>&1; then
    echo "gh CLI not found -- skipping GitHub commit status upload" >&2
    [ "$STATE" = "success" ]
    exit $?
fi

SHA="$(git rev-parse HEAD)"
echo "Uploading commit status for $SHA..."
gh api "repos/{owner}/{repo}/statuses/$SHA" \
    -f state="$STATE" \
    -f context="gpu-tests/kwin-capture" \
    -f description="$DESCRIPTION" \
    >/dev/null

echo "Done -- see the commit status on GitHub (context: gpu-tests/kwin-capture)."

[ "$STATE" = "success" ]
