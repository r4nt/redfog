#!/usr/bin/env bash
# Confirms that a narrow bubblewrap sandbox can hide unwanted /dev/dri
# devices (e.g. an iGPU) from KWin's --virtual backend, while leaving
# everything else about the environment untouched -- before wiring this
# into redfog-broker's actual compositor-spawn path.
#
# Background: on a hybrid Intel+NVIDIA machine, KWin's --virtual backend
# (used for every redfog compositor session) has no GPU-selection logic at
# all in any released version -- its findRenderDevice() (src/backends/
# virtual/virtual_backend.cpp) just takes the first DRM device libdrm
# enumerates, with zero vendor/preference filtering. On such a machine this
# can silently pick the iGPU instead of the GPU redfog's own NVENC/CUDA/
# Vulkan-bridge encode path is built around, producing OOM errors and
# garbled video (confirmed live: VulkanBridge::import_persistent's
# vkAllocateMemory failing with ERROR_OUT_OF_DEVICE_MEMORY at higher
# resolutions, garbled pixels at lower ones -- both traced to the wrong
# physical GPU rendering KWin's own scene, not a redfog bug). The upstream
# fix (KWIN_RENDER_NODES env var, via a new GpuManager class) isn't in any
# released KWin version yet (landed 2026-07-09, five days after v6.7.3 was
# tagged) -- see TODO.md.
#
# What this script does: compiles a small C probe that replicates KWin's
# own findRenderDevice() logic exactly (same drmGetDevices2 libdrm call,
# same order, same "first device with an openable render node wins"
# fallback), runs it unsandboxed to find your GPUs, then re-runs it inside
# a `bwrap` sandbox that hides every /dev/dri device except the one to
# keep -- confirming the sandbox actually changes which device gets picked,
# and that nothing else about the environment (rest of /dev, filesystem,
# user identity) changed in the process.
#
# Usage:
#   bash scripts/test-drm-device-sandboxing.sh
#   bash scripts/test-drm-device-sandboxing.sh --keep-vendor Intel   # test hiding NVIDIA instead, for symmetry-checking
#
# Needs `gcc`, libdrm headers (already installed as a redfog build
# dependency), and `bwrap` (bubblewrap) -- installed automatically on
# Arch/CachyOS if missing and running interactively.

set -uo pipefail

KEEP_VENDOR="NVIDIA"
if [ "${1:-}" = "--keep-vendor" ] && [ -n "${2:-}" ]; then
    KEEP_VENDOR="$2"
fi

if [ "$(id -u)" -ne 0 ]; then
    echo "Re-running as root (bwrap's mount-namespace bind operations need CAP_SYS_ADMIN -- this also matches how redfog-broker, which is always root, will actually run this)..."
    exec sudo -E "$0" "$@"
fi

if ! command -v bwrap >/dev/null 2>&1; then
    if command -v pacman >/dev/null 2>&1; then
        echo "bwrap not found -- installing bubblewrap via pacman..."
        pacman -S --noconfirm bubblewrap
    else
        echo "bwrap (bubblewrap) not found and this isn't a pacman system -- install it manually and re-run." >&2
        exit 1
    fi
fi

WORK_DIR="$(mktemp -d /tmp/redfog-drm-sandbox-test.XXXXXX)"
trap 'rm -rf "$WORK_DIR"' EXIT

PROBE_SRC="$WORK_DIR/probe.c"
PROBE_BIN="$WORK_DIR/probe"

# Mirrors KWin's VirtualBackend::findRenderDevice() (src/backends/virtual/
# virtual_backend.cpp, confirmed against the v6.7.3 tag and current 6.7.x
# releases) closely enough to show exactly what it would pick: same
# drmGetDevices2 call, same enumeration order, same "try to open each
# candidate's render node, first success wins" fallback -- so whichever
# device this prints as picked is genuinely what KWin's --virtual backend
# would use too.
cat > "$PROBE_SRC" <<'EOF'
#include <stdio.h>
#include <string.h>
#include <xf86drm.h>

