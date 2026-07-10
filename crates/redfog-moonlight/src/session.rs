//! Login -> User `CompositorSession` handoff state machine, driven by RTSP
//! events. The RTSP-driven analogue of `kwin-viewer`'s winit-loop handoff:
//! `/launch` spawns the Login compositor and streams it; once it exits
//! (login succeeded), we spawn the User compositor and repoint the video/
//! audio/input pipelines at it — same two-session dance, different trigger.
//!
//! Single session at a time for this iteration — a second `/launch` while
//! one is active is rejected, matching a reasonable v1 restriction.

use std::net::IpAddr;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::Duration;

use redfog_core::{AudioLoopback, CompositorSession, InputForwarder, InputSink, SessionType};

use crate::audio::{AudioPacketizer, AudioSender};
use crate::control::{ControlEventHandler, InputEvent};
use crate::pairing::{LaunchHandler, RemoteInputKey};
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
    /// (e.g. `["plasmashell", "--no-respawn"]`).
    pub user_app: Vec<String>,
    pub bitrate_kbps: u32,
    /// Logs every mouse input event (move/button/scroll) at `info` level
    /// when true — separate from `RUST_LOG=debug`, which also floods with
    /// per-frame video encoder logs. For diagnosing real-client mouse
    /// behavior (sensitivity, drops, event shape) without that noise.
    pub log_mouse_events: bool,
    /// Path to redfog-broker's Unix socket. When set, the User session
    /// (post-login) is spawned via the broker (`Authenticate` then
    /// `SpawnSession`, see design.md's "Privilege separation: broker vs.
    /// server") instead of directly via `CompositorSession::spawn` — the
    /// production path, and what the integration test exercises with a
    /// fake-auth/force-spawn-user broker. `None` keeps today's direct-spawn
    /// behavior, for standalone use without a broker.
    pub broker_socket_path: Option<std::path::PathBuf>,
}

struct RunningSession {
    kind: SessionType,
    width: u32,
    height: u32,
    compositor: CompositorSession,
    input_forwarder: Box<dyn InputSink>,
    video_pipeline: gstreamer::Pipeline,
    audio_pipeline: gstreamer::Pipeline,
    _audio_loopback: AudioLoopback,
}

enum State {
    Idle,
    /// Claimed by an in-flight `/launch` call that's still spawning the
    /// compositor (a slow step: KWin startup, D-Bus activation, etc.) — a
    /// placeholder so a second, concurrent `/launch` (real clients retry
    /// `/launch` on their own if the first attempt is slow) can't also see
    /// `Idle` and race into `spawn_session()` at the same time. Without
    /// this, two racing spawns fight over the same KWin Wayland socket name
    /// and both die almost immediately — confirmed live: 9 leaked
    /// kwin_wayland/redfog-login process pairs from one retry storm.
    Spawning,
    /// Launched (compositor running) but RTSP hasn't reached PLAY yet.
    Launched { session: RunningSession },
    /// Streaming: encoder/sender pipelines are live.
    Streaming { session: RunningSession },
}

struct Shared {
    state: State,
    video_sender: Option<Arc<VideoSender>>,
    audio_sender: Option<Arc<AudioSender>>,
}

