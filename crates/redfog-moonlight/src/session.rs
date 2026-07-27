//! Login -> User `CompositorSession` handoff state machine, driven by RTSP
//! events. The RTSP-driven analogue of `viewer`'s `--mode handoff` winit-loop:
//! `/launch` spawns the Login compositor and streams it; once it exits
//! (login succeeded), we spawn the User compositor and repoint the video/
//! audio/input pipelines at it — same two-session dance, different trigger.
//!
//! Multiple physical clients can be concurrently attached and streaming —
//! `Shared::clients` holds one independent slot per `client_ip` (the
//! connecting peer's address, from `/launch`'s HTTP request — see
//! `LaunchHandler::launch`'s doc comment), each running its own instance of
//! this exact Login->User dance. Concurrency is scoped to *this*, though:
//! `background_sessions` (see below) is still global, keyed by username —
//! logging in as the same user (from any client) resumes that same
//! backgrounded session instead of starting a fresh one; logging in as a
//! different user (or from a different client_ip) just adds another
//! concurrently-active slot. Only an explicit "Log Out" on the login screen
//! actually terminates a session. See `CONCURRENT_SESSIONS.md` for the full
//! design writeup, including the deliberate restriction this does NOT lift:
//! mixing `Backend::Kwin`+`VideoEncoder::NvencDirect` and
//! `Backend::GstWaylandDisplay` concurrently is unverified and not
//! recommended, even though nothing here enforces it.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use redfog_core::{AudioLoopback, InputSink, SessionType};
pub use session_backend::Backend;
use session_backend::SpawnedCompositor;

use crate::audio::{AudioPacketizer, AudioSender};
use crate::control::{ControlEventHandler, ControlRegistry, InputEvent};
use crate::pairing::{ClientKey, LaunchHandler, RemoteInputKey};
use crate::rtsp::{AnnouncedParams, RtspHandler};
use crate::video::{VideoPacketizer, VideoSender};

pub struct SessionConfig {
    pub bind_addr: IpAddr,
    pub video_port: u16,
    pub audio_port: u16,
    /// Command to run for the Login stage streamed on `/launch`, before the
    /// user has authenticated (e.g. `["target/release/redfog-login"]`).
    /// Overridable so tests can swap in a purpose-built stand-in instead of
    /// the real login GUI.
    pub login_app: Vec<String>,
    /// Command to run for the real desktop session once login succeeds
    /// (e.g. `["plasmashell", "--no-respawn"]`, or `["sway"]` for the
    /// gst-wayland-display backend).
    pub user_app: Vec<String>,
    pub bitrate_kbps: u32,
    pub video_encoder: redfog_core::VideoEncoder,
    /// Logs every mouse input event (move/button/scroll) at `info` level
    /// when true — separate from `RUST_LOG=debug`, which also floods with
    /// per-frame video encoder logs. For diagnosing real-client mouse
    /// behavior (sensitivity, drops, event shape) without that noise.
    pub log_mouse_events: bool,
    /// Path to redfog-broker's Unix socket. When set, the User session
    /// (post-login) is spawned via the broker (`Authenticate` then
    /// `SpawnSession`/`SpawnPayload` depending on `backend`, see design.md's
    /// "Privilege separation: broker vs. server") instead of directly via
    /// `CompositorSession::spawn` — the production path, and what the
    /// integration test exercises with a fake-auth/force-spawn-user broker.
    /// `None` keeps today's direct-spawn behavior, for standalone use
    /// without a broker.
    pub broker_socket_path: Option<std::path::PathBuf>,
    pub backend: Backend,
    /// Operator-configured, named session options offered on the login
    /// screen (see `redfog_login_protocol::load_presets`) — what a
    /// `LoginRequest::Authenticate.session` name other than
    /// `"user-configured"` resolves against. Loaded once at startup;
    /// `redfog-login`'s own copy comes from independently reading the same
    /// file, not from this — see `load_presets`'s doc comment for why.
    pub session_presets: Vec<redfog_login_protocol::SessionPreset>,
}

// `Backend`/`SpawnedCompositor` themselves live in `session-backend` now —
// shared with `viewer`'s standalone debug tooling, see its doc comments for
// the ownership model of each variant and why `SpawnedCompositor::terminate`/
// `try_wait`/`video_source`/`input_sink` are as simple as they are.

/// Everything about a session that must (a) stay bit-for-bit identical
/// across a Login -> User handoff — the same wire-level RTSP session's
/// crypto key, RTP sequence/timestamp continuity, and adaptive-bitrate
/// state, just a different compositor underneath — and (b) be freshly
/// (re-)established only for a genuinely new `/launch`, never a handoff or
/// resume within the same launch. Built once per `/launch` (`SessionOrigin::
/// fresh`, in `SessionManager::launch`) and threaded into every
/// `spawn_session` call for that launch's lifetime, including
/// `handoff_to_user`'s — which reads it back out of the old Login
/// `RunningSession` (`.origin.clone()`) rather than constructing a fresh
/// one, and also overwrites a *resumed* background session's stale origin
/// with it (a resume is still a brand new wire-level RTSP session from the
/// client's perspective, even though the compositor underneath is old).
#[derive(Clone)]
struct SessionOrigin {
    /// The `Shared::clients` key this session lives under — carried forward
    /// across a Login -> User handoff same as everything else here, so the
    /// encoder callbacks' per-frame gating check (see `build_pipelines`)
    /// looks the right slot up regardless of which stage built them.
    client_key: ClientKey,
    /// The `/launch` HTTP peer's address — NOT the identity key for
    /// anything (see `ClientKey`'s doc comment for why plain IP can't tell
    /// two clients sharing an address apart), but still needed to
    /// best-effort-resolve which `ClientKey` slot an incoming RTSP
    /// connection belongs to (see `resolve_client_key_by_ip`), since RTSP
    /// itself carries no cert or client-echoed token at all.
    client_ip: IpAddr,
    /// Random per-launch token, sent to the client as `X-SS-Ping-Payload`
    /// in the RTSP `SETUP` response (see `rtsp.rs`) and echoed back in
    /// every video/audio `PING` it sends thereafter — the identity key for
    /// `video_sender`/`audio_sender`'s learned-address bookkeeping (see
    /// `crate::udp_sender`'s doc comment). Deliberately NOT `client_ip` or
    /// `RunningSession::generation`: unlike either of those, this is
    /// unambiguous even when two concurrent clients share a source address
    /// — an existing, already-supported wire-protocol mechanism for this,
    /// not a redfog invention (see moonlight-common-rust's
    /// `PingSenderConfig`). Always
    /// printable ASCII (see `SessionOrigin::fresh`), not fully arbitrary
    /// bytes: it round-trips through an RTSP header (`X-SS-Ping-Payload`)
    /// as a plain UTF-8 string on the wire (moonlight-common-rust reads it
    /// back via a bare `payload_str.as_bytes()`, no decoding step), and
    /// arbitrary bytes risk corrupting that text-based framing (embedded
    /// CR/LF, invalid UTF-8, ...).
    ping_token: [u8; 16],
    /// This launch's client-generated remote-input key — decrypts the ENet
    /// control channel (see `control::ControlRegistry`, matched by trying
    /// every registered rikey rather than by address) and encrypts audio
    /// (see `AudioPacketizer::packetize_encrypted`).
    rikey: [u8; 16],
    rikey_key_id: u32,
    video_packetizer: Arc<Mutex<VideoPacketizer>>,
    audio_packetizer: Arc<Mutex<AudioPacketizer>>,
    stream_start: Instant,
    /// Server-side adaptive bitrate ceiling/current — see
    /// `redfog_core::set_encoder_bitrate`'s doc comment.
    target_bitrate_kbps: u32,
    current_bitrate_kbps: u32,
}

impl SessionOrigin {
    /// Constructs brand-new continuity state for a genuinely new `/launch`
    /// — never call this for a handoff or resume within the same launch
    /// (see this type's own doc comment for why).
    fn fresh(client_key: ClientKey, client_ip: IpAddr, rikey: [u8; 16], rikey_key_id: u32, default_bitrate_kbps: u32) -> Self {
        Self {
            client_key,
            client_ip,
            ping_token: random_ping_token(),
            rikey,
            rikey_key_id,
            video_packetizer: Arc::new(Mutex::new(VideoPacketizer::new())),
            audio_packetizer: Arc::new(Mutex::new(AudioPacketizer::new())),
            stream_start: Instant::now(),
            target_bitrate_kbps: default_bitrate_kbps,
            current_bitrate_kbps: default_bitrate_kbps,
        }
    }
}

/// See `SessionOrigin::ping_token`'s doc comment for why this is restricted
/// to printable ASCII rather than `rand::random::<[u8; 16]>()`.
fn random_ping_token() -> [u8; 16] {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut token = [0u8; 16];
    for byte in &mut token {
        *byte = CHARSET[rand::random::<usize>() % CHARSET.len()];
    }
    token
}

struct RunningSession {
    kind: SessionType,
    width: u32,
    height: u32,
    /// The client's requested fps from `/launch`'s `mode=WxHxFPS` — carried
    /// across a Login->User handoff the same way `width`/`height` are (see
    /// `handoff_to_user`), so the same cap applies to both stages of one
    /// RTSP session. Used to build `Option<u32>` fps caps for
    /// `build_pipelines`/`make_encoder_pipeline` — see
    /// `redfog_core::make_encoder_pipeline`'s doc comment for why `None`
    /// (not just a very high number) is kept as a real, distinct option.
    fps: u32,
    /// `Option` purely so `Drop` (see below) and `discard_running_session`/
    /// `handoff_to_user`'s Login teardown can each take it out via
    /// `Option::take` — a plain field can't be partially moved out of a
    /// type that implements `Drop`. Always `Some` from construction until
    /// the moment one of those takes it; never observably `None` anywhere
    /// else, so every read site below just `.as_mut()`/`.as_ref()`s past it.
    compositor: Option<SpawnedCompositor>,
    input_forwarder: Box<dyn InputSink>,
    video_pipeline: gstreamer::Pipeline,
    audio_pipeline: gstreamer::Pipeline,
    /// `Some` only for `VideoEncoder::NvencDirect` — see `build_pipelines`.
    /// `video_pipeline` is an empty placeholder in that case; this is the
    /// real control surface (`request_keyframe`/`set_bitrate`). `Arc`'d so
    /// the same one-time post-start keyframe-forcing task pattern used for
    /// `video_pipeline` (a clone handed to a detached `tokio::spawn`) works
    /// here too.
    cuda_direct_session: Option<Arc<redfog_core::CudaDirectEncoderSession>>,
    _audio_loopback: AudioLoopback,
    /// The `session_id` this session was registered under with the broker
    /// (`SpawnSession` for Kwin, `SpawnPayload` for GstWaylandDisplay), if
    /// spawned via one at all — `None` for the Login stage (always spawned
    /// directly, never via the broker) and for a standalone (no-broker)
    /// User stage. `SpawnedCompositor::try_wait` can't see a broker-spawned
    /// session die (the actual child lives in the broker's process tree,
    /// not ours — its `kwin_process`/`payload_process` are `None` in that
    /// case by design), so this is what `watch_user_session_exit` queries
    /// the broker's `IsSessionAlive` with instead.
    broker_session_id: Option<String>,
    /// Identifies which "generation" of encoder callback this session's
    /// `video_pipeline`/`audio_pipeline` were built with — every encoder
    /// callback checks its own captured `generation` against whatever's
    /// currently in `Shared::clients` for `origin.client_ip` before sending
    /// anything, so a pipeline whose session is no longer the active one for
    /// that client — including one that's supposed to be dead but whose
    /// GStreamer-level teardown never actually completed — simply stops,
    /// regardless of whether its own teardown ever finishes. Assigned once,
    /// at `spawn_session` time, and never changes across a resume (resume
    /// reuses this same pipeline/session, not a fresh one — see
    /// `handoff_to_user`).
    generation: u64,
    /// Continuity/identity state — see `SessionOrigin`'s own doc comment.
    origin: SessionOrigin,
    /// The User stage's resolved backend/payload, if any (see
    /// `SelectedSession`'s doc comment) — `None` for the Login stage.
    /// Stored here (not read from a global by `spawn_gst_payload_in_
    /// background`) so a *different* client's concurrently-in-flight login
    /// can never race this session's own resolved config.
    selected_session: Option<SelectedSession>,
    /// Signals the bus-watcher threads `start_streaming` spawns for this
    /// session's *original* `video_pipeline`/`audio_pipeline` to stop.
    /// Confirmed live to matter: those threads run `bus.timed_pop_filtered`
    /// in a loop for as long as this flag stays `false`, and each holds its
    /// own clone of the pipeline — as long as that thread is alive, the
    /// pipeline's underlying GStreamer objects (and whatever internal
    /// worker threads its elements spun up, e.g. `x264enc`'s own encoding
    /// thread pool) stay alive too, *regardless* of `set_state(Null)`
    /// reporting success — a state-machine transition doesn't drop any
    /// references. Without this, every discarded/superseded generation's
    /// bus-watcher threads (and the whole pipeline object graph behind
    /// them) leaked forever: confirmed live, thread count climbed roughly
    /// linearly with total generations ever created (61 threads, 33 of
    /// them stray pad-task threads, after only 4 generations), with every
    /// session getting progressively slower as a result (CPU contention
    /// from every previous generation's still-alive pipeline). Set once, at
    /// `spawn_session` time; signalled from `discard_running_session` and
    /// `rebuild_for_resume` right before their respective pipelines are
    /// superseded.
    bus_watchers_stop: Arc<AtomicBool>,
}

/// Safety net, not the primary teardown path (that's still
/// `discard_running_session`/`background_or_discard`, which do a proper,
/// possibly-blocking `terminate()` on a detached thread — see their own
/// doc comments): confirmed live, a `RunningSession` silently going out of
/// scope with no explicit teardown at all (e.g. `HashMap::insert` quietly
/// dropping whatever value used to be at that key) leaked its entire
/// GStreamer video/audio pipeline — encoder state, PipeWire-mapped
/// buffers, all of it — forever, in this process's own memory. That's
/// exactly what an OOM incident traced back to: `redfog-server` itself
/// (not any compositor child process, which stayed small) had grown to
/// ~28GB resident. Before this `Drop` impl existed, `SpawnedCompositor`
/// had no `Drop` of its own either — cleanup only ever happened if some
/// call site remembered to explicitly invoke `terminate()`. Best-effort
/// and deliberately non-blocking (`kill_best_effort`, not `terminate`):
/// `Drop` can run at unpredictable points, and blocking here on a
/// possibly-wedged child (the same class of hang already confirmed for
/// the Login stage's reader-thread `join()`) would just move the hang
/// somewhere even harder to diagnose.
impl Drop for RunningSession {
    fn drop(&mut self) {
        use gstreamer::prelude::*;
        // Bounded, via `run_with_timeout` — not a bare call. This is a
        // safety-net path (the primary teardown is `discard_running_
        // session`, which already attempted the same transition), but a
        // second unbounded attempt against a genuinely wedged pipeline
        // would just hang this thread too, defeating the point of
        // bounding the first attempt at all.
        let video_pipeline = self.video_pipeline.clone();
        let _ = run_with_timeout(move || { let _ = video_pipeline.set_state(gstreamer::State::Null); }, Duration::from_secs(5));
        let audio_pipeline = self.audio_pipeline.clone();
        let _ = run_with_timeout(move || { let _ = audio_pipeline.set_state(gstreamer::State::Null); }, Duration::from_secs(5));
        if let Some(compositor) = self.compositor.as_mut() {
            compositor.kill_best_effort();
        }
    }
}

