//! Headless runtime bring-up: private D-Bus session + PipeWire/wireplumber.
//!
//! This is the process-level setup that proto.sh used to perform in bash.
//! It is not specific to the prototype viewer — a future moonlight-style
//! server needs the exact same bring-up before it can spawn compositor
//! sessions via `CompositorSession::spawn`.

use std::env;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const REEXEC_MARKER: &str = "_REDFOG_INNER";

/// Re-exec the current process inside `dbus-run-session` so headless
/// compositor services (KWin, plasmashell) get a private D-Bus session bus
/// instead of colliding with the desktop session's bus (plasmashell would
/// fail to claim org.kde.plasmashell, and our KWin would steal org.kde.KWin
/// from the desktop portal).
///
/// Must be called as the very first thing in `main()`, before any other
/// setup. No-op if already running inside the private bus.
pub fn ensure_private_dbus_session() {
    if env::var_os(REEXEC_MARKER).is_some() {
        return;
    }
    let exe = env::current_exe().expect("could not resolve current executable path");
    let args: Vec<String> = env::args().skip(1).collect();
    let err = Command::new("dbus-run-session")
        .arg("--")
        .arg(exe)
        .args(&args)
        .env(REEXEC_MARKER, "1")
        .exec();
    panic!("failed to exec dbus-run-session: {err}");
}

fn wait_for_path(path: &Path, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// A running PipeWire + wireplumber pair on an isolated `XDG_RUNTIME_DIR`,
/// with `PIPEWIRE_REMOTE` exported for this process (and its children) to
/// pick up. Killed on drop.
pub struct HeadlessRuntime {
    pub runtime_dir: PathBuf,
    pub pipewire_socket: PathBuf,
    pipewire: Child,
    wireplumber: Child,
}

impl HeadlessRuntime {
    /// Start PipeWire and wireplumber rooted at `runtime_dir`, wait for the
    /// socket to appear, and export `PIPEWIRE_REMOTE` for the current
    /// process so `CompositorSession::spawn` picks it up automatically.
    pub fn start(runtime_dir: impl Into<PathBuf>) -> Result<Self, BoxError> {
        let runtime_dir = runtime_dir.into();
        std::fs::create_dir_all(&runtime_dir)?;
        let mut perms = std::fs::metadata(&runtime_dir)?.permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&runtime_dir, perms)?;

        let pipewire_socket = runtime_dir.join("pipewire-0");
        for stale in [
            pipewire_socket.clone(),
            PathBuf::from(format!("{}.lock", pipewire_socket.display())),
            runtime_dir.join("pipewire-0-manager"),
            runtime_dir.join("pipewire-0-manager.lock"),
        ] {
            let _ = std::fs::remove_file(stale);
        }

        // By default, libpipewire-module-access assigns new clients on the
        // regular pipewire-0 socket "default" (restricted) access, and they
        // wait forever for wireplumber to explicitly upgrade it — which
        // never happens for a client connecting under a different uid than
        // this (private, headless-only) PipeWire instance runs as. Confirmed
        // live: a broker-spawned KWin, granted socket-file access via ACL,
        // still hung on "Failed to connect PipeWire context" without this.
        // Since this instance is already fully isolated from the user's real
        // desktop PipeWire (own XDG_RUNTIME_DIR, own D-Bus session, nothing
        // else ever shares it), granting "unrestricted" to all clients on it
        // is safe — there's nothing else on it to protect against.
        //
        // This instance is already entirely bespoke, so rather than merge a
        // drop-in into the system's config search path (uncertain semantics,
        // and would also make it depend on $HOME/$XDG_CONFIG_HOME, which
        // this deliberately isolated setup otherwise doesn't touch), it owns
        // a self-contained config directory outright: the system's default
        // pipewire.conf copied in once, plus our own override on top, with
        // PIPEWIRE_CONFIG_DIR pointed at nothing else. No dependency on the
        // real user's config space at all, in either direction.
        let pipewire_config_dir = runtime_dir.join("pipewire-config");
        std::fs::create_dir_all(pipewire_config_dir.join("pipewire.conf.d"))?;
        const SYSTEM_PIPEWIRE_CONF: &str = "/usr/share/pipewire/pipewire.conf";
        std::fs::copy(SYSTEM_PIPEWIRE_CONF, pipewire_config_dir.join("pipewire.conf"))
            .map_err(|e| format!("failed to copy {SYSTEM_PIPEWIRE_CONF} into {pipewire_config_dir:?}: {e}"))?;
        // Specifying access.socket at all switches libpipewire-module-access
        // out of its "legacy" mode (which defaults every client to
        // unrestricted with no config at all) into explicit socket-based
        // mode — where any socket *not* listed here falls back to
        // "default" (restricted). pipewire-0-manager (wireplumber's own
        // socket) must stay listed as unrestricted too, or this would
        // silently break wireplumber's ability to manage the graph at all
        // — confirmed live: omitting it here still left KWin unable to
        // connect, even with pipewire-0 itself correctly set.
        std::fs::write(
            pipewire_config_dir.join("pipewire.conf.d/99-redfog-unrestricted-access.conf"),
            "module.access.args = {\n    access.socket = {\n        pipewire-0 = \"unrestricted\"\n        pipewire-0-manager = \"unrestricted\"\n    }\n}\n",
        )?;

        // TEMPORARY debugging aid for the cross-UID PipeWire access
        // investigation (see design.md) — un-suppresses PipeWire's own
        // stdout/stderr and adds verbosity so its actual server-side
        // rejection reason is visible, instead of only KWin's generic
        // "Failed to connect PipeWire context" wrapper message. Remove once
        // that's understood.
        let debug_pipewire = std::env::var_os("REDFOG_DEBUG_PIPEWIRE_LOG").is_some();
        let mut pipewire_cmd = Command::new("pipewire");
        pipewire_cmd.env("XDG_RUNTIME_DIR", &runtime_dir).env("PIPEWIRE_CONFIG_DIR", &pipewire_config_dir);
        if debug_pipewire {
            pipewire_cmd.arg("-v").arg("-v").stdout(Stdio::inherit()).stderr(Stdio::inherit());
        } else {
            pipewire_cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
        let pipewire = pipewire_cmd.spawn().map_err(|e| format!("failed to spawn pipewire: {e}"))?;

        if !wait_for_path(&pipewire_socket, Duration::from_secs(10)) {
            return Err("PipeWire socket did not appear within 10s".into());
        }

        let wireplumber = Command::new("wireplumber")
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("PIPEWIRE_REMOTE", &pipewire_socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to spawn wireplumber: {e}"))?;
        // wireplumber needs a moment to bring the PipeWire graph out of
        // 'suspended' before nodes will transition to running.
        std::thread::sleep(Duration::from_secs(1));

        env::set_var("PIPEWIRE_REMOTE", &pipewire_socket);

        Ok(Self {
            runtime_dir,
            pipewire_socket,
            pipewire,
            wireplumber,
        })
    }
}

impl Drop for HeadlessRuntime {
    fn drop(&mut self) {
        let _ = self.wireplumber.kill();
        let _ = self.wireplumber.wait();
        let _ = self.pipewire.kill();
        let _ = self.pipewire.wait();
    }
}
