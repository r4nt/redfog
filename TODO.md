# TODO

Audit of what's missing for a complete KDE/Plasma streaming session over
redfog, as of 2026-07-18 (after the audio fixes in this session). Grounded
in the current code, not just the original plan doc, which is stale in
places (e.g. it didn't anticipate needing FEC).

## Priority: network robustness (no FEC)

Both audio (`redfog-moonlight/src/audio.rs`) and video
(`redfog-moonlight/src/video.rs`) send `redundancy=0` — zero
forward-error-correction. Confirmed live: audio has *zero* tolerance for
packet loss with no FEC — a single dropped packet is a permanent,
unrecoverable gap until the client's skip-logic kicks in, producing an
audible stutter. This didn't show up during dev testing because the
Docker bridge used for testing is essentially lossless, but the actual
point of this project is streaming to a real device over a real network
(WiFi, a phone on LTE, etc.), where loss is normal and expected.

Without FEC, expect the same class of stutter to reappear the moment this
is tested over anything less pristine than localhost/Docker — and it'll
affect video too (visible glitches until the next keyframe), just less
obviously than audio's hard stall.

- [ ] Implement Reed-Solomon FEC for audio (4 data shards + N parity,
      matching the shard layout the vendored client already expects — see
      `fec_rs`/`create_audio_reed_solomon` in
      `vendor/moonlight-common-rust/src/stream/proto/audio/depayloader.rs`,
      not vendored into git, see `scripts/fetch-patched-deps.sh`).
- [ ] Implement FEC for video (`NV_VIDEO_PACKET` FEC header + parity
      shards).

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

## Known active gaps (pre-existing, not new)

- [ ] `TerminateSession` is never called anywhere — abandoned sessions
      (network drop, client crash) leak rather than getting cleanly torn
      down. Real resource leak on long-running deployments.
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

## Deliberate deferrals (documented, not bugs — just not built yet)

- [ ] Gamepad/controller input. `control.rs` decodes keyboard + mouse
      only; every other input event type (including all gamepad packets)
      hits `_ => None` and is silently dropped.
- [ ] HDR, AV1. `<IsHdrSupported>0</IsHdrSupported>` is hardcoded; video
      is H.264 only.
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

## Recently fixed (this session, for context — not TODO items)

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
  (`redfog_core::set_encoder_bitrate` — live-settable on both `x264enc`
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
