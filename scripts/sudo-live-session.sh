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
# One thing the package deliberately doesn't build/wire up at all is an
# opt-in extra here, in both modes, off by default (extra git clone +
# cargo-c build, not worth paying on every quick KWin-only test run):
#   REDFOG_LIVE_SWAY=1          also builds/wires up the gst-wayland-display
#                                Sway backend (MIT, cloned into vendor/) --
#                                without this, only the KDE Plasma/KWin
#                                session-picker entry actually works.
#
#   REDFOG_LIVE_STANDALONE=1    forces standalone (both processes as root) mode
#                                even when redfog-git is installed -- package-
#                                parity mode is chosen purely by whether the
#                                package is *installed* (binaries + unit files
#                                present), not whether its service happens to
#                                be running at invocation time, so stopping
#                                redfog-server.service yourself first has no
#                                effect on which mode this script picks. Not
#                                needed for REDFOG_LIVE_NSYS specifically (see
#                                below) -- kept only as a general escape hatch
#                                for other cases wanting a from-scratch root
#                                run without uninstalling the package.
#
#   REDFOG_LIVE_NSYS=1          launches redfog-server itself under `nsys
#                                launch` (NVIDIA Nsight Systems -- must already
#                                be on PATH, not installed by this script),
#                                ready to be profiled but *not yet collecting
#                                data* -- injection/instrumentation overhead is
#                                paid at launch either way, but actual
#                                recording only happens between separate `nsys
#                                start`/`nsys stop` commands you run yourself
#                                from another terminal once redfog is up:
#
#                                  sudo nsys start --session=redfog-live --sample=process-tree \
#                                    --force-overwrite=true -o /tmp/redfog-live-nsys/redfog-server-$(date +%s)
#                                  ...drive whatever real workload you want to
#                                  capture (log in, get into the real KWin/
#                                  User desktop, interact)...
#                                  sudo nsys stop --session=redfog-live --keep=30
#
#                                `sudo`, not run as yourself: `nsys launch`
#                                always runs as a more privileged identity
#                                than you (root in standalone mode, the
#                                unprivileged-but-different-from-you 'redfog'
#                                system user in package-parity mode) --
#                                confirmed live that targeting a session you
#                                can't reach fails *completely silently* (exit
#                                0, no output, nothing happens), easy to miss
#                                without checking for a report afterward.
#
#                                `--keep=<seconds>` on `stop` discards
#                                everything older than that, so even if you
#                                `start` right away, keeping e.g. 30s at `stop`
#                                time cleanly excludes the Login stage's own
#                                brief encode burst (~3s, always software
#                                x265 -- see VideoEncoder::Software's forced
#                                use for SessionType::Login in session.rs,
#                                unrelated to whatever's actually being
#                                investigated in the real User-stage session)
#                                as long as you've been in the real desktop
#                                longer than that. Report lands wherever `-o`
#                                on `start` pointed; `nsys sessions list` shows
#                                any sessions still open if you lose track.
#
#                                Works entirely within package-parity mode's
#                                normal privilege model (the real, unprivileged
#                                'redfog' system user) -- no root/--run-as=
#                                dance needed at all, unlike an older
#                                REDFOG_LIVE_NSYS design here that wrapped the
#                                whole session in `nsys profile` from launch to
#                                Ctrl-C (confirmed live: nsys, launched via
#                                `sudo`, silently drops its *target* back to
#                                the pre-sudo user by default even though nsys
#                                itself keeps root -- broke every filesystem
#                                operation redfog-server's startup does, fixed
#                                at the time with `--run-as=root`, now moot
#                                since `nsys launch` here is invoked directly
#                                by systemd's `User=redfog`, never through
#                                `sudo` at all). Works by overriding
#                                redfog-server.service's ExecStart= via the
#                                same ephemeral drop-in the debug env vars
#                                already use (torn down on exit, same as
#                                those). CPU-sampling backtraces inside the
#                                report may still come back empty depending on
#                                this machine's kernel.perf_event_paranoid --
#                                that's about the unprivileged 'redfog' user
#                                specifically needing root/CAP_PERFMON for
#                                sampling when perf_event_paranoid is at its
#                                most restrictive (2), separate from the
#                                dropped-privilege issue above.
#
#                                Trace scope is explicitly `cuda,vulkan,osrt,
#                                nvtx` -- NOT nsys's own default
#                                (`cuda,vulkan,osrt,opengl`, no `vulkan`,
#                                useless `opengl` for a pipeline that never
#                                touches it). `vulkan` matters here: on
#                                non-Ampere+ GPUs this pipeline's per-frame
#                                detile copy runs through `vulkan_bridge.rs`
#                                (`vkCmdCopyImageToBuffer`), invisible without
#                                it -- confirmed live, a first baseline
#                                capture with nsys's own defaults showed
#                                `cuda_gpu_kern_sum: SKIPPED (does not contain
#                                CUDA kernel data)`, i.e. saw none of the real
#                                per-frame GPU work at all. NVENC's own encode
#                                calls are a separate vendor API (NvEncodeAPI,
#                                not CUDA) that none of nsys's trace
#                                categories cover -- only the CUDA-driver/
#                                Vulkan work surrounding it is expected to
#                                show up, not the encode itself.
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
    # what packaging/arch/PKGBUILD's own build() builds. redfog-session-init
    # is the privilege-drop helper spawn_via_systemd execs into — see its
    # own doc comment in redfog-broker/src/session.rs.
    build_pkgs=(-p redfog-server -p redfog-broker -p redfog-login -p redfog-pair -p redfog-session-init)
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
        --setenv="REDFOG_LIVE_STANDALONE=${REDFOG_LIVE_STANDALONE:-}" \
        --setenv="REDFOG_LIVE_NSYS=${REDFOG_LIVE_NSYS:-}" \
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

