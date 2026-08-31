#!/usr/bin/env bash
# Brute-force search for a working NVIDIA block-linear (tiled) DRM format
# modifier on a GPU whose driver doesn't advertise one via
# eglQueryDmaBufModifiersEXT -- see scripts/test-screencast-dmabuf-roundtrip.sh
# for the background (KWin's screencast DMA-BUF path needs *some* modifier
# it can both allocate a renderable buffer with *and* successfully reimport
# as a GL texture; LINEAR fails that test on NVIDIA generally, and a real
# tiled modifier is what rescues it -- confirmed live on an RTX 2080. On
# a GTX 1070 (agnesi), the EGL query never offers one at all, only LINEAR).
#
# The NVIDIA block-linear modifier encoding
# (DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D in drm_fourcc.h) is a bitfield of
# (compression c, sector-layout s, GOB-height/generation g, page-kind k,
# GOB-height-log2 h) -- everything except the "page kind" field is
# documented with a small enumerated range; "page kind" is explicitly
# documented as GPU-model-and-format-internal, derived by NVIDIA's own
# driver, with no public table anywhere. There is no way to *derive* the
# right value for a given GPU from the header alone -- but since the
# search space is small (a handful of documented g/h values times 256
# possible page-kind values), it's cheap to just try all of them directly
# against the real driver and see which ones (if any) actually work, per
# GPU generation.
#
# Fixes c=0 (no lossless-compression scheme -- the simplest, most broadly
# applicable case; a basic uncompressed screencast buffer has no reason to
# need one) for the primary sweep, varying g (0..2, all three documented
# generation mappings, since it costs nothing to check all of them even
# though g=0 "Fermi-Volta" is what Pascal is documented to need), h (0..5,
# all six documented GOB-height options), s (0..3, all four documented
# sector-layout values -- confirmed live this matters: the known-working
# RTX 2080 modifier has s=1, not s=0, found only after re-deriving it by
# hand against the real macro rather than trusting an earlier, incomplete
# manual bit-decode), and k (0..255, the full page-kind byte).
#
# For each candidate that successfully allocates, also does the *full*
# round trip (EGL import as texture + framebuffer-complete check) using
# one shared EGL context set up once up front -- allocating alone isn't
# sufficient (that's exactly the trap testCreateDmaBuf's own comment warns
# about: "may fail on some drivers with some modifiers"), only a modifier
# that survives the whole thing is actually usable by KWin's real
# screencast path.
#
# Usage:
#   bash scripts/test-screencast-dmabuf-modifier-search.sh <render-node> <width> <height>
#   e.g.: bash scripts/test-screencast-dmabuf-modifier-search.sh /dev/dri/renderD128 1920 1080
#
# Builds to a scratch dir and cleans up after itself -- the compiled binary
# is never meant to be committed, only this source.

set -uo pipefail

if [ "$#" -lt 3 ]; then
    echo "usage: $0 <render-node> <width> <height>" >&2
    echo "  e.g.: $0 /dev/dri/renderD128 1920 1080" >&2
    exit 2
fi

WORK_DIR="$(mktemp -d /tmp/redfog-dmabuf-modifier-search.XXXXXX)"
trap 'rm -rf "$WORK_DIR"' EXIT

SRC="$WORK_DIR/search.c"
BIN="$WORK_DIR/search"

cat > "$SRC" <<'EOF'
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

