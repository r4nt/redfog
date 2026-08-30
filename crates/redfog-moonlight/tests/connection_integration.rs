//! Fully self-contained integration test using the real reference client
//! (`moonlight-common-rust`, GPL-3.0-or-later, dev-only): spawns an isolated
//! `redfog-server` subprocess (own ports, own runtime dir, own paired-client
//! state — never touches a real `redfog-server` that might already be
//! running on the default ports/runtime dir) and drives it through the exact
//! scenario that was broken and fixed earlier in this project: connect,
//! send input, hand off Login->User while streaming stays alive, close the
//! window without a clean disconnect, reconnect.
//!
//! Uses `redfog-test-ux` (a purpose-built stand-in, see that crate) as both
//! the Login and User stage instead of the real login GUI or an external app
//! like `glxgears` — it repaints continuously (guaranteeing frames without
//! waiting on screen damage from something else) and logs every mouse/key
//! event it receives to stdout in a format this test greps for, which is
//! direct proof input reached the session rather than just "the client's
//! send didn't error". It exits on 'Q', giving the test a deterministic way
//! to trigger the Login->User handoff instead of racing on the shared
//! global `/tmp/trigger-login` file `redfog-login` itself uses.
//!
//! Requires: `cargo build --workspace` first (spawns `target/debug/
//! redfog-server` and `target/debug/redfog-test-ux` directly rather than
//! depending on them as crates, since redfog-server depends on
//! redfog-moonlight, which would make it a dependency cycle).
//!
//! Also spawns a real `redfog-broker` alongside `redfog-server` — the User
//! stage (post Login->User handoff) is spawned via the broker's real
//! `Authenticate`/`SpawnSession` IPC protocol, not directly, exercising that
//! code path end to end. Two env vars keep this runnable without sudo:
//! `REDFOG_BROKER_FAKE_AUTH` skips the real PAM check, and
//! `REDFOG_BROKER_FAKE_SPAWN` skips real systemd unit placement/
//! `systemd-run --uid=` (both need root — see design.md's "Privilege
//! separation: broker vs. server") in favor of spawning `kwin_wayland`
//! directly, same mechanism `CompositorSession::spawn` already uses. The
//! systemd/cross-user path itself isn't exercised by this test; it needs
//! actual root and manual verification (`sudo -E cargo test ...` with those
//! two env vars unset).

use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonlight_common::crypto::rustcrypto::RustCryptoBackend;
use moonlight_common::high::tokio::MoonlightHost;
use moonlight_common::http::client::tokio_hyper::TokioHyperClient;
use moonlight_common::http::pair::PairPin;
use moonlight_common::http::{ClientIdentifier, ClientSecret};
use moonlight_common::stream::audio::AudioConfig;
use moonlight_common::stream::control::{
    ActiveGamepads, KeyAction, KeyCode, KeyFlags, KeyModifiers, MouseButton, MouseButtonAction,
};
use moonlight_common::stream::proto::control::input_batcher::ClientInputEvent;
use moonlight_common::stream::tokio::MoonlightStream;
use moonlight_common::stream::video::{ColorRange, ColorSpace, VideoCapabilities, VideoFormats};
use moonlight_common::stream::{AesIv, AesKey, EncryptionFlags, MoonlightStreamSettings, StreamingConfig};

use redfog_moonlight::tls::ServerIdentity;

/// Windows VK code for 'Q' — what a real client sends; our server's
/// `vk_to_evdev` translates it to the Linux evdev keycode KWin's
/// fake-input protocol expects.
const VK_Q: i16 = 0x51;

fn pick_free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Registers `PR_SET_PDEATHSIG(SIGKILL)` in the about-to-exec child so the
/// kernel kills it the moment *this* process (its direct parent — the test
/// binary itself) dies for any reason, not just on a clean unwind: a hang
/// followed by an external `kill -9`, a genuine crash, or `SIGKILL` on this
/// test process never runs `ServerProcess`/`BrokerProcess`'s Drop impls (Drop
/// only fires on unwind), which is how these tests have leaked kwin_wayland/
/// pipewire/redfog-broker instances in the background before. Only closes
/// this one parent->child hop — see `redfog-broker`'s own use of this same
/// mechanism on its direct `kwin_wayland` child in `SessionManager::
/// spawn_fake` for the next hop down.
fn die_with_parent(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            let parent_before = libc::getppid();
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Read back what the kernel actually recorded, not just
            // trusting a zero return code — confirmed live that this can
            // silently not stick in at least one real environment (still
            // being root-caused), which a bare return-code check can't
            // detect: the call "succeeds" but the child never actually
            // dies with its parent. Failing the spawn outright here is far
            // better than silently proceeding with a protection that isn't
            // actually in effect.
            let mut got_sig: libc::c_int = -1;
            if libc::prctl(libc::PR_GET_PDEATHSIG, &mut got_sig as *mut libc::c_int) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if got_sig != libc::SIGKILL {
                return Err(std::io::Error::other(format!(
                    "PR_SET_PDEATHSIG(SIGKILL) reported success but PR_GET_PDEATHSIG reads back {got_sig} instead of {}",
                    libc::SIGKILL
                )));
            }
            // Close the race where the parent already exited between
            // fork() and this prctl() call: we'd have already been
            // reparented (to the nearest subreaper, usually PID 1) and
            // would never receive the signal for a parent that's already
            // gone.
            if libc::getppid() != parent_before {
                libc::raise(libc::SIGKILL);
            }
            Ok(())
        });
    }
}

/// A dead-but-unreaped zombie is still "gone" for our purposes here — see
/// `direct_child_dies_when_its_direct_parent_dies`'s comment on why bare
/// existence/`comm` checks aren't enough.
fn process_is_alive(pid: i32) -> bool {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return false;
    };
    !status.lines().any(|l| l.trim_start().starts_with("State:") && l.contains('Z'))
}

/// Not a real test on its own — re-exec'd by
/// `direct_child_dies_when_its_direct_parent_dies` as a throwaway "middle"
/// process (a real subprocess, not just a function call, since
/// `die_with_parent`'s whole point only shows up across an actual
/// parent/child boundary). Spawns a grandchild with `die_with_parent`,
/// prints its pid, then sleeps until killed — the outer test SIGKILLs
/// *this* process and checks whether the grandchild died too.
#[test]
fn pdeathsig_helper_middle_process() {
    if std::env::var_os("REDFOG_PDEATHSIG_TEST_HELPER").is_none() {
        return;
    }
    let mut cmd = Command::new("sleep");
    cmd.arg("100").stdout(Stdio::null()).stderr(Stdio::null());
    die_with_parent(&mut cmd);
    let child = cmd.spawn().expect("spawn grandchild");
    println!("GRANDCHILD_PID={}", child.id());
    use std::io::Write;
    std::io::stdout().flush().unwrap();
    std::thread::sleep(Duration::from_secs(100));
}

