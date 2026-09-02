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
//!
//! [`MultiClientUdpSender::send_batch_to`] exists alongside the plain
//! per-buffer [`MultiClientUdpSender::send_to`] because a single encoded
//! video frame can legitimately need many separate UDP datagrams — the
//! wire protocol's fixed ~1KB per-packet payload size (see `video.rs`'s
//! `REQUESTED_PACKET_SIZE`) isn't something we control, so a large frame
//! (a keyframe especially) means a correspondingly large *number* of
//! individually-tiny `sendto`s, all for data that was already fully
//! computed and ready to go before the first one was even issued.
//! Confirmed live via CPU profiling that `sendto` was the single largest
//! syscall category in a real capture (~28k calls across ~1.4k frames,
//! averaging ~20 shards/frame) — `sendmmsg(2)` collapses a whole frame's
//! worth of already-ready shards into one syscall instead of one per
//! shard, with zero change to what's actually sent on the wire (same
//! datagrams, same boundaries, same content) and zero added latency
//! (nothing is delayed to accumulate a batch — the "batch" is just
//! whatever was already sitting fully-formed in memory).

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

    fn learned_addr(&self, ping_token: [u8; 16]) -> Result<SocketAddr, String> {
        let s = self.state.lock().unwrap();
        s.addresses
            .get(&ping_token)
            .copied()
            .ok_or_else(|| format!("{} client address not yet known for this session (wait_for_client not called/completed)", self.label))
    }

    pub async fn send_to(&self, ping_token: [u8; 16], data: &[u8]) -> Result<(), String> {
        let addr = self.learned_addr(ping_token)?;
        self.socket.send_to(data, addr).await.map_err(|e| format!("{} send failed: {e}", self.label))?;
        Ok(())
    }

    /// Sends every buffer in `data` to the same learned destination as one
    /// `sendmmsg(2)` batch instead of one `send_to`/`sendto` syscall per
    /// buffer — see this module's own doc comment addendum below for why
    /// this exists at all. Same wire output either way (each buffer is
    /// still its own, independent UDP datagram — nothing about packet
    /// boundaries or count changes), just fewer syscalls to get there.
    ///
    /// `tokio::net::UdpSocket` has no `sendmmsg` of its own (it's a
    /// Linux-specific syscall outside tokio's cross-platform socket API),
    /// so this drives the raw syscall by hand through `try_io` — tokio's
    /// own documented pattern for "do a raw syscall myself, still
    /// correctly integrated with the runtime's readiness tracking" (as
    /// opposed to a bare `libc` call, which could block the whole runtime
    /// thread if the send buffer were ever actually full).
    pub async fn send_batch_to(&self, ping_token: [u8; 16], data: &[Vec<u8>]) -> Result<(), String> {
        if data.is_empty() {
            return Ok(());
        }
        let addr = self.learned_addr(ping_token)?;
        let dest = socket2::SockAddr::from(addr);

        let mut remaining = data;
        while !remaining.is_empty() {
            self.socket.writable().await.map_err(|e| format!("{} writable: {e}", self.label))?;
            match self.socket.try_io(tokio::io::Interest::WRITABLE, || send_mmsg_batch(&self.socket, &dest, remaining)) {
                Ok(sent) => remaining = &remaining[sent..],
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(format!("{} sendmmsg failed: {e}", self.label)),
            }
        }
        Ok(())
    }
}

