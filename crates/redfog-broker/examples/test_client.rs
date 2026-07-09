//! Throwaway diagnostic: exercises the broker's Authenticate request path
//! against a running broker instance (REDFOG_BROKER_SOCKET).

use redfog_broker_protocol::{read_response, write_request, BrokerRequest};
use tokio::io::BufReader;
use tokio::net::UnixStream;

#[tokio::main]
async fn main() {
    let path = std::env::var("REDFOG_BROKER_SOCKET").expect("set REDFOG_BROKER_SOCKET");
    let username = std::env::args().nth(1).expect("usage: test_client <username> <password>");
    let password = std::env::args().nth(2).expect("usage: test_client <username> <password>");

    let stream = UnixStream::connect(&path).await.expect("connect to broker");
    let mut reader = BufReader::new(stream);

    write_request(&mut reader, &BrokerRequest::Authenticate { username, password })
        .await
        .expect("write request");
    let response = read_response(&mut reader).await.expect("read response");
    println!("{response:?}");
}