/// Verifies the actual mechanism `die_with_parent` relies on — not just
/// that it compiles, but that a child registered with it genuinely dies the
/// moment its direct parent is killed, including a hard `SIGKILL` that
/// gives the parent no chance to run any cleanup of its own. This is the
/// property the whole leaked-process fix depends on; the rest of the test
/// suite only demonstrates the *absence* of leaks, which is a much weaker
/// signal (could just as easily mean the mechanism silently isn't firing at
/// all and everything happens to exit cleanly anyway).
#[test]
fn direct_child_dies_when_its_direct_parent_dies() {
    let exe = std::env::current_exe().expect("current_exe");
    let mut middle = Command::new(&exe)
        .args(["--exact", "pdeathsig_helper_middle_process", "--nocapture"])
        .env("REDFOG_PDEATHSIG_TEST_HELPER", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn middle helper process");

    let stdout = middle.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).expect("read helper output");
        assert!(n > 0, "middle process exited before printing GRANDCHILD_PID");
        if let Some(pid) = line.trim().strip_prefix("GRANDCHILD_PID=") {
            let grandchild_pid: i32 = pid.parse().expect("parse grandchild pid");

            unsafe { libc::kill(middle.id() as i32, libc::SIGKILL) };
            middle.wait().expect("wait for middle process");

            // Neither bare `/proc/{pid}` existence nor `/proc/{pid}/comm` is
            // enough to prove the grandchild is genuinely still running —
            // confirmed live (via `act`'s own container, a bare
            // `tail -f /dev/null` as PID 1): PDEATHSIG does fire correctly,
            // the grandchild does die, but once reparented to a PID 1 that
            // never calls wait() on orphans it sits forever as a zombie —
            // still present in /proc, `comm` still reading "sleep", despite
            // already being dead. A zombie's `State:` in /proc/{pid}/status
            // is `Z`; treat that the same as gone.
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                if !process_is_alive(grandchild_pid) {
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "grandchild pid {grandchild_pid} is still alive 30s after its parent was SIGKILLed");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Bootstraps a working `systemd --user` instance for `SessionManager::
/// spawn_fake_pam`'s `systemd-run --user --scope` call, without requiring
/// `loginctl enable-linger` (a persistent, reboot-surviving setting — not
/// something to force on just to run tests) or an active logind session
/// (this dev box's Chrome-Remote-Desktop-based login has neither). Returns
/// `None` (callers should skip the test, not fail it) if there's no
/// passwordless sudo available to bootstrap it — e.g. CI/Docker runners
/// typically do have one, but that's a deployment choice, not something to
/// assume.
///
/// `user@<uid>.service` is a plain template unit on the *system* systemd
/// instance; starting it directly (root-only, but fully reversible via
/// `systemctl stop` right after — unlike lingering, it leaves no persistent
/// config behind) does the same cgroup-delegation/`XDG_RUNTIME_DIR` setup
/// logind would do on a real login. `DBUS_SESSION_BUS_ADDRESS` is the one
/// extra piece logind also sets that a bare `systemctl start` doesn't —
/// confirmed live: `systemd-run --user --scope` fails with "Process
/// org.freedesktop.systemd1 exited with status 1" without it, even once the
/// bus socket already exists at `$XDG_RUNTIME_DIR/bus`.
#[cfg(feature = "compositor-tests")]
fn ensure_user_systemd_for_fake_pam_spawn() -> Option<Vec<(String, String)>> {
    let uid = nix::unistd::Uid::current().as_raw();
    let runtime_dir = format!("/run/user/{uid}");
    let bus_address = format!("unix:path={runtime_dir}/bus");
    let env = vec![("XDG_RUNTIME_DIR".to_string(), runtime_dir.clone()), ("DBUS_SESSION_BUS_ADDRESS".to_string(), bus_address.clone())];

    // Already working (real login, or lingering already on) — don't touch
    // anything.
    let already_working = Command::new("systemd-run")
        .args(["--user", "--scope", "--collect", "--quiet", "--unit=redfog-it-user-systemd-probe", "--", "true"])
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("DBUS_SESSION_BUS_ADDRESS", &bus_address)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if already_working {
        return Some(env);
    }

    let unit = format!("user@{uid}.service");
    let started = Command::new("sudo").args(["-n", "systemctl", "start", &unit]).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
    if !started {
        eprintln!(
            "SKIP: no passwordless sudo to start {unit} — this test needs a working `systemd --user` \
             instance (see `ensure_user_systemd_for_fake_pam_spawn`'s doc comment for why plain lingering \
             isn't required, but *some* form of root access to bootstrap it still is)"
        );
        return None;
    }

    let bus_path = PathBuf::from(format!("{runtime_dir}/bus"));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !bus_path.exists() {
        if std::time::Instant::now() > deadline {
            eprintln!("SKIP: {unit} started but {runtime_dir}/bus never appeared");
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Some(env)
}

fn workspace_binary(name: &str) -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/redfog-moonlight; the workspace target
    // dir is two levels up. Neither redfog-server nor redfog-test-ux can be
    // a dev-dependency of this crate (redfog-server depends on
    // redfog-moonlight itself), so there's no CARGO_BIN_EXE_* env var for
    // them — locate the binaries directly instead.
    //
    // Not just a bare `target/debug/{name}` — confirmed live that
    // `cargo llvm-cov` builds into `target/llvm-cov-target/debug/` instead
    // (its own separate, instrumented target dir), which a hardcoded
    // `target/debug` path silently can't find, failing every test that
    // needs a workspace binary. Check `CARGO_TARGET_DIR` first in case
    // that's been set explicitly, then both known layouts.
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let candidates = [
        std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from),
        Some(workspace_root.join("target")),
        Some(workspace_root.join("target/llvm-cov-target")),
    ];
    for candidate in candidates.into_iter().flatten() {
        let path = candidate.join("debug").join(name);
        if path.exists() {
            return path;
        }
    }
    panic!(
        "{name} binary not found under target/debug or target/llvm-cov-target/debug (relative to \
         {workspace_root:?}) — run `cargo build --workspace` (or `cargo llvm-cov --workspace --no-report`) first"
    );
}

/// Kills the whole process group on drop (the child is spawned as its own
/// group leader via `process_group(0)`), so the
/// dbus-run-session/pipewire/wireplumber/kwin_wayland/redfog-test-ux tree
/// underneath doesn't leak past the test.
struct ServerProcess {
    child: Child,
    runtime_dir: PathBuf,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let pgid = nix::unistd::Pid::from_raw(-(self.child.id() as i32));
        let _ = nix::sys::signal::kill(pgid, nix::sys::signal::Signal::SIGTERM);
        let _ = self.child.wait();
        redfog_test_cleanup::untrack_extra_pgid(self.child.id() as i32);
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
    }
}

/// Kills the broker's whole process group on drop — in
/// `REDFOG_BROKER_FAKE_SPAWN` mode the broker spawns `kwin_wayland` as its
/// own direct child (not under a separate systemd unit), so killing just
/// the broker's own pid would orphan it. Also does best-effort cleanup of
/// any `redfog-session-*` systemd units left behind by the real (non-fake)
/// path — the broker's own `TerminateSession` handles that on a clean path,
/// but a test failure/panic could leave one running otherwise.
struct BrokerProcess {
    child: Child,
}

impl Drop for BrokerProcess {
    fn drop(&mut self) {
        let pgid = nix::unistd::Pid::from_raw(-(self.child.id() as i32));
        let _ = nix::sys::signal::kill(pgid, nix::sys::signal::Signal::SIGTERM);
        let _ = self.child.wait();
        redfog_test_cleanup::untrack_extra_pgid(self.child.id() as i32);
        // Best-effort: only relevant for the real (non-FAKE_SPAWN) systemd
        // path, which this test doesn't use — skip entirely otherwise so a
        // plain, unprivileged test run doesn't print confusing "access
        // denied" noise from a daemon-reload attempt that has nothing to
        // clean up anyway. list-units --all won't show units that never
        // loaded, but redfog-session-* units placed in UNIT_DIR are named
        // predictably enough to just glob and stop/remove.
        let mut found_any = false;
        if let Ok(entries) = std::fs::read_dir("/run/systemd/system") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("redfog-session-") {
                    found_any = true;
                    let _ = Command::new("systemctl").args(["stop", &name]).status();
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        if found_any {
            let _ = Command::new("systemctl").arg("daemon-reload").status();
        }
    }
}

/// Picks which spawn path the broker exercises: the real systemd/cross-user
/// path when run as root (via `sudo -E cargo test ...`), or the sudo-free
/// `REDFOG_BROKER_FAKE_SPAWN` direct-spawn path otherwise. Targets
/// `$SUDO_USER` (the invoking non-root user), not root itself — real
/// desktop sessions/PipeWire/D-Bus generally don't tolerate running as root.
fn broker_spawn_mode_env() -> Vec<(String, String)> {
    // `$SUDO_USER`'s presence, not bare root status, is what actually means
    // "deliberately invoked via `sudo -E cargo test ...` to exercise the
    // real systemd/cross-user path" (see scripts/sudo-test-runner.sh).
    // Confirmed live: CI/Docker containers commonly run natively as root
    // with no `sudo` involved at all and no SUDO_USER set — treating bare
    // root as that signal made every test needing this path panic in CI.
    match std::env::var("SUDO_USER") {
        Ok(target_user) => {
            println!("SUDO_USER set — exercising the REAL systemd/cross-user path, spawning as {target_user}");
            vec![("REDFOG_BROKER_FORCE_SPAWN_USER".to_string(), target_user)]
        }
        Err(_) => vec![("REDFOG_BROKER_FAKE_SPAWN".to_string(), "1".to_string())],
    }
}

/// Kills the `journalctl -f` follower on drop.
struct JournalFollower {
    child: Child,
}

impl Drop for JournalFollower {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct TestServer {
    _process: ServerProcess,
    _broker: BrokerProcess,
    _journal: Option<JournalFollower>,
    http_port: u16,
    stdout_lines: Arc<Mutex<Vec<String>>>,
}

impl TestServer {
    fn spawn() -> Self {
        Self::spawn_with_broker_env(Vec::new())
    }

    /// Like `spawn()`, but with additional env vars passed to `redfog-
    /// broker` — e.g. `REDFOG_BROKER_PAM_SPAWN=1` to force the real PAM-
    /// spawn path (see `real_pam_spawn_env`) instead of whatever
    /// `broker_spawn_mode_env` would otherwise pick.
    fn spawn_with_broker_env(extra_broker_env: Vec<(String, String)>) -> Self {
        // Not what actually installs the protection (that's
        // redfog-test-cleanup's own #[ctor], which already ran before this
        // test binary's main() even started) -- this call just keeps the
        // linker from stripping the crate out as unused, and documents,
        // right where the real subprocess-spawning begins, that it's
        // active. See that crate's doc comment for the full mechanism and
        // why die_with_parent (above) alone isn't enough for kwin_wayland.
        redfog_test_cleanup::ensure_active();

        let runtime_dir = std::env::temp_dir().join(format!("redfog-it-runtime-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&runtime_dir).unwrap();
        // Short and fixed (not under runtime_dir, which has a variable-
        // length UUID component) to comfortably stay under Unix socket
        // path's SUN_LEN limit (~108 bytes) regardless.
        let broker_socket = std::env::temp_dir().join(format!("redfog-it-broker-{}.sock", uuid::Uuid::new_v4()));

        let http_port = pick_free_port();
        let https_port = pick_free_port();
        let rtsp_port = pick_free_port();
        let video_port = pick_free_port();
        let control_port = pick_free_port();
        let audio_port = pick_free_port();

        let test_ux = workspace_binary("redfog-test-ux");
        let test_ux = test_ux.to_str().unwrap();

        // Created before spawning anything so both redfog-server's AND the
        // broker's captured output feed into the same buffer — the User
        // stage's redfog-test-ux (spawned via the broker's fake_spawn,
        // inheriting the *broker's* stdout, not redfog-server's) would
        // otherwise print straight to the test harness's own terminal and
        // never reach `stdout_contains`/`wait_for_stdout`, which only ever
        // checked redfog-server's own captured output — confirmed live:
        // "TESTUX[redfog-user-0]: started" appeared in the raw test output
        // but without the "[redfog-server]" prefix, and wait_for_stdout
        // still timed out even though the line had genuinely been printed.
        let stdout_lines = Arc::new(Mutex::new(Vec::<String>::new()));

        // Under the real (root/systemd) broker path, the User stage's
        // redfog-test-ux runs as a systemd service whose stdout goes to the
        // journal, not to any pipe redfog-broker/redfog-server themselves
        // capture — confirmed live: "TESTUX[redfog-user-0]: started"
        // genuinely appeared in `journalctl -u redfog-session-0.service` but
        // never in stdout_lines, so wait_for_stdout timed out even though
        // the session was working correctly. Follow the journal for
        // whichever `redfog-session-*` units the broker creates (a glob,
        // not a fixed unit name, since the session id increments across
        // reconnects) into the same buffer. Not needed under
        // REDFOG_BROKER_FAKE_SPAWN (the non-root path), which inherits
        // redfog-broker's own piped stdout instead, already captured below.
        let journal = if std::env::var_os("SUDO_USER").is_some() {
            let mut journal_cmd = Command::new("journalctl");
            journal_cmd
                .args(["--no-pager", "-f", "-n", "0", "-o", "cat", "-u", "redfog-session-*"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            let mut child = journal_cmd.spawn().expect("spawn journalctl -f");
            let stdout = child.stdout.take().unwrap();
            let stdout_lines = stdout_lines.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    println!("[journal] {line}");
                    stdout_lines.lock().unwrap().push(line);
                }
            });
            Some(JournalFollower { child })
        } else {
            None
        };

        // FAKE_AUTH always skips the real PAM check (no real credential-entry
        // path exists yet to test against — see design.md's "Authentication:
        // a real graphical login screen"). The spawn path is conditional:
        // plain `cargo test` uses FAKE_SPAWN (direct kwin_wayland spawn, no
        // root needed); `sudo -E cargo test ...` instead exercises the REAL
        // systemd/cross-user path (unit generation, socket activation,
        // `systemd-run --uid=` targeting $SUDO_USER) — see
        // `broker_spawn_mode_env`. Either way this exercises the real broker
        // IPC protocol end to end (Authenticate/SpawnSession/
        // TerminateSession, redfog-server's broker-calling code,
        // CompositorSession::attach) plus the full existing video/audio/
        // input/reconnect coverage against a broker-spawned session.
        let mut broker_cmd = Command::new(workspace_binary("redfog-broker"));
        broker_cmd
            .env("REDFOG_RUNTIME_DIR", &runtime_dir)
            .env("REDFOG_BROKER_SOCKET", &broker_socket)
            .env("REDFOG_BROKER_FAKE_AUTH", "1")
            .envs(broker_spawn_mode_env())
            .envs(extra_broker_env)
            .env("RUST_LOG", "redfog_broker=debug")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group so BrokerProcess's Drop can kill the whole
            // tree (broker -> fake-spawned kwin_wayland) with one signal.
            .process_group(0);
        die_with_parent(&mut broker_cmd);
        // Wrapped in its Drop-safe struct immediately — a bare `Child`'s
        // Drop impl does NOT kill the process, so if anything below this
        // point panics before `TestServer` itself is constructed, an
        // unwrapped child would leak (confirmed live: this is exactly how
        // earlier failed runs left orphaned pipewire/wireplumber/kwin_wayland
        // processes behind).
        let mut broker = BrokerProcess { child: broker_cmd.spawn().expect("spawn redfog-broker") };
        // Own process group (see the comment on BrokerProcess) means the
        // outer test-binary-wide watchdog installed by ensure_active()
        // above can't see this subtree on its own -- register it so the
        // watchdog also kills it if the whole test binary dies before
        // BrokerProcess's own Drop gets a chance to run.
        redfog_test_cleanup::track_extra_pgid(broker.child.id() as i32);
        {
            let stdout = broker.child.stdout.take().unwrap();
            let stdout_lines = stdout_lines.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    println!("[redfog-broker] {line}");
                    stdout_lines.lock().unwrap().push(line);
                }
            });
        }
        {
            let stderr = broker.child.stderr.take().unwrap();
            let stdout_lines = stdout_lines.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    eprintln!("[redfog-broker] {line}");
                    stdout_lines.lock().unwrap().push(line);
                }
            });
        }

        // Wait for the broker's socket to appear before starting
        // redfog-server, which connects to it lazily on first use anyway,
        // but failing fast here gives a clearer error than a mysterious
        // "connection refused" deep into the test.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !broker_socket.exists() {
            assert!(std::time::Instant::now() < deadline, "redfog-broker never created its socket at {broker_socket:?}");
            std::thread::sleep(Duration::from_millis(100));
        }

        let mut cmd = Command::new(workspace_binary("redfog-server"));
        cmd.env("REDFOG_RUNTIME_DIR", &runtime_dir)
            .env("REDFOG_BROKER_SOCKET", &broker_socket)
            .env("REDFOG_HTTP_PORT", http_port.to_string())
            .env("REDFOG_HTTPS_PORT", https_port.to_string())
            .env("REDFOG_RTSP_PORT", rtsp_port.to_string())
            .env("REDFOG_VIDEO_PORT", video_port.to_string())
            .env("REDFOG_CONTROL_PORT", control_port.to_string())
            .env("REDFOG_AUDIO_PORT", audio_port.to_string())
            .env("REDFOG_LOGIN_APP", test_ux)
            .env("REDFOG_USER_APP", test_ux)
            .env("RUST_LOG", "redfog_moonlight=debug,redfog_server=debug")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group so Drop can kill the whole tree (dbus-run-
            // session -> redfog-server -> pipewire/wireplumber -> the Login
            // stage's kwin_wayland/redfog-test-ux) with one signal. The User
            // stage's kwin_wayland is spawned by the broker instead (either
            // directly, in FAKE_SPAWN mode, or via a systemd unit under
            // sudo), so it's cleaned up by BrokerProcess's Drop, not this
            // process group.
            .process_group(0);
        die_with_parent(&mut cmd);

        // Wrapped immediately, same reasoning as `broker` above.
        let mut process = ServerProcess { child: cmd.spawn().expect("spawn redfog-server"), runtime_dir };
        // See the identical call after spawning `broker` above.
        redfog_test_cleanup::track_extra_pgid(process.child.id() as i32);

        {
            let stdout = process.child.stdout.take().unwrap();
            let stdout_lines = stdout_lines.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    println!("[redfog-server] {line}");
                    stdout_lines.lock().unwrap().push(line);
                }
            });
        }
        {
            let stderr = process.child.stderr.take().unwrap();
            let stdout_lines = stdout_lines.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    eprintln!("[redfog-server] {line}");
                    stdout_lines.lock().unwrap().push(line);
                }
            });
        }

        // Wait for the HTTP server to actually accept connections rather
        // than sleeping a fixed guess — dbus-run-session + PipeWire bring-up
        // can take a couple of seconds.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if std::net::TcpStream::connect(("127.0.0.1", http_port)).is_ok() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "redfog-server never came up on port {http_port}");
            std::thread::sleep(Duration::from_millis(100));
        }

        TestServer {
            _process: process,
            _broker: broker,
            _journal: journal,
            http_port,
            stdout_lines,
        }
    }

    fn stdout_contains(&self, needle: &str) -> bool {
        self.stdout_lines.lock().unwrap().iter().any(|line| line.contains(needle))
    }

    fn count_stdout(&self, needle: &str) -> usize {
        self.stdout_lines.lock().unwrap().iter().filter(|line| line.contains(needle)).count()
    }

    async fn wait_for_stdout(&self, needle: &str, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        while !self.stdout_contains(needle) {
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {needle:?} in redfog-server's output");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Waits for `needle`'s occurrence count to exceed `baseline` — unlike
    /// `wait_for_stdout`, safe to reuse the same needle across multiple
    /// checkpoints in one test, since stdout is a growing, never-cleared
    /// log and a plain "does it appear" check would trivially pass on a
    /// stale match from earlier.
    async fn wait_for_new_stdout(&self, needle: &str, baseline: usize, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.count_stdout(needle) <= baseline {
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for a new {needle:?} in redfog-server's output");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }


    /// Same path `redfog-server`'s own `main.rs` derives (`REDFOG_RUNTIME_DIR`
    /// is fixed per-test, `REDFOG_LOGIN_SOCKET` itself is left unset) — see
    /// `LoginReportServer`. Lets a test speak `LoginRequest`/`LoginResponse`
    /// directly, the same wire protocol the real login screen's "Log out"
    /// button sends, without needing a UI to click.
    fn login_socket_path(&self) -> PathBuf {
        self._process.runtime_dir.join("login.sock")
    }
}

/// Sends one `LoginRequest` over the login-report socket and returns the
/// matching `LoginResponse` — the same round trip `redfog-login`'s own
/// credential/log-out UI performs, just driven directly instead of through a
/// rendered button click.
async fn send_login_request(socket_path: &std::path::Path, request: redfog_login_protocol::LoginRequest) -> redfog_login_protocol::LoginResponse {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(socket_path).await.unwrap_or_else(|e| panic!("failed to connect to login socket {socket_path:?}: {e}"));
    let mut reader = BufReader::new(stream);
    let mut line = serde_json::to_string(&request).expect("LoginRequest always serializes");
    line.push('\n');
    reader.get_mut().write_all(line.as_bytes()).await.expect("write LoginRequest");
    let mut response_line = String::new();
    reader.read_line(&mut response_line).await.expect("read LoginResponse");
    serde_json::from_str(response_line.trim_end()).expect("LoginResponse always deserializes")
}

fn default_stream_settings() -> MoonlightStreamSettings {
    MoonlightStreamSettings {
        width: 1280,
        height: 720,
        fps: 60,
        fps_x100: 6000,
        bitrate: 10_000,
        packet_size: 1024,
        encryption_flags: EncryptionFlags::empty(),
        streaming_remotely: StreamingConfig::Local,
        sops: false,
        hdr: false,
        supported_video_formats: VideoFormats::H264,
        color_space: ColorSpace::Rec709,
        color_range: ColorRange::Limited,
        local_audio_play_mode: false,
        audio_config: AudioConfig::STEREO,
        gamepads_attached: ActiveGamepads::empty(),
        gamepads_persist_after_disconnect: false,
        enable_mic: false,
    }
}

fn video_capabilities() -> VideoCapabilities {
    VideoCapabilities {
        reference_frame_invalidation_h264: false,
        reference_frame_invalidation_h265: false,
        reference_frame_invalidation_av1: false,
        pull_renderer: false,
        slices_per_frame: None,
    }
}

/// `send_input` can fail with `NotConnected` if called before the control
/// channel's own ENet handshake (a handful of round trips, separate from
/// the RTSP handshake `MoonlightStream::connect` waits for) has finished —
/// confirmed live. Retry briefly instead of requiring callers to guess a
/// safe fixed delay.
async fn send_input_retrying(stream: &MoonlightStream, event: ClientInputEvent) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match stream.send_input(event.clone()) {
            Ok(()) => return,
            Err(e) if tokio::time::Instant::now() < deadline => {
                eprintln!("send_input not ready yet ({e}), retrying");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => panic!("send_input failed after retrying: {e}"),
        }
    }
}

/// A single `send_input` landing while the session is still finishing its
/// video/audio UDP handshake (`on_play`'s background task hasn't yet set
/// `state = Streaming`, even though the RTSP `PLAY` response the client
/// waits for has already returned) gets silently dropped by `on_input` —
/// not queued, not retried server-side, just lost. Waiting longer doesn't
/// help if the one send happened to land in that window; resending until
/// the server actually reports having received it does.
async fn send_input_until_seen(stdout_lines: &Arc<Mutex<Vec<String>>>, stream: &MoonlightStream, event: ClientInputEvent, needle: &str, timeout: Duration) {
    send_input_until_new_seen(stdout_lines, stream, event, needle, 0, timeout).await;
}

/// Like `send_input_until_seen`, but requires `needle`'s occurrence count to
/// exceed `min_count` rather than merely appearing — needed wherever the
/// same text can legitimately appear more than once (e.g. a reconnect to
/// the same User-stage session logs the identical needle a second time) so
/// a stale match from a previous connection doesn't trivially satisfy the
/// wait without actually proving the input reached the *current* one.
async fn send_input_until_new_seen(
    stdout_lines: &Arc<Mutex<Vec<String>>>,
    stream: &MoonlightStream,
    event: ClientInputEvent,
    needle: &str,
    min_count: usize,
    timeout: Duration,
) {
    let count = |needle: &str| stdout_lines.lock().unwrap().iter().filter(|line| line.contains(needle)).count();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        send_input_retrying(stream, event.clone()).await;
        if tokio::time::timeout(Duration::from_millis(300), async {
            while count(needle) <= min_count {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok()
        {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out resending input, waiting for a new {needle:?} in redfog-server's output");
    }
}

async fn send_key(stream: &MoonlightStream, vk: i16, down: bool) {
    send_input_retrying(
        stream,
        ClientInputEvent::Keyboard {
            action: if down { KeyAction::Down } else { KeyAction::Up },
            flags: KeyFlags::empty(),
            key_code: KeyCode(vk),
            modifiers: KeyModifiers::empty(),
        },
    )
    .await;
}

/// Continuously polls `stream` for video/audio frames until `stop` resolves,
/// tracking the longest gap between consecutive video frames — used to
/// prove streaming never stalls across the Login->User handoff (the video-
/// continuity bug fixed earlier reset RTP state on every handoff and froze
/// the stream instead).
async fn poll_frames_tracking_gaps(stream: &MoonlightStream, stop: impl std::future::Future<Output = ()>) -> (usize, Duration) {
    tokio::pin!(stop);
    let mut video_frames = 0usize;
    let mut last_frame_at: Option<tokio::time::Instant> = None;
    let mut max_gap = Duration::ZERO;
    loop {
        tokio::select! {
            _ = &mut stop => break,
            frame = stream.poll_video_frame() => {
                if frame.is_err() {
                    break;
                }
                let now = tokio::time::Instant::now();
                if let Some(last) = last_frame_at {
                    max_gap = max_gap.max(now.duration_since(last));
                }
                last_frame_at = Some(now);
                video_frames += 1;
            }
            _ = stream.poll_audio_frame() => {}
        }
    }
    (video_frames, max_gap)
}

#[cfg(feature = "compositor-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_client_connects_reconnects_and_sends_input() {
    // Our own rustls usage and moonlight-common's pull in different crypto
    // provider backends (ring vs aws-lc-rs); can't auto-pick one when both
    // are linked into the same test binary.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

    let server = TestServer::spawn();

    let client_identity = ServerIdentity::generate().expect("generate client identity");
    let client_identifier = ClientIdentifier::from_pem(pem::parse(&client_identity.cert_pem).unwrap());
    let client_secret = ClientSecret::from_pem(pem::parse(&client_identity.private_key_pem).unwrap());

    let host = MoonlightHost::<TokioHyperClient>::new("127.0.0.1".to_string(), server.http_port, Some("it-client".to_string()))
        .expect("construct MoonlightHost");

    let pin = PairPin::new_random(&RustCryptoBackend).expect("generate pin");
    let pin_str = pin.to_string();
    let http_port = server.http_port;
    let submit_task = tokio::task::spawn_blocking(move || {
        std::thread::sleep(Duration::from_millis(300));
        ureq::post(&format!("http://127.0.0.1:{http_port}/submit-pin"))
            .send_form(&[("uniqueid", "it-client"), ("pin", &pin_str)])
            .expect("submit-pin request");
    });
    host.pair(&client_identifier, &client_secret, "connection-integration-test".to_string(), pin, RustCryptoBackend)
        .await
        .expect("pairing must succeed");
    submit_task.await.unwrap();

    let mut settings = default_stream_settings();
    let server_version = host.version().await.expect("server version");
    let gfe_version = host.gfe_version().await.expect("gfe version");
    let codec_support = host.server_codec_mode_support().await.expect("codec support");
    settings.adjust_for_server(server_version, &gfe_version, codec_support).expect("settings compatible");

    // ---- First connection: the Login stage. ----
    let stream_config = host
        .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
        .await
        .expect("first launch must succeed");
    let crypto_backend = Arc::new(RustCryptoBackend);
    let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
        .await
        .expect("first stream must connect");

    server.wait_for_stdout("TESTUX[login]: started", Duration::from_secs(45)).await;

    // ---- Simulated client mouse movement + key press, verified by proof
    // it actually reached the Login-stage session, not just that the
    // client's send didn't error. Absolute, targeting the window's likely
    // center — a small relative move from an unknown starting cursor
    // position may never land inside test-ux's (non-fullscreen) window at
    // all, so it'd never see the event even though the compositor correctly
    // received and forwarded it. ----
    send_input_until_seen(
        &server.stdout_lines,
        &stream,
        ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
        "TESTUX[login]: pointer_moved",
        Duration::from_secs(10),
    )
    .await;

    // A window only gets *keyboard* focus from a click, not just pointer
    // hover — confirmed live: sending a key press right after the mouse
    // move above (no click) reached fake_input and got forwarded
    // server-side, but test-ux never saw it.
    send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
    send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    send_key(&stream, VK_Q.wrapping_add(1) /* VK_R, an arbitrary non-exit key */, true).await;
    server.wait_for_stdout("TESTUX[login]: key_pressed", Duration::from_secs(20)).await;

    // ---- Confirm streaming works before touching the handoff, and start
    // tracking frame gaps from here through the handoff below. ----
    let (login_frames, _) = poll_frames_tracking_gaps(&stream, tokio::time::sleep(Duration::from_secs(2))).await;
    assert!(login_frames > 0, "expected video frames from the Login-stage test UX");

    // ---- Trigger the Login->User handoff deterministically (redfog-test-ux
    // exits on 'Q', same `--exit-with-session` trigger a real login success
    // uses) while continuously polling frames, proving the stream survives
    // the handoff instead of stalling — the exact bug fixed earlier, where
    // resetting RTP sequence/frame-index state on every compositor handoff
    // froze the video permanently. ----
    send_key(&stream, VK_Q, true).await;
    send_key(&stream, VK_Q, false).await;
    let (handoff_frames, max_gap) = poll_frames_tracking_gaps(&stream, async {
        server.wait_for_stdout("TESTUX[redfog-user-0]: started", Duration::from_secs(45)).await;
        // A little settle time after the User stage starts, so the next
        // input-verification step lands on a session that's fully up.
        tokio::time::sleep(Duration::from_millis(500)).await;
    })
    .await;
    assert!(handoff_frames > 0, "expected video frames to keep flowing across the Login->User handoff");
    assert!(
        max_gap < Duration::from_secs(3),
        "video stalled for {max_gap:?} across the handoff — the video-continuity bug is back"
    );

    // ---- Confirm input reaches the new, User-stage session too. Absolute,
    // not relative — this is a brand new KWin compositor instance (a
    // separate process from the Login stage's), so the cursor's position
    // here is unknown/undefined, same reasoning as the Login-stage move
    // above. ----
    send_input_until_seen(
        &server.stdout_lines,
        &stream,
        ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
        "TESTUX[redfog-user-0]: pointer_moved",
        Duration::from_secs(10),
    )
    .await;

    // ---- Simulate closing the window: drop the stream without any clean
    // RTSP TEARDOWN / control-channel disconnect, exactly like a closed
    // browser tab. The server has no way to know this happened yet. ----
    drop(stream);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ---- Reconnect: a brand new stream_config/AES key, same client. Under
    // the old "single session" model this silently reattached to whatever
    // was still `Streaming` on the User stage — the exact scenario that was
    // once broken (stale queued PING misrouting the stream, and the new
    // peer's own control connection getting caught by its own stale-peer
    // disconnect sweep) and fixed. That old behavior is gone on purpose now:
    // every `/launch` always shows a fresh Login screen (see
    // `SessionManager::launch`'s doc comment) — the User session that was
    // running gets *backgrounded* instead (see
    // `background_or_discard_active_session`), not silently reattached to
    // and not killed either. ----
    let login_started_before_reconnect = server.count_stdout("TESTUX[login]: started");
    let stream_config = host
        .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
        .await
        .expect("reconnect launch must succeed");
    let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
        .await
        .expect("reconnect stream must connect");

    // A genuinely new Login-stage process, not a silent reattachment to the
    // User session that was still running — this also exercises the same
    // generation-based stale-peer disconnect fix the old reconnect-retake
    // path used to validate (the abandoned first connection's control peer
    // must not leak input into this new Login session).
    server
        .wait_for_new_stdout("TESTUX[login]: started", login_started_before_reconnect, Duration::from_secs(10))
        .await;
    let (video_frames, _) = poll_frames_tracking_gaps(&stream, tokio::time::sleep(Duration::from_secs(2))).await;
    assert!(video_frames > 0, "expected video frames from the fresh Login stage after reconnect");

    // ---- NOT exercised here: logging in again as the same user to actually
    // *resume* the backgrounded session above. `SessionManager::
    // handoff_to_user` has code for this (reusing the existing compositor
    // instead of spawning a fresh one), but it's a confirmed-broken, open
    // problem for `Backend::Kwin` — see that function's own doc comment for
    // the full story: un-pausing the resumed session's video pipeline
    // reliably leaves the client stuck forever re-requesting an IDR frame,
    // and a tempting fix (rebuild the pipeline fresh, the same way resize
    // already does) makes it worse by crashing the `kwin_wayland` process
    // itself. Left as a known gap rather than asserted here as if it works.
    // The backgrounded session itself is still real and still alive though
    // (confirmed below by killing its actual compositor process and
    // observing the death get detected) — only *resuming* it is unresolved.
    // ----

    // ---- Simulate a real logout: kill the User stage's kwin_wayland out
    // from under the server, the same way it would actually exit if a real
    // client used it to log out of Plasma. Before `SessionManager::
    // watch_user_session_exit` existed, nothing ever noticed this — server
    // state stayed `Streaming` around a session nothing could reconnect to,
    // exactly the bug reported live. Kills only the one process this test
    // itself spawned (scoped to this test's own unique runtime dir), never
    // a bare `pkill kwin_wayland` — that would just as happily take down a
    // real desktop session sharing the machine. ----
    let login_started_before_logout = server.count_stdout("TESTUX[login]: started");
    kill_broker_spawned_user_compositor(server._broker.child.id());

    // The watcher polls every 2s (see watch_user_session_exit) — give it
    // room to notice and reset state to Idle before trying to reconnect.
    tokio::time::sleep(Duration::from_secs(4)).await;

    let stream_config = host
        .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
        .await
        .expect("launch after simulated logout must succeed");
    let stream = MoonlightStream::connect(stream_config, settings, crypto_backend, video_capabilities())
        .await
        .expect("stream after simulated logout must connect");

    // A genuinely fresh Login stage, not a silent reconnect to the now-dead
    // User session — proves the server actually reset to `Idle` and
    // re-launched, rather than either erroring out or handing the client a
    // session with nothing behind it.
    server
        .wait_for_new_stdout("TESTUX[login]: started", login_started_before_logout, Duration::from_secs(10))
        .await;
    let (post_logout_frames, _) = poll_frames_tracking_gaps(&stream, tokio::time::sleep(Duration::from_secs(2))).await;
    assert!(post_logout_frames > 0, "expected video frames from the fresh Login stage after simulated logout");
}

/// Reproduces a live bug report end to end, in a controlled/debuggable
/// environment instead of guessing from a real machine's logs: connect,
/// log in, disconnect/reconnect (backgrounds the User session), log in
/// again as the same user (resumes it — a confirmed-broken, open problem
/// for `Backend::Kwin`'s video pipeline, see `SessionManager::
/// handoff_to_user`'s doc comment, deliberately not asserted on here),
/// disconnect/reconnect a *third* time, then check whether the fresh
/// Login stage's control channel actually works — real input, over the
/// real encrypted ENet control channel (not just "video frames arrive").
/// Live testing via `moonlight-web` against a real `sudo-live-session.sh`
/// run found the control channel's GCM decryption failing 100% of the
/// time after exactly this sequence (confirmed via `tracing::warn!`
/// instrumentation showing the stored and attempted-decrypt keys already
/// matched — not a simple key-mismatch bug), while server-side video kept
/// encoding normally the whole time. Bounded by an overall timeout so a
/// genuine hang fails the test cleanly instead of blocking the suite.
#[cfg(feature = "compositor-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn control_channel_survives_resume_then_reconnect() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

        let server = TestServer::spawn();

        let client_identity = ServerIdentity::generate().expect("generate client identity");
        let client_identifier = ClientIdentifier::from_pem(pem::parse(&client_identity.cert_pem).unwrap());
        let client_secret = ClientSecret::from_pem(pem::parse(&client_identity.private_key_pem).unwrap());

        let host = MoonlightHost::<TokioHyperClient>::new("127.0.0.1".to_string(), server.http_port, Some("it-client".to_string()))
            .expect("construct MoonlightHost");

        let pin = PairPin::new_random(&RustCryptoBackend).expect("generate pin");
        let pin_str = pin.to_string();
        let http_port = server.http_port;
        let submit_task = tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(300));
            ureq::post(&format!("http://127.0.0.1:{http_port}/submit-pin"))
                .send_form(&[("uniqueid", "it-client"), ("pin", &pin_str)])
                .expect("submit-pin request");
        });
        host.pair(&client_identifier, &client_secret, "connection-integration-test".to_string(), pin, RustCryptoBackend)
            .await
            .expect("pairing must succeed");
        submit_task.await.unwrap();

        let mut settings = default_stream_settings();
        let server_version = host.version().await.expect("server version");
        let gfe_version = host.gfe_version().await.expect("gfe version");
        let codec_support = host.server_codec_mode_support().await.expect("codec support");
        settings.adjust_for_server(server_version, &gfe_version, codec_support).expect("settings compatible");
        let crypto_backend = Arc::new(RustCryptoBackend);

        // ---- First connection: Login, then handoff to a fresh User session. ----
        let stream_config = host
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("first launch must succeed");
        let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
            .await
            .expect("first stream must connect");
        server.wait_for_stdout("TESTUX[login]: started", Duration::from_secs(45)).await;
        send_input_until_seen(
            &server.stdout_lines,
            &stream,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        send_key(&stream, VK_Q, true).await;
        send_key(&stream, VK_Q, false).await;
        server.wait_for_stdout("TESTUX[redfog-user-0]: started", Duration::from_secs(45)).await;

        // ---- Disconnect, reconnect: backgrounds the User session, shows a
        // fresh Login (the part already confirmed working elsewhere). ----
        drop(stream);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let login_started_before_resume = server.count_stdout("TESTUX[login]: started");
        let stream_config = host
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("second launch must succeed");
        let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
            .await
            .expect("second stream must connect");
        server
            .wait_for_new_stdout("TESTUX[login]: started", login_started_before_resume, Duration::from_secs(10))
            .await;

        // ---- Log in again as the same (placeholder "user") account —
        // `handoff_to_user` finds it already backgrounded and resumes it
        // instead of spawning fresh. Confirmed via the server's own log
        // line, not just inferred from timing. ----
        let resumed_before = server.count_stdout("resuming existing session for user");
        send_input_until_seen(
            &server.stdout_lines,
            &stream,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        send_key(&stream, VK_Q, true).await;
        send_key(&stream, VK_Q, false).await;
        server.wait_for_new_stdout("resuming existing session for user", resumed_before, Duration::from_secs(10)).await;
        // Deliberately not asserting anything about video here — the KWin
        // resume-video hang is a known, separate, still-open problem (see
        // `SessionManager::handoff_to_user`'s doc comment). Give it a
        // moment to reach its (hung or not) steady state before moving on,
        // matching the live timing this bug was found with.
        tokio::time::sleep(Duration::from_secs(3)).await;

        // ---- Disconnect, reconnect a *third* time — the actual question:
        // does the fresh Login stage's control channel (real input, over
        // the real encrypted ENet channel) actually work? A longer settle
        // time than the other reconnects above — a real human reconnecting
        // after seeing a resume hang wouldn't retry within 500ms. ----
        drop(stream);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let login_started_before_recovery = server.count_stdout("TESTUX[login]: started");
        let stream_config = host
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("third launch (recovery attempt) must succeed");
        let stream = MoonlightStream::connect(stream_config, settings, crypto_backend, video_capabilities())
            .await
            .expect("third stream (recovery attempt) must connect");
        server
            .wait_for_new_stdout("TESTUX[login]: started", login_started_before_recovery, Duration::from_secs(15))
            .await;

        let pointer_moved_before = server.count_stdout("TESTUX[login]: pointer_moved");
        send_input_until_new_seen(
            &server.stdout_lines,
            &stream,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            pointer_moved_before,
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let key_pressed_before = server.count_stdout("TESTUX[login]: key_pressed");
        send_key(&stream, VK_Q.wrapping_add(1), true).await;
        server
            .wait_for_new_stdout("TESTUX[login]: key_pressed", key_pressed_before, Duration::from_secs(10))
            .await;
    })
    .await
    .expect("control_channel_survives_resume_then_reconnect timed out — the control channel never recovered after resume+reconnect");
}

/// Regression test for a real bug found live: taking a session over from a
/// *different physical client* (not a same-client reconnect/resume — client
/// A never disconnects here) left the client that took it over receiving
/// literally no video/audio ever again, even though the server-side encoder
/// kept right on producing frames and the taken-over session's mouse/
/// keyboard input kept forwarding correctly the whole time.
///
/// Root cause: `SessionManager::build_pipelines`'s video/audio encoder
/// callbacks captured whichever `ClientKey` was current when the session was
/// first spawned, and used it to look their session back up in
/// `Shared::clients` on every single frame — fine for a same-client resume
/// (the key never changes), but `take_active_session_by_username` (this
/// takeover's own code path) *removes* the old client's slot and re-inserts
/// under the new client's key, without ever rebuilding the pipelines/
/// callbacks (see `rebuild_for_resume`'s doc comment for why that's
/// deliberate). The callbacks' stale lookup then silently found nothing and
/// returned before ever sending, forever, for both video and audio.
///
/// Fixed by looking sessions up by `generation` (stable across every future
/// resume/takeover on the same `RunningSession`) instead of `client_key` —
/// see `session_by_generation`. The assertion below is the one that would
/// have failed before that fix: not that the server *encoded* frames after
/// the takeover (it always did — that's what made this bug so easy to miss
/// from server-side logs alone), but that client B's own stream actually
/// *received* some.
#[cfg(feature = "compositor-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_client_takeover_keeps_video_flowing_to_the_new_client() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

        let server = TestServer::spawn();
        let http_port = server.http_port;
        let crypto_backend = Arc::new(RustCryptoBackend);

        // ---- Client A: logs all the way in and stays connected/actively
        // streaming — never disconnects. A takeover of an *actively attached*
        // session (not merely backgrounded) is what exercises
        // `take_active_session_by_username`'s "was_active_takeover" branch,
        // matching the exact live scenario this bug was found in. ----
        let identity_a = ServerIdentity::generate().expect("generate client A identity");
        let identifier_a = ClientIdentifier::from_pem(pem::parse(&identity_a.cert_pem).unwrap());
        let secret_a = ClientSecret::from_pem(pem::parse(&identity_a.private_key_pem).unwrap());
        let host_a = MoonlightHost::<TokioHyperClient>::new("127.0.0.1".to_string(), http_port, Some("it-client-a".to_string()))
            .expect("construct MoonlightHost for client A");
        let pin_a = PairPin::new_random(&RustCryptoBackend).expect("generate pin for client A");
        let pin_a_str = pin_a.to_string();
        let submit_task_a = tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(300));
            ureq::post(&format!("http://127.0.0.1:{http_port}/submit-pin"))
                .send_form(&[("uniqueid", "it-client-a"), ("pin", &pin_a_str)])
                .expect("submit-pin request for client A");
        });
        host_a
            .pair(&identifier_a, &secret_a, "connection-integration-test-a".to_string(), pin_a, RustCryptoBackend)
            .await
            .expect("client A pairing must succeed");
        submit_task_a.await.unwrap();

        let mut settings = default_stream_settings();
        let server_version = host_a.version().await.expect("server version");
        let gfe_version = host_a.gfe_version().await.expect("gfe version");
        let codec_support = host_a.server_codec_mode_support().await.expect("codec support");
        settings.adjust_for_server(server_version, &gfe_version, codec_support).expect("settings compatible");

        let stream_config_a = host_a
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("client A launch must succeed");
        let stream_a = MoonlightStream::connect(stream_config_a, settings.clone(), crypto_backend.clone(), video_capabilities())
            .await
            .expect("client A stream must connect");
        server.wait_for_stdout("TESTUX[login]: started", Duration::from_secs(45)).await;
        send_input_until_seen(
            &server.stdout_lines,
            &stream_a,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream_a, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream_a, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        send_key(&stream_a, VK_Q, true).await;
        send_key(&stream_a, VK_Q, false).await;
        server.wait_for_stdout("TESTUX[redfog-user-0]: started", Duration::from_secs(45)).await;
        // Confirm client A's own stream is genuinely alive/streaming before
        // handing it over — otherwise a "no frames" result below wouldn't
        // distinguish this bug from "nothing was ever streaming at all".
        let (baseline_frames, _) = poll_frames_tracking_gaps(&stream_a, tokio::time::sleep(Duration::from_secs(2))).await;
        assert!(baseline_frames > 0, "client A never received any video before the takeover — nothing to take over");

        // ---- Client B: a completely separate physical client (own identity,
        // own pairing) logs in as the same placeholder "user" account while
        // client A is still actively attached — the "taking over already-
        // active session ... from another client" path, not a plain resume. ----
        let identity_b = ServerIdentity::generate().expect("generate client B identity");
        let identifier_b = ClientIdentifier::from_pem(pem::parse(&identity_b.cert_pem).unwrap());
        let secret_b = ClientSecret::from_pem(pem::parse(&identity_b.private_key_pem).unwrap());
        let host_b = MoonlightHost::<TokioHyperClient>::new("127.0.0.1".to_string(), http_port, Some("it-client-b".to_string()))
            .expect("construct MoonlightHost for client B");
        let pin_b = PairPin::new_random(&RustCryptoBackend).expect("generate pin for client B");
        let pin_b_str = pin_b.to_string();
        let submit_task_b = tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(300));
            ureq::post(&format!("http://127.0.0.1:{http_port}/submit-pin"))
                .send_form(&[("uniqueid", "it-client-b"), ("pin", &pin_b_str)])
                .expect("submit-pin request for client B");
        });
        host_b
            .pair(&identifier_b, &secret_b, "connection-integration-test-b".to_string(), pin_b, RustCryptoBackend)
            .await
            .expect("client B pairing must succeed");
        submit_task_b.await.unwrap();

        let login_started_before_b = server.count_stdout("TESTUX[login]: started");
        let stream_config_b = host_b
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("client B launch must succeed");
        let stream_b = MoonlightStream::connect(stream_config_b, settings.clone(), crypto_backend.clone(), video_capabilities())
            .await
            .expect("client B stream must connect");
        server.wait_for_new_stdout("TESTUX[login]: started", login_started_before_b, Duration::from_secs(10)).await;
        send_input_until_seen(
            &server.stdout_lines,
            &stream_b,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream_b, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream_b, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let takeover_before = server.count_stdout("taking over already-active session for user");
        send_key(&stream_b, VK_Q, true).await;
        send_key(&stream_b, VK_Q, false).await;
        server.wait_for_new_stdout("taking over already-active session for user", takeover_before, Duration::from_secs(10)).await;

        // ---- The actual assertion: client B's own stream must actually
        // *receive* video after the takeover — not just that the server-side
        // encoder kept producing frames (it always did, even with the bug:
        // the drop happened at the send step, invisible from server-side
        // encoder logs alone). ----
        let (post_takeover_frames, _) = poll_frames_tracking_gaps(&stream_b, tokio::time::sleep(Duration::from_secs(5))).await;
        assert!(
            post_takeover_frames > 0,
            "client B received zero video frames in 5s after taking the session over from client A — \
             the encoder callbacks are still looking the session up by a stale client_key"
        );

        drop(stream_a);
        drop(stream_b);
    })
    .await
    .expect("cross_client_takeover_keeps_video_flowing_to_the_new_client timed out");
}