enum ClientState {
    /// Claimed by an in-flight `/launch` call (from this same `client_ip`)
    /// that's still spawning the compositor (a slow step: KWin startup,
    /// D-Bus activation, etc.) — a placeholder so a second, concurrent
    /// `/launch` from the *same* client (real clients retry `/launch` on
    /// their own if the first attempt is slow) can't also see this slot
    /// absent and race into `spawn_session()` at the same time. Without
    /// this, two racing spawns fight over the same KWin Wayland socket name
    /// and both die almost immediately — confirmed live: 9 leaked
    /// kwin_wayland/redfog-login process pairs from one retry storm.
    Spawning,
    /// Launched (compositor running) but RTSP hasn't reached PLAY yet.
    Launched { session: RunningSession },
    /// Streaming: encoder/sender pipelines are live.
    Streaming { session: RunningSession },
}

impl ClientState {
    fn session(&self) -> Option<&RunningSession> {
        match self {
            ClientState::Launched { session } | ClientState::Streaming { session } => Some(session),
            ClientState::Spawning => None,
        }
    }

    fn session_mut(&mut self) -> Option<&mut RunningSession> {
        match self {
            ClientState::Launched { session } | ClientState::Streaming { session } => Some(session),
            ClientState::Spawning => None,
        }
    }

    fn into_session(self) -> Option<RunningSession> {
        match self {
            ClientState::Launched { session } | ClientState::Streaming { session } => Some(session),
            ClientState::Spawning => None,
        }
    }
}

/// One physical client's (`client_ip`'s) whole Login->User lifecycle slot —
/// see `SessionOrigin`'s doc comment and this module's own doc comment for
/// how these coexist across concurrent clients. Absent from
/// `Shared::clients` == idle (nothing attached for that client).
struct ClientSlot {
    state: ClientState,
    /// The background tasks spawned alongside `SessionManager::video_sender`/
    /// `audio_sender` to log whether this client ever PINGs them (see
    /// `on_play`) — tracked here specifically so a superseding launch from
    /// the *same* client can `.abort()` them. Without this, confirmed live:
    /// each task holds its own `Arc<Video/AudioSender>` clone for as long as
    /// its 30s `wait_for_client` timeout runs, draining the learned address
    /// notwithstanding — so a client that reconnects within that window
    /// (routine, not a rare edge case) hits the *same* fixed video/audio UDP
    /// port still bound by the old, merely-abandoned task, fails to bind
    /// with "Address already in use", and — because `on_play` just logs
    /// that error and returns — never reaches `start_streaming` at all: this
    /// slot stays at `Launched` forever, so every input event is silently
    /// dropped (`on_input` only acts on `ClientState::Streaming`) and no
    /// video/audio ever flows, even though a real Login process is visibly
    /// running. This is what "no login screen" actually was.
    video_wait_task: Option<tokio::task::JoinHandle<()>>,
    audio_wait_task: Option<tokio::task::JoinHandle<()>>,
}

impl ClientSlot {
    fn spawning() -> Self {
        Self { state: ClientState::Spawning, video_wait_task: None, audio_wait_task: None }
    }
}

struct Shared {
    /// One entry per currently-attached physical client (see `ClientKey`'s
    /// doc comment for why this is the key, not plain IP) — see this
    /// module's own doc comment. Absence of an entry is that client's
    /// "Idle".
    clients: HashMap<ClientKey, ClientSlot>,
}

pub struct SessionManager {
    config: SessionConfig,
    shared: Mutex<Shared>,
    /// Bound once at server startup (not per-launch, not per-session) — see
    /// `crate::udp_sender`'s doc comment for why: one shared socket for the
    /// whole server's lifetime, with concurrent sessions distinguished by
    /// learned per-client destination address rather than by port.
    video_sender: Arc<VideoSender>,
    audio_sender: Arc<AudioSender>,
    /// Shared with `control::ControlServer` — one ENet host for the whole
    /// server's lifetime, concurrent sessions distinguished by their own
    /// registered rikey (see `control::ControlRegistry`'s doc comment)
    /// rather than a single shared key.
    control_registry: Arc<ControlRegistry>,
    /// Signaled whenever some `Shared::clients` entry transitions away from
    /// `ClientState::Spawning` — lets a concurrent `/launch` from the same
    /// client (real clients retry on their own if the first attempt is slow
    /// — KWin startup, D-Bus activation) wait for that spawn to finish and
    /// reconnect to it, instead of getting a hard error for a request that
    /// would otherwise have succeeded.
    spawn_done: Condvar,
    self_ref: OnceLock<Weak<SessionManager>>,
    /// Unique per-attempt id passed to the broker's `SpawnSession` — avoids
    /// systemd unit name collisions across successive launch/cancel cycles
    /// within the same `redfog-server` process lifetime.
    next_broker_session_id: AtomicU64,
    /// Source of `RunningSession::generation` values — see that field's own
    /// doc comment.
    next_generation: AtomicU64,
    /// What `handle_login_report` resolved for a given Login-stage
    /// `RunningSession::generation`, once `redfog-login`'s reported
    /// credentials pass the broker's `Authenticate` check — read (and
    /// removed) by `handoff_to_user` for that same generation once the
    /// Login stage exits. Keyed by generation (reported by `redfog-login`
    /// itself via `REDFOG_LOGIN_GENERATION` — see `LoginRequest::
    /// Authenticate`'s doc comment), not a bare global cell: with
    /// concurrent sessions, multiple `redfog-login` processes can be
    /// connected to `LoginReportServer` at once, and nothing else
    /// identifies which one a given report is for.
    pending_logins: Mutex<HashMap<u64, PendingLoginResult>>,
    /// User sessions that aren't the one currently attached to a live
    /// RTSP/video/audio stream, but are still alive — a disconnect (a fresh
    /// `/launch` replacing the active session, an explicit `/cancel`, or a
    /// closed client) backgrounds a `SessionType::User` session into here
    /// instead of killing it (see `background_or_discard`), and a later
    /// login as the same username (from any client) resumes it (see
    /// `handoff_to_user`) rather than starting a new one. Keyed by username,
    /// deliberately global (not per-client_ip) — at most one background/
    /// active session per user at a time, regardless of which physical
    /// client is attached to it. Never holds a `SessionType::Login` entry:
    /// the Login stage is stateless UI with nothing worth preserving, so
    /// it's always discarded outright.
    background_sessions: Mutex<HashMap<String, RunningSession>>,
}

/// What `handle_login_report` resolved for one Login-stage generation — see
/// `SessionManager::pending_logins`'s doc comment.
struct PendingLoginResult {
    username: String,
    selected: Option<SelectedSession>,
}

/// The User stage's backend/payload as resolved by `handle_login_report` —
/// either one of the two fixed presets (`config.user_app` unchanged, a
/// fixed default `desktop_name`, no `glx_vendor`), or read from the target
/// user's own `~/.config/redfog/session.toml` when they picked "Custom" on
/// the login screen (`BrokerRequest::ReadUserSessionConfig`). Either way,
/// by the time this exists it's a complete, concrete session — callers
/// never need to know which source it came from.
#[derive(Clone)]
struct SelectedSession {
    backend: Backend,
    user_app: Vec<String>,
    desktop_name: String,
    glx_vendor: Option<String>,
}

/// Tracks encoded video output over a rolling window and reports fps/kbps
/// at INFO on an interval, instead of one DEBUG line per frame — so
/// `RUST_LOG=info` (the normal default) shows whether the fps cap
/// negotiated at spawn (`spawn_session`'s startup log) is what's actually
/// being delivered, without either per-frame log volume or needing to
/// parse timestamps out of DEBUG output by hand.
struct EncodedFrameStats {
    window_start: Instant,
    frames: u32,
    bytes: u64,
}

impl EncodedFrameStats {
    const REPORT_INTERVAL: Duration = Duration::from_secs(5);

    fn new() -> Self {
        Self { window_start: Instant::now(), frames: 0, bytes: 0 }
    }

    /// Returns `Some((fps, kbps))` once `REPORT_INTERVAL` has elapsed since
    /// the window started, resetting it; `None` otherwise.
    fn record(&mut self, frame_bytes: usize) -> Option<(f64, f64)> {
        self.frames += 1;
        self.bytes += frame_bytes as u64;
        let elapsed = self.window_start.elapsed();
        if elapsed < Self::REPORT_INTERVAL {
            return None;
        }
        let secs = elapsed.as_secs_f64();
        let fps = self.frames as f64 / secs;
        let kbps = (self.bytes as f64 * 8.0 / 1000.0) / secs;
        *self = Self::new();
        Some((fps, kbps))
    }
}

impl SessionManager {
    /// Async (unlike everything else that just constructs and hands back an
    /// `Arc` synchronously): binding `video_sender`/`audio_sender` needs a
    /// running tokio runtime (see `crate::udp_sender::MultiClientUdpSender::
    /// bind`) and can fail (the port already in use), so this can too.
    pub async fn new(config: SessionConfig) -> Result<Arc<Self>, String> {
        let video_sender = Arc::new(VideoSender::bind(config.bind_addr, config.video_port).await?);
        let audio_sender = Arc::new(AudioSender::bind(config.bind_addr, config.audio_port).await?);
        let this = Arc::new(Self {
            config,
            video_sender,
            audio_sender,
            control_registry: Arc::new(ControlRegistry::new()),
            shared: Mutex::new(Shared { clients: HashMap::new() }),
            spawn_done: Condvar::new(),
            self_ref: OnceLock::new(),
            next_broker_session_id: AtomicU64::new(0),
            next_generation: AtomicU64::new(0),
            pending_logins: Mutex::new(HashMap::new()),
            background_sessions: Mutex::new(HashMap::new()),
        });
        let _ = this.self_ref.set(Arc::downgrade(&this));
        Ok(this)
    }

    /// Shared with `control::ControlServer` — see `SessionManager::
    /// control_registry`'s field doc comment.
    pub fn control_registry(&self) -> Arc<ControlRegistry> {
        self.control_registry.clone()
    }

    /// Validates credentials reported by `redfog-login` (see
    /// `crate::login_report`) via the broker's real PAM-backed
    /// `Authenticate`, then resolves `session_name` — either the literal
    /// `"user-configured"` sentinel (the login screen's "Custom" entry) or
    /// the `name` of one of `config.session_presets` — into a concrete
    /// [`SelectedSession`] for the subsequent User-stage spawn. The
    /// `"user-configured"` case round-trips through the broker's
    /// `ReadUserSessionConfig`, gated behind the `Authenticate` above
    /// having already succeeded (never used to decide what an
    /// unauthenticated party sees). Without a broker configured
    /// (standalone use), just requires a non-empty username, matching
    /// `redfog-login`'s original no-op placeholder behavior — the session
    /// is still resolved and remembered either way, though
    /// `"user-configured"` has no user to read a config for in that case
    /// and is rejected.
    pub async fn handle_login_report(&self, username: String, password: String, session_name: String, generation: u64) -> Result<(), String> {
        if let Some(broker_socket_path) = &self.config.broker_socket_path {
            use redfog_broker_protocol::{read_response, write_request, BrokerRequest, BrokerResponse};
            use tokio::io::BufReader;
            use tokio::net::UnixStream;

            let stream = UnixStream::connect(broker_socket_path)
                .await
                .map_err(|e| format!("failed to connect to broker at {broker_socket_path:?}: {e}"))?;
            let mut reader = BufReader::new(stream);
            write_request(&mut reader, &BrokerRequest::Authenticate { username: username.clone(), password })
                .await
                .map_err(|e| format!("failed to send Authenticate to broker: {e}"))?;
            match read_response(&mut reader).await.map_err(|e| format!("failed to read Authenticate response: {e}"))? {
                BrokerResponse::Authenticate(Ok(())) => {}
                BrokerResponse::Authenticate(Err(e)) => return Err(e),
                other => return Err(format!("unexpected broker response to Authenticate: {other:?}")),
            }
        } else if username.trim().is_empty() {
            return Err("Username cannot be empty".to_string());
        }

        let selected = if session_name == "user-configured" {
            let broker_socket_path = self
                .config
                .broker_socket_path
                .as_ref()
                .ok_or_else(|| "\"Custom\" requires a broker (no REDFOG_BROKER_SOCKET configured)".to_string())?;
            use redfog_broker_protocol::{read_response, write_request, BrokerRequest, BrokerResponse};
            use tokio::io::BufReader;
            use tokio::net::UnixStream;

            let stream = UnixStream::connect(broker_socket_path)
                .await
                .map_err(|e| format!("failed to connect to broker at {broker_socket_path:?}: {e}"))?;
            let mut reader = BufReader::new(stream);
            write_request(&mut reader, &BrokerRequest::ReadUserSessionConfig { username: username.clone() })
                .await
                .map_err(|e| format!("failed to send ReadUserSessionConfig to broker: {e}"))?;
            let user_config = match read_response(&mut reader).await.map_err(|e| format!("failed to read ReadUserSessionConfig response: {e}"))? {
                BrokerResponse::ReadUserSessionConfig(Ok(Some(config))) => config,
                BrokerResponse::ReadUserSessionConfig(Ok(None)) => {
                    return Err("no ~/.config/redfog/session.toml found — create one to use \"Custom\", or pick a preset session".to_string())
                }
                BrokerResponse::ReadUserSessionConfig(Err(e)) => return Err(e),
                other => return Err(format!("unexpected broker response to ReadUserSessionConfig: {other:?}")),
            };
            SelectedSession {
                backend: user_config.backend.parse()?,
                user_app: user_config.payload,
                desktop_name: user_config.desktop_name.unwrap_or_else(|| "sway".to_string()),
                glx_vendor: user_config.glx_vendor,
            }
        } else {
            let preset = self
                .config
                .session_presets
                .iter()
                .find(|p| p.name == session_name)
                .ok_or_else(|| format!("unknown session {session_name:?} (was the sessions config changed after the login screen started?)"))?;
            SelectedSession {
                backend: preset.backend.parse()?,
                user_app: preset.payload.clone(),
                desktop_name: preset.desktop_name.clone().unwrap_or_else(|| "sway".to_string()),
                glx_vendor: preset.glx_vendor.clone(),
            }
        };

        self.pending_logins.lock().unwrap().insert(generation, PendingLoginResult { username, selected: Some(selected) });
        Ok(())
    }