static const char *vendor_name(const char *pci_vendor_hex) {
    if (!pci_vendor_hex) return "unknown";
    if (strcasecmp(pci_vendor_hex, "0x8086") == 0) return "Intel";
    if (strcasecmp(pci_vendor_hex, "0x10de") == 0) return "NVIDIA";
    if (strcasecmp(pci_vendor_hex, "0x1002") == 0) return "AMD";
    return pci_vendor_hex;
}

int main(void) {
    int device_count = drmGetDevices2(0, NULL, 0);
    if (device_count <= 0) {
        fprintf(stderr, "drmGetDevices2 found no devices (or failed): %d\n", device_count);
        return 1;
    }

    drmDevicePtr devices[64];
    if (device_count > 64) device_count = 64;
    int got = drmGetDevices2(0, devices, device_count);
    if (got < 0) {
        fprintf(stderr, "drmGetDevices2 (fetch) failed: %d\n", got);
        return 1;
    }

    printf("drmGetDevices2 enumerated %d device(s), in this order:\n\n", got);

    int picked = 0;
    for (int i = 0; i < got; i++) {
        drmDevicePtr dev = devices[i];

        const char *vendor_hex = NULL;
        char vendor_buf[16] = {0};
        if (dev->bustype == DRM_BUS_PCI && dev->deviceinfo.pci) {
            snprintf(vendor_buf, sizeof(vendor_buf), "0x%04x", dev->deviceinfo.pci->vendor_id);
            vendor_hex = vendor_buf;
        }

        int node_type = DRM_NODE_RENDER;
        if (dev->bustype == DRM_BUS_PLATFORM && dev->businfo.platform &&
            dev->businfo.platform->fullname &&
            strcmp(dev->businfo.platform->fullname, "vgem") == 0) {
            node_type = DRM_NODE_PRIMARY;
        }

        int has_node = dev->available_nodes & (1 << node_type);
        const char *node_path = has_node ? dev->nodes[node_type] : NULL;

        printf("  [%d] vendor=%s bustype=%d %s=%s\n",
               i, vendor_name(vendor_hex), dev->bustype,
               node_type == DRM_NODE_RENDER ? "render_node" : "primary_node",
               node_path ? node_path : "(none available)");

        if (!picked && has_node) {
            printf("      <-- KWIN WOULD PICK THIS ONE (first with a usable node)\n");
            picked = 1;
        }
    }

    if (!picked) {
        printf("\nNo device had a usable node -- KWin's findRenderDevice() would return null here.\n");
    }

    drmFreeDevices(devices, got);
    return 0;
}
EOF

echo "=== compiling probe ==="
if ! gcc -O2 -o "$PROBE_BIN" "$PROBE_SRC" -ldrm -I/usr/include/libdrm; then
    echo "compile failed -- do you have libdrm's headers installed? (they ship with the libdrm package itself on Arch/CachyOS)" >&2
    exit 1
fi

echo
echo "=== unsandboxed: what does this machine actually have? ==="
"$PROBE_BIN"
UNSANDBOXED_OUTPUT="$("$PROBE_BIN")"

TOTAL_DEVICES="$(echo "$UNSANDBOXED_OUTPUT" | grep -c '^  \[')"
if [ "$TOTAL_DEVICES" -lt 2 ]; then
    echo
    echo "Only $TOTAL_DEVICES DRM device found -- nothing to hide, so there's nothing meaningful for this script to test here."
    exit 0
fi

KEEP_NODE="$(echo "$UNSANDBOXED_OUTPUT" | grep "vendor=$KEEP_VENDOR" | grep -oE '/dev/dri/render[A-Za-z0-9]+' | head -1)"
if [ -z "$KEEP_NODE" ]; then
    echo
    echo "No device with vendor=$KEEP_VENDOR (and a usable render node) found -- can't test keeping it visible. Devices found:" >&2
    echo "$UNSANDBOXED_OUTPUT" >&2
    exit 1
fi

