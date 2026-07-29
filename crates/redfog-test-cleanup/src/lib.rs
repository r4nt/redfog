//! Automatic child-process cleanup for integration tests, via a dedicated
//! process group plus a watchdog sibling — no shell wrapper required, just a
//! dev-dependency plus one call to [`ensure_active`] at the top of each
//! relevant test file.
//!
//! An earlier version of this crate used a re-exec into a fresh PID+user
//! namespace (kernel guarantees every process in a namespace dies when its
//! PID 1 dies, regardless of capabilities — unlike `PR_SET_PDEATHSIG`, which
//! is unconditionally cleared by the kernel at `execve()` time for any
//! binary with attached file capabilities, e.g. `kwin_wayland`'s
//! `cap_sys_nice=ep`, see `man 2 prctl`). That worked in isolation, but
//! confirmed live: `kwin_wayland` itself never completes startup inside such
//! a namespace (hangs forever in `ppoll()`, no Wayland socket ever created)
//! — reproduced with zero of this crate's code involved, via a bare
//! `unshare --user --pid --fork --map-current-user -- kwin_wayland ...`, so
//! it's a real incompatibility, not a bug here. Root cause not fully pinned
//! down (capabilities and DRM device access were both confirmed still fine
//! inside the namespace); not worth chasing further given the mechanism
//! below has none of this risk.
//!
//! Process groups don't touch namespaces, capabilities, or credentials at
//! all — spawned processes can't tell the difference. [`ensure_active`]
//! moves the calling test binary into its own new process group
//! (`setpgid(0, 0)`), so every process it (transitively) spawns afterwards
//! inherits that group unless it explicitly changes its own. A watchdog
//! sibling, forked at the same time, uses `pidfd_open` + `poll` to block
//! until the main process's *pid* actually exits, then does
//! `kill(-pgid, SIGKILL)`, taking down the whole group — including itself,
//! which is harmless, since it's about to exit anyway.
//!
//! A pidfd, not a pipe, is the key: an earlier version used a pipe whose
//! write end only the main process held (`O_CLOEXEC`, so spawned children
//! wouldn't inherit a copy), relying on `read()` returning EOF once every
//! holder of that end was gone. That broke the moment `redfog-core`'s own
//! `ensure_private_dbus_session()` did its own legitimate
//! `Command::new("dbus-run-session")...exec()` — an in-place re-exec of the
//! *same pid*, not a new process — which closed the `O_CLOEXEC` write end
//! right there, and the watchdog killed the whole group before the real
//! test had even started (confirmed live: log stopped right after
//! `running 1 test`, exit code 137). A pidfd tracks the pid's actual
//! lifetime across any number of `execve()` calls — it only becomes ready
//! when that pid truly terminates — so it isn't fooled by a process
//! re-exec'ing itself into a different program along the way.
//!
//! [`track_extra_pgid`]/[`untrack_extra_pgid`] cover one thing the default
//! group can't: a test that spawns a real child (e.g. `redfog-server`) into
//! its *own* separate process group via `.process_group(0)`, specifically
//! so its own `Drop` impl can `kill()` just that subtree without also
//! catching unrelated sibling test processes. That's fine for a clean pass
//! or a panic (unwind runs `Drop`), but not for the whole test binary
//! dying abnormally before `Drop` gets a chance to run — confirmed live:
//! `connection_integration.rs`'s tests do exactly this for `redfog-server`/
//! `redfog-broker`, and a real, orphaned `kwin_wayland` (two hops down,
//! under `redfog-server`) was left running hours later. The registration
//! is a plain directory of files named after each tracked pgid (created on
//! track, removed on untrack) rather than an in-memory list, because the
//! watchdog is a separate process forked long before any of this — it
//! can't see later updates to a Rust-level static the way the main process
//! can; it only needs a snapshot of whichever files still exist at the
//! moment it actually wakes up and acts.

use std::path::PathBuf;
use std::sync::{Once, OnceLock};

static EXTRA_PGID_DIR: OnceLock<PathBuf> = OnceLock::new();

fn extra_pgid_dir(pid: libc::pid_t) -> PathBuf {
    std::env::temp_dir().join(format!("redfog-test-cleanup-extra-{pid}"))
}

/// Register an additional process group (by its id — for a
/// `.process_group(0)`-spawned child, that's just its own pid) that the
/// watchdog should also `kill(-pgid, SIGKILL)` if the main test process
/// dies before its own cleanup runs. Call this right after spawning such a
/// child; call [`untrack_extra_pgid`] once it's been cleanly torn down (by
/// its own `Drop` impl, say), so a later, unrelated process that happens to
/// reuse the same pgid number doesn't get killed by mistake. No-op if
/// [`ensure_active`] never successfully installed.
pub fn track_extra_pgid(pgid: i32) {
    if let Some(dir) = EXTRA_PGID_DIR.get() {
        let _ = std::fs::write(dir.join(pgid.to_string()), b"");
    }
}

/// See [`track_extra_pgid`]. Best-effort; fine to call even if it was never
/// tracked (e.g. the child failed to spawn in the first place).
pub fn untrack_extra_pgid(pgid: i32) {
    if let Some(dir) = EXTRA_PGID_DIR.get() {
        let _ = std::fs::remove_file(dir.join(pgid.to_string()));
    }
}

