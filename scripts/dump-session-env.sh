#!/usr/bin/env bash
# Dumps the actual, live environment of the running kwin_wayland/plasmashell
# processes (whichever session-spawn path is currently active), filtered to
# the vars that matter for KSycoca app resolution (XDG_DATA_DIRS etc.).
#
# Run directly (no need to wrap in sudo yourself — it shells out to sudo
# per-process, since /proc/<pid>/environ of another uid's process needs it):
#
#   bash scripts/dump-session-env.sh
#
# Run it once against the systemd path and once against the PAM path
# (REDFOG_BROKER_PAM_SPAWN) and diff the two outputs.

set -uo pipefail

VARS_REGEX='^(XDG_[A-Z_]+|HOME|DBUS_SESSION_BUS_ADDRESS|WAYLAND_DISPLAY|DISPLAY|KDE_[A-Z_]+|DESKTOP_SESSION|PATH|USER|LOGNAME)='

dump_pid() {
    local pid="$1" label="$2"
    echo "=== $label (pid $pid) ==="
    if ! sudo test -r "/proc/$pid/environ"; then
        echo "  (could not read /proc/$pid/environ — process may have exited)"
        return
    fi
    sudo cat "/proc/$pid/environ" | tr '\0' '\n' | grep -E "$VARS_REGEX" | sort
    echo ""
}

found_any=0
for pid in $(pgrep -x kwin_wayland); do
    found_any=1
    dump_pid "$pid" "kwin_wayland"
done
for pid in $(pgrep -x plasmashell); do
    found_any=1
    dump_pid "$pid" "plasmashell"
done

if [ "$found_any" -eq 0 ]; then
    echo "no kwin_wayland or plasmashell processes found — is a session running?"
    exit 1
fi
