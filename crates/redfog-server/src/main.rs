use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use redfog_moonlight::clients::ClientManager;
use redfog_moonlight::control::ControlServer;
use redfog_moonlight::discovery::Discovery;
use redfog_moonlight::pairing::PairingServer;
use redfog_moonlight::rtsp::RtspServer;
use redfog_moonlight::session::{SessionConfig, SessionManager};
use redfog_moonlight::tls::ServerIdentity;

const HTTP_PORT: u16 = 47989;
const HTTPS_PORT: u16 = 47984;
const RTSP_PORT: u16 = 48010;
const VIDEO_PORT: u16 = 47998;
const CONTROL_PORT: u16 = 47999;
const AUDIO_PORT: u16 = 48000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // Must run before anything else touches D-Bus: re-execs the whole
    // process inside dbus-run-session on first launch.
    redfog_core::ensure_private_dbus_session();

    gstreamer::init()?;

    let _headless_runtime = redfog_core::HeadlessRuntime::start(redfog_core::DEFAULT_RUNTIME_DIR)
        .map_err(|e| e as Box<dyn std::error::Error>)?;

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(run())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let bind_addr: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    let hostname = gethostname::gethostname().to_string_lossy().to_string();

    let state_dir = redfog_moonlight::tls::default_state_dir();
    let identity = ServerIdentity::load_or_create(&state_dir).map_err(|e| format!("failed to load server identity: {e}"))?;
    let clients = Arc::new(ClientManager::new(&state_dir, identity.cert_pem.clone(), identity.private_key_pem.clone()));

    let session_manager = SessionManager::new(SessionConfig {
        bind_addr,
        video_port: VIDEO_PORT,
        audio_port: AUDIO_PORT,
        // First iteration: fixed "Desktop" app is plasmashell, matching what
        // proto.sh/kwin-viewer already exercise.
        user_app: vec!["plasmashell".to_string(), "--no-respawn".to_string()],
        bitrate_kbps: 10_000,
    });

    let pairing_server = Arc::new(PairingServer {
        clients: clients.clone(),
        identity,
        hostname: hostname.clone(),
        http_port: HTTP_PORT,
        https_port: HTTPS_PORT,
        rtsp_port: RTSP_PORT,
        launch_handler: session_manager.clone(),
    });

    let rtsp_server = Arc::new(RtspServer {
        port: RTSP_PORT,
        video_port: VIDEO_PORT,
        control_port: CONTROL_PORT,
        audio_port: AUDIO_PORT,
        default_width: 1920,
        default_height: 1080,
        default_fps: 60,
        handler: session_manager.clone(),
    });

    let control_server = ControlServer {
        port: CONTROL_PORT,
        key: session_manager.rikey_cell(),
        handler: session_manager.clone(),
    };

    let _discovery = Discovery::spawn(&hostname, bind_addr, HTTP_PORT).map_err(|e| tracing::warn!("mDNS discovery not started: {e}")).ok();

    tracing::info!("redfog-server starting: http={HTTP_PORT} https={HTTPS_PORT} rtsp={RTSP_PORT} video={VIDEO_PORT} control={CONTROL_PORT} audio={AUDIO_PORT}");

    tokio::try_join!(
        async { pairing_server.clone().serve_http(bind_addr).await },
        async { pairing_server.clone().serve_https(bind_addr).await },
        async { rtsp_server.clone().serve(bind_addr).await },
        async { control_server.serve(bind_addr).await },
    )?;

    Ok(())
}
