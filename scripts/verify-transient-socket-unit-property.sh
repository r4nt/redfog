#!/usr/bin/env bash
# One-shot live check -- kept despite a CONFIRMED NEGATIVE result, as a
# record against re-attempting this exact approach later (same reasoning
# `crates/redfog-broker/src/session.rs`'s own `spawn_via_pam` doc comment
# gives for why it walked this back): does linking a `systemd-run`-started
# transient *service* to a separately-written `.socket` unit via the
# `Sockets=` service property (rather than writing a matching persistent
# `.service` unit file and letting them pair by name) let it receive the
# socket's already-bound fd via LISTEN_FDS?
#
# `systemd-run` (this machine: systemd 261) has no generic way to hand an
# arbitrary broker-owned fd (e.g. a UnixListener the broker already bound
# itself) into a transient *service* it starts -- confirmed via
# `systemd-run --help`, which only offers `--pipe` (stdio, fds 0-2) and
# nothing like `--fd=`. `Sockets=` looked like the natural way around that
# (a real systemd.service(5) directive), but two rounds of live testing
# here found it doesn't work at all on this systemd version:
#   1. `systemctl start <name>.socket` refuses outright unless a
#      name-matched `<name>.service` is already a *loaded* unit ("Socket
#      service X.service not loaded, refusing") -- a transient unit only
#      becomes "loaded" once `systemd-run` actually starts it, which can't
#      happen before the socket itself needs to already be bound.
#   2. Even skipping that (writing the .socket unit but never starting it
#      directly, hoping the service's own start would pull it in as a
#      dependency), `systemd-run --property=Sockets=...` itself fails with
#      "Unknown assignment: Sockets=..." -- it isn't a settable property
#      for a transient unit at all, regardless of ordering.
#
# Conclusion: pre-binding a Wayland socket with controlled permissions
# still requires writing BOTH a `.socket` *and* a name-matched `.service`
# unit file to disk (exactly what the pre-existing, already-working
# `spawn_via_systemd` did) -- there's no way to keep only the `.socket`
# file and defer the paired service to a `systemd-run` invocation.
#
# Must run as root. No PAM involved -- this is purely about the
# socket-activation fd handoff, orthogonal to the PAM checks the other two
# verify-*.sh scripts in this directory already passed.
#
# Usage: sudo scripts/verify-transient-socket-unit-property.sh
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "must run as root: sudo $0 $*" >&2
    exit 1
fi

unit_name="redfog-socket-prop-verify-$$"
socket_unit_path="/run/systemd/system/${unit_name}.socket"
test_path="/tmp/redfog-socket-prop-verify-$$.sock"

cleanup() {
    systemctl stop "${unit_name}.service" 2>/dev/null || true
    systemctl stop "${unit_name}.socket" 2>/dev/null || true
    rm -f "$socket_unit_path"
    systemctl daemon-reload 2>/dev/null || true
    systemctl reset-failed "${unit_name}.service" "${unit_name}.socket" 2>/dev/null || true
    rm -f "$test_path"
}
trap cleanup EXIT

echo "[1/4] writing transient-ish .socket unit at $socket_unit_path (ListenStream=$test_path)..."
cat > "$socket_unit_path" <<EOF
[Socket]
ListenStream=$test_path
SocketMode=0660
EOF
systemctl daemon-reload
# Deliberately NOT `systemctl start`-ing the .socket unit directly here --
# a first attempt at that failed outright ("Socket service X.service not
# loaded, refusing"): starting a .socket unit apparently requires its
# default name-matched .service to already be a *loaded* unit, which a
# transient (systemd-run-created) service never is until the moment it's
# actually started. Testing here whether the Sockets= property on the
# service side instead makes systemd auto-activate this (merely
# daemon-reloaded, not yet started) .socket unit as an ordering dependency
# when the transient service starts.
echo "  (.socket unit written + daemon-reloaded, NOT started directly -- testing whether Sockets= on the service pulls it in)"

echo "[2/4] starting a transient SERVICE (via systemd-run, no unit file written for it) with"
echo "      --property=Sockets=${unit_name}.socket, checking it receives the fd via LISTEN_FDS..."
# The payload just checks $LISTEN_FDS/$LISTEN_FDNAMES and writes what it saw
# to a result file, then exits -- Type=oneshot-ish via --wait.
result_file="/tmp/redfog-socket-prop-verify-$$-result.txt"
rm -f "$result_file"
if ! systemd-run --wait --collect "--unit=${unit_name}" \
    "--property=Sockets=${unit_name}.socket" \
    -- /bin/sh -c "echo \"LISTEN_FDS=\$LISTEN_FDS LISTEN_FDNAMES=\$LISTEN_FDNAMES\" > $result_file"; then
    echo "FAIL: starting the transient service itself failed -- diagnostics:"
    systemctl status "${unit_name}.socket" --no-pager -l || true
    systemctl status "${unit_name}.service" --no-pager -l || true
    echo "--- journalctl -u ${unit_name}.service ---"
    journalctl -u "${unit_name}.service" --no-pager --output=cat || true
    exit 1
fi

echo "[3/4] result:"
cat "$result_file" 2>/dev/null || echo "  NO RESULT FILE -- service likely failed to start at all"

echo "[4/4] verdict:"
if grep -q "LISTEN_FDS=1" "$result_file" 2>/dev/null; then
    echo "PASS: the transient service received exactly one fd via LISTEN_FDS, matching the .socket unit linked via the Sockets= property."
    rm -f "$result_file"
    exit 0
else
    echo "FAIL: expected LISTEN_FDS=1 in $result_file, got:"
    cat "$result_file" 2>/dev/null || echo "  (file missing)"
    rm -f "$result_file"
    exit 1
fi
