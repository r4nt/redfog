//! Second `redfog_core::VideoSource`/`InputSink` backend, built on
//! `gst-wayland-display` (https://github.com/games-on-whales/gst-wayland-display)
//! instead of KWin — exists to prove `redfog-core`'s trait boundary isn't
//! secretly KWin-shaped. Where KWin is spawned as a subprocess and connected
//! to over a Wayland socket for capture (`kwin-capture`'s private
//! `zkde_screencast_unstable_v1` protocol) and input
//! (`org_kde_kwin_fake_input`), this backend embeds the compositor
//! in-process as a GStreamer element (`waylanddisplaysrc`): capture is just
//! that element's video pads, and input is sent to it directly as
//! `CustomUpstream` GStreamer events — no separate protocol connection step
//! for either.
//!
//! `waylanddisplaysrc` itself is single-window and has no Xwayland support
//! (see gst-wayland-display's README) — nowhere near enough to host a real
//! desktop. So this backend's actual payload is a *nested* Sway instance
//! (configurable — see [`NestedSessionConfig`]), connected to
//! `waylanddisplaysrc`'s own Wayland socket as an ordinary client, giving
//! real multi-window management and Xwayland for free without either of
//! those needing any backend-specific code here at all.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gst::prelude::*;

use redfog_core::InputSink;

/// `render-node` value for `waylanddisplaysrc` when no working GPU
/// acceleration is available — see project notes on the NVIDIA GBM issue.
/// Pass a real DRM render node path (e.g. `/dev/dri/renderD128`) instead
/// when one works.
pub const RENDER_NODE_SOFTWARE: &str = "software";

/// Builds the `waylanddisplaysrc` element, wrapped in a `capsfilter` forcing
/// `width`/`height` (not yet part of a pipeline, not yet started — the
/// caller adds the returned element to a `gst::Pipeline` via
/// `redfog_core::VideoSource::Element`, same as any other source). Its
/// Wayland socket doesn't exist until that pipeline reaches at least
/// `Paused` — see [`wait_for_wayland_socket`].
///
/// The capsfilter is required, not cosmetic: `waylanddisplaysrc` has no
/// width/height *property* (only a wide negotiable caps range, confirmed
/// via `gst-inspect-1.0`) and no default resolution of its own — left
/// unconstrained, it negotiates down to the smallest satisfiable caps,
/// which is a literal `1x1` frame (confirmed live: an unconstrained
/// pipeline renders nothing, the exact "window opens but shows nothing"
/// symptom this fixes). KWin's `VideoSource::PipeWireNode` path doesn't
/// need this — the real resolution is already baked into the PipeWire
/// stream itself by the time it reaches `pipewiresrc`.
pub fn make_source_element(render_node: &str, width: i32, height: i32) -> Result<gst::Element, String> {
    let waylandsrc = gst::ElementFactory::make("waylanddisplaysrc")
        .name("waylanddisplaysrc")
        .property("render-node", render_node)
        .build()
        .map_err(|e| {
            format!(
                "failed to create waylanddisplaysrc element: {e} — is gst-wayland-display built \
                 and its plugin directory on GST_PLUGIN_PATH?"
            )
        })?;
    let caps = gst::Caps::builder("video/x-raw")
        .field("width", width)
        .field("height", height)
        .field("framerate", gst::Fraction::new(30, 1))
        .build();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .name("src")
        .property("caps", &caps)
        .build()
        .map_err(|e| format!("failed to create capsfilter element: {e}"))?;

    let bin = gst::Bin::builder().name("waylanddisplaysrc-bin").build();
    bin.add_many([&waylandsrc, &capsfilter]).map_err(|e| format!("failed to add elements to bin: {e}"))?;
    waylandsrc.link(&capsfilter).map_err(|e| format!("failed to link waylanddisplaysrc to capsfilter: {e}"))?;

    let src_pad = capsfilter.static_pad("src").ok_or("capsfilter has no src pad")?;
    let ghost_pad = gst::GhostPad::with_target(&src_pad).map_err(|e| format!("failed to create ghost pad: {e}"))?;
    ghost_pad.set_active(true).map_err(|e| format!("failed to activate ghost pad: {e}"))?;
    bin.add_pad(&ghost_pad).map_err(|e| format!("failed to add ghost pad to bin: {e}"))?;

    Ok(bin.upcast())
}

