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
    local name="$1" url="$2" commit="$3" patch="$4"
    local dest="$vendor_dir/$name"

    if [ -d "$dest" ]; then
        echo "[$name] already present at $dest, skipping (delete it to re-fetch)"
        return
    fi

    echo "[$name] cloning $url @ $commit..."
    git clone --quiet "$url" "$dest"
    git -C "$dest" checkout --quiet "$commit"
    rm -rf "$dest/.git"

    echo "[$name] applying $patch..."
    git -C "$dest" apply --quiet "$repo_root/$patch" 2>/dev/null \
        || (cd "$dest" && patch -p1 --quiet < "$repo_root/$patch")

    echo "[$name] ready."
}

# GPL-3.0-or-later, dev-only (redfog-moonlight's integration tests/examples;
# never shipped in our own server). Patches two upstream bugs in its RTSP
# Transport-header parsing (wrong delimiter, wrong port fallback constant,
# didn't handle port ranges) that made the ENet control channel unable to
# connect whenever server_port differed from 47998 — confirmed live. A fix
# will be proposed upstream separately.
fetch_and_patch \
    "moonlight-common-rust" \
    "https://github.com/MrCreativ3001/moonlight-common-rust" \
    "06f0d2efbb4e1c769cdd8f8d5a92e00fc192842b" \
    "patches/moonlight-common-rust-rtsp-port-parsing.patch"
