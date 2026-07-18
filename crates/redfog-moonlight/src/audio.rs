//! Audio packetization + UDP sender (48000/udp).
//!
//! Simpler than video: one RTP-style 12-byte header (`0x80`, payload type
//! `97`, sequence number, timestamp, ssrc=0) directly followed by one Opus
//! frame, one packet per frame. Real Sunshine/moonshine also group every 4
//! packets into a Reed-Solomon FEC block (2 parity shards with a specific
//! parity matrix Moonlight's client expects) for loss recovery; this
//! iteration sends redundancy=0 (only the 4 data packets, no parity), the
//! same "valid degenerate case" choice made for video in `video.rs`.
//!
//! Layout derived from reading a known-working implementation's wire code
//! (not vendored), see the plan doc for context.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::UdpSocket;

const RTP_HEADER_SIZE: usize = 12;
const AUDIO_PAYLOAD_TYPE: u8 = 97;

pub struct AudioPacketizer {
    sequence_number: u16,
}

impl Default for AudioPacketizer {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioPacketizer {
    pub fn new() -> Self {
        Self { sequence_number: 0 }
    }

    /// Encrypt one Opus frame (AES-128-CBC + PKCS7, base-protocol audio
    /// encryption — see `crypto::cbc_encrypt`'s doc comment for why this
    /// isn't optional) and wrap it in Moonlight's audio RTP-style header.
    /// `key` is the client's `rikey`; `key_id` is `rikeyid`. The IV's first 4
    /// bytes must be `key_id + this packet's RTP sequence number` — computed
    /// here, *before* `packetize` assigns and increments that same sequence
    /// number, so the two stay in lockstep the way the client's depayloader
    /// expects (it derives the same IV from the header's own sequence
    /// number).
    pub fn packetize_encrypted(&mut self, opus_frame: &[u8], rtp_timestamp: u32, key: &[u8; 16], key_id: u32) -> Vec<u8> {
        let mut iv = [0u8; 16];
        iv[0..4].copy_from_slice(&key_id.wrapping_add(self.sequence_number as u32).to_be_bytes());
        let ciphertext = crate::crypto::cbc_encrypt(opus_frame, key, &iv);
        self.packetize(&ciphertext, rtp_timestamp)
    }

    /// Wrap one Opus frame in Moonlight's audio RTP-style header.
    pub fn packetize(&mut self, opus_frame: &[u8], rtp_timestamp: u32) -> Vec<u8> {
        let mut packet = vec![0u8; RTP_HEADER_SIZE + opus_frame.len()];
        packet[0] = 0x80;
        packet[1] = AUDIO_PAYLOAD_TYPE;
        packet[2..4].copy_from_slice(&self.sequence_number.to_be_bytes());
        packet[4..8].copy_from_slice(&rtp_timestamp.to_be_bytes());
        packet[8..12].copy_from_slice(&0u32.to_be_bytes()); // ssrc
        packet[RTP_HEADER_SIZE..].copy_from_slice(opus_frame);

        self.sequence_number = self.sequence_number.wrapping_add(1);
        packet
    }
}

/// Same "wait for the client's `PING`" pattern as `VideoSender` — see there
/// for why the address isn't known upfront.
pub struct AudioSender {
    socket: Arc<UdpSocket>,
    client_addr: std::sync::Mutex<Option<SocketAddr>>,
}

impl AudioSender {
    pub async fn bind(bind_addr: std::net::IpAddr, port: u16) -> Result<Self, String> {
        let socket = UdpSocket::bind((bind_addr, port))
            .await
            .map_err(|e| format!("failed to bind audio udp {}:{}: {e}", bind_addr, port))?;
        Ok(Self {
            socket: Arc::new(socket),
            client_addr: std::sync::Mutex::new(None),
        })
    }

    /// See `VideoSender::drain_pending`'s doc comment — same reasoning,
    /// same bug, same fix.
    pub fn drain_pending(&self) {
        let mut buf = [0u8; 1024];
        while self.socket.try_recv_from(&mut buf).is_ok() {}
    }