pub struct SessionManager {
    config: SessionConfig,
    shared: Mutex<Shared>,
    /// Signaled whenever `shared.state` transitions away from `Spawning` —
    /// lets a concurrent `/launch` (real clients retry on their own if the
    /// first attempt is slow — KWin startup, D-Bus activation) wait for that
    /// spawn to finish and reconnect to it, instead of getting a hard error
    /// for a request that would otherwise have succeeded.
    spawn_done: Condvar,
    self_ref: OnceLock<Weak<SessionManager>>,
    /// Shared with `control::ControlServer` — see its doc comment for why
    /// this is a cell rather than passed in at construction.
    rikey_cell: Arc<Mutex<Option<[u8; 16]>>>,
    /// Bumped whenever `rikey_cell` changes for a reconnect/takeover (see
    /// `set_rikey`) — see `control::ControlServer::rikey_generation`'s doc
    /// comment for the full reasoning (a plain "disconnect everyone
    /// connected right now" flag caught the new client's own brand-new peer
    /// too, confirmed live).
    rikey_generation: Arc<AtomicU64>,
    /// RTP sequence numbers, frame indices, and the timestamp clock's epoch
    /// must stay continuous across a Login->User handoff — it's the same
    /// wire-level RTSP/video session from the client's perspective, just a
    /// different compositor underneath. Recreating these per-`spawn_session`
    /// call (as originally written) reset sequence numbers/frame indices back
    /// near zero on every handoff; real clients' RTP jitter buffers/frame
    /// trackers treat a sudden drop like that as stale/duplicate data and
    /// silently stop accepting new frames — confirmed live: video froze on
    /// the last login-screen frame forever after handoff, while input (a
    /// separate control-channel path) kept working fine. `reset_stream_state`
    /// is the only thing allowed to replace these, and only for a genuinely
    /// new `/launch`, not a handoff within the same session.
    video_packetizer: Mutex<Arc<Mutex<VideoPacketizer>>>,
    audio_packetizer: Mutex<Arc<Mutex<AudioPacketizer>>>,
    stream_start: Mutex<std::time::Instant>,
    /// Unique per-attempt id passed to the broker's `SpawnSession` — avoids
    /// systemd unit name collisions across successive launch/cancel cycles
    /// within the same `redfog-server` process lifetime.
    next_broker_session_id: AtomicU64,
    /// Set by `handle_login_report` once `redfog-login`'s reported
    /// credentials pass the broker's `Authenticate` check — the real
    /// account `spawn_user_compositor` spawns the User stage as, replacing
    /// the `"user"` placeholder used before this was wired up (and still
    /// the fallback when nothing ever reports in, e.g. `redfog-test-ux`'s
    /// stand-in login stage in tests).
    authenticated_username: Mutex<Option<String>>,
}

impl SessionManager {
    pub fn new(config: SessionConfig) -> Arc<Self> {
        let this = Arc::new(Self {
            config,
            shared: Mutex::new(Shared {
                state: State::Idle,
                video_sender: None,
                audio_sender: None,
            }),
            spawn_done: Condvar::new(),
            self_ref: OnceLock::new(),
            rikey_cell: Arc::new(Mutex::new(None)),
            rikey_generation: Arc::new(AtomicU64::new(0)),
            video_packetizer: Mutex::new(Arc::new(Mutex::new(VideoPacketizer::new()))),
            audio_packetizer: Mutex::new(Arc::new(Mutex::new(AudioPacketizer::new()))),
            stream_start: Mutex::new(std::time::Instant::now()),
            next_broker_session_id: AtomicU64::new(0),
            authenticated_username: Mutex::new(None),
        });
        let _ = this.self_ref.set(Arc::downgrade(&this));
        this
    }

    /// Validates credentials reported by `redfog-login` (see
    /// `crate::login_report`) via the broker's real PAM-backed
    /// `Authenticate`, and remembers the username on success for the
    /// subsequent User-stage `SpawnSession` call. Without a broker
    /// configured (standalone use), just requires a non-empty username,
    /// matching `redfog-login`'s original no-op placeholder behavior.
    pub async fn handle_login_report(&self, username: String, password: String) -> Result<(), String> {
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
        *self.authenticated_username.lock().unwrap() = Some(username);
        Ok(())
    }

    /// Shared cell for `control::ControlServer` to read the current
    /// session's `rikey` from.
    pub fn rikey_cell(&self) -> Arc<Mutex<Option<[u8; 16]>>> {
        self.rikey_cell.clone()
    }

    /// Shared with `control::ControlServer` — see the field's doc comment.
    pub fn rikey_generation(&self) -> Arc<AtomicU64> {
        self.rikey_generation.clone()
    }

