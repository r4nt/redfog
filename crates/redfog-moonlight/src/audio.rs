//! Audio packetization + UDP sender (48000/udp).
//!
//! One RTP-style 12-byte header (`0x80`, payload type `97`, sequence number,
//! timestamp, ssrc=0) directly followed by one Opus frame, one packet per
//! frame. Real Sunshine/GFE also group every 4 data packets into a
//! Reed-Solomon FEC block (2 parity shards, packet type `127`, each preceded
//! by its own 12-byte `AudioFecHeader`) for loss recovery — implemented here
//! too, using the exact non-default parity matrix real clients expect (see
//! `AUDIO_FEC_PARITY_MATRIX`'s doc comment). This requires every shard in a
//! group to be the same length, which is why `redfog-core`'s audio pipeline
//! forces constant-bitrate Opus (`bitrate-type=cbr`) — see that pipeline
//! description's doc comment.
//!
//! Layout derived from reading a known-working implementation's wire code
//! (not vendored), see the plan doc for context.

use std::net::{IpAddr, SocketAddr};

use crate::udp_sender::MultiClientUdpSender;

const RTP_HEADER_SIZE: usize = 12;
const AUDIO_PAYLOAD_TYPE: u8 = 97;
const AUDIO_PAYLOAD_TYPE_FEC: u8 = 127;
const AUDIO_FEC_HEADER_SIZE: usize = 12;
const AUDIO_FEC_PAYLOAD_OFFSET: usize = RTP_HEADER_SIZE + AUDIO_FEC_HEADER_SIZE;
const AUDIO_DATA_SHARDS: usize = 4;
const AUDIO_FEC_SHARDS: usize = 2;

/// The Reed-Solomon parity matrix real Moonlight clients expect for audio
/// FEC reconstruction — NOT the matrix a generic Reed-Solomon library would
/// derive on its own from `(4, 2)`. Sourced from real moonlight-common-c's
/// `RtpAudioQueue.c` (inherited from OpenFEC) and confirmed against
/// moonlight-common-rust's own `create_audio_reed_solomon`. Using any other
/// matrix would produce parity shards a real client can compute checksums
/// against but can never actually reconstruct data from.
const AUDIO_FEC_PARITY_MATRIX: [u8; AUDIO_FEC_SHARDS * AUDIO_DATA_SHARDS] = [0x77, 0x40, 0x38, 0x0e, 0xc7, 0xa7, 0x0d, 0x6c];

fn new_audio_reed_solomon() -> fec_rs::ReedSolomon {
    let mut reed_solomon = fec_rs::ReedSolomon::new(AUDIO_DATA_SHARDS, AUDIO_FEC_SHARDS).expect("4 data + 2 parity shards is a valid Reed-Solomon configuration");
    reed_solomon.set_parity_matrix(&AUDIO_FEC_PARITY_MATRIX).expect("matrix has exactly parity_shards * data_shards elements");
    reed_solomon
}

pub struct AudioPacketizer {
    sequence_number: u16,
    /// Timestamp of the first data packet in the in-progress group of 4 —
    /// carried in each FEC packet's `AudioFecHeader` so the client can
    /// reconstruct a lost packet's timestamp too, not just its payload.
    base_timestamp: u32,
    /// Post-encryption payload bytes of the in-progress group of 4 data
    /// packets, indexed by `sequence_number % 4` — fed to Reed-Solomon once
    /// the 4th packet of the group completes it.
    data_shards: [Vec<u8>; AUDIO_DATA_SHARDS],
    reed_solomon: fec_rs::ReedSolomon,
    /// Diagnostic escape hatch (`REDFOG_DISABLE_AUDIO_FEC=1`, see
    /// sudo-live-session.sh): real Sunshine/GFE reuse each FEC packet's
    /// sequence number with the next data packet's (by design — see
    /// `generate_fec_packets`'s doc comment), which official clients
    /// discriminate by packet type before tracking sequence continuity.
    /// Suspected of confusing at least one non-reference client
    /// (moonlight-web) into treating the reused sequence number as a
    /// stale/duplicate packet and stalling its jitter buffer -- toggle
    /// this off to confirm/rule that out without a full revert.
    fec_enabled: bool,
}

