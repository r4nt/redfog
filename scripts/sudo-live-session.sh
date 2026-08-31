#!/usr/bin/env bash
# Run this as your normal user (NOT via sudo directly — see below), leave
# it running, connect a real Moonlight client from another machine on the
# network at this host's IP:
#
#   bash scripts/sudo-live-session.sh
#
# Package-parity mode (default whenever redfog-git is installed --
# packaging/arch/PKGBUILD, `makepkg -si`): builds the same binaries the
# package ships, installs them straight over the package's own
# /usr/bin/redfog-{server,broker,login,pair}, then starts the REAL
# redfog-broker.service/redfog-server.service systemd units -- the exact
# same privilege-separated setup (unprivileged "redfog" system user for
# redfog-server, root broker, shared /run/redfog-server runtime dir) a real
# install runs. This exists specifically because that setup has its own
# real permission/environment bugs that a from-scratch root-run test never
# exercises at all -- three separate ones were only ever found by testing
# the actual package, never this script, before this rewrite (a
# REDFOG_RUNTIME_DIR mismatch between the two services, and a missing ACL
# grant for redfog-server's own dedicated system user). Any *.conf drop-in
# this script writes under /run/systemd/system/*.d/ to inject debug env
# vars is removed again on exit -- /etc/redfog/redfog.conf and the units
# themselves are never touched, so this never drifts from what makepkg -si
# would produce on its own.
#
# Falls back to a standalone, from-scratch root-run mode (today's original
# behavior, pre-package-parity) if redfog-git isn't installed at all --
# still useful for quick iteration before you've ever run `makepkg -si`,
# but doesn't exercise the packaged privilege-separation/systemd-unit model
# above, so prefer installing the package first if you're chasing anything
# permission- or environment-related.
#
# Two things the package deliberately doesn't build/wire up at all are
# opt-in extras here, in both modes, off by default (each has its own real
# cost -- an extra git clone + cargo-c build for Sway, real PAM session
# establishment for PAM_SPAWN -- not worth paying on every quick KWin-only
# test run):
#   REDFOG_LIVE_SWAY=1          also builds/wires up the gst-wayland-display
#                                Sway backend (MIT, cloned into vendor/) --
#                                without this, only the KDE Plasma/KWin
#                                session-picker entry actually works.
#   REDFOG_BROKER_PAM_SPAWN=1   experimental direct fork/PAM/setuid session
#                                path (crates/redfog-session-init) instead
#                                of the systemd-run path the package always
#                                uses -- see design.md's "Privilege
#                                separation" section and project memory.
#
# Ctrl-C stops both processes (and, in package mode, reverts the units to
# exactly what the package itself installed) and cleans up.

set -uo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR_DIR="$REPO_DIR/vendor/gst-wayland-display"
PLUGIN_DIR="$VENDOR_DIR/install/lib/gstreamer-1.0"
SELF="$REPO_DIR/scripts/sudo-live-session.sh"

if [ "$(id -u)" -ne 0 ]; then
    # ---- Setup phase: must run as your normal user, not root (see the
    # header comment for why). ----
    if [ -n "${SUDO_USER:-}" ]; then
        echo "error: run this directly as yourself, not via 'sudo bash ...' — it escalates itself for only the part that needs root." >&2
        exit 1
    fi

    if [ -n "${REDFOG_LIVE_SWAY:-}" ]; then
        if [ ! -d "$VENDOR_DIR" ]; then
            echo "cloning gst-wayland-display (MIT) into $VENDOR_DIR..."
            mkdir -p "$REPO_DIR/vendor"
            git clone --depth 1 https://github.com/games-on-whales/gst-wayland-display.git "$VENDOR_DIR"
        fi
        if ! cargo cinstall --version >/dev/null 2>&1; then
            echo "installing cargo-c (one-time build tool for gst-wayland-display)..."
            cargo install cargo-c
        fi
        if [ ! -e "$PLUGIN_DIR/libgstwaylanddisplaysrc.so" ]; then
            echo "building gst-wayland-display plugin (cargo cinstall)..."
            (cd "$VENDOR_DIR" && cargo cinstall --prefix="$VENDOR_DIR/install")
        fi
    fi

    # --release, not a plain debug build: confirmed live, redfog-login's
    # own rendering is ~800ms/frame in debug vs ~2ms/frame in release —
    # slow enough in debug to starve input responsiveness badly enough to
    # feel completely broken (input events queue up behind frame writes).
    #
    # Only the binaries actually needed, not `--workspace`: matches exactly
    # what packaging/arch/PKGBUILD's own build() builds, plus
    # redfog-session-init when REDFOG_BROKER_PAM_SPAWN is opted into (the
    # package never builds that one at all -- see session_init_path's doc
    # comment in redfog-broker for why it still works unpackaged, via
    # REDFOG_SESSION_INIT_PATH below).
    build_pkgs=(-p redfog-server -p redfog-broker -p redfog-login -p redfog-pair)
    if [ -n "${REDFOG_BROKER_PAM_SPAWN:-}" ]; then
        build_pkgs+=(-p redfog-session-init)
    fi
    echo "building redfog (release): ${build_pkgs[*]}"
    (cd "$REPO_DIR" && cargo build --release "${build_pkgs[@]}")

    echo "re-executing as root for the broker/server run phase (sudo may prompt for your password)..."
    exec sudo -E env "PATH=$PATH" "$SELF" "$@"
