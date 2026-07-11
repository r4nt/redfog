//! Ad-hoc test for `BrokerRequest::ReadUserSessionConfig` against a real,
//! running broker (started via `sudo`) — confirms both the "file exists"
//! and "file missing" cases actually work against a real user's home
//! directory, past normal `700` permissions the broker's own root
//! privilege reads through.
//!
//! Usage:
//!   REDFOG_BROKER_SOCKET=/tmp/redfog-broker-test.sock \
//!   cargo run -p redfog-broker --example read_user_session_config_test -- <username>

use redfog_broker_protocol::{read_response, write_request, BrokerRequest, BrokerResponse};
use tokio::io::BufReader;
use tokio::net::UnixStream;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: {} <username>", args[0]);
        std::process::exit(2);
    }
    let username = &args[1];

    let broker_socket = std::env::var("REDFOG_BROKER_SOCKET").unwrap_or_else(|_| "/tmp/redfog-runtime/broker.sock".to_string());
    let stream = UnixStream::connect(&broker_socket).await?;
    let mut reader = BufReader::new(stream);

    write_request(&mut reader, &BrokerRequest::ReadUserSessionConfig { username: username.clone() }).await?;
    match read_response(&mut reader).await? {
        BrokerResponse::ReadUserSessionConfig(Ok(Some(config))) => {
            println!("found: {config:#?}");
        }
        BrokerResponse::ReadUserSessionConfig(Ok(None)) => {
            println!("not found (Ok(None)) — no ~/.config/redfog/session.toml for {username}");
        }
        BrokerResponse::ReadUserSessionConfig(Err(e)) => {
            println!("error: {e}");
        }
        other => println!("unexpected response: {other:?}"),
    }

    Ok(())
}
