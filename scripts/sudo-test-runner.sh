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

# connection_integration.rs's ServerProcess/BrokerProcess (and
# redfog-broker's own REDFOG_BROKER_FAKE_SPAWN kwin_wayland child) now set
# PR_SET_PDEATHSIG(SIGKILL) on themselves (see `die_with_parent` there), so
# `cargo test`'s own process dying -- for any reason -- already kills
# redfog-server/redfog-broker directly, without needing a wrapping scope for
# that specific hop. That's not enough on its own here, though, which is why
# this script still wraps each run in its own transient systemd scope
# (system instance -- this script already runs as root, so unlike
# `run-tests.sh`'s old `--user` attempt, this never depended on logind/
# lingering in the first place):
#
#   - REDFOG_BROKER_PAM_SPAWN=1 (this script's own default) and the
#     systemd-unit path (REDFOG_BROKER_PAM_SPAWN=0) both spawn kwin_wayland
#     wrapped in their *own*, separately-named `systemd-run --scope`
#     (redfog-session-*) -- that scope is *not* nested under this one
#     (systemd-run places a new scope under the default slice unless told
#     otherwise, regardless of the caller's own cgroup) and doesn't die just
#     because the broker that created it does; PDEATHSIG can't reach it,
#     only an explicit `systemctl kill` can. The sweep after each run (and
#     in the trap) handles that, independent of whether
#     BrokerProcess::drop's own equivalent (Rust-side) ever got a chance to
#     run.
#   - This script is a long-lived trigger loop, not a one-shot run: if *it*
#     gets interrupted (Ctrl-C/SIGTERM) while a run is still in flight, the
#     trap's explicit `systemctl stop` is what tears down that in-flight
#     run's tree -- PDEATHSIG only fires once a parent process has actually
#     died, and this script staying alive while `cargo test` hangs is
#     exactly the case that doesn't cover.
#
# `timeout 120` below only kills `cargo test`'s own process on a hang; it
# doesn't reach the rest of the tree by itself -- the scope-stop does that.
CURRENT_TEST_UNIT=""
cleanup_current_test_unit() {
    if [ -n "$CURRENT_TEST_UNIT" ]; then
        systemctl stop "$CURRENT_TEST_UNIT" >/dev/null 2>&1 || true
    fi
    for u in $(systemctl list-units 'redfog-session-*' --all --no-legend 2>/dev/null | awk '{print $1}'); do
        systemctl stop "$u" >/dev/null 2>&1 || true
    done
}
trap cleanup_current_test_unit EXIT INT TERM

while true; do
    if [ -f "$TRIGGER_FILE" ]; then
        rm -f "$TRIGGER_FILE" "$DONE_FILE"
        {
            echo "=== $(date -Iseconds): running test ==="
        } > "$LOG_FILE"
        CURRENT_TEST_UNIT="redfog-test-$$-$RANDOM"
        (cd "$REPO_DIR" && REDFOG_DEBUG_PIPEWIRE_LOG=1 REDFOG_BROKER_PAM_SPAWN="$REDFOG_BROKER_PAM_SPAWN" \
            systemd-run --scope --collect --unit="$CURRENT_TEST_UNIT" -- timeout 120 cargo test -p redfog-moonlight --test connection_integration -- --nocapture) >> "$LOG_FILE" 2>&1
        echo "EXIT_CODE: $?" >> "$LOG_FILE"
        cleanup_current_test_unit
        CURRENT_TEST_UNIT=""
        echo "=== $(date -Iseconds): done ===" >> "$LOG_FILE"
        touch "$DONE_FILE"
        chmod 644 "$LOG_FILE" "$DONE_FILE" 2>/dev/null || true
    fi
    sleep 1
done
