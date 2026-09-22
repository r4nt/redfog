//! Fake Moonlight client: connects to the ENet control channel and sends a
//! real encrypted MouseMoveRelative event, over actual sockets. Validates
//! the ENet + AES-GCM decrypt + input-decode + InputForwarder wiring.
//!
//! Usage: cargo run --example fake_client_control -- <host:port> <rikey-hex>

use std::net::SocketAddrV4;
use std::time::{Duration, Instant};

use enet::{Address, BandwidthLimit, ChannelLimit, Enet, Event, Packet, PacketMode};

const CONTROL_MSG_ENCRYPTED: u16 = 0x0001;
const CONTROL_MSG_INPUT_DATA: u16 = 0x0206;
const MOUSE_MOVE_RELATIVE: u32 = 0x00000007;

fn gcm_encrypt(plaintext: &[u8], key: &[u8; 16], iv: &[u8; 12]) -> (Vec<u8>, [u8; 16]) {
    use aes_gcm::aead::AeadInPlace;
    use aes_gcm::{Aes128Gcm, Key, KeyInit, Nonce};
    let cipher = Aes128Gcm::new(Key::<Aes128Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(iv);
    let mut buffer = plaintext.to_vec();
    let tag = cipher.encrypt_in_place_detached(nonce, b"", &mut buffer).unwrap();
    (buffer, tag.into())
}

fn build_encrypted_message(key: &[u8; 16], sequence_number: u32, inner: &[u8]) -> Vec<u8> {
    let mut iv = [0u8; 12];
    iv[0..4].copy_from_slice(&sequence_number.to_le_bytes());
    // Serverbound marker (this simulates a client sending to the server).
    iv[10] = b'C';
    iv[11] = b'C';
    let (ciphertext, tag) = gcm_encrypt(inner, key, &iv);

    let mut wrapper_payload = Vec::new();
    wrapper_payload.extend(sequence_number.to_le_bytes());
    wrapper_payload.extend(tag);
    wrapper_payload.extend(ciphertext);

    let mut buffer = Vec::new();
    buffer.extend(CONTROL_MSG_ENCRYPTED.to_le_bytes());
    buffer.extend((wrapper_payload.len() as u16).to_le_bytes());
    buffer.extend(wrapper_payload);
    buffer
}

fn mouse_move_relative_message(dx: i16, dy: i16) -> Vec<u8> {
    let mut event = Vec::new();
    event.extend(MOUSE_MOVE_RELATIVE.to_le_bytes());
    event.extend(dx.to_be_bytes());
    event.extend(dy.to_be_bytes());

    let mut payload = Vec::new();
    payload.extend((event.len() as u32).to_be_bytes());
    payload.extend(&event);

    let mut message = Vec::new();
    message.extend(CONTROL_MSG_INPUT_DATA.to_le_bytes());
    message.extend((payload.len() as u16).to_le_bytes());
    message.extend(payload);
    message
}

// No tokio needed here (unlike the old tokio_enet version this replaced):
// the real `enet` crate's API is synchronous, matching the real C
// `libenet` it binds — see `control.rs`'s own doc comment on the
// `use enet::...` import for why redfog's control channel uses it now.
fn main() {
    let mut args = std::env::args().skip(1);
    let target_str = args.next().unwrap_or_else(|| "127.0.0.1:47999".to_string());
    // enet-rs's own `Address` type is IPv4-only -- see `ControlServer::
    // serve_blocking`'s own comment on the same constraint.
    let target: SocketAddrV4 = target_str.parse().expect("expected an IPv4 host:port, e.g. 127.0.0.1:47999");
    let rikey_hex = args.next().expect("usage: fake_client_control <host:port> <rikey-hex>");
    let key: [u8; 16] = hex::decode(rikey_hex).unwrap().try_into().unwrap();

    let enet_ctx = Enet::new().expect("initialize libenet");
    // `None` address + `create_host::<()>`: this is a client-only endpoint
    // (no per-peer associated data needed, unlike the real server side).
    let mut host = enet_ctx.create_host::<()>(None, 1, ChannelLimit::Maximum, BandwidthLimit::Unlimited, BandwidthLimit::Unlimited).expect("create enet client host");
    let address = Address::new(*target.ip(), target.port());
    host.connect(&address, 1, 0).expect("connect");
    println!("connecting to {target}...");

    let mut connected = false;
    let mut sent = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    'outer: while Instant::now() < deadline {
        // `flush()` can't be called while `event` (and the `peer` borrowed
        // out of it) are still alive -- unlike the old tokio_enet version
        // this replaced, `enet`'s own `Event`/`Peer` genuinely borrow from
        // `host`, so the flush has to wait until this whole `if let`
        // block's borrow ends.
        let mut just_sent = false;
        if let Ok(Some(mut event)) = host.service(200) {
            match event {
                Event::Connect(ref mut peer) => {
                    println!("connected, sending 20 mouse-move events...");
                    connected = true;
                    for i in 0..20u32 {
                        let msg = build_encrypted_message(&key, i, &mouse_move_relative_message(5, 0));
                        if let Ok(packet) = Packet::new(&msg, PacketMode::ReliableSequenced) {
                            let _ = peer.send_packet(packet, 0);
                        }
                    }
                    sent = true;
                    just_sent = true;
                }
                Event::Disconnect(..) => break 'outer,
                _ => {}
            }
        }
        if just_sent {
            host.flush();
        }
    }
    // Keep servicing the connection for a few more seconds so ENet's reliable
    // retransmission actually has a chance to complete before we exit and
    // drop the socket.
    let linger_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < linger_deadline {
        let _ = host.service(200);
    }

    if connected && sent {
        println!("SENT mouse-move events over the real ENet control channel.");
    } else {
        println!("FAILED: connected={connected} sent={sent}");
        std::process::exit(1);
    }
}
