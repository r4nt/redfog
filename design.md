# Redfog: Multi-User Game Streaming Server

## Overview

Redfog is a remote desktop server purpose-built for gaming. It fuses the headless streaming approach of [Games on Whales](https://github.com/games-on-whales/gow) with the ergonomics of a traditional multi-seat remote desktop system, implemented in Rust and speaking the [Moonlight](https://moonlight-stream.org/) (GameStream/NVIDIA) protocol.

The key insight is that modern compositor stacks — specifically KDE's KWin — can render directly into a PipeWire stream with no physical display attached. Redfog exploits this to host fully isolated, GPU-accelerated game sessions per user, forwarding both video and audio over PipeWire before encoding and transmitting them via Moonlight.

---

## Goals

| Goal | Description |
|------|-------------|
| Headless operation | No real monitor required; virtual framebuffers are replaced by compositor-native PipeWire sinks |
| Multi-user concurrency | Multiple users can be logged in and gaming simultaneously, each in a fully isolated session |
| Low-latency streaming | Moonlight protocol with hardware-accelerated encode (VAAPI / NVENC / AMF) |
| Unified A/V capture | PipeWire as the single transport layer for both video frames and audio — no X11 capture, no DMA-BUF copies through intermediate Wayland layers |
| System login | A Moonlight-visible login screen lets users authenticate against local PAM accounts before a session is created |
| Rust implementation | Server logic written in Rust for safety, performance, and ease of async I/O |

## Non-Goals

- General-purpose remote desktop (no clipboard sync, file transfer, etc. in v1)
- Cloud / VM hosting (bare-metal or passthrough GPU assumed)
- Windows or macOS host support
- Re-implementing a compositor — KWin (or another PipeWire-capable compositor) is a runtime dependency

---

## Architecture

```
┌──────────────────────────────────────────────────────────────────────────────┐
│  Moonlight Client (PC / TV / phone)                                          │
└──────────────────────┬───────────────────────────────────────────────────────┘
                       │  Moonlight protocol (TCP/UDP)
                       ▼
┌──────────────────────────────────────────────────────────────────────────────┐
│  redfog-server  (Rust)                                                       │
│                                                                              │
│  ┌──────────────┐   ┌─────────────────────────────────────────────────────┐ │
│  │  Discovery   │   │  Session Manager                                    │ │
│  │  (mDNS/SSDP) │   │  - Auth gate (PAM)                                  │ │
│  └──────────────┘   │  - Session lifecycle (create / resume / destroy)    │ │
│                     │  - Per-user GPU/PipeWire node allocation             │ │
│                     └─────────────────────────────────────────────────────┘ │
│                                    │                                         │
│           ┌────────────────────────┴─────────────────────┐                  │
│           ▼                                               ▼                  │
│  ┌─────────────────────┐                    ┌─────────────────────┐         │
│  │  Login Session       │                    │  User Session N      │         │
│  │  (virtual KWin +     │                    │  (virtual KWin +     │         │
│  │   greeter UI)        │  ...               │   full DE / game)    │         │
│  └─────────┬───────────┘                    └──────────┬──────────┘         │
│            │ PipeWire video node                        │ PipeWire video node │
│            │ PipeWire audio node                        │ PipeWire audio node │
│            ▼                                            ▼                    │
│  ┌──────────────────────────────────────────────────────────────────────┐   │
│  │  Capture & Encode Pipeline (per session)                             │   │
│  │  PipeWire consumer → GStreamer / FFmpeg → H.264 / H.265 / AV1       │   │
│  └──────────────────────────────────────────────────────────────────────┘   │
│                                    │                                         │
│  ┌──────────────────────────────────────────────────────────────────────┐   │
│  │  Moonlight Stream Handler (per session)                              │   │
│  │  Control (RTSP-like) · Video (RTP/UDP) · Audio (RTP/UDP) · Input    │   │
│  └──────────────────────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────────────────┘
```

---

## Components

### 1. Discovery

Implements mDNS (`_nvstream._tcp`) and SSDP advertisement so that Moonlight clients find the server on the local network without manual IP entry, matching the behavior clients expect from an NVIDIA GameStream host.

### 2. Session Manager

The central authority for session state:

- **Authentication gate** — before a session is allocated, the client must pass PAM authentication. Redfog presents a lightweight login flow via the Moonlight pairing + PIN mechanism repurposed as a credential exchange, then validates credentials with PAM.
- **Session lifecycle** — creates, suspends, resumes, and tears down per-user compositor instances and their associated PipeWire graph nodes.
- **Resource allocation** — assigns GPU render nodes (`/dev/dri/renderD*`), PipeWire client namespaces, and network ports per session.

### 3. Login Session

When a client connects but has no active session, they land in a virtual login session:

- A minimal KWin instance (headless, PipeWire video backend) renders a greeter UI (e.g., a QML app or a lightweight Wayland greeter).
- The greeter captures credentials and hands them to the Session Manager over a local IPC channel.
- On success the client is seamlessly switched to their user session.
- The login session has no GPU-intensive workload; it can share resources with the server process.

### 4. User Sessions

Each authenticated user gets an isolated session:

- **Compositor** — a KWin instance launched under the user's UID in headless mode (`KWIN_PLATFORM=virtual` / `--platform=virtual`) with PipeWire video interception enabled. In this mode KWin exposes its output as a PipeWire video source node; redfog connects to that node as a consumer. No DRM/KMS, no virtual framebuffer device, no intermediate Wayland layers.
- **Audio** — a per-session PipeWire daemon (or virtual sink in the system PipeWire graph, scoped to the user's UID) captures game audio.
- **Isolation** — sessions do not share Wayland sockets, PipeWire graphs, or GPU contexts. A crashed game in one session does not affect others.
- **GPU** — games run on the real GPU via DRI render nodes. Multi-user concurrency relies on the GPU driver's ability to time-share (standard on modern AMD/Intel; NVIDIA requires MIG or vGPU on some cards).

### 5. Capture & Encode Pipeline

Per session, a PipeWire consumer reads frames from KWin's output node:

- **Zero-copy path** — DMA-BUF handles are passed from KWin → PipeWire → encoder without CPU copies where the driver supports it.
- **Encoder** — GStreamer or a custom Rust pipeline (via `gstreamer-rs`) drives VAAPI / NVENC / AMF for H.264, H.265, or AV1 depending on client negotiation.
- **Audio** — Opus encoding of the PipeWire audio sink output.

### 6. Moonlight Stream Handler

Implements the GameStream protocol spoken by all Moonlight clients:

- **Pairing** — PIN-based pairing with client certificate pinning.
- **Control channel** — RTSP-like TCP channel for session negotiation (resolution, codec, bitrate, frame rate).
- **Video channel** — RTP over UDP with FEC, IDR injection on packet loss.
- **Audio channel** — RTP over UDP (Opus).
- **Input channel** — keyboard, mouse, gamepad (XInput-compatible) events forwarded into the session's Wayland compositor via `uinput` or the compositor's input injection API.

---

## Rendering Pipeline & Performance

### Frame path

KWin renders using OpenGL/Vulkan on the GPU regardless of the virtual platform. Frames are produced in GPU memory as GBM/DRM buffers and exported as DMA-BUF file descriptors. PipeWire passes those fds to our consumer process; the hardware encoder (VAAPI / NVENC / AMF) imports them directly and encodes without touching CPU memory.

```
┌─────────────────────────────────────────────────────────────────┐
│ Normal (composited) path                                        │
│                                                                 │
│  Game (Vulkan/GL)                                               │
│    → GPU framebuffer                                            │
│    → wl_surface commit                                          │
│    → KWin composite blit  (GPU→GPU, negligible)                 │
│    → DMA-BUF fd via PipeWire                                    │
│    → hardware encoder import → bitstream                        │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ Direct scanout path  (fullscreen game, KWin 6.x)                │
│                                                                 │
│  Game (Vulkan/GL)                                               │
│    → GPU framebuffer                                            │
│    → DMA-BUF fd via PipeWire  (KWin composite skipped)         │
│    → hardware encoder import → bitstream                        │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ Fallback path  (DMA-BUF negotiation fails — must not happen)    │
│                                                                 │
│  Game → KWin composite (GPU) → CPU readback                     │
│    → PipeWire shared memory → CPU memcpy → encoder              │
└─────────────────────────────────────────────────────────────────┘
```

| Scenario | Extra GPU work | CPU copy |
|---|---|---|
| Fullscreen game, direct scanout | None | No |
| Windowed / composited | One KWin blit (GPU) | No |
| DMA-BUF negotiation failure | One KWin blit (GPU) | Yes — unacceptable |

### Direct scanout

KWin 6.x supports direct scanout / unredirection for fullscreen Wayland clients in virtual mode. When the game is the sole fullscreen client, KWin skips its own composite pass and hands the game's GPU buffer straight to PipeWire. This is the best-case path: the encoder receives the game's own framebuffer with zero intermediate copies or blits.

### DMA-BUF as a hard requirement

The shm fallback — where KWin reads back the GPU framebuffer to CPU memory — is not a graceful degradation; it is a throughput and latency cliff. Redfog must:

1. Negotiate DMA-BUF support explicitly during PipeWire stream setup.
2. Fail fast with a clear error if the driver or format negotiation rejects it, rather than silently falling back to the shm path.
3. Log the active path (DMA-BUF / shm) at session start so it is always observable.

DMA-BUF export works reliably on modern AMD (radeonsi/RADV) and Intel (iris/ANV) with Mesa. NVIDIA requires the open kernel driver (≥ 555) and may need `nvidia-drm.modeset=1`.

### NVIDIA + virtual backend (prototype finding)

Prototype testing on NVIDIA RTX 2080 (driver 610.43.02) revealed a blocking issue with `KWIN_PLATFORM=virtual`:

KWin's `GpuManager` scans `/dev/dri/` via udev and calls `RenderDevice::open()` → `DrmDevice::openWithAuthentication()` → `gbm_create_device()` to find GPU render devices. On NVIDIA proprietary, `gbm_create_device()` segfaults on `/dev/dri/renderD128` even though `nvidia-drm_gbm.so` is present at `/usr/lib/gbm/`. As a result, `GpuManager` finds no render devices, `VirtualBackend::supportedCompositors()` returns an empty list, and KWin falls back to software rendering. PipeWire stream caps negotiate to `BGRx` (SHM / CPU readback) rather than `DMA_DRM`.

Possible remediation paths (to be investigated):
- `nvidia-drm.modeset=1` kernel parameter — required for NVIDIA Wayland/DRM and may unblock GBM
- Switch the virtual backend to `EGL_PLATFORM_DEVICE_EXT` (NVIDIA's preferred headless EGL path) instead of `EGL_PLATFORM_GBM_KHR`
- Use `KWIN_PLATFORM=drm` with a virtual output, letting KWin acquire DRM master — requires being the sole compositor on the card

AMD and Intel (Mesa) are the primary targets for v1 due to full GBM and DMA-BUF support.

---

## Key Design Decisions

### PipeWire as the sole A/V transport layer

Rather than screen-capturing a running compositor with a separate tool (wf-recorder, OBS, etc.), redfog relies on KWin's native PipeWire output. This means:

- Frames never exist as a separate shared memory region; the compositor writes them directly into the PipeWire graph.
- Adding or removing a streaming client is a graph-link operation, not a process restart.
- Audio and video share the same session/UID scoping primitives, simplifying multi-user isolation.

### No virtual display device

GoW and similar projects often use a dummy EDID kernel module or a virtual GPU to trick compositors into thinking a monitor is connected. KWin's virtual platform backend eliminates this requirement entirely. Launching KWin with `KWIN_PLATFORM=virtual` (or `--platform=virtual`) starts the compositor in a headless mode that natively outputs into PipeWire. Redfog then connects to that PipeWire video node as a consumer. The approach is stable, upstream, and requires no kernel module management or privileged DRM access from the compositor.

### Moonlight over a custom Sunshine re-implementation

[Sunshine](https://github.com/LizardByte/Sunshine) already implements the Moonlight server side, but it is single-user and C++. Redfog re-implements the protocol in Rust to:

- Natively support concurrent sessions with async I/O (Tokio).
- Integrate tightly with the session lifecycle and PipeWire graph management.
- Avoid the subprocess/IPC overhead of wrapping Sunshine per user.

The Moonlight protocol is well-documented via the open-source client and Sunshine reference implementation.

### PAM for authentication

Local Unix accounts are the authentication source. PAM allows the server to reuse existing system credential stores (passwords, LDAP, Kerberos, YubiKey PAM modules, etc.) with no custom user database.

---

## Multi-User Concurrency Model

```
User A logs in  →  Session A created  (KWin-A, PW-graph-A, GPU ctx-A)
User B logs in  →  Session B created  (KWin-B, PW-graph-B, GPU ctx-B)

Both stream independently over separate Moonlight connections.
GPU driver time-shares between render contexts.
PipeWire graphs are isolated by UID / node ownership.
```

Session state is persisted across reconnects: if a client drops and reconnects, the compositor and game continue running; the new connection re-attaches to the existing PipeWire node.

---

## Input Injection & Production Privilege Model

### `fake_input`: a narrow, confirmed KWin bug that turned out not to matter here

The prototype forwards mouse/keyboard through KWin's `org_kde_kwin_fake_input` Wayland protocol (`pointer_motion`, `keyboard_key`, etc.) — simple to wire up since it's just another Wayland client request from the same process already managing the compositor session. Initial testing (spawning a compositor running a client that requests a real pointer lock via `zwp_pointer_constraints_v1`, then calling `fake_input.pointer_motion()` directly) found: **a native Wayland client holding an active pointer lock stops receiving `fake_input`'s relative motion entirely** — neither `zwp_relative_pointer_v1` nor plain `wl_pointer.motion` gets delivered. This looked like it would explain reports of mouse-look feeling slow in games.

**It doesn't, and the decision reverted.** The user pointed out that Portal (Source engine) played fine with `fake_input` — just slow, not frozen — which contradicts "zero events get through" if Portal used the same code path. It doesn't: Portal runs through XWayland, using SDL2's `SDL_SetRelativeMouseMode` (X11-style pointer grab), a completely different KWin code path from native Wayland's `zwp_pointer_constraints_v1`. Built a faithful test instead (`redfog-test-ux/examples/sdl2_relative_pointer_check.rs`, run through XWayland via `redfog-core/examples/sdl2_relative_pointer_direct_test.rs`): **all 50/50 `fake_input.pointer_motion()` calls arrived as perfectly 1:1 SDL2 relative-motion events with relative mouse mode active.** Combined with every other test run during this investigation (reference-client isolated/burst/sustained events, non-locked native Wayland) also showing clean 1:1 forwarding with no scaling, drops, or latency growth, `fake_input` has no confirmed problem on any path that actually matters for redfog. The pointer-lock bug is real, but specific to native Wayland clients — none of redfog's own compositor payloads (login screen, games via XWayland) hit it. (Separately, the user has patched a *different* known KWin mouse-lock bug — nested Wayland-in-Wayland sessions — which redfog doesn't hit either, since KWin here always runs directly via `--virtual`, never nested; mentioned here only as corroborating evidence that KWin has real, narrow bugs in this area, not that it's the same one.)

**Decision: keep `fake_input` as the input injection mechanism**, for both the prototype and production. The original "slow mouse" report that started this investigation remains unexplained by anything server-side and currently points at the reporting client's own machine/mouse hardware, not redfog.

### Future idea: `uinput` virtual devices, for a potentially nicer UX later

Not adopted now, but worth keeping on file: injecting input via a real `uinput` kernel virtual device (matching reference project Wolf's `inputtino::Mouse`) would make injected input indistinguishable from a real physical device to every layer above it (libinput, XWayland, anti-cheat/hardware-input checks some games perform, etc.), which `fake_input` — a compositor-specific synthetic-injection protocol — can never fully guarantee. If a game or edge case is ever found where this distinction matters, this is the fallback:

- `/dev/uinput` defaults to `root:root`, mode `0660` — needs a udev rule granting group access, the same pattern Wolf ships (`85-wolf.rules`): `KERNEL=="uinput", SUBSYSTEM=="misc", MODE="0660", GROUP="input", OPTIONS+="static_node=uinput"`. Whatever creates the device just needs `input` group membership, no root.
- Per-session isolation of concurrent sessions' virtual devices should use **Linux mount-namespace isolation** (the same primitive Docker gives Wolf "for free" via containers, but via systemd's native sandboxing): `PrivateDevices=yes` on the session's systemd unit gives it a private `/dev/` with no physical devices by default, then `BindPaths=/dev/input/eventN:/dev/input/eventN` bind-mounts in only that session's own uinput node. GPU render nodes get their own `BindPaths=` entry and can safely be shared read/write across sessions.
- Rejected for this: systemd-logind multi-seat (`loginctl attach`) — creating a new seat requires *at least one graphics card*, meaning a fabricated virtual DRM device (`vkms`) per concurrent session just to satisfy that rule, since KWin's `--virtual` backend never touches DRM/KMS at all. Confirmed KWin's input backend (`libinput_udev_assign_seat`, plus a "Failed to query Seat session property" string in `libkwin.so`) reads its seat from the logind session it's running under, which would also mean driving session creation through PAM/`pam_systemd` — avoidable complexity for no benefit over mount namespaces.

If this is ever revisited, note that `fake_input` needs none of the above at all — it's a plain per-compositor-socket Wayland protocol call, not a system-wide device, so each session already gets isolation for free from its own distinct Wayland socket. This machinery only becomes necessary if `uinput` actually gets adopted.

Adopting this later is purely additive to the broker design below, not a redesign: the broker gains `input` group membership plus the uinput device creation ioctls (a natural fit, since it's already the one small, privileged, audited component), and its existing `systemd-run --uid=` call just gains the `PrivateDevices=`/`BindPaths=` flags above. The privilege-separation model, the `systemd-run --uid=` mechanism, and everything else about the broker stays exactly as designed.

### Privilege separation: broker vs. server

This part is independent of the `fake_input`/`uinput` question above — it's driven by a separate requirement: one `redfog-server` process, dynamically switching which local user's session is being streamed, rather than a fixed per-user port/instance. `redfog-server` itself parses untrusted network input directly off the wire (HTTP/RTSP/ENet/TLS/pairing crypto) — it must never run as root; a parsing bug there would otherwise be a full root compromise. Spawning a compositor session as an arbitrary local user does need *some* privileged operation, so that operation is isolated into a small, separately-auditable **broker** component:

- `redfog-server` (unprivileged, dedicated service user) handles all protocol/network/video/audio logic, and talks to the broker over a narrow local IPC channel: essentially "start a session for user X" / "session ready at socket Y" / "terminate session for user X".
- The **broker** requests session startup via `systemd-run --uid=<target-user> ...` — letting systemd itself (PID 1, already root, already battle-tested) perform the actual privilege drop and set up `XDG_RUNTIME_DIR`/D-Bus session/logind registration correctly, the same way any `User=` systemd service gets it. With `fake_input` as the injection mechanism, no `PrivateDevices=`/`BindPaths=` device isolation is needed here — see above.

Critically, **the broker itself does not need to run as root.** `systemd-run`'s cross-user unit-start capability is gated by a single polkit action, `org.freedesktop.systemd1.manage-units` (default: `auth_admin`). A polkit rule (`/etc/polkit-1/rules.d/`) can grant this action specifically to a dedicated, unprivileged `redfog-broker` service user — and polkit rules can inspect call details, so the grant can be scoped further to only permit managing units matching a `redfog-session-*` naming pattern, not arbitrary system services. The broker's full privilege footprint ends up being just that narrowly-scoped polkit grant — no root, no `CAP_SETUID`, no special capabilities on the broker process itself. If the broker is ever compromised, the blast radius is "start/stop units named `redfog-session-*`," not "root."

```
Moonlight client
      │
      ▼
redfog-server (unprivileged service user)
  - HTTP/RTSP/ENet/pairing/TLS parsing
  - video/audio encode
  - IPC: "start session for user X" ──────────┐
                                               ▼
                                   redfog-broker (unprivileged, dedicated user)
                                     - polkit-authorized systemd-run --uid=X
                                               │
                                               ▼
                                   systemd (PID 1, root) — actual privilege drop,
                                   PAM session setup, XDG_RUNTIME_DIR, logind
                                               │
                                               ▼
                                   KWin (--virtual) as user X
                                     - input via fake_input (own Wayland socket,
                                       no device-level isolation needed)
```

### Authentication: a real graphical login screen, SDDM-style

The broker model raises an obvious question: if spawning a session as user X is itself the privileged, target-user-determining operation, how can a graphical login screen work at all — doesn't showing *any* UI require already knowing which user to spawn it as?

No — the same way SDDM's own greeter doesn't run as whichever user eventually logs in. SDDM's greeter runs under its own dedicated, unprivileged identity, purely to collect a username/password, which a privileged component then checks via PAM *before* the real session is spawned as that authenticated user. Redfog's `redfog-login` (already built, `crates/redfog-login`) fits this pattern directly, with no chicken-and-egg problem:

- The login compositor keeps running under `redfog-server`'s own unprivileged identity, exactly as it does today (`CompositorSession::spawn` + `fake_input`) — it was never spawned "as the target user" to begin with, so nothing about the broker model changes how it's launched.
- It doesn't need `uinput` or per-session device isolation either, even if that's ever adopted for the game session later (see "Future idea" above) — a login form is a plain GUI (absolute mouse, text entry), and `fake_input` is already proven perfectly linear for that case regardless.
- Today `redfog-login` fakes authentication (accepts any non-empty username/password, `main.rs`) and has no way to report which username was entered — `session.rs`'s `watch_login_exit()` only watches the process exit status. Three concrete changes turn this into the real thing:
  1. **`redfog-login` reports the submitted credentials** to `redfog-server` over a local channel (a Unix socket, path passed via env var — not stdout, so a password never risks ending up in a log) instead of faking success.
  2. **The actual PAM check lives in the broker, not in `redfog-login` or `redfog-server`.** Verifying a real Unix password (`pam_unix.so`) needs `/etc/shadow` access (root or `shadow` group) — that privilege has no business being granted to the GUI process rendering inside a network-streamed compositor, or to the network-facing `redfog-server`. The broker already is redfog's one small, audited, privileged component, so its job simply grows to: (a) `pam_authenticate` the submitted credentials, (b) on success, spawn the real "User" session as that username exactly as already designed above.
  3. **`redfog-server` is pure passthrough for credentials** — it forwards them from `redfog-login` to the broker and relays the broker's accept/reject back to the login UI (so it can show an error and let the user retry), without itself ever needing shadow access or doing anything privilege-sensitive with the password.
- The Login→User handoff state machine itself is unchanged — same video/audio/control re-pointing already built and tested. The only difference is *who* the target user is (whichever username the broker just authenticated, not implicitly "whoever's running the server") and *how* that session gets spawned (via the broker's `systemd-run --uid=`, not a direct `CompositorSession::spawn` under the server's own identity).

---

## Technology Stack

| Layer | Technology |
|-------|-----------|
| Language | Rust (async, Tokio runtime) |
| Streaming protocol | Moonlight / GameStream |
| Compositor | KWin (headless + PipeWire output plugin) |
| A/V graph | PipeWire (via `pipewire-rs`) |
| Encode | GStreamer (`gstreamer-rs`) · VAAPI / NVENC / AMF |
| Authentication | PAM (`pam` crate) |
| Discovery | mDNS (`mdns-sd`) + SSDP |
| IPC (server↔compositor) | D-Bus / Unix socket |
| Input injection | KWin `org_kde_kwin_fake_input` Wayland protocol; `uinput` virtual devices considered as a future option (see "Input Injection & Production Privilege Model") |

---

## Open Questions

1. **Multi-GPU assignment** — on systems with multiple GPUs, should sessions be round-robin assigned, or should users be able to request a specific GPU?
2. **NVIDIA vGPU** — NVIDIA's open-source driver stack does not yet support full concurrent multi-user GPU sharing without vGPU licensing. Document the limitation; AMD/Intel are the primary targets for v1.
3. **HDR** — Moonlight supports HDR; KWin's PipeWire backend HDR path is still maturing. Defer to v2.
4. **Greeter UI** — build a minimal custom Wayland greeter or integrate an existing one (e.g., `greetd` + `wlgreet`)?