    /// Whether `username` currently has a running (possibly backgrounded)
    /// session — see `LoginRequest::CheckUsername`'s doc comment for why
    /// this needs no password. `shared.state` is deliberately not
    /// consulted: while the login screen asking this question is even on
    /// screen at all, whatever it's replacing has *already* been
    /// backgrounded into `background_sessions` (see `launch()`), so that
    /// map alone is authoritative for "is anyone's session running".
    pub fn handle_check_username(&self, username: &str) -> bool {
        self.background_sessions.lock().unwrap().contains_key(username)
    }

    /// Terminates `username`'s session outright — see
    /// `LoginRequest::LogOut`'s doc comment for how this differs from a
    /// plain disconnect/reconnect, which only backgrounds a session rather
    /// than ending it. Requires re-proving the password (not just having
    /// once authenticated from wherever the session originally started) —
    /// same reasoning `Authenticate` itself has: this session may well have
    /// been started from a completely different connection than the one
    /// asking to end it now.
    pub async fn handle_log_out(&self, username: String, password: String) -> Result<(), String> {
        let log_out_start = std::time::Instant::now();
        tracing::info!("handle_log_out({username}): starting");
        if let Some(broker_socket_path) = &self.config.broker_socket_path {
            use redfog_broker_protocol::{read_response, write_request, BrokerRequest, BrokerResponse};
            use tokio::io::BufReader;
            use tokio::net::UnixStream;

            let stream = UnixStream::connect(broker_socket_path)
                .await
                .map_err(|e| format!("failed to connect to broker at {broker_socket_path:?}: {e}"))?;
            let mut reader = BufReader::new(stream);
            write_request(&mut reader, &BrokerRequest::Authenticate { username: username.clone(), password })
                .await
                .map_err(|e| format!("failed to send Authenticate to broker: {e}"))?;
            match read_response(&mut reader).await.map_err(|e| format!("failed to read Authenticate response: {e}"))? {
                BrokerResponse::Authenticate(Ok(())) => {}
                BrokerResponse::Authenticate(Err(e)) => return Err(e),
                other => return Err(format!("unexpected broker response to Authenticate: {other:?}")),
            }
        } else if username.trim().is_empty() {
            return Err("Username cannot be empty".to_string());
        }

        // Virtually always found here: every `/launch` (including the one
        // that spawned the very login screen sending this request)
        // backgrounds whatever was previously attached before spawning
        // Login (see `launch()`). `shared.clients` is still checked as a
        // fallback for the standalone/no-broker edge case where nothing
        // else has run yet — scanned by username (not a fixed client_ip):
        // this request doesn't carry one, and the session being logged out
        // may be attached to a *different* physical client than whichever
        // one is asking (e.g. logging out a stale session from a fresh
        // login screen elsewhere).
        let from_background = self.background_sessions.lock().unwrap().remove(&username);
        let from_active = if from_background.is_some() {
            None
        } else {
            let mut shared = self.shared.lock().unwrap();
            let target_key = shared
                .clients
                .iter()
                .find_map(|(key, slot)| slot.state.session().filter(|s| matches!(&s.kind, SessionType::User(u) if *u == username)).map(|_| key.clone()));
            match target_key {
                Some(key) => {
                    let taken = take_active_session(&mut shared, &key);
                    if let Some(session) = &taken {
                        self.video_sender.drain_pending(session.origin.ping_token);
                        self.audio_sender.drain_pending(session.origin.ping_token);
                        self.control_registry.forget(session.origin.rikey);
                    }
                    taken
                }
                None => None,
            }
        };

        let Some(session) = from_background.or(from_active) else {
            return Err(format!("no running session found for user {username:?}"));
        };

        // Best-effort: the broker's own `IsSessionAlive` polling (see
        // `watch_user_session_exit`) would eventually notice and self-clean
        // its bookkeeping even if this fails, so a failure here logs a
        // warning rather than the whole log-out. Bounded — not a bare
        // `.await` — so that even if the broker's own `terminate()` hangs
        // for some reason not yet accounted for, this `LogOut` request
        // still returns to the caller instead of hanging forever itself.
        if let (Some(broker_socket_path), Some(broker_session_id)) = (&self.config.broker_socket_path, &session.broker_session_id) {
            use redfog_broker_protocol::{read_response, write_request, BrokerRequest, BrokerResponse};
            use tokio::io::BufReader;
            use tokio::net::UnixStream;
            tracing::info!("handle_log_out({username}): calling broker TerminateSession for broker_session_id={broker_session_id}");
            let broker_call_start = std::time::Instant::now();
            let outcome = tokio::time::timeout(Duration::from_secs(15), async {
                let stream = UnixStream::connect(broker_socket_path).await.map_err(|e| format!("failed to connect to broker: {e}"))?;
                let mut reader = BufReader::new(stream);
                write_request(&mut reader, &BrokerRequest::TerminateSession { session_id: broker_session_id.clone() })
                    .await
                    .map_err(|e| format!("failed to send TerminateSession to broker: {e}"))?;
                read_response(&mut reader).await.map_err(|e| format!("failed to read TerminateSession response: {e}"))
            })
            .await;
            match outcome {
                Ok(Ok(BrokerResponse::TerminateSession(Ok(())))) => {
                    tracing::info!("handle_log_out({username}): broker TerminateSession succeeded after {:?}", broker_call_start.elapsed())
                }
                Ok(Ok(BrokerResponse::TerminateSession(Err(e)))) => tracing::warn!("broker failed to terminate session for user {username}: {e}"),
                Ok(Ok(other)) => tracing::warn!("unexpected broker response to TerminateSession: {other:?}"),
                Ok(Err(e)) => tracing::warn!("TerminateSession round trip failed for user {username}: {e}"),
                Err(_) => tracing::error!(
                    "handle_log_out({username}): broker TerminateSession did not respond within 15s (waited {:?}) — the broker's own \
                     terminate() is presumably still stuck on something; proceeding with local cleanup rather than hanging this LogOut \
                     request forever",
                    broker_call_start.elapsed()
                ),
            }
        }
        // Detached — see `background_or_discard`'s doc comment on its own
        // `Login` branch for why a `discard_running_session` call must
        // never run inline on a task the caller (here, the `/log-out`
        // request itself) needs to return promptly.
        std::thread::spawn(move || discard_running_session(session));
        tracing::info!("handle_log_out({username}): done after {:?}", log_out_start.elapsed());
        Ok(())
    }

    /// An owned `Arc<Self>` for moving into spawned tasks — trait methods
    /// here take plain `&self` (so `SessionManager` stays usable as a
    /// trait object across `LaunchHandler`/`RtspHandler`/`ControlEventHandler`
    /// without relying on `Arc<Self>`-receiver methods on trait objects).
    fn arc_self(&self) -> Arc<Self> {
        self.self_ref.get().and_then(Weak::upgrade).expect("self_ref set in new()")
    }

    /// Spawns the Login compositor — always headless (see
    /// `session_backend::spawn_login_compositor`'s doc comment), never
    /// goes through the broker, since it doesn't need to run as any
    /// particular target user (see design.md's "Authentication: a real
    /// graphical login screen").
    fn spawn_login_compositor(&self, width: u32, height: u32, generation: u64) -> Result<SpawnedCompositor, String> {
        session_backend::spawn_login_compositor(&self.config.login_app, width, height, generation)
    }