impl Default for AudioPacketizer {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioPacketizer {
    pub fn new() -> Self {
        Self {
            sequence_number: 0,
            base_timestamp: 0,
            data_shards: std::array::from_fn(|_| Vec::new()),
            reed_solomon: new_audio_reed_solomon(),
            fec_enabled: std::env::var_os("REDFOG_DISABLE_AUDIO_FEC").is_none(),
        }
    }

    /// Encrypt one Opus frame (AES-128-CBC + PKCS7, base-protocol audio
    /// encryption — see `crypto::cbc_encrypt`'s doc comment for why this
    /// isn't optional) and wrap it in Moonlight's audio RTP-style header,
    /// generating the group's 2 FEC packets too once every 4th data packet
    /// completes a group. `key` is the client's `rikey`; `key_id` is
    /// `rikeyid`. The IV's first 4 bytes must be `key_id + this packet's RTP
    /// sequence number` — computed here, *before* `packetize` assigns and
    /// increments that same sequence number, so the two stay in lockstep the
    /// way the client's depayloader expects (it derives the same IV from the
    /// header's own sequence number).
    ///
    /// Returns the packets to send, in wire order: always the one data
    /// packet, followed by the group's 2 FEC packets on every 4th call.
    pub fn packetize_encrypted(&mut self, opus_frame: &[u8], rtp_timestamp: u32, key: &[u8; 16], key_id: u32) -> Vec<Vec<u8>> {
        let mut iv = [0u8; 16];
        iv[0..4].copy_from_slice(&key_id.wrapping_add(self.sequence_number as u32).to_be_bytes());
        let ciphertext = crate::crypto::cbc_encrypt(opus_frame, key, &iv);
        self.packetize(&ciphertext, rtp_timestamp)
    }

    /// Wrap one Opus frame in Moonlight's audio RTP-style header. See
    /// `packetize_encrypted` for the FEC-packet-generation behavior this
    /// shares.
    pub fn packetize(&mut self, opus_frame: &[u8], rtp_timestamp: u32) -> Vec<Vec<u8>> {
        let mut packet = vec![0u8; RTP_HEADER_SIZE + opus_frame.len()];
        packet[0] = 0x80;
        packet[1] = AUDIO_PAYLOAD_TYPE;
        packet[2..4].copy_from_slice(&self.sequence_number.to_be_bytes());
        packet[4..8].copy_from_slice(&rtp_timestamp.to_be_bytes());
        packet[8..12].copy_from_slice(&0u32.to_be_bytes()); // ssrc
        packet[RTP_HEADER_SIZE..].copy_from_slice(opus_frame);

        let shard_index = (self.sequence_number % AUDIO_DATA_SHARDS as u16) as usize;
        if shard_index == 0 {
            self.base_timestamp = rtp_timestamp;
        }
        self.data_shards[shard_index] = opus_frame.to_vec();

        self.sequence_number = self.sequence_number.wrapping_add(1);

        let mut packets = vec![packet];
        if self.fec_enabled && shard_index == AUDIO_DATA_SHARDS - 1 {
            packets.extend(self.generate_fec_packets());
        }
        packets
    }

    /// Reed-Solomon-encodes the just-completed group of 4 data shards into
    /// the group's 2 FEC packets. Note: unlike data packets, generating
    /// these does NOT advance `self.sequence_number` any further than the
    /// group's 4 data packets already did — FEC packets get their own
    /// sequence numbers (`self.sequence_number` and `+1`) for loss
    /// detection, but the *next* data packet continues right after the 4th
    /// one, matching moonlight-common-rust's `AudioPayloader::push_frame`.
    fn generate_fec_packets(&mut self) -> Vec<Vec<u8>> {
        let shard_len = self.data_shards[0].len();
        if self.data_shards.iter().any(|shard| shard.len() != shard_len) {
            // Should be unreachable with constant-bitrate Opus (see this
            // module's doc comment) -- guarded rather than asserted since a
            // dropped FEC block just costs loss-recovery for this one
            // group, not correctness.
            tracing::warn!("audio FEC: opus frame sizes vary within a group of 4 (expected constant-bitrate Opus) -- skipping FEC for this group");
            return Vec::new();
        }

        let base_sequence_number = self.sequence_number.wrapping_sub(AUDIO_DATA_SHARDS as u16);
        let mut fec_packet0 = self.new_fec_packet(0, base_sequence_number, shard_len);
        let mut fec_packet1 = self.new_fec_packet(1, base_sequence_number, shard_len);

        self.reed_solomon
            .encode_sep(&self.data_shards, &mut [&mut fec_packet0[AUDIO_FEC_PAYLOAD_OFFSET..], &mut fec_packet1[AUDIO_FEC_PAYLOAD_OFFSET..]])
            .expect("4 uniform-length data shards + 2 parity shards matches ReedSolomon::new(4, 2)");

        vec![fec_packet0, fec_packet1]
    }

