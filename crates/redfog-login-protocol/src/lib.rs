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
//!
//! Also owns [`SessionPreset`]/[`load_presets`] — not wire types, but a
//! *schema* both sides need to agree on, same reasoning as the wire types
//! themselves: `redfog-login` and `redfog-server` each read
//! `~/.../sessions.toml` directly and independently (see `load_presets`'s
//! doc comment for why this doesn't need its own protocol round-trip), so
//! centralizing the format here is what keeps those two readings from
//! drifting apart, not a request/response shape.

use serde::{Deserialize, Serialize};

pub mod render;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LoginRequest {
    /// `session` is either the `name` of one of the operator-configured
    /// [`SessionPreset`]s (see `load_presets`), or the literal
    /// `"user-configured"` sentinel — the login screen's "Custom" option,
    /// which tells `SessionManager` to resolve the actual backend/payload
    /// from the target user's own `~/.config/redfog/session.toml` instead
    /// (read via the broker's `ReadUserSessionConfig`, gated behind this
    /// same `Authenticate` having already succeeded).
    Authenticate { username: String, password: String, session: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LoginResponse {
    Authenticate(Result<(), String>),
}

/// One operator-configured, named entry in `sessions.toml` — what the login
/// screen's dropdown actually shows, and what `SessionManager` resolves a
/// non-"Custom" `LoginRequest::Authenticate.session` name against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPreset {
    pub name: String,
    /// `"kwin"` or `"gst-wayland-display"` — same strings `REDFOG_BACKEND`
    /// uses (see `session_backend::Backend`'s `FromStr`/`as_str`). Kept as
    /// a plain `String` here rather than depending on that crate's
    /// `Backend` type directly — same reasoning as `LoginRequest`'s old
    /// `backend` field had: `redfog-login` has no reason to pull in
    /// `session-backend`'s much heavier dependency graph (gstreamer,
    /// redfog-core, ...) just for one enum. `redfog-server` validates this
    /// parses at startup (see its own loading code) rather than deferring
    /// the error to whenever someone happens to pick a broken entry.
    pub backend: String,
    pub payload: Vec<String>,
    /// gst-wayland-display-only — see `NestedSessionConfig`'s fields of the
    /// same name. Ignored for `backend = "kwin"`.
    #[serde(default)]
    pub desktop_name: Option<String>,
    #[serde(default)]
    pub glx_vendor: Option<String>,
}

/// Default config path — overridable via `REDFOG_SESSIONS_CONFIG` (see each
/// binary's own env-var handling); `redfog-server` exports the path it
/// actually resolved into that same env var for its spawned Login-stage
/// process tree, so `redfog-login` is guaranteed to read the identical file
/// rather than independently re-resolving a possibly-different default.
pub const DEFAULT_SESSIONS_CONFIG_PATH: &str = "/etc/redfog/sessions.toml";

/// The presets offered when no `sessions.toml` exists at all — matches
/// what `redfog-login`'s picker originally hardcoded before this existed,
/// so a deployment that's never created the file keeps working unchanged.
pub fn default_presets() -> Vec<SessionPreset> {
    vec![
        SessionPreset {
            name: "KDE Plasma".to_string(),
            backend: "kwin".to_string(),
            payload: vec!["plasmashell".to_string(), "--no-respawn".to_string()],
            desktop_name: None,
            glx_vendor: None,
        },
        SessionPreset {
            name: "Sway".to_string(),
            backend: "gst-wayland-display".to_string(),
            payload: vec!["sway".to_string()],
            desktop_name: Some("sway".to_string()),
            glx_vendor: None,
        },
    ]
}

#[derive(Deserialize)]
struct SessionsFile {
    #[serde(rename = "session", default)]
    sessions: Vec<SessionPreset>,
}

/// Reads and parses `path` (TOML, an array of `[[session]]` tables — see
/// [`SessionPreset`]'s fields), falling back to [`default_presets`] if the
/// file doesn't exist at all. A malformed file (bad TOML, or one that
/// parses but defines zero entries) is a hard error rather than a silent
/// fallback — an operator who *did* create the file almost certainly wants
/// to know it didn't take effect, not see their edits silently ignored.
///
/// Called independently by both `redfog-server` (to resolve a submitted
/// preset name at login time) and `redfog-login` (to render the picker) —
/// deliberately not a request/response over `LoginRequest`/`LoginResponse`:
/// the preset list isn't sensitive (unlike a specific user's own
/// `~/.config/redfog/session.toml`, which genuinely does need the broker's
/// privilege to read — see `BrokerRequest::ReadUserSessionConfig`), so
/// there's no privilege boundary to cross by having each side just read
/// the same world-readable file directly, the same way SDDM/GDM scan
/// `.desktop` files themselves rather than querying a session-list RPC.
pub fn load_presets(path: &str) -> Result<Vec<SessionPreset>, String> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(default_presets()),
        Err(e) => return Err(format!("failed to read {path}: {e}")),
    };
    let file: SessionsFile = toml::from_str(&contents).map_err(|e| format!("failed to parse {path}: {e}"))?;
    if file.sessions.is_empty() {
        return Err(format!("{path} defines no [[session]] entries"));
    }
    Ok(file.sessions)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(contents: &str) -> String {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("redfog-login-protocol-test-{}-{n}.toml", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn missing_file_falls_back_to_defaults() {
        let path = std::env::temp_dir().join("redfog-login-protocol-test-does-not-exist.toml");
        let presets = load_presets(path.to_str().unwrap()).unwrap();
        assert_eq!(presets, default_presets());
    }

    #[test]
    fn parses_a_real_config_file() {
        let path = write_temp(
            r#"
            [[session]]
            name = "KDE Plasma"
            backend = "kwin"
            payload = ["plasmashell", "--no-respawn"]

            [[session]]
            name = "Sway (custom)"
            backend = "gst-wayland-display"
            payload = ["sway", "-c", "/tmp/whatever.conf"]
            desktop_name = "sway"
            glx_vendor = "nvidia"
            "#,
        );
        let presets = load_presets(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        assert_eq!(presets.len(), 2);
        assert_eq!(presets[1].name, "Sway (custom)");
        assert_eq!(presets[1].backend, "gst-wayland-display");
        assert_eq!(presets[1].payload, vec!["sway", "-c", "/tmp/whatever.conf"]);
        assert_eq!(presets[1].desktop_name.as_deref(), Some("sway"));
        assert_eq!(presets[1].glx_vendor.as_deref(), Some("nvidia"));
    }

    #[test]
    fn empty_sessions_list_is_an_error() {
        let path = write_temp("");
        let result = load_presets(&path);
        std::fs::remove_file(&path).unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn malformed_toml_is_an_error() {
        let path = write_temp("this is not valid toml [[[");
        let result = load_presets(&path);
        std::fs::remove_file(&path).unwrap();
        assert!(result.is_err());
    }
}
