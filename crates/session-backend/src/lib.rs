//! Shared compositor-spawning abstraction: [`Backend`]/[`SpawnedCompositor`],
//! used by both the real production login flow (`redfog-moonlight::session`)
//! and standalone debug tooling (`viewer`) — so choosing a compositor
//! backend, or adding a third one, needs to be taught in exactly one place
//! rather than duplicated across every binary that spawns one.
//!
//! Deliberately synchronous except where a broker round-trip is genuinely
//! unavoidable network I/O (`spawn_user_compositor_via_broker`,
//! `spawn_gst_payload`'s broker branch) — and even those never call
//! `tokio::spawn` themselves. `redfog-moonlight` hit an unexplained
//! `Send`-bound failure wiring an owned `gstreamer::Element`-bearing struct
//! through `tokio::spawn` inside a long `async fn` chain (see its
//! `session.rs` doc comments) that no amount of isolated `assert_send`
//! checking predicted; the workaround was firing a fresh, independent
//! `tokio::spawn` rather than awaiting inline. Keeping that spawn decision
//! at the call site (background task for redfog-moonlight, `block_on` for a
//! non-async winit event loop in `viewer`) means this crate can't
//! reintroduce that bug — it never owns the choice of executor.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Duration;

use gstreamer::prelude::*;
use redfog_core::{CompositorSession, InputSink, SessionType, VideoSource};

pub use gst_backend::NestedSessionConfig;

/// Which compositor implementation backs a session — see [`SpawnedCompositor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    #[default]
    Kwin,
    GstWaylandDisplay,
}

impl Backend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Kwin => "kwin",
            Backend::GstWaylandDisplay => "gst-wayland-display",
        }
    }
}

/// Wire/env-var representation — `"kwin"` / `"gst-wayland-display"`, the
/// same strings `REDFOG_BACKEND` already used before this existed (see
/// `redfog-server::main`) and now also what a login screen reports over
/// `redfog-login-protocol`.
impl std::str::FromStr for Backend {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "kwin" => Ok(Backend::Kwin),
            "gst-wayland-display" => Ok(Backend::GstWaylandDisplay),
            other => Err(format!("unknown backend {other:?} (expected \"kwin\" or \"gst-wayland-display\")")),
        }
    }
}

/// Which compositor backend produced a session's video/input surface —
/// [`Backend`] selects between the first two for the *User* stage; the
/// third, [`Self::HeadlessLogin`], is what the *Login* stage always uses
/// instead, regardless of `Backend` (see [`spawn_login_compositor`]'s doc
/// comment for why Login doesn't need — and no longer even offers — a
/// choice here at all).
///
/// `Kwin`: broker-owned end-to-end (when spawned via the broker) or
/// caller-owned (direct spawn) — either way, `CompositorSession` itself
/// already abstracts that difference (see its `kwin_process: Option<Child>`).
///
/// `GstWaylandDisplay`: always caller-owned. The caller constructs and owns
/// the `waylanddisplaysrc` GStreamer element directly, including its
/// Wayland socket — the broker (if used at all) only grants the target user
/// access to that already-existing socket and spawns the nested payload
/// (e.g. Sway) as that user (`BrokerRequest::SpawnPayload`), which is why
/// its `payload_process` field is filled in later, by [`spawn_gst_payload`],
/// not at construction time (the payload's own Wayland socket doesn't exist
/// until the pipeline built around `element` reaches at least `Paused`).
///
/// `HeadlessLogin`: no compositor, no Wayland socket, no KWin or
/// gst-wayland-display at all — `redfog-login` renders its own frames
/// in-process (`tiny-skia`/`embedded-graphics`) and ships them over a
/// plain Unix stream (`redfog_login_protocol::render`) straight into a
/// GStreamer `appsrc`, which is what `element` here actually is.
pub enum SpawnedCompositor {
    Kwin(CompositorSession),
    GstWaylandDisplay {
        element: gstreamer::Element,
        runtime_dir: String,
        socket_path: PathBuf,
        socket_name: String,
        /// `None` until [`spawn_gst_payload`] fills it in, and `None`
        /// forever in the broker-spawned case — the broker owns that
        /// process, not us, same reasoning as `CompositorSession`'s own
        /// `kwin_process: None` for a broker-attached session.
        payload_process: Option<Child>,
    },
    HeadlessLogin {
        child: Child,
        /// The `appsrc` element frames are pushed into — see
        /// [`spawn_login_compositor`]'s background reader thread.
        element: gstreamer::Element,
        /// A live handle to the same connection the reader thread reads
        /// frames from — `try_clone()`'d again by `input_sink()` for
        /// writing input events back the other way (see
        /// `HeadlessLoginInputSink`). Kept here (not just handed off
        /// entirely to the reader thread) so `terminate()` can
        /// `shutdown()` it to stop that thread cleanly.
        input_stream: UnixStream,
        reader_thread: Option<std::thread::JoinHandle<()>>,
    },
}

