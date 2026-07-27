# Concurrent Sessions — Planning Notes

Current state: `redfog-server` supports exactly one *actively streaming*
client at a time. A second client's `/launch` always displaces whatever was
active — for a `User` session, the underlying compositor/apps are preserved
(backgrounded, resumable later), but its *stream* is unconditionally cut off
from that point on. This document captures what we found while investigating
whether/how to support genuinely concurrent active clients, and is meant as
planning input, not a design decision yet.

## Why redfog is single-client today

- `SessionConfig::video_port`/`audio_port` are fixed, single values for the
  whole server (not per-session).
- `Shared` (the server's one piece of mutable session state) holds a single
  `video_sender: Option<Arc<VideoSender>>` / `audio_sender: Option<Arc<...>>`
  and a single `active_generation: Option<u64>` — only one "generation" is
  ever allowed to actually send data at a time. Every encoder callback checks
  its own captured generation against this before touching the sender at all
  (see `Shared::active_generation`'s doc comment in `session.rs` — this
  exists specifically to stop a zombie pipeline whose GStreamer-level
  teardown never finished from injecting frames into a newer session).
- `on_play` either binds a fresh `VideoSender`/`AudioSender` pair (replacing
  whatever was there) or "retakes" the existing one by repointing its learned
  client address to the new client — either way, the previous client stops
  receiving data.
- `background_sessions: Mutex<HashMap<String, RunningSession>>` preserves a
  `User` session's compositor/GStreamer pipelines (kept in `Playing` state,
  producing ~nothing while idle since capture is damage-driven) so a later
  login as the *same* user can resume it — but nothing about backgrounding
  keeps that session's *stream* alive to any client. It's paused from the
  client's perspective, not multiplexed.

## How Wolf (Games on Whales) actually does it

Initially assumed (from doc summaries) that Wolf allocates a separate
video/audio/control port *per session*. Checked the actual source
(`games-on-whales/wolf`, `stable` branch) — **that's wrong.** The real
mechanism:

- `state/data-structures.hpp`: `video_stream_port`/`audio_stream_port`/
  `control_stream_port` are fixed, global constants (48100/48200/47999),
  only overridable server-wide via env vars — same value for every session,
  not allocated per-client.
- `rtp/udp-ping.cpp`: **one shared UDP socket per port type**, bound once at
  server startup, for the whole server's lifetime. When any client's PING
  packet arrives, Wolf learns that packet's source `(client_ip, client_port)`
  and fires an event carrying that address *plus a handle to the same shared
  socket*. Each session's own pipeline then sends its encoded RTP data via
  `sendto()` on that *same shared socket*, addressed to its own remembered
  client address.

So concurrent sessions aren't separated by port at all — they're multiplexed
onto the same socket, distinguished purely by destination address per
outgoing packet (and source address for incoming pings/control). Nothing
about UDP requires a `connect()`'d 1:1 socket; `sendto`/`recvfrom` to
arbitrary peers on one bound socket is normal, unremarkable usage.

This matters for redfog: it means supporting concurrent clients does **not**
require the bigger change of per-session port allocation/binding. What it
does require: multiple sessions' encoder pipelines all producing frames
*concurrently* (not one active + N idle-backgrounded), and a send path that
can address an arbitrary/multiple destinations from shared sockets instead
of assuming a single learned peer.

## What would actually need to change in redfog

1. `Shared.video_sender`/`audio_sender` (currently a single `Option`) would
   need to become a collection: one learned destination address + packetizer
   state per concurrently-active session, all sharing the same bound socket.
2. `active_generation: Option<u64>` (single value, the "only one session may
   send" gate) would need to become a set of concurrently-active generations.
3. Backgrounded sessions currently stay idle (by design — capture is
   damage-driven, so an idle session costs ~nothing). Concurrent *active*
   streaming means N sessions all actually encoding at once, not idle.
4. Need to check whether `VideoSender`/`AudioSender`'s actual send call
   already supports addressing an arbitrary per-call destination, or
   whether it's built around a single fixed/learned peer (not yet checked).

## NVENC concurrent-session findings

Investigated after a live failure: a backgrounded `User` session (using the
new `VideoEncoder::NvencDirect` path — see `project_cuda_direct_nvenc`
memory) caused every subsequent Login-stage attempt (regular GStreamer
`nvh264enc`) to fail with `NvEncOpenEncodeSessionEx`/"Failed to open
session", for as long as the backgrounded session stayed alive.

Ruled out via direct, isolated testing (not just theorizing):
- **Not a hard concurrent-session cap in general** — confirmed live: 3
  concurrent `ffmpeg -c:v h264_nvenc` processes run fine simultaneously on
  this GPU/driver.
- **Not "two sessions in one process" in general** — a minimal isolated test
  (`crates/kwin-capture/tests/nvenc_two_sessions_same_process.rs`) opening
  two raw `nvidia-video-codec-sdk` sessions back-to-back in one process:
  works fine.
- **Not "our session + a real GStreamer `nvh264enc` session" in general
  either** — the same test file's second case (`nvenc_session_plus_
  gstreamer_nvh264enc`) opens our own session, then runs a real
  `videotestsrc ! nvh264enc ! fakesink` pipeline alongside it: also works
  fine (reaches EOS, no error).

So the live failure depends on something none of these isolated repros
capture — possibly the ~20 minutes of backgrounding before the failure, the
`CudaDirectEncoderSession` thread actively polling/encoding on an ongoing
basis (vs. the test's one-shot-then-idle), or something specific to the
actual Login-stage pipeline construction. **Immediate practical fix (Gemini,
already applied)**: the Login stage now always uses `VideoEncoder::Software`
(cheap, no GPU dependency) rather than falling back to GStreamer's
`nvh264enc` — this resolves the observed symptom regardless of root cause.

**Working theory for the deeper cause (Gemini)**, not yet independently
verified: multiple concurrent NVENC encoders are fine as long as they share
the same underlying CUDA context *family* — i.e., multiple concurrent
`Backend::Kwin` + `NvencDirect` sessions (all going through our own
`cuda_import`/`vulkan_bridge` path, all retaining the same primary CUDA
context) should be fine together, but *mixing* that path with
`Backend::GstWaylandDisplay` (which has its own, separate GStreamer/CUDA
context creation) is expected to conflict.

**Accepted constraint for now**: don't run one `Kwin+NvencDirect` session
and one `GstWaylandDisplay` session concurrently — treat that specific
combination as unsupported rather than something to debug as a surprise
later. All-same-backend concurrency (e.g. N concurrent `Kwin+NvencDirect`
sessions) is the more promising case to actually pursue, if/when we build
real multi-client support.

## Open questions before designing this for real

- Confirm `VideoSender`/`AudioSender`'s actual per-send addressing
  flexibility (item 4 above).
- Decide whether "concurrent" means multiple *different users'* sessions,
  multiple sessions for the *same* user, or both.
- Decide what happens to `background_sessions` semantics once backgrounding
  no longer needs to mean "not streaming" — is there still a reason to
  background a session at all if it could just keep streaming to whoever's
  still connected?
- Independently verify (rather than just accept) Gemini's same-CUDA-context
  theory before leaning on it for a real design, given how many of our own
  theories about the NVENC failure didn't survive isolated testing.
