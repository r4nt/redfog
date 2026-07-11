//! redfog-broker: the one small, separately-privileged component in redfog.
//! Handles PAM authentication and spawning a target user's compositor
//! session (via templated systemd units) on `redfog-server`'s behalf, so
//! the large, network-facing, untrusted-input-parsing `redfog-server`
//! process itself never needs any elevated privilege. See design.md's
//! "Privilege separation: broker vs. server".

mod auth;
mod session;

use redfog_broker_protocol::{read_request, write_response, BrokerRequest, BrokerResponse};
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};

fn socket_path() -> String {
    std::env::var("REDFOG_BROKER_SOCKET").unwrap_or_else(|_| "/tmp/redfog-runtime/broker.sock".to_string())
}

/// Resolves a group name to a gid via `getent group`, the same pattern
/// `session::resolve_user` uses for usernames.
async fn resolve_group(name: &str) -> Result<u32, String> {
    let output = tokio::process::Command::new("getent")
        .args(["group", name])
        .output()
        .await
        .map_err(|e| format!("failed to run getent group {name}: {e}"))?;
    if !output.status.success() {
        return Err(format!("getent group {name} exited with {}", output.status));
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<&str> = line.trim().split(':').collect();
    let gid: &str = fields.get(2).ok_or_else(|| format!("could not parse getent group {name} output: {line:?}"))?;
    gid.parse().map_err(|e| format!("invalid gid in getent group {name} output: {e}"))
}

/// The broker trusts whatever connects to this socket to already be a
/// legitimate `redfog-server` — none of `BrokerRequest`'s other variants
/// (`SpawnSession`, `SpawnPayload`, `ReadUserSessionConfig`, ...) require
/// re-proving identity on the same connection, so the socket's own file
/// permissions are the actual access-control boundary, not just a umask
/// accident. `UnixListener::bind` alone leaves that to the ambient umask
/// (confirmed live: came out `rwxr-xr-x`, root-owned — no *write* bit for
/// group/other, which a Unix-domain `connect()` requires, so a distinct
/// unprivileged `redfog-server` service account couldn't reach it at all).
///
/// With `REDFOG_BROKER_SOCKET_GROUP` set: chown to `root:<group>`, mode
/// `0660` — the proper fix, for a deployment that's set up a dedicated
/// group both this process and `redfog-server`'s service account belong
/// to. Without it: mode `0666` (world-accessible) with a loud warning,
/// since design.md's "Privilege separation: broker vs. server" already
/// documents the broker's *own* trust model as the unhardened "simple
/// version" for now (runs as root via `sudo`, no scoped service user or
/// polkit yet) — this at least makes that documented-simple deployment
/// actually reach `redfog-server` end to end, rather than silently
/// depending on whatever umask happened to be active.
async fn secure_socket(path: &str) -> Result<(), String> {
    let c_path = std::ffi::CString::new(path).map_err(|e| format!("invalid socket path {path:?}: {e}"))?;
    match std::env::var("REDFOG_BROKER_SOCKET_GROUP") {
        Ok(group) => {
            let gid = resolve_group(&group).await?;
            if unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) } != 0 {
                return Err(format!("failed to chown {path} to group {group} ({gid}): {}", std::io::Error::last_os_error()));
            }
            std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o660))
                .map_err(|e| format!("failed to chmod {path}: {e}"))?;
            tracing::info!("broker socket {path} restricted to root:{group} (mode 0660)");
        }
        Err(_) => {
            std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o666))
                .map_err(|e| format!("failed to chmod {path}: {e}"))?;
            tracing::warn!(
                "REDFOG_BROKER_SOCKET_GROUP not set — broker socket {path} is world-accessible (mode 0666): \
                 any local user can request sessions as other users through it, with no password check at \
                 that point (BrokerRequest's other variants trust the connection, not a re-proven identity). \
                 Set REDFOG_BROKER_SOCKET_GROUP to a dedicated group both redfog-broker and redfog-server's \
                 service account belong to for a properly scoped deployment."
            );
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let path = socket_path();
    if let Some(parent) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&path); // stale socket from a previous run

    let listener = UnixListener::bind(&path)?;
    secure_socket(&path).await?;
    tracing::info!("redfog-broker listening on {path}");

    let sessions = std::sync::Arc::new(session::SessionManager::new());

    loop {
        let (stream, _addr) = listener.accept().await?;
        let sessions = sessions.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, sessions).await {
                tracing::warn!("connection handler exited: {e}");
            }
        });
    }
}

async fn handle_connection(
    stream: UnixStream,
    sessions: std::sync::Arc<session::SessionManager>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(stream);
    loop {
        let request = match read_request(&mut reader).await? {
            Some(r) => r,
            None => return Ok(()), // peer closed
        };

        let response = match request {
            BrokerRequest::Authenticate { username, password } => {
                tracing::info!("authenticating user {username}");
                BrokerResponse::Authenticate(auth::authenticate(username, password).await)
            }
            BrokerRequest::SpawnSession { session_id, username, width, height, socket_name, payload } => {
                tracing::info!("spawning session {session_id} for user {username} ({width}x{height})");
                let result = sessions
                    .spawn(&session_id, &username, width, height, &socket_name, &payload)
                    .await
                    .map(|wayland_socket_path| redfog_broker_protocol::SpawnedSession { wayland_socket_path });
                BrokerResponse::SpawnSession(result)
            }
            BrokerRequest::SpawnPayload { session_id, username, socket_path, runtime_dir, argv, env } => {
                tracing::info!("spawning payload for session {session_id}, user {username}, against caller-owned socket {socket_path}");
                let result = sessions.spawn_payload(&session_id, &username, &socket_path, &runtime_dir, &argv, &env).await;
                BrokerResponse::SpawnPayload(result)
            }
            BrokerRequest::TerminateSession { session_id } => {
                tracing::info!("terminating session {session_id}");
                BrokerResponse::TerminateSession(sessions.terminate(&session_id).await)
            }
            BrokerRequest::ReadUserSessionConfig { username } => {
                tracing::info!("reading session.toml for user {username}");
                BrokerResponse::ReadUserSessionConfig(sessions.read_user_session_config(&username).await)
            }
        };

        write_response(&mut reader, &response).await?;
    }
}