impl SpawnedCompositor {
    pub fn video_source(&self) -> VideoSource {
        match self {
            Self::Kwin(session) => session.video_source(),
            Self::GstWaylandDisplay { element, .. } => VideoSource::Element(element.clone()),
            Self::HeadlessLogin { element, .. } => VideoSource::Element(element.clone()),
        }
    }

    /// KWin needs an explicit, fallible Wayland-protocol connection step;
    /// gst-wayland-display doesn't (`send_event` works on the element
    /// directly, no protocol handshake) — safe to call before the
    /// compositor's socket even exists, unlike KWin's. `HeadlessLogin`'s
    /// connection already exists by construction time (see
    /// `spawn_login_compositor`), so this is infallible in practice for it
    /// too — only `try_clone()`'s I/O error path can fail at all.
    pub fn input_sink(&self) -> Result<Box<dyn InputSink>, String> {
        match self {
            Self::Kwin(session) => Ok(Box::new(
                redfog_core::InputForwarder::connect(&session.socket_path)
                    .map_err(|e| format!("failed to connect input forwarder: {e}"))?,
            )),
            Self::GstWaylandDisplay { element, .. } => Ok(Box::new(gst_backend::GstInputSink::new(element.clone()))),
            Self::HeadlessLogin { input_stream, .. } => {
                let stream = input_stream.try_clone().map_err(|e| format!("failed to clone login frame stream: {e}"))?;
                Ok(Box::new(HeadlessLoginInputSink { stream }))
            }
        }
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        match self {
            Self::Kwin(session) => session.try_wait(),
            Self::GstWaylandDisplay { payload_process: Some(child), .. } => child.try_wait(),
            Self::GstWaylandDisplay { payload_process: None, .. } => Ok(None),
            Self::HeadlessLogin { child, .. } => child.try_wait(),
        }
    }

    pub fn terminate(self) {
        match self {
            Self::Kwin(session) => session.terminate(),
            Self::GstWaylandDisplay { mut payload_process, .. } => {
                if let Some(mut child) = payload_process.take() {
                    let _ = child.kill();
                }
            }
            Self::HeadlessLogin { mut child, input_stream, reader_thread, .. } => {
                // Unblocks the reader thread's blocking read (see
                // spawn_login_compositor) so it actually exits instead of
                // hanging around past the process it was serving.
                let _ = input_stream.shutdown(std::net::Shutdown::Both);
                let _ = child.kill();
                let _ = child.wait();
                if let Some(t) = reader_thread {
                    let _ = t.join();
                }
            }
        }
    }

