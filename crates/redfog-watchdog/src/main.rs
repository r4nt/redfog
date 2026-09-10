//! Crash backstop for `redfog-server`/`redfog-broker`: a small, independent
//! poll loop that kills any orphaned redfog session left behind if either
//! process dies without a chance to run its own graceful `SIGTERM` handler
//! (`SIGKILL`, OOM, panic) — see `SessionManager::terminate_all`
//! (`redfog-broker`) and `SessionManager::shutdown_all_sessions`
//! (`redfog-moonlight`) for the graceful path this exists to back up, not
//! replace.
//!
//! Deliberately dumb: no flock, no shared lock file, no grace period. A
//! clean `systemctl restart`/`stop` already runs its own graceful handler
//! *synchronously* before the unit exits (see the `SIGTERM` handling in
//! both `main.rs`s), so it leaves nothing for this sweep to find in the
//! normal case — only a genuine crash does.
//!
//! **Not** plain `systemctl is-active` polling, despite that being the
//! obvious first design (an earlier version of this file used exactly
//! that) — **confirmed live** it misses real crashes outright: both units
//! have `Restart=on-failure`/`RestartSec=2`, so a `kill -9`'d
//! `redfog-broker` was back to `is-active`/healthy within ~2s, and this
//! poll loop's own interval (independently of how short it's configured)
//! can trivially land entirely *after* that restart completes without ever
//! sampling the down window in between — the crashed instance's own
//! in-memory session bookkeeping is gone regardless (a freshly restarted
//! process starts with an empty tracking map either way), but nothing ever
//! told this loop to go looking for what it left behind. Tracking each
//! unit's `InvocationID` (a UUID systemd assigns fresh on every single
//! start, automatic restarts included) between polls instead closes this:
//! a *change* — not just a momentary "not active" — is what actually
//! proves a previous instance ended and a new one began, and unlike a
//! polled snapshot, this can't be timed around: the restart itself is what
//! produces the new id, whenever it happens between two consecutive polls.
//!
//! Plain `std`, no `tokio` — a poll loop shelling out to `systemctl`/
//! `loginctl` (same precedent `redfog-broker`'s own `run_systemctl`/
//! `run_loginctl` helpers already set) has no need for an async runtime.
//! Standalone, not a shared dependency on `redfog-broker`'s own crate: this
//! process needs to keep working *specifically because* the broker might
//! be dead, so it can't depend on anything the broker's own liveness
//! would affect.

use std::process::Command;
use std::time::Duration;

fn main() {
    tracing_subscriber::fmt::init();

    let poll_interval = std::env::var("REDFOG_WATCHDOG_POLL_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(10));
    tracing::info!("redfog-watchdog starting, poll interval {poll_interval:?}");

    let mut last_server = unit_liveness("redfog-server.service");
    let mut last_broker = unit_liveness("redfog-broker.service");

    loop {
        std::thread::sleep(poll_interval);
        let server = unit_liveness("redfog-server.service");
        let broker = unit_liveness("redfog-broker.service");
        if server.crashed_since(&last_server) || broker.crashed_since(&last_broker) {
            tracing::warn!(
                "redfog-server {:?} -> {:?}, redfog-broker {:?} -> {:?} — sweeping for orphaned redfog sessions",
                last_server,
                server,
                last_broker,
                broker
            );
            sweep_orphaned_sessions();
        }
        last_server = server;
        last_broker = broker;
    }
}

/// A unit's liveness fingerprint at one point in time: whether `systemctl
/// is-active` currently says it's up, and its current `InvocationID` (see
/// this module's own doc comment for why the id, not just the active bit,
/// is what actually matters here).
#[derive(Debug, PartialEq, Eq)]
struct UnitLiveness {
    active: bool,
    invocation_id: String,
}

impl UnitLiveness {
    /// True if the unit crashed (or was otherwise restarted) at some point
    /// between `previous` and now — either it's down *right now*, or its
    /// `InvocationID` moved on to a fresh one since the last poll, which
    /// only happens when a previous instance ended and a new one started.
    fn crashed_since(&self, previous: &UnitLiveness) -> bool {
        !self.active || self.invocation_id != previous.invocation_id
    }
}

fn unit_liveness(unit: &str) -> UnitLiveness {
    UnitLiveness { active: systemctl_is_active(unit), invocation_id: systemctl_show_value(unit, "InvocationID").unwrap_or_default() }
}

fn systemctl_is_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn systemctl_show_value(unit: &str, property: &str) -> Option<String> {
    let output = Command::new("systemctl").args(["show", unit, "-p", property, "--value"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `loginctl list-sessions`, filtered to sessions whose `Desktop` property
/// is `redfog` **and** whose `Service` property is `redfog-session` (both
/// set by `/etc/pam.d/redfog-session`'s `pam_systemd.so desktop=redfog
/// type=wayland` line and the dedicated PAM service name itself — see
/// `redfog_broker::session::ensure_pam_session_service`'s own doc comment.
/// Confirmed live, `scripts/verify-pam-systemd-desktop-property.sh`) —
/// belt and suspenders, `Service=` is free and `Desktop=` is one PAM
/// config argument, cleanly distinguishing redfog's own sessions from an
/// operator's real desktop/SSH login without a separate registry this
/// process would otherwise need to keep in sync with the broker's own.
///
/// Each matching session still alive gets `loginctl kill-session
/// --kill-whom=all` — tears down the whole process tree (kwin_wayland,
/// plasmashell, portals, ...) along with the transient
/// `redfog-session-<id>.service`/`.socket` units `systemd-run` created for
/// it, same as `redfog-broker`'s own `terminate()` does for the graceful
/// path.
fn sweep_orphaned_sessions() {
    for session_id in list_session_ids() {
        let desktop = show_session_property(&session_id, "Desktop").unwrap_or_default();
        let service = show_session_property(&session_id, "Service").unwrap_or_default();
        if desktop != "redfog" || service != "redfog-session" {
            continue;
        }
        tracing::warn!("killing orphaned redfog session {session_id} (Desktop={desktop}, Service={service})");
        let result = Command::new("loginctl").args(["kill-session", &session_id, "--signal=SIGKILL", "--kill-whom=all"]).status();
        match result {
            Ok(status) if status.success() => tracing::info!("killed orphaned redfog session {session_id}"),
            Ok(status) => tracing::error!("loginctl kill-session {session_id} exited with {status}"),
            Err(e) => tracing::error!("failed to run loginctl kill-session {session_id}: {e}"),
        }
    }
}

/// Session ids from `loginctl list-sessions` — the first whitespace-
/// separated column, same convention
/// `scripts/verify-pam-systemd-desktop-property.sh` already relies on.
fn list_session_ids() -> Vec<String> {
    let output = match Command::new("loginctl").args(["list-sessions", "--no-legend"]).output() {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            tracing::warn!("loginctl list-sessions exited with {}: {}", output.status, String::from_utf8_lossy(&output.stderr));
            return Vec::new();
        }
        Err(e) => {
            tracing::warn!("failed to run loginctl list-sessions: {e}");
            return Vec::new();
        }
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next().map(str::to_string))
        .collect()
}

fn show_session_property(session_id: &str, property: &str) -> Option<String> {
    let output = Command::new("loginctl").args(["show-session", session_id, "-p", property, "--value"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
