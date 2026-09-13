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
        git -C "$dest" apply --quiet "$repo_root/$patch" 2>/dev/null \
            || (cd "$dest" && patch -p1 --quiet < "$repo_root/$patch")
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
