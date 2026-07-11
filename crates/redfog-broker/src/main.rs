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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let path = socket_path();
    if let Some(parent) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&path); // stale socket from a previous run

    let listener = UnixListener::bind(&path)?;
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
        };

        write_response(&mut reader, &response).await?;
    }
}
