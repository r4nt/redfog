#!/usr/bin/env bash
# Live check for session-lifecycle plan verification step 4: does
# `systemctl restart redfog-server` (with an active session) tear the
# logind session down *immediately* (via SessionManager::shutdown_all_
# sessions's SIGTERM handler), rather than leaving it orphaned until
# redfog-watchdog's next poll interval catches it?
#
# Must run as root. Expects at least one real redfog session already
# active (Desktop=redfog, Service=redfog-session in `loginctl list-
# sessions`) -- this WILL end that session; don't run this while you still
# want to use it.
#
# Usage: sudo scripts/verify-restart-kills-sessions.sh
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "must run as root: sudo $0 $*" >&2
    exit 1
fi

redfog_session_ids() {
    loginctl list-sessions --no-legend 2>/dev/null | awk '{print $1}' | while read -r id; do
        desktop="$(loginctl show-session "$id" -p Desktop --value 2>/dev/null || true)"
        service="$(loginctl show-session "$id" -p Service --value 2>/dev/null || true)"
        if [ "$desktop" = "redfog" ] && [ "$service" = "redfog-session" ]; then
            echo "$id"
        fi
    done
}

echo "[1/3] redfog sessions active before restart:"
before="$(redfog_session_ids)"
if [ -z "$before" ]; then
    echo "FAIL: no active redfog session found (Desktop=redfog/Service=redfog-session) -- log in via a real client first, then rerun."
    exit 1
fi
echo "$before" | while read -r id; do
    echo "  session $id: $(loginctl show-session "$id" -p Leader --value 2>/dev/null)"
done

echo "[2/3] systemctl restart redfog-server..."
restart_start=$(date +%s)
systemctl restart redfog-server
restart_end=$(date +%s)
echo "  restart returned after $((restart_end - restart_start))s"

echo "[3/3] redfog sessions active after restart:"
after="$(redfog_session_ids)"
if [ -z "$after" ]; then
    echo "  (none)"
else
    echo "$after"
fi

still_present=""
for id in $before; do
    if echo "$after" | grep -qx "$id"; then
        still_present="$still_present $id"
    fi
done

if [ -n "$still_present" ]; then
    echo
    echo "FAIL: session(s)$still_present from before the restart are still present in loginctl after it -- shutdown_all_sessions did not (or did not yet) clean them up."
    exit 1
else
    echo
    echo "PASS: every redfog session active before the restart is gone immediately after it."
    exit 0
fi