    /// Resizes the compositor's own output surface — meaningful only for
    /// backends whose surface size isn't fixed at construction time.
    /// Returns whether anything actually changed, so callers know whether
    /// to rebuild their pipeline around a fresh [`Self::video_source`]
    /// afterward (KWin's `pipewiresrc` needs a fresh element per resize
    /// anyway — see `VideoSource::into_element` — so this is safe to do
    /// unconditionally when `true`; doing it when `false` would be either a
    /// no-op or, for `GstWaylandDisplay`/`HeadlessLogin`, actively wrong —
    /// see `spawn_gst_compositor`'s doc comment on why its
    /// `VideoSource::Element` can't just be re-added to a new pipeline).
    ///
    /// A documented no-op for `GstWaylandDisplay`/`HeadlessLogin`, not a
    /// limitation callers need to special-case per backend themselves:
    /// neither has a width/height that can change live (`waylanddisplaysrc`'s
    /// capsfilter caps are fixed once built — see `gst_backend::
    /// make_source_element`'s doc comment; `redfog-login`'s own canvas size
    /// is likewise fixed for the process's lifetime).
    pub fn resize(&self, width: i32, height: i32) -> bool {
        match self {
            Self::Kwin(session) => {
                session.capture_session.resize(width, height);
                true
            }
            Self::GstWaylandDisplay { .. } | Self::HeadlessLogin { .. } => false,
        }
    }
}

/// [`InputSink`] for [`SpawnedCompositor::HeadlessLogin`] — ships each call
/// straight to `redfog-login` as a [`redfog_login_protocol::render::
/// LoginInputEvent`], for it to apply to its own UI state directly (there's
/// no compositor/XKB left to do keymap translation, so `redfog-login`
/// itself maps evdev keycodes to characters — see its own doc comments).
struct HeadlessLoginInputSink {
    stream: UnixStream,
}

impl InputSink for HeadlessLoginInputSink {
    fn keyboard_key(&mut self, keycode: u32, pressed: bool) {
        let _ = redfog_login_protocol::render::write_input(&mut self.stream, &redfog_login_protocol::render::LoginInputEvent::KeyboardKey { keycode, pressed });
    }
    fn pointer_motion(&mut self, dx: f64, dy: f64) {
        let _ = redfog_login_protocol::render::write_input(&mut self.stream, &redfog_login_protocol::render::LoginInputEvent::MouseMoveRelative { dx, dy });
    }
    fn pointer_motion_absolute(&mut self, x: f64, y: f64) {
        let _ = redfog_login_protocol::render::write_input(&mut self.stream, &redfog_login_protocol::render::LoginInputEvent::MouseMoveAbsolute { x, y });
    }
    fn button(&mut self, button: u32, pressed: bool) {
        let _ = redfog_login_protocol::render::write_input(&mut self.stream, &redfog_login_protocol::render::LoginInputEvent::MouseButton { button, pressed });
    }
    fn axis(&mut self, axis: u32, value: f64) {
        let _ = redfog_login_protocol::render::write_input(&mut self.stream, &redfog_login_protocol::render::LoginInputEvent::MouseAxis { axis, value });
    }
}

/// Builds (but does not play, and does not spawn any payload for) a
/// `waylanddisplaysrc` element + its bookkeeping — shared by every caller
/// that needs a `Backend::GstWaylandDisplay` compositor, since the Login and
/// User stages (and a standalone viewer's single-session mode) are
/// otherwise identical; only the eventual payload-spawn mechanism differs
/// (see [`spawn_gst_payload`]).
pub fn spawn_gst_compositor(width: u32, height: u32, label: &str) -> Result<SpawnedCompositor, String> {
    let render_node = std::env::var("REDFOG_GST_RENDER_NODE").unwrap_or_else(|_| gst_backend::RENDER_NODE_SOFTWARE.to_string());
    // `label` (e.g. "redfog-login-0") only names *our own* runtime dir,
    // keeping different sessions' sockets in separate directories —
    // waylanddisplaysrc has no property to name its own socket; its
    // Smithay compositor always auto-picks "wayland-1" as the first socket
    // in a fresh runtime dir (confirmed live: assuming it would honor
    // `label` as the actual socket name left callers waiting on a file that
    // was never going to appear).
    let runtime_dir = format!("{}/{label}-runtime", redfog_core::default_runtime_dir());
    std::fs::create_dir_all(&runtime_dir).map_err(|e| format!("failed to create {runtime_dir}: {e}"))?;
    // waylanddisplaysrc reads XDG_RUNTIME_DIR synchronously inside
    // pipeline.set_state(Playing) — must be set before that, not after
    // (confirmed live: a RuntimeDirNotSet panic otherwise).
    std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir);
    let element = gst_backend::make_source_element(&render_node, width as i32, height as i32)?;
    let socket_name = "wayland-1";
    let socket_path = PathBuf::from(&runtime_dir).join(socket_name);
    Ok(SpawnedCompositor::GstWaylandDisplay {
        element,
        runtime_dir,
        socket_path,
        socket_name: socket_name.to_string(),
        payload_process: None,
    })
}