/// Finds every direct child of the calling process whose `/proc/<pid>/comm`
/// matches `name`, and `SIGKILL`s each one along with everything
/// transitively parented under it. Precisely scoped via `/proc`'s `PPid:`
/// links, not a name-based `pkill` — never touches an unrelated process
/// that just happens to share the same binary name (e.g. a real desktop
/// session's own `kwin_wayland`).
///
/// For a test-spawned compositor like `kwin_wayland`, this is what
/// `redfog_core::CompositorSession::terminate`/`kill_best_effort` don't do:
/// those only ever signal the one child process they hold a `Child` handle
/// for, not whatever *it* went on to spawn (`Xwayland`, the `--exit-with-
/// session` payload) — confirmed live, killing just `kwin_wayland` left
/// `glxgears`/`Xwayland` running as orphans. Call this from a test's own
/// `Drop` guard right alongside (or instead of) `kill_best_effort()`.
pub fn kill_descendants_named(name: &str) {
    let my_pid = unsafe { libc::getpid() };
    for pid in direct_children_named(my_pid, name) {
        kill_tree(pid);
    }
}

fn direct_children_named(parent: libc::pid_t, name: &str) -> Vec<libc::pid_t> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else { return found };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<libc::pid_t>() else { continue };
        let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else { continue };
        if comm.trim() != name {
            continue;
        }
        if ppid_of(pid) == Some(parent) {
            found.push(pid);
        }
    }
    found
}

fn ppid_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status.lines().find_map(|l| l.strip_prefix("PPid:")).and_then(|v| v.trim().parse().ok())
}

fn kill_tree(pid: libc::pid_t) {
    // Gather the whole tree before killing anything -- killing a parent
    // first and then walking /proc for its children risks missing some if
    // it's already been reaped/reparented by the time we get to them.
    let mut all = vec![pid];
    let mut frontier = vec![pid];
    while let Some(p) = frontier.pop() {
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let Ok(child_pid) = entry.file_name().to_string_lossy().parse::<libc::pid_t>() else { continue };
                if ppid_of(child_pid) == Some(p) {
                    all.push(child_pid);
                    frontier.push(child_pid);
                }
            }
        }
    }
    for p in all.into_iter().rev() {
        unsafe { libc::kill(p, libc::SIGKILL) };
    }
}

/// Call this once, at the very top of `main` in any test binary that
/// spawns real subprocesses needing automatic cleanup — a `#[ctor]` inside
/// this crate already runs this automatically before any test code (Rust's
/// own test harness included) executes, but a real call site is still
/// needed so the linker doesn't strip this crate out as unused; the exact
/// place doesn't matter as long as it runs (the ctor already guarantees
/// *when*). Safe to call multiple times or from multiple test files linked
/// into the same binary: only the first call does anything.
///
/// Best-effort: on any failure, logs a warning and continues without the
/// protection, rather than failing the whole test run over a missing
/// safety net.
pub fn ensure_active() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if let Err(e) = install() {
            eprintln!(
                "redfog-test-cleanup: warning: failed to install process-group cleanup guard ({e}) -- \
                 continuing without it; spawned child processes may leak if this test hangs/crashes"
            );
        }
    });
}

#[ctor::ctor]
fn _install() {
    ensure_active();
}

fn install() -> std::io::Result<()> {
    let my_pid = unsafe { libc::getpid() };

    // Everything spawned from here on (unless it calls setpgid itself)
    // joins this new group, distinct from whatever job-control group
    // `cargo test`/the shell put us in.
    if unsafe { libc::setpgid(0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    let extra_dir = extra_pgid_dir(my_pid);
    std::fs::create_dir_all(&extra_dir)?;
    let _ = EXTRA_PGID_DIR.set(extra_dir.clone());

    // `ensure_private_dbus_session()` re-execs this same pid in place into
    // `dbus-run-session`, which then *forks a new child* to re-run this
    // binary -- a genuinely separate pid, running its own `#[ctor]`, so it
    // ends up with its own, disconnected process group + watchdog. Without
    // this, killing the original top-level pid (the only one `cargo test`
    // /`timeout` ever has) kills nothing useful: confirmed live, kwin_wayland
    // survived because it belonged to that second, unreachable group. Tying
    // each invocation to *its own* immediate parent via PDEATHSIG bridges
    // the two: dbus-run-session isn't capability-bearing, so unlike
    // `kwin_wayland` this isn't cleared at its exec (see module docs).
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };

    match unsafe { libc::fork() } {
        -1 => Err(std::io::Error::last_os_error()),
        0 => {
            // Watchdog: joined the new group automatically at fork (it
            // happened after setpgid above), which is fine -- it's about to
            // kill(-pgid) that same group anyway, itself included.
            let pgid = unsafe { libc::getpgrp() };
            let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, my_pid, 0) };
            if pidfd >= 0 {
                let mut pfd = libc::pollfd { fd: pidfd as i32, events: libc::POLLIN, revents: 0 };
                loop {
                    let ret = unsafe { libc::poll(&mut pfd, 1, -1) };
                    if ret > 0 {
                        break; // pidfd readable: the main process's pid has exited.
                    }
                    if ret < 0 {
                        let err = std::io::Error::last_os_error();
                        if err.kind() == std::io::ErrorKind::Interrupted {
                            continue;
                        }
                        break;
                    }
                }
            }
            if let Ok(entries) = std::fs::read_dir(&extra_dir) {
                for entry in entries.flatten() {
                    if let Some(extra_pgid) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) {
                        unsafe { libc::kill(-extra_pgid, libc::SIGKILL) };
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&extra_dir);
            // Last: this group includes the watchdog itself (see the
            // comment above), and self-targeted SIGKILL can take effect
            // before returning to userspace -- confirmed live, a naive
            // `kill(-pgid) then remove_dir_all` left the directory behind
            // every time because this line never actually got to run.
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
            std::process::exit(0);
        }
        _watchdog_pid => Ok(()),
    }
}