fi

# ---- Run phase: root from here on (via the re-exec above, or a direct
# `sudo -E ... bash sudo-live-session.sh` invocation for anyone who knows
# what they're doing and wants to skip the build check). ----
: "${SUDO_USER:?must be run via sudo, not as a raw root login}"

# Move into our own systemd scope before starting anything, isolated from
# whatever cgroup this shell happens to have inherited (typically your
# desktop session's own — e.g. chrome-remote-desktop@<user>.service, if
# you're running this from a terminal inside a CRD-connected desktop, the
# common case this script was written for). Confirmed live: redfog-server
# leaked memory until the kernel OOM-killed it, and because it was still
# sitting inside that same cgroup, the kill took the whole CRD session down
# with it, not just redfog-server — see project memory for the incident.
# `systemd-run --scope` always talks to PID1's manager to create a brand
# new, top-level-slice-scoped unit regardless of the caller's own cgroup,
# so this genuinely decouples the two rather than just hoping `setsid`'s
# new session/process-group also implied a new cgroup (it doesn't).
# `MemoryMax` is a real backstop, not just cosmetic isolation: a future leak
# now gets OOM-killed within this scope specifically, long before it could
# threaten the rest of a 31G machine that also runs your desktop session.
#
# Package mode doesn't actually run redfog-server/redfog-broker as
# descendants of this scope at all (systemctl start hands them to their own
# real units instead) -- kept anyway so the standalone fallback below still
# gets the same isolation it always has, and so this phase's own `install`/
# `systemctl`/build-artifact-copy work isn't affected either way.
if [ -z "${REDFOG_LIVE_SCOPED:-}" ]; then
    echo "moving into a dedicated systemd scope (redfog-live.slice), isolated from this shell's own cgroup..."
    exec systemd-run --scope --unit="redfog-live-$$" --slice=redfog-live.slice --collect \
        --property="MemoryMax=10G" \
        --setenv="REDFOG_LIVE_SCOPED=1" \
        --setenv="SUDO_USER=$SUDO_USER" \
        --setenv="PATH=$PATH" \
        --setenv="REDFOG_LIVE_SWAY=${REDFOG_LIVE_SWAY:-}" \
        --setenv="REDFOG_BROKER_PAM_SPAWN=${REDFOG_BROKER_PAM_SPAWN:-}" \
        --setenv="GST_TRACERS=${GST_TRACERS:-}" \
        --setenv="GST_DEBUG=${GST_DEBUG:-}" \
        --setenv="REDFOG_VIDEO_ENCODER=${REDFOG_VIDEO_ENCODER:-}" \
        --setenv="REDFOG_DEBUG_GST_DEBUG=${REDFOG_DEBUG_GST_DEBUG:-}" \
        --setenv="REDFOG_DEBUG_KWIN_LOGGING_RULES=${REDFOG_DEBUG_KWIN_LOGGING_RULES:-}" \
        --setenv="REDFOG_LIVE_BROKER_RUST_LOG=${REDFOG_LIVE_BROKER_RUST_LOG:-}" \
        --setenv="REDFOG_LIVE_SERVER_RUST_LOG=${REDFOG_LIVE_SERVER_RUST_LOG:-}" \
        --setenv="REDFOG_LIVE_TLS_KEYLOG=${REDFOG_LIVE_TLS_KEYLOG:-}" \
        --setenv="REDFOG_LOG_MOUSE_EVENTS=${REDFOG_LOG_MOUSE_EVENTS:-}" \
        -- "$SELF" "$@"
