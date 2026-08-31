//! Spawns a target user's compositor session via templated systemd
//! `.socket`/`.service` units — see design.md's "Cross-user socket
//! reachability" section for why: KWin must run as the target user, but the
//! Wayland socket's permissions need to be controlled independently of
//! that, so systemd binds it (via `SocketUser=`) and hands KWin the already-
//! listening fd (`--wayland-fd`), rather than KWin calling `bind()` itself.
//!
//! Writing unit files into `/run/systemd/system/` and reloading/starting
//! them needs the `org.freedesktop.systemd1.manage-unit-files` and
//! `org.freedesktop.systemd1.manage-units` polkit actions respectively —
//! see design.md for how those get scoped to the broker's own service user
//! without granting root.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use tokio::process::Child;

const UNIT_DIR: &str = "/run/systemd/system";

/// Registers `PR_SET_PDEATHSIG(SIGKILL)` on the about-to-exec child so the
/// kernel kills it the moment this broker process dies for any reason (not
/// just a clean shutdown that gets to call `terminate()`) — closes the same
/// class of leaked-`kwin_wayland`-after-the-broker-died gap that integration
/// tests hit when the test binary itself gets killed externally. Only
/// covers a direct child of *this* process; `spawn_fake`'s `kwin_wayland` is
/// exactly that, but the `Scoped` variants' `dbus-run-session` -> forked
/// `kwin_wayland` hop isn't (`dbus-run-session` doesn't propagate this to
/// its own children) — that class is already handled by `terminate()`'s
/// scope-kill instead.
fn die_with_parent(cmd: &mut tokio::process::Command) {
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
            // fork() and this prctl() call — we'd already be reparented
            // and would never receive the signal for a parent that's
            // already gone.
            if libc::getppid() != parent_before {
                libc::raise(libc::SIGKILL);
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod die_with_parent_tests {
    use super::die_with_parent;
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    /// A dead-but-unreaped zombie is still "gone" for our purposes here — see
    /// `child_dies_when_its_direct_parent_dies`'s comment on why bare
    /// existence/`comm` checks aren't enough.
    fn process_is_alive(pid: i32) -> bool {
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            return false;
        };
        !status.lines().any(|l| l.trim_start().starts_with("State:") && l.contains('Z'))
    }

    /// Not a real test on its own — re-exec'd by
    /// `child_dies_when_its_direct_parent_dies` as a throwaway "middle"
    /// process (a real subprocess, not just a function call in the same
    /// process, since `die_with_parent`'s whole point only shows up across
    /// an actual parent/child boundary). Spawns a grandchild with
    /// `die_with_parent`, prints its pid, then sleeps until killed — the
    /// outer test SIGKILLs *this* process and checks whether the
    /// grandchild died too.
    #[tokio::test]
    async fn pdeathsig_helper_middle_process() {
        if std::env::var_os("REDFOG_PDEATHSIG_TEST_HELPER").is_none() {
            return;
        }
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("100").stdout(Stdio::null()).stderr(Stdio::null());
        die_with_parent(&mut cmd);
        let child = cmd.spawn().expect("spawn grandchild");
        println!("GRANDCHILD_PID={}", child.id().expect("child has a pid right after spawn"));
        use std::io::Write;
        std::io::stdout().flush().unwrap();
        tokio::time::sleep(Duration::from_secs(100)).await;
    }

    #[test]
    fn child_dies_when_its_direct_parent_dies() {
        let exe = std::env::current_exe().expect("current_exe");
        let mut middle = std::process::Command::new(&exe)
            .args(["--exact", "session::die_with_parent_tests::pdeathsig_helper_middle_process", "--nocapture"])
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

                // SIGKILL, not a graceful stop — simulates the exact
                // scenario die_with_parent exists for: the parent dying
                // with no chance to run any cleanup of its own.
                unsafe { libc::kill(middle.id() as i32, libc::SIGKILL) };
                middle.wait().expect("wait for middle process");

                // Neither bare `/proc/{pid}` existence nor `/proc/{pid}/comm`
                // is enough to prove the grandchild is genuinely still
                // running — confirmed live (via `act`'s own container, a bare
                // `tail -f /dev/null` as PID 1): PDEATHSIG does fire
                // correctly, the grandchild does die, but once reparented to
                // a PID 1 that never calls wait() on orphans it sits forever
                // as a zombie — still present in /proc, `comm` still reading
                // "sleep", despite already being dead. A zombie's `State:` in
                // /proc/{pid}/status is `Z`; treat that the same as gone.
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    if !process_is_alive(grandchild_pid) {
                        return;
                    }
                    assert!(Instant::now() < deadline, "grandchild pid {grandchild_pid} is still alive 30s after its parent was SIGKILLed");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}

enum ActiveSession {
    /// Persistent, templated `.socket`/`.service` unit files — see
    /// `spawn_via_systemd`.
    Systemd { unit_name: String },
    /// `REDFOG_BROKER_FAKE_SPAWN` mode — see `spawn()`. Deliberately a bare
    /// tracked child, no scope: this path never nests a wrapper process
    /// (unlike `Scoped`'s spawn sites), so there's nothing a plain
    /// `child.kill()` could fail to reach.
    DirectChild { child: Child },
    /// A session wrapped in its own transient systemd scope
    /// (`systemd-run --scope --collect --unit=<unit_name>`) — used by
    /// `spawn_payload`, whose caller-provided `argv` could itself be (or
    /// spawn) a process that forks its own real children rather than
    /// exec-chaining — a plain `terminate()` that only kills the top-level
    /// tracked PID would leave any such forked descendant orphaned and
    /// running forever. Confirmed live once already (a different spawn
    /// path, since removed): after a log-out, `kwin_wayland` was still
    /// alive with `PPid=1`, still holding its PipeWire/DRM connection,
    /// degrading every session spawned after it.
    ///
    /// A bare process group (an earlier fix attempt, `.process_group(0)` +
    /// a negative-PID `kill()`) is not quite enough on its own: any
    /// descendant that calls `setsid()`/`setpgid()` itself (not unusual for
    /// a display or session-managing process wanting its own job-control
    /// session) escapes it silently. A cgroup can't be escaped without
    /// explicit privileged action, so `terminate()` kills the *scope*
    /// (`systemctl kill --kill-who=all <unit_name>.scope`), not just a
    /// process group. `child` tracks the `systemd-run` invocation itself
    /// (which execs directly into the target command via the *same* PID —
    /// registering the scope for its own already-running PID before
    /// exec'ing, not an extra fork) purely so it can be `wait()`ed/reaped
    /// once the scope's been killed. Always registered against the
    /// *system* manager (`systemd-run`, no `--user`) — `terminate()`'s own
    /// `logind_session_id_for` check additionally handles the case where
    /// the tracked process ends up migrated into a *different* cgroup than
    /// the one it was registered in (e.g. a real PAM session opening and
    /// creating its own `session-N.scope` — confirmed live this happens
    /// and silently garbage-collects the original scope out from under a
    /// naive `systemctl kill` on `unit_name` alone).
    Scoped { child: Child, unit_name: String },
}

pub struct SessionManager {
    active: Mutex<HashMap<String, ActiveSession>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self { active: Mutex::new(HashMap::new()) }
    }

    pub async fn spawn(
        &self,
        session_id: &str,
        username: &str,
        width: u32,
        height: u32,
        socket_name: &str,
        payload: &[String],
    ) -> Result<String, String> {
        if std::env::var_os("REDFOG_BROKER_FAKE_SPAWN").is_some() {
            return self.spawn_fake(session_id, width, height, socket_name, payload).await;
        }

        // For integration testing: spawn as whatever user is actually
        // running the test (which must exist and be able to run a real
        // desktop session) instead of the requested username, so this
        // exercises the real systemd-run/socket-activation/capture/input
        // path without needing a second, separately-provisioned account.
        // Never set this in production — it defeats per-user targeting.
        let username = match std::env::var("REDFOG_BROKER_FORCE_SPAWN_USER") {
            Ok(forced) => {
                tracing::warn!("REDFOG_BROKER_FORCE_SPAWN_USER set — spawning as {forced} instead of requested {username}");
                forced
            }
            Err(_) => username.to_string(),
        };
        self.spawn_via_systemd(session_id, &username, width, height, socket_name, payload).await
    }

    /// Bypasses systemd entirely: spawns `kwin_wayland` directly as the
    /// broker's own user (same mechanism `CompositorSession::spawn` already
    /// uses), rather than generating/loading systemd units and calling
    /// `systemd-run --uid=`. For integration testing everything *except*
    /// the parts that genuinely need root (unit placement, cross-user
    /// spawning) — those are exercised by the systemd path instead, which
    /// needs `sudo`. Never set this in production; it defeats both
    /// cross-user spawning and the Wayland-socket permission isolation the
    /// systemd path provides.
    async fn spawn_fake(&self, session_id: &str, width: u32, height: u32, socket_name: &str, payload: &[String]) -> Result<String, String> {
        tracing::warn!("REDFOG_BROKER_FAKE_SPAWN set — spawning kwin_wayland directly, no systemd/cross-user involved");

        let runtime_dir = format!("{}/session-{session_id}", default_runtime_dir());
        let wayland_socket_path = format!("{runtime_dir}/{socket_name}");
        std::fs::create_dir_all(&runtime_dir).map_err(|e| format!("failed to create {runtime_dir}: {e}"))?;
        let _ = std::fs::remove_file(&wayland_socket_path);

        let kwin_path = which_kwin_wayland().unwrap_or_else(|| "kwin_wayland".to_string());
        let pipewire_socket_path = format!("{}/pipewire-0", default_runtime_dir());
        // Like `pipewire_socket_path` above: PipeWire/wireplumber/pipewire-
        // pulse all run under `redfog-server`'s own identity, in *its*
        // runtime dir (`HeadlessRuntime::start`), not this session's private
        // `runtime_dir` — pointing PULSE_SERVER at the session dir instead
        // (an earlier bug here) meant it never had a pulse server listening
        // on it at all.
        let pulse_socket_path = pulse_socket_path();

        let mut cmd = tokio::process::Command::new(&kwin_path);
        cmd.env("KWIN_PLATFORM", "virtual")
            .env("KWIN_WAYLAND_NO_PERMISSION_CHECKS", "1")
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("PIPEWIRE_REMOTE", &pipewire_socket_path)
            .env("PULSE_SERVER", format!("unix:{pulse_socket_path}"))
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .arg("--virtual")
            .arg("--width")
            .arg(width.to_string())
            .arg("--height")
            .arg(height.to_string())
            .arg("--scale")
            .arg("1")
            .arg("--no-lockscreen")
            .arg("--socket")
            .arg(socket_name)
            .arg("--xwayland");
        if !payload.is_empty() {
            cmd.arg("--exit-with-session");
            cmd.arg(&payload[0]);
            if payload.len() > 1 {
                cmd.arg("--");
                for arg in &payload[1..] {
                    cmd.arg(arg);
                }
            }
        }
        // Deliberately *not* its own process group, unlike
        // `spawn_payload`: this is the test-only,
        // sudo-free path, and the integration test's own `BrokerProcess`
        // Drop impl kills its whole tree (broker -> this direct child)
        // by the *broker's* process group — giving this child its own
        // group would silently escape that sweep (confirmed: doing this
        // once left real, orphaned `kwin_wayland`/`redfog-test-ux` pairs
        // running after a test run finished). `terminate()`'s own
        // `DirectChild` case still works fine with a plain single-PID
        // `child.kill()` regardless, since this path has no
        // `dbus-run-session`-style wrapper forking a separate child in
        // the first place.
        cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        die_with_parent(&mut cmd);
        let child = cmd.spawn().map_err(|e| format!("failed to spawn {kwin_path}: {e}"))?;

        let socket_path_buf = PathBuf::from(&wayland_socket_path);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !socket_path_buf.exists() {
            if std::time::Instant::now() > deadline {
                return Err(format!("KWin Wayland socket {wayland_socket_path} failed to appear"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }

        self.active.lock().unwrap().insert(session_id.to_string(), ActiveSession::DirectChild { child });
        Ok(wayland_socket_path)
    }

    async fn spawn_via_systemd(
        &self,
        session_id: &str,
        username: &str,
        width: u32,
        height: u32,
        socket_name: &str,
        payload: &[String],
    ) -> Result<String, String> {
        let unit_name = format!("redfog-session-{session_id}");
        let runtime_dir = format!("{}/session-{session_id}", default_runtime_dir());
        let wayland_socket_path = format!("{runtime_dir}/{socket_name}");

        std::fs::create_dir_all(&runtime_dir).map_err(|e| format!("failed to create {runtime_dir}: {e}"))?;
        // This directory is used as the target user's own XDG_RUNTIME_DIR
        // for the session -- but the broker (root) just created it, so it
        // starts out root-owned, mode 0755. Read/traverse alone isn't
        // enough: KWin/Xwayland (running as `username`) also need to
        // *create* files in it directly (e.g. Xwayland's own EIS lockfile)
        // -- confirmed live: without this, that lockfile creation failed
        // with EACCES, which libei reported as the misleading "is another
        // EIS running?", which made Xwayland fail to start entirely, which
        // in turn hung any client whose clipboard support falls back to
        // connecting to X11 (e.g. egui/arboard) waiting forever for a
        // display that was never going to appear.
        match tokio::process::Command::new("chown").args([username, &runtime_dir]).output().await {
            Ok(output) if output.status.success() => {
                tracing::info!("chowned {runtime_dir} to {username}");
            }
            Ok(output) => {
                return Err(format!(
                    "chown {runtime_dir} to {username} exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            Err(e) => return Err(format!("failed to run chown on {runtime_dir}: {e}")),
        }

        let broker_user = current_username().map_err(|e| format!("failed to determine broker's own username: {e}"))?;

        let socket_unit = format!(
            "[Socket]\nListenStream={wayland_socket_path}\nSocketUser={broker_user}\nSocketMode=0660\n"
        );
        let kwin_path = which_kwin_wayland().unwrap_or_else(|| "kwin_wayland".to_string());
        // KWin's own XDG_RUNTIME_DIR is a fresh, per-session directory (for
        // Wayland-socket isolation — see design.md's "Cross-user socket
        // reachability"), but PipeWire/wireplumber stay running under
        // redfog-server's own identity in *its* runtime dir, per that same
        // section — so PIPEWIRE_REMOTE must be an absolute path pointing
        // there, not a bare name that'd resolve inside KWin's own (empty,
        // unrelated) runtime dir instead.
        let pipewire_socket_path = format!("{}/pipewire-0", default_runtime_dir());
        // Same reasoning as pipewire_socket_path: pipewire-pulse also runs
        // under redfog-server's own identity, in its runtime dir — not
        // this session's private `runtime_dir`.
        let pulse_socket_path = pulse_socket_path();
        // redfog-server owns and creates this socket under its own
        // identity (see design.md's "Cross-user socket reachability") — the
        // target user's KWin needs an explicit grant to connect in, since
        // it's a different uid.
        //
        // Two grants are needed, not one: `HeadlessRuntime::start()` sets
        // its runtime dir to mode 0700 (owner-only) — Unix requires
        // *execute/traverse* permission on every directory component of a
        // path, not just read/write on the final file, so without also
        // granting that on the parent directory, the target user can't even
        // reach the socket file regardless of its own ACL. Confirmed live:
        // granting only the socket file left KWin's connection attempt
        // never even reaching PipeWire's own access-control code at all
        // (visible in its access-check log) — it failed at the kernel/
        // filesystem level first, silently, before ever getting there.
        async fn grant_acl(username: &str, path: &str, perm: &str, what: &str) {
            match tokio::process::Command::new("setfacl")
                .args(["-m", &format!("u:{username}:{perm}"), path])
                .output()
                .await
            {
                Ok(output) if output.status.success() => {
                    tracing::info!("granted {username} {what} access on {path}");
                }
                Ok(output) => {
                    tracing::warn!(
                        "setfacl granting {username} {what} access on {path} exited with {}: {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                Err(e) => {
                    tracing::warn!("failed to run setfacl granting {username} {what} access on {path}: {e}");
                }
            }
        }
        for (path, perm, what) in [
            (default_runtime_dir(), "x", "traverse"),
            (pipewire_socket_path.clone(), "rw", "connect to"),
            (format!("{}/pulse", default_runtime_dir()), "x", "traverse"),
            (pulse_socket_path.clone(), "rw", "connect to"),
        ] {
            grant_acl(username, &path, perm, what).await;
        }
        // See `redfog_server_user`'s doc comment: without this, redfog-server
        // (once it's not root/`username` itself) can't even traverse into
        // `default_runtime_dir()` to reach this session's own Wayland socket
        // below, regardless of that socket's own permissions.
        if let Some(server_user) = redfog_server_user() {
            grant_acl(&server_user, &default_runtime_dir(), "x", "traverse").await;
        }
        // dbus-run-session gives KWin (and whatever it spawns via
        // --exit-with-session, e.g. plasmashell) its own private, ephemeral
        // D-Bus session bus — without this, a systemd service running as
        // `username` falls back to that user's *real* D-Bus session bus
        // (the well-known /run/user/<uid>/bus), which already has a real
        // plasmashell registered on org.kde.plasmashell if the user has an
        // actual desktop session running. Confirmed live: klimek's real
        // desktop already owns that name. The direct-spawn path
        // (`CompositorSession::spawn`) doesn't need this itself since
        // `redfog-server`'s own `ensure_private_dbus_session()` already
        // wraps its *entire* process tree — but this systemd unit is a
        // separate process tree that never goes through that.
        let session_init_path = session_init_path()?;
        // `redfog-session-init <username> -- <command>...` (see its own doc
        // comment): does the *correct* uid/gid/supplementary-group drop
        // (initgroups from /etc/group, then setgid, then setuid) before
        // exec'ing into the real payload. Used here instead of this unit's
        // own `User=` (removed below) specifically so `bwrap`, further out
        // in this same exec chain, runs as real root rather than already
        // privilege-dropped — see gpu_sandbox_argv_prefix's doc comment for
        // why that distinction matters (running bwrap unprivileged forces it
        // into a user namespace, which remaps ownership of anything not the
        // calling uid, e.g. root-owned `/tmp/.X11-unix` — confirmed live,
        // this broke Xwayland with "not owned by root or us" on every
        // single spawn once GPU-sandboxing started actually restricting
        // /dev/dri instead of being a no-op). Deliberately does *not* open a
        // real PAM session (redfog-session-init no longer has that code at
        // all — see its own doc comment for why: it moves the whole process
        // tree into its own separate `pam_systemd`-created scope, breaking
        // this unit's cgroup-based termination) — the bwrap-as-root fix
        // never depended on a PAM session in the first place.
        let mut exec_start = format!(
            "{} {username} -- dbus-run-session -- {kwin_path} --virtual --width {width} --height {height} --scale 1 \
             --no-lockscreen --wayland-fd 3 --socket {socket_name} --xwayland",
            session_init_path.display()
        );
        if !payload.is_empty() {
            let session_script_path = write_session_script(&runtime_dir, socket_name, &pipewire_socket_path, &pulse_socket_path, payload)?;
            exec_start.push_str(&format!(" --exit-with-session {session_script_path}"));
        }
        // See gpu_sandbox_argv_prefix's doc comment. Prepended last, so it
        // wraps everything above (including redfog-session-init and
        // --exit-with-session) — with `User=` removed below, this unit's
        // ExecStart runs as real root, so bwrap here does its mount-namespace
        // setup without ever needing an unprivileged user namespace,
        // avoiding the identity-remap problem entirely rather than working
        // around its symptoms.
        if let Some(prefix) = gpu_sandbox_argv_prefix() {
            exec_start = format!("{} {exec_start}", prefix.join(" "));
        }
        let (_uid, _gid, home_dir, shell) = resolve_user(username).await?;
        // No `User=` here (unlike before) — see exec_start's own comment:
        // this unit now runs as root throughout, with redfog-session-init
        // (inside the ExecStart chain) doing the actual privilege drop. No
        // PAM session opened at all, so the entire process tree stays
        // inside this unit's own cgroup, and a plain `systemctl stop`
        // reliably kills all of it, the same as before this unit ever
        // involved redfog-session-init at all.
        //
        // Explicit HOME/USER/LOGNAME/SHELL below, unlike before: systemd's
        // own `User=` directive used to set these automatically (an NSS
        // lookup for the target user), for free, before the process ever
        // started. `setuid()`/`setgid()` (what redfog-session-init does
        // instead) only ever change process credentials, never environment
        // variables — without these, every KDE/Qt app in the session
        // inherited whatever HOME/SHELL this unit started with as root
        // (unset, in practice), confirmed live twice: every app tried to
        // read/write config under a literal `/.config/...` instead of the
        // real home directory, and konsole couldn't find `""` as a shell
        // and silently fell back to a default instead of the target user's
        // real one.
        let mut service_unit = format!(
            "[Service]\n\
             Type=simple\n\
             WorkingDirectory={home_dir}\n\
             Environment=HOME={home_dir}\n\
             Environment=SHELL={shell}\n\
             Environment=USER={username}\n\
             Environment=LOGNAME={username}\n\
             Environment=XDG_RUNTIME_DIR={runtime_dir}\n\
             Environment=PIPEWIRE_REMOTE={pipewire_socket_path}\n\
             Environment=PULSE_SERVER=unix:{pulse_socket_path}\n\
             Environment=KWIN_PLATFORM=virtual\n\
             Environment=KWIN_WAYLAND_NO_PERMISSION_CHECKS=1\n\
             Environment=LIBGL_ALWAYS_SOFTWARE=1\n\
             Environment=XDG_SESSION_TYPE=wayland\n\
             Environment=XDG_CURRENT_DESKTOP=KDE\n\
             Environment=DESKTOP_SESSION=plasma\n\
             Environment=KDE_FULL_SESSION=true\n\
             Environment=KDE_SESSION_VERSION=6\n\
             Environment=XDG_DATA_DIRS=/usr/local/share:/usr/share\n\
             Environment=XDG_CONFIG_DIRS=/etc/xdg\n\
             Environment=XDG_MENU_PREFIX=plasma-\n"
        );
        // TEMPORARY debugging aid: kwin_wayland's own logging is otherwise
        // silent about most of what it does. `kwin_screencast` is the
        // relevant Qt logging category for its PipeWire/DMA-BUF producer
        // (found via `strings` on the installed screencast.so — this repo
        // has no KWin source to grep). Output lands wherever this unit's
        // own journal goes, not the server's.
        if let Ok(rules) = std::env::var("REDFOG_DEBUG_KWIN_LOGGING_RULES") {
            service_unit.push_str(&format!("Environment=QT_LOGGING_RULES={rules}\n"));
        }
        service_unit.push_str(&format!("ExecStart={exec_start}\n"));

        let socket_unit_path = PathBuf::from(UNIT_DIR).join(format!("{unit_name}.socket"));
        let service_unit_path = PathBuf::from(UNIT_DIR).join(format!("{unit_name}.service"));
        std::fs::write(&socket_unit_path, socket_unit).map_err(|e| format!("failed to write {socket_unit_path:?}: {e}"))?;
        std::fs::write(&service_unit_path, service_unit).map_err(|e| format!("failed to write {service_unit_path:?}: {e}"))?;

        run_systemctl(&["daemon-reload"]).await?;
        // The name-matching between a .socket and .service unit only
        // triggers the service *lazily*, on the socket's first incoming
        // connection attempt (confirmed against `man systemd.socket`'s
        // Service= docs, and live: starting only the .service left KWin
        // trying to use an fd 3 that was never actually passed, failing
        // with "Failed to add 3 fd to display"). KWin is the one listening
        // on this socket, not connecting to it, so it must start
        // immediately regardless of whether anything has connected yet —
        // start the .socket explicitly first (binding it), then the
        // .service (which then picks up the already-bound fd via
        // LISTEN_FDS on its own startup, not through the lazy path).
        run_systemctl(&["start", &format!("{unit_name}.socket")]).await?;
        // Starting the .socket unit is what actually creates the socket
        // file on disk (ListenStream= binds it), so this grant can only
        // happen now, not earlier alongside the others above. The file only
        // gets SocketMode=0660 owned by the broker's own user — the target
        // user isn't in that group, so without this the KWin session's own
        // --exit-with-session child (running as that unprivileged user) has
        // no rw on the socket it's actually listening on.
        grant_acl(username, &wayland_socket_path, "rw", "connect to").await;
        // `redfog-server`'s own `CaptureSession::connect` (a *third* identity,
        // distinct from both the broker/root and `username`) needs the same
        // grant on this same file — see `redfog_server_user`'s doc comment.
        // Confirmed live this is real, not hypothetical: with no dedicated
        // `redfog` system user (redfog-server running as root instead), this
        // was a non-issue (root bypasses DAC checks entirely) — packaging
        // this with `User=redfog` in `redfog-server.service` surfaced it.
        if let Some(server_user) = redfog_server_user() {
            grant_acl(&server_user, &wayland_socket_path, "rw", "connect to").await;
        }
        run_systemctl(&["start", &format!("{unit_name}.service")]).await?;

        self.active
            .lock()
            .unwrap()
            .insert(session_id.to_string(), ActiveSession::Systemd { unit_name });
        Ok(wayland_socket_path)
    }

    /// Grants `username` access to a socket/runtime dir the *caller*
    /// already created and owns (e.g. redfog-moonlight embedding a
    /// `gst-wayland-display` pipeline directly in its own process), then
    /// spawns `argv` (with `env` applied) as that user pointed at it —
    /// unlike `spawn_via_systemd`, which creates the whole compositor
    /// runtime dir/socket itself, this one's caller already owns both. See
    /// `BrokerRequest::SpawnPayload`'s doc comment for the broader picture.
    pub async fn spawn_payload(
        &self,
        session_id: &str,
        username: &str,
        socket_path: &str,
        runtime_dir: &str,
        argv: &[String],
        env: &[(String, String)],
    ) -> Result<(), String> {
        let (_uid, _gid, home_dir, _shell) = resolve_user(username).await?;

        // Unlike spawn_via_systemd's runtime dir (which the broker creates
        // and chowns fully to the target user), this one is owned by the
        // caller and needs to *stay* that way — grant access instead of
        // transferring ownership. A default ACL (`-d`) is required too,
        // since the payload itself creates new files/sockets directly
        // inside it (Sway's own IPC socket, Xwayland's socket) — a plain
        // `-m` grant only covers files that already exist at grant time.
        for args in [
            vec!["-m".to_string(), format!("u:{username}:rwx"), runtime_dir.to_string()],
            vec!["-d".to_string(), "-m".to_string(), format!("u:{username}:rwx"), runtime_dir.to_string()],
            vec!["-m".to_string(), format!("u:{username}:rw"), socket_path.to_string()],
        ] {
            match tokio::process::Command::new("setfacl").args(&args).output().await {
                Ok(output) if output.status.success() => tracing::info!("setfacl {} succeeded", args.join(" ")),
                Ok(output) => tracing::warn!(
                    "setfacl {} exited with {}: {}",
                    args.join(" "),
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                ),
                Err(e) => tracing::warn!("failed to run setfacl {}: {e}", args.join(" ")),
            }
        }

        let session_init_path = session_init_path()?;
        let unit_name = format!("redfog-payload-session-{}-{session_id}", std::process::id());
        // Scope-wrapped (see `ActiveSession::Scoped`'s doc comment): the
        // caller-provided `argv` may itself be (or spawn) a wrapper process
        // with its own forked children, and `terminate()` needs to be able
        // to kill the *whole* tree — including anything that calls
        // `setsid()`/`setpgid()` along the way, which a plain process group
        // can't reach but a cgroup can't be escaped from.
        let mut cmd = tokio::process::Command::new("systemd-run");
        cmd.arg("--scope").arg("--collect").arg(format!("--unit={unit_name}")).arg("--").arg(&session_init_path).arg(username).arg("--").args(argv);
        cmd.env_clear()
            .env("HOME", &home_dir)
            .env("USER", username)
            .env("LOGNAME", username)
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/bin")
            .current_dir(&home_dir)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        for (key, value) in env {
            cmd.env(key, value);
        }

        let child = cmd.spawn().map_err(|e| format!("failed to spawn systemd-run: {e}"))?;
        self.active.lock().unwrap().insert(session_id.to_string(), ActiveSession::Scoped { child, unit_name });
        Ok(())
    }

    /// Backing implementation for `BrokerRequest::IsSessionAlive` — see its
    /// doc comment for why only the broker can answer this. Self-cleaning:
    /// a session found to have exited is removed from `active` (and, for
    /// the `Systemd` case, torn down the same way `terminate` would) right
    /// here, rather than waiting on an explicit `TerminateSession` that the
    /// caller has no reason to send — detecting the death via this call is
    /// the whole reason the caller didn't already know to send one.
    pub async fn is_session_alive(&self, session_id: &str) -> bool {
        // `Child::try_wait` is synchronous and non-blocking, so the
        // Child-backed cases resolve (and, if dead, get removed) entirely
        // under one lock. The `Systemd` case can't: checking it needs an
        // async `systemctl is-active` call, so it only reads the unit name
        // here and finishes the check (and any cleanup) after the lock is
        // released, below.
        enum Peek {
            Alive,
            DeadChild,
            NeedsSystemdCheck(String),
            Unknown,
        }
        let peek = {
            let mut active = self.active.lock().unwrap();
            let peek = match active.get_mut(session_id) {
                Some(ActiveSession::DirectChild { child } | ActiveSession::Scoped { child, .. }) => {
                    match child.try_wait() {
                        Ok(None) => Peek::Alive,
                        _ => Peek::DeadChild,
                    }
                }
                Some(ActiveSession::Systemd { unit_name }) => Peek::NeedsSystemdCheck(unit_name.clone()),
                None => Peek::Unknown,
            };
            if matches!(peek, Peek::DeadChild) {
                active.remove(session_id);
            }
            peek
        };

        match peek {
            Peek::Alive => true,
            Peek::DeadChild | Peek::Unknown => false,
            Peek::NeedsSystemdCheck(unit_name) => {
                let alive = run_systemctl(&["is-active", "--quiet", &format!("{unit_name}.service")]).await.is_ok();
                if !alive {
                    self.active.lock().unwrap().remove(session_id);
                    // Same teardown `terminate` does for the Systemd case —
                    // the service already stopped itself, but the socket
                    // unit and generated unit files are still there.
                    let _ = run_systemctl(&["stop", &format!("{unit_name}.socket")]).await;
                    let _ = std::fs::remove_file(PathBuf::from(UNIT_DIR).join(format!("{unit_name}.socket")));
                    let _ = std::fs::remove_file(PathBuf::from(UNIT_DIR).join(format!("{unit_name}.service")));
                    let _ = run_systemctl(&["daemon-reload"]).await;
                }
                alive
            }
        }
    }

    /// Backing implementation for `BrokerRequest::ReadUserSessionConfig` —
    /// see its doc comment for why only the broker can do this (root reads
    /// past normal `700` home-directory permissions; `resolve_user` is the
    /// same helper `spawn_payload`/`home_dir_for` already use). `Ok(None)`
    /// for a missing file is the expected, common case (most users won't
    /// have created one), not an error.
    pub async fn read_user_session_config(&self, username: &str) -> Result<Option<redfog_broker_protocol::UserSessionConfig>, String> {
        let (_uid, _gid, home_dir, _shell) = resolve_user(username).await?;
        let path = std::path::Path::new(&home_dir).join(".config/redfog/session.toml");
        let contents = match tokio::fs::read_to_string(&path).await {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("failed to read {path:?}: {e}")),
        };
        toml::from_str(&contents).map(Some).map_err(|e| format!("failed to parse {path:?}: {e}"))
    }

    pub async fn terminate(&self, session_id: &str) -> Result<(), String> {
        let overall_start = std::time::Instant::now();
        tracing::info!("terminate({session_id}): starting");
        let session = self
            .active
            .lock()
            .unwrap()
            .remove(session_id)
            .ok_or_else(|| format!("no active session {session_id}"))?;

        match session {
            ActiveSession::DirectChild { mut child } => {
                // No process group of its own (see `spawn_fake`'s own
                // comment) — it directly tracks the real `kwin_wayland`
                // process, no wrapper forking a separate child underneath,
                // so a plain single-PID kill is already correct here.
                let _ = child.kill().await;
            }
            ActiveSession::Scoped { mut child, unit_name } => {
                // `pam_open_session` (via `pam_systemd.so`, when the
                // spawned process opens a real PAM session) registers a
                // genuine logind session for the target user and migrates
                // the calling process's *cgroup* into that session's own
                // `session-<id>.scope` — independently of, and escaping,
                // whatever cgroup `systemd-run --scope` originally placed
                // it in. Confirmed live: `loginctl session-status <id>`
                // showed the *entire* spawned tree (kwin_wayland,
                // plasmashell, portals, ...) under a fresh logind session,
                // while the custom scope unit this process was launched in
                // had already been silently garbage-collected (nothing
                // left in it to track) — killing that scope was a no-op,
                // and `child.wait()` below then hung forever waiting for a
                // process nothing had actually signaled. Check the
                // *current* cgroup at kill time and prefer
                // `loginctl kill-session` if a real logind session is what
                // actually contains it now — defensive against this
                // regardless of whether anything on this particular path
                // currently opens a PAM session at all.
                let pid = child.id();
                let logind_session = pid.and_then(logind_session_id_for);
                tracing::info!(
                    "terminate({session_id}): tracked pid={pid:?}, unit={unit_name}.scope, current cgroup shows logind \
                     session={logind_session:?}"
                );
                if let Some(session_id) = &logind_session {
                    let result = run_loginctl(&["kill-session", session_id, "--signal=SIGKILL", "--kill-whom=all"]).await;
                    tracing::info!("terminate: loginctl kill-session {session_id} -> {result:?}");
                } else {
                    // Kills *every* process in the scope's cgroup, not
                    // just this one tracked PID — see `ActiveSession::
                    // Scoped`'s own doc comment for why a plain
                    // process-group kill (an earlier fix attempt) isn't
                    // reliably enough on its own (a descendant calling
                    // `setsid()`/`setpgid()` escapes it; nothing escapes
                    // a cgroup). Always the *system* manager — nothing
                    // registers one of these scopes with `--user` anymore.
                    let unit = format!("{unit_name}.scope");
                    let args = vec!["kill", "--kill-who=all", "--signal=SIGKILL", &unit];
                    let result = run_systemctl(&args).await;
                    tracing::info!("terminate: systemctl {args:?} -> {result:?}");
                }
                // Best-effort either way: reaps the tracked child
                // regardless of whether the kill call above succeeded.
                // Bounded, not a bare `.await` — if the kill above didn't
                // actually reach the process for some reason not yet
                // understood, this must not hang `terminate()` (and
                // therefore the whole `LogOut` request) forever; better to
                // return an error the caller can see and log than to sit
                // silently stuck.
                let wait_start = std::time::Instant::now();
                match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
                    Ok(result) => tracing::info!("terminate: child.wait() returned {result:?} after {:?}", wait_start.elapsed()),
                    Err(_) => tracing::error!(
                        "terminate: child.wait() for pid={pid:?} did not return within 10s of killing it — the process is still alive \
                         somehow despite the kill above; giving up waiting on it rather than hanging this LogOut request forever"
                    ),
                }
            }
            ActiveSession::Systemd { unit_name } => {
                // Socket first, then service — stopping the service while
                // its socket is still active logs a harmless but confusing
                // "triggering units are still active" warning (confirmed
                // live); stopping the socket first avoids it entirely.
                run_systemctl(&["stop", &format!("{unit_name}.socket")]).await?;
                run_systemctl(&["stop", &format!("{unit_name}.service")]).await?;
                let _ = std::fs::remove_file(PathBuf::from(UNIT_DIR).join(format!("{unit_name}.socket")));
                let _ = std::fs::remove_file(PathBuf::from(UNIT_DIR).join(format!("{unit_name}.service")));
                run_systemctl(&["daemon-reload"]).await?;
            }
        }
        tracing::info!("terminate({session_id}): done after {:?}", overall_start.elapsed());
        Ok(())
    }
}

async fn run_systemctl(args: &[&str]) -> Result<(), String> {
    let output = tokio::process::Command::new("systemctl")
        .args(args)
        .output()
        .await
        .map_err(|e| format!("failed to run systemctl {args:?}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "systemctl {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

async fn run_loginctl(args: &[&str]) -> Result<(), String> {
    let output = tokio::process::Command::new("loginctl")
        .args(args)
        .output()
        .await
        .map_err(|e| format!("failed to run loginctl {args:?}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "loginctl {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

/// See `terminate()`'s `Scoped` case: `pam_open_session` can migrate the
/// process into a real logind session's own `session-<id>.scope` cgroup,
/// independently of whatever scope it was originally launched in. Reads
/// `/proc/<pid>/cgroup` and extracts that session id, if present.
fn logind_session_id_for(pid: u32) -> Option<String> {
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    parse_logind_session_id(&cgroup)
}

/// Pure parsing, split out from `logind_session_id_for` so it's testable
/// without a real `/proc` entry. Cgroup v2 (the only kind modern systemd
/// uses) has exactly one line, e.g.
/// `0::/user.slice/user-1000.slice/session-c14.scope` — but this scans
/// every line and tolerates a leading `0::`/`N:name:` prefix regardless,
/// in case of a hybrid/legacy cgroup v1 layout.
fn parse_logind_session_id(cgroup_contents: &str) -> Option<String> {
    const MARKER: &str = "/session-";
    for line in cgroup_contents.lines() {
        let Some(idx) = line.find(MARKER) else { continue };
        let rest = &line[idx + MARKER.len()..];
        // Isolate just the `<id>.scope` path segment first (there may be
        // nested sub-cgroup path components after it), then strip the
        // `.scope` suffix.
        let segment = rest.split('/').next().unwrap_or(rest);
        let id = segment.strip_suffix(".scope").unwrap_or(segment);
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::parse_logind_session_id;

    #[test]
    fn parses_session_id_from_real_cgroup_v2_format() {
        // Confirmed live: exactly this format, from a session redfog-
        // broker's own PAM-spawned process got migrated into.
        assert_eq!(
            parse_logind_session_id("0::/user.slice/user-1000.slice/session-c14.scope\n"),
            Some("c14".to_string())
        );
    }

    #[test]
    fn parses_session_id_with_nested_subpath() {
        assert_eq!(
            parse_logind_session_id("0::/user.slice/user-1000.slice/session-c14.scope/some/nested/cgroup\n"),
            Some("c14".to_string())
        );
    }

    #[test]
    fn returns_none_when_not_in_a_logind_session() {
        assert_eq!(parse_logind_session_id("0::/redfog-pam-session-12345-0.scope\n"), None);
        assert_eq!(parse_logind_session_id("0::/user.slice/user-1000.slice\n"), None);
        assert_eq!(parse_logind_session_id(""), None);
    }

    #[test]
    fn tolerates_legacy_cgroup_v1_style_lines() {
        assert_eq!(
            parse_logind_session_id("1:name=systemd:/user.slice/user-1000.slice/session-5.scope\n7:memory:/user.slice\n"),
            Some("5".to_string())
        );
    }
}

fn current_username() -> Result<String, String> {
    std::env::var("USER").map_err(|e| e.to_string())
}

/// Looks up `username`'s uid/gid/home directory/shell via NSS (`getent
/// passwd`), rather than assuming `/home/{username}` or relying on
/// systemd's `%h` specifier for the home directory — confirmed live that
/// `%h` in a *system* unit's `WorkingDirectory=` resolves against the
/// service manager's own context (root), not the target user, landing new
/// sessions in `/root` instead of the target user's actual home. Used by
/// `spawn_via_systemd` and `spawn_payload`. Note `redfog-session-init` (the
/// actual privilege-dropping helper both of those exec into) does this
/// same NSS lookup independently, itself, since it runs as a separate
/// process with no access to this async fn.
async fn resolve_user(username: &str) -> Result<(u32, u32, String, String), String> {
    let output = tokio::process::Command::new("getent")
        .args(["passwd", username])
        .output()
        .await
        .map_err(|e| format!("failed to run getent passwd {username}: {e}"))?;
    if !output.status.success() {
        return Err(format!("getent passwd {username} exited with {}", output.status));
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<&str> = line.trim().split(':').collect();
    let (Some(uid), Some(gid), Some(home), Some(shell)) = (fields.get(2), fields.get(3), fields.get(5), fields.get(6)) else {
        return Err(format!("could not parse getent passwd {username} output: {line:?}"));
    };
    if home.is_empty() {
        return Err(format!("empty home directory in getent passwd {username} output: {line:?}"));
    }
    let uid: u32 = uid.parse().map_err(|e| format!("invalid uid in getent passwd {username} output: {e}"))?;
    let gid: u32 = gid.parse().map_err(|e| format!("invalid gid in getent passwd {username} output: {e}"))?;
    Ok((uid, gid, home.to_string(), shell.to_string()))
}

/// `--exit-with-session` takes exactly *one* value, which KWin itself
/// shell-splits (`KShell::splitArgs`) into program+args — confirmed by
/// reading `main_wayland.cpp`. Appending `-- <args>` at the outer (systemd
/// `ExecStart=`, or argv, in the direct-fork path) level never reaches that
/// split at all; it lands in KWin's separate `--applications-to-start`
/// feature instead — a pre-existing bug (confirmed live: `plasmashell
/// --no-respawn` always ran as bare `plasmashell`, `--no-respawn` silently
/// dropped every time).
///
/// Writing a wrapper *script* file and pointing `--exit-with-session` at
/// that single path (no embedded args/quoting at all) sidesteps that
/// entirely, and also gives us a place to run
/// `dbus-update-activation-environment` first: a D-Bus-exec-activated
/// service Plasma Shell hard-depends on (`kactivitymanagerd`) defaults to
/// X11/xcb and crashes unless the session bus's own activation environment
/// has `WAYLAND_DISPLAY` — confirmed live via "Could not load the Qt
/// platform plugin xcb" / "Aborting shell load: the activity manager
/// daemon is not running". Nothing sets that by default; the original
/// prototype (`proto.sh`) did this exact call by hand. It must run *inside*
/// this session's own `dbus-run-session` bus, which only exists once
/// KWin's `--exit-with-session` mechanism actually fires (i.e. once the
/// compositor is already fully up) — so doing it here, right before
/// exec'ing the real payload, gets that ordering for free, no separate
/// wait-for-socket polling needed.
fn write_session_script(runtime_dir: &str, socket_name: &str, pipewire_socket_path: &str, pulse_socket_path: &str, payload: &[String]) -> Result<String, String> {
    fn shell_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
    let payload_cmd = payload.iter().map(|arg| shell_quote(arg)).collect::<Vec<_>>().join(" ");
    let session_script = format!(
        "#!/bin/sh\n\
         export PULSE_SERVER=unix:{pulse_socket_path}\n\
         dbus-update-activation-environment --systemd WAYLAND_DISPLAY={socket_name} XDG_RUNTIME_DIR={runtime_dir} PIPEWIRE_REMOTE={pipewire_socket_path} PULSE_SERVER=unix:{pulse_socket_path}\n\
         exec {payload_cmd}\n"
    );
    let session_script_path = format!("{runtime_dir}/session-start.sh");
    std::fs::write(&session_script_path, session_script).map_err(|e| format!("failed to write {session_script_path}: {e}"))?;
    let mut perms = std::fs::metadata(&session_script_path)
        .map_err(|e| format!("failed to stat {session_script_path}: {e}"))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&session_script_path, perms).map_err(|e| format!("failed to chmod {session_script_path}: {e}"))?;
    Ok(session_script_path)
}

/// Locates the `redfog-session-init` helper binary alongside the broker's
/// own executable (same workspace target dir) — an env var override exists
/// for the same reason `REDFOG_KWIN_WAYLAND_PATH` does, for tests/non-standard
/// installs.
fn session_init_path() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("REDFOG_SESSION_INIT_PATH") {
        return Ok(PathBuf::from(path));
    }
    let exe = std::env::current_exe().map_err(|e| format!("failed to determine current_exe: {e}"))?;
    let dir = exe.parent().ok_or_else(|| format!("{exe:?} has no parent directory"))?;
    Ok(dir.join("redfog-session-init"))
}

fn which_kwin_wayland() -> Option<String> {
    std::env::var("REDFOG_KWIN_WAYLAND_PATH").ok()
}

/// KWin's `--virtual` backend (what every redfog session spawns) has no
/// GPU-selection logic in any released version: `findRenderDevice()`
/// (`src/backends/virtual/virtual_backend.cpp`) just takes libdrm's first
/// enumerated DRM device, with zero vendor/preference filtering. On a
/// hybrid Intel+NVIDIA machine this can silently pick the iGPU instead of
/// the GPU redfog's CUDA/NVENC/Vulkan-bridge encode path is built around —
/// confirmed live on a GTX 1070 + iGPU test machine: OOM at 1080p, garbled
/// video at 720p, both traced to the wrong physical GPU rendering KWin's
/// own scene, not a redfog bug. Upstream's real fix (`KWIN_RENDER_NODES`
/// env var, via a new `GpuManager` class) isn't in any released KWin
/// version yet (landed 2026-07-09, after v6.7.3 was tagged).
///
/// Until that ships, this works around it unconditionally: every spawn
/// hides `/dev/dri` down to a single, deliberately-chosen render node via
/// `bwrap`, before KWin ever gets a chance to enumerate anything — the
/// exact approach validated manually via
/// `scripts/test-drm-device-sandboxing.sh` on the affected machine. Always
/// narrowing to exactly one node, rather than only doing this on machines
/// that look ambiguous, is deliberate: one code path, one rule, regardless
/// of how many GPUs happen to be installed — a machine with a single GPU
/// just narrows down to the only node that was ever there. Callers should
/// prepend the returned argv to whatever they'd otherwise exec: `--bind /
/// /` + `--dev-bind /dev /dev` mirror the entire real filesystem and device
/// tree first (so nothing *else* about the session — `sudo`, process
/// visibility, `/dev/input`, arbitrary paths — changes), `--tmpfs /dev/dri`
/// then blanks out just that one directory, and the final `--dev-bind`
/// re-adds only the node `select_gpu_render_node` chose. No namespace
/// unsharing beyond the mount namespace `bwrap` always creates.
///
/// Returns `None` (skip sandboxing, behave exactly as before) only when
/// there's truly nothing to narrow to (no `/dev/dri` render nodes at all —
/// a machine with no GPU) or when `bwrap` itself isn't installed (logged,
/// not fatal — see its own check below).
fn gpu_sandbox_argv_prefix() -> Option<Vec<String>> {
    let node = select_gpu_render_node()?;

    let bwrap_available =
        std::process::Command::new("bwrap").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if !bwrap_available {
        tracing::warn!(
            "would sandbox /dev/dri down to {node} for KWin, but `bwrap` isn't installed -- skipping (install bubblewrap to enable it; KWin will \
             see every render node on this machine and may pick the wrong one)"
        );
        return None;
    }

    let extra = related_dri_nodes(&node);

    let sibling_desc = match (&extra.card_node, extra.by_path_links.len()) {
        (None, 0) => String::new(),
        (Some(card), 0) => format!(" (plus {card})"),
        (None, n) => format!(" (plus {n} by-path symlink(s))"),
        (Some(card), n) => format!(" (plus {card} and {n} by-path symlink(s))"),
    };
    tracing::info!("sandboxing /dev/dri down to {node}{sibling_desc} for KWin");

    let mut argv = vec![
        "bwrap".to_string(),
        "--bind".to_string(),
        "/".to_string(),
        "/".to_string(),
        "--dev-bind".to_string(),
        "/dev".to_string(),
        "/dev".to_string(),
        "--tmpfs".to_string(),
        "/dev/dri".to_string(),
        "--dev-bind".to_string(),
        node.clone(),
        node,
    ];
    if let Some(card_node) = extra.card_node {
        argv.push("--dev-bind".to_string());
        argv.push(card_node.clone());
        argv.push(card_node);
    }
    if !extra.by_path_links.is_empty() {
        argv.push("--dir".to_string());
        argv.push("/dev/dri/by-path".to_string());
        for (link_path, target) in extra.by_path_links {
            argv.push("--symlink".to_string());
            argv.push(target);
            argv.push(link_path);
        }
    }
    argv.push("--".to_string());
    Some(argv)
}

/// Sibling device nodes for the same physical GPU as `render_node`, besides
/// the render node itself: the "primary"/KMS node (`/dev/dri/cardN`) and any
/// `/dev/dri/by-path/*` symlinks pointing at either one. `select_gpu_render_node`
/// only ever needs the render node itself (confirmed against KWin's own
/// `findRenderDevice()` — `src/backends/virtual/virtual_backend.cpp` — which
/// never opens a primary/card node for a normal, non-`vgem` PCI device, since
/// a headless `--virtual` backend never does real KMS/mode-setting), but
/// nothing rules out some *other* part of the process tree (Xwayland, a
/// future feature) wanting the sibling node or a by-path symlink for the
/// exact same physical GPU we've already decided to expose — there's no
/// isolation benefit to hiding those too (we're only trying to hide the
/// *other* GPU), so include them rather than find out the hard way, the way
/// `renderD129`'s sibling-Intel-node warning got found the hard way.
struct RelatedDriNodes {
    card_node: Option<String>,
    by_path_links: Vec<(String, String)>, // (link path under /dev/dri/by-path, symlink target as originally stored)
}

fn related_dri_nodes(render_node: &str) -> RelatedDriNodes {
    let render_name = render_node.rsplit('/').next().unwrap_or(render_node);

    let card_node = std::fs::read_dir(format!("/sys/class/drm/{render_name}/device/drm"))
        .ok()
        .and_then(|entries| {
            entries.flatten().find_map(|e| {
                let name = e.file_name();
                let name = name.to_str()?;
                name.starts_with("card").then(|| format!("/dev/dri/{name}"))
            })
        });
    let card_name = card_node.as_deref().map(|p| p.rsplit('/').next().unwrap_or(p).to_string());

    let mut by_path_links = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/dev/dri/by-path") {
        for entry in entries.flatten() {
            let Ok(target) = std::fs::read_link(entry.path()) else { continue };
            let target_name = target.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if target_name == render_name || Some(target_name) == card_name.as_deref() {
                by_path_links.push((entry.path().to_string_lossy().into_owned(), target.to_string_lossy().into_owned()));
            }
        }
    }

    RelatedDriNodes { card_node, by_path_links }
}

/// Picks which `/dev/dri/renderD*` node KWin's `--virtual` backend should
/// be allowed to see (see `gpu_sandbox_argv_prefix`'s doc comment for why
/// this matters at all). Two sources, checked in order:
///
/// 1. `REDFOG_GPU_RENDER_NODES` — a colon-separated, priority-ordered list
///    of render-node paths (e.g.
///    `/dev/dri/renderD128:/dev/dri/renderD129`). The first entry that
///    actually exists on this machine wins. Deliberately *not* "expose all
///    of them" — bind order doesn't control which one KWin's own libdrm
///    enumeration picks first (that's sysfs/PCI-topology order, outside our
///    control), so exposing more than one wouldn't actually let this
///    variable determine the outcome; narrowing to exactly one is the only
///    way to make the choice deterministic. Render-node paths were picked
///    over e.g. PCI bus IDs as the identifier here because they need no
///    extra resolution step and this function's own `tracing::info!` (or
///    `scripts/test-drm-device-sandboxing.sh`) already prints the
///    vendor<->path mapping needed to fill this in.
/// 2. Auto-detection: every render node's PCI vendor is read from
///    `/sys/class/drm/<node>/device/vendor`, ranked NVIDIA > AMD > anything
///    unrecognized > Intel (integrated graphics is the least-preferred
///    fallback, not excluded outright — a machine with only an iGPU still
///    needs *a* node picked), and the highest-ranked one wins (ties broken
///    by path, for determinism). NVIDIA is ranked first because it's the
///    only vendor redfog's CUDA/NVENC/Vulkan-bridge pipeline currently
///    supports.
///
/// Returns `None` only when `/dev/dri` has no render nodes at all.
fn select_gpu_render_node() -> Option<String> {
    fn vendor_rank(vendor_hex: &str) -> u8 {
        match vendor_hex {
            "0x10de" => 0, // NVIDIA -- the only vendor redfog's encode pipeline currently supports
            "0x1002" => 1, // AMD
            "0x8086" => 3, // Intel -- deprioritized, but still picked if it's the only node present
            _ => 2,        // unrecognized -- treat as a dedicated GPU until proven otherwise
        }
    }

    let dri_entries = std::fs::read_dir("/dev/dri").ok()?;
    let mut nodes: Vec<(String, String)> = Vec::new(); // (path, lowercased vendor hex, e.g. "0x10de")
    for entry in dri_entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("renderD") {
            continue;
        }
        let vendor = std::fs::read_to_string(format!("/sys/class/drm/{name}/device/vendor"))
            .map(|v| v.trim().to_ascii_lowercase())
            .unwrap_or_default();
        nodes.push((format!("/dev/dri/{name}"), vendor));
    }
    if nodes.is_empty() {
        return None;
    }

    if let Ok(configured) = std::env::var("REDFOG_GPU_RENDER_NODES") {
        let priority: Vec<&str> = configured.split(':').filter(|s| !s.is_empty()).collect();
        if let Some(chosen) = priority.iter().find(|candidate| nodes.iter().any(|(path, _)| path == *candidate)) {
            return Some((*chosen).to_string());
        }
        tracing::warn!(
            "REDFOG_GPU_RENDER_NODES={configured:?} set but none of those paths exist on this machine (found: {:?}) -- falling back to \
             auto-detection",
            nodes.iter().map(|(path, _)| path.as_str()).collect::<Vec<_>>()
        );
    }

    nodes.sort_by(|(path_a, vendor_a), (path_b, vendor_b)| vendor_rank(vendor_a).cmp(&vendor_rank(vendor_b)).then_with(|| path_a.cmp(path_b)));
    Some(nodes.into_iter().next().unwrap().0)
}

fn default_runtime_dir() -> String {
    std::env::var("REDFOG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp/redfog-runtime".to_string())
}

/// Which identity `redfog-server` itself runs as, if it's not the same
/// process/user as this broker — `None` (the default, if unset) means
/// "don't grant anyone else access beyond `username`", preserving the old
/// behavior every dev/test invocation still relies on, where `redfog-server`
/// runs as root (bypassing DAC checks entirely) or as the same uid doing the
/// spawning.
///
/// Real gap this closes: once packaging gave `redfog-server` its own
/// dedicated, unprivileged system user (`packaging/arch/redfog-server.service`'s
/// `User=redfog`), it needs the exact same kind of per-resource ACL grant
/// `username` (the login target) already gets on `default_runtime_dir()`
/// and the session's Wayland socket — `CaptureSession::connect` in
/// `redfog-core::session_backend` runs as *this* identity, not root or
/// `username`. Confirmed live: without this, `default_runtime_dir()` itself
/// (mode 0710, no ACL entry for anyone but `username`) blocked even a
/// traverse/`stat()`, so redfog-server's wait loop for the compositor socket
/// timed out claiming it "failed to appear" even though the broker and KWin
/// had both already created it successfully.
fn redfog_server_user() -> Option<String> {
    std::env::var("REDFOG_SERVER_USER").ok().filter(|s| !s.is_empty())
}

/// PipeWire/wireplumber/pipewire-pulse all run under `redfog-server`'s own
/// identity, in *its* runtime dir (`HeadlessRuntime::start`) — never a
/// session's own private `runtime_dir` (that's KWin's isolated
/// `XDG_RUNTIME_DIR`, a completely different directory). A single helper
/// exists specifically so every call site gets this from one place instead
/// of hand-building the same path — a hand-built copy of this once drifted
/// from `pipewire_socket_path`'s equivalent construction and pointed at the
/// wrong directory, which meant `PULSE_SERVER` never had a pulse server
/// listening on it at all (confirmed live: KDE showed "connection to sound
/// server lost").
fn pulse_socket_path() -> String {
    format!("{}/pulse/native", default_runtime_dir())
}

#[cfg(test)]
mod pulse_socket_path_tests {
    use super::*;

    #[test]
    fn always_rooted_at_default_runtime_dir_not_a_session_dir() {
        // SAFETY: this test doesn't run concurrently with anything else
        // that reads/writes REDFOG_RUNTIME_DIR — it's the only test in this
        // crate that touches it.
        unsafe { std::env::set_var("REDFOG_RUNTIME_DIR", "/tmp/redfog-runtime-test-marker") };
        assert_eq!(pulse_socket_path(), "/tmp/redfog-runtime-test-marker/pulse/native");
        unsafe { std::env::remove_var("REDFOG_RUNTIME_DIR") };
    }
}
