//! Live verification probe (kept as a reusable diagnostic, same convention
//! as `scripts/test-*.sh` elsewhere in this repo): does `pam_systemd.so`'s
//! `desktop=` module argument actually propagate to logind's `Desktop=`
//! session property? **Confirmed live** on this machine via
//! `scripts/verify-pam-systemd-desktop-property.sh` — `Desktop=` and
//! `Service=` both come through exactly as expected, underpinning the
//! session-lifecycle plan's watchdog session-identification filter.
//!
//! Requires a temp PAM service file first:
//!   sudo tee /etc/pam.d/redfog-desktop-verify <<'EOF'
//!   auth     required   pam_permit.so
//!   account  required   pam_permit.so
//!   session  optional   pam_systemd.so desktop=redfog-verify-marker
//!   EOF
//!
//! Then: sudo ./target/debug/examples/pam_desktop_verify <username>
//! While it's sleeping, in another terminal:
//!   loginctl list-sessions
//!   loginctl show-session <id> -p Desktop -p Service -p Type -p Leader

use pam::Client;

fn main() {
    let username = std::env::args().nth(1).unwrap_or_else(|| "manuel".to_string());
    let mut client = Client::with_password("redfog-desktop-verify").expect("pam init failed — did you create /etc/pam.d/redfog-desktop-verify?");
    client.conversation_mut().set_credentials(username.clone(), "");
    client.authenticate().expect("authenticate (pam_permit, should always succeed)");
    client.open_session().expect("open_session failed");
    println!("PAM session opened for {username}, this process PID={}", std::process::id());
    println!("In another terminal, run:");
    println!("  loginctl list-sessions");
    println!("  loginctl show-session <id-with-Leader-matching-the-PID-above> -p Desktop -p Service -p Type");
    println!("Sleeping 20s before closing the session...");
    std::thread::sleep(std::time::Duration::from_secs(20));
    println!("Closing session now.");
}