/// Spawns the Login compositor — always [`SpawnedCompositor::HeadlessLogin`],
/// regardless of [`Backend`]: unlike the User stage, Login never needs to
/// match whatever backend ends up chosen there (the two stages' video/audio
/// pipelines are torn down and rebuilt from scratch at handoff regardless —
/// see `redfog-moonlight::session`'s `handoff_to_user` — so there was never
/// a technical reason for them to match, only historical accident from when
/// `Backend` was a single global choice both stages inherited). This also
/// means Login never goes through a broker at all — it doesn't need to run
/// as any particular target user (see design.md's "Authentication: a real
/// graphical login screen") — and, now, doesn't depend on KWin or
/// gst-wayland-display being installed/built at all either.
///
/// `redfog-login` is entirely first-party code (see its own module doc
/// comments): rather than needing a real compositor just to host one small
/// form, it renders its own frames (`tiny-skia`/`embedded-graphics`, no
/// GPU) and ships them over a plain Unix stream
/// (`redfog_login_protocol::render`) straight into a GStreamer `appsrc` —
/// this function's whole job is standing that stream up: bind a socket,
/// spawn `login_app` pointed at it via env vars, accept the one connection
/// it makes, then hand frames arriving on it to a background thread that
/// pushes them into the `appsrc`.
pub fn spawn_login_compositor(login_app: &[String], width: u32, height: u32) -> Result<SpawnedCompositor, String> {
    if login_app.is_empty() {
        return Err("login_app must not be empty".to_string());
    }

    let socket_path = format!("{}/login-frame-{}.sock", redfog_core::default_runtime_dir(), std::process::id());
    if let Some(parent) = Path::new(&socket_path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("failed to create {parent:?}: {e}"))?;
    }
    let _ = std::fs::remove_file(&socket_path); // stale socket from a previous run
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).map_err(|e| format!("failed to bind login frame socket {socket_path}: {e}"))?;
    listener.set_nonblocking(true).map_err(|e| format!("failed to set login frame socket nonblocking: {e}"))?;

    let mut cmd = std::process::Command::new(&login_app[0]);
    cmd.args(&login_app[1..]);
    cmd.env("REDFOG_LOGIN_FRAME_SOCKET", &socket_path);
    cmd.env("REDFOG_LOGIN_WIDTH", width.to_string());
    cmd.env("REDFOG_LOGIN_HEIGHT", height.to_string());
    cmd.stdout(std::process::Stdio::inherit()).stderr(std::process::Stdio::inherit());
    let mut child = cmd.spawn().map_err(|e| format!("failed to spawn {login_app:?}: {e}"))?;

    // Poll-accept with a timeout — matches the polling style already used
    // elsewhere in this codebase (e.g. gst_backend::wait_for_wayland_socket)
    // rather than a blocking accept with no bound, so a login_app that
    // fails to even start (bad path, missing library, ...) doesn't hang
    // this call forever.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(format!("{login_app:?} exited before connecting to the login frame socket ({status})"));
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    return Err(format!("{login_app:?} never connected to the login frame socket within 10s"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("failed to accept login frame connection: {e}")),
        }
    };
    let _ = std::fs::remove_file(&socket_path); // no longer needed once connected
    stream.set_nonblocking(false).map_err(|e| format!("failed to set login frame stream blocking: {e}"))?;

    let appsrc = gstreamer::ElementFactory::make("appsrc")
        .name("login-appsrc")
        .property("format", gstreamer::Format::Time)
        .property("is-live", true)
        .property("block", false)
        .build()
        .map_err(|e| format!("failed to create appsrc element: {e}"))?;
    let caps = gstreamer::Caps::builder("video/x-raw")
        .field("format", "RGBA")
        .field("width", width as i32)
        .field("height", height as i32)
        .field("framerate", gstreamer::Fraction::new(30, 1))
        .build();
    appsrc.set_property("caps", &caps);

    let app_src = appsrc
        .clone()
        .dynamic_cast::<gstreamer_app::AppSrc>()
        .map_err(|_| "appsrc element factory didn't produce an AppSrc".to_string())?;

    let reader_stream = stream.try_clone().map_err(|e| format!("failed to clone login frame stream: {e}"))?;
    let reader_thread = std::thread::spawn(move || {
        let mut reader = reader_stream;
        loop {
            match redfog_login_protocol::render::read_message(&mut reader) {
                Ok(Some(redfog_login_protocol::render::Message::Frame { rgba, .. })) => {
                    let mut buffer = gstreamer::Buffer::with_size(rgba.len()).expect("buffer allocation");
                    {
                        let buffer_mut = buffer.get_mut().expect("freshly allocated buffer is never shared");
                        buffer_mut.copy_from_slice(0, &rgba).expect("buffer sized exactly for rgba");
                    }
                    if app_src.push_buffer(buffer).is_err() {
                        break; // pipeline gone/EOS
                    }
                }
                Ok(Some(redfog_login_protocol::render::Message::Input(_))) => {} // wrong direction on this stream, ignore
                Ok(None) => break,                                                // redfog-login exited/closed the connection
                Err(e) => {
                    tracing::warn!("login frame socket read error: {e}");
                    break;
                }
            }
        }
    });

    Ok(SpawnedCompositor::HeadlessLogin { child, element: appsrc, input_stream: stream, reader_thread: Some(reader_thread) })
}