    /// Acquires the User compositor — via the broker if configured
    /// (`Authenticate` then `SpawnSession`/`SpawnPayload` depending on
    /// `backend`, the production path), or directly otherwise (standalone
    /// use without a broker).
    /// Returns the compositor alongside the username it was actually
    /// spawned as — the caller must thread this same value into the
    /// `SessionType::User(...)` it builds around the result (see
    /// `handoff_to_user`), not re-derive or hardcode its own placeholder.
    /// Confirmed live: `handoff_to_user` used to pass a hardcoded
    /// `"user".to_string()` into `SessionType::User(...)` regardless of
    /// this method's own (correct) username resolution below — harmless
    /// for `Backend::Kwin` (its `SpawnSession` call already takes the real
    /// username as a direct parameter here, not through `SessionType`), but
    /// silently spawned the gst-wayland-display backend's nested payload as
    /// the literal account `"user"` instead of whoever actually logged in,
    /// since `spawn_gst_payload_in_background` reads the username back out
    /// of `RunningSession.kind` — a real, unrelated account, if one happens
    /// to exist on the target system, or a `SpawnPayload` failure otherwise.
    /// `reported`: what `handle_login_report` resolved for this login's own
    /// generation, if anything (see `SessionManager::pending_logins`'s doc
    /// comment) — passed in by the caller (`handoff_to_user`) rather than
    /// read from a global here, so a *different* client's concurrently
    /// in-flight login attempt can never race this one's own resolution.
    /// `None` (standalone use without a broker, or `redfog-test-ux`'s
    /// stand-in login stage in tests) falls back to the placeholder "user"
    /// and the old Authenticate-with-empty-password call, which is exactly
    /// what `REDFOG_BROKER_FAKE_AUTH` validates.
    async fn spawn_user_compositor(
        &self,
        width: u32,
        height: u32,
        fps: u32,
        reported: &Option<PendingLoginResult>,
    ) -> Result<(SpawnedCompositor, String, Option<String>), String> {
        let username = reported.as_ref().map(|r| r.username.clone()).unwrap_or_else(|| "user".to_string());
        // Chosen on the login screen itself — falls back to the server's
        // own startup default (config.backend/config.user_app) when
        // nothing was ever reported in, same reasoning as `username` above.
        let selected = reported.as_ref().and_then(|r| r.selected.clone());
        let backend = selected.as_ref().map(|s| s.backend).unwrap_or(self.config.backend);
        let user_app = selected.as_ref().map(|s| s.user_app.clone()).unwrap_or_else(|| self.config.user_app.clone());

        let Some(broker_socket_path) = &self.config.broker_socket_path else {
            return session_backend::spawn_user_compositor_direct(backend, &username, &user_app, width, height, fps).map(|c| (c, username, None));
        };

        // The one `session_id` used for this whole User-stage spawn attempt
        // — for `Backend::Kwin` it's registered with the broker right below
        // (`SpawnSession`); for `Backend::GstWaylandDisplay` the actual
        // broker registration (`SpawnPayload`) happens later, in
        // `spawn_gst_payload_in_background`, once the compositor's own
        // socket exists — see `RunningSession::broker_session_id`'s doc
        // comment for why the same id has to reach that later call too,
        // not a freshly generated one.
        let session_id = self.next_broker_session_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed).to_string();
        tracing::info!("spawn_user_compositor: calling broker SpawnSession for {username} (broker_session_id={session_id})");
        let broker_call_start = std::time::Instant::now();
        let result = session_backend::spawn_user_compositor_via_broker(
            backend,
            broker_socket_path,
            session_id.clone(),
            &username,
            "",
            reported.is_some(),
            &user_app,
            width,
            height,
            fps,
        )
        .await;
        tracing::info!(
            "spawn_user_compositor: broker SpawnSession for {username} (broker_session_id={session_id}) returned {} after {:?}",
            if result.is_ok() { "Ok" } else { "Err" },
            broker_call_start.elapsed()
        );
        result.map(|c| (c, username, Some(session_id)))
    }

    /// Builds a fresh encoder/audio pipeline pair around `compositor`'s
    /// video source and `audio_loopback` — split out of `spawn_session` so
    /// its packetizer/stream-start lookup logic (see the comment further
    /// down) is documented and testable in one place. Also used by
    /// `rebuild_for_resume` (below) to rebuild a backgrounded session's
    /// pipelines fresh before resuming it.
    ///
    /// HISTORY: resuming a backgrounded `Backend::Kwin` session used to just
    /// `Pause`/un-`Pause` its existing pipelines rather than calling this —
    /// an earlier attempt to rebuild fresh ones here instead (the same way
    /// KWin's resize handling rebuilds around a fresh `pipewiresrc`) was
    /// tried and reverted: confirmed live, tearing the *old* pipeline down
    /// to `Null` while the session was otherwise still alive crashed the
    /// `kwin_wayland` compositor process itself (its virtual backend
    /// exited with `WinitEventLoop(ExitFailure(1))`), not merely the
    /// GStreamer side. Simply cycling the existing pipeline `Playing ->
    /// Paused -> Playing` avoided that crash but still left the client
    /// stuck forever re-requesting an IDR frame that never arrived.
    ///
    /// Root-caused since (see `rebuild_for_resume`'s doc comment): reusing
    /// the same `pipewiresrc`/downstream elements is what's actually
    /// broken, not fully rebuilding — the earlier crash came from tearing
    /// the *old* pipeline down synchronously (in the same call that also
    /// tried to reconnect), not from rebuilding fresh ones as such.
    /// `rebuild_for_resume` avoids that by never waiting on the old
    /// pipeline's teardown at all.
    fn build_pipelines(
        &self,
        kind: &SessionType,
        compositor: &SpawnedCompositor,
        audio_loopback: &AudioLoopback,
        generation: u64,
        fps_cap: Option<u32>,
        client_key: ClientKey,
        initial_bitrate_kbps: u32,
    ) -> (gstreamer::Pipeline, gstreamer::Pipeline, Option<Arc<redfog_core::CudaDirectEncoderSession>>) {
        // GStreamer's appsink callbacks run on GStreamer's own streaming
        // threads, not tokio worker threads — `tokio::spawn` would panic
        // there ("no reactor running"). Capture a `Handle` (valid from any
        // thread) instead, since we're called from within an async context.
        let handle = tokio::runtime::Handle::current();

        let video_encoder = if matches!(kind, SessionType::Login) {
            redfog_core::VideoEncoder::Software
        } else {
            self.config.video_encoder
        };
        let this = self.arc_self();
        // Distinct per generation: `pipewiresrc`/`pipewiresink` share one
        // underlying PipeWire core/thread-loop across every element in the
        // process with the same client identity, so reusing one name across
        // generations means a single wedged (abandoned-on-timeout) pipeline
        // permanently poisons every later session's video/audio too —
        // confirmed live via matching mutex addresses across generations.
        let video_client_name = format!("redfog-video-gen-{generation}");
        let audio_client_name = format!("redfog-audio-gen-{generation}");
        let video_stats = Arc::new(Mutex::new(EncodedFrameStats::new()));
        let on_video_access_unit = {
            let handle = handle.clone();
            let this = this.clone();
            let video_stats = video_stats.clone();
            let client_key = client_key.clone();
            move |data: Vec<u8>, is_key_frame: bool| {
                tracing::debug!("video encoder produced {} bytes, key_frame={is_key_frame}", data.len());
                if let Some((fps, kbps)) = video_stats.lock().unwrap().record(data.len()) {
                    tracing::info!("video: {fps:.1} fps, {kbps:.0} kbps (generation={generation})");
                }
                // `origin` (packetizer/stream_start/etc) is looked up fresh
                // from `shared` on every frame, not captured once at build
                // time — a *resumed* background session's pipeline is
                // rebuilt fresh (see this method's caller) well after the
                // `/launch` that's resuming it already replaced its origin,
                // and this is what makes the rebuilt pipeline pick that up.
                // The `generation` check alongside it is `RunningSession::
                // generation`'s doc comment's whole point: a pipeline whose
                // session is no longer the active one for this client —
                // including one that's supposed to be dead but whose
                // GStreamer-level teardown never actually completed — must
                // not send at all, not just skip after the fact.
                let origin = {
                    let shared = this.shared.lock().unwrap();
                    let Some(session) = shared.clients.get(&client_key).and_then(|slot| slot.state.session()) else { return };
                    if session.generation != generation {
                        return;
                    }
                    session.origin.clone()
                };
                let sender = this.video_sender.clone();
                // RTP timestamps use a 90kHz clock (standard for video) —
                // derived from wall-clock time since streaming started
                // rather than a fixed per-frame increment, since frames
                // aren't encoded at a perfectly even interval.
                let rtp_timestamp = (origin.stream_start.elapsed().as_secs_f64() * 90_000.0) as u32;
                let shards = origin.video_packetizer.lock().unwrap().packetize(&data, is_key_frame, rtp_timestamp);
                let ping_token = origin.ping_token;
                handle.spawn(async move {
                    // Bounded, not a bare `.await` — a task holding this
                    // `sender` clone forever (if `send_shards` itself never
                    // returned) would pin the fixed video UDP port bound
                    // permanently, with no future session ever able to bind
                    // it again. Not the root cause of the "port never
                    // recovers after a resume hang" bug (that turned out to
                    // be `start_streaming`'s own unbounded blocking call —
                    // see its doc comment), but a real, separate hardening:
                    // nothing here should ever be able to hang forever.
                    match tokio::time::timeout(Duration::from_secs(2), sender.send_shards(ping_token, &shards)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => tracing::warn!("video send failed: {e}"),
                        Err(_) => tracing::warn!("video send timed out after 2s — dropping this frame rather than holding its sender open forever"),
                    }
                });
            }
        };

        // `NvencDirect` only ever applies to a real `KwinNativeDmaBuf`
        // source (the User/`Backend::Kwin` stage) — `video_source()` always
        // returns `VideoSource::Login` for the Login stage regardless of
        // the requested encoder (see `SpawnedCompositor::video_source`'s
        // `HeadlessLogin` arm), so the *same* server-wide `video_encoder`
        // config must still fall back to a real GStreamer encoder there.
        let source = compositor.video_source(Some(video_encoder));
        let mut cuda_direct_session = None;
        let video_pipeline = if video_encoder == redfog_core::VideoEncoder::NvencDirect
            && matches!(source, redfog_core::VideoSource::KwinNativeDmaBuf { .. })
        {
            let session = redfog_core::make_cuda_direct_encoder_session(source, initial_bitrate_kbps, on_video_access_unit);
            cuda_direct_session = Some(Arc::new(session));
            // `NvencDirect` has no GStreamer pipeline at all — this empty
            // placeholder exists only so the rest of this type's machinery
            // (Drop's `set_state(Null)`, `start_streaming`'s Playing-
            // transition, `request_keyframe`/`set_encoder_bitrate`, which are
            // all now no-ops for a pipeline with no named elements) has a
            // `gstreamer::Pipeline` to hold — see `cuda_direct_session`
            // above for the real control surface. `input_sink` never looks
            // at this either: `NvencDirect` is only ever paired with
            // `Backend::Kwin`, whose `input_sink` ignores the pipeline
            // argument entirely (see `SpawnedCompositor::input_sink`).
            gstreamer::Pipeline::new()
        } else {
            // Login (or any other non-`KwinNativeDmaBuf` source) can't use
            // `NvencDirect` — fall back to regular GStreamer NVENC for it,
            // same as if the server-wide config had been `Nvenc` all along.
            let pipeline_encoder =
                if video_encoder == redfog_core::VideoEncoder::NvencDirect { redfog_core::VideoEncoder::Nvenc } else { video_encoder };
            redfog_core::make_encoder_pipeline(
                source,
                &video_client_name,
                pipeline_encoder,
                fps_cap,
                initial_bitrate_kbps,
                on_video_access_unit,
            )
        };

        let audio_pipeline = redfog_core::make_audio_pipeline(audio_loopback, &audio_client_name, move |packet| {
            let origin = {
                let shared = this.shared.lock().unwrap();
                let Some(session) = shared.clients.get(&client_key).and_then(|slot| slot.state.session()) else { return };
                if session.generation != generation {
                    return;
                }
                session.origin.clone()
            };
            let sender = this.audio_sender.clone();
            let (key, key_id) = (origin.rikey, origin.rikey_key_id);
            // NOT a 48kHz sample-rate clock, despite that being the
            // textbook RTP/Opus answer (and what this line used to do) —
            // Moonlight's audio wire format is a genuine protocol deviation
            // here: the timestamp field is plain milliseconds. Confirmed
            // against moonlight-common-rust, which documents this directly
            // (`stream/audio.rs`: "Timestamps are in milliseconds") and
            // simulates it the same way for its C-bindings fallback
            // (incrementing by `frame_duration.as_millis()` per packet).
            // Sending 48000x too fast made the client's presentation clock
            // run ~48x ahead of real time, which it then had to "catch up"
            // to — confirmed live: audio played in silent-then-rushed-
            // garbled-burst cycles until this was fixed.
            let rtp_timestamp = origin.stream_start.elapsed().as_millis() as u32;
            let opus_packet = origin.audio_packetizer.lock().unwrap().packetize_encrypted(&packet, rtp_timestamp, &key, key_id);
            // `block_on`, deliberately NOT `handle.spawn` (unlike the video
            // callback above): spawned tasks have no ordering guarantee
            // relative to each other once handed to tokio's scheduler, so
            // under any scheduling jitter two packets could hit the wire
            // out of the order they were captured in. The client's audio
            // depayloader has zero tolerance for that — any packet arriving
            // with a sequence number lower than one it's already seen gets
            // dropped as permanently stale (no FEC to recover it, since we
            // send redundancy=0), which stalls its jitter buffer until
            // enough forward packets pile up to skip past the gap, then
            // dumps them all at once. Confirmed live: the client logged
            // exactly this ("Network dropped audio data (expected 990, but
            // received 991)") and audio played in silent-then-rushed-burst
            // cycles until sends were forced strictly in-order here.
            // GStreamer already calls `new_sample` serially on one thread
            // per pipeline, so blocking this thread on the send (bounded,
            // same as video's spawn-based timeout) is enough to guarantee
            // that ordering all the way onto the wire.
            let ping_token = origin.ping_token;
            handle.block_on(async move {
                match tokio::time::timeout(Duration::from_secs(2), sender.send_packet(ping_token, &opus_packet)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!("audio send failed: {e}"),
                    Err(_) => tracing::warn!("audio send timed out after 2s — dropping this packet rather than holding its sender open forever"),
                }
            });
        });

        (video_pipeline, audio_pipeline, cuda_direct_session)
    }

    /// Rebuilds a backgrounded session's screencast capture and video/audio
    /// pipelines fully fresh before resuming it — a no-op for backends
    /// other than `Backend::Kwin` (see `SpawnedCompositor::
    /// Rebuilds a backgrounded session's video/audio pipelines.
    ///
    /// Formerly, this rebuilt the capture session and GStreamer pipelines fresh
    /// to work around the "damage-source stall" (where KWin didn't send frames
    /// due to lack of damage, causing keyframe request hangs).
    ///
    /// However, this recreation required calling `new_stream` which leaked
    /// stream proxies, causing KWin's compositor to stall at 1 FPS on
    /// unconsumed buffers.
    ///
    /// Now, we keep the GStreamer pipelines running in the Playing state
    /// during backgrounding (see `background_or_discard`). Because the
    /// pipelines are never paused, they remain active, preventing both the
    /// damage-source stall (the encoder remains hot and ready) and the
    /// stream proxy leaks. Consequently, `rebuild_for_resume` is a no-op.
    async fn rebuild_for_resume(&self, background: RunningSession) -> Result<RunningSession, String> {
        Ok(background)
    }

    /// `generation` is minted by the caller (`launch()` for Login,
    /// `handoff_to_user()` for User), not here — Login needs it *before*
    /// this is even called, to pass to `spawn_login_compositor` as
    /// `REDFOG_LOGIN_GENERATION` (see `LoginRequest::Authenticate`'s doc
    /// comment). `origin` carries this launch's continuity state — see
    /// `SessionOrigin`'s doc comment for who constructs it and when.
    fn spawn_session(
        &self,
        kind: SessionType,
        width: u32,
        height: u32,
        fps: u32,
        compositor: SpawnedCompositor,
        broker_session_id: Option<String>,
        generation: u64,
        origin: SessionOrigin,
        selected_session: Option<SelectedSession>,
    ) -> Result<RunningSession, String> {
        // Not derived from compositor.socket_name(): for Backend::Kwin that
        // happens to already be unique per stage ("redfog-login-0" /
        // "redfog-user-0"), but for Backend::GstWaylandDisplay it's always
        // literally "wayland-1" (waylanddisplaysrc's own fixed socket name
        // — see spawn_gst_compositor) — using it here would collide the
        // Login and User stages' pw-loopback sink names during handoff,
        // since the old Login session isn't dropped (and its AudioLoopback
        // torn down) until after the new User one is already spawned.
        let audio_session_name = match &kind {
            SessionType::Login => "redfog-login-0".to_string(),
            SessionType::User(_) => "redfog-user-0".to_string(),
        };
        let audio_loopback = AudioLoopback::spawn(&audio_session_name)
            .map_err(|e| format!("failed to spawn audio loopback for {audio_session_name}: {e}"))?;
        // `fps == 0` (shouldn't happen from a real client — parse_mode's
        // fallback is 60 — but defensively) means uncapped, same as never
        // having requested a cap at all.
        let fps_cap = (fps > 0).then_some(fps);
        tracing::info!(
            "spawn_session({kind:?}, generation={generation}): {width}x{height}, encoder={:?}, bitrate_ceiling={}kbps, fps_cap={fps_cap:?}",
            self.config.video_encoder,
            self.config.bitrate_kbps,
        );
        let (video_pipeline, audio_pipeline, cuda_direct_session) =
            self.build_pipelines(&kind, &compositor, &audio_loopback, generation, fps_cap, origin.client_key.clone(), origin.target_bitrate_kbps);
        // Must come *after* `build_pipelines`, not before (this used to be
        // the other way around): `SpawnedCompositor::GstWaylandDisplay`'s
        // `input_sink` now looks up its `waylanddisplaysrc` element inside
        // the just-built `video_pipeline` (see `session_backend::
        // SpawnedCompositor::input_sink`'s doc comment) rather than holding
        // a pre-built element from before any pipeline existed.
        let input_forwarder = compositor.input_sink(Some(&video_pipeline))?;

        Ok(RunningSession {
            kind,
            width,
            height,
            fps,
            compositor: Some(compositor),
            input_forwarder,
            video_pipeline,
            audio_pipeline,
            cuda_direct_session,
            _audio_loopback: audio_loopback,
            broker_session_id,
            generation,
            origin,
            selected_session,
            bus_watchers_stop: Arc::new(AtomicBool::new(false)),
        })
    }

    /// For `Backend::GstWaylandDisplay`: fires a background task that waits
    /// for the compositor's Wayland socket to actually exist (it doesn't
    /// until the pipeline built around it reaches Playing — see
    /// `spawn_gst_compositor`'s doc comment) and then spawns the nested
    /// payload — directly if this is the Login stage or a standalone
    /// (no-broker) User stage, via the broker's `SpawnPayload` otherwise.
    /// No-op for `Backend::Kwin` (its payload is already running by the
    /// time this is called, via KWin's own `--exit-with-session`).
    ///
    /// A background task rather than something `start_streaming` awaits
    /// inline: this dates from when `RunningSession`/`SpawnedCompositor::
    /// GstWaylandDisplay` held a `gstreamer::Element` directly (no longer
    /// true — see `VideoSource`'s doc comment in `redfog-core`), and
    /// something about that combination — confirmed live,
    /// `SpawnedCompositor: Send` and `SessionManager: Sync` both held in
    /// isolation, yet an `async fn` taking `&self` and an owned
    /// `RunningSession` still didn't — defeated `tokio::spawn`'s `Send`
    /// bound once threaded through `watch_login_exit`. Firing a fresh,
    /// independent task here sidestepped it entirely; kept as the
    /// established pattern (the task reaches back into `self.shared` under
    /// its own lock once done, exactly like `watch_login_exit` already
    /// does, rather than holding any owned/borrowed session state across an
    /// await itself.
    fn spawn_gst_payload_in_background(
        &self,
        kind: SessionType,
        runtime_dir: String,
        socket_path: PathBuf,
        socket_name: String,
        broker_session_id: Option<String>,
        selected: Option<SelectedSession>,
        client_key: ClientKey,
    ) {
        let this = self.arc_self();
        tokio::spawn(async move {
            // Login always uses config.login_app / the default nested
            // config — there's no selection to honor yet at that stage
            // (see spawn_login_compositor's doc comment). The User stage
            // uses whatever handle_login_report resolved for this session
            // (see `RunningSession::selected_session`'s doc comment), presets
            // and "Custom" alike — see SelectedSession's doc comment.
            let (nested_config, username) = match &kind {
                SessionType::Login => (session_backend::NestedSessionConfig { command: this.config.login_app.clone(), ..Default::default() }, None),
                SessionType::User(username) => {
                    let nested_config = match selected {
                        Some(s) => session_backend::NestedSessionConfig { command: s.user_app, desktop_name: s.desktop_name, glx_vendor: s.glx_vendor },
                        None => session_backend::NestedSessionConfig { command: this.config.user_app.clone(), ..Default::default() },
                    };
                    (nested_config, Some(username.clone()))
                }
            };

            // Login always spawns directly (never via broker, see
            // spawn_login_compositor's doc comment) — as does a standalone
            // User stage with no broker configured at all. Only a
            // broker-configured User stage goes through SpawnPayload, using
            // the same `session_id` `spawn_user_compositor` already
            // generated for this same spawn attempt (see
            // `RunningSession::broker_session_id`'s doc comment for why it
            // has to be that one, not a fresh one minted here).
            let broker = match (&this.config.broker_socket_path, &username, &broker_session_id) {
                (Some(broker_socket_path), Some(username), Some(session_id)) => {
                    Some((broker_socket_path.as_path(), session_id.clone(), username.clone()))
                }
                _ => None,
            };

            let spawned_child = match session_backend::spawn_gst_payload(&runtime_dir, &socket_path, &socket_name, &nested_config, broker, Duration::from_secs(10)).await {
                Ok(child) => child,
                Err(e) => {
                    tracing::error!("failed to spawn gst-wayland-display payload: {e}");
                    return;
                }
            };

            let mut shared = this.shared.lock().unwrap();
            if let Some(ClientState::Streaming { session }) = shared.clients.get_mut(&client_key).map(|slot| &mut slot.state) {
                if session.kind == kind {
                    if let Some(SpawnedCompositor::GstWaylandDisplay { payload_process, .. }) = session.compositor.as_mut() {
                        *payload_process = spawned_child;
                    }
                }
            }
        });
    }

    /// Bring the encoder/audio pipelines to PLAYING and, unless `is_resume`
    /// (see below), spawn the nested payload for `Backend::GstWaylandDisplay`
    /// (a no-op for `Backend::Kwin` — its payload is already running), the
    /// pipeline bus-message watcher threads, and the background task that
    /// watches this session for exit — `watch_login_exit` for the Login
    /// stage (hands off to the User stage on success), or
    /// `watch_user_session_exit` for the User stage (resets to `Idle` on
    /// death, since there's no next stage to hand off to).
    ///
    /// `is_resume` is true when `session` came from `background_sessions`
    /// (see `handoff_to_user`) rather than a fresh `spawn_session` — always
    /// false for the Login stage, which is never backgrounded/resumed. All
    /// three of the one-time steps above must be skipped for a resume:
    /// the nested payload/compositor is already running (spawning another
    /// would either fail outright or produce a second, conflicting client
    /// on the same socket), and the bus-watcher/exit-watcher tasks spawned
    /// the *first* time this session was started are still alive and still
    /// watching it (the exit-watcher keeps tracking a backgrounded session
    /// across the whole background/resume cycle — see its own doc comment)
    /// — spawning a second set would just duplicate them.
    ///
    /// The `Playing` transition itself runs on tokio's dedicated blocking-
    /// thread pool, never directly on whatever task calls this — confirmed
    /// live (via a dedicated integration test, not guesswork) to matter: for
    /// a resumed session this exact call can hang against the same wedged
    /// KWin/PipeWire negotiation `handoff_to_user`'s doc comment already
    /// describes, and running it inline meant `handoff_to_user` itself could
    /// never return, leaving `shared.state` stuck at `Idle` (set at its own
    /// top) forever. That, in turn, made the *next* `/launch`'s `was_idle`
    /// branch — reasonably assuming "nothing was attached, so no stale peer
    /// needs sweeping" — skip disconnecting the previous connection's still-
    /// alive ENet peer, which kept sending messages encrypted with a now-
    /// stale rikey. Those messages fail GCM authentication forever, which is
    /// what a live report of "the login screen doesn't respond to input"
    /// after a resume+reconnect actually traced back to.
    ///
    /// Returns an explicitly boxed future (`Pin<Box<dyn Future + Send>>`)
    /// rather than being a plain `async fn` — this method, `handoff_to_user`,
    /// and `watch_login_exit` call each other in a genuine cycle (this
    /// spawns `watch_login_exit`, which awaits `handoff_to_user`, which
    /// spawns this), and with all three as ordinary `async fn`s on the same
    /// `impl` block, `cargo build` reports `error[E0391]: cycle detected`
    /// trying to resolve their opaque return types against each other for
    /// `Send` — not just a "future is not `Send`" diagnostic, and not fixable
    /// by boxing at the call sites alone (confirmed live: that still expands
    /// this method's own opaque type as part of checking the boxed block's
    /// `Send`-ness). Erasing the type here, at the definition, is what
    /// actually breaks the cycle: callers just see a concrete, already-boxed
    /// type instead of an opaque one needing recursive inference.
    fn start_streaming(
        &self,
        session: RunningSession,
        is_resume: bool,
        video_wait_task: Option<tokio::task::JoinHandle<()>>,
        audio_wait_task: Option<tokio::task::JoinHandle<()>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            use gstreamer::prelude::*;
            let generation = session.generation;
            let client_key = session.origin.client_key.clone();
            let start_streaming_start = std::time::Instant::now();
            tracing::info!("start_streaming(generation={generation}, is_resume={is_resume}, kind={:?}): starting", session.kind);
            // Bounded — confirmed via a dedicated integration test (not
            // guesswork) that this can hang even for a session with
            // nothing to do with the known KWin resume hang (a plain
            // fresh Login, using `HeadlessLogin`/`appsrc` for video, no
            // real compositor at all). The mechanism: every session's
            // *audio* pipeline goes through the same shared PipeWire
            // daemon regardless of video backend (`pipewiresrc` via a
            // per-session `pw-loopback` sink) — so once *any* session's
            // video-capture negotiation wedges that daemon (the resume
            // hang), it can block a totally unrelated session's own audio
            // `set_state(Playing)` call too.
            tracing::info!("start_streaming(generation={generation}): beginning video Playing-transition");
            let video_start = std::time::Instant::now();
            let video_pipeline = session.video_pipeline.clone();
            match tokio::time::timeout(Duration::from_secs(5), tokio::task::spawn_blocking(move || {
                use gstreamer::prelude::*;
                let set_state_result = video_pipeline.set_state(gstreamer::State::Playing);
                // `set_state` can return `Async` — the transition was only
                // *requested*, not confirmed complete — so log what it
                // actually settled into, not just that the call returned.
                // Diagnosing the "resume reliably leaves the client stuck
                // forever re-requesting an IDR frame" known limitation (see
                // `handoff_to_user`'s doc comment): does this ever actually
                // reach `Playing`, or does the pending state stay stuck at
                // `Playing` forever while `current` never leaves `Paused`?
                let (query_result, current, pending) = video_pipeline.state(gstreamer::ClockTime::from_seconds(3));
                (set_state_result, query_result, current, pending)
            }))
            .await
            {
                Ok(Ok((set_state_result, query_result, current, pending))) => tracing::info!(
                    "start_streaming(generation={generation}): video Playing-transition finished after {:?} \
                     (set_state returned {:?}, 3s state query returned {:?}, current={:?}, pending={:?})",
                    video_start.elapsed(), set_state_result, query_result, current, pending
                ),
                Ok(Err(e)) => tracing::error!("video Playing-transition task panicked: {e}"),
                Err(_) => tracing::error!("video Playing-transition timed out after 5s for generation={generation}"),
            }

            // Grab everything else needed out of `session` before moving
            // it into `shared.state` below — audio's own Playing-
            // transition, and the bus-watcher threads, run against clones
            // from here on, independent of the moved value.
            let audio_pipeline_for_bg = session.audio_pipeline.clone();
            let video_pipeline_for_keyframe = session.video_pipeline.clone();
            let cuda_direct_session_for_keyframe = session.cuda_direct_session.clone();
            let bus_watchers_stop = session.bus_watchers_stop.clone();
            let gst_payload_info = if !is_resume {
                match session.compositor.as_ref() {
                    Some(SpawnedCompositor::GstWaylandDisplay { runtime_dir, socket_path, socket_name, .. }) => Some((
                        session.kind.clone(),
                        runtime_dir.clone(),
                        socket_path.clone(),
                        socket_name.clone(),
                        session.broker_session_id.clone(),
                        session.selected_session.clone(),
                    )),
                    _ => None,
                }
            } else {
                None
            };
            let bus_pipelines = (!is_resume).then(|| [("video", session.video_pipeline.clone()), ("audio", session.audio_pipeline.clone())]);
            let is_login = matches!(session.kind, SessionType::Login);
            let username = match &session.kind {
                SessionType::User(username) => Some(username.clone()),
                SessionType::Login => None,
            };

            // Marks this generation active + moves to `Streaming` as soon
            // as *video* is ready — deliberately not waiting for audio
            // too. This is the actual fix for a live-reported "login
            // screen takes ~10-15s to come back": before this, a session
            // wasn't considered streaming (frames gated on
            // `active_generation`, input gated on `state == Streaming`,
            // see `on_input`) until *both* transitions had been attempted,
            // even though video routinely finishes in well under a
            // millisecond (`HeadlessLogin`'s video never touches PipeWire
            // at all) — audio being slow or wedged (the still-open
            // PipeWire issue) shouldn't hold a perfectly fine video stream
            // hostage. Audio's own transition now runs fully in the
            // background below and simply starts flowing whenever (if
            // ever) it actually becomes ready.
            {
                let mut shared = self.shared.lock().unwrap();
                shared.clients.insert(client_key.clone(), ClientSlot { state: ClientState::Streaming { session }, video_wait_task, audio_wait_task });
            }
            tracing::info!(
                "start_streaming(generation={generation}): marked active after {:?} (video-gated only; audio continues in the background)",
                start_streaming_start.elapsed()
            );

            // Force a real keyframe shortly after the video pipeline
            // starts — confirmed live: the very first frame captured right
            // after a fresh spawn or resume can be an essentially blank/
            // incomplete composite (the compositor hasn't painted its real
            // content yet — plasmashell etc. take from tens of ms to a few
            // seconds to actually render), and since `pipewiresrc` only
            // pushes new frames on Wayland *damage* (see project memory on
            // the "damage-source stall"), there's no guaranteed further
            // keyframe to correct it — only whatever incidental damage
            // happens to occur (cursor movement, a moved window), applied
            // as *deltas* against that same incomplete base. Visually this
            // is exactly "stripes that slowly heal as the mouse moves" on a
            // fresh login, and much worse (large chunks of the desktop
            // never correcting) after a resume, whose target is otherwise
            // already idle and generates little to no incidental damage on
            // its own. One forced IDR request after a real settle delay
            // gives the compositor time to have painted its actual content
            // at least once, so the correction is a real fix rather than
            // hoping enough deltas accumulate.
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(700)).await;
                tracing::info!("start_streaming(generation={generation}): forcing a keyframe to correct any incomplete initial composite");
                redfog_core::request_keyframe(&video_pipeline_for_keyframe);
                if let Some(cuda_direct_session) = &cuda_direct_session_for_keyframe {
                    cuda_direct_session.request_keyframe();
                }
            });

            // Audio: still bounded, but detached — never gates anything
            // above from here on.
            let audio_start = std::time::Instant::now();
            tokio::spawn(async move {
                match tokio::time::timeout(Duration::from_secs(5), tokio::task::spawn_blocking(move || {
                    use gstreamer::prelude::*;
                    let _ = audio_pipeline_for_bg.set_state(gstreamer::State::Playing);
                }))
                .await
                {
                    Ok(Ok(())) => tracing::info!("start_streaming(generation={generation}): background audio Playing-transition finished after {:?}", audio_start.elapsed()),
                    Ok(Err(e)) => tracing::error!("audio Playing-transition task panicked: {e}"),
                    Err(_) => tracing::error!(
                        "audio Playing-transition timed out after 5s for generation={generation} — proceeding without audio rather than blocking every session after it"
                    ),
                }
            });

            if let Some((kind, runtime_dir, socket_path, socket_name, broker_session_id, selected_session)) = gst_payload_info {
                self.spawn_gst_payload_in_background(kind, runtime_dir, socket_path, socket_name, broker_session_id, selected_session, client_key.clone());
            }
            if let Some(pipelines) = bus_pipelines {
                for (name, pipeline) in pipelines {
                    let bus = pipeline.bus().unwrap();
                    let bus_watchers_stop = bus_watchers_stop.clone();
                    std::thread::spawn(move || {
                        use gstreamer::MessageView;
                        // Bounded poll, not `iter_timed(ClockTime::NONE)`
                        // (blocks forever) — this thread holds `bus` (and
                        // through it, `pipeline`) alive for as long as it
                        // runs, so an unbounded wait here meant this
                        // generation's whole pipeline object graph (and any
                        // internal worker threads its elements spun up)
                        // never got freed even after `set_state(Null)`
                        // reported success elsewhere — see
                        // `RunningSession::bus_watchers_stop`'s doc comment.
                        while !bus_watchers_stop.load(std::sync::atomic::Ordering::Relaxed) {
                            let Some(msg) = bus.timed_pop(gstreamer::ClockTime::from_mseconds(200)) else { continue };
                            match msg.view() {
                                MessageView::Error(e) => {
                                    tracing::error!("{name} pipeline error: {} ({:?})", e.error(), e.debug());
                                }
                                MessageView::Warning(w) => {
                                    tracing::warn!("{name} pipeline warning: {} ({:?})", w.error(), w.debug());
                                }
                                MessageView::StateChanged(s) if msg.src().map(|s| s.type_() == gstreamer::Pipeline::static_type()).unwrap_or(false) => {
                                    tracing::debug!("{name} pipeline state: {:?} -> {:?}", s.old(), s.current());
                                }
                                _ => {}
                            }
                        }
                    });
                }
            }

            if is_resume {
                return;
            }
            if is_login {
                let this = self.arc_self();
                tokio::spawn(async move { this.watch_login_exit(client_key).await });
            } else if let Some(username) = username {
                let this = self.arc_self();
                tokio::spawn(async move { this.watch_user_session_exit(username).await });
            }
        })
    }

    /// `client_key` identifies which client's slot to watch — safe to fix
    /// at spawn time here (unlike `watch_user_session_exit`'s username): a
    /// Login session is never backgrounded/resumed, so it can only ever end
    /// under the same client_key it started under.
    async fn watch_login_exit(self: Arc<Self>, client_key: ClientKey) {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let status = {
                let mut shared = self.shared.lock().unwrap();
                match shared.clients.get_mut(&client_key).map(|slot| &mut slot.state) {
                    Some(ClientState::Streaming { session }) if matches!(session.kind, SessionType::Login) => {
                        session.compositor.as_mut().expect("compositor present until torn down").try_wait().ok().flatten()
                    }
                    _ => return, // no longer the login session (already handed off, or idle)
                }
            };
            let Some(status) = status else { continue };
            // A crashed/killed compositor (e.g. the Wayland connection
            // breaking mid-session — confirmed live) is NOT the same as the
            // user successfully logging in; `try_wait().is_some()` alone
            // can't tell those apart. Silently handing off to plasmashell
            // after a crash previously masked the real failure entirely.
            if status.success() {
                tracing::info!("login session exited, handing off to user session");
                if let Err(e) = self.handoff_to_user(client_key.clone()).await {
                    tracing::error!("failed to hand off to user session: {e}");
                }
            } else {
                tracing::error!("login compositor exited unexpectedly ({status}); resetting session instead of handing off");
                let _ = self.cancel(client_key);
            }
            return;
        }
    }

    /// The User-stage counterpart to `watch_login_exit` — without this,
    /// nothing ever notices a User session ending (e.g. logging out): the
    /// old code stopped watching entirely the moment handoff happened, so
    /// `shared.state` stayed `Streaming` around a session nothing could
    /// reconnect to. Unlike Login (always spawned directly by us), a
    /// broker-spawned User session's real process lives in the broker's own
    /// process tree — `session.compositor.try_wait()` can't see it exit
    /// (see `RunningSession::broker_session_id`'s doc comment) — so this
    /// falls back to asking the broker directly via `IsSessionAlive`
    /// whenever there's no local child handle to poll. Polls less
    /// aggressively than `watch_login_exit` (a User session is expected to
    /// run far longer than a login prompt, and the broker fallback costs a
    /// round trip — for `ActiveSession::Systemd` specifically, a
    /// `systemctl is-active` subprocess — not just a local `try_wait()`).
    ///
    /// One of these is spawned per username, the first time that user's
    /// session ever starts (see `start_streaming`'s `is_resume` parameter —
    /// a resume deliberately does *not* spawn a second one), and it keeps
    /// tracking that same session across the whole rest of its life —
    /// through being backgrounded (a fresh `/launch` replacing it, or an
    /// explicit `/cancel`) and resumed any number of times — checking
    /// whichever of `shared.state`/`background_sessions` it currently lives
    /// in on each tick. It only stops once the session is gone from *both*
    /// (logged out via `handle_log_out`, or found dead here).
    async fn watch_user_session_exit(self: Arc<Self>, username: String) {
        enum Location {
            /// Which `ClientKey` it's currently attached under —
            /// deliberately re-discovered every tick (not fixed at spawn
            /// time, unlike `watch_login_exit`'s): a resume can reattach
            /// this same username's session under a *different* client_key
            /// than the one that first started it (a different physical
            /// client resuming the same account), and this watcher has to
            /// keep tracking it wherever it currently lives.
            Active(ClientKey),
            Backgrounded,
        }
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;

            let active_check = {
                let mut shared = self.shared.lock().unwrap();
                let target_key = shared
                    .clients
                    .iter()
                    .find_map(|(key, slot)| slot.state.session().filter(|s| matches!(&s.kind, SessionType::User(u) if *u == username)).map(|_| key.clone()));
                target_key.and_then(|key| {
                    let ClientState::Streaming { session } = &mut shared.clients.get_mut(&key)?.state else { return None };
                    let exited = session.compositor.as_mut().expect("compositor present until torn down").try_wait().ok().flatten().is_some();
                    Some((key, session.broker_session_id.clone(), exited))
                })
            };
            let located = if let Some((key, broker_session_id, exited_locally)) = active_check {
                Some((Location::Active(key), broker_session_id, exited_locally))
            } else {
                let mut background = self.background_sessions.lock().unwrap();
                background.get_mut(&username).map(|session| {
                    let exited = session.compositor.as_mut().expect("compositor present until torn down").try_wait().ok().flatten().is_some();
                    (Location::Backgrounded, session.broker_session_id.clone(), exited)
                })
            };
            let Some((location, broker_session_id, exited_locally)) = located else {
                return; // no longer tracked anywhere — logged out, or this watcher's been superseded
            };

            let dead = if exited_locally {
                true
            } else if let Some(session_id) = &broker_session_id {
                match self.query_broker_session_alive(session_id).await {
                    Ok(alive) => !alive,
                    Err(e) => {
                        tracing::warn!("failed to query broker session {session_id} liveness for user {username}: {e}");
                        false
                    }
                }
            } else {
                false // no local handle and no broker configured — standalone use, nothing to check
            };

            if dead {
                tracing::info!("session for user {username} ended (logged out, or crashed)");
                match location {
                    Location::Active(key) => {
                        let taken = {
                            let mut shared = self.shared.lock().unwrap();
                            take_active_session(&mut shared, &key)
                        };
                        if let Some(session) = taken {
                            self.video_sender.drain_pending(session.origin.ping_token);
                            self.audio_sender.drain_pending(session.origin.ping_token);
                            self.control_registry.forget(session.origin.rikey);
                            // Detached — see `background_or_discard`'s doc
                            // comment on its own `Login` branch for why a
                            // `discard_running_session` call must never run
                            // inline on a task other code depends on
                            // finishing promptly. This one's a User
                            // session's confirmed-dead compositor, not a
                            // Login teardown specifically, but the same
                            // kill/wait/join hang risk applies, and nothing
                            // here needs the teardown to have actually
                            // finished before this watcher task returns.
                            std::thread::spawn(move || discard_running_session(session));
                        }
                    }
                    Location::Backgrounded => {
                        if let Some(session) = self.background_sessions.lock().unwrap().remove(&username) {
                            std::thread::spawn(move || discard_running_session(session));
                        }
                    }
                }
                return;
            }
        }
    }

    /// `BrokerRequest::IsSessionAlive` round trip — see `watch_user_session_exit`.
    async fn query_broker_session_alive(&self, session_id: &str) -> Result<bool, String> {
        use redfog_broker_protocol::{read_response, write_request, BrokerRequest, BrokerResponse};
        use tokio::io::BufReader;
        use tokio::net::UnixStream;

        let broker_socket_path = self.config.broker_socket_path.as_ref().ok_or_else(|| "no broker configured".to_string())?;
        let stream = UnixStream::connect(broker_socket_path).await.map_err(|e| format!("failed to connect to broker at {broker_socket_path:?}: {e}"))?;
        let mut reader = BufReader::new(stream);
        write_request(&mut reader, &BrokerRequest::IsSessionAlive { session_id: session_id.to_string() })
            .await
            .map_err(|e| format!("failed to send IsSessionAlive to broker: {e}"))?;
        match read_response(&mut reader).await.map_err(|e| format!("failed to read IsSessionAlive response: {e}"))? {
            BrokerResponse::IsSessionAlive(alive) => Ok(alive),
            other => Err(format!("unexpected broker response to IsSessionAlive: {other:?}")),
        }
    }

    async fn handoff_to_user(&self, client_key: ClientKey) -> Result<(), String> {
        let handoff_start = std::time::Instant::now();
        tracing::info!("handoff_to_user({client_key:?}): starting");
        let old_login = {
            let mut shared = self.shared.lock().unwrap();
            let Some(mut slot) = shared.clients.remove(&client_key) else {
                return Err("handoff requested but this client has no session".to_string());
            };
            match slot.state {
                ClientState::Streaming { session } => session,
                other => {
                    slot.state = other;
                    shared.clients.insert(client_key, slot);
                    return Err("handoff requested but no login session is streaming".to_string());
                }
            }
        };
        let (width, height, fps) = (old_login.width, old_login.height, old_login.fps);
        // Carried forward wholesale — see `SessionOrigin`'s doc comment:
        // this is the same wire-level RTSP session (crypto key, RTP
        // continuity, adaptive bitrate) regardless of whether the User
        // stage ends up fresh or resumed from background.
        let origin = old_login.origin.clone();
        let login_generation = old_login.generation;

        // Detached, not awaited — the old Login's pipelines/compositor are
        // independent objects the *new* session never touches, so there's
        // no real reason tearing them down needs to finish (or even start)
        // before we go resolve and spawn the new session below. Previously
        // this was three sequential, individually-timeout-bounded steps
        // (video/audio `Null`, compositor teardown) awaited right here —
        // technically safe (nothing hung forever) but still added up to
        // 15s of dead time in the critical path of "does the login UI come
        // back", confirmed live, purely from tearing down a session nobody
        // needs anymore. `discard_running_session` already does exactly
        // this teardown (with its own bounded timeouts — see its doc
        // comment) and is already used this same detached way by every
        // other call site that discards a `RunningSession`; this just
        // brings `handoff_to_user` in line with them instead of being the
        // one place that still blocked on it.
        tracing::info!("handoff_to_user: backgrounding old login's teardown (detached, not blocking the new session) after {:?}", handoff_start.elapsed());
        std::thread::spawn(move || discard_running_session(old_login));

        // `handle_login_report` already resolved which username this is
        // before Login even exited, keyed by this same login's own
        // generation (see `pending_logins`'s doc comment) — if that user
        // already has a backgrounded session (see `background_or_discard`),
        // resume it instead of starting a fresh one: the desktop they left
        // behind (and anything still running in it) is exactly where they
        // left it.
        //
        // For all backends, resuming a backgrounded session is a clean no-op
        // (see `rebuild_for_resume`'s doc comment). The GStreamer pipelines
        // are kept in the Playing state while backgrounded, so the PipeWire
        // stream remains hot and active, avoiding both the damage-source stall
        // on resume and the need to recreate/reconnect the capture session.
        let reported = self.pending_logins.lock().unwrap().remove(&login_generation);
        let username = reported.as_ref().map(|r| r.username.clone()).unwrap_or_else(|| "user".to_string());
        let existing_background = self.background_sessions.lock().unwrap().remove(&username);
        let (user_session, is_resume) = match existing_background {
            Some(mut background) => {
                tracing::info!("resuming existing session for user {username}");
                // This resume is still a brand-new wire-level RTSP session
                // from the client's perspective (a fresh `/launch`, fresh
                // RTP epoch) even though the compositor underneath is old —
                // and possibly from a *different* physical client than
                // whoever originally started it. Overwrite its stale origin
                // with this login's, same as the fresh-spawn branch below.
                background.origin = origin;
                background.selected_session = reported.and_then(|r| r.selected);
                let rebuild_start = std::time::Instant::now();
                let background = self.rebuild_for_resume(background).await?;
                tracing::info!("handoff_to_user: rebuilt pipelines for resume after {:?}", rebuild_start.elapsed());
                // No separate Playing-transition here — `start_streaming`
                // below does it unconditionally (independently timeout-
                // bounded per pipeline) regardless of `is_resume`, so doing
                // it again here first would just be a redundant, unbounded
                // duplicate of the exact hang this project already hit and
                // fixed once (see `start_streaming`'s own doc comment).
                (background, true)
            }
            None => {
                tracing::info!("handoff_to_user: no backgrounded session for {username}, spawning a fresh user compositor");
                let spawn_start = std::time::Instant::now();
                let (compositor, resolved_username, broker_session_id) = self.spawn_user_compositor(width, height, fps, &reported).await?;
                tracing::info!("handoff_to_user: spawn_user_compositor for {resolved_username} finished after {:?}", spawn_start.elapsed());
                let generation = self.next_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let selected = reported.and_then(|r| r.selected);
                (
                    self.spawn_session(SessionType::User(resolved_username), width, height, fps, compositor, broker_session_id, generation, origin, selected)?,
                    false,
                )
            }
        };
        tracing::info!("handoff_to_user: calling start_streaming (is_resume={is_resume}) after {:?} total so far", handoff_start.elapsed());
        self.start_streaming(user_session, is_resume, None, None).await;
        tracing::info!("handoff_to_user: done after {:?} total", handoff_start.elapsed());
        Ok(())
    }
}

