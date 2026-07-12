//! Tiny CLI for relaying a Moonlight pairing PIN to `redfog-server` — the
//! same "read the PIN off the client, type it in somewhere" step a real
//! Moonlight app's own pairing dialog does, just from a terminal instead of
//! a browser form (`/pin`), since not every client (e.g. `moonlight-web`)
//! surfaces one.
//!
//! Usage: `redfog-pair <PIN> [--uniqueid <ID>] [--host <HOST>] [--port <PORT>]`
//!
//! `--uniqueid` is optional: `redfog-server` exposes which `uniqueid`(s) are
//! currently mid-handshake via `/pending-pairs` (not part of the real
//! Moonlight protocol — a small tooling hook added alongside this binary),
//! so if exactly one client is waiting, this picks it automatically instead
//! of requiring you to already know it (e.g. from grepping server logs,
//! which is how this used to be done by hand).

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut pin = None;
    let mut unique_id = None;
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
            arg if pin.is_none() => pin = Some(arg.to_string()),
            arg => return usage_error(&format!("unexpected argument {arg:?}")),
        }
        i += 1;
    }

    let Some(pin) = pin else {
        return usage_error("missing PIN");
    };

    let unique_id = match unique_id {
        Some(id) => id,
        None => match pick_pending_client(&host, port) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("redfog-pair: {e}");
                return ExitCode::FAILURE;
            }
        },
    };

    match submit_pin(&host, port, &unique_id, &pin) {
        Ok(()) => {
            println!("paired {unique_id} successfully");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("redfog-pair: failed to pair {unique_id}: {e}");
            ExitCode::FAILURE
        }
    }
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
        [] => Err("no client is currently waiting to pair — start pairing from your Moonlight client first, then try again".to_string()),
        [only] => Ok(only.to_string()),
        many => Err(format!(
            "multiple clients are waiting to pair — pass --uniqueid to pick one:\n{}",
            many.iter().map(|id| format!("  {id}")).collect::<Vec<_>>().join("\n")
        )),
    }
}

fn submit_pin(host: &str, port: u16, unique_id: &str, pin: &str) -> Result<(), String> {
    let response = ureq::post(&format!("http://{host}:{port}/submit-pin")).send_form(&[("uniqueid", unique_id), ("pin", pin)]);
    match response {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(_, response)) => Err(response.into_string().unwrap_or_else(|e| format!("(and failed to read the error body: {e})"))),
        Err(e) => Err(e.to_string()),
    }
}

fn print_usage() {
    eprintln!("usage: redfog-pair <PIN> [--uniqueid <ID>] [--host <HOST>] [--port <PORT>]");
    eprintln!();
    eprintln!("Relays a Moonlight pairing PIN to redfog-server. If your Moonlight client");
    eprintln!("is currently showing a PIN and waiting to pair, just run:");
    eprintln!();
    eprintln!("    redfog-pair 1234");
    eprintln!();
    eprintln!("--host/--port default to 127.0.0.1:47989 (or $REDFOG_HTTP_PORT).");
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("redfog-pair: {message}");
    print_usage();
    ExitCode::FAILURE
}