/// Reproduces a second live finding, distinct from the control-channel one
/// above: on a real machine, after a resume hang (see
/// `SessionManager::handoff_to_user`'s KNOWN LIMITATION doc comment), the
/// fixed video UDP port eventually became *permanently* stuck —
/// `bind_with_retry`'s 2s budget was nowhere near enough, and unlike a
/// transient race, it never recovered on its own even hours later; only
/// restarting `redfog-server` cleared it. Confirmed live via `ss -ulnp`:
/// the port's receive queue held ~180KB of unread datagrams, and the
/// process had accumulated 101 threads (`x264enc`'s own thread pool times
/// however many stuck sessions never actually tore down).
///
/// This test triggers exactly one resume hang, then checks whether a
/// *later* reconnect ever gets a working video stream again — repeating
/// the check a few times with real settle time between attempts, since the
/// live symptom was "never recovers", not "occasionally flaky".
#[cfg(feature = "compositor-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn video_port_recovers_after_a_resume_hang() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

        let server = TestServer::spawn();

        let client_identity = ServerIdentity::generate().expect("generate client identity");
        let client_identifier = ClientIdentifier::from_pem(pem::parse(&client_identity.cert_pem).unwrap());
        let client_secret = ClientSecret::from_pem(pem::parse(&client_identity.private_key_pem).unwrap());

        let host = MoonlightHost::<TokioHyperClient>::new("127.0.0.1".to_string(), server.http_port, Some("it-client".to_string()))
            .expect("construct MoonlightHost");

        let pin = PairPin::new_random(&RustCryptoBackend).expect("generate pin");
        let pin_str = pin.to_string();
        let http_port = server.http_port;
        let submit_task = tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(300));
            ureq::post(&format!("http://127.0.0.1:{http_port}/submit-pin"))
                .send_form(&[("uniqueid", "it-client"), ("pin", &pin_str)])
                .expect("submit-pin request");
        });
        host.pair(&client_identifier, &client_secret, "connection-integration-test".to_string(), pin, RustCryptoBackend)
            .await
            .expect("pairing must succeed");
        submit_task.await.unwrap();

        let mut settings = default_stream_settings();
        let server_version = host.version().await.expect("server version");
        let gfe_version = host.gfe_version().await.expect("gfe version");
        let codec_support = host.server_codec_mode_support().await.expect("codec support");
        settings.adjust_for_server(server_version, &gfe_version, codec_support).expect("settings compatible");
        let crypto_backend = Arc::new(RustCryptoBackend);

        // ---- First connection: Login, then handoff to a fresh User session. ----
        let stream_config = host
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("first launch must succeed");
        let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
            .await
            .expect("first stream must connect");
        server.wait_for_stdout("TESTUX[login]: started", Duration::from_secs(45)).await;
        send_input_until_seen(
            &server.stdout_lines,
            &stream,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        send_key(&stream, VK_Q, true).await;
        send_key(&stream, VK_Q, false).await;
        server.wait_for_stdout("TESTUX[redfog-user-0]: started", Duration::from_secs(45)).await;

        // ---- Disconnect, reconnect: backgrounds the User session, shows a
        // fresh Login. ----
        drop(stream);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let login_started_before_resume = server.count_stdout("TESTUX[login]: started");
        let stream_config = host
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("second launch must succeed");
        let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
            .await
            .expect("second stream must connect");
        server
            .wait_for_new_stdout("TESTUX[login]: started", login_started_before_resume, Duration::from_secs(10))
            .await;

        // ---- Trigger exactly one resume hang. ----
        let resumed_before = server.count_stdout("resuming existing session for user");
        send_input_until_seen(
            &server.stdout_lines,
            &stream,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        send_key(&stream, VK_Q, true).await;
        send_key(&stream, VK_Q, false).await;
        server.wait_for_new_stdout("resuming existing session for user", resumed_before, Duration::from_secs(10)).await;
        // Real settle time — matching the live timing this was found with,
        // not asserting anything about video here (the resume-video hang
        // itself is a known, separate, still-open problem).
        tokio::time::sleep(Duration::from_secs(5)).await;
        drop(stream);

        // ---- The actual question: does a *later* reconnect ever get a
        // working video stream again? Repeat a few times with real settle
        // time between attempts — the live symptom was "never recovers",
        // not "occasionally flaky", so even a couple of attempts spread
        // over real wall-clock time should be enough to show it either
        // way. ----
        // Checked via the *server's* own "video client announced itself"
        // log (proof the client's PING reached us and the video sender
        // actually learned a real address to send to), not the client's
        // own frame buffer — confirmed live that the reference client
        // library can tear down its whole stream (video included) right
        // after successfully receiving a frame, due to a separate,
        // already-known "control stream hasn't successfully connected
        // yet" quirk on rapid reconnects — racing a frame-count check
        // against that teardown produces false negatives even when video
        // genuinely recovered.
        let mut recovered = false;
        for attempt in 1..=4 {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let bind_failures_before = server.count_stdout("failed to bind video sender");
            let login_started_before = server.count_stdout("TESTUX[login]: started");
            let announced_before = server.count_stdout("video client announced itself");
            let stream_config = match host
                .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("attempt {attempt}: launch failed: {e}");
                    continue;
                }
            };
            let Ok(stream) = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities()).await else {
                eprintln!("attempt {attempt}: stream connect failed");
                continue;
            };
            // Give the fresh Login a moment to actually start before
            // checking for frames — matches other tests' own timing.
            let _ = tokio::time::timeout(
                Duration::from_secs(10),
                server.wait_for_new_stdout("TESTUX[login]: started", login_started_before, Duration::from_secs(10)),
            )
            .await;
            let video_announced = tokio::time::timeout(
                Duration::from_secs(10),
                server.wait_for_new_stdout("video client announced itself", announced_before, Duration::from_secs(10)),
            )
            .await
            .is_ok();
            let bind_failed = server.count_stdout("failed to bind video sender") > bind_failures_before;
            eprintln!("attempt {attempt}: video_announced={video_announced}, bind_failed={bind_failed}");
            drop(stream);
            if video_announced && !bind_failed {
                recovered = true;
                break;
            }
        }
        assert!(
            recovered,
            "video port never recovered after the resume hang across 4 reconnect attempts — matches the live \"needs a full restart\" symptom"
        );
    })
    .await
    .expect("video_port_recovers_after_a_resume_hang timed out");
}

