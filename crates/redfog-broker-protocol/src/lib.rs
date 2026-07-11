//! IPC protocol between `redfog-server` (unprivileged, network-facing) and
//! `redfog-broker` (small, separately-privileged) — see design.md's
//! "Privilege separation: broker vs. server". Newline-delimited JSON over a
//! Unix socket; each request gets exactly one response.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BrokerRequest {
    /// Checks a submitted username/password via PAM. Does not spawn
    /// anything — see `SpawnSession` for that, once this succeeds.
    Authenticate { username: String, password: String },
    /// Spawns the target user's compositor session (KWin + supporting
    /// D-Bus/session setup) as `username`, identified by `session_id` for
    /// later `TerminateSession` calls. `payload` is the command run as
    /// KWin's `--exit-with-session` (e.g. `["plasmashell", "--no-respawn"]`)
    /// — without it KWin starts with no session app at all. `socket_name`
    /// is KWin's `--socket` value — the caller's choice, not the broker's,
    /// since it also becomes the session's `WAYLAND_DISPLAY` (which the
    /// caller's own `CompositorSession`-equivalent bookkeeping keys off of).
    SpawnSession { session_id: String, username: String, width: u32, height: u32, socket_name: String, payload: Vec<String> },
    /// For backends where the *caller* (not the broker) already created and
    /// owns the compositor/Wayland socket — e.g. redfog-moonlight embedding
    /// a `gst-wayland-display` pipeline directly in its own process, unlike
    /// KWin, which the broker spawns and owns itself (see `SpawnSession`).
    /// The broker's job shrinks to: grant `username` access to an
    /// already-existing `socket_path`/`runtime_dir`, then spawn `payload`
    /// (e.g. `["sway"]`) as that user pointed at it — no compositor of its
    /// own to create. `argv`/`env` are the exact command shape to run,
    /// typically from a backend crate's own command-building helper (e.g.
    /// `gst_backend::command_and_env`) so the broker doesn't need to know
    /// backend-specific details like D-Bus wrapping.
    SpawnPayload {
        session_id: String,
        username: String,
        socket_path: String,
        runtime_dir: String,
        argv: Vec<String>,
        env: Vec<(String, String)>,
    },
    TerminateSession { session_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BrokerResponse {
    Authenticate(Result<(), String>),
    SpawnSession(Result<SpawnedSession, String>),
    SpawnPayload(Result<(), String>),
    TerminateSession(Result<(), String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnedSession {
    /// Path to the Wayland socket `redfog-server`'s `InputForwarder` and
    /// `CompositorSession`-equivalent capture code should connect to.
    pub wayland_socket_path: String,
}

/// Serializes `request` as one JSON line and writes it to the stream
/// underlying `reader` (its `BufReader` wrapping doesn't affect writes).
pub async fn write_request(reader: &mut BufReader<UnixStream>, request: &BrokerRequest) -> std::io::Result<()> {
    write_line(reader.get_mut(), request).await
}

/// Reads one JSON line from `reader` and deserializes it as a `BrokerResponse`.
pub async fn read_response(reader: &mut BufReader<UnixStream>) -> std::io::Result<BrokerResponse> {
    read_line(reader).await?.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "peer closed"))
}

/// Serializes `response` as one JSON line and writes it to the stream
/// underlying `reader`.
pub async fn write_response(reader: &mut BufReader<UnixStream>, response: &BrokerResponse) -> std::io::Result<()> {
    write_line(reader.get_mut(), response).await
}

/// Reads one JSON line from `reader` and deserializes it as a `BrokerRequest`.
/// `Ok(None)` means the peer closed the connection cleanly (EOF).
pub async fn read_request(reader: &mut BufReader<UnixStream>) -> std::io::Result<Option<BrokerRequest>> {
    read_line(reader).await
}

async fn write_line<T: Serialize>(stream: &mut UnixStream, value: &T) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value).expect("protocol types always serialize");
    line.push('\n');
    stream.write_all(line.as_bytes()).await
}

async fn read_line<T: for<'de> Deserialize<'de>>(reader: &mut BufReader<UnixStream>) -> std::io::Result<Option<T>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None); // EOF, peer closed
    }
    serde_json::from_str(line.trim_end())
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
