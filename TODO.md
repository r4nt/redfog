# TODO

Audit of what's missing for a complete KDE/Plasma streaming session over
redfog, as of 2026-08-31 (after getting HEVC fully working end to end).
Grounded in the current code, not just the original plan doc, which is
stale in places (e.g. it didn't anticipate needing FEC).

## Priority: network robustness

Audio and video FEC are both implemented and confirmed live now (see
"Recently fixed" below) — neither has specifically been tested against
real induced packet loss yet (confirmed not to break anything with FEC
active, not yet confirmed to actually recover a dropped shard end to end
against a real client).

## Cross-repo: moonlight-web-stream (~/src/moonlight-web-stream)

- [ ] Real network-adaptive behavior for the browser-facing hop. The
      actual topology when using the web client is browser <-WebRTC->
      `moonlight-web-stream` <-Moonlight protocol-> redfog, i.e. two
      separate hops with two separate loss/congestion domains. Our new
      server-side adaptive bitrate (see below) only ever sees the second
      hop (redfog <-> moonlight-web-stream's `streamer`), which in any
      normal deployment (same host/LAN) is close to lossless — it can't
      see or react to the browser's real connection, which is the one
      that actually matters. Checked `streamer/src/transport/webrtc/
      video.rs`: it declares `nack`/`nack,pli`/`goog-remb` as supported
      RTCP feedback types in the SDP codec capabilities, but nothing in
      that codebase actually reads incoming RTCP feedback from the
      browser — it's registered capability, not active handling. Right
      now nothing anywhere reacts to the browser's real conditions;
      manually lowering bitrate/fps/resolution in the client's settings
      menu (a static, connect-time-only choice, sent once via `/launch`)
      is the only lever a user has today.
      Fixing this is entirely a `moonlight-web-stream`-side change, not a
      redfog one: read the browser's REMB/PLI feedback and either (a)
      translate a sustained low bandwidth estimate into a synthetic
      signal sent upstream to redfog (piggybacking on the same
      `LossStats`-reactive path we just built), or (b) handle it locally
      (buffer/re-pace what it forwards) without ever involving redfog at
      all. Not started; flagging for later, possibly as an actual
      contribution to that project rather than redfog itself.

## By design (not a bug — noted here so it doesn't get re-flagged)

- A User-stage session that just disconnects (network drop, client
  closing the app) is *backgrounded*, not torn down — its compositor
  keeps running in `Playing` state indefinitely so a reconnect is instant
  instead of a slow respawn. `TerminateSession` only fires from an
  explicit "Log Out" in the login UI (`SessionManager::handle_log_out`);
  a reboot also ends it, obviously. Verified live: reconnecting
  repeatedly as the same user reuses the exact same compositor process
  every time (`handoff_to_user` looks it up in `background_sessions` by
  username first) — it does not accumulate one per reconnect. What stays
  alive is one compositor per real user account that's ever connected and
  not explicitly logged out, which is the intended behavior (persistent
  sessions, like a real desktop), not a leak.

## Known active gaps (pre-existing, not new)

- [ ] `connection_integration` test failures: `login_after_log_out_recovers_
      from_a_resume_hang`, `video_port_recovers_after_a_resume_hang`,
      `video_throttles_after_resume_under_input_driven_damage` all
      currently fail. Confirmed real resume behavior works fine in actual
      live/manual testing right now, so this is very likely the test
      harness/environment (this whole session's sandbox has been through a
      lot of churn — repeated broker/server restarts, leftover systemd
      units, etc.), not a real functional regression — but not actually
      verified which yet. Deliberately left as a TODO rather than chased
      down now. `gst_wayland_display_backend_smoke_test` also fails but is
      expected to in a plain `cargo test` (needs a separately-built plugin
      dir most environments, including this one, don't have configured —
      normally excluded via `--skip` in real CI/local runs).
- [ ] `CudaDirectEncoderSession::reconfigure`'s same-resolution fast path
      (`reconfigure_reuses_capture_connection` in
      `kwin-capture/tests/nvenc_reconfigure_reuses_capture.rs`, still
      `#[ignore]`d) leaks a few `/dmabuf:` fds per call — only on
      pre-Ampere GPUs, where frame import goes through
      `vulkan_bridge.rs`'s detile-to-linear-buffer
      path instead of a direct CUDA array import. Read through the whole
      teardown chain (`RegisteredResource`/`ImportedLinear`/`MappedBuffer`/
      `ExternalMemory`/`BridgedImage` drop order) without finding the bug;
      needs either deeper live debugging or a GPU where the direct-import
      path is actually exercised to compare against. **Now believed to be
      the same root cause as a worse, since-fixed symptom** — see
      "Recently fixed" above: this exact fast path was also silently
      breaking moonlight-web-stream's HEVC decode on every
      same-resolution takeover/resume, bad enough that `session.rs` no
      longer calls `reconfigure()` at all (always does a full rebuild
      instead). One real, live-observed architectural detail worth
      starting from if this gets picked up: `kwin-capture` links *two*
      different versions of the `cudarc` crate simultaneously (`cudarc`
      0.19 for `cuda_import.rs`'s `CudaImporter`, and `cudarc016` = 0.16
      for the `CudaContext` `nvidia-video-codec-sdk` requires), both
      believed to retain/release the *same* underlying CUDA primary
      context for device 0 via refcounting — and both get torn down and
      recreated together on every `reconfigure()` cycle, repeatedly, on
      the same thread. A fresh spawn only ever exercises that pairing
      once. Needs live GPU-level tooling (`cuda-gdb`, driver-level
      context/refcount tracing) to actually confirm, not more static
      reading — already tried once without success.

## Deliberate deferrals (documented, not bugs — just not built yet)

- [ ] Gamepad/controller input. `control.rs` decodes keyboard + mouse
      only; every other input event type (including all gamepad packets)
      hits `_ => None` and is silently dropped.
- [ ] HDR, AV1. `<IsHdrSupported>0</IsHdrSupported>` is hardcoded. Video
      itself now does both H.264 and HEVC (see "recently fixed" below) —
      AV1 isn't implemented, and testing it needs Ada Lovelace+ hardware
      (the first NVIDIA generation with AV1 encode support).
- [ ] HiDPI passthrough. KWin's virtual output is spawned with
      `--scale 1` hardcoded; never scales.
- [ ] Live resolution/fps *re*negotiation (i.e. changing it mid-session,
      without a reconnect). The client's requested resolution and fps cap
      (see "recently fixed" below) are both applied now, but only once, at
      `/launch` — the "Foundation Sunshine dynamic stream param change"
      extension bundles a true live version of this with bitrate, but
      they're not actually the same problem: bitrate needs zero client
      cooperation (an H.264 bitstream doesn't encode its own bitrate
      anywhere, so nothing downstream needs telling — see server-side
      adaptive bitrate below). Resolution/fps *changing mid-stream* are
      structural — the client's rendering surface, texture allocation, and
      jitter-buffer sizing all need advance notice — so this genuinely
      needs client-side protocol support redfog doesn't control
      (`moonlight-common-rust`/`moonlight-web-stream` don't have it
      either; it's an unimplemented TODO in the vendored library itself).
      Cross-repo effort, not just a redfog change. Would need a reconnect
      to change today.
- [ ] Config is ~15+ separate `REDFOG_*` env vars across
      `redfog-server`/`redfog-broker`/`redfog-core`, mixing real
      user-facing settings (backend, encoder, ports, bitrate) with
      debug-only escape hatches and test-only overrides in one flat
      namespace. Worth converging real config into a file (following the
      `session_presets` TOML precedent) before anyone but the maintainer
      runs this. Not urgent for solo dev iteration.

## Recently fixed (2026-08-31, for context — not TODO items)

- **HEVC working end to end** (negotiation, Login-stage codec selection,
  and the real per-session NVENC encode path) — four independent,
  compounding bugs, each masking the next:
  1. Real clients (moonlight-qt, moonlight-web-stream) never offered HEVC
     in RTSP ANNOUNCE at all, regardless of `/serverinfo`'s
     `ServerCodecModeSupport` bit. Root cause: real clients (confirmed
     against actual moonlight-common-c and Sunshine source) gate HEVC
     eligibility on finding the literal string
     `sprop-parameter-sets=AAAAAU` in the RTSP DESCRIBE response — a
     signal completely separate from `/serverinfo` that redfog never
     sent. `rtsp.rs`'s `sdp()` now includes it.
  2. The Login stage's video pipeline was built and started playing
     *before* RTSP ANNOUNCE ever revealed the negotiated codec, always
     defaulting to H.264 regardless of what the client's decoder had
     already committed to. Fixed by deferring the Login pipeline's real
     construction from `/launch` time to right before RTSP PLAY, which
     always comes after ANNOUNCE for every client (`session.rs`'s
     `start_streaming`).
  3. GStreamer's `x265enc` (Login stage's software HEVC encoder) only
     emits VPS/SPS/PPS once, at stream start — every keyframe after the
     first arrived with no parameter sets, undecodable. Fixed with
     `h265parse config-interval=-1`.
  4. The real per-session NVENC path (`CudaDirectEncoderSession`) crashed
     `encode_picture` on the second HEVC frame. Root cause:
     `NV_ENC_PIC_PARAMS_HEVC::displayPOCSyntax` ("required to be set if
     client is handling the picture type decision") and `refPicFlag` were
     left zeroed on every frame — NVENC was very likely failing a POC
     monotonicity check on the first manually-typed P-frame. Fixed with a
     per-session POC counter, which also kept `request_keyframe()` (needed
     for packet-loss recovery) working in place for HEVC exactly like it
     already did for H.264 — no encoder rebuild, ~17ms.
  Confirmed live end to end with multiple real clients: resolution
  changes, bitrate reconnects, codec switching, and keyframe recovery all
  tested working.

- **Audio FEC implemented** (`redfog-moonlight/src/audio.rs`) — groups
  every 4 data packets into a Reed-Solomon block (2 parity shards, packet
  type 127) using the exact non-default parity matrix real clients expect
  (from moonlight-common-c's `RtpAudioQueue.c`). Requires constant-bitrate
  Opus (`redfog-core`'s audio pipeline now sets `bitrate-type=cbr` —
  Reed-Solomon needs every shard in a group to be the same length, which
  the default constrained-VBR doesn't guarantee). Confirmed correct via a
  round-trip reconstruction test, and confirmed live against moonlight-qt
  (clean audio, no regressions) — not yet tested against real induced
  packet loss specifically.

- **Video FEC implemented** (`redfog-moonlight/src/video.rs`) — every
  encoded access unit is its own Reed-Solomon FEC block: `FEC_PERCENTAGE`
  (20%) of its data shards' worth of parity shards, floored at
  `MIN_REQUIRED_FEC_PACKETS` (2, matching the value already advertised in
  RTSP ANNOUNCE's SDP). Unlike audio, uses the *standard* Reed-Solomon
  matrix (confirmed against moonlight-common-rust's own
  `create_video_reed_solomon` — no custom parity matrix needed for
  video). Single FEC block per frame only, same limitation
  moonlight-common-rust's own reference payloader has (no
  `VideoMultiFecBlocks` splitting for frames needing more than 255 total
  shards). Confirmed correct via a Reed-Solomon reconstruction round-trip
  test, and confirmed live — real playback verified working with FEC
  active. Not yet specifically tested against real induced packet loss
  (i.e. confirmed not to *break* anything, not yet confirmed to actually
  *recover* a dropped shard end to end against a real client).

- **`CudaDirectEncoderSession::reconfigure`'s same-resolution fast path
  disabled — was silently breaking moonlight-web-stream's HEVC decode on
  every takeover/resume.** `reconcile_video_pipeline` (`session.rs`) now
  always does a full pipeline rebuild (new capture connection, brand new
  `CudaDirectEncoderSession`) on every takeover/resume, even when nothing
  about width/height/fps/bitrate/codec changed at all — it no longer ever
  calls `reconfigure()`. Root-caused live: a same-resolution
  resume/takeover (the common case — reconnecting with identical or
  bitrate-only-different settings) left moonlight-web-stream's client
  permanently stuck waiting for an IDR frame that redfog-server had
  actually already sent (confirmed via keyframe-production/-send logging
  added specifically to check this — the server was demonstrably healthy,
  producing and sending real keyframes throughout). moonlight-qt was
  never affected by the same server-side behavior — ruling out a purely
  client-side (moonlight-web-stream) bug, since a real bitstream defect
  would affect any correctly-implemented decoder, not just one. Only
  reproduces on the pre-Ampere Vulkan-bridge import path
  (`vulkan_bridge.rs`'s detile-to-linear-buffer path, this dev machine's
  Turing RTX 2080), and is very likely the *same* root cause as the
  already-known `reconfigure_reuses_capture_connection` fd leak below —
  changing only resolution (which always fully rebuilt already) or doing
  a truly fresh spawn never showed the bug; same-resolution
  `reconfigure()` reliably did. Confirmed fixed live: HEVC takeover now
  works reliably against moonlight-web-stream after this change.
  `CudaDirectEncoderSession::reconfigure` itself is unused now (kept, in
  case its underlying leak/bug gets root-caused and the fast path is
  worth reinstating for its socket-leak-avoidance benefit — see the gap
  below) but nothing calls it from the live code path anymore.

## Fixed 2026-07-18 (older, for context — not TODO items)

- **CPU usage during high-fps/high-bitrate NVENC streaming: 34% → ~10% of
  a core.** Root-caused via non-invasive per-thread `/proc/<pid>/task/*/stat`
  sampling (no `perf` available; avoids `gdb`/`perf record`'s
  pause-the-target problem) at 1920x1080@120fps, ~37Mbps, against a live
  session with continuous full-frame damage (`glxgears`):
  1. Nearly all CPU was on `pipewiresrc`'s own streaming thread — because
     `make_encoder_pipeline` linked `pipewiresrc ! videorate` straight into
     the `videoconvert`/encoder downstream bin with no thread boundary, so
     everything downstream (format conversion, encoder submission) ran
     synchronously on PipeWire's own I/O callback thread. Added a `queue`
     (`leaky=downstream`, `max-size-buffers=2` — a pure threading boundary,
     not a latency buffer) between capture and convert/encode so this work
     can land on a different core. 34% → ~15%.
  2. Per-thread sampling after the `queue` fix showed the cost had just
     moved, unchanged in magnitude, onto the queue's own output thread —
     meaning it was never PipeWire I/O to begin with, it was `videoconvert`
     (forcing PipeWire's captured format to BGRx for `nvh264enc`). Checked
     `nvh264enc`'s actual sink caps (`gst-inspect-1.0 nvh264enc`): it
     accepts plain system-memory `video/x-raw` directly in a wide format
     list (confirmed live via `gst-launch-1.0` that negotiation still
     succeeds with no format forced), and does its own upload+colorspace
     handling on the GPU. Removed the `videoconvert`/forced-BGRx step
     entirely from `video_encoder_downstream_description`'s `Nvenc` arm —
     PipeWire's native captured format now goes straight into `nvh264enc`.
     ~15% → ~10%.
  3. Isolated the remaining ~10% with a synthetic `videotestsrc ! nvh264enc`
     pipeline (no PipeWire/KWin at all, same resolution/fps/bitrate): still
     ~24-29% of a core just to hand raw system-memory frames to `nvh264enc`
     at 1920x1080@120fps — i.e. this is NVENC's own host-to-device transfer
     cost (pageable system memory needs a CPU-driven staging copy; there's
     no way around *some* CPU cost here without genuinely GPU-resident
     source frames), not something in redfog's own code, and the live
     pipeline (~10%) is already doing *better* than this synthetic
     worst-case, likely thanks to PipeWire's own buffer pool. Tried an
     explicit `cudaupload` element ahead of `nvh264enc` in the synthetic
     pipeline as a further probe (28.7% → 23.2%, a real but modest
     improvement) — not worth the added pipeline complexity given it
     doesn't beat what the live pipeline already achieves.
  - **Remaining lever, not pursued here**: the fundamental reason frames
    exist in CPU/system memory at all is `LIBGL_ALWAYS_SOFTWARE=1`, forced
    unconditionally on every `kwin_wayland` spawn as a workaround for the
    known NVIDIA GBM segfault (see project memory). A real fix for *that*
    would let KWin render with the actual GPU and export DMA-BUF frames
    PipeWire could hand to `nvh264enc` with zero CPU-side copying at all —
    a much bigger, previously-parked investigation, not attempted here.

- PULSE_SERVER pointed at the wrong runtime dir + missing ACL grant
  (`redfog-broker/src/session.rs`).
- Audio sent as plaintext instead of the base-protocol-mandatory
  AES-128-CBC encryption (`redfog-moonlight/src/crypto.rs`, `audio.rs`).
- `HeadlessRuntime`'s PipeWire instance defaulted to the host's real ALSA
  sink instead of the per-session loopback sink
  (`redfog-core/src/lib.rs`'s `AudioLoopback::spawn`).
- Audio RTP timestamps used a 48kHz sample-rate clock instead of the
  milliseconds Moonlight's wire format actually expects.
- Audio packets were sent via a spawned task per packet with no ordering
  guarantee, risking reordering.
- Opus frames were encoded at 20ms instead of the 5ms the client
  hardcodes an assumption around, causing a deterministic
  silence-then-burst playback pattern.
- Hardware video encoding (NVENC via `nvh264enc`) implemented and wired
  in as `redfog_core::VideoEncoder`, auto-detected via GStreamer element
  factory lookup (`detect_video_encoder`) and overridable with
  `REDFOG_VIDEO_ENCODER=software|nvenc`. Verified live: auto-selected
  nvenc without any env var set, both Login and User generation video
  pipelines transitioned to Playing cleanly, no bus errors.
- Server-side adaptive bitrate. `control.rs` now parses `LossStats`
  (0x0201, base protocol — every real client sends it, not a Sunshine
  extension), which reports the frame index of the last frame the client
  fully received. `SessionManager::on_loss_stats` compares that against
  the frame number we've actually sent and steps the *running* encoder's
  `bitrate` property up/down accordingly
  (`redfog_core::set_encoder_bitrate` — live-settable on both `openh264enc`
  and `nvh264enc`, no pipeline rebuild or client cooperation needed).
  Heuristic multiplicative step down/up with a dead zone
  (`adapt_bitrate_kbps`, unit tested); never exceeds the configured
  `bitrate_kbps` ceiling. Not live-tested yet against an actually lossy
  network (only exercised via unit tests so far) — worth verifying with a
  real degraded connection, not just Docker-bridge/localhost.
- Observability: `spawn_session` now logs resolution/encoder/bitrate
  ceiling at INFO on every spawn; `on_loss_stats` logs every report at
  DEBUG (not just ones that change anything), so the adaptive loop's
  activity is actually visible instead of silent-unless-triggered.
- **Real bug, not just a gap**: the client's requested resolution was
  *never* applied, for every session, regardless of client settings.
  `pairing.rs`'s `/launch` handler read separate `width`/`height`/`fps`
  query params that no real client ever sends — real clients (confirmed
  against moonlight-common-rust's own launch-request builder) send one
  combined `mode=1920x1080x30` param instead, which nothing parsed. Every
  session silently ran at the hardcoded 1920x1080x60 default no matter
  what was actually requested. Fixed with a proper `mode=WxHxFPS` parser
  (`PairingServer::parse_mode`, unit tested against the real wire format,
  missing/malformed fallback, and explicitly *not* picking up the old
  broken separate keys if present).
- FPS cap. The client's requested fps (now correctly parsed, see above)
  is enforced ahead of the encoder — a real mechanism, not a hack:
  confirmed live by checking Wolf's own capture pipeline (games-on-
  whales' GameStream server, `~/src/gow-wolf/src/moonlight-server/
  streaming/streaming.cpp`), which forces an explicit `framerate={fps}/1`
  on its `waylanddisplaysrc` source for the exact same reason — a fixed
  bitrate budget divided across fewer frames means more bits, and better
  quality, per frame, especially under heavy motion. redfog's own
  `gst-backend` (the alternate, non-default `GstWaylandDisplay` backend)
  already did this too, just hardcoded at 30 — this brings the same
  capability to the default KWin path, driven by the client's actual
  request.
  First attempt used a `videorate max-rate={fps}` element and briefly
  shipped, then broke ALL streaming (not just resume) live, including a
  fresh Login spawn — root-caused via `GST_DEBUG=videorate:6` on a live
  connection_integration run to `videorate` locking up after exactly one
  output buffer despite a continuous, correctly-negotiated 30/1 input
  (the deep "why" inside `videorate` itself was never fully diagnosed,
  only the symptom); reverted immediately once confirmed via jj
  bisection. Replaced with a different, self-built mechanism: an always-
  present `identity name=fps_cap_gate` element in the encoder downstream
  bin (`redfog_core::video_encoder_downstream_description`) that
  `make_encoder_pipeline` optionally attaches a buffer-drop pad probe to
  (`install_fps_cap_probe`) — wall-clock (`Instant`)-based, stateless per
  buffer, deliberately NOT looking at buffer PTS/pipeline clock/segment
  to sidestep whatever internal state `videorate` got stuck on. `None`
  never attaches a probe at all — the gate element is a true no-op
  (`sync=false` passthrough), keeping fully dynamic/damage-driven capture
  byte-identical to pre-fps-cap behavior. `Some(fps)` only ever drops
  buffers arriving faster than `1/fps` apart; content updating slower
  than the cap, or a generously high requested fps on a fast local/LAN
  connection, passes through untouched. Unit tested including a live
  GStreamer `appsrc`/`identity`/`appsink` pipeline test that verifies
  both burst-throttling to exactly one buffer AND recovery/pass-through
  after waiting past the cap interval — the specific property that would
  have caught the `videorate` regression. Full `connection_integration`
  suite re-run after the rewrite: 9/11 pass, the only 2 failures being
  the pre-existing `gst_wayland_display_backend_smoke_test` (environment,
  see below) and the already-deferred resume-control-health flake —
  strictly better than the pre-fps-cap baseline (which had 3 resume
  tests failing), so the pad-probe approach did not reintroduce the
  regression.