# Shared between both modes below -- see REDFOG_LIVE_NSYS's header comment.
# `nsys_session` (vs "") below marks that redfog-server was launched under
# `nsys launch`, ready for you to `nsys start`/`nsys stop` yourself once
# it's up -- `nsys_start_cmd`/`nsys_stop_cmd` are printed verbatim once the
# server's ready (below), computed here so both modes share one spot.
nsys_session=""
nsys_start_cmd=""
nsys_stop_cmd=""
if [ -n "${REDFOG_LIVE_NSYS:-}" ]; then
    if ! command -v nsys >/dev/null 2>&1; then
        echo "error: REDFOG_LIVE_NSYS is set but 'nsys' (NVIDIA Nsight Systems) isn't on PATH." >&2
        exit 1
    fi
    nsys_dir="/tmp/redfog-live-nsys"
    mkdir -p "$nsys_dir"
    nsys_session="redfog-live"
    nsys_out="$nsys_dir/redfog-server-$(date +%s)"
    # sudo, not run as yourself: `nsys launch` here always runs as a more
    # privileged identity than you (root in standalone mode, the
    # unprivileged-but-*different*-from-you 'redfog' system user in
    # package-parity mode) -- confirmed live that `nsys start`/`nsys stop`
    # targeting a session they can't reach fails *completely silently*
    # (exit 0, no output at all, nothing happens) rather than erroring, so
    # this is easy to not notice at all without checking for a report
    # afterward. `nsys`'s own session-coordination state under
    # /tmp/nvidia/nsight_systems/ is deliberately world-writable (built for
    # cross-user coordination), so this isn't a raw filesystem permission
    # wall -- more likely an ownership check inside nsys's own session
    # protocol, which root should be able to cross regardless.
    # --sample=process-tree: NOT on by default here the way it is for
    # `nsys profile`/`nsys launch` -- confirmed via `nsys start --help`,
    # `--sample=`'s "defaults to process-tree" rule is conditioned on *this
    # command* launching the target, which `start` never does (that already
    # happened via `nsys launch`); its own default is `none`. Without this,
    # confirmed live: a real capture came back with zero CPU IP-sampling
    # data at all (no COMPOSITE_EVENTS/SAMPLING_CALLCHAINS tables), even
    # though everything else (API/OS-runtime tracing) worked fine.
    nsys_start_cmd="sudo nsys start --session=${nsys_session} --sample=process-tree --force-overwrite=true -o ${nsys_out}"
    nsys_stop_cmd="sudo nsys stop --session=${nsys_session} --keep=30"
fi