# Sibling nodes for the SAME physical GPU as $KEEP_NODE: the primary/KMS
# node (/dev/dri/cardN) and any /dev/dri/by-path/* symlinks pointing at
# either one. KWin's own findRenderDevice() never needs these (confirmed
# against its source -- a headless --virtual backend never does real
# KMS/mode-setting), but nothing rules out some *other* part of the process
# tree wanting them for the same GPU we've already decided to expose -- see
# gpu_sandbox_argv_prefix's doc comment in redfog-broker/src/session.rs,
# which this mirrors.
RENDER_NAME="$(basename "$KEEP_NODE")"
CARD_NAME="$(ls "/sys/class/drm/$RENDER_NAME/device/drm/" 2>/dev/null | grep '^card' | head -1)"
CARD_NODE=""
[ -n "$CARD_NAME" ] && CARD_NODE="/dev/dri/$CARD_NAME"

BY_PATH_ARGS=()
if [ -d /dev/dri/by-path ]; then
    for link in /dev/dri/by-path/*; do
        [ -e "$link" ] || continue
        target="$(readlink "$link")"
        target_name="$(basename "$target")"
        if [ "$target_name" = "$RENDER_NAME" ] || { [ -n "$CARD_NAME" ] && [ "$target_name" = "$CARD_NAME" ]; }; then
            BY_PATH_ARGS+=(--symlink "$target" "$link")
        fi
    done
fi

BWRAP_ARGS=(--bind / / --dev-bind /dev /dev --tmpfs /dev/dri --dev-bind "$KEEP_NODE" "$KEEP_NODE")
[ -n "$CARD_NODE" ] && BWRAP_ARGS+=(--dev-bind "$CARD_NODE" "$CARD_NODE")
if [ "${#BY_PATH_ARGS[@]}" -gt 0 ]; then
    BWRAP_ARGS+=(--dir /dev/dri/by-path)
    BWRAP_ARGS+=("${BY_PATH_ARGS[@]}")
fi

echo
echo "=== sandboxed: hiding every /dev/dri device except $KEEP_NODE${CARD_NODE:+, $CARD_NODE}${BY_PATH_ARGS:+, and ${#BY_PATH_ARGS[@]} by-path symlink(s)} ==="
SANDBOX_OUTPUT="$(bwrap "${BWRAP_ARGS[@]}" -- "$PROBE_BIN")"
echo "$SANDBOX_OUTPUT"

echo
echo "=== sanity checks: did anything *else* change? ==="
bwrap "${BWRAP_ARGS[@]}" -- bash -c '
        echo "--- /dev/dri contents inside sandbox (should show only what was explicitly bound) ---"
        ls -la /dev/dri
        ls -la /dev/dri/by-path 2>/dev/null
        echo "--- /dev/input still populated? ---"
        ls /dev/input 2>&1 | head -5
        echo "--- identity/filesystem still normal? ---"
        whoami
        ls /home >/dev/null 2>&1 && echo "/home readable: yes"
        echo "--- known gotcha: does /tmp/.X11-unix look owned by root or us? (Xwayland refuses it otherwise) ---"
        stat -c "uid=%u gid=%g" /tmp/.X11-unix 2>&1
        echo "    (root-owned but this shows a non-zero, non-\"our uid\" owner? that is the unprivileged-userns identity remap --"
        echo "     confirmed real, breaks Xwayland when this whole bwrap invocation itself runs unprivileged, e.g. under a"
        echo "     systemd unit'\''s User= -- see TODO.md. spawn_via_systemd runs bwrap as real root and does not hit this.)"
    '

echo
echo "=== verdict ==="
PICKED_LINE="$(echo "$SANDBOX_OUTPUT" | grep -B1 'KWIN WOULD PICK' | head -1)"
if echo "$PICKED_LINE" | grep -q "vendor=$KEEP_VENDOR"; then
    echo "PASS: inside the sandbox, KWin's own device-selection logic would pick vendor=$KEEP_VENDOR ($KEEP_NODE)."
    echo "Compare against the unsandboxed run above to confirm this actually changed the outcome."
else
    echo "UNEXPECTED: sandboxed probe didn't pick vendor=$KEEP_VENDOR. Output was:" >&2
    echo "$SANDBOX_OUTPUT" >&2
    exit 1
fi
