//! Login -> User `CompositorSession` handoff state machine, driven by RTSP
//! events. The RTSP-driven analogue of `kwin-viewer`'s winit-loop handoff:
//! `/launch` spawns the Login compositor and streams it; once it exits
//! (login succeeded), we spawn the User compositor and repoint the video/
//! audio/input pipelines at it — same two-session dance, different trigger.
//!
//! Single session at a time for this iteration — a second `/launch` while
//! one is active is rejected, matching a reasonable v1 restriction.

use std::net::IpAddr;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::Duration;

use redfog_core::{AudioLoopback, CompositorSession, InputForwarder, SessionType};

use crate::audio::{AudioPacketizer, AudioSender};
use crate::control::{ControlEventHandler, InputEvent};
use crate::pairing::{LaunchHandler, RemoteInputKey};
use crate::rtsp::{AnnouncedParams, RtspHandler};
use crate::video::{VideoPacketizer, VideoSender};

pub struct SessionConfig {
    pub bind_addr: IpAddr,
    pub video_port: u16,
    pub audio_port: u16,
    /// Command to run for the real desktop session once login succeeds
    /// (e.g. `["plasmashell", "--no-respawn"]`).
    pub user_app: Vec<String>,
    pub bitrate_kbps: u32,
}

struct RunningSession {
    kind: SessionType,
    width: u32,
    height: u32,
    compositor: CompositorSession,
    input_forwarder: InputForwarder,
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
        });
        let _ = this.self_ref.set(Arc::downgrade(&this));
        this
    }

    /// Shared cell for `control::ControlServer` to read the current
    /// session's `rikey` from.
    pub fn rikey_cell(&self) -> Arc<Mutex<Option<[u8; 16]>>> {
        self.rikey_cell.clone()
    }

    /// An owned `Arc<Self>` for moving into spawned tasks — trait methods
    /// here take plain `&self` (so `SessionManager` stays usable as a
    /// trait object across `LaunchHandler`/`RtspHandler`/`ControlEventHandler`
    /// without relying on `Arc<Self>`-receiver methods on trait objects).
    fn arc_self(&self) -> Arc<Self> {
        self.self_ref.get().and_then(Weak::upgrade).expect("self_ref set in new()")
    }

    fn spawn_session(&self, kind: SessionType, width: u32, height: u32) -> Result<RunningSession, String> {
        let (socket_name, payload): (&str, Vec<String>) = match &kind {
            // TEMPORARY: swapped to glxgears to test the damage-source theory
            // (continuous animation vs. redfog-login's one-shot static paint).
            SessionType::Login => ("redfog-login-0", vec!["glxgears".to_string()]),
            SessionType::User(_) => ("redfog-user-0", self.config.user_app.clone()),
        };

        let compositor = CompositorSession::spawn(kind.clone(), socket_name, width as i32, height as i32, 1.0, &payload)
            .map_err(|e| format!("failed to spawn {socket_name}: {e}"))?;
        let input_forwarder =
            InputForwarder::connect(&compositor.socket_path).map_err(|e| format!("failed to connect input forwarder: {e}"))?;
        let audio_loopback =
            AudioLoopback::spawn(socket_name).map_err(|e| format!("failed to spawn audio loopback for {socket_name}: {e}"))?;

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
        let video_packetizer = Arc::new(Mutex::new(VideoPacketizer::new()));
        // RTP timestamps use a 90kHz clock (standard for video) — derived
        // from wall-clock time since streaming started rather than a fixed
        // per-frame increment, since frames aren't encoded at a perfectly
        // even interval.
        let stream_start = std::time::Instant::now();
        let video_pipeline = redfog_core::make_encoder_pipeline(compositor.pipewire_node_id, bitrate, {
            let handle = handle.clone();
            let this = this.clone();
            move |data, is_key_frame| {
                tracing::debug!("video encoder produced {} bytes, key_frame={is_key_frame}", data.len());
                let Some(sender) = this.shared.lock().unwrap().video_sender.clone() else { return };
                let rtp_timestamp = (stream_start.elapsed().as_secs_f64() * 90_000.0) as u32;
                let shards = video_packetizer.lock().unwrap().packetize(&data, is_key_frame, rtp_timestamp);
                handle.spawn(async move {
                    let _ = sender.send_shards(&shards).await;
                });
            }
        });

        let audio_packetizer = Arc::new(Mutex::new(AudioPacketizer::new()));
        let audio_pipeline = redfog_core::make_audio_pipeline(&audio_loopback, move |packet| {
            let Some(sender) = this.shared.lock().unwrap().audio_sender.clone() else { return };
            // Opus's RTP clock rate is 48kHz regardless of the actual sample rate.
            let rtp_timestamp = (stream_start.elapsed().as_secs_f64() * 48_000.0) as u32;
            let opus_packet = audio_packetizer.lock().unwrap().packetize(&packet, rtp_timestamp);
            handle.spawn(async move {
                let _ = sender.send_packet(&opus_packet).await;
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
            let exited = {
                let mut shared = self.shared.lock().unwrap();
                match &mut shared.state {
                    State::Streaming { session } if matches!(session.kind, SessionType::Login) => {
                        session.compositor.try_wait().ok().flatten().is_some()
                    }
                    _ => return, // no longer the login session (already handed off, or idle)
                }
            };
            if exited {
                tracing::info!("login session exited, handing off to user session");
                if let Err(e) = self.handoff_to_user() {
                    tracing::error!("failed to hand off to user session: {e}");
                }
                return;
            }
        }
    }

    fn handoff_to_user(&self) -> Result<(), String> {
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

        let user_session = self.spawn_session(SessionType::User("user".to_string()), width, height)?;
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
            // A client that gave up before RTSP reached PLAY (closed early,
            // network hiccup, etc.) previously had no way back in short of
            // an explicit /cancel — the compositor it spawned is still
            // alive and well, so just let the retry reconnect to it rather
            // than hard-erroring "a session is already active".
            if matches!(shared.state, State::Launched { .. }) {
                drop(shared);
                *self.rikey_cell.lock().unwrap() = Some(rikey.key);
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
        // A panic during spawn (e.g. a bad GStreamer pipeline description —
        // this has actually happened) must not skip the state reset below:
        // without `catch_unwind` here, `Spawning` would be stuck forever and
        // every future `/launch` would just time out waiting on a condvar
        // nothing will ever notify.
        let spawn_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.spawn_session(SessionType::Login, width, height)
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
        Ok(())
    }
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

        let session = {
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
                State::Launched { session } => Some(session),
                other => {
                    shared.state = other;
                    None
                }
            }
        };
        let Some(session) = session else {
            tracing::warn!("PLAY received but no session is in Launched state");
            return;
        };

        let this = self.arc_self();
        tokio::spawn(async move {
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

            // `wait_for_client` loops forever if no PING ever arrives (a
            // session that gets abandoned before the client pings). Without
            // a timeout, that keeps this task's `Arc<VideoSender>` (and its
            // bound UDP socket) alive indefinitely — confirmed live: a stale
            // session left port 47998 bound, so every later session's own
            // bind failed with "Address already in use" and streamed
            // nothing at all.
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

            this.start_streaming(session);
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
        let fwd = &session.input_forwarder;
        match event {
            InputEvent::KeyDown { keycode } => fwd.fake_input.keyboard_key(keycode, 1),
            InputEvent::KeyUp { keycode } => fwd.fake_input.keyboard_key(keycode, 0),
            InputEvent::MouseMoveRelative { dx, dy } => fwd.fake_input.pointer_motion(dx as f64, dy as f64),
            InputEvent::MouseMoveAbsolute { x, y, screen_width, screen_height } => {
                if screen_width > 0 && screen_height > 0 {
                    // Client viewport coords -> our actual output resolution.
                    let scaled_x = x as f64 / screen_width as f64 * session.width as f64;
                    let scaled_y = y as f64 / screen_height as f64 * session.height as f64;
                    fwd.fake_input.pointer_motion_absolute(scaled_x, scaled_y);
                }
            }
            InputEvent::MouseButtonDown { button } => fwd.fake_input.button(button, 1),
            InputEvent::MouseButtonUp { button } => fwd.fake_input.button(button, 0),
            InputEvent::ScrollVertical { amount } => fwd.fake_input.axis(0, amount as f64),
            InputEvent::ScrollHorizontal { amount } => fwd.fake_input.axis(1, amount as f64),
        }
        let _ = fwd.conn.flush();
    }

    fn on_request_idr_frame(&self) {
        let shared = self.shared.lock().unwrap();
        if let State::Streaming { session } = &shared.state {
            redfog_core::request_keyframe(&session.video_pipeline);
        }
    }
}
