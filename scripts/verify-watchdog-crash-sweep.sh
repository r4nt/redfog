#!/usr/bin/env bash
# Live check for session-lifecycle plan verification step 5: if
# redfog-broker (or redfog-server) dies without a chance to run its own
# graceful SIGTERM handler (simulated here via `kill -9`, bypassing it
# entirely), does redfog-watchdog notice within one poll interval and
# actually reach the orphaned session via `loginctl kill-session`?
#
# Also checks verification step 6: a normal, non-redfog session (yours,
# running this script) must never be touched by the sweep.
#
# Must run as root. Expects at least one real redfog session already
# active (Desktop=redfog, Service=redfog-session in `loginctl list-
# sessions`) -- this WILL end that session, and will restart redfog-broker
# (systemd's Restart=on-failure brings it back on its own).
#
# Usage: sudo scripts/verify-watchdog-crash-sweep.sh
set -uo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "must run as root: sudo $0 $*" >&2
    exit 1
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
watchdog_pid=""
watchdog_log="/tmp/redfog-watchdog-verify-$$.log"

cleanup() {
    if [ -n "$watchdog_pid" ] && kill -0 "$watchdog_pid" 2>/dev/null; then
        kill "$watchdog_pid" 2>/dev/null || true
        wait "$watchdog_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT

redfog_session_ids() {
    loginctl list-sessions --no-legend 2>/dev/null | awk '{print $1}' | while read -r id; do
        desktop="$(loginctl show-session "$id" -p Desktop --value 2>/dev/null || true)"
        service="$(loginctl show-session "$id" -p Service --value 2>/dev/null || true)"
        if [ "$desktop" = "redfog" ] && [ "$service" = "redfog-session" ]; then
            echo "$id"
        fi
    done
}
non_redfog_session_ids() {
    loginctl list-sessions --no-legend 2>/dev/null | awk '{print $1}' | while read -r id; do
        desktop="$(loginctl show-session "$id" -p Desktop --value 2>/dev/null || true)"
        if [ "$desktop" != "redfog" ]; then
            echo "$id"
        fi
    done
}

echo "[1/6] active redfog session(s) before the crash:"
before_redfog="$(redfog_session_ids)"
if [ -z "$before_redfog" ]; then
    echo "FAIL: no active redfog session found (Desktop=redfog/Service=redfog-session) -- log in via a real client first, then rerun."
    exit 1
fi
echo "$before_redfog"
before_non_redfog="$(non_redfog_session_ids)"
echo "  non-redfog sessions (must be untouched throughout): $(echo "$before_non_redfog" | tr '\n' ' ')"

echo "[2/6] building redfog-watchdog..."
if [ -n "${SUDO_USER:-}" ]; then
    sudo -u "$SUDO_USER" -- cargo build -p redfog-watchdog --manifest-path "$repo_root/Cargo.toml"
else
    cargo build -p redfog-watchdog --manifest-path "$repo_root/Cargo.toml"
fi
watchdog_bin="$repo_root/target/debug/redfog-watchdog"

echo "[3/6] starting redfog-watchdog in the background (3s poll interval, log: $watchdog_log)..."
REDFOG_WATCHDOG_POLL_INTERVAL_SECS=3 RUST_LOG=info "$watchdog_bin" >"$watchdog_log" 2>&1 &
watchdog_pid=$!
sleep 1
if ! kill -0 "$watchdog_pid" 2>/dev/null; then
    echo "FAIL: redfog-watchdog exited immediately -- log:"
    cat "$watchdog_log"
    exit 1
fi
echo "  watchdog pid=$watchdog_pid"

echo "[4/6] simulating a real crash: kill -9 redfog-broker's main pid (bypassing its own SIGTERM handler entirely)..."
broker_pid="$(systemctl show redfog-broker -p MainPID --value)"
if [ -z "$broker_pid" ] || [ "$broker_pid" = "0" ]; then
    echo "FAIL: could not determine redfog-broker's MainPID"
    exit 1
fi
echo "  redfog-broker MainPID=$broker_pid"
kill -9 "$broker_pid"

echo "[5/6] waiting up to 15s for the watchdog to notice and sweep the orphaned session(s)..."
deadline=$(( $(date +%s) + 15 ))
swept=0
while [ "$(date +%s)" -lt "$deadline" ]; do
    still_present=""
    for id in $before_redfog; do
        if redfog_session_ids | grep -qx "$id"; then
            still_present="$still_present $id"
        fi
    done
    if [ -z "$still_present" ]; then
        swept=1
        break
    fi
    sleep 1
done

echo "[6/6] results:"
echo "--- watchdog log ---"
cat "$watchdog_log"
echo "--- session state now ---"
loginctl list-sessions --no-legend 2>&1

after_non_redfog="$(non_redfog_session_ids)"
non_redfog_touched=""
for id in $before_non_redfog; do
    if ! echo "$after_non_redfog" | grep -qx "$id"; then
        non_redfog_touched="$non_redfog_touched $id"
    fi
done

echo
if [ "$swept" -eq 1 ] && [ -z "$non_redfog_touched" ]; then
    echo "PASS: watchdog swept the orphaned redfog session(s) after the simulated crash, and left non-redfog session(s) untouched."
    exit 0
else
    [ "$swept" -eq 0 ] && echo "FAIL: redfog session(s)$still_present still present 15s after the crash -- watchdog did not sweep them."
    [ -n "$non_redfog_touched" ] && echo "FAIL: non-redfog session(s)$non_redfog_touched disappeared -- watchdog touched something it shouldn't have."
    exit 1
fi