impl LaunchHandler for SessionManager {
    fn launch(&self, width: u32, height: u32, fps: u32, rikey: RemoteInputKey, client_key: ClientKey, client_ip: IpAddr) -> Result<(), String> {
        let taken;
        {
            let mut shared = self.shared.lock().unwrap();
            // Real clients retry `/launch` on their own if the first attempt
            // is slow (KWin startup, D-Bus activation) — wait for that
            // in-flight spawn to finish rather than erroring a request that
            // would otherwise have succeeded once the first one lands. Only
            // waited on when we actually raced one from this *same*
            // client_key (this slot was already `Spawning` when this call
            // arrived) — this is specifically the "my own retry" case, not
            // a new connection arriving after some earlier, unrelated spawn
            // already finished (see below).
            let raced_a_spawn_in_flight = matches!(shared.clients.get(&client_key).map(|s| &s.state), Some(ClientState::Spawning));
            if raced_a_spawn_in_flight {
                let (guard, timeout_result) = self
                    .spawn_done
                    .wait_timeout_while(shared, Duration::from_secs(15), |s| matches!(s.clients.get(&client_key).map(|c| &c.state), Some(ClientState::Spawning)))
                    .unwrap();
                shared = guard;
                if timeout_result.timed_out() && matches!(shared.clients.get(&client_key).map(|c| &c.state), Some(ClientState::Spawning)) {
                    return Err("timed out waiting for a concurrent launch to finish spawning".to_string());
                }
                // Whatever that in-flight spawn produced is treated as
                // *this* call's own result too — it's really one logical
                // launch that just took two HTTP round trips, not a second,
                // independent connection — so it does not get its own
                // fresh Login screen the way a genuinely separate `/launch`
                // does below.
                if let Some(session) = shared.clients.get_mut(&client_key).and_then(|slot| slot.state.session_mut()) {
                    session.origin.rikey = rikey.key;
                    session.origin.rikey_key_id = rikey.key_id as u32;
                    let session_rikey = session.origin.rikey;
                    drop(shared);
                    self.control_registry.register(session_rikey);
                    return Ok(());
                }
                // The in-flight spawn failed (back to absent) — fall
                // through and spawn fresh below, same as if this call had
                // never raced it at all.
            }

            // Every other `/launch` from this client always shows a fresh
            // Login screen — never silently reconnects to whatever was
            // previously attached. Picking which user (if any) to resume is
            // now the login screen's own job (Login/Resume/Log Out), not
            // something decided implicitly by which client happens to
            // reconnect. Whatever *was* attached under this client_key gets
            // backgrounded (if it was a real User session) or discarded
            // (Login, or nothing at all) first — a *different* client_key's
            // own slot is entirely untouched, which is the whole point of
            // keying `Shared::clients` this way.
            taken = take_active_session(&mut shared, &client_key);
            // Claim `Spawning` before releasing the lock — see
            // `ClientState::Spawning`'s doc comment for why this has to
            // happen atomically with the check above rather than after
            // `spawn_session()` returns.
            shared.clients.insert(client_key.clone(), ClientSlot::spawning());
        }
        // Backgrounded/discarded *after* releasing the lock above — see
        // `take_active_session`'s doc comment for why this can never
        // happen while still holding it.
        if let Some(session) = taken {
            self.video_sender.drain_pending(session.origin.ping_token);
            self.audio_sender.drain_pending(session.origin.ping_token);
            self.control_registry.forget(session.origin.rikey);
            background_or_discard(session, &self.background_sessions);
        }
        // A genuinely new RTSP session (not a handoff) — fresh RTP sequence
        // numbers/frame indices/timestamps/adaptive-bitrate state. See
        // `SessionOrigin`'s doc comment for why this must NOT also happen
        // inside `handoff_to_user`.
        let generation = self.next_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let origin = SessionOrigin::fresh(client_key.clone(), client_ip, rikey.key, rikey.key_id as u32, self.config.bitrate_kbps);
        // A panic during spawn (e.g. a bad GStreamer pipeline description —
        // this has actually happened) must not skip the state reset below:
        // without `catch_unwind` here, `Spawning` would be stuck forever and
        // every future `/launch` from this client would just time out
        // waiting on a condvar nothing will ever notify.
        let spawn_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let compositor = self.spawn_login_compositor(width, height, generation)?;
            self.spawn_session(SessionType::Login, width, height, fps, compositor, None, generation, origin, None)
        }));
        let session = match spawn_result {
            Ok(Ok(session)) => session,
            Ok(Err(e)) => {
                self.shared.lock().unwrap().clients.remove(&client_key);
                self.spawn_done.notify_all();
                return Err(e);
            }
            Err(panic) => {
                self.shared.lock().unwrap().clients.remove(&client_key);
                self.spawn_done.notify_all();
                std::panic::resume_unwind(panic);
            }
        };
        // Registered unconditionally (no "was this client idle before"
        // distinction needed, unlike the old single-slot global sweep this
        // replaced): registration is scoped to this rikey and bumps its own
        // epoch, so it can only ever disconnect a *stale* peer previously
        // matched to this same rikey — never a different, unrelated
        // client's brand-new peer, which is exactly the failure mode the
        // old global generation sweep had to work around.
        self.control_registry.register(rikey.key);
        let mut shared = self.shared.lock().unwrap();
        shared.clients.insert(client_key, ClientSlot { state: ClientState::Launched { session }, video_wait_task: None, audio_wait_task: None });
        drop(shared);
        self.spawn_done.notify_all();
        Ok(())
    }

    fn resume(&self, _client_key: ClientKey) -> Result<(), String> {
        Err("resume not yet implemented".to_string())
    }

    fn cancel(&self, client_key: ClientKey) -> Result<(), String> {
        let taken = {
            let mut shared = self.shared.lock().unwrap();
            take_active_session(&mut shared, &client_key)
        };
        if let Some(session) = taken {
            self.video_sender.drain_pending(session.origin.ping_token);
            self.audio_sender.drain_pending(session.origin.ping_token);
            self.control_registry.forget(session.origin.rikey);
            background_or_discard(session, &self.background_sessions);
        }
        Ok(())
    }
}