    /// Sets the active `rikey` for a reconnect/takeover and bumps the
    /// generation counter so `control::ControlServer` disconnects peers left
    /// over from before this point — see `rikey_generation`'s doc comment.
    fn set_rikey(&self, key: [u8; 16]) {
        *self.rikey_cell.lock().unwrap() = Some(key);
        self.rikey_generation.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    /// Start a fresh RTP sequence/frame-index/timestamp epoch for a genuinely
    /// new `/launch` (a new RTSP session from the client's perspective) — NOT
    /// called on a Login->User handoff, which must keep the existing state.
    fn reset_stream_state(&self) {
        *self.video_packetizer.lock().unwrap() = Arc::new(Mutex::new(VideoPacketizer::new()));
        *self.audio_packetizer.lock().unwrap() = Arc::new(Mutex::new(AudioPacketizer::new()));
        *self.stream_start.lock().unwrap() = std::time::Instant::now();
    }

    /// An owned `Arc<Self>` for moving into spawned tasks — trait methods
    /// here take plain `&self` (so `SessionManager` stays usable as a
    /// trait object across `LaunchHandler`/`RtspHandler`/`ControlEventHandler`
    /// without relying on `Arc<Self>`-receiver methods on trait objects).
    fn arc_self(&self) -> Arc<Self> {
        self.self_ref.get().and_then(Weak::upgrade).expect("self_ref set in new()")
    }

    /// Spawns the Login compositor directly — never goes through the
    /// broker, since it doesn't need to run as any particular target user
    /// (see design.md's "Authentication: a real graphical login screen").
    fn spawn_login_compositor(&self, width: u32, height: u32) -> Result<CompositorSession, String> {
        CompositorSession::spawn(SessionType::Login, "redfog-login-0", width as i32, height as i32, 1.0, &self.config.login_app)
            .map_err(|e| format!("failed to spawn redfog-login-0: {e}"))
    }

    /// Acquires the User compositor — via the broker if configured
    /// (`Authenticate` then `SpawnSession`, the production path), or
    /// directly otherwise (standalone use without a broker).
    async fn spawn_user_compositor(&self, width: u32, height: u32) -> Result<CompositorSession, String> {
        // `handle_login_report` sets this once `redfog-login`'s credentials
        // have already passed the broker's real, password-checked
        // `Authenticate` — re-sending an empty password through that same
        // check below would just fail against real PAM, so a real username
        // here means skip straight to `SpawnSession`. `None` (nothing ever
        // reported in — standalone use without a broker, or
        // `redfog-test-ux`'s stand-in login stage in tests) falls back to
        // the placeholder "user" and the old Authenticate-with-empty-
        // password call, which is exactly what `REDFOG_BROKER_FAKE_AUTH`
        // validates.
        let reported_username = self.authenticated_username.lock().unwrap().clone();
        let username = reported_username.clone().unwrap_or_else(|| "user".to_string());

        let Some(broker_socket_path) = &self.config.broker_socket_path else {
            return CompositorSession::spawn(
                SessionType::User(username),
                "redfog-user-0",
                width as i32,
                height as i32,
                1.0,
                &self.config.user_app,
            )
            .map_err(|e| format!("failed to spawn redfog-user-0: {e}"));
        };

        use redfog_broker_protocol::{read_response, write_request, BrokerRequest, BrokerResponse};
        use tokio::io::BufReader;
        use tokio::net::UnixStream;

        let stream = UnixStream::connect(broker_socket_path)
            .await
            .map_err(|e| format!("failed to connect to broker at {broker_socket_path:?}: {e}"))?;
        let mut reader = BufReader::new(stream);

        if reported_username.is_none() {
            write_request(
                &mut reader,
                &BrokerRequest::Authenticate { username: username.clone(), password: String::new() },
            )
            .await
            .map_err(|e| format!("failed to send Authenticate to broker: {e}"))?;
            match read_response(&mut reader).await.map_err(|e| format!("failed to read Authenticate response: {e}"))? {
                BrokerResponse::Authenticate(Ok(())) => {}
                BrokerResponse::Authenticate(Err(e)) => return Err(format!("broker rejected authentication: {e}")),
                other => return Err(format!("unexpected broker response to Authenticate: {other:?}")),
            }
        }

        let session_id = self.next_broker_session_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed).to_string();
        write_request(
            &mut reader,
            &BrokerRequest::SpawnSession {
                session_id,
                username: username.clone(),
                width,
                height,
                socket_name: "redfog-user-0".to_string(),
                payload: self.config.user_app.clone(),
            },
        )
        .await
        .map_err(|e| format!("failed to send SpawnSession to broker: {e}"))?;
        let wayland_socket_path = match read_response(&mut reader).await.map_err(|e| format!("failed to read SpawnSession response: {e}"))? {
            BrokerResponse::SpawnSession(Ok(spawned)) => spawned.wayland_socket_path,
            BrokerResponse::SpawnSession(Err(e)) => return Err(format!("broker failed to spawn session: {e}")),
            other => return Err(format!("unexpected broker response to SpawnSession: {other:?}")),
        };

        CompositorSession::attach(
            SessionType::User(username),
            "redfog-user-0",
            std::path::PathBuf::from(wayland_socket_path),
            width as i32,
            height as i32,
            1.0,
        )
        .map_err(|e| format!("failed to attach to broker-spawned session: {e}"))
    }