/// Polls `{runtime_dir}/{socket_name}` until it appears (created by
/// `waylanddisplaysrc`'s Smithay compositor once the pipeline is playing)
/// or `timeout` elapses.
pub fn wait_for_wayland_socket(runtime_dir: &str, socket_name: &str, timeout: Duration) -> Result<(), String> {
    let path = std::path::Path::new(runtime_dir).join(socket_name);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!("{path:?} did not appear within {timeout:?}"))
}

/// The nested Wayland client run inside `waylanddisplaysrc`'s compositor —
/// see the module doc comment for why this needs to be a full compositor
/// (Sway) rather than a bare app. Defaults to Sway; anything that behaves
/// like a Wayland compositor/app works equally well, configured by the
/// caller (see `redfog-broker`'s equivalent `payload: &[String]` pattern).
pub struct NestedSessionConfig {
    pub command: Vec<String>,
    /// Value for `XDG_SESSION_DESKTOP`/`XDG_CURRENT_DESKTOP` — matches
    /// Wolf's own real, confirmed-working env (`/tmp/sway-env` dump from a
    /// live `wolf` container: `XDG_SESSION_DESKTOP=sway XDG_CURRENT_DESKTOP=sway`),
    /// not a guess. `"sway"` for the default payload; change if
    /// [`NestedSessionConfig::command`] runs something else.
    pub desktop_name: String,
    /// Sets `__GLX_VENDOR_LIBRARY_NAME` for nested apps if given (e.g.
    /// `"nvidia"`). Not universal — Wolf's own real working env doesn't set
    /// this at all — but confirmed live as *required* on a machine with
    /// more than one GLVND EGL vendor file installed (here:
    /// `/usr/share/glvnd/egl_vendor.d/10_nvidia.json` AND `50_mesa.json`):
    /// without it, GLXGears silently falls back to Mesa software rendering
    /// (confirmed via `nvidia-smi` showing no process at all); with it, it
    /// shows up using the real GPU. `None` leaves GLVND's own
    /// auto-detection alone, which is correct on a machine with only one
    /// vendor file present (e.g. Wolf's containers).
    pub glx_vendor: Option<String>,
}

impl Default for NestedSessionConfig {
    fn default() -> Self {
        Self { command: vec!["sway".to_string()], desktop_name: "sway".to_string(), glx_vendor: None }
    }
}

