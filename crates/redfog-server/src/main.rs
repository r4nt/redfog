use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use redfog_moonlight::clients::ClientManager;
use redfog_moonlight::control::ControlServer;
use redfog_moonlight::discovery::Discovery;
use redfog_moonlight::pairing::PairingServer;
use redfog_moonlight::rtsp::RtspServer;
use redfog_moonlight::session::{SessionConfig, SessionManager};
use redfog_moonlight::tls::ServerIdentity;

fn env_port(name: &str, default: u16) -> u16 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // Must run before anything else touches D-Bus: re-execs the whole
    // process inside dbus-run-session on first launch.
    redfog_core::ensure_private_dbus_session();

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

    let bind_addr: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    let hostname = gethostname::gethostname().to_string_lossy().to_string();

    let state_dir = redfog_moonlight::tls::default_state_dir();
    let identity = ServerIdentity::load_or_create(&state_dir).map_err(|e| format!("failed to load server identity: {e}"))?;
    let clients = Arc::new(ClientManager::new(&state_dir, identity.cert_pem.clone(), identity.private_key_pem.clone()));

    let session_manager = SessionManager::new(SessionConfig {
        bind_addr,
        video_port,
        audio_port,
        user_app,
        bitrate_kbps: 10_000,
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

    let _discovery = Discovery::spawn(&hostname, bind_addr, http_port).map_err(|e| tracing::warn!("mDNS discovery not started: {e}")).ok();

    tracing::info!("redfog-server starting: http={http_port} https={https_port} rtsp={rtsp_port} video={video_port} control={control_port} audio={audio_port}");

    tokio::try_join!(
        async { pairing_server.clone().serve_http(bind_addr).await },
        async { pairing_server.clone().serve_https(bind_addr).await },
        async { rtsp_server.clone().serve(bind_addr).await },
        async { control_server.serve(bind_addr).await },
    )?;

    Ok(())
}
