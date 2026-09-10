#!/usr/bin/env bash
# One-shot live check: does pam_systemd.so's `desktop=` module argument
# actually propagate to logind's `Desktop=` session property? Confirms the
# mechanism the real /etc/pam.d/redfog-session (see the session-lifecycle
# plan) depends on, before any real code is built around it.
#
# Must run as root (opening a PAM/logind session requires it). Builds and
# runs crates/redfog-broker/examples/pam_desktop_verify.rs, which opens a
# real (temporary) PAM session against a throwaway PAM service file, then
# this script inspects it via loginctl and reports pass/fail.
#
# Usage: sudo scripts/verify-pam-systemd-desktop-property.sh [username]
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "must run as root: sudo $0 $*" >&2
    exit 1
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
username="${1:-${SUDO_USER:-manuel}}"
pam_service_name="redfog-desktop-verify"
pam_service_file="/etc/pam.d/$pam_service_name"
marker="redfog-verify-marker"
binary="$repo_root/target/debug/examples/pam_desktop_verify"

cleanup() {
    if [ -n "${child_pid:-}" ] && kill -0 "$child_pid" 2>/dev/null; then
        kill "$child_pid" 2>/dev/null || true
        wait "$child_pid" 2>/dev/null || true
    fi
    rm -f "$pam_service_file"
}
trap cleanup EXIT

echo "[1/4] building the verify example (as $SUDO_USER, not root, to avoid messing up target/ ownership)..."
if [ -n "${SUDO_USER:-}" ]; then
    sudo -u "$SUDO_USER" -- cargo build -p redfog-broker --example pam_desktop_verify --manifest-path "$repo_root/Cargo.toml"
else
    cargo build -p redfog-broker --example pam_desktop_verify --manifest-path "$repo_root/Cargo.toml"
fi

echo "[2/4] writing temporary PAM service file $pam_service_file..."
cat > "$pam_service_file" <<EOF
auth     required   pam_permit.so
account  required   pam_permit.so
session  optional   pam_systemd.so desktop=$marker
EOF

echo "[3/4] opening a real PAM/logind session for user '$username'..."
"$binary" "$username" &
child_pid=$!
sleep 2

session_id="$(loginctl list-sessions --no-legend 2>/dev/null | awk -v pid="$child_pid" '$5 == pid {print $1}')"
if [ -z "$session_id" ]; then
    echo "FAIL: no logind session found with Leader PID $child_pid"
    echo "loginctl list-sessions output was:"
    loginctl list-sessions --no-legend || true
    exit 1
fi

echo "[4/4] session $session_id found (Leader=$child_pid), checking properties..."
desktop="$(loginctl show-session "$session_id" -p Desktop --value 2>/dev/null || echo "<error>")"
service="$(loginctl show-session "$session_id" -p Service --value 2>/dev/null || echo "<error>")"
type_="$(loginctl show-session "$session_id" -p Type --value 2>/dev/null || echo "<error>")"

echo "  Desktop=$desktop"
echo "  Service=$service"
echo "  Type=$type_"

if [ "$desktop" = "$marker" ]; then
    echo
    echo "PASS: pam_systemd.so's desktop= argument propagated correctly to Desktop=$marker"
    exit 0
else
    echo
    echo "FAIL: expected Desktop=$marker, got Desktop=$desktop"
    exit 1
fi
