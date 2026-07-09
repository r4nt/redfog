//! IPC wire types between `redfog-login` and `redfog-server`: reports the
//! credentials the user typed so `SessionManager` can validate them via the
//! broker (see design.md's "Authentication: a real graphical login screen")
//! and use the real account for the subsequent User-stage `SpawnSession`,
//! instead of a placeholder. Newline-delimited JSON over a Unix socket, same
//! wire convention as `redfog-broker-protocol` — a separate, smaller
//! protocol crate rather than reusing that one, since this is login-app <->
//! server, not server <-> broker, and `redfog-login` (a plain blocking
//! `eframe` app) has no reason to depend on tokio the way that one does.
//! Each side implements its own line read/write using whatever I/O
//! primitives fit its own runtime; only these types need to stay in sync.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LoginRequest {
    Authenticate { username: String, password: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LoginResponse {
    Authenticate(Result<(), String>),
}