/// Spawns the User compositor directly (no broker) — standalone use.
pub fn spawn_user_compositor_direct(backend: Backend, username: &str, user_app: &[String], width: u32, height: u32) -> Result<SpawnedCompositor, String> {
    match backend {
        Backend::Kwin => CompositorSession::spawn(SessionType::User(username.to_string()), "redfog-user-0", width as i32, height as i32, 1.0, user_app)
            .map(SpawnedCompositor::Kwin)
            .map_err(|e| format!("failed to spawn redfog-user-0: {e}")),
        Backend::GstWaylandDisplay => spawn_gst_compositor(width, height, "redfog-user-0"),
    }
}

/// Acquires the User compositor via the broker: `Authenticate` (unless
/// `skip_authenticate` — the caller already validated credentials some
/// other way, e.g. `redfog-moonlight`'s login-report flow) then
/// `SpawnSession`/`SpawnPayload` depending on `backend`.
///
/// For `Kwin`, this fully spawns the compositor (the broker owns it
/// end-to-end). For `GstWaylandDisplay`, this only builds the local
/// element/socket (identical to [`spawn_gst_compositor`]) — the broker
/// `SpawnPayload` call happens later, via [`spawn_gst_payload`], once that
/// socket actually exists.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_user_compositor_via_broker(
    backend: Backend,
    broker_socket_path: &Path,
    session_id: String,
    username: &str,
    password: &str,
    skip_authenticate: bool,
    user_app: &[String],
    width: u32,
    height: u32,
) -> Result<SpawnedCompositor, String> {
    use redfog_broker_protocol::{read_response, write_request, BrokerRequest, BrokerResponse};
    use tokio::io::BufReader;
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(broker_socket_path)
        .await
        .map_err(|e| format!("failed to connect to broker at {broker_socket_path:?}: {e}"))?;
    let mut reader = BufReader::new(stream);

    if !skip_authenticate {
        write_request(&mut reader, &BrokerRequest::Authenticate { username: username.to_string(), password: password.to_string() })
            .await
            .map_err(|e| format!("failed to send Authenticate to broker: {e}"))?;
        match read_response(&mut reader).await.map_err(|e| format!("failed to read Authenticate response: {e}"))? {
            BrokerResponse::Authenticate(Ok(())) => {}
            BrokerResponse::Authenticate(Err(e)) => return Err(format!("broker rejected authentication: {e}")),
            other => return Err(format!("unexpected broker response to Authenticate: {other:?}")),
        }
    }

    match backend {
        Backend::Kwin => {
            write_request(
                &mut reader,
                &BrokerRequest::SpawnSession {
                    session_id,
                    username: username.to_string(),
                    width,
                    height,
                    socket_name: "redfog-user-0".to_string(),
                    payload: user_app.to_vec(),
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
                SessionType::User(username.to_string()),
                "redfog-user-0",
                PathBuf::from(wayland_socket_path),
                width as i32,
                height as i32,
                1.0,
            )
            .map(SpawnedCompositor::Kwin)
            .map_err(|e| format!("failed to attach to broker-spawned session: {e}"))
        }
        // The broker interaction for this backend (SpawnPayload) happens
        // later, in spawn_gst_payload, once the socket built here actually
        // exists — see SpawnedCompositor's doc comment. Authenticate above
        // still applies.
        Backend::GstWaylandDisplay => spawn_gst_compositor(width, height, "redfog-user-0"),
    }
}

/// Waits for a `Backend::GstWaylandDisplay` compositor's Wayland socket to
/// actually exist (it doesn't until the pipeline built around it reaches
/// `Playing` — see [`spawn_gst_compositor`]'s doc comment), then spawns the
/// nested payload — directly if `broker` is `None`, otherwise via the
/// broker's `SpawnPayload`. Returns the direct child handle (`None` if
/// broker-spawned — the broker owns that process, not us).
///
/// Takes the compositor's `runtime_dir`/`socket_path`/`socket_name` as
/// plain values rather than a `&SpawnedCompositor` reference deliberately —
/// `redfog-moonlight` fires this from an independent `tokio::spawn`ed
/// background task (see its `spawn_gst_payload_in_background`'s doc
/// comment for the unexplained `Send`-bound issue that pattern works around),
/// and keeping every value crossing that boundary as plain owned
/// strings/paths, never an `Element`-bearing `SpawnedCompositor`, is a
/// deliberate part of that workaround.
pub async fn spawn_gst_payload(
    runtime_dir: &str,
    socket_path: &Path,
    socket_name: &str,
    nested: &NestedSessionConfig,
    broker: Option<(&Path, String, String)>,
    timeout: Duration,
) -> Result<Option<Child>, String> {
    gst_backend::wait_for_wayland_socket(runtime_dir, socket_name, timeout)?;

    match broker {
        None => gst_backend::spawn_nested_session(nested, runtime_dir, socket_name).map(Some),
        Some((broker_socket_path, session_id, username)) => {
            let (argv, env) = gst_backend::command_and_env(nested, runtime_dir, socket_name)?;
            use redfog_broker_protocol::{read_response, write_request, BrokerRequest, BrokerResponse};
            use tokio::io::BufReader;
            use tokio::net::UnixStream;

            let stream = UnixStream::connect(broker_socket_path)
                .await
                .map_err(|e| format!("failed to connect to broker at {broker_socket_path:?}: {e}"))?;
            let mut reader = BufReader::new(stream);
            write_request(
                &mut reader,
                &BrokerRequest::SpawnPayload {
                    session_id,
                    username,
                    socket_path: socket_path.to_string_lossy().to_string(),
                    runtime_dir: runtime_dir.to_string(),
                    argv,
                    env,
                },
            )
            .await
            .map_err(|e| format!("failed to send SpawnPayload to broker: {e}"))?;
            match read_response(&mut reader).await.map_err(|e| format!("failed to read SpawnPayload response: {e}"))? {
                BrokerResponse::SpawnPayload(Ok(())) => Ok(None),
                BrokerResponse::SpawnPayload(Err(e)) => Err(format!("broker failed to spawn payload: {e}")),
                other => Err(format!("unexpected broker response to SpawnPayload: {other:?}")),
            }
        }
    }
}