/// Tracks video frame delivery via the *server's* own "video encoder
/// produced" log line rather than the client's `poll_video_frame()` —
/// deliberately, not for lack of a client-side option. `poll_video_frame()`
/// depends on the reference client library's own stream object staying
/// fully alive, and `video_port_recovers_after_a_resume_hang`'s own doc
/// comment already documents that it can tear its whole stream down right
/// after a rapid reconnect (a separate, known "control stream hasn't
/// successfully connected yet" quirk) — confirmed hitting exactly that
/// here: an earlier version of this test using `poll_frames_tracking_gaps`
/// reported *zero* frames on every single cycle, uniformly, which is the
/// signature of the client giving up, not the server failing to send
/// anything (the server's own logs showed frames the whole time). Counting
/// server-side log lines sidesteps that client-library quirk entirely.
#[cfg(feature = "compositor-tests")]
async fn track_server_side_video_frame_gaps(server: &TestServer, baseline: usize, window: Duration) -> (usize, Duration) {
    let (frames, max_gap, _gaps) = track_server_side_video_frame_gap_sequence(server, baseline, window).await;
    (frames, max_gap)
}

/// Same idea as `track_server_side_video_frame_gaps`, but also returns every
/// individual gap in arrival order — needed to check for a *pattern* (a
/// gap sequence that grows over time, or periodizes) rather than just a
/// single worst-case number, matching how the original throttling bug was
/// actually characterized live (progressively growing gaps over several
/// *minutes* of one continuous connection, eventually settling into a
/// ~60s-periodic cadence) rather than found via many quick reconnects.
#[cfg(feature = "compositor-tests")]
async fn track_server_side_video_frame_gap_sequence(server: &TestServer, baseline: usize, window: Duration) -> (usize, Duration, Vec<Duration>) {
    let window_start = tokio::time::Instant::now();
    let mut last_count = baseline;
    let mut last_change_at = window_start;
    let mut max_gap = Duration::ZERO;
    let mut gaps = Vec::new();
    while tokio::time::Instant::now() < window_start + window {
        tokio::time::sleep(Duration::from_millis(30)).await;
        let count = server.count_stdout("video encoder produced");
        if count > last_count {
            let now = tokio::time::Instant::now();
            let gap = now.duration_since(last_change_at);
            gaps.push(gap);
            max_gap = max_gap.max(gap);
            last_change_at = now;
            last_count = count;
        }
    }
    let tail_gap = tokio::time::Instant::now().saturating_duration_since(last_change_at);
    max_gap = max_gap.max(tail_gap);
    (last_count - baseline, max_gap, gaps)
}

