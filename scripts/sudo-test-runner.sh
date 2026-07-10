#!/usr/bin/env bash
# Run this once via sudo, leave it running:
#   sudo -E env "PATH=$PATH" bash scripts/sudo-test-runner.sh
#
# Waits for a trigger file to appear, then runs the redfog-moonlight
# connection_integration test (as root, exercising the real cross-user
# broker path) and logs output + exit code to a known location. Loops
# until killed (Ctrl-C).
#
# Trigger a run from another terminal:
#   touch /tmp/redfog-test-trigger
# Then check /tmp/redfog-test-done (appears once the run finishes) and
# /tmp/redfog-test-output.log for the result.
#
# REDFOG_BROKER_PAM_SPAWN: defaults to 1, using the direct fork/PAM/setuid
# session path (crates/redfog-session-init) instead of generating systemd
# units. Set to empty/0 in your own environment before invoking (sudo -E
# preserves it) to test the systemd-unit path instead.

set -uo pipefail

TRIGGER_FILE="/tmp/redfog-test-trigger"
DONE_FILE="/tmp/redfog-test-done"
LOG_FILE="/tmp/redfog-test-output.log"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REDFOG_BROKER_PAM_SPAWN="${REDFOG_BROKER_PAM_SPAWN-1}"

: "${SUDO_USER:?must be run via sudo, not as a raw root login — the broker needs \$SUDO_USER to know which non-root user to target}"

echo "redfog test runner started (SUDO_USER=$SUDO_USER, PAM_SPAWN=${REDFOG_BROKER_PAM_SPAWN:-<unset, systemd-unit path>})"
echo "  waiting for trigger: touch $TRIGGER_FILE"
echo "  output logged to:    $LOG_FILE"
echo "  done marker:         $DONE_FILE (removed when a run starts, created when it finishes)"
echo "  ctrl-C to stop this loop"

rm -f "$TRIGGER_FILE" "$DONE_FILE"

while true; do
    if [ -f "$TRIGGER_FILE" ]; then
        rm -f "$TRIGGER_FILE" "$DONE_FILE"
        {
            echo "=== $(date -Iseconds): running test ==="
        } > "$LOG_FILE"
        (cd "$REPO_DIR" && REDFOG_DEBUG_PIPEWIRE_LOG=1 REDFOG_BROKER_PAM_SPAWN="$REDFOG_BROKER_PAM_SPAWN" timeout 120 cargo test -p redfog-moonlight --test connection_integration -- --nocapture) >> "$LOG_FILE" 2>&1
        echo "EXIT_CODE: $?" >> "$LOG_FILE"
        echo "=== $(date -Iseconds): done ===" >> "$LOG_FILE"
        touch "$DONE_FILE"
        chmod 644 "$LOG_FILE" "$DONE_FILE" 2>/dev/null || true
    fi
    sleep 1
done
