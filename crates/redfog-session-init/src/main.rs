//! Small, dedicated helper: opens a real PAM session (auth + `pam_open_
//! session`, registering a genuine logind session with `pam_systemd.so`,
//! and unlocking KWallet via `pam_kwallet5.so` if configured in the PAM
//! service used — see `/etc/pam.d/redfog-session`), drops privileges (the
//! correct way: `initgroups` from `/etc/group`, then `setgid`, then
//! `setuid`), then execs into a given command. Exists as a *separate
//! binary* (rather than logic inlined into `redfog-broker`'s own
//! `pre_exec` closure) specifically to avoid a classic hazard: PAM calls
//! are not async-signal-safe — running them in a `fork()`ed child of a
//! multi-threaded process (redfog-broker's own tokio runtime) silently
//! deadlocked, confirmed live, when this logic ran directly in its own
//! `pre_exec` closure. A freshly `exec`'d process is single-threaded from
//! the start, so this stays a separate binary.
//!
//! Usage: `redfog-session-init <username> -- <command> [args...]`
//!
//! The real PAM session *was* removed from here once already (see git
//! history around commit `79141f4`): the earlier attempt migrated the
//! whole process tree into `pam_systemd`'s own new `session-N.scope`,
//! escaping whatever unit/cgroup the caller had spawned it in, which broke
//! that caller's cgroup-based `terminate()`. This version's caller
//! (`spawn_via_pam` in `redfog-broker/src/session.rs`) is built around
//! that escape instead of fighting it: it reads the *real* logind session
//! id this process reports back (see below) and terminates via
//! `loginctl kill-session`, not a cgroup-based kill.
//!
//! ## Why this process forks instead of exec'ing straight through
//!
//! PAM's own session bookkeeping (what `pam_close_session()` needs to
//! correctly tell logind "this session is over") is stored as private data
//! on the *specific* `PamHandle` `pam_open_session()` was called on — not
//! retrievable any other way, and not something a *different*, later PAM
//! transaction (even against the same service/username) can look up by
//! session id. **Confirmed live**: a first version of this file opened the
//! session and immediately `exec()`'d away, same as the privilege-drop-only
//! version before it — `loginctl kill-session <id> --kill-whom=all`
//! reliably killed every process (confirmed via `Tasks: 0` on the scope),
//! but the session itself stayed listed in `loginctl list-sessions`
//! forever, stuck `State=closing`/`Active: active (abandoned)` — because
//! nothing had ever called `pam_close_session()` for it. Calling logind's
//! own `ReleaseSession()` D-Bus method directly (what `pam_close_session()`
//! calls internally) isn't a fix either — its own docs
//! (`org.freedesktop.login1(5)`) say it "should never be invoked directly
//! by clients... exclusively the job of PAM and its pam_systemd(8) module".
//!
//! So: after `pam_open_session()` succeeds, this process `fork()`s (safe
//! here specifically because it's single-threaded and has made no more PAM
//! calls since — the same async-signal-safety reasoning that keeps this a
//! separate exec'd binary in the first place). The **child** continues
//! exactly as before (privilege drop, `exec()` into the real payload) and
//! never touches PAM again. The **parent** keeps the original `PamHandle`
//! alive and does nothing else but wait — either for the child to exit on
//! its own, or for `redfog-broker`'s `terminate()` to ask it to close early
//! (`systemctl kill --kill-who=main --signal=SIGTERM
//! redfog-session-<id>.service`, which reaches the parent specifically
//! since it's still the unit's tracked main PID — it never replaced itself
//! via `exec()`) — then calls `pam_close_session()`/`pam_setcred(Delete)`/
//! `pam_end()` and, as a final backstop, kills whatever's left of the tree
//! via the scope unit directly (`systemctl kill --kill-who=all
//! session-<id>.scope`, not `loginctl kill-session <id>`: that depends on
//! logind still considering the session "known", which the
//! `pam_close_session()` call just above may have already ended, whereas
//! the systemd scope *unit* persists independently of that until its
//! cgroup is actually empty).
//!
//! ## Why the low-level `pam::functions`/`pam::ffi` API, not `pam::Client`
//!
//! `pam::Client` (the crate's high-level wrapper, used by
//! `redfog-broker/src/auth.rs`'s own `Authenticate` check) has no accessor
//! for its own (private) `PamHandle` field at all — which both the
//! fork/parent design above and the `PAM_KWALLET5_LOGIN` propagation below
//! need direct access to. Its own `initialize_environment` also only ever
//! sets five fixed vars (`HOME`/`USER`/`LOGNAME`/`SHELL`/`PWD`, via an
//! independent NSS lookup, not from PAM's own env list at all) — set
//! directly by this file instead, for the same reason.
//!
//! That env gap matters here specifically: **confirmed live** that PAM/
//! logind session-open alone isn't enough to stop KWallet's interactive
//! prompt (login worked; KWallet still prompted). Root-caused via
//! `strings /usr/lib/security/pam_kwallet5.so` and reading
//! `/usr/lib/pam_kwallet_init` (from the `kwallet-pam` package): the `auth`
//! phase captures the password into a small local holding socket and
//! exports its path as the PAM env var `PAM_KWALLET5_LOGIN` (via
//! `pam_putenv`, *inside* the PAM transaction) — but nothing later in a
//! normal desktop session gets that password out of the socket except
//! `pam_kwallet_init` itself (`env | socat STDIN UNIX-CONNECT:
//! $PAM_KWALLET5_LOGIN`), which needs `PAM_KWALLET5_LOGIN` present in its
//! *own* process environment to find that socket at all. Two gaps, both
//! fixed below:
//! 1. `PAM_KWALLET5_LOGIN` never reached this process's real environment
//!    in the first place — fixed by calling `pam::getenv(handle,
//!    "PAM_KWALLET5_LOGIN")` directly on the handle and applying it via
//!    `std::env::set_var`. Deliberately *not* `pam::getenvlist()` (a bulk
//!    dump of every PAM-internal var, which would've been the more
//!    obviously "complete" fix): **confirmed live** that its underlying
//!    parser has a real off-by-one bug (the crate's own source has a
//!    `// FIXME: implement this properly` on it) that includes the `=`
//!    separator in the parsed *key* — `std::env::set_var` on a name
//!    containing `=` panics outright, which took this whole process down
//!    the first time this was tried with the bulk API. `getenv` (one named
//!    var, no `=`-splitting involved at all) sidesteps that bug entirely.
//! 2. Nothing in a redfog session's own compositor bring-up would ever
//!    *run* `pam_kwallet_init` even with that env var present — a real KDE
//!    desktop login relies on either a `systemd --user` unit
//!    (`plasma-kwallet-pam.service`) or KDE's own XDG-autostart-phase
//!    session-startup machinery (`startplasma-wayland`) to invoke it,
//!    neither of which redfog's minimal `kwin_wayland --exit-with-session
//!    <payload>` bring-up ever goes through. Fixed in `write_session_
//!    script` (`redfog-broker/src/session.rs`): the generated wrapper
//!    script now runs `/usr/lib/pam_kwallet_init` itself, right before
//!    `exec`ing the real payload — same hook point that script's existing
//!    `dbus-update-activation-environment` call already uses.
//!
//! The low-level API needs its own PAM conversation callback (`pam::
//! Client`'s own `into_pam_conv`/`converse` internals aren't public) —
//! `converse` below is a direct, faithful copy of the `pam` crate's own
//! (private) implementation, not new logic: every other call
//! (`pam::start`/`authenticate`/`acct_mgmt`/`setcred`/`open_session`/
//! `close_session`/`getenv`/`end`) is the crate's own public, safe wrapper
//! around the matching libpam function.
//!
//! ## Password
//!
//! The target user's plaintext password is needed to open a real PAM
//! session (`pam_kwallet5.so`'s session hook can only unlock with a
//! password its own `auth` hook captured moments earlier on the *same*
//! PAM transaction). Never passed via argv (readable by any local user via
//! `/proc/<pid>/cmdline`) and never written to a file on either end either:
//! `spawn_via_pam` (`redfog-broker/src/session.rs`) uses systemd's own
//! `LoadCredential=` unit directive, pointed at an `AF_UNIX` *stream
//! socket* the broker binds rather than a plain file path — systemd itself
//! connects to that socket once, at process invocation, and reads the
//! password straight off the connection. This process just reads the
//! result back out of systemd's own credential store,
//! `$CREDENTIALS_DIRECTORY/password` — a location systemd itself backs
//! with non-swappable memory where possible, exposes read-only, and tears
//! down automatically when this unit stops (crash or not), so there's
//! nothing for this process to unlink itself either.
//!
//! ## Reporting the logind session id back
//!
//! Once `pam_open_session` succeeds, this process's cgroup has already
//! migrated into a real `session-<id>.scope` the broker doesn't otherwise
//! know the id of. Written to `REDFOG_SESSION_ID_REPORT_FILE` (inside the
//! same `runtime_dir`, so no fd-passing needed here either) — the broker
//! polls for that file the same way it already polls for the Wayland/
//! PipeWire sockets to appear elsewhere in this codebase.

