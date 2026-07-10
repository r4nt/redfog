//! Small, dedicated helper: opens a real PAM session for a target user,
//! drops privileges, then execs into a given command. Exists as a
//! *separate binary* (rather than logic inlined into `redfog-broker`'s own
//! `pre_exec` closure) specifically to avoid a classic hazard: PAM calls
//! (loading `pam_systemd.so`, allocating memory, etc.) are not
//! async-signal-safe, and running them in a `fork()`ed child of a
//! multi-threaded process (redfog-broker's own tokio runtime) can silently
//! deadlock — confirmed live, the broker hung with zero output when this
//! logic ran directly in its own `pre_exec` closure. A freshly `exec`'d
//! process is single-threaded from the start, so none of that applies here.
//!
//! Usage: `redfog-session-init <username> -- <command> [args...]`
//!
//! The broker is responsible for clearing `FD_CLOEXEC` on any file
//! descriptor it wants to survive through to `<command>` *before* spawning
//! this helper (a plain fcntl() call in the broker's own already-running
//! process, not inside a fork — safe) — once cleared, the fd survives this
//! process's own subsequent `execvp()` same as any other inherited fd, no
//! special handling needed here.

use std::ffi::CString;
use std::os::unix::process::CommandExt;

/// Dedicated PAM service name — see redfog-broker's session.rs for why
/// this isn't "system-auth" (real credential check already happened
/// earlier, separately) or "systemd-user" (systemd's own internal use).
const PAM_SESSION_SERVICE: &str = "redfog-session";

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

    if let Err(e) = open_pam_session(username) {
        eprintln!("redfog-session-init: {e}");
        std::process::exit(1);
    }

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

/// Opens (and deliberately never closes — see redfog-broker's session.rs
/// for the known limitation this shares) a real PAM session: registers a
/// genuine logind session (via `pam_systemd.so` in `/etc/pam.d/redfog-session`),
/// unlike a plain systemd `User=` uid switch. `auth`/`account` are
/// `pam_permit.so` in that service file — no real credential check
/// happens here, that already happened earlier via a separate PAM
/// interaction; this call exists only to open the session.
fn open_pam_session(username: &str) -> Result<(), String> {
    let mut client = pam::Client::with_password(PAM_SESSION_SERVICE).map_err(|e| format!("pam init failed: {e}"))?;
    client.conversation_mut().set_credentials(username, "");
    client
        .authenticate()
        .map_err(|e| format!("pam authenticate (session-only, pam_permit.so) failed: {e}"))?;
    client.open_session().map_err(|e| format!("pam open_session failed: {e}"))?;
    // This process is about to exec() into the real session command
    // anyway, discarding this Client either way — std::mem::forget just
    // skips Client's own Drop (which would try close_session()) rather
    // than let it attempt a pointless one immediately before that happens.
    std::mem::forget(client);
    Ok(())
}
