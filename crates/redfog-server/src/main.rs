use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use redfog_moonlight::clients::ClientManager;
use redfog_moonlight::control::ControlServer;
use redfog_moonlight::discovery::Discovery;
use redfog_moonlight::login_report::LoginReportServer;
use redfog_moonlight::pairing::PairingServer;
use redfog_moonlight::rtsp::RtspServer;
use redfog_moonlight::session::{SessionConfig, SessionManager};
use redfog_moonlight::tls::ServerIdentity;

fn env_port(name: &str, default: u16) -> u16 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // rustls::ServerConfig::builder() (pairing.rs's HTTPS server) picks the
    // process-default CryptoProvider automatically, but only if exactly one
    // is compiled in — with both `ring` and `aws-lc-rs` linked (workspace
    // feature unification pulls in `ring` transitively via dev-only test
    // deps; `tls.rs`'s own `AcceptAnyClientCert` already hardcodes
    // `aws_lc_rs`) that auto-detection is ambiguous and panics at startup.
    // Installing explicitly here makes the pick unambiguous regardless of
    // what else happens to be linked in.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("no CryptoProvider installed yet");

    // Must run before anything else touches D-Bus: re-execs the whole
    // process inside dbus-run-session on first launch.
    redfog_core::ensure_private_dbus_session();

    // For Backend::GstWaylandDisplay: waylanddisplaysrc isn't installed
    // system-wide, so it won't be on GStreamer's default plugin search path
    // — must be set before gstreamer::init(), same as viewer does.
    if let Ok(plugin_dir) = std::env::var("REDFOG_GST_WAYLAND_DISPLAY_PLUGIN_DIR") {
        let existing = std::env::var("GST_PLUGIN_PATH").unwrap_or_default();
        let combined = if existing.is_empty() { plugin_dir } else { format!("{plugin_dir}:{existing}") };
        std::env::set_var("GST_PLUGIN_PATH", combined);
    }
    gstreamer::init()?;

    let _headless_runtime = redfog_core::HeadlessRuntime::start(redfog_core::default_runtime_dir())
        .map_err(|e| e as Box<dyn std::error::Error>)?;

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(run())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Overridable so a self-contained integration test can run its own
    // instance, on its own ports, alongside a real redfog-server already
    // running on the default ones — see redfog-moonlight/tests/.
    let http_port = env_port("REDFOG_HTTP_PORT", 47989);
    let https_port = env_port("REDFOG_HTTPS_PORT", 47984);
    let rtsp_port = env_port("REDFOG_RTSP_PORT", 48010);
    let video_port = env_port("REDFOG_VIDEO_PORT", 47998);
    let control_port = env_port("REDFOG_CONTROL_PORT", 47999);
    let audio_port = env_port("REDFOG_AUDIO_PORT", 48000);
    // Space-separated, e.g. "glxgears" or "plasmashell --no-respawn".
    let user_app: Vec<String> = std::env::var("REDFOG_USER_APP")
        .unwrap_or_else(|_| "plasmashell --no-respawn".to_string())
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let login_app: Vec<String> = std::env::var("REDFOG_LOGIN_APP")
        .unwrap_or_else(|_| "target/release/redfog-login".to_string())
        .split_whitespace()
        .map(str::to_string)
        .collect();

    let bind_addr: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    let hostname = gethostname::gethostname().to_string_lossy().to_string();

    let state_dir = redfog_moonlight::tls::default_state_dir();
    let identity = ServerIdentity::load_or_create(&state_dir).map_err(|e| format!("failed to load server identity: {e}"))?;
    let clients = Arc::new(ClientManager::new(&state_dir, identity.cert_pem.clone(), identity.private_key_pem.clone()));

    let log_mouse_events = std::env::var("REDFOG_LOG_MOUSE_EVENTS").is_ok_and(|v| v != "0");
    let broker_socket_path = std::env::var("REDFOG_BROKER_SOCKET").ok().map(std::path::PathBuf::from);
    let backend = match std::env::var("REDFOG_BACKEND").as_deref() {
        Ok("gst-wayland-display") => redfog_moonlight::session::Backend::GstWaylandDisplay,
        Ok("kwin") | Err(_) => redfog_moonlight::session::Backend::Kwin,
        Ok(other) => return Err(format!("unknown REDFOG_BACKEND {other:?} (expected \"kwin\" or \"gst-wayland-display\")").into()),
    };

    // Where redfog-login reports the credentials it collects (see
    // design.md's "Authentication: a real graphical login screen") —
    // exported as an env var so the Login-stage KWin process (and its
    // --exit-with-session child, redfog-login) inherit it.
    let login_socket_path: std::path::PathBuf = std::env::var("REDFOG_LOGIN_SOCKET")
        .unwrap_or_else(|_| format!("{}/login.sock", redfog_core::default_runtime_dir()))
        .into();
    std::env::set_var("REDFOG_LOGIN_SOCKET", &login_socket_path);

    let session_manager = SessionManager::new(SessionConfig {
        bind_addr,
        video_port,
        audio_port,
        login_app,
        user_app,
        bitrate_kbps: 10_000,
        broker_socket_path,
        log_mouse_events,
        backend,
    });

    let pairing_server = Arc::new(PairingServer {
        clients: clients.clone(),
        identity,
        hostname: hostname.clone(),
        http_port,
        https_port,
        rtsp_port,
        launch_handler: session_manager.clone(),
    });

    let rtsp_server = Arc::new(RtspServer {
        port: rtsp_port,
        video_port,
        control_port,
        audio_port,
        default_width: 1920,
        default_height: 1080,
        default_fps: 60,
        handler: session_manager.clone(),
        session_id: format!("{:016X}", rand::random::<u64>()),
    });

    let control_server = ControlServer {
        port: control_port,
        key: session_manager.rikey_cell(),
        handler: session_manager.clone(),
        rikey_generation: session_manager.rikey_generation(),
    };

    let login_report_server = Arc::new(LoginReportServer {
        socket_path: login_socket_path,
        session_manager: session_manager.clone(),
    });

    let _discovery = Discovery::spawn(&hostname, bind_addr, http_port).map_err(|e| tracing::warn!("mDNS discovery not started: {e}")).ok();

    tracing::info!("redfog-server starting: http={http_port} https={https_port} rtsp={rtsp_port} video={video_port} control={control_port} audio={audio_port}");

    tokio::try_join!(
        async { pairing_server.clone().serve_http(bind_addr).await },
        async { pairing_server.clone().serve_https(bind_addr).await },
        async { rtsp_server.clone().serve(bind_addr).await },
        async { control_server.serve(bind_addr).await },
        async { login_report_server.serve().await },
    )?;

    Ok(())
}
