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

## Known active gaps (pre-existing, not new)

- [ ] `TerminateSession` is never called anywhere — abandoned sessions
      (network drop, client crash) leak rather than getting cleanly torn
      down. Real resource leak on long-running deployments.

## Deliberate deferrals (documented, not bugs — just not built yet)

- [ ] Gamepad/controller input. `control.rs` decodes keyboard + mouse
      only; every other input event type (including all gamepad packets)
      hits `_ => None` and is silently dropped.
- [ ] HDR, AV1. `<IsHdrSupported>0</IsHdrSupported>` is hardcoded; video
      is H.264 only.
- [ ] HiDPI passthrough. KWin's virtual output is spawned with
      `--scale 1` hardcoded; never scales.
- [ ] Live bitrate/resolution renegotiation. Fixed at `/launch`; would
      need a reconnect to change.
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