/// Extracts whatever's currently attached under `client_key` out of
/// `shared.clients` (removing that slot entirely) — the only thing safe to
/// do while still holding `shared`'s lock. Deliberately does NOT touch the
/// extracted session's pipelines/compositor itself: those calls can
/// genuinely block — confirmed live, a hung KWin/PipeWire negotiation can
/// wedge a GStreamer `set_state` call indefinitely, not just fail to
/// deliver frames — and this lock must never be held through a call that
/// might not return. Every other request the server handles needs this
/// same lock (every future `/launch`, `/cancel`, RTSP action, the
/// video/audio encoder callbacks, ...), so blocking here wedges the whole
/// server, permanently, not just this one call — including the one thing
/// that must always keep working no matter what: a fresh `/launch` always
/// being able to show a working Login screen regardless of what's gone
/// wrong downstream.
///
/// This bug was real, not hypothetical: an earlier version called
/// `set_state`/`terminate` on the extracted session while still holding
/// this same lock (both here and in the death-detection path below), and a
/// hung resume left the *entire server* wedged — every subsequent
/// `/launch` hung forever waiting for a lock a stuck pipeline call was
/// still holding, so no client ever saw a Login screen again short of
/// restarting the process. Callers must drop the lock before acting on the
/// returned session.
///
/// Caller is responsible for also forgetting the extracted session's
/// `origin.ping_token`/`origin.rikey` from `SessionManager::video_sender`/
/// `audio_sender`/`control_registry` (`.drain_pending(ping_token)`/
/// `.forget(rikey)` on each) — this function only has access to `Shared`,
/// not those (which live directly on `SessionManager`, bound once at
/// startup).
fn take_active_session(shared: &mut Shared, client_key: &ClientKey) -> Option<RunningSession> {
    let mut slot = shared.clients.remove(client_key)?;
    // Sender state isn't the only thing holding a session's resources
    // alive — see `ClientSlot::video_wait_task`'s doc comment: each sender's
    // `wait_for_client` background task holds its own clone for up to a 30s
    // timeout, independent of whatever the caller does with the sender's
    // learned-address bookkeeping. Aborting them here (not just waiting
    // them out) is what actually lets a new session start listening for its
    // own client's ping immediately.
    if let Some(task) = slot.video_wait_task.take() {
        task.abort();
    }
    if let Some(task) = slot.audio_wait_task.take() {
        task.abort();
    }
    slot.state.into_session()
}