    fn spawn_session(&self, kind: SessionType, width: u32, height: u32, compositor: CompositorSession) -> Result<RunningSession, String> {
        let socket_name = compositor.socket_name.clone();
        let input_forwarder: Box<dyn InputSink> = Box::new(
            InputForwarder::connect(&compositor.socket_path).map_err(|e| format!("failed to connect input forwarder: {e}"))?,
        );
        let audio_loopback = AudioLoopback::spawn(&socket_name)
            .map_err(|e| format!("failed to spawn audio loopback for {socket_name}: {e}"))?;

        // GStreamer's appsink callbacks run on GStreamer's own streaming
        // threads, not tokio worker threads — `tokio::spawn` would panic
        // there ("no reactor running"). Capture a `Handle` (valid from any
        // thread) instead, since we're called from within an async context.
        let handle = tokio::runtime::Handle::current();

        // NOTE: senders don't exist yet at this point — this runs from
        // `/launch`, before RTSP `PLAY` creates them (see `on_play`). Look
        // the *current* sender up fresh on every frame via `self`, rather
        // than capturing today's (always-`None`) value once here.
        let bitrate = self.config.bitrate_kbps;
        let this = self.arc_self();
        // Shared across the whole RTSP session (see the doc comment on these
        // fields) — NOT recreated here, so a Login->User handoff keeps
        // sequence numbers/frame indices/timestamps continuous.
        let video_packetizer = self.video_packetizer.lock().unwrap().clone();
        // RTP timestamps use a 90kHz clock (standard for video) — derived
        // from wall-clock time since streaming started rather than a fixed
        // per-frame increment, since frames aren't encoded at a perfectly
        // even interval.
        let stream_start = *self.stream_start.lock().unwrap();
        let video_pipeline = redfog_core::make_encoder_pipeline(compositor.video_source(), bitrate, {
            let handle = handle.clone();
            let this = this.clone();
            move |data, is_key_frame| {
                tracing::debug!("video encoder produced {} bytes, key_frame={is_key_frame}", data.len());
                let Some(sender) = this.shared.lock().unwrap().video_sender.clone() else { return };
                let rtp_timestamp = (stream_start.elapsed().as_secs_f64() * 90_000.0) as u32;
                let shards = video_packetizer.lock().unwrap().packetize(&data, is_key_frame, rtp_timestamp);
                handle.spawn(async move {
                    if let Err(e) = sender.send_shards(&shards).await {
                        tracing::warn!("video send failed: {e}");
                    }
                });
            }
        });

        let audio_packetizer = self.audio_packetizer.lock().unwrap().clone();
        let audio_pipeline = redfog_core::make_audio_pipeline(&audio_loopback, move |packet| {
            let Some(sender) = this.shared.lock().unwrap().audio_sender.clone() else { return };
            // Opus's RTP clock rate is 48kHz regardless of the actual sample rate.
            let rtp_timestamp = (stream_start.elapsed().as_secs_f64() * 48_000.0) as u32;
            let opus_packet = audio_packetizer.lock().unwrap().packetize(&packet, rtp_timestamp);
            handle.spawn(async move {
                if let Err(e) = sender.send_packet(&opus_packet).await {
                    tracing::warn!("audio send failed: {e}");
                }
            });
        });

        Ok(RunningSession {
            kind,
            width,
            height,
            compositor,
            input_forwarder,
            video_pipeline,
            audio_pipeline,
            _audio_loopback: audio_loopback,
        })
    }