/// The actual `sendmmsg(2)` call, run from inside `UdpSocket::try_io` (see
/// `send_batch_to`) so its `WouldBlock` return is what tells tokio's
/// readiness tracking to wait and retry, same as any other non-blocking
/// socket op. Returns how many of `buffers` were actually accepted by the
/// kernel this call (can be fewer than `buffers.len()` if the send buffer
/// fills mid-batch — the caller retries the remainder after the socket is
/// writable again, same as a partial `write()`).
///
/// # Safety-adjacent notes
/// `iov_base`/`msg_name` are `*mut` fields on `iovec`/`msghdr` for
/// symmetry with the read side of these same C structs (`recvmmsg` writes
/// through them) — `sendmmsg` only ever reads through them here, so
/// casting away `const` is standard practice for this FFI, not actually
/// unsound.
fn send_mmsg_batch(socket: &UdpSocket, dest: &socket2::SockAddr, buffers: &[Vec<u8>]) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd;

    let mut iovecs: Vec<libc::iovec> =
        buffers.iter().map(|b| libc::iovec { iov_base: b.as_ptr() as *mut _, iov_len: b.len() }).collect();
    let mut msgs: Vec<libc::mmsghdr> = iovecs
        .iter_mut()
        .map(|iov| libc::mmsghdr {
            msg_hdr: libc::msghdr {
                msg_name: dest.as_ptr() as *mut _,
                msg_namelen: dest.len(),
                msg_iov: iov,
                msg_iovlen: 1,
                msg_control: std::ptr::null_mut(),
                msg_controllen: 0,
                msg_flags: 0,
            },
            msg_len: 0,
        })
        .collect();

    let sent = unsafe { libc::sendmmsg(socket.as_raw_fd(), msgs.as_mut_ptr(), msgs.len() as u32, 0) };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(sent as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Registers `client`'s address the same way a real client's PING
    /// does (see `bind`'s receive loop) — a 20-byte `[16-byte token][4-byte
    /// seq]` datagram, source address is what gets learned.
    async fn register(sender: &MultiClientUdpSender, client: &UdpSocket, sender_addr: SocketAddr, token: [u8; 16]) {
        let mut ping = [0u8; PING_PAYLOAD_PACKET_SIZE];
        ping[..16].copy_from_slice(&token);
        client.send_to(&ping, sender_addr).await.expect("send ping");
        tokio::time::timeout(Duration::from_secs(2), sender.wait_for_client(token)).await.expect("ping arrived").expect("wait_for_client");
    }

    #[tokio::test]
    async fn send_batch_to_delivers_every_buffer_as_its_own_datagram() {
        let sender = MultiClientUdpSender::bind("127.0.0.1".parse().unwrap(), 0, "test").await.unwrap();
        let sender_addr = sender.socket.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let token = [7u8; 16];
        register(&sender, &client, sender_addr, token).await;

        // More than a couple, to exercise the real sendmmsg array path
        // (not just a degenerate 1-2 element case).
        let buffers: Vec<Vec<u8>> = (0..40u8).map(|i| vec![i; 50 + i as usize]).collect();
        sender.send_batch_to(token, &buffers).await.expect("send_batch_to");

        let mut received: Vec<Vec<u8>> = Vec::new();
        let mut buf = [0u8; 2048];
        for _ in 0..buffers.len() {
            let (len, from) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf)).await.expect("recv before timeout").expect("recv_from");
            assert_eq!(from, sender_addr, "datagram should come from the sender's own socket");
            received.push(buf[..len].to_vec());
        }

        // UDP doesn't guarantee ordering in general, but a single
        // sendmmsg batch over loopback is realistically always delivered
        // in submission order -- asserting the stronger (order-preserving)
        // property here is deliberate: an ordering regression in the
        // batch-construction logic (e.g. iovecs pointing at the wrong
        // buffer) would otherwise still pass an order-independent check.
        assert_eq!(received, buffers, "every buffer should arrive intact, in submission order");
    }

    #[tokio::test]
    async fn send_batch_to_with_no_buffers_is_a_harmless_no_op() {
        let sender = MultiClientUdpSender::bind("127.0.0.1".parse().unwrap(), 0, "test").await.unwrap();
        let sender_addr = sender.socket.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let token = [9u8; 16];
        register(&sender, &client, sender_addr, token).await;

        sender.send_batch_to(token, &[]).await.expect("empty batch should succeed trivially");
    }
}