# ── Package-parity mode ──────────────────────────────────────────────────
if [ -z "${REDFOG_LIVE_STANDALONE:-}" ] \
    && [ -x /usr/bin/redfog-server ] && [ -x /usr/bin/redfog-broker ] \
    && [ -e /usr/lib/systemd/system/redfog-server.service ] \
    && [ -e /usr/lib/systemd/system/redfog-broker.service ]; then
    echo "redfog-git package detected — testing via the real installed systemd units (package-parity mode)."

    echo "installing freshly-built binaries over the package's own (in place; the currently-running process, if any, keeps its own already-open copy until restarted below)..."
    install -Dm755 "$REPO_DIR/target/release/redfog-server" /usr/bin/redfog-server
    install -Dm755 "$REPO_DIR/target/release/redfog-broker" /usr/bin/redfog-broker
    install -Dm755 "$REPO_DIR/target/release/redfog-login" /usr/bin/redfog-login
    install -Dm755 "$REPO_DIR/target/release/redfog-pair" /usr/bin/redfog-pair
    install -Dm755 "$REPO_DIR/target/release/redfog-session-init" /usr/bin/redfog-session-init

    # Ephemeral (/run, not /etc) drop-ins so the debug env vars below never
    # touch the package's own committed units or /etc/redfog/redfog.conf --
    # removed again in cleanup() below, restoring exactly what `makepkg -si`
    # installed. A distinct filename (not systemctl edit's default
    # "override.conf") so this never collides with/clobbers any override
    # you may have added yourself for a separate one-off debugging session.
    broker_env=("RUST_LOG=${REDFOG_LIVE_BROKER_RUST_LOG:-redfog_broker=info}")
    server_env=("RUST_LOG=${REDFOG_LIVE_SERVER_RUST_LOG:-redfog_moonlight=info,redfog_server=info,gst_backend=info}")
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

    if [ -n "$nsys_session" ]; then
        # redfog-server.service runs as the unprivileged 'redfog' system
        # user (User=/Group=redfog) -- it needs write access to the report
        # directory itself, not just this root setup phase.
        chown redfog:redfog "$nsys_dir"
        echo ""
        echo "redfog-server launched under nsys (session '$nsys_session'), not yet collecting."
        echo "Once redfog is up below, start/stop profiling yourself with:"
        echo "  $nsys_start_cmd"
        echo "  ...drive whatever real workload you want to capture..."
        echo "  $nsys_stop_cmd"
        echo ""
        # Empty `ExecStart=` clears the unit's own directive first (systemd
        # drop-in convention for replacing, not appending to, a single-value
        # key) before setting the wrapped one -- appended to the same file
        # write_dropin already wrote (still under its one open [Service]
        # section), so the existing cleanup() below removing that file
        # tears this override down too, nothing extra to add there.
        #
        # `nsys launch`, not `nsys profile`: report finalization now happens
        # via an explicit `nsys stop` you run yourself (see header comment),
        # completely decoupled from this process's own lifecycle -- unlike
        # the old `nsys profile`-wraps-everything design, there's no signal
        # timing to get right here any more, so this needs none of that
        # design's KillSignal=/KillMode= overrides.
        {
            echo "ExecStart="
            echo "ExecStart=/usr/bin/nsys launch --session-new=${nsys_session} --trace=cuda,vulkan,osrt,nvtx -- /usr/bin/redfog-server"
        } >> "/run/systemd/system/redfog-server.service.d/zz-redfog-live-session.conf"
    fi

    was_active_broker=$(systemctl is-active redfog-broker.service 2>/dev/null || true)
    was_active_server=$(systemctl is-active redfog-server.service 2>/dev/null || true)

    cleanup() {
        echo "stopping..."
        # Kills the backgrounded `journalctl -f` from below, if this is
        # being called while it's still running (the normal Ctrl-C case) --
        # otherwise it'd survive as an orphan once this script exits.
        jobs -p | xargs -r kill 2>/dev/null
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
    # Backgrounded + `wait`ed explicitly, not run directly in the
    # foreground: confirmed live, bash does not act on a pending trapped
    # signal (Ctrl-C/SIGTERM) while blocked waiting on a foreground command
    # — only once that command itself exits. `journalctl -f` never gets the
    # signal on its own (only this script's own PID does), so it runs
    # forever and the trap-based cleanup() below never fires, leaking
    # redfog-broker.service/redfog-server.service (and any active session
    # unit) indefinitely. `wait "$!"` (the builtin), unlike waiting on a
    # foreground pipeline directly, *is* documented to return immediately
    # once a trapped signal arrives, letting cleanup() run right away.
    journalctl -u redfog-broker -u redfog-server -f --since "-1s" &
    wait "$!"

# ── Standalone fallback (no package installed) ───────────────────────────
else
    if [ -n "${REDFOG_LIVE_STANDALONE:-}" ]; then
        echo "REDFOG_LIVE_STANDALONE=1 — forcing standalone root-run mode."
    else
        echo "redfog-git not installed — falling back to standalone root-run mode."
    fi
    echo "(doesn't exercise the packaged privilege-separation/systemd-unit model; run 'cd packaging/arch && makepkg -si' first for full parity.)"

    BROKER_LOG="/tmp/redfog-live-broker.log"
    SERVER_LOG="/tmp/redfog-live-server.log"

    cleanup() {
        echo "stopping..."
        # Plain SIGTERM to the whole group, nsys included, same as any other
        # case -- see REDFOG_LIVE_NSYS's header comment: `nsys launch`'s
        # report finalization now happens via an explicit `nsys stop` you run
        # yourself, decoupled from this process's own lifecycle, so (unlike
        # the old `nsys profile`-wraps-everything design) there's no signal
        # timing to get right here any more. Run `nsys stop --session=...`
        # yourself *before* Ctrl-C'ing this script if you haven't already --
        # once this process is gone, there's nothing left to stop collecting.
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
    # Pre-created at the final 0700 mode redfog-server's own
    # HeadlessRuntime::start wants, not left for it to chmod itself --
    # confirmed live that chmod-ing this dir can fail (EPERM) for a root
    # process running under `nsys profile` specifically (it restricts a
    # traced target's DAC-override authority even while still root), so
    # `HeadlessRuntime::start` now skips its own chmod call entirely
    # whenever the mode's already right. Only matters when REDFOG_LIVE_NSYS
    # is set, but harmless either way.
    mkdir -m 0700 /tmp/redfog-runtime
    rm -f /tmp/redfog-live-broker.sock

    # Persistent (never rm -rf'd, unlike /tmp/redfog-runtime above) --
    # TLS identity + the paired-client list live here (tls.rs's
    # default_state_dir(), via REDFOG_STATE_DIR below). Without this,
    # default_state_dir() falls back to the *runtime* dir instead (there's
    # no $STATE_DIRECTORY here the way a real systemd unit would set one),
    # which this script wipes at the top of every single run -- confirmed
    # live, that forced re-pairing a real Moonlight client from scratch on
    # every standalone-mode invocation. Still under /tmp (won't survive a
    # reboot, unlike package-parity mode's real StateDirectory=redfog-server
    # -> /var/lib/redfog-server), but that's an acceptable, much rarer cost
    # compared to every single script re-run.
    mkdir -m 0700 -p /tmp/redfog-live-state

    echo "starting redfog-broker..."
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
    nsys_cmd=()
    if [ -n "$nsys_session" ]; then
        echo ""
        echo "redfog-server launched under nsys (session '$nsys_session'), not yet collecting."
        echo "Once redfog is up below, start/stop profiling yourself with:"
        echo "  $nsys_start_cmd"
        echo "  ...drive whatever real workload you want to capture..."
        echo "  $nsys_stop_cmd"
        echo ""
        # --run-as=root: confirmed live that nsys, launched under `sudo`,
        # silently drops its *target* application back to the pre-sudo
        # invoking user by default (SUDO_UID-based) even though nsys itself
        # keeps root -- broke every filesystem operation redfog-server's
        # own startup does against directories redfog-broker (not
        # nsys-wrapped, stays real root) already created as root. Still
        # needed here even with `nsys launch` (unlike package-parity mode
        # below, launched directly by systemd's `User=redfog`, never through
        # `sudo`, so this drop-to-original-user behavior never triggers
        # there): this whole standalone branch is still reached via `sudo`.
        nsys_cmd=(nsys launch --session-new="$nsys_session" --trace=cuda,vulkan,osrt,nvtx --run-as=root --)
    fi
    REDFOG_BROKER_SOCKET=/tmp/redfog-runtime/broker.sock \
    REDFOG_STATE_DIR=/tmp/redfog-live-state \
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
    setsid "${nsys_cmd[@]}" "$REPO_DIR/target/release/redfog-server" > "$SERVER_LOG" 2>&1 &
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