    /// Bring the encoder/audio pipelines to PLAYING and, if this is the
    /// Login session, spawn the background task watching for it to exit.
    fn start_streaming(&self, session: RunningSession) {
        use gstreamer::prelude::*;
        let _ = session.video_pipeline.set_state(gstreamer::State::Playing);
        let _ = session.audio_pipeline.set_state(gstreamer::State::Playing);

        for (name, pipeline) in [("video", session.video_pipeline.clone()), ("audio", session.audio_pipeline.clone())] {
            let bus = pipeline.bus().unwrap();
            std::thread::spawn(move || {
                use gstreamer::MessageView;
                for msg in bus.iter_timed(gstreamer::ClockTime::NONE) {
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

        let is_login = matches!(session.kind, SessionType::Login);
        {
            let mut shared = self.shared.lock().unwrap();
            shared.state = State::Streaming { session };
        }

        if is_login {
            let this = self.arc_self();
            tokio::spawn(async move { this.watch_login_exit().await });
        }
    }

    async fn watch_login_exit(self: Arc<Self>) {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let status = {
                let mut shared = self.shared.lock().unwrap();
                match &mut shared.state {
                    State::Streaming { session } if matches!(session.kind, SessionType::Login) => {
                        session.compositor.try_wait().ok().flatten()
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
                if let Err(e) = self.handoff_to_user().await {
                    tracing::error!("failed to hand off to user session: {e}");
                }
            } else {
                tracing::error!("login compositor exited unexpectedly ({status}); resetting session instead of handing off");
                let _ = self.cancel();
            }
            return;
        }
    }

    async fn handoff_to_user(&self) -> Result<(), String> {
        use gstreamer::prelude::*;
        let old_login = {
            let mut shared = self.shared.lock().unwrap();
            match std::mem::replace(&mut shared.state, State::Idle) {
                State::Streaming { session } => session,
                other => {
                    shared.state = other;
                    return Err("handoff requested but no login session is streaming".to_string());
                }
            }
        };
        let (width, height) = (old_login.width, old_login.height);

        let _ = old_login.video_pipeline.set_state(gstreamer::State::Null);
        let _ = old_login.audio_pipeline.set_state(gstreamer::State::Null);
        old_login.compositor.terminate();

        let compositor = self.spawn_user_compositor(width, height).await?;
        let user_session = self.spawn_session(SessionType::User("user".to_string()), width, height, compositor)?;
        self.start_streaming(user_session);
        Ok(())
    }
}

impl LaunchHandler for SessionManager {
    fn launch(&self, width: u32, height: u32, _fps: u32, rikey: RemoteInputKey) -> Result<(), String> {
        {
            let mut shared = self.shared.lock().unwrap();
            // Real clients retry `/launch` on their own if the first attempt
            // is slow (KWin startup, D-Bus activation) — wait for that
            // in-flight spawn to finish rather than erroring a request that
            // would otherwise have succeeded once the first one lands.
            let (guard, timeout_result) = self
                .spawn_done
                .wait_timeout_while(shared, Duration::from_secs(15), |s| matches!(s.state, State::Spawning))
                .unwrap();
            shared = guard;
            if timeout_result.timed_out() && matches!(shared.state, State::Spawning) {
                return Err("timed out waiting for a concurrent launch to finish spawning".to_string());
            }
            // A client reconnecting to an already-active session — either it
            // gave up before RTSP reached PLAY (closed early, network
            // hiccup) and is retrying, or a window/tab was closed without a
            // clean disconnect (browsers don't reliably send one, confirmed
            // live) and a new window is taking over. Either way the
            // compositor is still alive and well: keep it running exactly as
            // is (no respawn, no re-login) and just accept the new client's
            // key. If already `Streaming`, `on_play` (triggered by this new
            // client's own RTSP PLAY) re-learns its address instead of
            // continuing to send to the old, now-gone one.
            if matches!(shared.state, State::Launched { .. } | State::Streaming { .. }) {
                drop(shared);
                self.set_rikey(rikey.key);
                return Ok(());
            }
            if !matches!(shared.state, State::Idle) {
                return Err("a session is already active".to_string());
            }
            // Claim `Idle` before releasing the lock — see `State::Spawning`'s
            // doc comment for why this has to happen atomically with the
            // check above rather than after `spawn_session()` returns.
            shared.state = State::Spawning;
        }
        // A genuinely new RTSP session (not a handoff) — start fresh RTP
        // sequence numbers/frame indices/timestamps. See the doc comment on
        // `SessionManager`'s packetizer fields for why this must NOT also
        // happen inside `handoff_to_user`.
        self.reset_stream_state();
        // A panic during spawn (e.g. a bad GStreamer pipeline description —
        // this has actually happened) must not skip the state reset below:
        // without `catch_unwind` here, `Spawning` would be stuck forever and
        // every future `/launch` would just time out waiting on a condvar
        // nothing will ever notify.
        let spawn_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let compositor = self.spawn_login_compositor(width, height)?;
            self.spawn_session(SessionType::Login, width, height, compositor)
        }));
        let session = match spawn_result {
            Ok(Ok(session)) => session,
            Ok(Err(e)) => {
                let mut shared = self.shared.lock().unwrap();
                shared.state = State::Idle;
                drop(shared);
                self.spawn_done.notify_all();
                return Err(e);
            }
            Err(panic) => {
                let mut shared = self.shared.lock().unwrap();
                shared.state = State::Idle;
                drop(shared);
                self.spawn_done.notify_all();
                std::panic::resume_unwind(panic);
            }
        };
        // Plain `rikey_cell` set, NOT `set_rikey` — this is a genuinely
        // fresh launch (state was `Idle`), so there's no stale peer to
        // disconnect, and the client's own brand-new ENet peer often
        // connects within the same short window right after `/launch`
        // returns. Flagging a disconnect sweep here caught that new peer
        // too and killed it immediately after connecting — confirmed live:
        // broke every single launch, not just reconnects.
        *self.rikey_cell.lock().unwrap() = Some(rikey.key);
        let mut shared = self.shared.lock().unwrap();
        shared.state = State::Launched { session };
        drop(shared);
        self.spawn_done.notify_all();
        Ok(())
    }

    fn resume(&self) -> Result<(), String> {
        Err("resume not yet implemented".to_string())
    }

    fn cancel(&self) -> Result<(), String> {
        let mut shared = self.shared.lock().unwrap();
        teardown_active_session(&mut shared);
        Ok(())
    }
}

/// Tears down whatever session is active (if any) and resets `shared.state`
/// to `Idle`. Caller must already hold the lock. Shared between `/cancel`
/// and `/launch`'s "take over an already-streaming session" path.
fn teardown_active_session(shared: &mut Shared) {
    if let State::Launched { session } | State::Streaming { session } = std::mem::replace(&mut shared.state, State::Idle) {
        // Dropping a GStreamer pipeline while it's still Playing isn't
        // just a "Trying to dispose element ... but it is in PLAYING"
        // warning — confirmed live via coredumpctl: it segfaulted the
        // whole server on every single `/cancel` (a PipeWire/GStreamer
        // worker thread racing the Rust-side Drop). `handoff_to_user`
        // already gets this right; `cancel` didn't.
        use gstreamer::prelude::*;
        let _ = session.video_pipeline.set_state(gstreamer::State::Null);
        let _ = session.audio_pipeline.set_state(gstreamer::State::Null);
        session.compositor.terminate();
    }
    // `state` isn't the only thing holding a session's resources alive:
    // these two `Arc`s (each wrapping a bound UDP socket) live here
    // separately and `state` being reset above does nothing to them.
    // Confirmed live: leaving them set kept the old sockets open, so
    // every session after the first got "Address already in use" on
    // ports 47998/48000 and streamed nothing.
    shared.video_sender = None;
    shared.audio_sender = None;
}

impl RtspHandler for SessionManager {
    fn on_announce(&self, _params: AnnouncedParams) {
        // v1: resolution/fps were already fixed at spawn time from /launch's
        // params; ANNOUNCE's values aren't applied yet (see plan doc).
    }

    fn on_play(&self) {
        let bind_addr = self.config.bind_addr;
        let video_port = self.config.video_port;
        let audio_port = self.config.audio_port;

        enum PlayKind {
            /// First PLAY for this launch — pipelines aren't running yet,
            /// senders need binding.
            Fresh(RunningSession),
            /// A new client's PLAY while a previous one was already
            /// `Streaming` — e.g. a window/tab was closed without a clean
            /// disconnect and a new window took over via `/launch`'s
            /// reconnect path. The compositor and pipelines are already
            /// running and must stay untouched; only the senders' learned
            /// client address needs to move to the new client.
            Retake(RunningSession),
        }

        let kind = {
            let mut shared = self.shared.lock().unwrap();
            // PLAY can race ahead of `/launch` finishing the (slow: KWin +
            // D-Bus + PipeWire) compositor spawn — confirmed live against
            // moonlight-web, which opens its RTSP connection and sends PLAY
            // without waiting for the `/launch` HTTP response. Wait for the
            // in-flight spawn to finish instead of dropping this PLAY, same
            // pattern `launch()` uses for a concurrent `/launch`.
            let (guard, timeout_result) = self
                .spawn_done
                .wait_timeout_while(shared, Duration::from_secs(15), |s| matches!(s.state, State::Spawning))
                .unwrap();
            shared = guard;
            if timeout_result.timed_out() && matches!(shared.state, State::Spawning) {
                tracing::warn!("PLAY received but launch is still spawning after 15s, giving up");
                return;
            }
            match std::mem::replace(&mut shared.state, State::Idle) {
                State::Launched { session } => Some(PlayKind::Fresh(session)),
                State::Streaming { session } => Some(PlayKind::Retake(session)),
                other => {
                    shared.state = other;
                    None
                }
            }
        };
        let Some(kind) = kind else {
            tracing::warn!("PLAY received but no session is in Launched/Streaming state");
            return;
        };

        let this = self.arc_self();
        tokio::spawn(async move {
            let (session, video_sender, audio_sender, is_retake) = match kind {
                PlayKind::Fresh(session) => {
                    let video_sender = match VideoSender::bind(bind_addr, video_port).await {
                        Ok(s) => Arc::new(s),
                        Err(e) => {
                            tracing::error!("failed to bind video sender: {e}");
                            return;
                        }
                    };
                    let audio_sender = match AudioSender::bind(bind_addr, audio_port).await {
                        Ok(s) => Arc::new(s),
                        Err(e) => {
                            tracing::error!("failed to bind audio sender: {e}");
                            return;
                        }
                    };
                    {
                        let mut shared = this.shared.lock().unwrap();
                        shared.video_sender = Some(video_sender.clone());
                        shared.audio_sender = Some(audio_sender.clone());
                    }
                    (session, video_sender, audio_sender, false)
                }
                PlayKind::Retake(session) => {
                    // Reuse the existing, already-bound senders — `PLAY` can
                    // arrive from the new client on its own new RTSP TCP
                    // connection before the old one's UDP sockets would ever
                    // be rebound anyway, and re-binding the same ports here
                    // would just fail with "Address already in use".
                    let existing = {
                        let shared = this.shared.lock().unwrap();
                        (shared.video_sender.clone(), shared.audio_sender.clone())
                    };
                    let (Some(video_sender), Some(audio_sender)) = existing else {
                        tracing::error!("retaking a Streaming session but its senders are missing — dropping to Idle");
                        let mut shared = this.shared.lock().unwrap();
                        shared.state = State::Idle;
                        return;
                    };
                    // The previous client's own stale `PING`(s) may already
                    // be sitting in these sockets' receive buffers (UDP has
                    // no teardown to stop them, and nothing's read from
                    // these sockets since) — without this, `wait_for_client`
                    // below picks one of those up instead of the new
                    // client's, permanently misrouting the stream to the
                    // old, now-gone address. Confirmed live.
                    video_sender.drain_pending();
                    audio_sender.drain_pending();
                    (session, video_sender, audio_sender, true)
                }
            };

            // `wait_for_client` loops forever if no PING ever arrives (a
            // session that gets abandoned before the client pings). Without
            // a timeout, that keeps this task's `Arc<VideoSender>` (and its
            // bound UDP socket) alive indefinitely — confirmed live: a stale
            // session left port 47998 bound, so every later session's own
            // bind failed with "Address already in use" and streamed
            // nothing at all. For a retake, the sender keeps sending to the
            // old (now-gone) client address until this overwrites it with
            // the new one — harmless, just wasted bandwidth in the meantime.
            tokio::spawn({
                let video_sender = video_sender.clone();
                async move {
                    match tokio::time::timeout(Duration::from_secs(30), video_sender.wait_for_client()).await {
                        Ok(Ok(addr)) => tracing::info!("video client announced itself at {addr}"),
                        Ok(Err(e)) => tracing::warn!("video wait_for_client failed: {e}"),
                        Err(_) => tracing::warn!("no video client PING received within 30s, giving up"),
                    }
                }
            });
            tokio::spawn({
                let audio_sender = audio_sender.clone();
                async move {
                    match tokio::time::timeout(Duration::from_secs(30), audio_sender.wait_for_client()).await {
                        Ok(Ok(addr)) => tracing::info!("audio client announced itself at {addr}"),
                        Ok(Err(e)) => tracing::warn!("audio wait_for_client failed: {e}"),
                        Err(_) => tracing::warn!("no audio client PING received within 30s, giving up"),
                    }
                }
            });

            if is_retake {
                // Pipelines are already Playing and `watch_login_exit` (if
                // this is a login session) is already running — just put the
                // session back, don't redo any of `start_streaming`'s setup.
                let mut shared = this.shared.lock().unwrap();
                shared.state = State::Streaming { session };
            } else {
                this.start_streaming(session);
            }
        });
    }
}

impl ControlEventHandler for SessionManager {
    fn on_input(&self, event: InputEvent) {
        let mut shared = self.shared.lock().unwrap();
        let session = match &mut shared.state {
            State::Streaming { session } => session,
            _ => return,
        };
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

    fn on_request_idr_frame(&self) {
        let shared = self.shared.lock().unwrap();
        if let State::Streaming { session } = &shared.state {
            redfog_core::request_keyframe(&session.video_pipeline);
        }
    }
}
