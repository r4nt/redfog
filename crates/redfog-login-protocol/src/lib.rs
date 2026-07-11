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
    /// `backend` is `"kwin"` or `"gst-wayland-display"` — same strings
    /// `REDFOG_BACKEND` uses (see `session_backend::Backend`'s `FromStr`/
    /// `as_str`) — kept as a plain `String` here rather than depending on
    /// that crate's `Backend` type directly, since `redfog-login` is a
    /// minimal `eframe` GUI with no reason to pull in `session-backend`'s
    /// much heavier dependency graph (gstreamer, redfog-core, ...) just for
    /// one enum.
    Authenticate { username: String, password: String, backend: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LoginResponse {
    Authenticate(Result<(), String>),
}