/// Tries to reproduce, on the *full* real pipeline (real redfog-server,
/// real RTSP/video/control ports, x264enc, real UDP packet delivery to a
/// real client library — not the simplified single-process
/// `sustained_multi_resume_probe` example in redfog-core, which ran up to
/// 40 resume cycles with real continuously-rendering `glxgears` and never
/// showed any degradation at all), the severe post-resume video throttling
/// found live (see project memory / `kwin-capture`'s `current_stream` doc
/// comment for the abandoned-screencast-stream theory this was chasing).
/// `redfog-test-ux` is used as both Login and User stage specifically
/// because it repaints continuously on its own — same continuous-damage
/// guarantee `glxgears` was standing in for live, without an external
/// process dependency.
///
/// If this *does* reproduce it, the `max_gap` numbers printed per cycle are
/// the evidence; if it still doesn't, that's further evidence the bug needs
/// something even this harness doesn't have — a long-lived `kwin_wayland`
/// process accumulating state over real hours, not just several cycles in
/// under a couple of minutes.
#[cfg(feature = "compositor-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn video_stays_sustained_across_many_resumes_under_continuous_rendering() {
    const RESUME_CYCLES: usize = 8;
    const SUSTAIN_WINDOW: Duration = Duration::from_secs(6);
    const MAX_ACCEPTABLE_GAP: Duration = Duration::from_millis(1500);

    tokio::time::timeout(Duration::from_secs(240), async {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

        let server = TestServer::spawn();

        let client_identity = ServerIdentity::generate().expect("generate client identity");
        let client_identifier = ClientIdentifier::from_pem(pem::parse(&client_identity.cert_pem).unwrap());
        let client_secret = ClientSecret::from_pem(pem::parse(&client_identity.private_key_pem).unwrap());

        let host = MoonlightHost::<TokioHyperClient>::new("127.0.0.1".to_string(), server.http_port, Some("it-client".to_string()))
            .expect("construct MoonlightHost");

        let pin = PairPin::new_random(&RustCryptoBackend).expect("generate pin");
        let pin_str = pin.to_string();
        let http_port = server.http_port;
        let submit_task = tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(300));
            ureq::post(&format!("http://127.0.0.1:{http_port}/submit-pin"))
                .send_form(&[("uniqueid", "it-client"), ("pin", &pin_str)])
                .expect("submit-pin request");
        });
        host.pair(&client_identifier, &client_secret, "connection-integration-test".to_string(), pin, RustCryptoBackend)
            .await
            .expect("pairing must succeed");
        submit_task.await.unwrap();

        let mut settings = default_stream_settings();
        let server_version = host.version().await.expect("server version");
        let gfe_version = host.gfe_version().await.expect("gfe version");
        let codec_support = host.server_codec_mode_support().await.expect("codec support");
        settings.adjust_for_server(server_version, &gfe_version, codec_support).expect("settings compatible");
        let crypto_backend = Arc::new(RustCryptoBackend);

        // ---- First connection: Login, then handoff to a fresh User session. ----
        let stream_config = host
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("first launch must succeed");
        let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
            .await
            .expect("first stream must connect");
        server.wait_for_stdout("TESTUX[login]: started", Duration::from_secs(45)).await;
        send_input_until_seen(
            &server.stdout_lines,
            &stream,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        send_key(&stream, VK_Q, true).await;
        send_key(&stream, VK_Q, false).await;
        server.wait_for_stdout("TESTUX[redfog-user-0]: started", Duration::from_secs(45)).await;
        drop(stream);

        // ---- Repeated resume cycles: reconnect, drive Login -> User again
        // (background_sessions resumes the *same* User session rather than
        // spawning a fresh one), then observe frame delivery continuously
        // for a real, sustained window before disconnecting for the next
        // cycle — tracking the longest gap between frames, not just
        // whether any frame showed up. ----
        let mut cycle_results = Vec::new();
        for cycle in 1..=RESUME_CYCLES {
            tokio::time::sleep(Duration::from_millis(500)).await;

            let login_started_before = server.count_stdout("TESTUX[login]: started");
            let resumed_before = server.count_stdout("resuming existing session for user");
            let stream_config = host
                .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
                .await
                .unwrap_or_else(|e| panic!("cycle {cycle}: launch failed: {e}"));
            let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
                .await
                .unwrap_or_else(|e| panic!("cycle {cycle}: stream connect failed: {e}"));
            server.wait_for_new_stdout("TESTUX[login]: started", login_started_before, Duration::from_secs(10)).await;
            send_input_until_seen(
                &server.stdout_lines,
                &stream,
                ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
                "TESTUX[login]: pointer_moved",
                Duration::from_secs(10),
            )
            .await;
            send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
            send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            send_key(&stream, VK_Q, true).await;
            send_key(&stream, VK_Q, false).await;
            server.wait_for_new_stdout("resuming existing session for user", resumed_before, Duration::from_secs(10)).await;

            let baseline = server.count_stdout("video encoder produced");
            let (frames, max_gap) = track_server_side_video_frame_gaps(&server, baseline, SUSTAIN_WINDOW).await;
            eprintln!("cycle {cycle}: {frames} frames, max gap {max_gap:?}");
            cycle_results.push((cycle, frames, max_gap));
            drop(stream);
        }

        let worst = cycle_results.iter().max_by_key(|(_, _, gap)| *gap).unwrap();
        eprintln!("worst cycle: {worst:?}");
        for (cycle, frames, _gap) in &cycle_results {
            assert!(*frames > 0, "cycle {cycle}: zero video frames in a {SUSTAIN_WINDOW:?} window — full stall, not just throttling");
        }
        assert!(
            worst.2 <= MAX_ACCEPTABLE_GAP,
            "cycle {}: max gap {:?} exceeds {MAX_ACCEPTABLE_GAP:?} — severe post-resume throttling reproduced on the full pipeline; \
             per-cycle results: {cycle_results:?}",
            worst.0,
            worst.2
        );
    })
    .await
    .expect("video_stays_sustained_across_many_resumes_under_continuous_rendering timed out");
}

/// Different shape than `video_stays_sustained_across_many_resumes_under_
/// continuous_rendering`: that test does *many* resumes with a short (6s)
/// observation window each and stayed clean. But the throttling bug was
/// originally characterized live from a *single* continuous connection,
/// observed over several *minutes*, showing gaps that grew progressively
/// and eventually settled into a suspiciously exact ~60s-periodic cadence —
/// a pattern many-short-cycles can't show even in principle, since it
/// never gives any one resumed connection enough sustained real time to
/// drift. This test does the opposite trade: one resume, then a single
/// long (90s) observation window, printing every individual gap in order
/// so a growing/periodic pattern would actually be visible in the output,
/// not just collapsed into a single worst-case number.
#[cfg(feature = "compositor-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn video_frame_gaps_over_one_long_sustained_resumed_connection() {
    const OBSERVE_WINDOW: Duration = Duration::from_secs(90);
    const MAX_ACCEPTABLE_GAP: Duration = Duration::from_millis(1500);

    tokio::time::timeout(Duration::from_secs(150), async {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

        let server = TestServer::spawn();

        let client_identity = ServerIdentity::generate().expect("generate client identity");
        let client_identifier = ClientIdentifier::from_pem(pem::parse(&client_identity.cert_pem).unwrap());
        let client_secret = ClientSecret::from_pem(pem::parse(&client_identity.private_key_pem).unwrap());

        let host = MoonlightHost::<TokioHyperClient>::new("127.0.0.1".to_string(), server.http_port, Some("it-client".to_string()))
            .expect("construct MoonlightHost");

        let pin = PairPin::new_random(&RustCryptoBackend).expect("generate pin");
        let pin_str = pin.to_string();
        let http_port = server.http_port;
        let submit_task = tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(300));
            ureq::post(&format!("http://127.0.0.1:{http_port}/submit-pin"))
                .send_form(&[("uniqueid", "it-client"), ("pin", &pin_str)])
                .expect("submit-pin request");
        });
        host.pair(&client_identifier, &client_secret, "connection-integration-test".to_string(), pin, RustCryptoBackend)
            .await
            .expect("pairing must succeed");
        submit_task.await.unwrap();

        let mut settings = default_stream_settings();
        let server_version = host.version().await.expect("server version");
        let gfe_version = host.gfe_version().await.expect("gfe version");
        let codec_support = host.server_codec_mode_support().await.expect("codec support");
        settings.adjust_for_server(server_version, &gfe_version, codec_support).expect("settings compatible");
        let crypto_backend = Arc::new(RustCryptoBackend);

        // ---- First connection: Login, then handoff to a fresh User session. ----
        let stream_config = host
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("first launch must succeed");
        let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
            .await
            .expect("first stream must connect");
        server.wait_for_stdout("TESTUX[login]: started", Duration::from_secs(45)).await;
        send_input_until_seen(
            &server.stdout_lines,
            &stream,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        send_key(&stream, VK_Q, true).await;
        send_key(&stream, VK_Q, false).await;
        server.wait_for_stdout("TESTUX[redfog-user-0]: started", Duration::from_secs(45)).await;
        drop(stream);

        // ---- Exactly one resume, then a long, single sustained observation. ----
        tokio::time::sleep(Duration::from_millis(500)).await;
        let login_started_before = server.count_stdout("TESTUX[login]: started");
        let resumed_before = server.count_stdout("resuming existing session for user");
        let stream_config = host
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("resume launch must succeed");
        let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
            .await
            .expect("resume stream must connect");
        server.wait_for_new_stdout("TESTUX[login]: started", login_started_before, Duration::from_secs(10)).await;
        send_input_until_seen(
            &server.stdout_lines,
            &stream,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        send_key(&stream, VK_Q, true).await;
        send_key(&stream, VK_Q, false).await;
        server.wait_for_new_stdout("resuming existing session for user", resumed_before, Duration::from_secs(10)).await;

        let baseline = server.count_stdout("video encoder produced");
        let (frames, max_gap, gaps) = track_server_side_video_frame_gap_sequence(&server, baseline, OBSERVE_WINDOW).await;
        eprintln!("post-resume: {frames} frames over {OBSERVE_WINDOW:?}, max gap {max_gap:?}");
        eprintln!("gap sequence ({} gaps): {gaps:?}", gaps.len());
        drop(stream);

        assert!(frames > 0, "zero video frames in a {OBSERVE_WINDOW:?} window after resume — full stall, not just throttling");
        assert!(
            max_gap <= MAX_ACCEPTABLE_GAP,
            "max gap {max_gap:?} exceeds {MAX_ACCEPTABLE_GAP:?} over a single {OBSERVE_WINDOW:?} post-resume connection — \
             severe throttling reproduced; full gap sequence: {gaps:?}"
        );
    })
    .await
    .expect("video_frame_gaps_over_one_long_sustained_resumed_connection timed out");
}

