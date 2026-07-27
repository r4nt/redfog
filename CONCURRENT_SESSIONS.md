# Concurrent Sessions

Status: **implemented** for the streaming/session-identity layer (see
"What was built" below). Still open: independently verifying the NVENC
same-CUDA-context-family concurrency assumption (see "NVENC concurrent-session
findings") — nothing currently exercises two simultaneously-active
`Kwin+NvencDirect` sessions in anger.

## Where this started

`redfog-server` used to support exactly one *actively streaming* client at a
time. A second client's `/launch` always displaced whatever was active — for
a `User` session, the underlying compositor/apps were preserved (backgrounded,
resumable later), but its *stream* was unconditionally cut off from that
point on. The rest of this document was originally planning notes written
before any of this was built; it's kept (updated) as the design record for
why the implementation looks the way it does.

## Design starting point: one shared socket, not per-session ports

The obvious-looking approach — allocate a separate video/audio/control port
per session — turns out not to be necessary, and isn't what redfog does.
Concurrent sessions don't need to be separated by port at all: they can be
multiplexed onto **one UDP socket per stream type, bound once at server
startup**, distinguished purely by destination address per outgoing packet
(and by whatever the incoming packet identifies itself with — see below).
Nothing about UDP requires a `connect()`'d 1:1 socket; `sendto`/`recvfrom` to
arbitrary peers on one bound socket is normal, unremarkable usage. This is
the shape `crate::udp_sender::MultiClientUdpSender` implements.

## Client identity: why plain IP wasn't enough

The first implementation pass keyed everything (session slots, video/audio
dispatch, control-channel decryption) by the connecting `client_ip`. That
broke down for a real, live scenario: two different clients (different
paired devices, or literally two windows on the same machine) sharing a
source address — same NAT, or the same box — would get conflated into one
slot, with the second `/launch` evicting the first mid-flight.

Investigating the fix required actually reading moonlight-common-rust's
client code (not just docs) to find out what's genuinely available on each
connection:

- **`/launch`, `/resume`, `/cancel`** (HTTP/HTTPS): real clients call these
  over HTTPS with their paired TLS client certificate presented. The
  fingerprint is a real, stable per-*device* identity — unlike IP, it
  correctly separates two devices sharing an address. Plain IP is only a
  fallback for non-HTTPS/standalone use.
- **RTSP** (`OPTIONS`/`DESCRIBE`/`SETUP`*/`ANNOUNCE`/`PLAY`): one TCP
  connection *per request* (confirmed against the client's own connection
  code), and `/launch`'s response contains nothing the client echoes back on
  these — no session token, no cert. Correlating an RTSP connection to a
  specific `/launch` call is therefore irreducibly IP+timing-based.
- **Video/audio `PING`**: there's an existing, already-supported wire-protocol
  extension for exactly this problem — the server hands back a 16-byte token
  in the `SETUP` response's `X-SS-Ping-Payload` header, and the client echoes
  it in every `PING` it sends thereafter instead of a bare 4-byte magic. This
  gives an unambiguous per-session key that survives address collisions, at
  no protocol-compatibility cost — redfog just wasn't using it before.
- **ENet control channel**: carries no address-independent identity at all
  from the transport itself, but every message is (once encryption is
  negotiated, which redfog always advertises support for) AES-128-GCM-tagged
  with the client's `rikey`. Trying an unmatched peer's first *encrypted*
  message against every currently-registered rikey and checking which one
  authenticates is a real cryptographic match, not just correlation — an
  attacker without the right rikey cannot produce a message that verifies,
  regardless of source address. (Unencrypted messages, e.g. base-protocol
  keepalives, don't even read the key they're "parsed" with, so they can't be
  used for this — see `control::is_encrypted_message`'s doc comment.)

## What was built

- **`ClientKey`** (`pairing.rs`): `Cert(fingerprint)` when `/launch` etc.
  arrived over HTTPS with a client cert (the real-client common case),
  `Ip(addr)` fallback otherwise. This is the key for `SessionManager`'s
  per-client session slots (`Shared::clients: HashMap<ClientKey, ClientSlot>`)
  — replacing the single `Shared.state`/`active_generation` slot entirely.
  A second client's `/launch` now only ever touches *its own* slot.
- **`SessionOrigin`** (`session.rs`): bundles everything that must (a) stay
  identical across a Login -> User handoff — rikey, RTP packetizer/timestamp
  state, adaptive bitrate, the per-launch `ping_token` — and (b) be freshly
  constructed only for a genuinely new `/launch`, never a handoff/resume
  within the same launch. Carried on `RunningSession` and threaded through
  `spawn_session` explicitly (the generation is minted by the caller, not
  internally, since Login needs it *before* spawning — see
  `REDFOG_LOGIN_GENERATION` below).
- **`crate::udp_sender::MultiClientUdpSender`**: one UDP socket per stream
  type, bound once at server startup, keyed by `ping_token` (not
  `client_ip`/generation — both were tried and found wanting, see the
  module's own doc comment for the two specific bugs each caused).
- **`control::ControlRegistry`**: one ENet host, keyed by registered rikeys;
  `ControlServer::serve` matches unmatched peers by decrypt-trial (see
  above) and caches the match per-peer afterward so only the *first* message
  from a new peer costs the O(sessions) trial.
- **`rtsp.rs`**: `SETUP` responses for audio/video (not control — it doesn't
  PING) include `X-SS-Ping-Payload`, resolved via
  `session::resolve_client_key_by_ip` (best-effort IP+timing, an inherent
  limitation — see above).
- **Login -> User correlation fix**: `LoginReportServer` is one Unix socket
  serving every concurrently-running `redfog-login` process. `handle_login_report`
  used to write into global cells with no way to tell whose report was
  whose; fixed by adding `generation: u64` to `LoginRequest::Authenticate`
  (reported back via `REDFOG_LOGIN_GENERATION`, set by
  `session_backend::spawn_login_compositor`), correlating each report to its
  own Login session (`SessionManager::pending_logins`).

Deliberately unresolved: correlating a bare RTSP connection back to a
specific `/launch` call is still IP+timing-based, since the client echoes
nothing else. With `ClientKey` and the ping-token/rikey-trial fixes above,
this only matters as a narrow race window at connection-establishment time —
it no longer determines a session's identity for its whole lifetime the way
plain IP-keying did.

## NVENC concurrent-session findings

Investigated after a live failure: a backgrounded `User` session (using the
`VideoEncoder::NvencDirect` path — see `project_cuda_direct_nvenc` memory)
caused every subsequent Login-stage attempt (regular GStreamer `nvh264enc`)
to fail with `NvEncOpenEncodeSessionEx`/"Failed to open session", for as long
as the backgrounded session stayed alive.

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
capture. **Practical fix already applied**: the Login stage always uses
`VideoEncoder::Software` (cheap, no GPU dependency) rather than falling back
to GStreamer's `nvh264enc` — this resolves the observed symptom regardless
of root cause.

**Working theory for the deeper cause**, not yet independently verified:
multiple concurrent NVENC encoders are fine as long as they share the same
underlying CUDA context *family* — i.e., multiple concurrent `Backend::Kwin`
+ `NvencDirect` sessions (all going through our own `cuda_import`/
`vulkan_bridge` path, all retaining the same primary CUDA context) should be
fine together, but *mixing* that path with `Backend::GstWaylandDisplay`
(which has its own, separate GStreamer/CUDA context creation) is expected to
conflict.

**Accepted constraint for now**: don't run one `Kwin+NvencDirect` session and
one `GstWaylandDisplay` session concurrently — treat that specific
combination as unsupported rather than something to debug as a surprise
later. Nothing in the code enforces this; it's an operational constraint,
not a technical one.

## Open questions

- **Independently verify** (rather than just accept) the same-CUDA-context
  theory above by actually running two simultaneously-active
  `Kwin+NvencDirect` sessions, before leaning on it for real — the
  streaming/identity layer this document otherwise describes is done and
  tested, but nothing has yet exercised two *concurrently encoding*
  `NvencDirect` sessions at once.
- Decide what happens to `background_sessions` semantics now that
  concurrent *active* streaming is possible — is there still a reason to
  background a session (vs. just leaving it streaming to whoever's still
  connected) now that "streaming" no longer implies "the only one"?
- Decide whether two concurrent logins as the *same* username (from two
  different `ClientKey`s) should be explicitly rejected/merged, or left as
  today (each spawns/attaches independently — no crash, since broker session
  IDs are always freshly minted, but two redundant desktop sessions for one
  account is probably never what's wanted).