int main(int argc, char **argv) {
    if (argc < 4) {
        fprintf(stderr, "usage: %s <render-node> <width> <height>\n", argv[0]);
        return 2;
    }
    const char *node = argv[1];
    int width = atoi(argv[2]);
    int height = atoi(argv[3]);
    uint32_t format = DRM_FORMAT_ARGB8888; // "AR24"

    {
        const char *base = strrchr(node, '/');
        base = base ? base + 1 : node;
        char path[256], vendor[16] = "?", device[16] = "?";
        snprintf(path, sizeof(path), "/sys/class/drm/%s/device/vendor", base);
        FILE *f = fopen(path, "r");
        if (f) { if (fgets(vendor, sizeof(vendor), f)) vendor[strcspn(vendor, "\n")] = 0; fclose(f); }
        snprintf(path, sizeof(path), "/sys/class/drm/%s/device/device", base);
        f = fopen(path, "r");
        if (f) { if (fgets(device, sizeof(device), f)) device[strcspn(device, "\n")] = 0; fclose(f); }
        printf("=== searching for a working tiled modifier on %s (PCI vendor=%s device=%s), format=AR24, %dx%d ===\n\n",
               node, vendor, device, width, height);
    }

    int fd = open(node, O_RDWR);
    if (fd < 0) { fprintf(stderr, "open(%s) failed: %s\n", node, strerror(errno)); return 1; }
    struct gbm_device *gbm = gbm_create_device(fd);
    if (!gbm) { fprintf(stderr, "gbm_create_device failed\n"); return 1; }
    printf("gbm backend: %s\n", gbm_device_get_backend_name(gbm));

    // One shared EGL context for every candidate's import+framebuffer
    // check -- context/display setup is the expensive part; the
    // allocate-and-import loop itself is cheap.
    PFNEGLGETPLATFORMDISPLAYEXTPROC eglGetPlatformDisplayEXT =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay dpy = eglGetPlatformDisplayEXT ? eglGetPlatformDisplayEXT(EGL_PLATFORM_GBM_KHR, gbm, NULL)
                                               : eglGetDisplay((EGLNativeDisplayType)gbm);
    if (dpy == EGL_NO_DISPLAY) { fprintf(stderr, "eglGetPlatformDisplay failed\n"); return 1; }
    EGLint major, minor;
    if (!eglInitialize(dpy, &major, &minor)) { fprintf(stderr, "eglInitialize failed\n"); return 1; }
    printf("EGL %d.%d, vendor=%s\n\n", major, minor, eglQueryString(dpy, EGL_VENDOR));

    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint config_attribs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                               EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE};
    EGLConfig config;
    EGLint num_configs;
    if (!eglChooseConfig(dpy, config_attribs, &config, 1, &num_configs) || num_configs < 1) {
        fprintf(stderr, "eglChooseConfig failed\n"); return 1;
    }
    EGLint ctx_attribs[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, config, EGL_NO_CONTEXT, ctx_attribs);
    if (ctx == EGL_NO_CONTEXT) { fprintf(stderr, "eglCreateContext failed\n"); return 1; }
    if (!eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) { fprintf(stderr, "eglMakeCurrent failed\n"); return 1; }
    printf("GL_RENDERER=%s\n\n", (const char *)glGetString(GL_RENDERER));

    PFNEGLCREATEIMAGEKHRPROC eglCreateImageKHR = (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
    PFNEGLDESTROYIMAGEKHRPROC eglDestroyImageKHR = (PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC glEGLImageTargetTexture2DOES =
        (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    if (!eglCreateImageKHR || !glEGLImageTargetTexture2DOES) {
        fprintf(stderr, "required EGL/GL extension procs not available\n"); return 1;
    }

    int allocated_count = 0, complete_count = 0, tried = 0;
    // c=0 (no compression) fixed for this sweep -- see this script's own
    // header comment for why. g, h, s, k sweep their full
    // documented/possible ranges.
    for (int g = 0; g <= 2; g++) {
        for (int h = 0; h <= 5; h++) {
            for (int s = 0; s <= 3; s++) {
                for (int k = 0; k <= 255; k++) {
                    tried++;
                    uint64_t modifier = DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(0, s, g, k, h);
                    struct gbm_bo *bo = gbm_bo_create_with_modifiers2(gbm, width, height, format, &modifier, 1, GBM_BO_USE_RENDERING);
                    if (!bo) continue;
                    allocated_count++;

                    int dmabuf_fd = gbm_bo_get_fd_for_plane(bo, 0);
                    uint32_t stride = gbm_bo_get_stride_for_plane(bo, 0);
                    uint32_t offset = gbm_bo_get_offset(bo, 0);
                    uint64_t real_modifier = gbm_bo_get_modifier(bo); // driver may normalize/reject our exact bits -- log what it actually reports
                    int complete = 0;
                    if (dmabuf_fd >= 0) {
                        EGLint image_attribs[] = {
                            EGL_WIDTH, width, EGL_HEIGHT, height, EGL_LINUX_DRM_FOURCC_EXT, (EGLint)format,
                            EGL_DMA_BUF_PLANE0_FD_EXT, dmabuf_fd, EGL_DMA_BUF_PLANE0_OFFSET_EXT, (EGLint)offset,
                            EGL_DMA_BUF_PLANE0_PITCH_EXT, (EGLint)stride,
                            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, (EGLint)(real_modifier & 0xffffffff),
                            EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (EGLint)(real_modifier >> 32),
                            EGL_NONE,
                        };
                        EGLImageKHR image = eglCreateImageKHR(dpy, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, image_attribs);
                        if (image != EGL_NO_IMAGE_KHR) {
                            GLuint tex, fbo;
                            glGenTextures(1, &tex);
                            glBindTexture(GL_TEXTURE_2D, tex);
                            glEGLImageTargetTexture2DOES(GL_TEXTURE_2D, image);
                            if (glGetError() == GL_NO_ERROR) {
                                glGenFramebuffers(1, &fbo);
                                glBindFramebuffer(GL_FRAMEBUFFER, fbo);
                                glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
                                complete = glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE;
                                glDeleteFramebuffers(1, &fbo);
                            }
                            glDeleteTextures(1, &tex);
                            eglDestroyImageKHR(dpy, image);
                        }
                        close(dmabuf_fd);
                    }
                    if (complete) complete_count++;
                    printf("g=%d h=%d s=%d k=%3d -> requested modifier=0x%013llx, driver reports=0x%013llx : allocate=OK import+framebuffer=%s\n",
                           g, h, s, k, (unsigned long long)modifier, (unsigned long long)real_modifier, complete ? "COMPLETE (usable!)" : "failed");
                    gbm_bo_destroy(bo);
                }
            }
        }
    }

    printf("\n=== done: tried %d combinations, %d allocated, %d fully usable (import+framebuffer complete) ===\n",
           tried, allocated_count, complete_count);
    if (complete_count == 0) {
        printf("no working tiled modifier found in this search space -- this GPU/driver genuinely appears to have no usable\n"
               "non-LINEAR modifier for this format via this path (or one exists outside c=0,s=0 -- see this script's header).\n");
    }
    return complete_count > 0 ? 0 : 1;
}
EOF

echo "=== compiling ==="
if ! gcc -O2 -o "$BIN" "$SRC" $(pkg-config --cflags --libs gbm egl glesv2) -ldrm -I/usr/include/libdrm; then
    echo "compile failed -- do you have libgbm/libegl/libglesv2/libdrm dev headers installed?" >&2
    exit 1
fi

echo
"$BIN" "$@"
