#!/usr/bin/env bash
# Full, unfiltered environment diff between the redfog session's plasmashell
# and your real desktop's plasmashell — run directly, will prompt for sudo:
#
#   bash scripts/diff-session-env.sh

set -uo pipefail

redfog_pid=$(pgrep -f 'plasmashell --no-respawn' | head -1)
real_pid=$(pgrep -f 'plasmashell --replace' | head -1)

if [ -z "$redfog_pid" ]; then
    echo "no redfog plasmashell (--no-respawn) found — is a session running?"
    exit 1
fi
if [ -z "$real_pid" ]; then
    echo "no real-desktop plasmashell (--replace) found"
    exit 1
fi

echo "redfog pid: $redfog_pid   real desktop pid: $real_pid"

sudo cat "/proc/$redfog_pid/environ" | tr '\0' '\n' | sort > /tmp/redfog_env.txt
sudo cat "/proc/$real_pid/environ" | tr '\0' '\n' | sort > /tmp/real_env.txt

echo ""
echo "=== only in real desktop (missing from redfog) ==="
comm -23 /tmp/real_env.txt /tmp/redfog_env.txt
echo ""
echo "=== only in redfog session (not in real desktop) ==="
comm -13 /tmp/real_env.txt /tmp/redfog_env.txt
