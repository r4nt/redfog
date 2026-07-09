//! Unix-socket server for `redfog-login`'s credential reports — see
//! `redfog_login_protocol` and `SessionManager::handle_login_report`.

use std::path::PathBuf;
use std::sync::Arc;

use redfog_login_protocol::{LoginRequest, LoginResponse};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::session::SessionManager;

pub struct LoginReportServer {
    pub socket_path: PathBuf,
    pub session_manager: Arc<SessionManager>,
}

impl LoginReportServer {
    pub async fn serve(self: Arc<Self>) -> Result<(), String> {
        let _ = std::fs::remove_file(&self.socket_path);
        let listener = UnixListener::bind(&self.socket_path)
            .map_err(|e| format!("failed to bind login report socket at {:?}: {e}", self.socket_path))?;
        loop {
            let (stream, _) = listener.accept().await.map_err(|e| format!("failed to accept login report connection: {e}"))?;
            let this = self.clone();
            tokio::spawn(async move {
                if let Err(e) = this.handle_connection(stream).await {
                    tracing::warn!("login report connection error: {e}");
                }
            });
        }
    }

    async fn handle_connection(&self, stream: UnixStream) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await? == 0 {
                return Ok(()); // peer closed
            }
            let request: LoginRequest = serde_json::from_str(line.trim_end())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            let response = match request {
                LoginRequest::Authenticate { username, password } => {
                    LoginResponse::Authenticate(self.session_manager.handle_login_report(username, password).await)
                }
            };
            let mut response_line = serde_json::to_string(&response).expect("protocol types always serialize");
            response_line.push('\n');
            reader.get_mut().write_all(response_line.as_bytes()).await?;
        }
    }
}