use std::ffi::CString;
use std::os::raw::{c_int, c_void};
use std::os::unix::process::CommandExt;

use nix::unistd::ForkResult;

/// See this module's own doc comment: real auth *and* session-open on the
/// same PAM handle, required for `pam_kwallet5.so` to work at all.
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

    let mut password = read_password_credential().unwrap_or_else(|e| {
        eprintln!("redfog-session-init: {e}");
        std::process::exit(1);
    });

    let (uid, gid, home_dir, shell) = resolve_user(username).unwrap_or_else(|e| {
        eprintln!("redfog-session-init: {e}");
        std::process::exit(1);
    });

    let (handle, logind_session_id) = open_pam_session(username, &password).unwrap_or_else(|e| {
        eprintln!("redfog-session-init: {e}");
        std::process::exit(1);
    });
    // Explicitly zeroed rather than just dropped -- a `String`'s heap
    // buffer isn't otherwise guaranteed to be cleared before the
    // allocator reuses it.
    zero_string(&mut password);

    report_logind_session_id(&logind_session_id).unwrap_or_else(|e| {
        eprintln!("redfog-session-init: {e}");
        std::process::exit(1);
    });

    // SAFETY: single-threaded at this point (no PAM calls, no additional
    // threads spawned since main() started), which is exactly the
    // condition that makes fork() here safe -- see this module's own doc
    // comment for why the parent/child split exists at all.
    match unsafe { nix::unistd::fork() } {
        Err(e) => {
            eprintln!("redfog-session-init: fork failed: {e}");
            std::process::exit(1);
        }
        Ok(ForkResult::Parent { child }) => {
            wait_for_termination(child);
            close_pam_session_and_cleanup(handle, &logind_session_id);
            std::process::exit(0);
        }
        Ok(ForkResult::Child) => {
            // Falls through to the rest of main() below -- only the child
            // continues past this match.
        }
    }

    // No `Client::initialize_environment` equivalent running anymore (see
    // this module's own doc comment for why the low-level API is used
    // instead) -- set these directly ourselves, same values a real login
    // would set them to, via the same NSS lookup `resolve_user` already
    // did above.
    std::env::set_var("HOME", &home_dir);
    std::env::set_var("PWD", &home_dir);
    std::env::set_var("USER", username);
    std::env::set_var("LOGNAME", username);
    std::env::set_var("SHELL", &shell);

    // `WorkingDirectory=` used to be a systemd unit-file property; there's
    // no unit file for this anymore (see `spawn_via_pam`'s own doc
    // comment), so set the process's actual cwd directly instead — while
    // still root, same as the rest of this pre-privilege-drop setup. The
    // `PWD` env var set above alone doesn't move a child's real working
    // directory; anything that calls `getcwd()` instead of reading `$PWD`
    // needs this too.
    if let Err(e) = std::env::set_current_dir(&home_dir) {
        eprintln!("redfog-session-init: failed to chdir to {home_dir}: {e}");
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

/// Blocks until either `child` (the top of the real compositor tree, about
/// to privilege-drop and `exec()`) exits on its own, or this process
/// receives `SIGTERM` — `redfog-broker`'s `terminate()` sends that
/// specifically to *this* (the parent's) pid via `systemctl kill
/// --kill-who=main` to ask for an early, graceful close. Either way,
/// there's nothing more to wait for once this returns; the caller does the
/// actual PAM-close/cleanup work next, in normal (non-signal-handler)
/// context — see this module's own doc comment for why that split
/// matters (PAM calls aren't async-signal-safe).
fn wait_for_termination(child: nix::unistd::Pid) {
    use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
    use nix::sys::wait::waitpid;

    extern "C" fn noop_handler(_: c_int) {}
    // SA_RESTART deliberately NOT set (SaFlags::empty()): the whole point
    // of installing this handler is for the blocking waitpid() below to be
    // interrupted (EINTR) the instant SIGTERM arrives, rather than
    // silently auto-retried — that's the actual signal, not the handler
    // itself doing anything (it can't: PAM calls aren't async-signal-safe,
    // so the real cleanup has to happen after this function returns).
    let action = SigAction::new(SigHandler::Handler(noop_handler), SaFlags::empty(), SigSet::empty());
    unsafe {
        let _ = sigaction(Signal::SIGTERM, &action);
    }
    let _ = waitpid(child, None);
}

/// Real teardown for the PAM session this process opened — see this
/// module's own doc comment for why this has to run in the *original*
/// process that called `pam_open_session()` (this one), using the *same*
/// handle, and why `systemctl kill` against the scope unit directly is
/// used for the final process-tree sweep instead of `loginctl
/// kill-session <id>`.
fn close_pam_session_and_cleanup(handle: &mut pam::PamHandle, logind_session_id: &str) {
    let _ = pam::close_session(handle, false);
    let _ = pam::setcred(handle, pam::PamFlag::Delete_Cred);
    let _ = pam::end(handle, pam::PamReturnCode::Success);
    let _ = std::process::Command::new("systemctl")
        .args(["kill", "--kill-who=all", "--signal=SIGKILL", &format!("session-{logind_session_id}.scope")])
        .status();
}

fn zero_string(s: &mut String) {
    // SAFETY: overwriting valid UTF-8 bytes with a single ASCII byte (0)
    // keeps the buffer valid UTF-8 (all zero bytes), and we never read `s`
    // as text again afterward.
    unsafe {
        for b in s.as_bytes_mut() {
            *b = 0;
        }
    }
    s.clear();
}

/// Reads the password back out of systemd's own credential store — see
/// this module's own doc comment for why it ends up there
/// (`LoadCredential=password:<socket-path>` on the unit, systemd itself
/// having connected to the broker's socket and streamed it in). No unlink
/// needed: `$CREDENTIALS_DIRECTORY` is systemd-managed and torn down
/// automatically when this unit stops, crash or not.
fn read_password_credential() -> Result<String, String> {
    let dir = std::env::var("CREDENTIALS_DIRECTORY")
        .map_err(|_| "CREDENTIALS_DIRECTORY not set -- was LoadCredential=password:... dropped from the unit?".to_string())?;
    let path = format!("{dir}/password");
    std::fs::read_to_string(&path).map_err(|e| format!("failed to read password credential {path}: {e}"))
}

/// Login+password answers for the PAM conversation callback below — a
/// small stand-in for `pam::conv::PasswordConv` (not constructible from
/// outside the `pam` crate: its `new()` is `pub(crate)`).
struct PasswordAnswers {
    username: CString,
    password: CString,
}

/// Direct, faithful copy of `pam::conv::converse::<C>` for a fixed
/// `PasswordAnswers` responder instead of a generic `Conversation` impl —
/// see this module's own doc comment for why this can't just call the
/// crate's own (private) version. Reads `appdata_ptr` back as
/// `&PasswordAnswers` (leaked for the life of this process — see
/// `open_pam_session` — so it's guaranteed to still be valid for as long
/// as `handle` itself is, including the parent's much-later `close_session`
/// call) and answers every echo-on/echo-off prompt with the fixed
/// username/password respectively — this process only ever drives one,
/// non-interactive, fully-scripted PAM transaction, so nothing here needs
/// to vary per-message the way a real interactive prompt would.
unsafe extern "C" fn converse(
    num_msg: c_int,
    msg: *mut *const pam::PamMessage,
    out_resp: *mut *mut pam::PamResponse,
    appdata_ptr: *mut c_void,
) -> c_int {
    let resp = libc::calloc(num_msg as usize, std::mem::size_of::<pam::PamResponse>()) as *mut pam::PamResponse;
    if resp.is_null() {
        return pam::PamReturnCode::Buf_Err as c_int;
    }
    let answers = &*(appdata_ptr as *const PasswordAnswers);
    for i in 0..num_msg as isize {
        let m: &pam::PamMessage = &*(*msg.offset(i));
        let r: &mut pam::PamResponse = &mut *resp.offset(i);
        match pam::PamMessageStyle::from(m.msg_style) {
            pam::PamMessageStyle::Prompt_Echo_On => {
                r.resp = libc::strdup(answers.username.as_ptr());
            }
            pam::PamMessageStyle::Prompt_Echo_Off => {
                r.resp = libc::strdup(answers.password.as_ptr());
            }
            // Nothing interactive is watching for info/error text messages
            // (e.g. pam_faillock's lockout notices) -- leave that response
            // slot null, same as upstream's converse() does when a handler
            // declines to answer.
            pam::PamMessageStyle::Text_Info | pam::PamMessageStyle::Error_Msg => {}
        }
    }
    *out_resp = resp;
    pam::PamReturnCode::Success as c_int
}

/// Real auth (`pam_authenticate`/`pam_acct_mgmt`) followed by a real
/// session open (`pam_setcred(Establish)` + `pam_open_session` +
/// `pam_setcred(Reinitialize)`) against the dedicated `redfog-session` PAM
/// service (`/etc/pam.d/redfog-session`, written once by the broker at
/// startup — see `ensure_pam_session_service` in `redfog-broker`), which
/// includes `pam_kwallet5.so` in both the `auth` and `session` phases so
/// KWallet auto-unlocks the same way it does on a normal desktop login.
/// See this module's own doc comment for why this drives the low-level
/// `pam::functions` API directly instead of `pam::Client`, and pulls
/// `pam_kwallet5.so`'s `PAM_KWALLET5_LOGIN` out via a single targeted
/// `pam::getenv` call (not the crate's buggy bulk `pam::getenvlist`) into
/// the real process environment.
///
/// Returns the `PamHandle` itself (`'static`, via `Box::leak` on the
/// conversation callback's own backing data — see `PasswordAnswers`'s own
/// doc comment for why that's sound and deliberate here) rather than
/// closing the session before returning: the *caller* decides when to
/// close it (immediately, in the child, right before `exec()` — no, wait,
/// actually never in the child; only the parent, once the whole session's
/// lifetime is over — see this module's own top-level doc comment for the
/// full fork/parent design this exists for).
fn open_pam_session(username: &str, password: &str) -> Result<(&'static mut pam::PamHandle, String), String> {
    let username_c = CString::new(username).map_err(|e| format!("username contains NUL: {e}"))?;
    let password_c = CString::new(password).map_err(|e| format!("password contains NUL: {e}"))?;
    // Leaked (not a local): the conversation callback may be invoked by
    // PAM at any point up until `pam_end()`, which — per this module's own
    // fork design — can happen much later, in the parent, long after this
    // function itself has returned. A plain local would be dangling by
    // then.
    let answers: &'static PasswordAnswers = Box::leak(Box::new(PasswordAnswers { username: username_c, password: password_c }));
    let conv: &'static pam::ffi::pam_conv =
        Box::leak(Box::new(pam::ffi::pam_conv { conv: Some(converse), appdata_ptr: answers as *const PasswordAnswers as *mut c_void }));

    // `user: None` (not `Some(username)`), matching `redfog-broker/src/
    // auth.rs`'s already-proven-working `Client::with_password` pattern —
    // the conversation callback above answers the echo-on (username)
    // prompt itself, same as `set_credentials` does for `Client`.
    let handle = pam::start(PAM_SESSION_SERVICE, None, conv).map_err(|e| format!("pam_start failed (missing /etc/pam.d/{PAM_SESSION_SERVICE}?): {e}"))?;

    let code = pam::authenticate(handle, pam::PamFlag::None);
    if code != pam::PamReturnCode::Success {
        let _ = pam::end(handle, code);
        return Err(format!("pam_authenticate failed: {code:?}"));
    }
    let code = pam::acct_mgmt(handle, pam::PamFlag::None);
    if code != pam::PamReturnCode::Success {
        let _ = pam::end(handle, code);
        return Err(format!("pam_acct_mgmt failed: {code:?}"));
    }
    let code = pam::setcred(handle, pam::PamFlag::Establish_Cred);
    if code != pam::PamReturnCode::Success {
        let _ = pam::end(handle, code);
        return Err(format!("pam_setcred(Establish) failed: {code:?}"));
    }
    let code = pam::open_session(handle, false);
    if code != pam::PamReturnCode::Success {
        let _ = pam::setcred(handle, pam::PamFlag::Delete_Cred);
        let _ = pam::end(handle, code);
        return Err(format!("pam_open_session failed: {code:?}"));
    }
    // Follows openSSH's own convention (same as `pam::Client::open_session`
    // internally): pam_setcred before *and* after pam_open_session.
    let code = pam::setcred(handle, pam::PamFlag::Reinitialize_Cred);
    if code != pam::PamReturnCode::Success {
        let _ = pam::end(handle, code);
        return Err(format!("pam_setcred(Reinitialize) failed: {code:?}"));
    }

    // The actual fix the low-level API exists for: pull
    // `PAM_KWALLET5_LOGIN` (set by `pam_kwallet5.so`'s `auth` phase via
    // `pam_putenv`, inside this same transaction) out of PAM's internal
    // environment and apply it to this process's *real* environment — see
    // this module's own doc comment for why it's otherwise invisible to
    // anything downstream, and why `pam::getenv` (one named var) is used
    // instead of the crate's buggy `pam::getenvlist`.
    if let Ok(Some(login_socket)) = pam::getenv(handle, "PAM_KWALLET5_LOGIN") {
        std::env::set_var("PAM_KWALLET5_LOGIN", login_socket);
    }

    let session_id =
        logind_session_id().ok_or_else(|| "pam_open_session succeeded but no session-<id>.scope found in /proc/self/cgroup".to_string())?;
    Ok((handle, session_id))
}

/// Same parsing `redfog-broker`'s own `parse_logind_session_id` does
/// (duplicated rather than shared across the process boundary — this is a
/// tiny, pure, few-line helper, and this binary otherwise has no
/// dependency on the broker's own crate).
fn logind_session_id() -> Option<String> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    const MARKER: &str = "/session-";
    for line in cgroup.lines() {
        let idx = line.find(MARKER)?;
        let rest = &line[idx + MARKER.len()..];
        let segment = rest.split('/').next().unwrap_or(rest);
        let id = segment.strip_suffix(".scope").unwrap_or(segment);
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    None
}

fn report_logind_session_id(id: &str) -> Result<(), String> {
    let path = std::env::var("REDFOG_SESSION_ID_REPORT_FILE").map_err(|_| "REDFOG_SESSION_ID_REPORT_FILE not set".to_string())?;
    std::fs::write(&path, id).map_err(|e| format!("failed to write session id report {path}: {e}"))
}

fn resolve_user(username: &str) -> Result<(u32, u32, String, String), String> {
    let output = std::process::Command::new("getent")
        .args(["passwd", username])
        .output()
        .map_err(|e| format!("failed to run getent passwd {username}: {e}"))?;
    if !output.status.success() {
        return Err(format!("getent passwd {username} exited with {}", output.status));
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<&str> = line.trim().split(':').collect();
    let (Some(uid), Some(gid), Some(home), Some(shell)) = (fields.get(2), fields.get(3), fields.get(5), fields.get(6)) else {
        return Err(format!("could not parse getent passwd {username} output: {line:?}"));
    };
    let uid: u32 = uid.parse().map_err(|e| format!("invalid uid in getent passwd {username} output: {e}"))?;
    let gid: u32 = gid.parse().map_err(|e| format!("invalid gid in getent passwd {username} output: {e}"))?;
    if home.is_empty() {
        return Err(format!("empty home directory in getent passwd {username} output: {line:?}"));
    }
    Ok((uid, gid, home.to_string(), shell.to_string()))
}
