//! Ad-hoc test for `BrokerRequest::SpawnPayload` against a real, running
//! broker (started via `sudo`) — exercises the "caller already owns the
//! socket/runtime dir, broker just grants access + spawns a payload"
//! path, as opposed to `SpawnSession`'s "broker creates everything"
//! (already covered by `redfog-moonlight`'s `connection_integration` test).
//!
//! Usage: create a runtime dir + dummy socket file as your own unprivileged
//! user (simulating redfog-moonlight owning them), then:
//!
//!   REDFOG_BROKER_SOCKET=/tmp/redfog-runtime/broker.sock \
//!   cargo run -p redfog-broker --example spawn_payload_test -- <username> <runtime_dir> <socket_path>
//!
//! Spawns `bash -c 'id > $runtime_dir/spawn-payload-proof; touch $runtime_dir/sway.socket'`
//! as `<username>` and prints the proof file's contents — confirms both
//! that the ACL grants actually worked (the target user could write into a
//! dir it doesn't own) and that privilege-drop happened correctly (proof
//! file's `id` output should show the target user's real uid/groups).

use redfog_broker_protocol::{read_response, write_request, BrokerRequest, BrokerResponse};
use tokio::io::BufReader;
use tokio::net::UnixStream;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: {} <username> <runtime_dir> <socket_path>", args[0]);
        std::process::exit(2);
    }
    let username = &args[1];
    let runtime_dir = &args[2];
    let socket_path = &args[3];

    let broker_socket = std::env::var("REDFOG_BROKER_SOCKET").unwrap_or_else(|_| "/tmp/redfog-runtime/broker.sock".to_string());
    let stream = UnixStream::connect(&broker_socket).await?;
    let mut reader = BufReader::new(stream);

    let proof_path = format!("{runtime_dir}/spawn-payload-proof");
    let request = BrokerRequest::SpawnPayload {
        session_id: "spawn-payload-test-0".to_string(),
        username: username.clone(),
        socket_path: socket_path.clone(),
        runtime_dir: runtime_dir.clone(),
        argv: vec![
            "bash".to_string(),
            "-c".to_string(),
            format!("id > {proof_path}; touch {runtime_dir}/sway.socket; sleep 5"),
        ],
        env: vec![],
    };

    write_request(&mut reader, &request).await?;
    match read_response(&mut reader).await? {
        BrokerResponse::SpawnPayload(Ok(())) => println!("SpawnPayload: ok, spawned"),
        BrokerResponse::SpawnPayload(Err(e)) => {
            eprintln!("SpawnPayload failed: {e}");
            std::process::exit(1);
        }
        other => {
            eprintln!("unexpected response: {other:?}");
            std::process::exit(1);
        }
    }

    // Give the payload a moment to actually run and write the proof file.
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    match std::fs::read_to_string(&proof_path) {
        Ok(contents) => println!("proof file {proof_path}:\n{contents}"),
        Err(e) => {
            eprintln!("failed to read proof file {proof_path}: {e}");
            std::process::exit(1);
        }
    }

    Ok(())
}