/// Confirmed reproduction of the live "slow after resume" throttling, on
/// the full real pipeline — after two other test shapes in this file came
/// back completely clean (many quick resumes; one resume with a long 90s
/// window), both using `redfog-test-ux`'s own continuous ~33ms auto-repaint
/// timer as the damage source. The missing variable turned out not to be
/// idle-gap length (an early version of this test also tried that — same
/// throttling appeared with the idle gap shrunk to 100ms, ruling it out)
/// but *how* damage is generated: driving it via discrete input events
/// (`ClientInputEvent::MouseMoveAbsolute`, which `redfog-test-ux` repaints
/// in response to, same as egui's normal input-driven repaint) instead of
/// a steady internal timer. A real desktop has no such timer — its damage
/// is inherently event-driven — so the earlier "healthy" results were
/// specifically an artifact of `redfog-test-ux`'s artificial always-on
/// repaint, not evidence the bug doesn't exist.
///
/// Verified with a same-mechanism, same-session control: input-driven
/// damage *before* any resume is completely healthy (465 frames/10s, max
/// gap 68ms, no pattern), while the identical mechanism *after* a resume
/// on the same session reliably shows ~37-40 frames/10s (versus ~450-465
/// healthy) in a distinctive alternating fast/slow pattern (~31ms, then
/// ~700ms, repeating) — reproduced identically across multiple runs.
///
/// Needs `redfog-test-ux`'s `REDFOG_TEST_UX_NO_AUTOREPAINT` (only on the
/// User stage, via `extra_broker_env` — Login keeps its own auto-repaint so
/// the Login->User handoff stays reliable) so input is the *only* damage
/// source, matching a real desktop instead of masking the bug behind an
/// artificial timer.
#[cfg(feature = "compositor-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn video_throttles_after_resume_under_input_driven_damage() {
    const DAMAGE_WINDOW: Duration = Duration::from_secs(10);
    // A healthy connection (see this test's own pre-resume control run)
    // delivers ~450-465 frames in this window; the reproduced bug delivers
    // ~37-40. Well below either number, so this can't pass by accident.
    const MIN_HEALTHY_FRAMES: usize = 200;

    tokio::time::timeout(Duration::from_secs(60), async {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

        let server = TestServer::spawn_with_broker_env(vec![("REDFOG_TEST_UX_NO_AUTOREPAINT".to_string(), "1".to_string())]);

        let client_identity = ServerIdentity::generate().expect("generate client identity");
        let client_identifier = ClientIdentifier::from_pem(pem::parse(&client_identity.cert_pem).unwrap());
        let client_secret = ClientSecret::from_pem(pem::parse(&client_identity.private_key_pem).unwrap());

        let host = MoonlightHost::<TokioHyperClient>::new("127.0.0.1".to_string(), server.http_port, Some("it-client".to_string()))
            .expect("construct MoonlightHost");

        let pin = PairPin::new_random(&RustCryptoBackend).expect("generate pin");
        let pin_str = pin.to_string();
        let http_port = server.http_port;
        let submit_task = tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(300));
            ureq::post(&format!("http://127.0.0.1:{http_port}/submit-pin"))
                .send_form(&[("uniqueid", "it-client"), ("pin", &pin_str)])
                .expect("submit-pin request");
        });
        host.pair(&client_identifier, &client_secret, "connection-integration-test".to_string(), pin, RustCryptoBackend)
            .await
            .expect("pairing must succeed");
        submit_task.await.unwrap();

        let mut settings = default_stream_settings();
        let server_version = host.version().await.expect("server version");
        let gfe_version = host.gfe_version().await.expect("gfe version");
        let codec_support = host.server_codec_mode_support().await.expect("codec support");
        settings.adjust_for_server(server_version, &gfe_version, codec_support).expect("settings compatible");
        let crypto_backend = Arc::new(RustCryptoBackend);

        // ---- First connection: Login (still auto-repaints), then handoff
        // to a fresh User session (no auto-repaint — input is its only
        // damage source from here on). ----
        let stream_config = host
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("first launch must succeed");
        let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
            .await
            .expect("first stream must connect");
        server.wait_for_stdout("TESTUX[login]: started", Duration::from_secs(45)).await;
        send_input_until_seen(
            &server.stdout_lines,
            &stream,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        send_key(&stream, VK_Q, true).await;
        send_key(&stream, VK_Q, false).await;
        server.wait_for_stdout("TESTUX[redfog-user-0]: started", Duration::from_secs(45)).await;

        // ---- Control: same input-driven damage mechanism, same session,
        // *before* any resume — must be healthy, or this test isn't
        // isolating what it claims to. ----
        let pre_resume_frames = {
            let baseline = server.count_stdout("video encoder produced");
            println!("=== MARKER: pre-resume damage window START ===");
            let stream = stream.clone();
            let damage_task = tokio::spawn(drive_mouse_wiggle_damage(stream));
            let (frames, max_gap, gaps) = track_server_side_video_frame_gap_sequence(&server, baseline, DAMAGE_WINDOW).await;
            damage_task.abort();
            println!("=== MARKER: pre-resume damage window END ===");
            eprintln!("pre-resume control: {frames} frames over {DAMAGE_WINDOW:?}, max gap {max_gap:?}, gaps: {gaps:?}");
            frames
        };
        drop(stream);
        assert!(
            pre_resume_frames >= MIN_HEALTHY_FRAMES,
            "pre-resume control itself was unhealthy ({pre_resume_frames} frames, expected >= {MIN_HEALTHY_FRAMES}) — this test isn't \
             isolating what it claims to, investigate the control before trusting the post-resume result"
        );

        // ---- Reconnect: triggers the actual resume path
        // (`rebuild_for_resume`) against that same User session. ----
        tokio::time::sleep(Duration::from_millis(500)).await;
        let login_started_before = server.count_stdout("TESTUX[login]: started");
        let resumed_before = server.count_stdout("resuming existing session for user");
        let stream_config = host
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("resume launch must succeed");
        let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
            .await
            .expect("resume stream must connect");
        server.wait_for_new_stdout("TESTUX[login]: started", login_started_before, Duration::from_secs(10)).await;
        send_input_until_seen(
            &server.stdout_lines,
            &stream,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        send_key(&stream, VK_Q, true).await;
        send_key(&stream, VK_Q, false).await;
        server.wait_for_new_stdout("resuming existing session for user", resumed_before, Duration::from_secs(10)).await;

        // ---- The actual check: same input-driven damage mechanism, same
        // session, *after* the resume. ----
        let baseline = server.count_stdout("video encoder produced");
        println!("=== MARKER: post-resume damage window START ===");
        let damage_task = tokio::spawn(drive_mouse_wiggle_damage(stream.clone()));
        let (frames, max_gap, gaps) = track_server_side_video_frame_gap_sequence(&server, baseline, DAMAGE_WINDOW).await;
        damage_task.abort();
        println!("=== MARKER: post-resume damage window END ===");
        eprintln!("post-resume: {frames} frames over {DAMAGE_WINDOW:?}, max gap {max_gap:?}, gaps: {gaps:?}");
        drop(stream);

        assert!(
            frames >= MIN_HEALTHY_FRAMES,
            "post-resume video throttling reproduced: only {frames} frames over {DAMAGE_WINDOW:?} under continuous input-driven \
             damage (pre-resume control on the same session delivered {pre_resume_frames}); max gap {max_gap:?}; gap sequence: {gaps:?}"
        );
    })
    .await
    .expect("video_throttles_after_resume_under_input_driven_damage timed out");
}

/// Continuously sends mouse-move input over `stream` forever (meant to be
/// spawned and later `.abort()`ed) — the input-driven damage source for
/// `video_throttles_after_resume_under_input_driven_damage`'s pre/post-resume
/// comparison, matching how a real desktop's damage is actually generated
/// (event-driven, not a steady internal repaint timer).
#[cfg(feature = "compositor-tests")]
async fn drive_mouse_wiggle_damage(stream: MoonlightStream) {
    let mut x = 640.0;
    loop {
        x = if x > 700.0 { 600.0 } else { x + 10.0 };
        send_input_retrying(&stream, ClientInputEvent::MouseMoveAbsolute { x: x as i16, y: 360, reference_width: 1280, reference_height: 720 }).await;
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
}

/// Reproduces a third live finding: after a resume hang wedges the shared
/// PipeWire daemon (same trigger `video_port_recovers_after_a_resume_hang`
/// uses), logging out and logging back in again never completes — no
/// error, just permanent silence. Confirmed live via a stuck production
/// `redfog-server`: its log stopped advancing entirely right after "login
/// session exited, handing off to user session", and the process was still
/// alive (not crashed) minutes later.
///
/// Sends a real `LoginRequest::LogOut` directly over the login-report
/// socket — the same wire message the login screen's own "Log out" button
/// sends — rather than driving a UI, since `redfog-test-ux`'s headless
/// Login stand-in has no such button to click (see its own doc comment).
#[cfg(feature = "compositor-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_after_log_out_recovers_from_a_resume_hang() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

        let server = TestServer::spawn();

        let client_identity = ServerIdentity::generate().expect("generate client identity");
        let client_identifier = ClientIdentifier::from_pem(pem::parse(&client_identity.cert_pem).unwrap());
        let client_secret = ClientSecret::from_pem(pem::parse(&client_identity.private_key_pem).unwrap());

        let host = MoonlightHost::<TokioHyperClient>::new("127.0.0.1".to_string(), server.http_port, Some("it-client".to_string()))
            .expect("construct MoonlightHost");

        let pin = PairPin::new_random(&RustCryptoBackend).expect("generate pin");
        let pin_str = pin.to_string();
        let http_port = server.http_port;
        let submit_task = tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(300));
            ureq::post(&format!("http://127.0.0.1:{http_port}/submit-pin"))
                .send_form(&[("uniqueid", "it-client"), ("pin", &pin_str)])
                .expect("submit-pin request");
        });
        host.pair(&client_identifier, &client_secret, "connection-integration-test".to_string(), pin, RustCryptoBackend)
            .await
            .expect("pairing must succeed");
        submit_task.await.unwrap();

        let mut settings = default_stream_settings();
        let server_version = host.version().await.expect("server version");
        let gfe_version = host.gfe_version().await.expect("gfe version");
        let codec_support = host.server_codec_mode_support().await.expect("codec support");
        settings.adjust_for_server(server_version, &gfe_version, codec_support).expect("settings compatible");
        let crypto_backend = Arc::new(RustCryptoBackend);

        // ---- First connection: Login, then handoff to a fresh User session. ----
        let stream_config = host
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("first launch must succeed");
        let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
            .await
            .expect("first stream must connect");
        server.wait_for_stdout("TESTUX[login]: started", Duration::from_secs(45)).await;
        send_input_until_seen(
            &server.stdout_lines,
            &stream,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        send_key(&stream, VK_Q, true).await;
        send_key(&stream, VK_Q, false).await;
        server.wait_for_stdout("TESTUX[redfog-user-0]: started", Duration::from_secs(45)).await;

        // ---- Disconnect, reconnect: backgrounds the User session, shows a
        // fresh Login. ----
        drop(stream);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let login_started_before_resume = server.count_stdout("TESTUX[login]: started");
        let stream_config = host
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("second launch must succeed");
        let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
            .await
            .expect("second stream must connect");
        server
            .wait_for_new_stdout("TESTUX[login]: started", login_started_before_resume, Duration::from_secs(10))
            .await;

        // ---- Log in again as the same placeholder user — triggers the
        // resume path (and, on real hardware, the known KWin/PipeWire
        // resume hang this whole scenario depends on wedging). ----
        let resumed_before = server.count_stdout("resuming existing session for user");
        send_input_until_seen(
            &server.stdout_lines,
            &stream,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        send_key(&stream, VK_Q, true).await;
        send_key(&stream, VK_Q, false).await;
        server.wait_for_new_stdout("resuming existing session for user", resumed_before, Duration::from_secs(10)).await;
        // Real settle time, matching the live timing this bug was found
        // with — deliberately not asserting anything about video here (the
        // resume-video hang itself is a known, separate, still-open
        // problem).
        tokio::time::sleep(Duration::from_secs(5)).await;
        drop(stream);

        // ---- The actual question: log out, then log in again — does the
        // handoff for that *second* login ever complete? ----
        let log_out_response = send_login_request(
            &server.login_socket_path(),
            redfog_login_protocol::LoginRequest::LogOut { username: "user".to_string(), password: String::new() },
        )
        .await;
        assert!(
            matches!(log_out_response, redfog_login_protocol::LoginResponse::LogOut(Ok(()))),
            "log-out must succeed: {log_out_response:?}"
        );

        let login_started_before_relogin = server.count_stdout("TESTUX[login]: started");
        let user_started_before_relogin = server.count_stdout("TESTUX[redfog-user-0]: started");
        let stream_config = host
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("third launch (after log-out) must succeed");
        let stream = MoonlightStream::connect(stream_config, settings, crypto_backend, video_capabilities())
            .await
            .expect("third stream (after log-out) must connect");
        server
            .wait_for_new_stdout("TESTUX[login]: started", login_started_before_relogin, Duration::from_secs(10))
            .await;
        // `send_input_until_seen` (a bare "has this needle ever appeared")
        // would be trivially satisfied by the *first* or *second* login's
        // own identical log lines — need `_new_seen` with an explicit
        // baseline to actually prove *this* (third) login received it, same
        // reasoning as `control_channel_survives_resume_then_reconnect`'s
        // own third connection.
        let pointer_moved_before = server.count_stdout("TESTUX[login]: pointer_moved");
        send_input_until_new_seen(
            &server.stdout_lines,
            &stream,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            pointer_moved_before,
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let key_pressed_before = server.count_stdout("TESTUX[login]: key_pressed");
        send_key(&stream, VK_Q, true).await;
        server.wait_for_new_stdout("TESTUX[login]: key_pressed", key_pressed_before, Duration::from_secs(10)).await;
        send_key(&stream, VK_Q, false).await;

        // The actual assertion: a genuinely fresh User-stage spawn (a new
        // `TESTUX[redfog-user-0]: started`, since log-out removed the old
        // one from `background_sessions` — this must NOT just resume
        // something stale) completes within a generous but bounded window,
        // rather than hanging forever with no error at all (the live
        // symptom).
        server
            .wait_for_new_stdout("TESTUX[redfog-user-0]: started", user_started_before_relogin, Duration::from_secs(30))
            .await;
    })
    .await
    .expect("login_after_log_out_recovers_from_a_resume_hang timed out — matches the live symptom of the handoff hanging forever after a log-out during a PipeWire wedge");
}

/// Finds a `kwin_wayland` process that's a *grandchild* of `broker_pid` via
/// an intervening `dbus-run-session` — the exact shape `spawn_via_pam` (real
/// production) and `spawn_fake_pam` (its sudo-free test stand-in) both
/// spawn, unlike `spawn_fake`'s flat direct-child shape (see
/// `kill_broker_spawned_user_compositor`, which only looks one level deep).
/// Matches via `/proc/<pid>/status`'s `PPid:` field for the same reason that
/// helper does (see its own doc comment on why not `environ`).
#[cfg(feature = "compositor-tests")]
fn find_broker_grandchild_kwin_wayland_pid(broker_pid: u32) -> Option<u32> {
    let ppid_of = |pid: &str| -> Option<u32> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        status.lines().find_map(|l| l.strip_prefix("PPid:")).and_then(|v| v.trim().parse::<u32>().ok())
    };
    let comm_of = |pid: &str| -> Option<String> { std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_string()) };

    let dbus_run_session_pids: Vec<String> = std::fs::read_dir("/proc")
        .expect("read /proc")
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_string_lossy().parse::<u32>().ok()?.to_string();
            // `/proc/<pid>/comm` truncates to 15 visible chars (16-byte
            // `TASK_COMM_LEN` including the null terminator) — confirmed
            // live: "dbus-run-session" (16 chars) shows up as
            // "dbus-run-sessio".
            (comm_of(&pid).as_deref() == Some("dbus-run-sessio") && ppid_of(&pid) == Some(broker_pid)).then_some(pid)
        })
        .collect();

    std::fs::read_dir("/proc").expect("read /proc").flatten().find_map(|entry| {
        let pid = entry.file_name().to_string_lossy().parse::<u32>().ok()?;
        let pid_str = pid.to_string();
        (comm_of(&pid_str).as_deref() == Some("kwin_wayland") && dbus_run_session_pids.contains(&ppid_of(&pid_str)?.to_string())).then_some(pid)
    })
}

