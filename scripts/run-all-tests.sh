#!/usr/bin/env bash
# Runs literally everything this machine is capable of running: the
# sudo-free, no-special-hardware workspace suite (scripts/run-tests.sh),
# plus everything that needs a real NVIDIA GPU/compositor
# (scripts/run-gpu-tests.sh) -- kwin-capture's own tests, and
# connection_integration.rs's tests behind the "compositor-tests" cargo
# feature, specifically because they need that hardware.
#
# Just the two scripts run back to back; see each one's own header comment
# for what it covers and why. Needs a real NVIDIA GPU (this box has one) --
# run scripts/run-tests.sh alone on a machine without one.
#
# Usage:
#   bash scripts/run-all-tests.sh [--no-upload]
#   (--no-upload is forwarded to run-gpu-tests.sh, skipping its GitHub
#   commit-status upload)

set -uo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_DIR"

FAILED=0

echo "########################################"
echo "# 1/2: sudo-free, no-special-hardware suite (scripts/run-tests.sh)"
echo "########################################"
bash scripts/run-tests.sh || FAILED=1

echo ""
echo "########################################"
echo "# 2/2: GPU/compositor-dependent suites (scripts/run-gpu-tests.sh)"
echo "########################################"
bash scripts/run-gpu-tests.sh "${1:-}" || FAILED=1

echo ""
if [ "$FAILED" -eq 0 ]; then
    echo "=== ALL suites (sudo-free + GPU) passed ==="
else
    echo "=== one or more suites FAILED — see output above ==="
fi
exit "$FAILED"
