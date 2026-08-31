#!/usr/bin/env bash
# Standalone reproduction of KWin's ScreenCastStream::testCreateDmaBuf
# (src/plugins/screencast/screencaststream.cpp) -- the exact function that
# decides whether a redfog session gets real, zero-copy DMA-BUF screencast
# capture or falls back to a `MemPtr`/software-encode path. Runs entirely
# independent of KWin and redfog: no compositor, no session, just the GPU
# driver doing the same allocate -> import-as-texture -> framebuffer-complete
# round trip KWin's own code does, with verbose per-step logging (KWin's own
# version is silent on every failure branch, which is why this exists at
# all -- there was no way to tell which of five possible failure points was
# actually failing without either this or a live debugger attached to
# kwin_wayland).
#
# Background: on a GTX 1070 (agnesi), KWin's real screencast negotiation
# always falls back to `dmabuf: false`/`MemPtr`, forcing software H.265
# encoding instead of NVENC -- confirmed NOT caused by GPU selection, the
# bwrap sandbox, or anything in redfog's own code. Using this tool, found
# live on a working RTX 2080 machine: NVIDIA's GBM backend cannot allocate
# a *renderable* buffer (GBM_BO_USE_RENDERING and friends) with the LINEAR
# modifier at all -- a general NVIDIA/GBM limitation, not GPU-generation-
# specific. It doesn't matter on the RTX 2080 because that GPU's driver
# also advertises a real, working tiled modifier as a fallback, which KWin
# successfully uses instead. On the GTX 1070, redfog's own EGL modifier
# query (see kwin-capture/src/egl_dmabuf.rs) already showed the driver
# reports ONLY LINEAR for every format -- no working alternative -- so
# `testCreateDmaBuf` always fails there, with nothing left to fall back to.
#
# This script exists to let you confirm that failure directly, on whichever
# machine you're chasing this on, and to compare against a known-good
# modifier value from a machine where it *does* work. NEVER hardcode/guess
# a modifier the driver doesn't itself advertise and expect it to just
# work: the NVIDIA block-linear modifier encoding (see drm_fourcc.h's
# DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D docs) is explicitly GPU-generation-
# and driver-internal-specific (the "page kind" field is derived from
# `(format, GPU model, compression type, samples per pixel)` -- not public
# anywhere) -- guessing wrong reproduces the exact garbled-video symptom
# this whole investigation started from (a mismatched-modifier
# misinterpretation), not a working fix.
#
# Usage:
#   bash scripts/test-screencast-dmabuf-roundtrip.sh <render-node> <width> <height> <modifier-hex>
#
# Modifier is a required 4th arg on purpose -- there's no sane default;
# which modifier is even worth testing differs per GPU. Find the modifiers
# a given GPU's driver actually reports via
# kwin-capture's own diagnostic in egl_dmabuf.rs (or from a live session's
# own journal -- search for "query_dmabuf_formats" if that DIAGNOSTIC block
# is still present), or pass 0x0 to test plain LINEAR.
#
# Examples:
#   bash scripts/test-screencast-dmabuf-roundtrip.sh /dev/dri/renderD128 1920 1080 0x0
#   bash scripts/test-screencast-dmabuf-roundtrip.sh /dev/dri/renderD128 1920 1080 0x300000000606014
#
# Builds to a scratch dir and cleans up after itself -- the compiled binary
# is never meant to be committed, only this source.

set -uo pipefail

if [ "$#" -lt 4 ]; then
    echo "usage: $0 <render-node> <width> <height> <modifier-hex>" >&2
    echo "  e.g.: $0 /dev/dri/renderD128 1920 1080 0x0" >&2
    exit 2
fi

WORK_DIR="$(mktemp -d /tmp/redfog-dmabuf-roundtrip-test.XXXXXX)"
trap 'rm -rf "$WORK_DIR"' EXIT

SRC="$WORK_DIR/test_dmabuf_roundtrip.c"
BIN="$WORK_DIR/test_dmabuf_roundtrip"

