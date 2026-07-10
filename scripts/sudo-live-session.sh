#!/usr/bin/env bash
# Run this once via sudo, leave it running, connect a real Moonlight client
# from another machine on the network at this host's IP:
#
#   sudo -E env "PATH=$PATH" bash scripts/sudo-live-session.sh
#
# Starts the REAL redfog-broker + redfog-server on the default Moonlight
# ports, using the REAL redfog-login as the Login stage (type your actual
# account's username/password) and the REAL plasmashell as the User stage
# — the actual KDE desktop, not a test stand-in.
#
# REDFOG_BROKER_PAM_SPAWN: defaults to 1, using the direct fork/PAM/setuid
# session path (crates/redfog-session-init) instead of generating systemd
# units — see design.md's "Privilege separation" section and project
# memory for the comparison between the two. Set to empty/0 in your own
# environment before invoking (sudo -E preserves it) to use the
# systemd-unit path instead.
#
# Requires `cargo build --workspace` (or at least -p redfog-broker
# -p redfog-session-init -p redfog-server -p redfog-login) beforehand.
#
# Ctrl-C stops both processes and cleans up.

set -uo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BROKER_LOG="/tmp/redfog-live-broker.log"
SERVER_LOG="/tmp/redfog-live-server.log"
REDFOG_BROKER_PAM_SPAWN="${REDFOG_BROKER_PAM_SPAWN-1}"

: "${SUDO_USER:?must be run via sudo, not as a raw root login}"

cleanup() {
    echo "stopping..."
    [ -n "${SERVER_PID:-}" ] && kill -TERM "-$SERVER_PID" 2>/dev/null
    [ -n "${BROKER_PID:-}" ] && kill -TERM "-$BROKER_PID" 2>/dev/null
    wait 2>/dev/null
    for unit in /run/systemd/system/redfog-session-*; do
        [ -e "$unit" ] || continue
        name=$(basename "$unit")
        systemctl stop "$name" 2>/dev/null
        rm -f "$unit"
    done
    systemctl daemon-reload 2>/dev/null
    echo "stopped."
}
trap cleanup EXIT INT TERM

rm -rf /tmp/redfog-runtime
rm -f /tmp/redfog-live-broker.sock

echo "starting redfog-broker (PAM_SPAWN=${REDFOG_BROKER_PAM_SPAWN:-<unset, systemd-unit path>}, real PAM auth)..."
REDFOG_BROKER_PAM_SPAWN="$REDFOG_BROKER_PAM_SPAWN" \
RUST_LOG=redfog_broker=debug \
setsid "$REPO_DIR/target/debug/redfog-broker" > "$BROKER_LOG" 2>&1 &
BROKER_PID=$!

deadline=$((SECONDS + 10))
while [ ! -S /tmp/redfog-runtime/broker.sock ]; do
    if [ $SECONDS -ge $deadline ]; then
        echo "redfog-broker never created its socket, see $BROKER_LOG"
        exit 1
    fi
    sleep 0.2
done

echo "starting redfog-server on default ports (47989/47984/48010/...)..."
REDFOG_BROKER_SOCKET=/tmp/redfog-runtime/broker.sock \
REDFOG_LOGIN_APP="$REPO_DIR/target/debug/redfog-login" \
REDFOG_USER_APP="plasmashell --no-respawn" \
RUST_LOG=redfog_moonlight=debug,redfog_server=debug \
setsid "$REPO_DIR/target/debug/redfog-server" > "$SERVER_LOG" 2>&1 &
SERVER_PID=$!

deadline=$((SECONDS + 15))
while ! (exec 3<>/dev/tcp/127.0.0.1/47989) 2>/dev/null; do
    exec 3<&- 2>/dev/null || true
    if [ $SECONDS -ge $deadline ]; then
        echo "redfog-server never came up, see $SERVER_LOG"
        exit 1
    fi
    sleep 0.2
done
exec 3<&- 2>/dev/null || true

ip=$(ip -4 addr show 2>/dev/null | grep -oP '(?<=inet\s)\d+(\.\d+){3}' | grep -v '^127\.' | head -1)
echo ""
echo "=== redfog is up ==="
echo "Point a real Moonlight client at: $ip"
echo "(pairing PIN: watch $SERVER_LOG for the pairing request, or check the client UI)"
echo "broker log: $BROKER_LOG"
echo "server log: $SERVER_LOG"
echo "journal for the User-stage session (systemd-unit path only): journalctl -u 'redfog-session-*' -f"
echo ""
echo "Ctrl-C to stop."

wait
