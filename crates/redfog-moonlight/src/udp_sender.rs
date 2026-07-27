//! A UDP socket bound *once*, shared across every concurrent session —
//! sessions are distinguished by learned per-client destination address, not
//! by port. See `CONCURRENT_SESSIONS.md` for why this shape (one shared
//! socket per stream type, sessions multiplexed by remembered peer address,
//! not per-session port allocation).
//!
//! Keyed by a per-launch random `ping_token: [u8; 16]` (see
//! `SessionOrigin`'s doc comment), not by `client_ip` or `RunningSession::
//! generation`. Two earlier versions of this keyed by those instead —
//! both wrong, for different reasons:
//!
//! - Keying by `generation` broke a Login -> User handoff: the User stage
//!   mints a *new* generation for the same physical client, with no new
//!   `PING` to learn an address from (real clients stop pinging once they
//!   receive their first packet — see moonlight-common-rust's
//!   `PingSender::set_finished` — and the handoff itself is entirely
//!   server-side).
//! - Keying by `client_ip` can't tell two *different* concurrent clients
//!   apart when they share a source address (same NAT, or literally the
//!   same machine) — a real, live concern, not hypothetical.
//!
//! `ping_token` fixes both: it's stable across a Login -> User handoff (the
//! same physical client, same token, carried forward in `SessionOrigin`),
//! and it's unambiguous even when two sessions share a source IP, because
//! it's echoed back by the client itself in every `PING` it sends — this
//! relies on an existing, already-supported wire-protocol extension (see
//! `rtsp.rs`'s `SETUP` handling), not a redfog invention. The client learns
//! the token from our RTSP `SETUP` response and echoes it in a 20-byte
//! packet (`[u8; 16] token ++ [u8; 4] BE sequence_number`) instead of the
//! bare 4-byte `"PING"` magic it'd otherwise send — see
//! moonlight-common-rust's `ping.rs`/`packet.rs`.
//!
//! Replaces the older design where `VideoSender`/`AudioSender` each bound
//! their own socket per launch and tracked exactly one learned
//! `client_addr` — fine when only one session could ever be active, not
//! once more than one needs to stream concurrently.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use tokio::net::UdpSocket;

/// The echoed-token ping packet's wire size — 16-byte token + 4-byte BE
/// sequence number. See moonlight-common-rust's `stream::proto::packet`
/// (not vendored into git, see scripts/fetch-patched-deps.sh).
const PING_PAYLOAD_PACKET_SIZE: usize = 20;

#[derive(Default)]
struct SharedState {
    /// Learned destination address per `ping_token`, once its `PING`
    /// arrives.
    addresses: HashMap<[u8; 16], SocketAddr>,
    /// Wakes up `wait_for_client` once that token's address is learned.
    waiters: HashMap<[u8; 16], tokio::sync::oneshot::Sender<SocketAddr>>,
}

/// One shared UDP socket (video, or audio) plus the per-client
/// destination-address bookkeeping needed to multiplex multiple concurrent
/// sessions onto it.
pub struct MultiClientUdpSender {
    label: &'static str,
    socket: Arc<UdpSocket>,
    state: Arc<Mutex<SharedState>>,
}

impl MultiClientUdpSender {
    /// Binds the socket and spawns the one shared receive loop that lives
    /// for the process's lifetime, dispatching incoming `PING`s to
    /// whichever client its echoed `ping_token` identifies.
    pub async fn bind(bind_addr: IpAddr, port: u16, label: &'static str) -> Result<Self, String> {
        let socket = Arc::new(
            UdpSocket::bind((bind_addr, port))
                .await
                .map_err(|e| format!("failed to bind {label} udp {bind_addr}:{port}: {e}"))?,
        );
        let state = Arc::new(Mutex::new(SharedState::default()));

        let recv_socket = socket.clone();
        let recv_state = state.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            loop {
                let (len, addr) = match recv_socket.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("{label} udp recv failed: {e}");
                        continue;
                    }
                };
                // A bare 4-byte "PING" (no echoed token) carries nothing to
                // match on at all — this only happens if some client
                // doesn't honor the SETUP response header that requests it,
                // which every session we hand out now always sets (see
                // rtsp.rs). Nothing to route it to; just drop it.
                if len != PING_PAYLOAD_PACKET_SIZE {
                    tracing::trace!("{label}: ignoring {len}-byte datagram from {addr} (expected a {PING_PAYLOAD_PACKET_SIZE}-byte ping)");
                    continue;
                }
                let mut token = [0u8; 16];
                token.copy_from_slice(&buf[0..16]);
                let mut s = recv_state.lock().unwrap();
                s.addresses.insert(token, addr);
                if let Some(waiter) = s.waiters.remove(&token) {
                    let _ = waiter.send(addr);
                }
            }
        });

        Ok(Self { label, socket, state })
    }

    /// Forgets any previously-learned address for `ping_token` — call when
    /// this client's session ends, or before `wait_for_client` on a
    /// reconnect/takeover. Without this, a stale `PING` the *previous*
    /// session already sent (UDP has no connection teardown to stop it, and
    /// nothing was reading from this socket between sessions to consume it)
    /// gets picked up as if it belonged to the new one, permanently
    /// misrouting the stream to a now-stale destination — confirmed live: a
    /// fresh reference-client connection received the *old* session's
    /// address here and got zero video/audio frames for the entire session.
    pub fn forget(&self, ping_token: [u8; 16]) {
        let mut s = self.state.lock().unwrap();
        s.addresses.remove(&ping_token);
        s.waiters.remove(&ping_token);
    }

    /// Blocks until `ping_token`'s `PING` datagram arrives (or returns
    /// immediately if already learned), recording its address for
    /// subsequent sends. Call once after `PLAY`, before frames start
    /// flowing. No timeout here (matches the old behavior) — callers wrap
    /// this in their own timeout.
    pub async fn wait_for_client(&self, ping_token: [u8; 16]) -> Result<SocketAddr, String> {
        let rx = {
            let mut s = self.state.lock().unwrap();
            if let Some(&addr) = s.addresses.get(&ping_token) {
                return Ok(addr);
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            s.waiters.insert(ping_token, tx);
            rx
        };
        rx.await.map_err(|_| format!("{} sender dropped while waiting for client ping", self.label))
    }

    pub async fn send_to(&self, ping_token: [u8; 16], data: &[u8]) -> Result<(), String> {
        let addr = {
            let s = self.state.lock().unwrap();
            *s.addresses
                .get(&ping_token)
                .ok_or_else(|| format!("{} client address not yet known for this session (wait_for_client not called/completed)", self.label))?
        };
        self.socket.send_to(data, addr).await.map_err(|e| format!("{} send failed: {e}", self.label))?;
        Ok(())
    }
}