cat > "$SRC" <<'EOF'
// Standalone reproduction of KWin's ScreenCastStream::testCreateDmaBuf
// (src/plugins/screencast/screencaststream.cpp), independent of KWin
// entirely -- to isolate, with verbose per-step logging (unlike KWin's own
// silent `return std::nullopt` on every failure branch), exactly which
// step of the allocate -> import-as-texture -> framebuffer-complete round
// trip fails on a given GPU/driver, without needing to attach a debugger
// to a live compositor.
//
// Mirrors KWin's exact sequence:
//   1. gbm_create_device() on the given render node
//   2. gbm_bo_create_with_modifiers2() -- allocate a GBM buffer for the
//      given format+modifier (this is the actual "allocate" KWin does via
//      its DrmDevice's allocator)
//   3. Get the buffer's real DMA-BUF attributes (fd, stride, offset,
//      modifier) -- KWin's `buffer->dmabufAttributes()`
//   4. eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR, ...) + eglInitialize +
//      a real GLES context + eglMakeCurrent -- KWin's
//      `backend->openglContext()->makeCurrent()`
//   5. eglCreateImageKHR(..., EGL_LINUX_DMA_BUF_EXT, ...) on THAT SAME
//      dma-buf fd, then glEGLImageTargetTexture2DOES -- KWin's
//      `backend->importDmaBufAsTexture(*attrs)`
//   6. Attach to an FBO, glCheckFramebufferStatus == COMPLETE -- KWin's
//      `framebuffer->valid()`
//
// Build:
//   gcc -o test_dmabuf_roundtrip test_dmabuf_roundtrip.c \
//       $(pkg-config --cflags --libs gbm egl glesv2) -ldrm -I/usr/include/libdrm
//
// Run (render node and resolution default to /dev/dri/renderD128 and
// 1920x1080; modifier is a required 4th arg -- deliberately no default,
// see its own comment below for why):
//   ./test_dmabuf_roundtrip [/dev/dri/renderDXXX] [width] [height] <modifier-hex>
//   e.g.: ./test_dmabuf_roundtrip /dev/dri/renderD128 1920 1080 0x0

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <gbm.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <drm_fourcc.h>

#define STEP(fmt, ...) printf("[step] " fmt "\n", ##__VA_ARGS__)
#define FAIL(fmt, ...) do { printf("[FAIL] " fmt "\n", ##__VA_ARGS__); return 1; } while (0)
#define OK(fmt, ...) printf("[ OK ] " fmt "\n", ##__VA_ARGS__)

static const char *egl_error_string(EGLint err) {
    switch (err) {
        case EGL_SUCCESS: return "EGL_SUCCESS";
        case EGL_NOT_INITIALIZED: return "EGL_NOT_INITIALIZED";
        case EGL_BAD_ACCESS: return "EGL_BAD_ACCESS";
        case EGL_BAD_ALLOC: return "EGL_BAD_ALLOC";
        case EGL_BAD_ATTRIBUTE: return "EGL_BAD_ATTRIBUTE";
        case EGL_BAD_CONTEXT: return "EGL_BAD_CONTEXT";
        case EGL_BAD_CONFIG: return "EGL_BAD_CONFIG";
        case EGL_BAD_CURRENT_SURFACE: return "EGL_BAD_CURRENT_SURFACE";
        case EGL_BAD_DISPLAY: return "EGL_BAD_DISPLAY";
        case EGL_BAD_SURFACE: return "EGL_BAD_SURFACE";
        case EGL_BAD_MATCH: return "EGL_BAD_MATCH";
        case EGL_BAD_PARAMETER: return "EGL_BAD_PARAMETER";
        case EGL_BAD_NATIVE_PIXMAP: return "EGL_BAD_NATIVE_PIXMAP";
        case EGL_BAD_NATIVE_WINDOW: return "EGL_BAD_NATIVE_WINDOW";
        case EGL_CONTEXT_LOST: return "EGL_CONTEXT_LOST";
        default: return "unknown";
    }
}
#define EGL_CHECK(what) do { \
        EGLint e = eglGetError(); \
        if (e != EGL_SUCCESS) printf("       (%s: eglGetError() = 0x%x %s)\n", what, e, egl_error_string(e)); \
    } while (0)

