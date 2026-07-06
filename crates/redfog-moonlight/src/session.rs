//! Login -> User `CompositorSession` handoff state machine, driven by RTSP
//! events. The RTSP-driven analogue of `kwin-viewer`'s winit-loop handoff:
//! `/launch` spawns the Login compositor and streams it; once it exits
//! (login succeeded), we spawn the User compositor and repoint the video/
//! audio/input pipelines at it — same two-session dance, different trigger.
//!
//! Single session at a time for this iteration — a second `/launch` while
//! one is active is rejected, matching a reasonable v1 restriction.

use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock, Weak};

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
            SessionType::Login => ("redfog-login-0", vec!["target/release/redfog-login".to_string()]),
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
        let video_pipeline = redfog_core::make_encoder_pipeline(compositor.pipewire_node_id, bitrate, {
            let handle = handle.clone();
            let this = this.clone();
            move |data, is_key_frame| {
                let Some(sender) = this.shared.lock().unwrap().video_sender.clone() else { return };
                let shards = video_packetizer.lock().unwrap().packetize(&data, is_key_frame, 0, 0);
                handle.spawn(async move {
                    let _ = sender.send_shards(&shards).await;
                });
            }
        });

        let audio_packetizer = Arc::new(Mutex::new(AudioPacketizer::new()));
        let audio_pipeline = redfog_core::make_audio_pipeline(&audio_loopback, move |packet| {
            let Some(sender) = this.shared.lock().unwrap().audio_sender.clone() else { return };
            let opus_packet = audio_packetizer.lock().unwrap().packetize(&packet, 0);
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
            let shared = self.shared.lock().unwrap();
            if !matches!(shared.state, State::Idle) {
                return Err("a session is already active".to_string());
            }
        }
        let session = self.spawn_session(SessionType::Login, width, height)?;
        *self.rikey_cell.lock().unwrap() = Some(rikey.key);
        let mut shared = self.shared.lock().unwrap();
        shared.state = State::Launched { session };
        Ok(())
    }

    fn resume(&self) -> Result<(), String> {
        Err("resume not yet implemented".to_string())
    }

    fn cancel(&self) -> Result<(), String> {
        let mut shared = self.shared.lock().unwrap();
        if let State::Launched { session } | State::Streaming { session } = std::mem::replace(&mut shared.state, State::Idle) {
            session.compositor.terminate();
        }
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

            tokio::spawn({
                let video_sender = video_sender.clone();
                async move {
                    if let Ok(addr) = video_sender.wait_for_client().await {
                        tracing::info!("video client announced itself at {addr}");
                    }
                }
            });
            tokio::spawn({
                let audio_sender = audio_sender.clone();
                async move {
                    if let Ok(addr) = audio_sender.wait_for_client().await {
                        tracing::info!("audio client announced itself at {addr}");
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