/// Directly reproduces (and verifies the fix for) the actual live bug found
/// after `start_streaming`/`handoff_to_user`'s timeout fixes still didn't
/// resolve the user's live "log out, then can't open a new session"
/// symptom: the broker's `terminate()` used to call `child.kill()` on only
/// the top-level tracked PID, but `spawn_via_pam` spawns `redfog-session-
/// init` -> (exec) `dbus-run-session` -> (fork) real `kwin_wayland` — so
/// killing the tracked PID killed `dbus-run-session` but left `kwin_wayland`
/// itself orphaned and running forever, still holding its PipeWire/DRM
/// connection, degrading every session spawned after it. Confirmed live via
/// a real orphaned `kwin_wayland` with `PPid=1` surviving a log-out.
///
/// Uses `REDFOG_BROKER_FAKE_PAM_SPAWN` (`SessionManager::spawn_fake_pam`) —
/// the same process-tree shape as the real PAM-spawn path, without needing
/// real root/PAM/setuid — so this runs under a plain `cargo test`, no sudo
/// needed, unlike `real_pam_spawn_login_after_log_out_recovers_from_a_resume_hang`.
#[cfg(feature = "compositor-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn log_out_actually_kills_the_real_compositor_process() {
    let Some(user_systemd_env) = ensure_user_systemd_for_fake_pam_spawn() else {
        return;
    };
    tokio::time::timeout(Duration::from_secs(60), async {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

        let mut broker_env = vec![("REDFOG_BROKER_FAKE_PAM_SPAWN".to_string(), "1".to_string())];
        broker_env.extend(user_systemd_env);
        let server = TestServer::spawn_with_broker_env(broker_env);
        let broker_pid = server._broker.child.id();

        let client_identity = ServerIdentity::generate().expect("generate client identity");
        let client_identifier = ClientIdentifier::from_pem(pem::parse(&client_identity.cert_pem).unwrap());
        let client_secret = ClientSecret::from_pem(pem::parse(&client_identity.private_key_pem).unwrap());

        let host = MoonlightHost::<TokioHyperClient>::new("127.0.0.1".to_string(), server.http_port, Some("it-client".to_string()))
            .expect("construct MoonlightHost");

        let pin = PairPin::new_random(&RustCryptoBackend).expect("generate pin");
        let pin_str = pin.to_string();
        let http_port = server.http_port;
        let submit_task = tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(300));
            ureq::post(&format!("http://127.0.0.1:{http_port}/submit-pin"))
                .send_form(&[("uniqueid", "it-client"), ("pin", &pin_str)])
                .expect("submit-pin request");
        });
        host.pair(&client_identifier, &client_secret, "connection-integration-test".to_string(), pin, RustCryptoBackend)
            .await
            .expect("pairing must succeed");
        submit_task.await.unwrap();

        let mut settings = default_stream_settings();
        let server_version = host.version().await.expect("server version");
        let gfe_version = host.gfe_version().await.expect("gfe version");
        let codec_support = host.server_codec_mode_support().await.expect("codec support");
        settings.adjust_for_server(server_version, &gfe_version, codec_support).expect("settings compatible");
        let crypto_backend = Arc::new(RustCryptoBackend);

        // ---- Log in, handing off to a real (fake-pam-spawned) User session. ----
        let stream_config = host
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("launch must succeed");
        let stream = MoonlightStream::connect(stream_config, settings, crypto_backend, video_capabilities())
            .await
            .expect("stream must connect");
        server.wait_for_stdout("TESTUX[login]: started", Duration::from_secs(45)).await;
        send_input_until_seen(
            &server.stdout_lines,
            &stream,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        send_key(&stream, VK_Q, true).await;
        send_key(&stream, VK_Q, false).await;
        server.wait_for_stdout("TESTUX[redfog-user-0]: started", Duration::from_secs(45)).await;

        // The actual, real `kwin_wayland` process — a grandchild of the
        // broker via `dbus-run-session`, exactly like production.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let kwin_pid = loop {
            if let Some(pid) = find_broker_grandchild_kwin_wayland_pid(broker_pid) {
                break pid;
            }
            assert!(tokio::time::Instant::now() < deadline, "never found a real kwin_wayland grandchild of broker pid {broker_pid}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert!(std::path::Path::new(&format!("/proc/{kwin_pid}")).exists(), "kwin_wayland pid {kwin_pid} isn't actually alive");

        // Real settle time — `handoff_to_user`'s own async work
        // (`spawn_user_compositor`'s broker round trip, then
        // `start_streaming`) is still in flight for a moment after
        // `TESTUX[redfog-user-0]: started` — logging out before
        // `shared.state` actually reaches `Streaming`/`Launched` for this
        // session would just (correctly) fail to find it.
        tokio::time::sleep(Duration::from_secs(1)).await;
        drop(stream);

        // ---- Log out — the actual thing under test: does this really
        // kill `kwin_wayland`, or just the `dbus-run-session` wrapper
        // above it (the bug)? ----
        let log_out_response = tokio::time::timeout(
            Duration::from_secs(15),
            send_login_request(&server.login_socket_path(), redfog_login_protocol::LoginRequest::LogOut { username: "user".to_string(), password: String::new() }),
        )
        .await
        .expect("send_login_request(LogOut) itself timed out after 15s");
        assert!(
            matches!(log_out_response, redfog_login_protocol::LoginResponse::LogOut(Ok(()))),
            "log-out must succeed: {log_out_response:?}"
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while std::path::Path::new(&format!("/proc/{kwin_pid}")).exists() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "kwin_wayland pid {kwin_pid} is still alive 10s after log-out — orphaned, exactly the live bug this test reproduces"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("log_out_actually_kills_the_real_compositor_process timed out");
}

/// Same scenario as `login_after_log_out_recovers_from_a_resume_hang`, but
/// forcing `REDFOG_BROKER_PAM_SPAWN=1` — the *real* production spawn path
/// `scripts/sudo-live-session.sh` actually uses (`redfog-session-init` ->
/// `dbus-run-session` -> real `kwin_wayland`, running as a real target
/// user), not the sudo-free `REDFOG_BROKER_FAKE_SPAWN` direct-child path
/// every other test in this file uses. Written because the user reported
/// still seeing the exact same live symptoms (slow login-screen recovery,
/// resume hangs, log-out-then-log-in hangs) *after* both the
/// `start_streaming`/`handoff_to_user` timeout fixes and the broker's
/// process-group/`terminate()` fix landed — meaning something in the real
/// PAM-spawn path specifically (not exercised by the FAKE_SPAWN-based test
/// above, which passes reliably) is still broken.
///
/// Needs real root + a real target user account to spawn as (PAM session
/// open, `setuid`, `initgroups` — none of that can be faked) — run via
/// `sudo -E cargo test -p redfog-moonlight --test connection_integration
/// real_pam_spawn_login_after_log_out_recovers_from_a_resume_hang`, same as
/// this file's other root-conditional test. Silently does nothing (not a
/// failure) under a plain, non-root `cargo test` run, so it doesn't break
/// the default sudo-free suite.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_pam_spawn_login_after_log_out_recovers_from_a_resume_hang() {
    // `$SUDO_USER`'s presence, not bare root status, is what actually means
    // "deliberately invoked via `sudo -E cargo test ...`" — confirmed live
    // that CI/Docker containers commonly run natively as root with no
    // `sudo` involved and no SUDO_USER set, which made this attempt (and
    // fail) the real PAM path instead of skipping cleanly.
    if std::env::var_os("SUDO_USER").is_none() {
        eprintln!(
            "skipping real_pam_spawn_login_after_log_out_recovers_from_a_resume_hang: needs SUDO_USER set — \
             run via `sudo -E cargo test -p redfog-moonlight --test connection_integration \
             real_pam_spawn_login_after_log_out_recovers_from_a_resume_hang`"
        );
        return;
    }

    tokio::time::timeout(Duration::from_secs(150), async {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

        let server = TestServer::spawn_with_broker_env(vec![("REDFOG_BROKER_PAM_SPAWN".to_string(), "1".to_string())]);

        let client_identity = ServerIdentity::generate().expect("generate client identity");
        let client_identifier = ClientIdentifier::from_pem(pem::parse(&client_identity.cert_pem).unwrap());
        let client_secret = ClientSecret::from_pem(pem::parse(&client_identity.private_key_pem).unwrap());

        let host = MoonlightHost::<TokioHyperClient>::new("127.0.0.1".to_string(), server.http_port, Some("it-client".to_string()))
            .expect("construct MoonlightHost");

        let pin = PairPin::new_random(&RustCryptoBackend).expect("generate pin");
        let pin_str = pin.to_string();
        let http_port = server.http_port;
        let submit_task = tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(300));
            ureq::post(&format!("http://127.0.0.1:{http_port}/submit-pin"))
                .send_form(&[("uniqueid", "it-client"), ("pin", &pin_str)])
                .expect("submit-pin request");
        });
        host.pair(&client_identifier, &client_secret, "connection-integration-test".to_string(), pin, RustCryptoBackend)
            .await
            .expect("pairing must succeed");
        submit_task.await.unwrap();

        let mut settings = default_stream_settings();
        let server_version = host.version().await.expect("server version");
        let gfe_version = host.gfe_version().await.expect("gfe version");
        let codec_support = host.server_codec_mode_support().await.expect("codec support");
        settings.adjust_for_server(server_version, &gfe_version, codec_support).expect("settings compatible");
        let crypto_backend = Arc::new(RustCryptoBackend);

        // ---- First connection: Login, then handoff to a fresh User session
        // (a real, PAM-spawned `kwin_wayland` running as $SUDO_USER). ----
        let stream_config = host
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("first launch must succeed");
        let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
            .await
            .expect("first stream must connect");
        server.wait_for_stdout("TESTUX[login]: started", Duration::from_secs(45)).await;
        send_input_until_seen(
            &server.stdout_lines,
            &stream,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        send_key(&stream, VK_Q, true).await;
        send_key(&stream, VK_Q, false).await;
        server.wait_for_stdout("TESTUX[redfog-user-0]: started", Duration::from_secs(45)).await;

        // ---- Disconnect, reconnect: backgrounds the User session, shows a
        // fresh Login. ----
        drop(stream);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let login_started_before_resume = server.count_stdout("TESTUX[login]: started");
        let stream_config = host
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("second launch must succeed");
        let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
            .await
            .expect("second stream must connect");
        server
            .wait_for_new_stdout("TESTUX[login]: started", login_started_before_resume, Duration::from_secs(10))
            .await;

        // ---- Log in again as the same placeholder user — triggers the
        // resume path (and, on real hardware, the known KWin/PipeWire
        // resume hang this whole scenario depends on wedging). ----
        let resumed_before = server.count_stdout("resuming existing session for user");
        send_input_until_seen(
            &server.stdout_lines,
            &stream,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        send_key(&stream, VK_Q, true).await;
        send_key(&stream, VK_Q, false).await;
        server.wait_for_new_stdout("resuming existing session for user", resumed_before, Duration::from_secs(10)).await;
        // Real settle time, matching the live timing this bug was found
        // with — deliberately not asserting anything about video here (the
        // resume-video hang itself is a known, separate, still-open
        // problem).
        tokio::time::sleep(Duration::from_secs(5)).await;
        drop(stream);

        // ---- The actual question: log out, then log in again — does the
        // handoff for that *second* login ever complete, against the real
        // PAM-spawn path? ----
        let log_out_response = send_login_request(
            &server.login_socket_path(),
            redfog_login_protocol::LoginRequest::LogOut { username: "user".to_string(), password: String::new() },
        )
        .await;
        assert!(
            matches!(log_out_response, redfog_login_protocol::LoginResponse::LogOut(Ok(()))),
            "log-out must succeed: {log_out_response:?}"
        );

        let login_started_before_relogin = server.count_stdout("TESTUX[login]: started");
        let user_started_before_relogin = server.count_stdout("TESTUX[redfog-user-0]: started");
        let stream_config = host
            .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
            .await
            .expect("third launch (after log-out) must succeed");
        let stream = MoonlightStream::connect(stream_config, settings, crypto_backend, video_capabilities())
            .await
            .expect("third stream (after log-out) must connect");
        server
            .wait_for_new_stdout("TESTUX[login]: started", login_started_before_relogin, Duration::from_secs(10))
            .await;
        let pointer_moved_before = server.count_stdout("TESTUX[login]: pointer_moved");
        send_input_until_new_seen(
            &server.stdout_lines,
            &stream,
            ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
            "TESTUX[login]: pointer_moved",
            pointer_moved_before,
            Duration::from_secs(10),
        )
        .await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;
        send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let key_pressed_before = server.count_stdout("TESTUX[login]: key_pressed");
        send_key(&stream, VK_Q, true).await;
        server.wait_for_new_stdout("TESTUX[login]: key_pressed", key_pressed_before, Duration::from_secs(10)).await;
        send_key(&stream, VK_Q, false).await;

        // The actual assertion: a genuinely fresh User-stage spawn completes
        // within a generous but bounded window, rather than hanging forever
        // with no error at all (the live symptom).
        server
            .wait_for_new_stdout("TESTUX[redfog-user-0]: started", user_started_before_relogin, Duration::from_secs(30))
            .await;
    })
    .await
    .expect(
        "real_pam_spawn_login_after_log_out_recovers_from_a_resume_hang timed out — the real PAM-spawn path still hangs after a log-out during a PipeWire wedge",
    );
}