int main(int argc, char **argv) {
    const char *node = argc > 1 ? argv[1] : "/dev/dri/renderD128";
    int width = argc > 2 ? atoi(argv[2]) : 1920;
    int height = argc > 3 ? atoi(argv[3]) : 1080;
    // Modifier is a required 4th arg (hex, e.g. 0x0 for LINEAR or
    // 0x300000000606014 for a real tiled one) -- deliberately no silent
    // default: which modifier is even worth testing differs per GPU (this
    // machine's own driver only succeeds with its real tiled modifier, not
    // LINEAR -- confirmed live testing this exact tool), so guessing one
    // here would produce a misleading, non-representative result.
    if (argc <= 4) {
        fprintf(stderr, "usage: %s <render-node> <width> <height> <modifier-hex>\n", argv[0]);
        fprintf(stderr, "  e.g.: %s /dev/dri/renderD128 1920 1080 0x0\n", argv[0]);
        return 2;
    }
    uint32_t format = DRM_FORMAT_ARGB8888; // "AR24" -- matches the format redfog's own KWin session negotiates
    uint64_t modifier = strtoull(argv[4], NULL, 0);

    printf("=== testing %s, format=AR24 (0x%08x), modifier=0x%llx, %dx%d ===\n\n", node, format, (unsigned long long)modifier, width, height);

    // Identify which physical GPU this actually is via sysfs, before any
    // GBM/EGL call -- deliberately first, and independent of whether
    // anything below succeeds: GL_RENDERER (the only other GPU-identifying
    // string in this whole program) is only ever printed *after*
    // eglMakeCurrent, which never happens on the failure path this tool
    // exists to hit. Without this, two different NVIDIA GPUs failing at
    // the same early step would produce output indistinguishable from
    // each other -- no independent proof this ran against the GPU you
    // think it did, on whichever machine you're running it on.
    {
        const char *base = strrchr(node, '/');
        base = base ? base + 1 : node;
        char path[256];
        char vendor[16] = "?", device[16] = "?";
        snprintf(path, sizeof(path), "/sys/class/drm/%s/device/vendor", base);
        FILE *f = fopen(path, "r");
        if (f) { if (fgets(vendor, sizeof(vendor), f)) vendor[strcspn(vendor, "\n")] = 0; fclose(f); }
        snprintf(path, sizeof(path), "/sys/class/drm/%s/device/device", base);
        f = fopen(path, "r");
        if (f) { if (fgets(device, sizeof(device), f)) device[strcspn(device, "\n")] = 0; fclose(f); }
        printf("[info] PCI identity of %s: vendor=%s device=%s (via sysfs, independent of GBM/EGL)\n\n", node, vendor, device);
    }

    STEP("open %s", node);
    int fd = open(node, O_RDWR);
    if (fd < 0) FAIL("open(%s) failed: %s", node, strerror(errno));
    OK("opened, fd=%d", fd);

    STEP("gbm_create_device(fd)");
    struct gbm_device *gbm = gbm_create_device(fd);
    if (!gbm) FAIL("gbm_create_device failed");
    OK("gbm device created, backend name=%s", gbm_device_get_backend_name(gbm));

    STEP("gbm_bo_create_with_modifiers2(format=AR24, modifiers=[LINEAR], usage=GBM_BO_USE_RENDERING)");
    struct gbm_bo *bo = gbm_bo_create_with_modifiers2(gbm, width, height, format, &modifier, 1, GBM_BO_USE_RENDERING);
    if (!bo) {
        printf("       (retrying with GBM_BO_USE_LINEAR added explicitly)\n");
        bo = gbm_bo_create_with_modifiers2(gbm, width, height, format, &modifier, 1, GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR);
    }
    if (!bo) FAIL("gbm_bo_create_with_modifiers2 failed for every attempt -- THIS is where KWin's allocate() call would already return null");
    OK("gbm_bo allocated: %dx%d, planes=%d, modifier=0x%llx",
       gbm_bo_get_width(bo), gbm_bo_get_height(bo), gbm_bo_get_plane_count(bo),
       (unsigned long long)gbm_bo_get_modifier(bo));

    STEP("gbm_bo_get_fd_for_plane(bo, 0) -- get the real dma-buf fd");
    int dmabuf_fd = gbm_bo_get_fd_for_plane(bo, 0);
    if (dmabuf_fd < 0) FAIL("gbm_bo_get_fd_for_plane failed: %s", strerror(errno));
    uint32_t stride = gbm_bo_get_stride_for_plane(bo, 0);
    uint32_t offset = gbm_bo_get_offset(bo, 0);
    OK("dma-buf fd=%d, stride=%u, offset=%u", dmabuf_fd, stride, offset);

    STEP("eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR, gbm_device)");
    PFNEGLGETPLATFORMDISPLAYEXTPROC eglGetPlatformDisplayEXT =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay dpy = eglGetPlatformDisplayEXT ? eglGetPlatformDisplayEXT(EGL_PLATFORM_GBM_KHR, gbm, NULL)
                                               : eglGetDisplay((EGLNativeDisplayType)gbm);
    if (dpy == EGL_NO_DISPLAY) FAIL("eglGetPlatformDisplay(EXT) returned EGL_NO_DISPLAY");
    EGLint major, minor;
    if (!eglInitialize(dpy, &major, &minor)) { EGL_CHECK("eglInitialize"); FAIL("eglInitialize failed"); }
    OK("EGL %d.%d initialized, vendor=%s, extensions include dma_buf_import=%s",
       major, minor, eglQueryString(dpy, EGL_VENDOR),
       strstr(eglQueryString(dpy, EGL_EXTENSIONS), "EGL_EXT_image_dma_buf_import") ? "yes" : "NO");

    STEP("eglBindAPI(EGL_OPENGL_ES_API) + choose config + create context");
    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint config_attribs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                               EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE};
    EGLConfig config;
    EGLint num_configs;
    if (!eglChooseConfig(dpy, config_attribs, &config, 1, &num_configs) || num_configs < 1) {
        EGL_CHECK("eglChooseConfig"); FAIL("eglChooseConfig found no usable config");
    }
    EGLint ctx_attribs[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, config, EGL_NO_CONTEXT, ctx_attribs);
    if (ctx == EGL_NO_CONTEXT) { EGL_CHECK("eglCreateContext"); FAIL("eglCreateContext failed"); }
    OK("context created");

    STEP("eglMakeCurrent (surfaceless, matching KWin's own headless makeCurrent())");
    if (!eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
        EGL_CHECK("eglMakeCurrent"); FAIL("eglMakeCurrent failed");
    }
    OK("context is current, GL_RENDERER=%s", (const char *)glGetString(GL_RENDERER));

    STEP("eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT) importing our own freshly-allocated dma-buf back in");
    PFNEGLCREATEIMAGEKHRPROC eglCreateImageKHR = (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
    PFNEGLDESTROYIMAGEKHRPROC eglDestroyImageKHR = (PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
    if (!eglCreateImageKHR) FAIL("eglGetProcAddress(eglCreateImageKHR) returned NULL -- extension not exposed");
    EGLint image_attribs[] = {
        EGL_WIDTH, width,
        EGL_HEIGHT, height,
        EGL_LINUX_DRM_FOURCC_EXT, (EGLint)format,
        EGL_DMA_BUF_PLANE0_FD_EXT, dmabuf_fd,
        EGL_DMA_BUF_PLANE0_OFFSET_EXT, (EGLint)offset,
        EGL_DMA_BUF_PLANE0_PITCH_EXT, (EGLint)stride,
        EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, (EGLint)(modifier & 0xffffffff),
        EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (EGLint)(modifier >> 32),
        EGL_NONE,
    };
    EGLImageKHR image = eglCreateImageKHR(dpy, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, image_attribs);
    if (image == EGL_NO_IMAGE_KHR) {
        EGL_CHECK("eglCreateImageKHR");
        FAIL("eglCreateImageKHR failed -- THIS is the exact step KWin's importDmaBufAsTexture would fail at");
    }
    OK("EGLImage created");

    STEP("glEGLImageTargetTexture2DOES -- bind the imported image to a GL texture");
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC glEGLImageTargetTexture2DOES =
        (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    if (!glEGLImageTargetTexture2DOES) FAIL("eglGetProcAddress(glEGLImageTargetTexture2DOES) returned NULL");
    GLuint tex;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glEGLImageTargetTexture2DOES(GL_TEXTURE_2D, image);
    GLenum gl_err = glGetError();
    if (gl_err != GL_NO_ERROR) FAIL("glEGLImageTargetTexture2DOES set GL error 0x%x", gl_err);
    OK("texture bound to imported image");

    STEP("attach to FBO + glCheckFramebufferStatus");
    GLuint fbo;
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    GLenum status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    if (status != GL_FRAMEBUFFER_COMPLETE) {
        FAIL("glCheckFramebufferStatus = 0x%x, not GL_FRAMEBUFFER_COMPLETE (0x%x) -- THIS is the exact step KWin's framebuffer->valid() would fail at",
             status, GL_FRAMEBUFFER_COMPLETE);
    }
    OK("framebuffer complete");

    printf("\n=== ALL STEPS SUCCEEDED -- this GPU/driver can do the exact round trip KWin's screencast DMA-BUF path needs ===\n");

    eglDestroyImageKHR(dpy, image);
    close(dmabuf_fd);
    gbm_bo_destroy(bo);
    gbm_device_destroy(gbm);
    close(fd);
    return 0;
}
EOF

echo "=== compiling ==="
if ! gcc -O2 -o "$BIN" "$SRC" $(pkg-config --cflags --libs gbm egl glesv2) -ldrm -I/usr/include/libdrm; then
    echo "compile failed -- do you have libgbm/libegl/libglesv2/libdrm dev headers installed?" >&2
    exit 1
fi

echo
"$BIN" "$@"