    pub async fn wait_for_client(&self) -> Result<SocketAddr, String> {
        let mut buf = [0u8; 1024];
        loop {
            let (len, addr) = self
                .socket
                .recv_from(&mut buf)
                .await
                .map_err(|e| format!("audio udp recv failed: {e}"))?;
            if &buf[..len] == b"PING" {
                *self.client_addr.lock().unwrap() = Some(addr);
                return Ok(addr);
            }
            tracing::trace!("ignoring unexpected {len}-byte datagram on audio port before PING");
        }
    }

    pub async fn send_packet(&self, packet: &[u8]) -> Result<(), String> {
        let addr = self
            .client_addr
            .lock()
            .unwrap()
            .ok_or("audio client address not yet known (wait_for_client not called/completed)")?;
        self.socket
            .send_to(packet, addr)
            .await
            .map_err(|e| format!("audio send failed: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packetize_prepends_header_and_increments_sequence() {
        let mut packetizer = AudioPacketizer::new();
        let opus_frame = vec![0xAAu8; 40];

        let p0 = packetizer.packetize(&opus_frame, 1000);
        assert_eq!(p0.len(), RTP_HEADER_SIZE + opus_frame.len());
        assert_eq!(p0[0], 0x80);
        assert_eq!(p0[1], AUDIO_PAYLOAD_TYPE);
        assert_eq!(u16::from_be_bytes([p0[2], p0[3]]), 0);
        assert_eq!(&p0[RTP_HEADER_SIZE..], &opus_frame[..]);

        let p1 = packetizer.packetize(&opus_frame, 1960);
        assert_eq!(u16::from_be_bytes([p1[2], p1[3]]), 1);
    }

    /// Guards the two bugs found live: audio sent as plaintext (the client
    /// unconditionally AES-128-CBC-decrypts, regardless of the
    /// `encryptionSupported` SDP flags), and the IV not actually advancing
    /// per-packet the way the client's depayloader expects (derived from
    /// `rikeyid` + this packet's own RTP sequence number, computed
    /// independently here — not by calling `crypto::cbc_encrypt`'s own IV
    /// math back at it).
    #[test]
    fn packetize_encrypted_round_trips_and_varies_iv_per_sequence_number() {
        use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};

        fn decrypt(ciphertext: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Vec<u8> {
            type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
            let mut buffer = ciphertext.to_vec();
            Aes128CbcDec::new(key.into(), iv.into())
                .decrypt_padded_mut::<Pkcs7>(&mut buffer)
                .expect("valid pkcs7 padding")
                .to_vec()
        }

        let key = [0x7au8; 16];
        let key_id: u32 = 2260590725; // a real rikeyid value seen live
        let mut packetizer = AudioPacketizer::new();
        let frame0 = vec![0x11u8; 100];
        let frame1 = vec![0x22u8; 40]; // different size, like real VBR Opus

        let p0 = packetizer.packetize_encrypted(&frame0, 1000, &key, key_id);
        let p1 = packetizer.packetize_encrypted(&frame1, 1005, &key, key_id);

        // Never plaintext on the wire.
        assert_ne!(&p0[RTP_HEADER_SIZE..], &frame0[..]);

        let seq0 = u16::from_be_bytes([p0[2], p0[3]]);
        let seq1 = u16::from_be_bytes([p1[2], p1[3]]);
        assert_eq!((seq0, seq1), (0, 1));

        let mut iv0 = [0u8; 16];
        iv0[0..4].copy_from_slice(&key_id.wrapping_add(seq0 as u32).to_be_bytes());
        let mut iv1 = [0u8; 16];
        iv1[0..4].copy_from_slice(&key_id.wrapping_add(seq1 as u32).to_be_bytes());

        assert_eq!(decrypt(&p0[RTP_HEADER_SIZE..], &key, &iv0), frame0);
        assert_eq!(decrypt(&p1[RTP_HEADER_SIZE..], &key, &iv1), frame1);
    }
}