/// Proves `redfog-pair` actually works against the real protocol: runs a
/// genuine pairing handshake via `moonlight-common-rust` (cert generation,
/// salt, challenge/response — everything `real_client_connects_reconnects_
/// and_sends_input` does), but relays the PIN via the `redfog-pair` binary
/// itself instead of a raw HTTP call, exercising both its `/pending-pairs`
/// auto-pick path (no `--uniqueid` given) and its `/submit-pin` relay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redfog_pair_binary_completes_a_real_pairing_handshake() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

    let server = TestServer::spawn();

    let client_identity = ServerIdentity::generate().expect("generate client identity");
    let client_identifier = ClientIdentifier::from_pem(pem::parse(&client_identity.cert_pem).unwrap());
    let client_secret = ClientSecret::from_pem(pem::parse(&client_identity.private_key_pem).unwrap());

    let host = MoonlightHost::<TokioHyperClient>::new("127.0.0.1".to_string(), server.http_port, Some("redfog-pair-test-client".to_string()))
        .expect("construct MoonlightHost");

    let pin = PairPin::new_random(&RustCryptoBackend).expect("generate pin");
    let pin_str = pin.to_string();
    let http_port = server.http_port;
    let pair_task = tokio::task::spawn_blocking(move || {
        std::thread::sleep(Duration::from_millis(300));
        // No `--uniqueid` — proves the `/pending-pairs` auto-pick path
        // works (only this one client is mid-handshake).
        let output = std::process::Command::new(workspace_binary("redfog-pair"))
            .args(["--port", &http_port.to_string(), &pin_str])
            .output()
            .expect("run redfog-pair");
        assert!(
            output.status.success(),
            "redfog-pair exited with {}: stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    });
    host.pair(&client_identifier, &client_secret, "redfog-pair-test".to_string(), pin, RustCryptoBackend)
        .await
        .expect("pairing must succeed");
    pair_task.await.unwrap();
}

/// Kills the `kwin_wayland` process the broker spawned directly under
/// `REDFOG_BROKER_FAKE_SPAWN` (see `SessionManager::spawn_fake`), scoped
/// precisely to a direct child of `broker_pid` — this test's own broker
/// instance, and no other process. Deliberately never a bare `pkill
/// kwin_wayland`/`pkill -f kwin_wayland`: on a dev machine running this
/// test, that would just as happily kill a real, unrelated desktop session
/// sharing the box.
///
/// Matches via `/proc/<pid>/status`'s `PPid:` field, not `/proc/<pid>/
/// environ` (which would otherwise be the more direct way to scope this,
/// e.g. by `XDG_RUNTIME_DIR`) — confirmed live, reading a running
/// `kwin_wayland`'s `environ` fails even from this test's own uid, most
/// likely because KWin hardens itself against `ptrace`-based input/screen
/// snooping (`environ`/`mem` require `ptrace` access; `comm`/`status` don't
/// and stay readable regardless).
#[cfg(feature = "compositor-tests")]
fn kill_broker_spawned_user_compositor(broker_pid: u32) {
    let mut killed_any = false;
    for entry in std::fs::read_dir("/proc").expect("read /proc").flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else { continue };
        let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else { continue };
        if comm.trim() != "kwin_wayland" {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else { continue };
        let ppid = status.lines().find_map(|l| l.strip_prefix("PPid:")).and_then(|v| v.trim().parse::<u32>().ok());
        if ppid == Some(broker_pid) {
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL)
                .unwrap_or_else(|e| panic!("failed to kill kwin_wayland pid {pid}: {e}"));
            killed_any = true;
        }
    }
    assert!(killed_any, "no kwin_wayland process found as a direct child of broker pid {broker_pid} — can't simulate a logout");
}

/// Standalone (no broker at all — Login never uses one regardless of
/// backend, and without `broker_socket_path` set the User stage falls back
/// to the same direct-spawn path) smoke test for `Backend::GstWaylandDisplay`
/// — proves `spawn_gst_compositor`/`spawn_gst_payload_in_background`'s
/// direct-spawn branch, `VideoSource::Element`, `GstInputSink`, and the
/// Login->User handoff all work through the real client/RTSP/video/input
/// path, the same way `real_client_connects_reconnects_and_sends_input`
/// proves it for Kwin. Deliberately lighter than that test (no reconnect
/// coverage) — the broker-spawned (`SpawnPayload`) User-stage path needs
/// real root (setfacl/PAM privilege drop, no fake-spawn equivalent exists
/// for it yet) and is covered separately, manually, via
/// `redfog-broker/examples/spawn_payload_test.rs`.
struct GstTestServer {
    _process: ServerProcess,
    http_port: u16,
    stdout_lines: Arc<Mutex<Vec<String>>>,
}

impl GstTestServer {
    fn spawn() -> Self {
        let plugin_dir = std::env::var("REDFOG_GST_WAYLAND_DISPLAY_PLUGIN_DIR").expect(
            "REDFOG_GST_WAYLAND_DISPLAY_PLUGIN_DIR must be set to gst-wayland-display's built \
             gstreamer-1.0 plugin dir to run this test — see scripts/run-gst-viewer.sh",
        );

        let runtime_dir = std::env::temp_dir().join(format!("redfog-it-gst-runtime-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&runtime_dir).unwrap();

        let http_port = pick_free_port();
        let https_port = pick_free_port();
        let rtsp_port = pick_free_port();
        let video_port = pick_free_port();
        let control_port = pick_free_port();
        let audio_port = pick_free_port();

        let test_ux = workspace_binary("redfog-test-ux");
        let test_ux = test_ux.to_str().unwrap();

        let stdout_lines = Arc::new(Mutex::new(Vec::<String>::new()));

        let mut cmd = Command::new(workspace_binary("redfog-server"));
        cmd.env("REDFOG_RUNTIME_DIR", &runtime_dir)
            .env("REDFOG_BACKEND", "gst-wayland-display")
            .env("REDFOG_GST_WAYLAND_DISPLAY_PLUGIN_DIR", &plugin_dir)
            .env("REDFOG_GST_RENDER_NODE", "software")
            .env("REDFOG_HTTP_PORT", http_port.to_string())
            .env("REDFOG_HTTPS_PORT", https_port.to_string())
            .env("REDFOG_RTSP_PORT", rtsp_port.to_string())
            .env("REDFOG_VIDEO_PORT", video_port.to_string())
            .env("REDFOG_CONTROL_PORT", control_port.to_string())
            .env("REDFOG_AUDIO_PORT", audio_port.to_string())
            .env("REDFOG_LOGIN_APP", test_ux)
            .env("REDFOG_USER_APP", test_ux)
            .env("RUST_LOG", "redfog_moonlight=debug,redfog_server=debug,gst_backend=debug")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        die_with_parent(&mut cmd);

        let mut process = ServerProcess { child: cmd.spawn().expect("spawn redfog-server"), runtime_dir };
        // See the identical call in TestServer::spawn_with_broker_env.
        redfog_test_cleanup::track_extra_pgid(process.child.id() as i32);

        {
            let stdout = process.child.stdout.take().unwrap();
            let stdout_lines = stdout_lines.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    println!("[redfog-server] {line}");
                    stdout_lines.lock().unwrap().push(line);
                }
            });
        }
        {
            let stderr = process.child.stderr.take().unwrap();
            let stdout_lines = stdout_lines.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    eprintln!("[redfog-server] {line}");
                    stdout_lines.lock().unwrap().push(line);
                }
            });
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if std::net::TcpStream::connect(("127.0.0.1", http_port)).is_ok() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "redfog-server never came up on port {http_port}");
            std::thread::sleep(Duration::from_millis(100));
        }

        GstTestServer { _process: process, http_port, stdout_lines }
    }

    /// The Login stage's fixed "TESTUX[login]: ..." label (see this test's
    /// own doc comment on `wait_for_stdout` below) never collides with the
    /// User stage's own backend-specific label, so — unlike the Kwin
    /// test's `real_client_connects_reconnects_and_sends_input`, which
    /// genuinely does reconnect to the same session and needs count
    /// baselines — a plain "does it appear" check is always enough here.
    async fn wait_for_stdout(&self, needle: &str, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        while !self.stdout_lines.lock().unwrap().iter().any(|line| line.contains(needle)) {
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {needle:?} in redfog-server's output");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gst_wayland_display_backend_smoke_test() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

    let server = GstTestServer::spawn();

    let client_identity = ServerIdentity::generate().expect("generate client identity");
    let client_identifier = ClientIdentifier::from_pem(pem::parse(&client_identity.cert_pem).unwrap());
    let client_secret = ClientSecret::from_pem(pem::parse(&client_identity.private_key_pem).unwrap());

    let host = MoonlightHost::<TokioHyperClient>::new("127.0.0.1".to_string(), server.http_port, Some("it-client-gst".to_string()))
        .expect("construct MoonlightHost");

    let pin = PairPin::new_random(&RustCryptoBackend).expect("generate pin");
    let pin_str = pin.to_string();
    let http_port = server.http_port;
    let submit_task = tokio::task::spawn_blocking(move || {
        std::thread::sleep(Duration::from_millis(300));
        ureq::post(&format!("http://127.0.0.1:{http_port}/submit-pin"))
            .send_form(&[("uniqueid", "it-client-gst"), ("pin", &pin_str)])
            .expect("submit-pin request");
    });
    host.pair(&client_identifier, &client_secret, "gst-backend-smoke-test".to_string(), pin, RustCryptoBackend)
        .await
        .expect("pairing must succeed");
    submit_task.await.unwrap();

    let mut settings = default_stream_settings();
    let server_version = host.version().await.expect("server version");
    let gfe_version = host.gfe_version().await.expect("gfe version");
    let codec_support = host.server_codec_mode_support().await.expect("codec support");
    settings.adjust_for_server(server_version, &gfe_version, codec_support).expect("settings compatible");

    // ---- Login stage. ----
    let stream_config = host
        .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
        .await
        .expect("launch must succeed");
    let crypto_backend = Arc::new(RustCryptoBackend);
    let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
        .await
        .expect("stream must connect");

    // The Login stage is always headless now (see session_backend::
    // spawn_login_compositor), regardless of backend — redfog-test-ux logs
    // "TESTUX[login]: ..." for it unconditionally, distinct from the User
    // stage's backend-specific label ("wayland-1" here, for
    // Backend::GstWaylandDisplay), so unlike before, none of these checks
    // need count-baseline tracking to disambiguate "which stage logged
    // this" — each label only ever comes from one stage in this test.
    server.wait_for_stdout("TESTUX[login]: started", Duration::from_secs(45)).await;

    send_input_until_seen(
        &server.stdout_lines,
        &stream,
        ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
        "TESTUX[login]: pointer_moved",
        Duration::from_secs(10),
    )
    .await;

    let (login_frames, _) = poll_frames_tracking_gaps(&stream, tokio::time::sleep(Duration::from_secs(2))).await;
    assert!(login_frames > 0, "expected video frames from the Login-stage test UX (gst-wayland-display backend)");

    // ---- Trigger Login->User handoff (redfog-test-ux exits on 'Q'). ----
    send_key(&stream, VK_Q, true).await;
    send_key(&stream, VK_Q, false).await;
    let (handoff_frames, max_gap) = poll_frames_tracking_gaps(&stream, async {
        server.wait_for_stdout("TESTUX[wayland-1]: started", Duration::from_secs(45)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    })
    .await;
    assert!(handoff_frames > 0, "expected video frames to keep flowing across the Login->User handoff (gst-wayland-display backend)");
    assert!(max_gap < Duration::from_secs(3), "video stalled for {max_gap:?} across the handoff (gst-wayland-display backend)");

    send_input_until_seen(
        &server.stdout_lines,
        &stream,
        ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },
        "TESTUX[wayland-1]: pointer_moved",
        Duration::from_secs(10),
    )
    .await;
}

/// Diagnostic for a report of two independent real clients (moonlight-qt,
/// moonlight-web-stream) both falling back to H264 even with HEVC forced on
/// — this is the same reference client library moonlight-web-stream itself
/// is built on (`moonlight-common-rust`), so if this test also negotiates
/// H264 instead of HEVC, that's strong evidence of a real server-side bug
/// rather than something specific to either real client. Only checks the
/// RTSP-level negotiation (does the real client's own `negotiated_video_
/// format` — computed purely from parsing our `/serverinfo`'s
/// `ServerCodecModeSupport`, per `moonlight-common-rust`'s `proto/mod.rs` —
/// come out as HEVC), not the actual NVENC encode path (no GPU in this test
/// environment).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hevc_client_negotiates_hevc_and_login_stage_honors_it() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

    let server = TestServer::spawn();

    let client_identity = ServerIdentity::generate().expect("generate client identity");
    let client_identifier = ClientIdentifier::from_pem(pem::parse(&client_identity.cert_pem).unwrap());
    let client_secret = ClientSecret::from_pem(pem::parse(&client_identity.private_key_pem).unwrap());

    let host = MoonlightHost::<TokioHyperClient>::new("127.0.0.1".to_string(), server.http_port, Some("it-client-hevc".to_string()))
        .expect("construct MoonlightHost");

    let pin = PairPin::new_random(&RustCryptoBackend).expect("generate pin");
    let pin_str = pin.to_string();
    let http_port = server.http_port;
    let submit_task = tokio::task::spawn_blocking(move || {
        std::thread::sleep(Duration::from_millis(300));
        ureq::post(&format!("http://127.0.0.1:{http_port}/submit-pin"))
            .send_form(&[("uniqueid", "it-client-hevc"), ("pin", &pin_str)])
            .expect("submit-pin request");
    });
    host.pair(&client_identifier, &client_secret, "hevc-negotiation-test".to_string(), pin, RustCryptoBackend)
        .await
        .expect("pairing must succeed");
    submit_task.await.unwrap();

    let mut settings = default_stream_settings();
    // The only difference from `default_stream_settings()`'s H264-only
    // client — this is what a real client with "Force HEVC" enabled and a
    // working hardware decoder reports as its own capability.
    settings.supported_video_formats = VideoFormats::H264 | VideoFormats::H265 | VideoFormats::H265_MAIN10;
    let server_version = host.version().await.expect("server version");
    let gfe_version = host.gfe_version().await.expect("gfe version");
    let codec_support = host.server_codec_mode_support().await.expect("codec support");
    println!("server_codec_mode_support = {codec_support:?}");
    settings.adjust_for_server(server_version, &gfe_version, codec_support).expect("settings compatible");

    let stream_config = host
        .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
        .await
        .expect("launch must succeed");
    let crypto_backend = Arc::new(RustCryptoBackend);
    // Held (not used again) until this function returns — dropping it would
    // tear down the RTSP session before the assertions below get a chance
    // to observe it.
    let _stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
        .await
        .expect("stream must connect");

    server.wait_for_stdout("RTSP ANNOUNCE: client requested", Duration::from_secs(20)).await;
    assert!(
        server.stdout_contains("codec Hevc"),
        "client offered HEVC ({codec_support:?}) but server negotiated something else — full log:\n{}",
        server.stdout_lines.lock().unwrap().join("\n")
    );

    // Login's own video pipeline is deliberately deferred until here (see
    // `SessionManager::start_streaming`'s doc comment) precisely so it can
    // also honor whatever ANNOUNCE just negotiated (via `x265enc`, Login is
    // always `VideoEncoder::Software`), rather than always sending H.264
    // regardless of what the client's decoder committed to at ANNOUNCE time.
    server.wait_for_stdout("TESTUX[login]: started", Duration::from_secs(45)).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline && !server.stdout_contains("encoder=Software") {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        server.stdout_contains("codec=Hevc"),
        "Login's own pipeline didn't use HEVC even though the client negotiated it — full log:\n{}",
        server.stdout_lines.lock().unwrap().join("\n")
    );
}

/// Regression test for a real, now-fixed bug found while fixing the
/// Login-stage codec mismatch above: the post-handoff `CudaDirectEncoderSession`
/// (direct CUDA -> NVENC, no GStreamer — see `nvenc_session.rs`) successfully
/// registered and encoded the very first (IDR) HEVC frame, then failed
/// `encode_picture` on the very next (P-)frame with `EncodeError { kind:
/// Generic, string: None }`. Root-caused (via an isolated, since-deleted fast
/// repro) to manually setting `pictureType = P` for HEVC specifically — H.264
/// tolerates manual picture typing fine (`cuda_direct_nvenc.rs`/
/// `nvenc_reconfigure_reuses_capture.rs` both still pass unmodified), but
/// HEVC on this GPU/driver doesn't. Fixed by leaving NVENC's own picture-type
/// decision (`enablePTD`) on for HEVC only — see `nvenc_session.rs`'s module
/// doc comment for the real cost (`request_keyframe` has no effect for HEVC).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hevc_post_handoff_nvenc_direct_encode_stays_up() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();

    let server = TestServer::spawn();

    let client_identity = ServerIdentity::generate().expect("generate client identity");
    let client_identifier = ClientIdentifier::from_pem(pem::parse(&client_identity.cert_pem).unwrap());
    let client_secret = ClientSecret::from_pem(pem::parse(&client_identity.private_key_pem).unwrap());

    let host = MoonlightHost::<TokioHyperClient>::new("127.0.0.1".to_string(), server.http_port, Some("it-client-hevc2".to_string()))
        .expect("construct MoonlightHost");

    let pin = PairPin::new_random(&RustCryptoBackend).expect("generate pin");
    let pin_str = pin.to_string();
    let http_port = server.http_port;
    let submit_task = tokio::task::spawn_blocking(move || {
        std::thread::sleep(Duration::from_millis(300));
        ureq::post(&format!("http://127.0.0.1:{http_port}/submit-pin"))
            .send_form(&[("uniqueid", "it-client-hevc2"), ("pin", &pin_str)])
            .expect("submit-pin request");
    });
    host.pair(&client_identifier, &client_secret, "hevc-posthandoff-test".to_string(), pin, RustCryptoBackend)
        .await
        .expect("pairing must succeed");
    submit_task.await.unwrap();

    let mut settings = default_stream_settings();
    settings.supported_video_formats = VideoFormats::H264 | VideoFormats::H265 | VideoFormats::H265_MAIN10;
    let server_version = host.version().await.expect("server version");
    let gfe_version = host.gfe_version().await.expect("gfe version");
    let codec_support = host.server_codec_mode_support().await.expect("codec support");
    settings.adjust_for_server(server_version, &gfe_version, codec_support).expect("settings compatible");

    let stream_config = host
        .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
        .await
        .expect("launch must succeed");
    let crypto_backend = Arc::new(RustCryptoBackend);
    let stream = MoonlightStream::connect(stream_config, settings.clone(), crypto_backend.clone(), video_capabilities())
        .await
        .expect("stream must connect");

    server.wait_for_stdout("TESTUX[login]: started", Duration::from_secs(45)).await;
    send_key(&stream, VK_Q, true).await;
    send_key(&stream, VK_Q, false).await;
    server.wait_for_stdout("TESTUX[redfog-user-0]: started", Duration::from_secs(45)).await;

    // Checks the *combined* substring, not "codec=Hevc" alone — the Login
    // stage's own stats line already contains that on its own, and would
    // make this a false-positive pass regardless of what the post-handoff
    // NvencDirect pipeline actually does. "encoder=NvencDirect" never
    // appears in a Login-stage line (Login is always `Software`), so this
    // combined form can only ever come from a genuine post-handoff line.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline && !server.stdout_contains("encoder=NvencDirect, codec=Hevc") {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        server.stdout_contains("encoder=NvencDirect, codec=Hevc"),
        "post-handoff NvencDirect pipeline didn't use HEVC even though the client negotiated it — full log:\n{}",
        server.stdout_lines.lock().unwrap().join("\n")
    );
}
