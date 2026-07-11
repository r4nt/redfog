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

use tokio::process::Child;

const UNIT_DIR: &str = "/run/systemd/system";

enum ActiveSession {
    Systemd { unit_name: String },
    /// `REDFOG_BROKER_FAKE_SPAWN` mode — see `spawn()`.
    DirectChild { child: Child },
    /// `REDFOG_BROKER_PAM_SPAWN` mode — see `spawn_via_pam`.
    DirectPam { child: Child },
    /// See `spawn_payload` — a payload run against a socket the *caller*
    /// (not the broker) created and owns.
    Payload { child: Child },
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

        // Experimental alternative to spawn_via_systemd — see spawn_via_pam's
        // doc comment. Not the default yet: real PAM session support here is
        // new and hasn't seen the same mileage the systemd path has.
        if std::env::var_os("REDFOG_BROKER_PAM_SPAWN").is_some() {
            let username = match std::env::var("REDFOG_BROKER_FORCE_SPAWN_USER") {
                Ok(forced) => {
                    tracing::warn!("REDFOG_BROKER_FORCE_SPAWN_USER set — spawning as {forced} instead of requested {username}");
                    forced
                }
                Err(_) => username.to_string(),
            };
            return self.spawn_via_pam(session_id, &username, width, height, socket_name, payload).await;
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

        let mut cmd = tokio::process::Command::new(&kwin_path);
        cmd.env("KWIN_PLATFORM", "virtual")
            .env("KWIN_WAYLAND_NO_PERMISSION_CHECKS", "1")
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("PIPEWIRE_REMOTE", &pipewire_socket_path)
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
        let child = cmd
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("failed to spawn {kwin_path}: {e}"))?;

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
        ] {
            grant_acl(username, &path, perm, what).await;
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
        let mut exec_start = format!(
            "dbus-run-session -- {kwin_path} --virtual --width {width} --height {height} --scale 1 \
             --no-lockscreen --wayland-fd 3 --socket {socket_name} --xwayland"
        );
        if !payload.is_empty() {
            let session_script_path = write_session_script(&runtime_dir, socket_name, &pipewire_socket_path, payload)?;
            exec_start.push_str(&format!(" --exit-with-session {session_script_path}"));
        }
        let home_dir = home_dir_for(username).await?;
        let service_unit = format!(
            "[Service]\n\
             Type=simple\n\
             User={username}\n\
             WorkingDirectory={home_dir}\n\
             Environment=XDG_RUNTIME_DIR={runtime_dir}\n\
             Environment=PIPEWIRE_REMOTE={pipewire_socket_path}\n\
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
             Environment=XDG_MENU_PREFIX=plasma-\n\
             ExecStart={exec_start}\n"
        );

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
        // no rw on the socket it's actually listening on. Confirmed live:
        // redfog-server's own CaptureSession never hit this because it
        // connects as root, which bypasses file permission checks entirely
        // — only non-root clients on this same socket were ever affected.
        grant_acl(username, &wayland_socket_path, "rw", "connect to").await;
        run_systemctl(&["start", &format!("{unit_name}.service")]).await?;

        self.active
            .lock()
            .unwrap()
            .insert(session_id.to_string(), ActiveSession::Systemd { unit_name });
        Ok(wayland_socket_path)
    }

    /// Experimental alternative to `spawn_via_systemd`: instead of
    /// generating/loading templated systemd units and delegating the
    /// privilege drop and Wayland-socket handoff to systemd, the broker
    /// does both itself — binding the Wayland socket directly (as root,
    /// before dropping privilege, so the fd is inherited straight across
    /// `fork`+`exec` with no path-based permission check ever needed on it
    /// at all — eliminating that whole class of ACL bug by construction),
    /// opening a real PAM session (unlike the systemd path, which only ever
    /// authenticates, never calls `pam_open_session`), and dropping to the
    /// target uid/gid manually. See design.md / project memory for the
    /// comparison against the systemd-unit path this was modeled to
    /// simplify on (inspired by `idea.md` in the repo root).
    ///
    /// Gated behind `REDFOG_BROKER_PAM_SPAWN` rather than being the default:
    /// this is new and hasn't seen the mileage the systemd path has yet.
    ///
    /// KNOWN LIMITATIONS (acceptable for this experimental flag, not for
    /// production):
    /// - The opened PAM session is never explicitly closed (no live process
    ///   remains after `execve()` to call `pam_close_session()` — systemd's
    ///   own `PAMName=` keeps a small "(sd-pam)" placeholder process alive
    ///   for exactly this reason; this doesn't yet do that). Logind should
    ///   still reclaim it once every process in the session exits.
    /// - `terminate()` just kills the child directly, same as `spawn_fake`.
    async fn spawn_via_pam(
        &self,
        session_id: &str,
        username: &str,
        width: u32,
        height: u32,
        socket_name: &str,
        payload: &[String],
    ) -> Result<String, String> {
        let home_dir = home_dir_for(username).await?;

        let runtime_dir = format!("{}/session-{session_id}", default_runtime_dir());
        let wayland_socket_path = format!("{runtime_dir}/{socket_name}");
        std::fs::create_dir_all(&runtime_dir).map_err(|e| format!("failed to create {runtime_dir}: {e}"))?;
        // Same reasoning as spawn_via_systemd's chown: this directory is
        // used as the target user's own XDG_RUNTIME_DIR, and e.g. Xwayland
        // needs to create files directly in it.
        match tokio::process::Command::new("chown").args([username, &runtime_dir]).output().await {
            Ok(output) if output.status.success() => tracing::info!("chowned {runtime_dir} to {username}"),
            Ok(output) => {
                return Err(format!(
                    "chown {runtime_dir} to {username} exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            Err(e) => return Err(format!("failed to run chown on {runtime_dir}: {e}")),
        }

        let _ = std::fs::remove_file(&wayland_socket_path);
        let listener = std::os::unix::net::UnixListener::bind(&wayland_socket_path)
            .map_err(|e| format!("failed to bind {wayland_socket_path}: {e}"))?;
        let wayland_fd = std::os::unix::io::AsRawFd::as_raw_fd(&listener);

        let pipewire_socket_path = format!("{}/pipewire-0", default_runtime_dir());
        // The pre-bound fd handoff only helps *KWin itself* (which inherits
        // it directly via fork+exec, following FD_CLOEXEC being cleared
        // above) — it does nothing for any *other* client that connects to
        // this same socket by path instead, which is exactly what KWin's
        // own --exit-with-session child does (a separate process KWin
        // itself spawns via QProcess, looking up WAYLAND_DISPLAY/
        // XDG_RUNTIME_DIR and connect()ing like any normal Wayland client)
        // — confirmed live: without this grant, that child crashed with
        // the exact same WaylandError(Connection(NoCompositor)) the
        // systemd path hit before its own equivalent fix. PipeWire's
        // socket needs the same kind of grant for the same reason (KWin
        // connects to it by path too, no fd handoff for that one at all).
        for (path, perm, what) in [
            (default_runtime_dir(), "x", "traverse"),
            (pipewire_socket_path.clone(), "rw", "connect to"),
            (wayland_socket_path.clone(), "rw", "connect to"),
        ] {
            match tokio::process::Command::new("setfacl").args(["-m", &format!("u:{username}:{perm}"), &path]).output().await {
                Ok(output) if output.status.success() => tracing::info!("granted {username} {what} access on {path}"),
                Ok(output) => tracing::warn!(
                    "setfacl granting {username} {what} access on {path} exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                ),
                Err(e) => tracing::warn!("failed to run setfacl granting {username} {what} access on {path}: {e}"),
            }
        }

        let kwin_path = which_kwin_wayland().unwrap_or_else(|| "kwin_wayland".to_string());
        // Clearing FD_CLOEXEC here, directly in the broker's own
        // already-running process (not inside a fork), is safe — the
        // hazard is specifically about calling into non-async-signal-safe
        // code (like PAM below) *after* fork() in a multi-threaded
        // process, not about this single, simple syscall.
        let flags = unsafe { libc::fcntl(wayland_fd, libc::F_GETFD) };
        if flags == -1 {
            return Err(format!("fcntl F_GETFD on {wayland_fd} failed: {}", std::io::Error::last_os_error()));
        }
        if unsafe { libc::fcntl(wayland_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1 {
            return Err(format!("fcntl F_SETFD on {wayland_fd} failed: {}", std::io::Error::last_os_error()));
        }

        ensure_pam_session_service()?;
        let session_init_path = session_init_path()?;
        let mut cmd = tokio::process::Command::new(&session_init_path);
        cmd.arg(username)
            .arg("--")
            .arg("dbus-run-session")
            .arg("--")
            .arg(&kwin_path)
            .arg("--virtual")
            .arg("--width")
            .arg(width.to_string())
            .arg("--height")
            .arg(height.to_string())
            .arg("--scale")
            .arg("1")
            .arg("--no-lockscreen")
            .arg("--wayland-fd")
            .arg(wayland_fd.to_string())
            .arg("--socket")
            .arg(socket_name)
            .arg("--xwayland");
        if !payload.is_empty() {
            let session_script_path = write_session_script(&runtime_dir, socket_name, &pipewire_socket_path, payload)?;
            cmd.arg("--exit-with-session").arg(session_script_path);
        }

        // Deliberately env_clear()+explicit envs, not inherited from the
        // broker's own environment — same set as spawn_via_systemd's
        // Environment= lines, just built directly rather than templated
        // into a unit file string.
        cmd.env_clear()
            .env("HOME", &home_dir)
            .env("USER", username)
            .env("LOGNAME", username)
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/bin")
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("PIPEWIRE_REMOTE", &pipewire_socket_path)
            .env("KWIN_PLATFORM", "virtual")
            .env("KWIN_WAYLAND_NO_PERMISSION_CHECKS", "1")
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .env("XDG_SESSION_TYPE", "wayland")
            .env("XDG_CURRENT_DESKTOP", "KDE")
            .env("DESKTOP_SESSION", "plasma")
            .env("KDE_FULL_SESSION", "true")
            .env("KDE_SESSION_VERSION", "6")
            .env("XDG_DATA_DIRS", "/usr/local/share:/usr/share")
            .env("XDG_CONFIG_DIRS", "/etc/xdg")
            .env("XDG_MENU_PREFIX", "plasma-")
            .current_dir(&home_dir)
            // Inherits the broker's own stdout/stderr, same as spawn_fake —
            // the integration test captures the broker's piped output, so
            // this is what actually makes this session's output visible to
            // it at all (unlike the systemd path, there's no journald unit
            // to follow here).
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        let child = cmd.spawn().map_err(|e| format!("failed to spawn {session_init_path:?}: {e}"))?;
        // The child inherited its own copy of the listener's fd across
        // fork(); our copy can close now without affecting that.
        drop(listener);

        self.active.lock().unwrap().insert(session_id.to_string(), ActiveSession::DirectPam { child });
        Ok(wayland_socket_path)
    }

    /// Grants `username` access to a socket/runtime dir the *caller*
    /// already created and owns (e.g. redfog-moonlight embedding a
    /// `gst-wayland-display` pipeline directly in its own process), then
    /// spawns `argv` (with `env` applied) as that user pointed at it — the
    /// "just grant + spawn" counterpart to `spawn_via_pam`'s "create
    /// everything, including the compositor, then spawn". See
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
        let (_uid, _gid, home_dir) = resolve_user(username).await?;

        // Unlike spawn_via_pam's runtime dir (which the broker creates and
        // chowns fully to the target user), this one is owned by the
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

        ensure_pam_session_service()?;
        let session_init_path = session_init_path()?;
        let mut cmd = tokio::process::Command::new(&session_init_path);
        cmd.arg(username).arg("--").args(argv);
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

        let child = cmd.spawn().map_err(|e| format!("failed to spawn {session_init_path:?}: {e}"))?;
        self.active.lock().unwrap().insert(session_id.to_string(), ActiveSession::Payload { child });
        Ok(())
    }

    /// Backing implementation for `BrokerRequest::ReadUserSessionConfig` —
    /// see its doc comment for why only the broker can do this (root reads
    /// past normal `700` home-directory permissions; `resolve_user` is the
    /// same helper `spawn_via_pam`/`spawn_payload` already use). `Ok(None)`
    /// for a missing file is the expected, common case (most users won't
    /// have created one), not an error.
    pub async fn read_user_session_config(&self, username: &str) -> Result<Option<redfog_broker_protocol::UserSessionConfig>, String> {
        let (_uid, _gid, home_dir) = resolve_user(username).await?;
        let path = std::path::Path::new(&home_dir).join(".config/redfog/session.toml");
        let contents = match tokio::fs::read_to_string(&path).await {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("failed to read {path:?}: {e}")),
        };
        toml::from_str(&contents).map(Some).map_err(|e| format!("failed to parse {path:?}: {e}"))
    }

    pub async fn terminate(&self, session_id: &str) -> Result<(), String> {
        let session = self
            .active
            .lock()
            .unwrap()
            .remove(session_id)
            .ok_or_else(|| format!("no active session {session_id}"))?;

        match session {
            ActiveSession::DirectChild { mut child }
            | ActiveSession::DirectPam { mut child }
            | ActiveSession::Payload { mut child } => {
                let _ = child.kill().await;
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

fn current_username() -> Result<String, String> {
    std::env::var("USER").map_err(|e| e.to_string())
}

/// Looks up `username`'s home directory via NSS (`getent passwd`), rather
/// than assuming `/home/{username}` or relying on systemd's `%h` specifier
/// — confirmed live that `%h` in a *system* unit's `WorkingDirectory=`
/// resolves against the service manager's own context (root), not
/// `User=`, landing new sessions in `/root` instead of the target user's
/// actual home.
async fn home_dir_for(username: &str) -> Result<String, String> {
    Ok(resolve_user(username).await?.2)
}

/// Looks up `username`'s uid/gid/home directory via NSS (`getent passwd`) —
/// used by `spawn_via_pam`'s direct `setuid`/`setgid` privilege drop, which
/// (unlike `spawn_via_systemd`'s `User=` directive) has to resolve these
/// itself rather than letting systemd do it.
async fn resolve_user(username: &str) -> Result<(u32, u32, String), String> {
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
    let (Some(uid), Some(gid), Some(home)) = (fields.get(2), fields.get(3), fields.get(5)) else {
        return Err(format!("could not parse getent passwd {username} output: {line:?}"));
    };
    if home.is_empty() {
        return Err(format!("empty home directory in getent passwd {username} output: {line:?}"));
    }
    let uid: u32 = uid.parse().map_err(|e| format!("invalid uid in getent passwd {username} output: {e}"))?;
    let gid: u32 = gid.parse().map_err(|e| format!("invalid gid in getent passwd {username} output: {e}"))?;
    Ok((uid, gid, home.to_string()))
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
fn write_session_script(runtime_dir: &str, socket_name: &str, pipewire_socket_path: &str, payload: &[String]) -> Result<String, String> {
    fn shell_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
    let payload_cmd = payload.iter().map(|arg| shell_quote(arg)).collect::<Vec<_>>().join(" ");
    let session_script = format!(
        "#!/bin/sh\n\
         dbus-update-activation-environment --systemd WAYLAND_DISPLAY={socket_name} XDG_RUNTIME_DIR={runtime_dir} PIPEWIRE_REMOTE={pipewire_socket_path}\n\
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

/// Dedicated PAM service name for `spawn_via_pam`'s session-only PAM
/// interaction — deliberately *not* reusing "system-auth" (the real
/// credential check already happened earlier, via a separate
/// `pam::Client` in `auth.rs`, before `spawn_via_pam` is ever called) or
/// `"systemd-user"` (that name is systemd's own, for its own internal use).
/// `auth`/`account` are `pam_permit.so` (unconditionally succeed — no
/// password to check here, this Client only exists to open a session), and
/// `session` includes `pam_systemd.so` for real logind session
/// registration. Modeled directly on `/etc/pam.d/sddm-greeter` (same
/// "used only to open a session, not to authenticate" shape) and confirmed
/// against how Chrome Remote Desktop's own `chrome-remote-desktop@.service`
/// unit uses `PAMName=` for the same kind of purpose.
const PAM_SESSION_SERVICE: &str = "redfog-session";

/// Writes `/etc/pam.d/redfog-session` if it doesn't already exist. A
/// one-time, static system config file (unlike the per-session systemd
/// units this path replaces) — safe to check/create on every call.
fn ensure_pam_session_service() -> Result<(), String> {
    let path = format!("/etc/pam.d/{PAM_SESSION_SERVICE}");
    if std::path::Path::new(&path).exists() {
        return Ok(());
    }
    tracing::info!("creating {path} (one-time PAM service config for spawn_via_pam)");
    std::fs::write(
        &path,
        "#%PAM-1.0\n\
         auth        required    pam_permit.so\n\
         account     required    pam_permit.so\n\
         password    required    pam_deny.so\n\
         session     required    pam_systemd.so\n",
    )
    .map_err(|e| format!("failed to write {path}: {e}"))
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

fn default_runtime_dir() -> String {
    std::env::var("REDFOG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp/redfog-runtime".to_string())
}