    /// Builds one FEC packet's `RtpAudioHeader` + `AudioFecHeader`, payload
    /// left zeroed for the caller to fill via Reed-Solomon encoding.
    fn new_fec_packet(&self, fec_shard_index: u8, base_sequence_number: u16, shard_len: usize) -> Vec<u8> {
        let mut packet = vec![0u8; AUDIO_FEC_PAYLOAD_OFFSET + shard_len];
        packet[0] = 0x80;
        packet[1] = AUDIO_PAYLOAD_TYPE_FEC;
        let sequence_number = self.sequence_number.wrapping_add(fec_shard_index as u16);
        packet[2..4].copy_from_slice(&sequence_number.to_be_bytes());
        packet[4..8].copy_from_slice(&0u32.to_be_bytes()); // timestamp: unused for FEC packets, real base_timestamp lives in the FEC header below
        packet[8..12].copy_from_slice(&0u32.to_be_bytes()); // ssrc

        packet[12] = fec_shard_index;
        packet[13] = AUDIO_PAYLOAD_TYPE; // what the reconstructed shard should be treated as, not this packet's own type
        packet[14..16].copy_from_slice(&base_sequence_number.to_be_bytes());
        packet[16..20].copy_from_slice(&self.base_timestamp.to_be_bytes());
        packet[20..24].copy_from_slice(&0u32.to_be_bytes()); // ssrc

        packet
    }
}

/// Same "wait for the client's `PING`" pattern as `VideoSender` — see there
/// for why the address isn't known upfront, and for why this wraps one
/// shared, once-bound socket rather than binding its own per launch.
pub struct AudioSender {
    inner: MultiClientUdpSender,
}

impl AudioSender {
    pub async fn bind(bind_addr: IpAddr, port: u16) -> Result<Self, String> {
        Ok(Self { inner: MultiClientUdpSender::bind(bind_addr, port, "audio").await? })
    }

    /// See `VideoSender::drain_pending`'s doc comment — same reasoning,
    /// same bug, same fix.
    pub fn drain_pending(&self, ping_token: [u8; 16]) {
        self.inner.forget(ping_token);
    }

    /// See `VideoSender::wait_for_client`'s doc comment for why this is
    /// keyed by `ping_token`.
    pub async fn wait_for_client(&self, ping_token: [u8; 16]) -> Result<SocketAddr, String> {
        self.inner.wait_for_client(ping_token).await
    }