/// Spawns [`NestedSessionConfig::command`] with `WAYLAND_DISPLAY` pointed at
/// `waylanddisplaysrc`'s socket, wrapped in `dbus-run-session` so it gets
/// its own private D-Bus session bus — same reasoning as the KWin backend's
/// use of `dbus-run-session` (see `redfog-broker/src/session.rs`): Sway
/// itself doesn't hard-require D-Bus, but plenty of ordinary apps run
/// inside it do (portals, `alacritty`, etc.), and without a bus at all they
/// print "Failed to connect to user scope bus" warnings — confirmed live,
/// this fixes it, same as GOW/Wolf's own compositor hosting does. Call only
/// after [`wait_for_wayland_socket`] confirms the socket exists.
///
/// `waylanddisplaysrc`'s own `render_node` (see [`make_source_element`])
/// only controls how *it* composites the scene — it does not restrict what
/// render node nested apps use for their own rendering, which they pick
/// independently via standard EGL/GLX resolution (confirmed live: a nested
/// `glxgears` used the real NVIDIA GPU, visible in `nvidia-smi`, even while
/// the outer compositor ran with `render-node=software`). Sway's own
/// nested-Wayland backend currently hits a real, reproducible wlroots bug
/// (`legacy_drm_handle_device` assertion, `backend/wayland/backend.c`) when
/// the *host* compositor advertises GPU-backed DMA-BUF surfaces — not
/// something this function can work around; see `project-nvidia-gbm`
/// memory. `RENDER_NODE_SOFTWARE` avoids it entirely while nested apps
/// still get full GPU rendering, so it's a reasonable default rather than
/// merely a workaround.
///
/// GPU compositing (the *outer* compositor rendering with acceleration) is
/// a separate, deferred follow-up: Wolf's own real, working NVIDIA setup
/// (confirmed via an env dump taken from a live `wolf` container, not
/// guessed) requests CUDA memory buffers from `waylanddisplaysrc`
/// (`WOLF_VIDEO_BUFFER_CAPS=video/x-raw(memory:CUDAMemory)`) rather than
/// plain DMA-BUF/GBM — a different, likely crash-avoiding negotiation path
/// this crate doesn't implement yet. That same env dump is also the source
/// for `XDG_SESSION_TYPE`/`XDG_SESSION_DESKTOP`/`XDG_CURRENT_DESKTOP`/
/// `SWAYSOCK` below; notably absent from it: `WLR_NO_HARDWARE_CURSORS`,
/// `GBM_BACKEND`, `WLR_DRM_DEVICES`, `WLR_BACKENDS` — none of those are
/// part of Wolf's actual working configuration, despite being
/// plausible-sounding suggestions tried earlier in this backend's
/// development. [`NestedSessionConfig::glx_vendor`] is a separate,
/// *nested-app* rendering concern (not outer-compositor GPU compositing at
/// all) — see its own doc comment.
pub fn spawn_nested_session(
    config: &NestedSessionConfig,
    runtime_dir: &str,
    socket_name: &str,
) -> Result<Child, String> {
    if config.command.is_empty() {
        return Err("NestedSessionConfig::command must not be empty".to_string());
    }
    let mut cmd = Command::new("dbus-run-session");
    cmd.arg("--")
        .args(&config.command)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("WAYLAND_DISPLAY", socket_name)
        .env("XDG_SESSION_TYPE", "wayland")
        .env("XDG_SESSION_DESKTOP", &config.desktop_name)
        .env("XDG_CURRENT_DESKTOP", &config.desktop_name)
        .env("SWAYSOCK", format!("{runtime_dir}/sway.socket"))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(vendor) = &config.glx_vendor {
        cmd.env("__GLX_VENDOR_LIBRARY_NAME", vendor);
    }
    cmd.spawn().map_err(|e| format!("failed to spawn nested session {config:?}: {e}", config = config.command))
}

/// [`redfog_core::InputSink`] for this backend: sends `CustomUpstream`
/// GStreamer events directly to the `waylanddisplaysrc` element, using the
/// structure names/fields gst-wayland-display's plugin matches on
/// (confirmed against `gst-plugin-wayland-display/src/waylandsrc/imp.rs`:
/// `MouseMoveRelative`/`MouseMoveAbsolute`/`MouseButton`/`MouseAxis`/
/// `KeyboardKey`, all upstream `CustomUpstream` events) — no Wayland
/// protocol involved, unlike KWin's `org_kde_kwin_fake_input`.
pub struct GstInputSink {
    element: gst::Element,
}

impl GstInputSink {
    pub fn new(element: gst::Element) -> Self {
        Self { element }
    }

    fn send(&mut self, structure: gst::Structure) {
        let event = gst::event::CustomUpstream::builder(structure).build();
        self.element.send_event(event);
    }
}

impl InputSink for GstInputSink {
    fn keyboard_key(&mut self, keycode: u32, pressed: bool) {
        self.send(gst::Structure::builder("KeyboardKey").field("key", keycode).field("pressed", pressed).build());
    }

    fn pointer_motion(&mut self, dx: f64, dy: f64) {
        self.send(
            gst::Structure::builder("MouseMoveRelative").field("pointer_x", dx).field("pointer_y", dy).build(),
        );
    }

    fn pointer_motion_absolute(&mut self, x: f64, y: f64) {
        self.send(
            gst::Structure::builder("MouseMoveAbsolute").field("pointer_x", x).field("pointer_y", y).build(),
        );
    }

    fn button(&mut self, button: u32, pressed: bool) {
        self.send(gst::Structure::builder("MouseButton").field("button", button).field("pressed", pressed).build());
    }

    fn axis(&mut self, axis: u32, value: f64) {
        // gst-wayland-display's MouseAxis takes (x, y) scroll deltas rather
        // than an axis index + value — same convention `OrgKdeKwinFakeInput`
        // uses (0=vertical, 1=horizontal) mapped onto (y, x) here.
        let (x, y) = if axis == 0 { (0.0, value) } else { (value, 0.0) };
        self.send(gst::Structure::builder("MouseAxis").field("x", x).field("y", y).build());
    }
}
