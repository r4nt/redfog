//! PAM authentication for submitted username/password credentials.
//!
//! Needs no special privilege beyond what's already granted to any regular
//! process: on this system (and most modern distros) `pam_unix.so` shells
//! out to `unix_chkpwd`, a setuid-root helper, to do the actual `/etc/shadow`
//! comparison — the calling process (this broker) never touches shadow
//! directly. Confirmed via `ls -la /usr/bin/unix_chkpwd` showing the setuid
//! bit, and `/etc/pam.d/system-auth` using `pam_unix.so`.

use pam::Client;

/// PAM service name to authenticate against. Production should ship a
/// dedicated `/etc/pam.d/redfog` service file (typically just including
/// system-auth); "system-auth" is used directly here since that file
/// already exists on this system and needs no setup to test against.
const PAM_SERVICE: &str = "system-auth";

/// Authenticates `username`/`password` via PAM. Runs the blocking PAM calls
/// on a dedicated blocking thread since `pam::Client` isn't async.
///
/// If `REDFOG_BROKER_FAKE_AUTH` is set, skips PAM entirely and always
/// succeeds — for integration testing the rest of the broker/session
/// pipeline (systemd unit generation, socket activation, capture/input)
/// without needing a real PAM setup or credentials. Never set this in
/// production.
pub async fn authenticate(username: String, password: String) -> Result<(), String> {
    if std::env::var_os("REDFOG_BROKER_FAKE_AUTH").is_some() {
        tracing::warn!("REDFOG_BROKER_FAKE_AUTH set — accepting {username} without checking PAM at all");
        return Ok(());
    }
    tokio::task::spawn_blocking(move || authenticate_blocking(&username, &password))
        .await
        .map_err(|e| format!("PAM auth task panicked: {e}"))?
}

fn authenticate_blocking(username: &str, password: &str) -> Result<(), String> {
    let mut client = Client::with_password(PAM_SERVICE).map_err(|e| format!("failed to init PAM client: {e}"))?;
    client.conversation_mut().set_credentials(username, password);
    client.authenticate().map_err(|e| format!("authentication failed: {e}"))?;
    Ok(())
}
