#!/usr/bin/env bash
# Fetches and patches dependencies that need local fixes not yet upstream,
# without vendoring GPL source into this repo's git history. Run this once
# before building/testing (idempotent — skips anything already fetched).
#
# See patches/*.patch for what's applied and why.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
vendor_dir="$repo_root/vendor"
mkdir -p "$vendor_dir"

fetch_and_patch() {
    local name="$1" url="$2" commit="$3"
    shift 3
    local dest="$vendor_dir/$name"

    if [ -d "$dest" ]; then
        echo "[$name] already present at $dest, skipping (delete it to re-fetch)"
        return
    fi

    echo "[$name] cloning $url @ $commit..."
    git clone --quiet "$url" "$dest"
    git -C "$dest" checkout --quiet "$commit"
    rm -rf "$dest/.git"

    for patch in "$@"; do
        echo "[$name] applying $patch..."
        # Plain `patch`, not `git apply`: $dest has no .git of its own (removed
        # above), so `git apply` resolves paths against the *outer* redfog
        # repo instead — and since `vendor/` is gitignored there, it silently
        # *skips* any patch hunk that adds a new file (exit 0, no error),
        # rather than failing loudly. Confirmed live while adding the
        # inputtino touchscreen-wrapper patch. `patch -p1` has no such quirk.
        (cd "$dest" && patch -p1 --quiet < "$repo_root/$patch")
    done

    echo "[$name] ready."
}

# GPL-3.0-or-later, dev-only (redfog-moonlight's integration tests/examples;
# never shipped in our own server). Patches:
#  - rtsp-port-parsing: two upstream bugs in its RTSP Transport-header
#    parsing (wrong delimiter, wrong port fallback constant, didn't handle
#    port ranges) that made the ENet control channel unable to connect
#    whenever server_port differed from 47998 — confirmed live.
#  - stream-driver-abort-on-stop: each stream's background UDP driver task
#    (audio/video/control) never voluntarily exits on its own -- only a
#    real protocol/IO error makes `StreamDriver::poll` return -- so
#    `MoonlightStream::stop()`/`Drop` leaves them running forever;
#    `notify()` only wakes callers of poll_frame()/poll_packet(), not
#    these tasks. Fatal specifically inside a `#[tokio::test]`: a
#    panicking test's Runtime is dropped during unwind, and dropping a
#    Runtime waits for every still-running spawned task — confirmed live,
#    a real CI job burned its entire 30-minute budget this way after an
#    otherwise cleanly-bounded test assertion failure. Fixed by keeping
#    each driver task's JoinHandle and aborting it directly from stop().
# Fixes for both will be proposed upstream separately.
fetch_and_patch \
    "moonlight-common-rust" \
    "https://github.com/MrCreativ3001/moonlight-common-rust" \
    "06f0d2efbb4e1c769cdd8f8d5a92e00fc192842b" \
    "patches/moonlight-common-rust-rtsp-port-parsing.patch" \
    "patches/moonlight-common-rust-stream-driver-abort-on-stop.patch"

# MIT/Apache-2.0 — vendored here (see Cargo.toml's [patch.crates-io] comment)
# only because pam-sys 1.0.0-alpha5 never published a release with a newer
# bindgen; its own build-dependency pin (bindgen "0.69") panics against this
# machine's clang/LLVM 22.1.8 ("a `libclang` shared library is not loaded on
# this thread" — confirmed live, root cause not fully chased down, but
# unrelated bindgen 0.72+ users elsewhere in this workspace are unaffected).
fetch_and_patch \
    "pam-sys" \
    "https://github.com/1wilkens/pam-sys" \
    "v1.0.0-alpha5" \
    "patches/pam-sys-bindgen-bump.patch"

# MIT — vendored (not on crates.io at all: `bindings/rust/{inputtino-sys,
# inputtino}` are unpublished, and inputtino-sys's build.rs resolves the C++
# tree it builds via a relative `../../../` path, so the two must be
# co-located). Provides a virtual Xbox One gamepad via uinput (see
# design.md's "Future idea: uinput virtual devices" and the
# virtual-input-devices plan -- keyboard/mouse/touch stayed on KWin's
# fake_input instead, since KWin's headless backend has no real seat and its
# libinput integration can't discover *any* uinput device, virtual or
# physical; gamepad input bypasses the compositor entirely, since games read
# the controller device directly, so it's unaffected by that). No upstream
# release tags exist; pinned to a specific commit on the `stable` branch,
# same as moonlight-common-rust above. Patches:
#  - remove-dev-dependencies: the `inputtino` crate's own [dev-dependencies]
#    (for its own tests/examples, which we never build) pin `sdl2 = "0.37.0"`
#    -- conflicts at resolution time with redfog-test-ux's own `sdl2 = "^0.38"`
#    (both `links = "SDL2"", and Cargo's resolver considers a path
#    dependency's dev-dependencies too, even when nothing will build them)
#    -- confirmed live as a real `cargo build` failure before this patch.
#  - only-link-stdcxx: inputtino-sys's build.rs links both stdc++ (GCC/
#    libstdc++) and c++ (LLVM/libc++) unconditionally, but the CMake build
#    it drives always compiles with the system default `c++` compiler (GCC
#    everywhere we build), so libc++ is never actually needed -- and often
#    isn't installed at all. Confirmed live: linking failed with "unable to
#    find library -lc++" without this patch.
fetch_and_patch \
    "inputtino" \
    "https://github.com/games-on-whales/inputtino" \
    "d28ec79eb63324e68d73a7de22bcb5ff0a6f6bf8" \
    "patches/inputtino-remove-dev-dependencies.patch" \
    "patches/inputtino-only-link-stdcxx.patch"