/// Best-effort resolution of "which `ClientKey` slot does this source IP
/// belong to" — needed wherever a plain RTSP connection (which carries no
/// certificate or client-echoed token at all — see `ClientKey`'s doc
/// comment) has to be matched back to a session created via `/launch`.
/// Prefers a not-yet-`Streaming` (`Launched`) slot over a `Streaming` one (a
/// fresh `PLAY` is far more common than a retake), and the most recently
/// created (highest `generation`) match within either bucket — the newest
/// launch from a given IP is the one most likely to have its own RTSP
/// handshake following shortly after.
///
/// This is an inherent "IP + timing, best effort" correlation (see
/// `CONCURRENT_SESSIONS.md`) — there is no stronger signal available once a
/// connection carries no cert or token, so
/// this can still misattribute a connection to the wrong client if two
/// *different* clients sharing an IP race each other within the same
/// narrow window between one's `/launch` and its own RTSP handshake. It's
/// what gets a connection matched to *a* plausible session at all;
/// `ping_token`/rikey-trial matching (see `crate::udp_sender`/
/// `control::ControlRegistry`) is what keeps that session's own data
/// correctly separated from any other, concurrent one for the rest of its
/// life, regardless of this initial guess's accuracy.
fn resolve_client_key_by_ip(shared: &Shared, ip: IpAddr) -> Option<ClientKey> {
    shared
        .clients
        .iter()
        .filter_map(|(key, slot)| {
            slot.state
                .session()
                .filter(|s| s.origin.client_ip == ip)
                .map(|s| (key, s.generation, matches!(slot.state, ClientState::Launched { .. })))
        })
        .max_by_key(|(_, generation, is_launched)| (*is_launched, *generation))
        .map(|(key, ..)| key.clone())
}

/// Resolution for the control channel — unlike RTSP's, this one is exact,
/// not best-effort: `control::ControlServer` only ever calls
/// `ControlEventHandler` methods with a `rikey` it already matched via
/// successful GCM decryption (see its own doc comment), so there is exactly
/// one session (with overwhelming probability — rikeys are 128 random bits
/// generated fresh per launch) whose `origin.rikey` equals it.
fn resolve_client_key_by_rikey(shared: &Shared, rikey: [u8; 16]) -> Option<ClientKey> {
    shared.clients.iter().find(|(_, slot)| slot.state.session().is_some_and(|s| s.origin.rikey == rikey)).map(|(key, _)| key.clone())
}

/// What to do with a session `take_active_session` just extracted — split
/// into two functions specifically so the (potentially blocking) work here
/// never runs while `shared`'s lock is held; see `take_active_session`'s
/// doc comment for why that split exists at all.
///
/// A `SessionType::User` session is assumed still alive and worth keeping
/// — its encoder/audio pipelines are paused (not stopped — see
/// `handoff_to_user`'s resume path, which un-pauses rather than rebuilding
/// them) and the whole session is stashed in `background_sessions` under
/// its username, so a later login as the same user can resume it instead
/// of starting fresh. A `SessionType::Login` session has nothing worth
/// preserving — stateless UI — and is discarded outright, same as this
/// always worked.
///
/// Used by `/cancel` and a fresh `/launch` replacing whatever was
/// attached. NOT used once a session's death has already been *confirmed*
/// — see `watch_user_session_exit`'s call to `discard_running_session`
/// directly for that case, which must never re-insert an already-dead
/// session into `background_sessions`.
fn background_or_discard(session: RunningSession, background_sessions: &Mutex<HashMap<String, RunningSession>>) {
    match &session.kind {
        SessionType::User(username) => {
            let username = username.clone();
            // Keeping the GStreamer pipelines running in the `Playing` state
            // during backgrounding avoids the need to recreate the screencast
            // stream on resume, which originally caused 1 FPS throttling by
            // leaking stream proxies. When the screen is idle, GStreamer
            // consumes virtually zero CPU/GPU resources since no frames are
            // produced.
            background_sessions.lock().unwrap().insert(username, session);
        }
        SessionType::Login => {
            // Detached, not called inline: `discard_running_session` ends
            // up in `SpawnedCompositor::HeadlessLogin::terminate()` (the
            // Login stage always uses `HeadlessLogin` — see its own doc
            // comment), which does `child.kill(); child.wait();
            // reader_thread.join()`. Every one of those three steps is
            // capable of blocking indefinitely in the wrong circumstances
            // (a process wedged in an uninterruptible kernel sleep ignores
            // even SIGKILL until that clears; a reader thread depends on
            // `shutdown()` actually unblocking its in-flight read). Both of
            // this function's callers (`launch()`/`cancel()`, see their own
            // doc comments) already commit `shared.state` to `Spawning`
            // *before* calling this — so a hang right here, run inline,
            // would mean the fresh Login this same `/launch` is trying to
            // show never actually spawns, `Spawning` never clears, and
            // every subsequent reconnect just times out waiting on a
            // condvar nobody will ever notify: exactly "the login session
            // hangs on reconnect, and it never comes back" as reported
            // live. This is a materially new exposure from "always show a
            // fresh Login on every reconnect" specifically — that decision
            // means this teardown now runs on nearly every disconnect,
            // not just an occasional explicit `/cancel` from Idle. A
            // leaked, never-joining thread if the old process really is
            // wedged is an acceptable tradeoff next to that.
            std::thread::spawn(move || discard_running_session(session));
        }
    }
}

/// Tears down a session that's confirmed no longer wanted — logged out (see
/// `SessionManager::handle_log_out`), or found dead (see
/// `reset_after_confirmed_death`/`watch_user_session_exit`) — regardless of
/// whether it came from `shared.state` or `background_sessions`. Stopping a
/// `Playing` pipeline this way (`Null`, not `Paused`) is safe here
/// specifically because the session is being fully discarded, never
/// resumed afterward — see `background_or_discard_active_session`'s doc
/// comment for why merely *backgrounding* one must use `Paused` instead.
/// Runs a blocking closure on its own thread and waits up to `timeout` for
/// it to finish, returning whether it did. If it didn't, that thread is
/// abandoned (leaked) rather than blocking the *caller* — a small, bounded
/// cost (one stuck thread) instead of an unbounded one. Needed here (see
/// `discard_running_session`) because that function already runs on its
/// own detached thread with no async runtime available to it, so
/// `tokio::time::timeout` (used everywhere else this project bounds a
/// GStreamer `set_state` call) isn't an option.
fn run_with_timeout<F: FnOnce() + Send + 'static>(f: F, timeout: Duration) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    rx.recv_timeout(timeout).is_ok()
}

fn discard_running_session(mut session: RunningSession) {
    // Runs on its own detached OS thread (see every call site's own
    // comment on why) — a hang here can't block anything else in the
    // process, but that also means it's otherwise *invisible*: nothing
    // else ever reports on whether it actually finished. Logged
    // end-to-end for exactly that reason, not because anything here is
    // expected to be slow in the common case.
    //
    // Each step independently timeout-bounded via `run_with_timeout` —
    // confirmed live this was the one remaining unprotected `set_state`
    // call site in the whole codebase: a log-out during a resume-hang
    // wedge left this thread stuck forever on the very first line below
    // (`video_pipeline.set_state(Null)`, no further log line ever
    // followed), which — because this function owns `session` by value —
    // silently leaked the *entire* `RunningSession` (its GStreamer
    // pipelines, encoder thread pools, everything) forever. This is a
    // live-recurring instance of the exact leak that caused an earlier
    // OOM incident (see project memory): back then the fix was giving
    // `RunningSession` a `Drop` impl at all; this is the same class of
    // bug surfacing again at a call site that Drop alone can't help,
    // since the value is never dropped in the first place while a
    // still-running thread holds it.
    let start = std::time::Instant::now();
    let kind = session.kind.clone();
    tracing::info!("discard_running_session({kind:?}): starting on detached thread");
    use gstreamer::prelude::*;

    // Lets this session's bus-watcher threads (if any — spawned only for a
    // fresh, non-resumed session, see `start_streaming`) drop their own
    // clone of the pipeline, so it can actually be freed once this
    // function's own clones are `Null`'d too — see `bus_watchers_stop`'s
    // doc comment for why `set_state(Null)` alone doesn't achieve that.
    session.bus_watchers_stop.store(true, std::sync::atomic::Ordering::Relaxed);

    let video_pipeline = session.video_pipeline.clone();
    if run_with_timeout(move || { let _ = video_pipeline.set_state(gstreamer::State::Null); }, Duration::from_secs(5)) {
        tracing::info!("discard_running_session({kind:?}): video pipeline set to Null after {:?}", start.elapsed());
    } else {
        tracing::error!("discard_running_session({kind:?}): video pipeline Null-transition timed out after 5s — abandoning it rather than leaking this thread forever");
    }

    let audio_pipeline = session.audio_pipeline.clone();
    if run_with_timeout(move || { let _ = audio_pipeline.set_state(gstreamer::State::Null); }, Duration::from_secs(5)) {
        tracing::info!("discard_running_session({kind:?}): audio pipeline set to Null after {:?}", start.elapsed());
    } else {
        tracing::error!("discard_running_session({kind:?}): audio pipeline Null-transition timed out after 5s — abandoning it rather than leaking this thread forever");
    }

    // `.take()`, not a direct field move: `RunningSession` has a `Drop`
    // impl now (see its own doc comment), and Rust forbids partially
    // moving a field out of a `Drop`-implementing type.
    let compositor = session.compositor.take().expect("compositor present until torn down");
    if run_with_timeout(move || compositor.terminate(), Duration::from_secs(5)) {
        tracing::info!("discard_running_session({kind:?}): compositor terminated, done after {:?}", start.elapsed());
    } else {
        tracing::error!("discard_running_session({kind:?}): compositor teardown timed out after 5s — abandoning it rather than leaking this thread forever");
    }
}

impl RtspHandler for SessionManager {
    fn on_announce(&self, params: AnnouncedParams, client_ip: IpAddr) {
        if let Some(bitrate_kbps) = params.bitrate_kbps {
            tracing::info!("RTSP ANNOUNCE: client requested bitrate {} kbps", bitrate_kbps);
            // Apply it to this client's active pipeline encoder immediately
            // if it exists. `client_ip` is all RTSP carries — see
            // `resolve_client_key_by_ip`'s doc comment for the resulting
            // best-effort resolution.
            let mut shared = self.shared.lock().unwrap();
            let Some(client_key) = resolve_client_key_by_ip(&shared, client_ip) else { return };
            if let Some(session) = shared.clients.get_mut(&client_key).and_then(|slot| slot.state.session_mut()) {
                session.origin.target_bitrate_kbps = bitrate_kbps;
                session.origin.current_bitrate_kbps = bitrate_kbps;
                redfog_core::set_encoder_bitrate(&session.video_pipeline, bitrate_kbps);
                if let Some(cuda_direct_session) = &session.cuda_direct_session {
                    cuda_direct_session.set_bitrate(bitrate_kbps);
                }
            }
        }
    }

