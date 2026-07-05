use kwin_capture::CaptureSession;

fn env_i32(key: &str, default: i32) -> i32 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn main() {
    let width  = env_i32("REDFOG_WIDTH",  1920);
    let height = env_i32("REDFOG_HEIGHT", 1080);
    let scale  = env_f64("REDFOG_SCALE",  1.0);

    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or("wayland-0".into());
    let runtime = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR not set");
    let socket = if std::path::Path::new(&display).is_absolute() {
        std::path::PathBuf::from(display)
    } else {
        std::path::Path::new(&runtime).join(display)
    };

    let session = CaptureSession::connect(&socket, "redfog-output", width, height, scale)
        .expect("failed to create capture session");

    println!("{}", session.node_id());

    use std::io::BufRead;
    for line in std::io::BufReader::new(std::io::stdin()).lines().flatten() {
        if let Some(dims) = line.trim().strip_prefix("resize ") {
            if let Some((ws, hs)) = dims.split_once('x') {
                if let (Ok(w), Ok(h)) = (ws.trim().parse(), hs.trim().parse()) {
                    session.resize(w, h);
                }
            }
        }
    }

    loop { std::thread::sleep(std::time::Duration::from_secs(3600)); }
}
