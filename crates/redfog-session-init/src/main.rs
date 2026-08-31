//! Small, dedicated helper: drops privileges (the correct way: `initgroups`
//! from `/etc/group`, then `setgid`, then `setuid`), then execs into a given
//! command. Exists as a *separate binary* (rather than logic inlined into
//! `redfog-broker`'s own `pre_exec` closure) specifically to avoid a
//! classic hazard: the syscalls involved were originally paired with real
//! PAM calls here (`pam_systemd.so`, allocating memory, etc.), which are
//! not async-signal-safe — running them in a `fork()`ed child of a
//! multi-threaded process (redfog-broker's own tokio runtime) silently
//! deadlocked, confirmed live, when this logic ran directly in its own
//! `pre_exec` closure. A freshly `exec`'d process is single-threaded from
//! the start, so kept as a separate binary even after PAM itself was
//! removed from here (see below) — the privilege-drop syscalls alone are
//! still simplest to reason about pre-`exec`, not post-`fork`.
//!
//! Usage: `redfog-session-init <username> -- <command> [args...]`
//!
//! Used to optionally open a real PAM session first (`pam_systemd.so`,
//! registering a genuine logind session) — removed after confirming live
//! that doing so moves the whole process tree into its own separate
//! `pam_systemd`-created `session-N.scope`, *outside* whatever systemd
//! unit/cgroup the caller spawned it in. That broke the caller's own
//! cgroup-based cleanup: `terminate()`'s unit-stop only ever reached this
//! process itself (before its own `exec()` below), while `kwin_wayland`
//! (already reparented into the escaped scope) leaked forever, every
//! single session end. Nothing that calls this binary actually needed the
//! real PAM session for its own sake — just the correct privilege drop —
//! so it's gone rather than worked around.
//!
//! The broker is responsible for clearing `FD_CLOEXEC` on any file
//! descriptor it wants to survive through to `<command>` *before* spawning
//! this helper (a plain fcntl() call in the broker's own already-running
//! process, not inside a fork — safe) — once cleared, the fd survives this
//! process's own subsequent `execvp()` same as any other inherited fd, no
//! special handling needed here.

use std::ffi::CString;
use std::os::unix::process::CommandExt;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: redfog-session-init <username> -- <command> [args...]";
    if args.len() < 3 || args[1] != "--" {
        eprintln!("{usage}");
        std::process::exit(2);
    }
    let username = &args[0];
    let (command, command_args) = args[2..].split_first().expect("checked len above");

    let (uid, gid) = resolve_user(username).unwrap_or_else(|e| {
        eprintln!("redfog-session-init: {e}");
        std::process::exit(1);
    });

    // initgroups populates the target user's REAL supplementary groups
    // (video, audio, input, etc.) from /etc/group — without this, the
    // process keeps root's (the broker's) supplementary group list, which
    // is what caused konsole/Steam to go missing from the taskbar again
    // (KSycoca-dependent resolution needs the real group membership, e.g.
    // for XDG data dir access). Must run before setgid/setuid, while still
    // privileged enough to call it.
    let username_c = CString::new(username.as_str()).unwrap_or_else(|e| {
        eprintln!("redfog-session-init: username contains NUL: {e}");
        std::process::exit(1);
    });
    if let Err(e) = nix::unistd::initgroups(&username_c, nix::unistd::Gid::from_raw(gid)) {
        eprintln!("redfog-session-init: initgroups failed: {e}");
        std::process::exit(1);
    }

    // Order matters: gid before uid (changing gid needs to still be privileged).
    if let Err(e) = nix::unistd::setgid(nix::unistd::Gid::from_raw(gid)) {
        eprintln!("redfog-session-init: setgid failed: {e}");
        std::process::exit(1);
    }
    if let Err(e) = nix::unistd::setuid(nix::unistd::Uid::from_raw(uid)) {
        eprintln!("redfog-session-init: setuid failed: {e}");
        std::process::exit(1);
    }

    let err = std::process::Command::new(command).args(command_args).exec();
    eprintln!("redfog-session-init: failed to exec {command}: {err}");
    std::process::exit(1);
}

fn resolve_user(username: &str) -> Result<(u32, u32), String> {
    let output = std::process::Command::new("getent")
        .args(["passwd", username])
        .output()
        .map_err(|e| format!("failed to run getent passwd {username}: {e}"))?;
    if !output.status.success() {
        return Err(format!("getent passwd {username} exited with {}", output.status));
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<&str> = line.trim().split(':').collect();
    let (Some(uid), Some(gid)) = (fields.get(2), fields.get(3)) else {
        return Err(format!("could not parse getent passwd {username} output: {line:?}"));
    };
    let uid: u32 = uid.parse().map_err(|e| format!("invalid uid in getent passwd {username} output: {e}"))?;
    let gid: u32 = gid.parse().map_err(|e| format!("invalid gid in getent passwd {username} output: {e}"))?;
    Ok((uid, gid))
}
