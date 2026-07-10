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

This part is independent of the `fake_input`/`uinput` question above — it's driven by a separate requirement: one `redfog-server` process, dynamically switching which local user's session is being streamed, rather than a fixed per-user port/instance. `redfog-server` itself parses untrusted network input directly off the wire (HTTP/RTSP/ENet/TLS/pairing crypto) — it must never run as root; a parsing bug there would otherwise be a full root compromise. Spawning a compositor session as an arbitrary local user does need *some* privileged operation, so that operation is isolated into a small, separately-auditable **broker** component (`crates/redfog-broker`, implemented):

- `redfog-server` (unprivileged) handles all protocol/network/video/audio logic, and talks to the broker over a narrow local IPC channel (`redfog-broker-protocol`, newline-delimited JSON over a Unix socket): `Authenticate { username, password }`, `SpawnSession { session_id, username, width, height, socket_name, payload }` → `SpawnedSession { wayland_socket_path }`, `TerminateSession { session_id }`.
- The **broker** runs as root and, per session, writes a templated `redfog-session-{id}.socket` + `.service` unit pair directly to `/run/systemd/system/` and drives them via `systemctl` — **not** `systemd-run --uid=`. `systemd-run` was the original plan, but per-session unit files give more control over exact ordering (see below) and let the broker clean up by deleting the files rather than needing separate transient-unit bookkeeping.
- **Important correction from the original plan**: a system unit with `User={username}` does **not** get PAM session setup, logind registration, or `systemd --user` integration "for free," the way an interactive login does. It's just a process running as a different uid — `XDG_RUNTIME_DIR` has to be set explicitly (we point it at a broker-managed directory, not the real `/run/user/<uid>`), there's no real logind session, and `systemd --user`-activated services (like `kglobalaccel`) are simply unreachable. See "Getting a real KDE session to actually behave like one" below for what this actually requires as a result.

Currently the broker's own privilege model is the simple version — it runs as root (via `sudo`/a root-owned service) rather than the originally-envisioned polkit-scoped unprivileged user. Narrowing that (a dedicated `redfog-broker` service user + a polkit rule scoping `org.freedesktop.systemd1.manage-unit-files`/`manage-units` to `redfog-session-*`-named units) is still the right target and hasn't changed shape, just hasn't been implemented yet — tracked as an open item.

```
Moonlight client
      │
      ▼
redfog-server (unprivileged)
  - HTTP/RTSP/ENet/pairing/TLS parsing
  - spawns/owns pipewire+wireplumber, encode pipeline, InputForwarder
  - IPC: Authenticate, then SpawnSession for user X ──────────┐
                                               ▼
                                   redfog-broker (currently root; polkit-scoped
                                   unprivileged user still an open item)
                                     - pam_authenticate(username, password)
                                     - writes redfog-session-{id}.socket/.service
                                       to /run/systemd/system/, systemctl start
                                     - chowns the session's runtime dir to X
                                     - grants X access to redfog-server's
                                       existing PipeWire socket (setfacl)
                                     - grants X rw on the Wayland socket file
                                       itself (SocketUser= only covers the
                                       broker's own user, not X)
                                               │
                                               ▼
                                   systemd (PID 1, root) — binds the Wayland
                                   socket per the .socket unit, starts the
                                   .service as user X, passes the pre-bound fd
                                   (LISTEN_FDS) — no PAM session, no logind,
                                   no systemd --user: just a uid switch
                                               │
                                               ▼
                                   dbus-run-session -- KWin (--virtual
                                   --wayland-fd) as user X, own private D-Bus
                                   bus (see below for why)
                                     - never calls bind() on the Wayland socket
                                     - connects out to redfog-server's PipeWire
                                       socket to publish its screencast stream
                                     - fake_input via that same Wayland socket
                                     - --exit-with-session runs a generated
                                       wrapper script, not the payload
                                       directly (see below)
```

### Cross-user socket reachability: PipeWire stays with us, Wayland via socket activation

Once KWin runs as target user X (via the broker) while `redfog-server`'s encode pipeline and `InputForwarder` stay in `redfog-server`'s own unprivileged process (as they do today — no new per-session helper process, no new data-plane IPC layer), those two need to keep reaching the compositor's PipeWire and Wayland sockets across that user boundary. The fix differs for each, because they're created by different things:

- **PipeWire socket**: `redfog-server` already spawns the `pipewire`/`wireplumber` daemons itself (`HeadlessRuntime::start()`) — KWin only ever connects to that socket as a client to publish its screencast stream, it never creates it. So there's no cross-user problem to solve here at all: keep running PipeWire/wireplumber under `redfog-server`'s own identity (it's a media-routing daemon, it never needs target-user X's file permissions for anything), and just grant X's KWin process access to connect in — a one-time `setfacl -m u:X:rw <path>` on a socket `redfog-server` already owns, which needs no special privilege since owners can always ACL their own files.
- **Wayland socket**: different, because KWin genuinely is the server for this protocol — whoever calls `bind()`/`listen()` owns the resulting socket inode. But KWin doesn't have to be the one calling `bind()`: `kwin_wayland --wayland-fd <fd>` accepts an already-bound, listening socket fd instead of creating its own. The clean way to get a pre-bound fd into a `systemd-run --uid=X`-spawned process (not a direct fork/exec from our own process, so plain fd-inheritance doesn't apply) is systemd's own socket activation: a `.socket` unit (templated per session) with `ListenStream=<path>`, `SocketUser=`/`SocketGroup=`/`SocketMode=` fully under our control, paired with the service that starts KWin as user X. Systemd binds the socket itself, with whatever permissions we specify, and hands it to the service via the standard `LISTEN_FDS` mechanism — the socket's ownership is entirely independent of which user the service ends up running as.

Net effect: no ACLs needed for the Wayland socket itself... in theory. In practice, three more cross-uid gaps turned up once this actually ran end to end, each with its own distinct, initially-confusing symptom:

- **Parent-directory traverse**: Unix requires *execute* permission on every path component to reach a socket file, not just rw on the socket itself. `HeadlessRuntime::start()` sets its runtime dir to `0700`, so without also granting the target user `x` there via `setfacl`, KWin's connection to `redfog-server`'s PipeWire socket failed at the kernel/filesystem level — before ever reaching PipeWire's own access-control code, which made it look like a PipeWire permissions problem at first.
- **The Wayland socket file itself**: `SocketUser=` in the `.socket` unit only grants the *broker's own* user rw — the target user still needs an explicit `setfacl -m u:X:rw` on the socket file once it exists (it only appears once the `.socket` unit is actually started, so this grant has to happen after that, not alongside the others). Without it, a client spawned *as the target user* (e.g. the session's own `--exit-with-session` app) gets `WaylandError(Connection(NoCompositor))` — while `redfog-server`'s own capture client, connecting as root, works fine on the very same socket, since root bypasses file permission checks entirely. That asymmetry (one client on a socket works, another doesn't) is the tell that it's a permissions problem, not a compositor-readiness race.
- **Write access on the session's own runtime directory**: granting traverse (`x`) via ACL is not the same as ownership. The directory is created by the broker (root) before the target user's uid is known to need anything there beyond traversal — but Xwayland needs to *create* its own EIS lockfile inside it (this directory is that user's `XDG_RUNTIME_DIR`). Without write access, that creation fails with `EACCES`, which libei reports as the misleading `Libeis: ... is another EIS running?` (a permission failure disguised as a locking conflict) — which makes Xwayland fail to start — which then hangs any client whose clipboard support falls back to X11 (e.g. `egui`/`arboard`), forever, waiting for a display that will never appear. Fixed by `chown`ing the session directory to the target user right after creating it, rather than another ACL grant.

### Getting a real KDE session to actually behave like one

Getting KWin to run and a Wayland client to connect is necessary but nowhere near sufficient for a *real* desktop session (`plasmashell`, not a test stand-in) to actually work. Confirmed live, in order of discovery:

- **A private D-Bus session bus is required, and `dbus-run-session` is how the direct-spawn path already got this for free.** A system unit with `User=X` has no D-Bus session bus of its own by default — it falls back to X's *real* D-Bus session bus (`/run/user/<uid>/bus`) if one exists, which already has a real `plasmashell` registered on `org.kde.plasmashell` if X has an actual desktop session running elsewhere. Confirmed live (twice): once when a test accidentally shared the real bus, and once when the real bus's `plasmashell` got outright replaced (killed) by `plasmashell --replace` targeting it directly. `redfog-server`'s own login-stage compositor never hit this because `ensure_private_dbus_session()` already wraps *redfog-server's entire process tree* in `dbus-run-session` at startup — but the broker's systemd unit is a wholly separate process tree that never goes through that, so its `ExecStart=` wraps `kwin_wayland` in its own `dbus-run-session` explicitly.
- **`dbus-update-activation-environment` is required, and is the actual root cause of "Plasma Shell renders nothing."** A D-Bus-exec-activated helper Plasma Shell hard-depends on (`kactivitymanagerd`) is spawned by the session's `dbus-daemon` itself — which only knows about `WAYLAND_DISPLAY` if something explicitly tells it via `dbus-update-activation-environment`. Nothing did, by default, so `kactivitymanagerd` fell back to Qt's X11/`xcb` platform plugin, failed (`could not connect to display`, empty `DISPLAY`), crashed, and Plasma Shell aborted its own startup outright (`Aborting shell load: the activity manager daemon is not running`) — a solid-black screen with a working, responsive compositor underneath (mouse/decoration-hover worked fine), which was the actual, narrow bug, not a `kglobalaccel`/`systemd --user` gap as first suspected (that theory was disproved: a real `systemd --user` unit, with genuine `systemd --user` activation available, hit the exact same crash). This exact step is also documented in [[project-prototype-pipeline]]'s "dbus-update-activation-environment placement" note from the original prototype — this is a known, necessary step, not a new discovery, just one that hadn't been carried over to the broker's own session-spawning path.
- **`--exit-with-session <value>` takes exactly one shell-parsed string, not a command + separately forwarded args.** Confirmed by reading `main_wayland.cpp`: the whole value is fed through `KShell::splitArgs()` to get program+args; anything appended after a separate `--` in the *systemd* `ExecStart=` line lands in KWin's own, unrelated `--applications-to-start` feature instead, never reaching the session payload's actual argv. (`plasmashell --no-respawn` had silently always run as bare `plasmashell` as a result.) Fixed by writing a small generated wrapper script (`{runtime_dir}/session-start.sh`, `chmod 755`) that runs `dbus-update-activation-environment` and then `exec`s the real, correctly-quoted payload command, and pointing `--exit-with-session` at that single script path — which also sidesteps three layers of nested quoting (systemd → KWin's shell-splitting → an inner shell) that trying to inline all of this on one `ExecStart=` line would otherwise require. This also conveniently gets "wait until the compositor is actually ready" for free, with no separate polling: KWin only invokes `--exit-with-session` once its own startup is complete.
- **Session-identity environment variables** (`XDG_SESSION_TYPE=wayland`, `XDG_CURRENT_DESKTOP=KDE`, `DESKTOP_SESSION=plasma`, `KDE_FULL_SESSION=true`, `KDE_SESSION_VERSION=6`, explicit `XDG_DATA_DIRS`/`XDG_CONFIG_DIRS`) are what a real login sets via PAM/session-launcher scripts and nothing here does by default — without them, Plasma Shell rendered but its taskbar couldn't resolve pinned app IDs (konsole, Steam, etc.) via KSycoca.
- **`WorkingDirectory=`** defaults to `/` for a systemd service unit, unlike a real interactive login (which starts in `$HOME`). Set explicitly — and *not* via the `%h` specifier, which was confirmed live to resolve against the service manager's own context (root, landing sessions in `/root`) rather than `User=` in a *system* unit; resolved instead via a plain `getent passwd <user>` lookup for the actual home directory.

**The properly "official" way to get all of this is PAM's session stack** — `pam_open_session()` running `pam_systemd.so` (real logind session registration: `XDG_RUNTIME_DIR`, device ACLs, `systemd --user` activation, all at once) followed by execing the desktop's actual session launcher (for Plasma Wayland, that literally is `/usr/bin/startplasma-wayland`, which is also what SDDM itself runs). Redfog can't adopt that wholesale: `startplasma-wayland` always starts the compositor itself as part of that same sequence (there's no "compositor already exists, just do the rest" mode — confirmed live via `strings`/`--help`, it manages its own `startplasmacompositor` unconditionally), which conflicts with the broker owning KWin's startup for privilege-separation reasons in the first place. So the gaps above are patched individually rather than inherited for free. A real PAM session (`pam_open_session`) for the target user, instead of just `pam_authenticate`, is a plausible deeper fix for some of this (notably `systemd --user` activation) — bigger architectural change, not yet attempted.

### Authentication: a real graphical login screen, SDDM-style — implemented

The broker model raises an obvious question: if spawning a session as user X is itself the privileged, target-user-determining operation, how can a graphical login screen work at all — doesn't showing *any* UI require already knowing which user to spawn it as?

No — the same way SDDM's own greeter doesn't run as whichever user eventually logs in. SDDM's greeter runs under its own dedicated, unprivileged identity, purely to collect a username/password, which a privileged component then checks via PAM *before* the real session is spawned as that authenticated user. Redfog's `redfog-login` (already built, `crates/redfog-login`) fits this pattern directly, with no chicken-and-egg problem:

- The login compositor keeps running under `redfog-server`'s own unprivileged identity, exactly as it does today (`CompositorSession::spawn` + `fake_input`) — it was never spawned "as the target user" to begin with, so nothing about the broker model changes how it's launched.
- It doesn't need `uinput` or per-session device isolation either, even if that's ever adopted for the game session later (see "Future idea" above) — a login form is a plain GUI (absolute mouse, text entry), and `fake_input` is already proven perfectly linear for that case regardless.
- **Implemented.** `redfog-login` reports the submitted credentials to `redfog-server` over a local Unix socket (`REDFOG_LOGIN_SOCKET`, path passed via env var — not stdout, so a password never risks ending up in a log), using a small dedicated wire protocol (`crates/redfog-login-protocol`: `LoginRequest::Authenticate { username, password }` → `LoginResponse::Authenticate(Result<(), String>)`) rather than reusing `redfog-broker-protocol` — `redfog-login` is a plain blocking `eframe` app with no reason to depend on tokio the way the broker/server do, so only the wire *types* are shared, and each side does its own line-based I/O.
  1. **`redfog-server`'s `login_report::LoginReportServer`** listens on that socket and forwards each `Authenticate` request to `SessionManager::handle_login_report()`.
  2. **The actual PAM check lives in the broker.** `handle_login_report()` calls the broker's real `Authenticate` (when a `broker_socket_path` is configured — standalone/no-broker use just requires a non-empty username, matching the old placeholder behavior) and, on success, remembers the username for the subsequent `SpawnSession` call, replacing the `"user"` placeholder used before this was wired up. `spawn_user_compositor()` still separately calls the broker's `Authenticate` again before `SpawnSession` (now with the real username instead of the placeholder) — slightly redundant when a real login just happened, but keeps exercising that code path directly, and is what the `FAKE_SPAWN` test path (which never talks to `redfog-login` at all) relies on, falling back to `"user"` when nothing reported in.
  3. **`redfog-login` shows the broker's real accept/reject** (clearing the password field and displaying the error on rejection, letting the user retry) rather than accepting any non-empty input. Enter-to-submit works from either field (`lost_focus()` + `Key::Enter`, since `egui`'s `TextEdit` doesn't submit on Enter by default).
- The Login→User handoff state machine itself is unchanged — same video/audio/control re-pointing already built and tested. The only difference is *who* the target user is (whichever username the broker just authenticated, not implicitly "whoever's running the server") and *how* that session gets spawned (via the broker's templated systemd units, not a direct `CompositorSession::spawn` under the server's own identity).

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
5. **Broker's own privilege level** — currently runs as root; narrowing to a dedicated unprivileged `redfog-broker` user plus a polkit rule scoping `org.freedesktop.systemd1.manage-unit-files`/`manage-units` to `redfog-session-*`-named units (the original design) hasn't been implemented yet.
6. **Real PAM session registration** — the broker currently only calls `pam_authenticate`; a full `pam_open_session()` for the target user would register a genuine logind session and likely provide `systemd --user` activation, proper device ACLs, etc. "for free," closing some of the gaps documented in "Getting a real KDE session to actually behave like one" more holistically than patching each one individually. Bigger architectural change, not yet attempted.
7. **Real desktop session polish** — confirmed working end-to-end (real credentials, real `plasmashell`, taskbar/app resolution, correct working directory), but not yet exhaustively tested: gamepad/controller apps, multi-monitor, audio device selection, session persistence across client disconnects for a *real* (not test-stand-in) user app.