fi

if [ -n "${REDFOG_LIVE_SWAY:-}" ] && [ ! -e "$PLUGIN_DIR/libgstwaylanddisplaysrc.so" ]; then
    echo "warning: gst-wayland-display plugin not found at $PLUGIN_DIR — the Sway session option will fail if picked." >&2
    echo "         (this shouldn't happen via the normal 'bash scripts/sudo-live-session.sh' invocation; run that instead of sudo directly.)" >&2
fi

ip=$(ip -4 addr show 2>/dev/null | grep -oP '(?<=inet\s)\d+(\.\d+){3}' | grep -v '^127\.' | head -1)

# ── Package-parity mode ──────────────────────────────────────────────────
if [ -x /usr/bin/redfog-server ] && [ -x /usr/bin/redfog-broker ] \
    && [ -e /usr/lib/systemd/system/redfog-server.service ] \
    && [ -e /usr/lib/systemd/system/redfog-broker.service ]; then
    echo "redfog-git package detected — testing via the real installed systemd units (package-parity mode)."

    echo "installing freshly-built binaries over the package's own (in place; the currently-running process, if any, keeps its own already-open copy until restarted below)..."
    install -Dm755 "$REPO_DIR/target/release/redfog-server" /usr/bin/redfog-server
    install -Dm755 "$REPO_DIR/target/release/redfog-broker" /usr/bin/redfog-broker
    install -Dm755 "$REPO_DIR/target/release/redfog-login" /usr/bin/redfog-login
    install -Dm755 "$REPO_DIR/target/release/redfog-pair" /usr/bin/redfog-pair
    if [ -n "${REDFOG_BROKER_PAM_SPAWN:-}" ]; then
        install -Dm755 "$REPO_DIR/target/release/redfog-session-init" /usr/bin/redfog-session-init
    fi

    # Ephemeral (/run, not /etc) drop-ins so the debug env vars below never
    # touch the package's own committed units or /etc/redfog/redfog.conf --
    # removed again in cleanup() below, restoring exactly what `makepkg -si`
    # installed. A distinct filename (not systemctl edit's default
    # "override.conf") so this never collides with/clobbers any override
    # you may have added yourself for a separate one-off debugging session.
    broker_env=("RUST_LOG=${REDFOG_LIVE_BROKER_RUST_LOG:-redfog_broker=info}")
    server_env=("RUST_LOG=${REDFOG_LIVE_SERVER_RUST_LOG:-redfog_moonlight=info,redfog_server=info,gst_backend=info}")
    [ -n "${REDFOG_BROKER_PAM_SPAWN:-}" ] && broker_env+=(REDFOG_BROKER_PAM_SPAWN=1 REDFOG_SESSION_INIT_PATH=/usr/bin/redfog-session-init)
    [ -n "${REDFOG_DEBUG_KWIN_LOGGING_RULES:-}" ] && broker_env+=("REDFOG_DEBUG_KWIN_LOGGING_RULES=${REDFOG_DEBUG_KWIN_LOGGING_RULES}")
    [ -n "${REDFOG_LIVE_SWAY:-}" ] && server_env+=("REDFOG_GST_WAYLAND_DISPLAY_PLUGIN_DIR=${PLUGIN_DIR}")
    [ -n "${REDFOG_VIDEO_ENCODER:-}" ] && server_env+=("REDFOG_VIDEO_ENCODER=${REDFOG_VIDEO_ENCODER}")
    # Diagnostic toggle for audio.rs's AudioPacketizer -- see its
    # `fec_enabled` field doc comment. Set to isolate whether a client's
    # audio trouble comes from the FEC packets' sequence-number reuse.
    [ -n "${REDFOG_DISABLE_AUDIO_FEC:-}" ] && server_env+=("REDFOG_DISABLE_AUDIO_FEC=${REDFOG_DISABLE_AUDIO_FEC}")
    # Tuning knob for video.rs's VideoPacketizer -- see
    # `configured_fec_percentage`'s doc comment. Set to compare
    # bitrate/CPU overhead at different FEC percentages live (0 disables
    # video FEC entirely, for a zero-overhead baseline).
    [ -n "${REDFOG_VIDEO_FEC_PERCENTAGE:-}" ] && server_env+=("REDFOG_VIDEO_FEC_PERCENTAGE=${REDFOG_VIDEO_FEC_PERCENTAGE}")
    [ -n "${REDFOG_DEBUG_GST_DEBUG:-}" ] && server_env+=("REDFOG_DEBUG_GST_DEBUG=${REDFOG_DEBUG_GST_DEBUG}")
    [ -n "${GST_TRACERS:-}" ] && server_env+=("GST_TRACERS=${GST_TRACERS}")
    [ -n "${GST_DEBUG:-}" ] && server_env+=("GST_DEBUG=${GST_DEBUG}")
    # Opt-in per-mouse-event INFO logging (session.rs's on_input) --
    # unconditional on RUST_LOG's own level, so it's visible even without
    # bumping redfog_moonlight::session to debug (which would also drop a
    # debug! line per keystroke/mouse-move, a lot more volume).
    [ -n "${REDFOG_LOG_MOUSE_EVENTS:-}" ] && server_env+=("REDFOG_LOG_MOUSE_EVENTS=${REDFOG_LOG_MOUSE_EVENTS}")
    # Opt-in TLS session key logging (rustls honors SSLKEYLOGFILE directly)
    # -- lets a packet capture of this session's HTTPS traffic be decrypted
    # afterward (e.g. `tshark -o tls.keylog_file:...`). Never set by
    # default -- see pairing.rs's own doc comment on why this is gated at
    # all. Deliberately a separate REDFOG_LIVE_-prefixed name, translated
    # to the real SSLKEYLOGFILE only here, at the one place that actually
    # invokes redfog-server -- setting the raw SSLKEYLOGFILE for this
    # whole script would also reach the `cargo build` step above (env vars
    # apply to a script's entire process tree), and cargo's own HTTP/TLS
    # stack honors that same variable too: confirmed live, it eagerly
    # opens/creates the file the moment it sees it, as this user, before
    # the root phase even starts -- permanently blocking the actual
    # redfog-server process (running as a different user) from ever
    # writing to that same now-pre-existing file.
    [ -n "${REDFOG_LIVE_TLS_KEYLOG:-}" ] && server_env+=("SSLKEYLOGFILE=${REDFOG_LIVE_TLS_KEYLOG}")

    write_dropin() {
        local dir="/run/systemd/system/$1.d"
        shift
        mkdir -p "$dir"
        { echo "[Service]"; for kv in "$@"; do echo "Environment=$kv"; done; } > "$dir/zz-redfog-live-session.conf"
    }
    write_dropin redfog-broker.service "${broker_env[@]}"
    write_dropin redfog-server.service "${server_env[@]}"

    was_active_broker=$(systemctl is-active redfog-broker.service 2>/dev/null || true)
    was_active_server=$(systemctl is-active redfog-server.service 2>/dev/null || true)

    cleanup() {
        echo "stopping..."
        # Only stop a unit this script itself brought up -- if it was
        # already running as a real, persistent deployment before this
        # script started, leave it running (just without this session's
        # debug env overrides, removed below).
        [ "$was_active_broker" = "active" ] || systemctl stop redfog-broker.service 2>/dev/null
        [ "$was_active_server" = "active" ] || systemctl stop redfog-server.service 2>/dev/null
        rm -f "/run/systemd/system/redfog-broker.service.d/zz-redfog-live-session.conf" \
              "/run/systemd/system/redfog-server.service.d/zz-redfog-live-session.conf"
        for unit in /run/systemd/system/redfog-session-*; do
            [ -e "$unit" ] || continue
            name=$(basename "$unit")
            systemctl stop "$name" 2>/dev/null
            rm -f "$unit"
        done
        systemctl daemon-reload 2>/dev/null
        if [ "$was_active_broker" = "active" ] || [ "$was_active_server" = "active" ]; then
            echo "restarting redfog-broker/redfog-server to drop this session's debug overrides (they were already running before this script started)..."
            [ "$was_active_broker" = "active" ] && systemctl restart redfog-broker.service 2>/dev/null
            [ "$was_active_server" = "active" ] && systemctl restart redfog-server.service 2>/dev/null
        fi
        echo "stopped."
    }
    trap cleanup EXIT INT TERM

    systemctl daemon-reload
    echo "starting redfog-broker + redfog-server (real systemd units)..."
    systemctl restart redfog-broker.service
    systemctl restart redfog-server.service

    deadline=$((SECONDS + 15))
    while ! (exec 3<>/dev/tcp/127.0.0.1/47989) 2>/dev/null; do
        exec 3<&- 2>/dev/null || true
        if [ $SECONDS -ge $deadline ]; then
            echo "redfog-server never came up, see: journalctl -u redfog-server -n 100 --no-pager"
            exit 1
        fi
        sleep 0.2
    done
    exec 3<&- 2>/dev/null || true

    echo ""
    echo "=== redfog is up (package-parity mode: real systemd units, /usr/bin binaries, redfog-server running as its own unprivileged 'redfog' system user) ==="
    echo "Point a real Moonlight client at: $ip"
    echo "(pairing PIN: watch the journal below for the pairing request, or check the client UI)"
    if [ -n "${REDFOG_LIVE_SWAY:-}" ]; then
        echo "Login screen's session picker offers both KDE Plasma (kwin) and Sway (gst-wayland-display) — pick either."
    else
        echo "Login screen's session picker offers KDE Plasma only (set REDFOG_LIVE_SWAY=1 to also wire up the Sway option)."
    fi
    echo ""
    echo "Ctrl-C to stop and revert to exactly what the package itself installed."
    echo ""
    journalctl -u redfog-broker -u redfog-server -f --since "-1s"

