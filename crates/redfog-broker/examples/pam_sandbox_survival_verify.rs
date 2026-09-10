//! Live verification probe (kept as a reusable diagnostic, same convention
//! as `scripts/test-*.sh` elsewhere in this repo): does mount-namespace
//! sandboxing set up via `systemd-run --property=BindPaths=...`/
//! `TemporaryFileSystem=...` survive a `pam_open_session` call (which
//! migrates the process into a *different* cgroup, `session-N.scope`)?
//! **Confirmed live** via `scripts/verify-sandbox-survives-pam.sh` —
//! `/dev/dri` shows only the sandboxed render node both before and after
//! `pam_open_session`. Also how a real, load-bearing gotcha got found:
//! this only works as a transient *service* — `systemd-run --scope`
//! doesn't own the exec step, so `BindPaths=`/`TemporaryFileSystem=`
//! (which need systemd itself to set up the mount namespace at exec time)
//! silently don't apply to scopes at all ("Unknown assignment" error).
//!
//! This binary is the thing `systemd-run --scope` execs into (via the
//! wrapper script below) -- it opens a PAM session (migrating cgroups),
//! then lists /dev/dri to prove whether the BindPaths= restriction is
//! still in effect afterward.
//!
//! Requires the same /etc/pam.d/redfog-desktop-verify file the other probe
//! (pam_desktop_verify.rs) creates. Run via the accompanying shell script,
//! not directly -- see scripts/verify-sandbox-survives-pam.sh.

use pam::Client;

fn main() {
    println!("--- /dev/dri BEFORE pam_open_session ---");
    list_dev_dri();

    let username = std::env::args().nth(1).unwrap_or_else(|| "manuel".to_string());
    let mut client = Client::with_password("redfog-desktop-verify").expect("pam init failed — did you create /etc/pam.d/redfog-desktop-verify?");
    client.conversation_mut().set_credentials(username.clone(), "");
    client.authenticate().expect("authenticate (pam_permit, should always succeed)");
    client.open_session().expect("open_session failed");
    println!("PAM session opened for {username}, PID={}", std::process::id());

    println!("--- /dev/dri AFTER pam_open_session (this is the one that matters) ---");
    list_dev_dri();
}

fn list_dev_dri() {
    match std::fs::read_dir("/dev/dri") {
        Ok(entries) => {
            let mut names: Vec<String> = entries.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            println!("/dev/dri entries: {names:?}");
        }
        Err(e) => println!("/dev/dri unreadable: {e}"),
    }
}
