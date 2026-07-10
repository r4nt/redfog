#!/usr/bin/env bash
# Traces file lookups the redfog session's plasmashell does for menu/app
# resolution, to see exactly what it's looking for when you click a broken
# taskbar launcher (e.g. konsole). Run directly (will prompt for sudo):
#
#   bash scripts/trace-app-resolution.sh
#
# It attaches strace, waits for you to press Enter, then click the broken
# launcher in the streamed session BEFORE pressing Enter again to stop.

set -uo pipefail

pid=$(pgrep -f 'plasmashell --no-respawn' | head -1)
if [ -z "$pid" ]; then
    echo "no redfog plasmashell (--no-respawn) found — is a session running?"
    exit 1
fi

echo "tracing plasmashell pid $pid"
echo "press Enter to START tracing, then click the broken konsole/steam"
echo "launcher in the streamed session, then press Enter again to STOP."
read -r

sudo strace -f -tt -e trace=openat,open,stat,newfstatat -p "$pid" -o /tmp/plasma-strace.log &
strace_pid=$!

read -r -p "tracing... click the broken launcher now, then press Enter to stop: "

sudo kill "$strace_pid" 2>/dev/null
wait "$strace_pid" 2>/dev/null

echo ""
echo "=== menu/desktop-directories/applications-related lookups ==="
grep -E 'menu|desktop-directories|applications|konsole|steam' /tmp/plasma-strace.log | tail -100
echo ""
echo "full trace saved to /tmp/plasma-strace.log"
