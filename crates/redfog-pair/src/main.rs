//! Tiny CLI for relaying a Moonlight pairing PIN to `redfog-server` — the
//! same "read the PIN off the client, type it in somewhere" step a real
//! Moonlight app's own pairing dialog does, just from a terminal instead of
//! a browser form (`/pin`), since not every client (e.g. `moonlight-web`)
//! surfaces one. Also doubles as the tool for setting/updating a paired
//! device's HiDPI scale factor (`--scale`).
//!
//! Usage: `redfog-pair <PIN> [--uniqueid <ID>] [--scale <FACTOR>] [--resolution <WxH>] [--host <HOST>] [--port <PORT>]`
//!    or: `redfog-pair --fingerprint <FP> --scale <FACTOR> [--resolution <WxH>] [--host <HOST>] [--port <PORT>]`
//!    or: `redfog-pair --list [--host <HOST>] [--port <PORT>]`
//!
//! `--uniqueid` is optional: `redfog-server` exposes which `uniqueid`(s) are
//! currently mid-handshake via `/pending-pairs` (not part of the real
//! Moonlight protocol — a small tooling hook added alongside this binary),
//! so if exactly one client is waiting, this picks it automatically instead
//! of requiring you to already know it (e.g. from grepping server logs,
//! which is how this used to be done by hand).
//!
//! `--scale` is also optional: sets this device's HiDPI scale factor for
//! `kwin_wayland --scale` (e.g. `1`, `1.5`, `2`) — see `ClientManager::
//! set_scale_for_pending`'s doc comment for why this is per-device, not a
//! single server-wide setting (a TV and a laptop pairing with the same
//! server legitimately want different values). Can be set at pairing time
//! (alongside the PIN, via `--uniqueid`) or updated later without
//! re-pairing, via `--fingerprint` instead — a real client's `uniqueid`
//! can't reliably identify one physical device at all (see `redfog-pair
//! --list` below), so updating an *already-paired* device always goes by
//! fingerprint, never `--uniqueid`.
//!
//! `--list` prints every paired device's cert fingerprint, one per line —
//! this is how you find the value `--fingerprint` needs later, since
//! there's no other human-facing identity for an already-paired device
//! (confirmed against moonlight-qt's own source: every real client sends
//! the exact same hardcoded placeholder `uniqueid` to any non-genuine-GFE
//! server, redfog included, so `uniqueid` can't tell two paired devices
//! apart — only the cert fingerprint, established once at pairing time,
//! can).
//!
//! `--resolution <WxH>` (e.g. `3840x2160`) narrows `--scale` to just that
//! resolution instead of setting the device's plain default — the same
//! physical device can legitimately want a different scale at different
//! requested resolutions (a TV streamed at 4K vs. the same TV/app streamed
//! at 1080p for a lower-bandwidth link). Requires `--scale`; run once per
//! resolution to build up a full set of overrides for one device, e.g.:
//!
//! ```text
//! redfog-pair --fingerprint ab12cd34 --scale 2   --resolution 3840x2160
//! redfog-pair --fingerprint ab12cd34 --scale 1.5 --resolution 1920x1080
//! ```
//!
//! `--name <NAME>` gives the device a human-readable label ("My TV"), shown
//! by `--list` alongside its fingerprint — purely descriptive, never looked
//! up by (only the fingerprint is a real identity). Works the same as
//! `--scale`: at pairing time or updated later via `--fingerprint`, and
//! independently of `--scale` (passing just `--name` with no PIN/`--scale`
//! is valid, unlike `--resolution` alone).
//!
//! `--list` also shows, per device: `last_seen_ip` (the address it last
//! paired or launched a stream from — not a stable identity, DHCP leases
//! change, just a hint for telling unnamed devices apart), the resolution
//! that most recent launch requested (which, since redfog has no live
//! resolution renegotiation, *is* the current resolution for a
//! currently-streaming session, not just history), every distinct
//! resolution ever seen from it, and its currently configured scale
//! (default plus any per-resolution overrides) — enough to answer "what do
//! I even need a `--resolution` override for" and "did my last `--scale`
//! actually take" without guessing.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut pin = None;
    let mut unique_id = None;
    let mut fingerprint = None;
    let mut scale = None;
    let mut resolution = None;
    let mut name = None;
    let mut list = false;
    let mut host = "127.0.0.1".to_string();
    let mut port = std::env::var("REDFOG_HTTP_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(47989u16);

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--uniqueid" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return usage_error("--uniqueid requires a value");
                };
                unique_id = Some(value.clone());
            }
            "--fingerprint" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return usage_error("--fingerprint requires a value");
                };
                fingerprint = Some(value.clone());
            }
            "--scale" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return usage_error("--scale requires a value");
                };
                let Ok(parsed) = value.parse::<f64>() else {
                    return usage_error(&format!("invalid --scale {value:?} (expected a plain number, e.g. 1.5)"));
                };
                if !parsed.is_finite() || parsed <= 0.0 {
                    return usage_error(&format!("invalid --scale {value:?} (must be a positive, finite number)"));
                }
                scale = Some(parsed);
            }
            "--resolution" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return usage_error("--resolution requires a value");
                };
                let Some((w, h)) = value.split_once('x') else {
                    return usage_error(&format!("invalid --resolution {value:?} (expected WIDTHxHEIGHT, e.g. 1920x1080)"));
                };
                let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) else {
                    return usage_error(&format!("invalid --resolution {value:?} (expected WIDTHxHEIGHT, e.g. 1920x1080)"));
                };
                if w == 0 || h == 0 {
                    return usage_error(&format!("invalid --resolution {value:?} (width/height must be nonzero)"));
                }
                resolution = Some((w, h));
            }
            "--name" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return usage_error("--name requires a value");
                };
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return usage_error("--name must not be empty");
                }
                name = Some(trimmed.to_string());
            }
            "--list" => list = true,
            "--host" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return usage_error("--host requires a value");
                };
                host = value.clone();
            }
            "--port" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return usage_error("--port requires a value");
                };
                let Ok(parsed) = value.parse() else {
                    return usage_error(&format!("invalid --port {value:?}"));
                };
                port = parsed;
            }
            "-h" | "--help" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            arg if pin.is_none() && !list => pin = Some(arg.to_string()),
            arg => return usage_error(&format!("unexpected argument {arg:?}")),
        }
        i += 1;
    }

    if list {
        return match list_paired(&host, port) {
            Ok(devices) if devices.is_empty() => {
                println!("no paired devices");
                ExitCode::SUCCESS
            }
            Ok(devices) => {
                for (i, d) in devices.iter().enumerate() {
                    if i > 0 {
                        println!();
                    }
                    let name = d.name.as_deref().unwrap_or("(unnamed -- set one with --name)");
                    println!("{}  {name}", d.fingerprint);
                    match &d.last_seen_ip {
                        Some(ip) => print!("  last seen: {ip}"),
                        None => print!("  last seen: never (paired but hasn't launched a stream yet)"),
                    }
                    match &d.last_seen_resolution {
                        Some(res) => println!(" @ {res} (current resolution, if it's streaming right now)"),
                        None => println!(),
                    }
                    if !d.seen_resolutions.is_empty() {
                        println!("  resolutions seen: {}", d.seen_resolutions.join(", "));
                    }
                    match (d.scale, d.scale_by_resolution.is_empty()) {
                        (None, true) => println!("  scale: not set (defaults to 1)"),
                        (default, _) => {
                            let default_str = default.map(|s| s.to_string()).unwrap_or_else(|| "not set (defaults to 1)".to_string());
                            let mut overrides: Vec<(&String, &f64)> = d.scale_by_resolution.iter().collect();
                            overrides.sort_by_key(|(res, _)| (*res).clone());
                            let overrides_str = overrides.iter().map(|(res, s)| format!("{res}={s}")).collect::<Vec<_>>().join(", ");
                            match overrides_str.is_empty() {
                                true => println!("  scale: default={default_str}"),
                                false => println!("  scale: default={default_str}, {overrides_str}"),
                            }
                        }
                    }
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("redfog-pair: {e}");
                ExitCode::FAILURE
            }
        };
    }

    if unique_id.is_some() && fingerprint.is_some() {
        return usage_error("pass only one of --uniqueid/--fingerprint, not both");
    }
    if pin.is_some() && fingerprint.is_some() {
        return usage_error("a PIN only makes sense with --uniqueid (pairing) -- an already-paired device (--fingerprint) has no PIN step left");
    }
    if pin.is_none() && scale.is_none() && name.is_none() {
        return usage_error("missing PIN (or --scale/--name, to update a device without one)");
    }
    if resolution.is_some() && scale.is_none() {
        return usage_error("--resolution requires --scale");
    }

    // `--fingerprint` always identifies an already-paired device directly
    // -- no auto-discovery needed or possible (unlike `--uniqueid`, it
    // isn't something a device is currently "waiting" under).
    let identity = match &fingerprint {
        Some(fp) => Identity::Fingerprint(fp.clone()),
        None => match unique_id {
            Some(id) => Identity::UniqueId(id),
            None => match pick_pending_client(&host, port) {
                Ok(id) => Identity::UniqueId(id),
                Err(e) => {
                    eprintln!("redfog-pair: {e}");
                    return ExitCode::FAILURE;
                }
            },
        },
    };

    match submit_pin(&host, port, &identity, pin.as_deref(), scale, resolution, name.as_deref()) {
        Ok(()) => {
            if pin.is_some() {
                let Identity::UniqueId(id) = &identity else { unreachable!("pin+fingerprint rejected above") };
                println!("paired {id} successfully");
            } else {
                let (label, still_pending) = match &identity {
                    Identity::Fingerprint(fp) => (fp.clone(), false),
                    Identity::UniqueId(id) => (id.clone(), true),
                };
                let suffix = if still_pending { " (still pending)" } else { "" };
                match (scale.is_some(), resolution, name.is_some()) {
                    (true, Some((w, h)), true) => println!("updated scale for {label}{suffix} at {w}x{h}, and its name"),
                    (true, Some((w, h)), false) => println!("updated scale for {label}{suffix} at {w}x{h}"),
                    (true, None, true) => println!("updated default scale and name for {label}{suffix}"),
                    (true, None, false) => println!("updated default scale for {label}{suffix}"),
                    (false, _, true) => println!("updated name for {label}{suffix}"),
                    (false, _, false) => unreachable!("main() already requires at least one of pin/scale/name"),
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("redfog-pair: failed: {e}");
            ExitCode::FAILURE
        }
    }
}

enum Identity {
    /// A device still mid-handshake.
    UniqueId(String),
    /// An already-paired device, by cert fingerprint (or a unique prefix
    /// of one — see `ClientManager::set_scale_for_paired`'s doc comment).
    Fingerprint(String),
}

/// Fetches `/pending-pairs` and, if exactly one client is waiting, returns
/// its `uniqueid`. Errors (with a helpful message) if zero or multiple are
/// waiting, since guessing wrong would just fail the actual pairing anyway.
fn pick_pending_client(host: &str, port: u16) -> Result<String, String> {
    let body = ureq::get(&format!("http://{host}:{port}/pending-pairs"))
        .call()
        .map_err(|e| format!("failed to reach redfog-server at {host}:{port}: {e}"))?
        .into_string()
        .map_err(|e| format!("failed to read /pending-pairs response: {e}"))?;
    let ids: Vec<&str> = body.lines().filter(|line| !line.is_empty()).collect();
    match ids.as_slice() {
        [] => Err("no client is currently waiting to pair -- start pairing from your Moonlight client first, then try again \
                    (or pass --fingerprint if you're updating an already-paired device -- see --list)"
            .to_string()),
        [only] => Ok(only.to_string()),
        many => Err(format!(
            "multiple clients are waiting to pair — pass --uniqueid to pick one:\n{}",
            many.iter().map(|id| format!("  {id}")).collect::<Vec<_>>().join("\n")
        )),
    }
}

/// Mirrors `redfog_moonlight::clients::PairedClientInfo` field-for-field —
/// kept as our own type (not a shared dependency on that crate) since this
/// binary only ever talks to it over HTTP, never links against it directly.
#[derive(serde::Deserialize)]
struct PairedDevice {
    fingerprint: String,
    name: Option<String>,
    last_seen_ip: Option<String>,
    last_seen_resolution: Option<String>,
    seen_resolutions: Vec<String>,
    scale: Option<f64>,
    scale_by_resolution: std::collections::HashMap<String, f64>,
}

/// Fetches `/paired-clients` — a JSON array (see `PairingServer::
/// paired_clients`'s doc comment for why JSON, not the flatter
/// tab-separated shape this used at first).
fn list_paired(host: &str, port: u16) -> Result<Vec<PairedDevice>, String> {
    let body = ureq::get(&format!("http://{host}:{port}/paired-clients"))
        .call()
        .map_err(|e| format!("failed to reach redfog-server at {host}:{port}: {e}"))?
        .into_string()
        .map_err(|e| format!("failed to read /paired-clients response: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("failed to parse /paired-clients response: {e}"))
}

/// `pin`/`scale`/`name` are each optional, but the caller (`main`) already
/// checked at least one is present — `/submit-pin` itself rejects a
/// request with none of them. `resolution` is only meaningful alongside
/// `scale` (also enforced by the caller before this is ever reached).
fn submit_pin(
    host: &str,
    port: u16,
    identity: &Identity,
    pin: Option<&str>,
    scale: Option<f64>,
    resolution: Option<(u32, u32)>,
    name: Option<&str>,
) -> Result<(), String> {
    let scale_str = scale.map(|s| s.to_string());
    let resolution_str = resolution.map(|(w, h)| format!("{w}x{h}"));
    let mut form = Vec::new();
    match identity {
        Identity::UniqueId(id) => form.push(("uniqueid", id.as_str())),
        Identity::Fingerprint(fp) => form.push(("fingerprint", fp.as_str())),
    }
    if let Some(pin) = pin {
        form.push(("pin", pin));
    }
    if let Some(scale_str) = &scale_str {
        form.push(("scale", scale_str));
    }
    if let Some(resolution_str) = &resolution_str {
        form.push(("resolution", resolution_str));
    }
    if let Some(name) = name {
        form.push(("name", name));
    }
    let response = ureq::post(&format!("http://{host}:{port}/submit-pin")).send_form(&form);
    match response {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(_, response)) => Err(response.into_string().unwrap_or_else(|e| format!("(and failed to read the error body: {e})"))),
        Err(e) => Err(e.to_string()),
    }
}

fn print_usage() {
    eprintln!("usage: redfog-pair <PIN> [--uniqueid <ID>] [--scale <FACTOR>] [--resolution <WxH>] [--name <NAME>] [--host <HOST>] [--port <PORT>]");
    eprintln!("   or: redfog-pair --fingerprint <FP> [--scale <FACTOR>] [--resolution <WxH>] [--name <NAME>] [--host <HOST>] [--port <PORT>]");
    eprintln!("   or: redfog-pair --list [--host <HOST>] [--port <PORT>]");
    eprintln!();
    eprintln!("Relays a Moonlight pairing PIN to redfog-server. If your Moonlight client");
    eprintln!("is currently showing a PIN and waiting to pair, just run:");
    eprintln!();
    eprintln!("    redfog-pair 1234");
    eprintln!();
    eprintln!("Pass --scale and/or --name to also set this device's HiDPI scale factor and");
    eprintln!("a human-readable label at pairing time:");
    eprintln!();
    eprintln!("    redfog-pair 1234 --scale 1.5 --name \"My TV\"");
    eprintln!();
    eprintln!("To update scale/name for a device that's already paired, use --fingerprint");
    eprintln!("(not --uniqueid -- a real client's uniqueid can't tell two paired devices");
    eprintln!("apart, see this binary's own doc comment). Run --list first to find it:");
    eprintln!();
    eprintln!("    redfog-pair --list");
    eprintln!("    redfog-pair --fingerprint ab12cd34 --scale 2");
    eprintln!("    redfog-pair --fingerprint ab12cd34 --name \"My TV\"");
    eprintln!();
    eprintln!("--fingerprint accepts a unique prefix, not just the full value. Add");
    eprintln!("--resolution to scope --scale to just that resolution instead of the");
    eprintln!("device's plain default -- useful if it streams at more than one resolution");
    eprintln!("and wants a different scale for each:");
    eprintln!();
    eprintln!("    redfog-pair --fingerprint ab12cd34 --scale 2   --resolution 3840x2160");
    eprintln!("    redfog-pair --fingerprint ab12cd34 --scale 1.5 --resolution 1920x1080");
    eprintln!();
    eprintln!("--list also shows each device's last-seen IP -- a hint, not a stable");
    eprintln!("identity, for telling unnamed devices apart.");
    eprintln!();
    eprintln!("--host/--port default to 127.0.0.1:47989 (or $REDFOG_HTTP_PORT).");
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("redfog-pair: {message}");
    print_usage();
    ExitCode::FAILURE
}