# ── Standalone fallback (no package installed) ───────────────────────────
else
    echo "redfog-git not installed — falling back to standalone root-run mode."
    echo "(doesn't exercise the packaged privilege-separation/systemd-unit model; run 'cd packaging/arch && makepkg -si' first for full parity.)"

    BROKER_LOG="/tmp/redfog-live-broker.log"
    SERVER_LOG="/tmp/redfog-live-server.log"
    REDFOG_BROKER_PAM_SPAWN="${REDFOG_BROKER_PAM_SPAWN-}"

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
    REDFOG_DEBUG_KWIN_LOGGING_RULES="${REDFOG_DEBUG_KWIN_LOGGING_RULES-}" \
    RUST_LOG="${REDFOG_LIVE_BROKER_RUST_LOG:-redfog_broker=info}" \
    setsid "$REPO_DIR/target/release/redfog-broker" > "$BROKER_LOG" 2>&1 &
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
    if [ -n "${REDFOG_VIDEO_ENCODER:-}" ]; then
        echo "REDFOG_VIDEO_ENCODER=$REDFOG_VIDEO_ENCODER (forced, overriding auto-detection)"
    fi
    REDFOG_BROKER_SOCKET=/tmp/redfog-runtime/broker.sock \
    REDFOG_LOGIN_APP="$REPO_DIR/target/release/redfog-login" \
    REDFOG_USER_APP="plasmashell --no-respawn" \
    REDFOG_GST_WAYLAND_DISPLAY_PLUGIN_DIR="${REDFOG_LIVE_SWAY:+$PLUGIN_DIR}" \
    REDFOG_DEBUG_GST_DEBUG="${REDFOG_DEBUG_GST_DEBUG-}" \
    REDFOG_VIDEO_ENCODER="${REDFOG_VIDEO_ENCODER:-}" \
    RUST_LOG="${REDFOG_LIVE_SERVER_RUST_LOG:-redfog_moonlight=info,redfog_server=info,gst_backend=info}" \
    GST_TRACERS="${GST_TRACERS:-}" \
    GST_DEBUG="${GST_DEBUG:-}" \
    SSLKEYLOGFILE="${REDFOG_LIVE_TLS_KEYLOG:-}" \
    REDFOG_LOG_MOUSE_EVENTS="${REDFOG_LOG_MOUSE_EVENTS:-}" \
    setsid "$REPO_DIR/target/release/redfog-server" > "$SERVER_LOG" 2>&1 &
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

    echo ""
    echo "=== redfog is up (standalone mode: both processes run as root) ==="
    echo "Point a real Moonlight client at: $ip"
    echo "(pairing PIN: watch $SERVER_LOG for the pairing request, or check the client UI)"
    if [ -n "${REDFOG_LIVE_SWAY:-}" ]; then
        echo "Login screen's session picker offers both KDE Plasma (kwin) and Sway (gst-wayland-display) — pick either."
    else
        echo "Login screen's session picker offers KDE Plasma only (set REDFOG_LIVE_SWAY=1 to also wire up the Sway option)."
    fi
    echo "broker log: $BROKER_LOG"
    echo "server log: $SERVER_LOG"
    echo "journal for the User-stage session (systemd-unit path only): journalctl -u 'redfog-session-*' -f"
    echo ""
    echo "Ctrl-C to stop."

    wait
fi
