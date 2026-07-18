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
    pipewire_pulse: Child,
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
        // are unable to see the virtual video/audio nodes wireplumber later
        // registers (confirmed live: the RTSP/GST client could pair and
        // connect, but sat waiting forever on a video stream whose source
        // node it didn't have PipeWire permission to see).
        // Since we are running in a dedicated, isolated runtime directory and
        // own process group anyway, we don't need any client-isolation
        // controls. Switch access control to "unrestricted" so clients can
        // see all nodes.
        // We configure this by writing a custom config override for this
        // isolated PipeWire run. Since we override PIPEWIRE_CONFIG_DIR, we need to
        // copy the system config in first.
        let pipewire_config_dir = runtime_dir.join("pipewire-config");
        std::fs::create_dir_all(pipewire_config_dir.join("pipewire.conf.d"))?;
        const SYSTEM_PIPEWIRE_CONF: &str = "/usr/share/pipewire/pipewire.conf";
        std::fs::copy(SYSTEM_PIPEWIRE_CONF, pipewire_config_dir.join("pipewire.conf"))
            .map_err(|e| format!("failed to copy {SYSTEM_PIPEWIRE_CONF} into {pipewire_config_dir:?}: {e}"))?;

        const SYSTEM_PULSE_CONF: &str = "/usr/share/pipewire/pipewire-pulse.conf";
        if std::path::Path::new(SYSTEM_PULSE_CONF).exists() {
            let _ = std::fs::copy(SYSTEM_PULSE_CONF, pipewire_config_dir.join("pipewire-pulse.conf"));
        }

        // Specifying access.socket at all switches libpipewire-module-access
        // to socket-based policy. Map our custom socket name
        // "pipewire-0-manager" to unrestricted, and fall back to mapping
        // "default" (restricted). pipewire-0-manager (wireplumber's own
        // connection to pipewire) needs unrestricted permissions so it can
        // connect, even with pipewire-0 itself correctly set.
        std::fs::write(
            pipewire_config_dir.join("pipewire.conf.d/99-redfog-unrestricted-access.conf"),
            "module.access.args = {\n    access.socket = {\n        pipewire-0 = \"unrestricted\"\n        pipewire-0-manager = \"unrestricted\"\n    }\n}\n",
        )?;

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

        let mut wireplumber_cmd = Command::new("wireplumber");
        wireplumber_cmd.env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("PIPEWIRE_REMOTE", &pipewire_socket)
            .env("PIPEWIRE_CONFIG_DIR", &pipewire_config_dir);
        if debug_pipewire {
            wireplumber_cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        } else {
            wireplumber_cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
        let wireplumber = wireplumber_cmd.spawn().map_err(|e| format!("failed to spawn wireplumber: {e}"))?;

        let mut pipewire_pulse_cmd = Command::new("pipewire-pulse");
        pipewire_pulse_cmd.env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("PIPEWIRE_REMOTE", &pipewire_socket)
            .env("PIPEWIRE_CONFIG_DIR", &pipewire_config_dir)
            .env_remove("PULSE_SERVER")
            .env_remove("PULSE_RUNTIME_PATH");
        if debug_pipewire {
            pipewire_pulse_cmd.arg("-v").stdout(Stdio::inherit()).stderr(Stdio::inherit());
        } else {
            pipewire_pulse_cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
        let pipewire_pulse = pipewire_pulse_cmd.spawn().map_err(|e| format!("failed to spawn pipewire-pulse: {e}"))?;

        let pulse_socket = runtime_dir.join("pulse/native");
        if !wait_for_path(&pulse_socket, Duration::from_secs(10)) {
            return Err("PipeWire-Pulse socket did not appear within 10s".into());
        }

        // wireplumber needs a moment to bring the PipeWire graph out of
        // 'suspended' before nodes will transition to running.
        std::thread::sleep(Duration::from_secs(1));

        env::set_var("PIPEWIRE_REMOTE", &pipewire_socket);

        Ok(Self {
            runtime_dir,
            pipewire_socket,
            pipewire,
            wireplumber,
            pipewire_pulse,
        })
    }
}

impl Drop for HeadlessRuntime {
    fn drop(&mut self) {
        let _ = self.wireplumber.kill();
        let _ = self.wireplumber.wait();
        let _ = self.pipewire_pulse.kill();
        let _ = self.pipewire_pulse.wait();
        let _ = self.pipewire.kill();
        let _ = self.pipewire.wait();
    }
}
