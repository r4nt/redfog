#!/usr/bin/env bash
# One-shot live check: does /dev/dri mount-namespace sandboxing set up via
# `systemd-run --scope --property=BindPaths=...`/`TemporaryFileSystem=...`
# survive a `pam_open_session` call inside that same scope (which migrates
# the process into a *different* cgroup, session-N.scope)? This is the
# load-bearing claim behind replacing redfog-broker's persistent .service
# unit with a transient systemd-run scope + real PAM session.
#
# Must run as root. Reuses the same throwaway PAM service file
# verify-pam-systemd-desktop-property.sh creates (recreated here too, so
# this script also works standalone).
#
# Usage: sudo scripts/verify-sandbox-survives-pam.sh [username]
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "must run as root: sudo $0 $*" >&2
    exit 1
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
username="${1:-${SUDO_USER:-manuel}}"
pam_service_file="/etc/pam.d/redfog-desktop-verify"
marker="redfog-verify-marker"
binary="$repo_root/target/debug/examples/pam_sandbox_survival_verify"
unit_name="redfog-sandbox-verify-$$"

cleanup() {
    rm -f "$pam_service_file"
    systemctl reset-failed "${unit_name}.scope" 2>/dev/null || true
}
trap cleanup EXIT

echo "[1/3] building the verify example (as $SUDO_USER, not root)..."
if [ -n "${SUDO_USER:-}" ]; then
    sudo -u "$SUDO_USER" -- cargo build -p redfog-broker --example pam_sandbox_survival_verify --manifest-path "$repo_root/Cargo.toml"
else
    cargo build -p redfog-broker --example pam_sandbox_survival_verify --manifest-path "$repo_root/Cargo.toml"
fi

echo "[2/3] writing temporary PAM service file $pam_service_file..."
cat > "$pam_service_file" <<EOF
auth     required   pam_permit.so
account  required   pam_permit.so
session  optional   pam_systemd.so desktop=$marker
EOF

echo "[3/3] running $binary inside a sandboxed transient SERVICE (not --scope -- scopes don't"
echo "      own the exec step, so BindPaths=/TemporaryFileSystem= don't apply to them at all)..."
systemd-run --wait --collect "--unit=$unit_name" \
    --property=TemporaryFileSystem=/dev/dri:dev \
    --property=BindPaths=/dev/dri/renderD128 \
    -- "$binary" "$username"
echo "-----------------------------------------------------------------"
echo "program output (from the journal, --pty doesn't reliably survive redirected capture):"
journalctl -u "$unit_name.service" --no-pager --output=cat
echo "-----------------------------------------------------------------"
echo
echo "Expected: the BEFORE line lists the full host /dev/dri (renderD128, card1, by-path)."
echo "Expected: the AFTER line (post pam_open_session) lists ONLY renderD128 — if it"
echo "shows the full host list again, the sandbox did NOT survive PAM's cgroup migration."
