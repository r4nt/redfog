//! Headless runtime bring-up: private D-Bus session + PipeWire/wireplumber.
//!
//! This is the process-level setup that proto.sh used to perform in bash.
//! It is not specific to the prototype viewer — a future moonlight-style
//! server needs the exact same bring-up before it can spawn compositor
//! sessions via `CompositorSession::spawn`.

use std::env;
use std::io::BufRead;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const ALREADY_SETUP_MARKER: &str = "_REDFOG_INNER";

/// A private D-Bus session bus, killed on drop. See
/// [`ensure_private_dbus_session`].
pub struct DbusSession {
    daemon: Child,
}

impl Drop for DbusSession {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

/// Spawns a private D-Bus session bus (`dbus-daemon --session`) and exports
/// `DBUS_SESSION_BUS_ADDRESS` for the current process (and its children) to
/// pick up, so headless compositor services (KWin, plasmashell) get a
/// private session bus instead of colliding with the desktop session's bus
/// (plasmashell would fail to claim org.kde.plasmashell, and our KWin would
/// steal org.kde.KWin from the desktop portal).
///
/// Must be called as the very first thing in `main()`, before any other
/// setup, and the returned guard held for as long as the private session is
/// needed — dropping it kills the bus. Returns `None`, a no-op, if an
/// earlier call in this same process already set one up.
///
/// Previously re-exec'd the whole process inside `dbus-run-session` instead
/// of spawning `dbus-daemon` directly — changed because that meant
/// `dbus-daemon`'s own service-activation noise (and everything else that
/// ended up sharing its fds through the re-exec) inherited this process's
/// raw stdout/stderr directly, bypassing Rust's own stdout machinery (and
/// therefore libtest's per-test capturing) entirely — confirmed live, the
/// single biggest remaining source of terminal noise across an ordinary
/// test run even after `kwin_wayland`'s own equivalent fix (see
/// `CompositorSession::spawn`). Spawning `dbus-daemon` directly, piped and
/// relayed through `println!`/`eprintln!` like everything else, fixes that
/// the same way — and is simpler besides: no re-exec, no marker env var
/// needing to survive it, no `current_exe()`/`args()` reconstruction.
///
/// `--nofork` (not `--fork`, `dbus-daemon`'s own default for exactly this
/// use case) is deliberate: confirmed live that `--fork` makes it call
/// `setsid()` and fully detach (new session, new process group, reparented
/// to init) — which would let it escape whatever process group/cleanup
/// mechanism the caller relies on (redfog-test-cleanup's, for tests).
/// `--nofork` keeps it a perfectly ordinary child, in the same process
/// group, that this guard can just `kill()` directly.
#[must_use = "the private D-Bus session bus is killed as soon as this is dropped -- bind it \
              to a variable that lives as long as it's needed"]
pub fn ensure_private_dbus_session() -> Option<DbusSession> {
    if env::var_os(ALREADY_SETUP_MARKER).is_some() {
        return None;
    }
    env::set_var(ALREADY_SETUP_MARKER, "1");

    let mut cmd = Command::new("dbus-daemon");
    cmd.arg("--session").arg("--print-address").arg("--nofork");
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut daemon = cmd.spawn().expect("failed to spawn dbus-daemon");

    let mut stdout = std::io::BufReader::new(daemon.stdout.take().expect("piped stdout"));
    let mut address = String::new();
    stdout.read_line(&mut address).expect("failed to read dbus-daemon's printed address");
    let address = address.trim();
    assert!(!address.is_empty(), "dbus-daemon printed an empty address");
    env::set_var("DBUS_SESSION_BUS_ADDRESS", address);

    // Relay the rest of stdout (unlikely to have more, but just in case)
    // and all of stderr through println!/eprintln!, not a raw inherited fd
    // — see this function's own doc comment for why.
    std::thread::spawn(move || {
        for line in stdout.lines().map_while(Result::ok) {
            println!("[dbus-daemon] {line}");
        }
    });
    if let Some(stderr) = daemon.stderr.take() {
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("[dbus-daemon] {line}");
            }
        });
    }

    Some(DbusSession { daemon })
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

/// Wraps `program` (`pipewire`/`wireplumber`) in a `bwrap` sandbox that
/// hides `/dev/snd` entirely -- an empty tmpfs over it, nothing
/// re-exposed -- so this isolated PipeWire instance can never see, and
/// wireplumber's own ALSA monitor can never claim, any of the host's real
/// hardware audio devices (speakers *or* microphone). `/dev/snd` isn't
/// namespaced by anything else this isolated instance already does (only
/// D-Bus/socket state is) -- see `AudioLoopback`'s own doc comment for the
/// sink-side symptom of this exact gap, confirmed live there: an app's
/// audio linking straight to the real hardware sink because nothing told
/// wireplumber not to pick it. The source (microphone) side has no
/// equivalent "force the default" metadata override the way the sink
/// does, and doesn't need one once the real device is never visible in
/// the first place -- simpler and more robust than trying to keep
/// steering wireplumber away from a device it can see, the same way
/// `redfog-broker`'s `gpu_sandbox_argv_prefix` hides every render node but
/// the intended one from KWin rather than hoping it picks correctly.
/// Falls back to running `program` unsandboxed (with a warning) if
/// `bwrap` isn't installed -- this crate's own package depends on it, but
/// a from-scratch dev environment might not have it yet.
fn hide_real_audio_devices(program: &str) -> Command {
    let bwrap_available =
        Command::new("bwrap").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if !bwrap_available {
        eprintln!(
            "redfog-core: would sandbox {program} away from /dev/snd's real hardware devices, but `bwrap` isn't installed -- skipping (install \
             bubblewrap to enable it; {program} will see the host's real audio hardware and wireplumber may pick it as a default)"
        );
        return Command::new(program);
    }
    let mut cmd = Command::new("bwrap");
    cmd.args(["--bind", "/", "/", "--dev-bind", "/dev", "/dev", "--tmpfs", "/dev/snd", "--", program]);
    cmd
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
        std::fs::create_dir_all(&runtime_dir)
            .map_err(|e| format!("failed to create runtime dir {runtime_dir:?}: {e}"))?;
        let metadata = std::fs::metadata(&runtime_dir)
            .map_err(|e| format!("failed to stat runtime dir {runtime_dir:?}: {e}"))?;
        // Skip the chmod syscall entirely when the mode's already right, not
        // just when it's a value-preserving no-op call -- confirmed live
        // that `chmod`/`fchmodat` can fail outright (EPERM) for a root
        // process under some profiler/sandboxing wrappers (observed under
        // `nsys profile`, which restricts a traced target's DAC-override
        // authority even while it's still running as root) regardless of
        // whether the requested mode actually differs from the current one
        // -- only avoiding the call altogether sidesteps that, which is why
        // this checks first instead of always calling set_permissions like
        // before.
        if metadata.permissions().mode() & 0o777 != 0o700 {
            let mut perms = metadata.permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(&runtime_dir, perms)
                .map_err(|e| format!("failed to chmod runtime dir {runtime_dir:?} to 0700: {e}"))?;
        }

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
        let mut pipewire_cmd = hide_real_audio_devices("pipewire");
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

        let mut wireplumber_cmd = hide_real_audio_devices("wireplumber");
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