    fn on_play(&self, client_ip: std::net::IpAddr) {
        enum PlayKind {
            /// First PLAY for this launch — pipelines aren't running yet.
            Fresh(RunningSession),
            /// A new client's PLAY while a previous one was already
            /// `Streaming` — e.g. a window/tab was closed without a clean
            /// disconnect and a new window took over via `/launch`'s
            /// reconnect path. The compositor and pipelines are already
            /// running and must stay untouched; only the senders' learned
            /// client address (for this session's `ping_token`) needs to
            /// move to the new client.
            Retake(RunningSession),
        }

        // Extracted (not left in place) while this processes, mirroring
        // `ClientState::Spawning`'s own absent-means-idle convention —
        // deliberately NOT replaced with a `Spawning` placeholder here: a
        // concurrent `/launch` from this same client racing this PLAY would
        // otherwise see `Spawning` and wait up to 15s on a condvar this
        // function never signals, even though PLAY handling itself
        // normally resolves in well under a second. `old_video_wait`/
        // `old_audio_wait` carry forward whatever wait-task handles this
        // slot already had (a previous PLAY's), so they can still be
        // aborted below rather than orphaned.
        let (kind, old_video_wait, old_audio_wait) = {
            let mut shared = self.shared.lock().unwrap();
            // `client_ip` is all RTSP carries — see
            // `resolve_client_key_by_ip`'s doc comment for the resulting
            // best-effort resolution.
            let Some(client_key) = resolve_client_key_by_ip(&shared, client_ip) else {
                tracing::warn!("PLAY received but no session is pending for {client_ip}");
                return;
            };
            // PLAY can race ahead of `/launch` finishing the (slow: KWin +
            // D-Bus + PipeWire) compositor spawn — confirmed live against
            // moonlight-web, which opens its RTSP connection and sends PLAY
            // without waiting for the `/launch` HTTP response. Wait for the
            // in-flight spawn to finish instead of dropping this PLAY, same
            // pattern `launch()` uses for a concurrent `/launch`.
            let (guard, timeout_result) = self
                .spawn_done
                .wait_timeout_while(shared, Duration::from_secs(15), |s| matches!(s.clients.get(&client_key).map(|c| &c.state), Some(ClientState::Spawning)))
                .unwrap();
            shared = guard;
            if timeout_result.timed_out() && matches!(shared.clients.get(&client_key).map(|c| &c.state), Some(ClientState::Spawning)) {
                tracing::warn!("PLAY received but launch is still spawning after 15s, giving up");
                return;
            }
            match shared.clients.remove(&client_key) {
                Some(mut slot) => {
                    let video_wait = slot.video_wait_task.take();
                    let audio_wait = slot.audio_wait_task.take();
                    let kind = match slot.state {
                        ClientState::Launched { session } => Some(PlayKind::Fresh(session)),
                        ClientState::Streaming { session } => Some(PlayKind::Retake(session)),
                        ClientState::Spawning => None,
                    };
                    (kind, video_wait, audio_wait)
                }
                None => (None, None, None),
            }
        };
        let Some(kind) = kind else {
            tracing::warn!("PLAY received but no session is in Launched/Streaming state for {client_ip}");
            return;
        };

        let this = self.arc_self();
        tokio::spawn(async move {
            let (session, is_retake) = match kind {
                PlayKind::Fresh(session) => (session, false),
                PlayKind::Retake(session) => {
                    // The previous client's own stale `PING`(s) may already
                    // be sitting in the shared sockets' receive buffers (UDP
                    // has no teardown to stop them, and nothing's read for
                    // this token since) — without this, `wait_for_client`
                    // below picks one of those up instead of the new
                    // client's, permanently misrouting the stream to the
                    // old, now-gone address. Confirmed live.
                    this.video_sender.drain_pending(session.origin.ping_token);
                    this.audio_sender.drain_pending(session.origin.ping_token);
                    (session, true)
                }
            };
            let client_key = session.origin.client_key.clone();
            let ping_token = session.origin.ping_token;

            // `wait_for_client` loops forever if no PING ever arrives (a
            // session that gets abandoned before the client pings). Without
            // a timeout, that leaves this token's entry in the shared
            // senders' bookkeeping alive indefinitely. For a retake, the
            // sender keeps sending to the old (now-gone) client address
            // until this overwrites it with the new one — harmless, just
            // wasted bandwidth in the meantime.
            //
            // The 30s timeout alone isn't tight enough on its own, though —
            // confirmed live: a client that disconnects and reconnects
            // within that window (routine now that every `/launch` always
            // shows a fresh Login — see `ClientSlot::video_wait_task`'s doc
            // comment for the full story) hits the exact same misrouting
            // this comment already describes, *while this task is still
            // well within its own 30s budget*. Storing the handles in
            // `shared` and having `take_active_session` `.abort()` them the
            // moment a session stops being attached is what actually closes
            // that gap.
            let video_wait_task = tokio::spawn({
                let video_sender = this.video_sender.clone();
                async move {
                    match tokio::time::timeout(Duration::from_secs(30), video_sender.wait_for_client(ping_token)).await {
                        Ok(Ok(addr)) => tracing::info!("video client announced itself at {addr}"),
                        Ok(Err(e)) => tracing::warn!("video wait_for_client failed: {e}"),
                        Err(_) => tracing::warn!("no video client PING received within 30s, giving up"),
                    }
                }
            });
            let audio_wait_task = tokio::spawn({
                let audio_sender = this.audio_sender.clone();
                async move {
                    match tokio::time::timeout(Duration::from_secs(30), audio_sender.wait_for_client(ping_token)).await {
                        Ok(Ok(addr)) => tracing::info!("audio client announced itself at {addr}"),
                        Ok(Err(e)) => tracing::warn!("audio wait_for_client failed: {e}"),
                        Err(_) => tracing::warn!("no audio client PING received within 30s, giving up"),
                    }
                }
            });
            // Defensive, not the expected case: a genuinely overlapping
            // PLAY (e.g. a retake racing a fresh one) could otherwise
            // orphan the previous handle here without ever aborting it.
            if let Some(old) = old_video_wait {
                old.abort();
            }
            if let Some(old) = old_audio_wait {
                old.abort();
            }

            if is_retake {
                // Pipelines are already Playing and `watch_login_exit` (if
                // this is a login session) is already running — just put the
                // session back, don't redo any of `start_streaming`'s setup.
                let mut shared = this.shared.lock().unwrap();
                shared.clients.insert(
                    client_key,
                    ClientSlot { state: ClientState::Streaming { session }, video_wait_task: Some(video_wait_task), audio_wait_task: Some(audio_wait_task) },
                );
            } else {
                // Always the Login stage's very first PLAY here (see
                // `PlayKind::Fresh`'s doc comment) — Login is never
                // backgrounded/resumed, so this is never a resume.
                this.start_streaming(session, false, Some(video_wait_task), Some(audio_wait_task)).await;
            }
        });
    }

    fn ping_token_for(&self, client_ip: IpAddr) -> Option<[u8; 16]> {
        let shared = self.shared.lock().unwrap();
        let client_key = resolve_client_key_by_ip(&shared, client_ip)?;
        Some(shared.clients.get(&client_key)?.state.session()?.origin.ping_token)
    }
}

impl ControlEventHandler for SessionManager {
    fn on_input(&self, rikey: [u8; 16], event: InputEvent) {
        let mut shared = self.shared.lock().unwrap();
        let Some(client_key) = resolve_client_key_by_rikey(&shared, rikey) else { return };
        let Some(ClientState::Streaming { session }) = shared.clients.get_mut(&client_key).map(|slot| &mut slot.state) else { return };
        let fwd = &mut session.input_forwarder;
        match event {
            InputEvent::KeyDown { keycode } => {
                tracing::debug!("forwarding KeyDown keycode={keycode}");
                fwd.keyboard_key(keycode, true)
            }
            InputEvent::KeyUp { keycode } => {
                tracing::debug!("forwarding KeyUp keycode={keycode}");
                fwd.keyboard_key(keycode, false)
            }
            InputEvent::MouseMoveRelative { dx, dy } => {
                tracing::debug!("forwarding MouseMoveRelative dx={dx} dy={dy}");
                if self.config.log_mouse_events {
                    tracing::info!("mouse event: MouseMoveRelative dx={dx} dy={dy}");
                }
                fwd.pointer_motion(dx as f64, dy as f64)
            }
            InputEvent::MouseMoveAbsolute { x, y, screen_width, screen_height } => {
                tracing::debug!("forwarding MouseMoveAbsolute x={x} y={y} screen_width={screen_width} screen_height={screen_height}");
                if self.config.log_mouse_events {
                    tracing::info!(
                        "mouse event: MouseMoveAbsolute x={x} y={y} screen_width={screen_width} screen_height={screen_height}"
                    );
                }
                if screen_width > 0 && screen_height > 0 {
                    // Client viewport coords -> our actual output resolution.
                    let scaled_x = x as f64 / screen_width as f64 * session.width as f64;
                    let scaled_y = y as f64 / screen_height as f64 * session.height as f64;
                    fwd.pointer_motion_absolute(scaled_x, scaled_y);
                }
            }
            InputEvent::MouseButtonDown { button } => {
                if self.config.log_mouse_events {
                    tracing::info!("mouse event: MouseButtonDown button={button}");
                }
                fwd.button(button, true)
            }
            InputEvent::MouseButtonUp { button } => {
                if self.config.log_mouse_events {
                    tracing::info!("mouse event: MouseButtonUp button={button}");
                }
                fwd.button(button, false)
            }
            InputEvent::ScrollVertical { amount } => {
                if self.config.log_mouse_events {
                    tracing::info!("mouse event: ScrollVertical amount={amount}");
                }
                fwd.axis(0, amount as f64)
            }
            InputEvent::ScrollHorizontal { amount } => {
                if self.config.log_mouse_events {
                    tracing::info!("mouse event: ScrollHorizontal amount={amount}");
                }
                fwd.axis(1, amount as f64)
            }
        }
        fwd.flush();
    }

    fn on_request_idr_frame(&self, rikey: [u8; 16]) {
        let shared = self.shared.lock().unwrap();
        let Some(client_key) = resolve_client_key_by_rikey(&shared, rikey) else { return };
        if let Some(ClientState::Streaming { session }) = shared.clients.get(&client_key).map(|slot| &slot.state) {
            redfog_core::request_keyframe(&session.video_pipeline);
            if let Some(cuda_direct_session) = &session.cuda_direct_session {
                cuda_direct_session.request_keyframe();
            }
        }
    }

    /// Server-side adaptive bitrate — see `redfog_core::set_encoder_bitrate`'s
    /// doc comment for why pure bitrate changes need no client cooperation
    /// beyond this already-standard report. Heuristic, not scientifically
    /// tuned (no access to real Sunshine's own constants) — multiplicative
    /// step down when the client's reported `last_good_frame` is
    /// meaningfully behind the frame we've actually just sent (a real,
    /// direct sign it's not keeping up), multiplicative recovery once it's
    /// caught back up, dead zone in between so single-frame jitter doesn't
    /// cause visible oscillation. Never exceeds `config.bitrate_kbps` — that
    /// stays the ceiling, not just a starting point.
    fn on_loss_stats(&self, rikey: [u8; 16], last_good_frame: u64) {
        let (client_key, video_pipeline, cuda_direct_session, video_packetizer, target_kbps, current_kbps) = {
            let shared = self.shared.lock().unwrap();
            let Some(client_key) = resolve_client_key_by_rikey(&shared, rikey) else { return };
            let Some(ClientState::Streaming { session }) = shared.clients.get(&client_key).map(|slot| &slot.state) else { return };
            (
                client_key,
                session.video_pipeline.clone(),
                session.cuda_direct_session.clone(),
                session.origin.video_packetizer.clone(),
                session.origin.target_bitrate_kbps,
                session.origin.current_bitrate_kbps,
            )
        };

        let next_frame_number = video_packetizer.lock().unwrap().next_frame_number();
        let frames_behind = (next_frame_number as u64).saturating_sub(last_good_frame);
        let new_kbps = adapt_bitrate_kbps(current_kbps, target_kbps, frames_behind);
        // Every report, not just ones that actually change anything —
        // otherwise there's no way to see this loop is even running (e.g.
        // to confirm it's just correctly deciding not to act) short of
        // instrumenting the client itself.
        tracing::debug!("loss stats: last_good_frame={last_good_frame} next_frame_number={next_frame_number} frames_behind={frames_behind} current_bitrate={current_kbps}kbps");

        if new_kbps != current_kbps {
            {
                let mut shared = self.shared.lock().unwrap();
                if let Some(session) = shared.clients.get_mut(&client_key).and_then(|slot| slot.state.session_mut()) {
                    session.origin.current_bitrate_kbps = new_kbps;
                }
            }
            tracing::debug!("adaptive bitrate: {current_kbps} -> {new_kbps} kbps (frames_behind={frames_behind})");
            redfog_core::set_encoder_bitrate(&video_pipeline, new_kbps);
            if let Some(cuda_direct_session) = &cuda_direct_session {
                cuda_direct_session.set_bitrate(new_kbps);
            }
        }
    }
}

/// Pure decision function for `on_loss_stats`'s server-side adaptive
/// bitrate — split out from it purely so the actual heuristic is
/// unit-testable without a running session/pipeline behind it. See that
/// method's own doc comment for the reasoning (multiplicative step
/// down/up, dead zone, ceiling never exceeds `target_kbps`).
fn adapt_bitrate_kbps(current_kbps: u32, target_kbps: u32, frames_behind: u64) -> u32 {
    let floor_kbps = (target_kbps / 4).max(1_000);
    if frames_behind > 3 {
        (current_kbps * 85 / 100).max(floor_kbps)
    } else if frames_behind <= 1 {
        (current_kbps * 105 / 100).min(target_kbps)
    } else {
        current_kbps
    }
}

#[cfg(test)]
mod adaptive_bitrate_tests {
    use super::adapt_bitrate_kbps;

    #[test]
    fn steps_down_when_client_falls_behind() {
        assert_eq!(adapt_bitrate_kbps(10_000, 10_000, 4), 8_500);
    }

    #[test]
    fn recovers_up_when_caught_up() {
        assert_eq!(adapt_bitrate_kbps(8_000, 10_000, 0), 8_400);
        assert_eq!(adapt_bitrate_kbps(8_000, 10_000, 1), 8_400);
    }

    #[test]
    fn dead_zone_leaves_bitrate_unchanged() {
        assert_eq!(adapt_bitrate_kbps(8_000, 10_000, 2), 8_000);
        assert_eq!(adapt_bitrate_kbps(8_000, 10_000, 3), 8_000);
    }

    #[test]
    fn never_recovers_past_target_ceiling() {
        assert_eq!(adapt_bitrate_kbps(9_900, 10_000, 0), 10_000);
    }

    #[test]
    fn never_drops_below_floor() {
        // floor is max(target/4, 1000) = 2_500 for a 10_000 target.
        assert_eq!(adapt_bitrate_kbps(2_600, 10_000, 10), 2_500);
        assert_eq!(adapt_bitrate_kbps(2_500, 10_000, 10), 2_500);
    }

    #[test]
    fn floor_has_an_absolute_minimum_for_low_targets() {
        // target/4 would be 500 for a 2_000 target, but the floor never
        // goes below 1_000 regardless of how low the configured target is.
        assert_eq!(adapt_bitrate_kbps(1_050, 2_000, 10), 1_000);
    }
}

#[cfg(test)]
mod config_tests {
    use super::Backend;

    /// The checked-in example config must actually load and have valid
    /// `backend` values — the same validation `redfog-server`'s own
    /// startup code does (see its `main.rs`), kept here rather than in
    /// `redfog-login-protocol` since that crate deliberately doesn't
    /// depend on `session-backend`'s `Backend` type (see its own doc
    /// comments on `SessionPreset::backend`/`load_presets`).
    #[test]
    fn checked_in_example_sessions_config_is_valid() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/sessions.toml.example");
        let presets = redfog_login_protocol::load_presets(path).expect("config/sessions.toml.example should parse");
        assert!(!presets.is_empty());
        for preset in &presets {
            preset.backend.parse::<Backend>().unwrap_or_else(|e| panic!("config/sessions.toml.example: session {:?}: {e}", preset.name));
        }
    }
}