    /// One `sendmmsg` batch for the whole group (the 1 data packet, plus 2
    /// FEC parity packets on every 4th call) instead of one `sendto` per
    /// packet — see `udp_sender.rs`'s doc comment and `VideoSender::
    /// send_shards`' own version of this for the same fix on the video
    /// side. A much smaller win here in absolute terms (at most 3 packets
    /// a batch, vs. up to hundreds for a video keyframe), but the
    /// machinery already exists and this path still adds up: real
    /// `sendmmsg` preserves submission order (each buffer is still its own
    /// independent datagram either way), so this doesn't weaken the strict
    /// in-order delivery this stream depends on (see this method's caller
    /// in session.rs) — if anything it's a stronger ordering guarantee
    /// than separately-awaited sends ever were, being one atomic kernel
    /// call instead of several.
    pub async fn send_packets(&self, ping_token: [u8; 16], packets: &[Vec<u8>]) -> Result<(), String> {
        self.inner.send_batch_to(ping_token, packets).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packetize_prepends_header_and_increments_sequence() {
        let mut packetizer = AudioPacketizer::new();
        let opus_frame = vec![0xAAu8; 40];

        let sent0 = packetizer.packetize(&opus_frame, 1000);
        assert_eq!(sent0.len(), 1, "no FEC block yet, only 1 of 4 data packets sent");
        let p0 = &sent0[0];
        assert_eq!(p0.len(), RTP_HEADER_SIZE + opus_frame.len());
        assert_eq!(p0[0], 0x80);
        assert_eq!(p0[1], AUDIO_PAYLOAD_TYPE);
        assert_eq!(u16::from_be_bytes([p0[2], p0[3]]), 0);
        assert_eq!(&p0[RTP_HEADER_SIZE..], &opus_frame[..]);

        let sent1 = packetizer.packetize(&opus_frame, 1960);
        let p1 = &sent1[0];
        assert_eq!(u16::from_be_bytes([p1[2], p1[3]]), 1);
    }

    /// Every 4th data packet must complete a Reed-Solomon FEC block: 2 extra
    /// packets (payload type 127), each with an `AudioFecHeader` referencing
    /// the group's first sequence number/timestamp, and payloads that
    /// actually reconstruct a dropped data shard using real Moonlight's
    /// parity matrix -- not just "some" Reed-Solomon matrix, since a generic
    /// one wouldn't match what a real client's depayloader expects.
    #[test]
    fn packetize_emits_fec_block_every_4th_packet_and_reconstructs() {
        let mut packetizer = AudioPacketizer::new();
        let frames: Vec<Vec<u8>> = (0..4u8).map(|i| vec![i; 32]).collect();

        let mut sent = Vec::new();
        for (i, frame) in frames.iter().enumerate() {
            sent.push(packetizer.packetize(frame, 1000 + i as u32 * 5));
        }

        assert_eq!(sent[0].len(), 1);
        assert_eq!(sent[1].len(), 1);
        assert_eq!(sent[2].len(), 1);
        assert_eq!(sent[3].len(), 3, "4th data packet must complete the group with 2 FEC packets");

        let fec0 = &sent[3][1];
        let fec1 = &sent[3][2];
        for fec_packet in [fec0, fec1] {
            assert_eq!(fec_packet[0], 0x80);
            assert_eq!(fec_packet[1], AUDIO_PAYLOAD_TYPE_FEC);
            assert_eq!(fec_packet[13], AUDIO_PAYLOAD_TYPE, "fec header's payload_type names what a reconstructed shard becomes");
            assert_eq!(u16::from_be_bytes([fec_packet[14], fec_packet[15]]), 0, "base_sequence_number is the group's first data packet");
            assert_eq!(u32::from_be_bytes([fec_packet[16], fec_packet[17], fec_packet[18], fec_packet[19]]), 1000, "base_timestamp is the group's first data packet's timestamp");
        }
        assert_eq!(fec0[12], 0);
        assert_eq!(fec1[12], 1);
        // FEC packets get their own sequence numbers, but don't advance the
        // real data-packet counter -- the 5th call below must still be 4.
        let seq_fec0 = u16::from_be_bytes([fec0[2], fec0[3]]);
        let seq_fec1 = u16::from_be_bytes([fec1[2], fec1[3]]);
        assert_eq!((seq_fec0, seq_fec1), (4, 5));

        let sent4 = packetizer.packetize(&[0xFFu8; 32], 1020);
        assert_eq!(u16::from_be_bytes([sent4[0][2], sent4[0][3]]), 4, "FEC packets must not have advanced the data sequence counter");

        // Reconstruct a dropped data shard (index 2) using the FEC payloads,
        // the same way a real client's depayloader would.
        let mut shards: Vec<Option<Vec<u8>>> = frames.iter().cloned().map(Some).collect();
        shards[2] = None;
        shards.push(Some(fec0[AUDIO_FEC_PAYLOAD_OFFSET..].to_vec()));
        shards.push(Some(fec1[AUDIO_FEC_PAYLOAD_OFFSET..].to_vec()));

        let reed_solomon = new_audio_reed_solomon();
        reed_solomon.reconstruct_data(&mut shards).expect("reconstruct dropped data shard from 2 fec parity shards");
        assert_eq!(shards[2].as_deref(), Some(frames[2].as_slice()));
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
        // Different size than frame0 -- this test only sends 2 of a group's
        // 4 packets, so it never reaches the FEC shard-uniformity
        // requirement (see `packetize_emits_fec_block_every_4th_packet_and_reconstructs`
        // for that); this just confirms per-packet encryption itself
        // doesn't assume a fixed length.
        let frame1 = vec![0x22u8; 40];

        let sent0 = packetizer.packetize_encrypted(&frame0, 1000, &key, key_id);
        let sent1 = packetizer.packetize_encrypted(&frame1, 1005, &key, key_id);
        let p0 = &sent0[0];
        let p1 = &sent1[0];

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
