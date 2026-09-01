//! Video packetization + UDP sender (47998/udp).
//!
//! Moonlight's video wire format: each shard is `RTP header(12) ++
//! padding(4) ++ NvVideoPacket header(16) ++ payload`, where the payload
//! stream is logically `[8-byte VideoFrameHeader] ++ [H.264 Annex-B access
//! unit]` split into fixed-size chunks. Every access unit is its own
//! Reed-Solomon FEC block: its data shards (already fixed-size, since real
//! depacketizers reject any packet whose length doesn't match the
//! negotiated packet size) get `FEC_PERCENTAGE`-worth of parity shards
//! appended (floored at `MIN_REQUIRED_FEC_PACKETS`, matching the value we
//! already advertise in RTSP ANNOUNCE's SDP,
//! `x-nv-vqos[0].fec.minRequiredFecPackets`) — unlike audio's FEC, this
//! uses the *standard* Reed-Solomon matrix (`fec_rs::ReedSolomon::new` with
//! no custom parity matrix), confirmed against moonlight-common-rust's own
//! `create_video_reed_solomon`. No video encryption (matches moonshine's
//! own conditionally-set `EncryptionFlags::Video` bit).
//!
//! Only ever generates a single FEC block per frame (no
//! `VideoMultiFecBlocks` splitting for frames needing more than
//! `MAX_SHARDS_PER_FEC_BLOCK` shards) — matches moonlight-common-rust's own
//! reference payloader, which has the identical limitation (its own
//! `generate_fec_block` doc comment: `// TODO: multi fec blocks?`).
//!
//! Layout derived from reading a known-working implementation's wire code
//! (not vendored), see the plan doc for context.

use std::net::{IpAddr, SocketAddr};

use crate::udp_sender::MultiClientUdpSender;

const NV_VIDEO_PACKET_SIZE: usize = 16;
const RTP_HEADER_SIZE: usize = 12;
const PADDING_SIZE: usize = 4;
const NV_PACKET_OFFSET: usize = RTP_HEADER_SIZE + PADDING_SIZE;
const PAYLOAD_OFFSET: usize = NV_PACKET_OFFSET + NV_VIDEO_PACKET_SIZE;
const VIDEO_FRAME_HEADER_SIZE: usize = 8;

/// NvVideoPacket header + payload per shard, i.e. `PAYLOAD_OFFSET - NV_PACKET_OFFSET
/// + payload`. 1024 matches Sunshine's common default; not yet negotiated
/// with the client (see plan doc known risks).
const REQUESTED_PACKET_SIZE: usize = 1024;

const RTP_FLAG_CONTAINS_PIC_DATA: u8 = 0x1;
const RTP_FLAG_END_OF_FRAME: u8 = 0x2;
const RTP_FLAG_START_OF_FRAME: u8 = 0x4;

/// Default target FEC overhead as a percentage of data shards — a plain
/// server-side policy choice (real Sunshine's own default), not something
/// the client negotiates. Overridable per-launch via
/// `REDFOG_VIDEO_FEC_PERCENTAGE` (see `configured_fec_percentage`) for
/// live bitrate/CPU-overhead experimentation — `0` disables video FEC
/// entirely (bypassing `MIN_REQUIRED_FEC_PACKETS` too, for a true
/// zero-overhead baseline to compare against). `MIN_REQUIRED_FEC_PACKETS`
/// matches the value already advertised in RTSP ANNOUNCE's SDP
/// (`x-nv-vqos[0].fec.minRequiredFecPackets:2`) — small/low-bitrate frames
/// with too few data shards for the configured percentage alone to reach
/// this floor get their `fec_percentage` recomputed upward to match,
/// exactly like moonlight-common-rust's reference payloader.
const DEFAULT_FEC_PERCENTAGE: usize = 20;
const MIN_REQUIRED_FEC_PACKETS: usize = 2;

/// Reads `REDFOG_VIDEO_FEC_PERCENTAGE` once (see `DEFAULT_FEC_PERCENTAGE`'s
/// doc comment) — an absent, empty (see `sudo-live-session.sh`'s own env-
/// passthrough note on why a *present but empty* value must be treated the
/// same as unset), or unparseable value all fall back to the default
/// rather than erroring, since this is a debugging/tuning knob, not a
/// required setting.
fn configured_fec_percentage() -> usize {
    std::env::var("REDFOG_VIDEO_FEC_PERCENTAGE")
        .ok()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_FEC_PERCENTAGE)
}
/// A single `VideoMultiFecBlocks` block can hold at most this many shards
/// (data + parity together) — see `wolf`'s `fec.hpp`, referenced in
/// moonlight-common-rust's own `packet.rs`.
const MAX_SHARDS_PER_FEC_BLOCK: usize = 255;

/// Packs `VideoFecInfo`'s three fields into the `fec_info` word — bit
/// layout confirmed against moonlight-common-rust's `VideoFecInfo::
/// serialize` (`packet.rs`): `data_shards_total` at bits 22..32,
/// `shard_index` at bits 12..22, `fec_percentage` at bits 4..12.
fn encode_fec_info(data_shards_total: u32, shard_index: u32, fec_percentage: u32) -> u32 {
    (shard_index << 12) | (data_shards_total << 22) | (fec_percentage << 4)
}

/// Turns encoded H.264 access units into Moonlight-framed UDP shards.
/// Pure/sync — the caller (an async task, or a sync GStreamer callback
/// forwarding into a channel) decides how packets actually get sent.
pub struct VideoPacketizer {
    sequence_number: u32,
    /// The `frame_index` field in every shard's `NvVideoPacket` header. Real
    /// decoders use this to detect frame boundaries/completeness and to drop
    /// stale/duplicate data — it must increment once per access unit, not
    /// stay fixed. Confirmed live: leaving every frame at index 0 streamed
    /// real bytes end to end (client received/ACKed them) but never
    /// displayed anything beyond (if that) the very first frame.
    frame_number: u32,
    /// See `configured_fec_percentage`'s doc comment. Read once at
    /// construction, not per-frame — this is a launch-time tuning knob, not
    /// something that changes mid-session.
    fec_percentage: usize,
    /// Keyed by `(nr_data_shards, nr_parity_shards)` — reused across frames
    /// that land on the same shard counts (the common case: those only
    /// change when a frame's encoded size crosses a
    /// `requested_shard_payload_size` boundary) instead of rebuilding a
    /// `ReedSolomon` from scratch every single frame. Confirmed live via CPU
    /// IP-sampling that the rebuild — a real O(n^3) matrix inversion
    /// (`fec_rs::Matrix::invert`) — dominated this crate's own sampled CPU
    /// time under a normal FEC-enabled session, ~51% of all leaf samples
    /// landing in `Matrix::invert`/`ReedSolomon::new` combined. A small
    /// `HashMap`, not a single-slot cache: real sessions can legitimately
    /// bounce between a couple of nearby shard counts frame to frame (e.g.
    /// right at a size boundary), and the key space is bounded anyway by
    /// `MAX_SHARDS_PER_FEC_BLOCK`.
    reed_solomon_cache: std::collections::HashMap<(usize, usize), fec_rs::ReedSolomon>,
}

impl Default for VideoPacketizer {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoPacketizer {
    pub fn new() -> Self {
        // Real hosts start frame numbering at 1, not 0 — confirmed against
        // moonlight-common-rust's own `VideoPayloader` ("Frame Index Starts
        // at 1!"). 0 may be treated as a sentinel/invalid value by strict
        // depacketizers.
        Self {
            sequence_number: 0,
            frame_number: 1,
            fec_percentage: configured_fec_percentage(),
            reed_solomon_cache: std::collections::HashMap::new(),
        }
    }

    /// The frame number that will be assigned to the *next* `packetize()`
    /// call — i.e. one past the last frame actually sent. Used by adaptive
    /// bitrate to compare against a client's self-reported `last_good_frame`
    /// (same numbering space: it's this exact counter, embedded in every
    /// shard's header, that the client's depayloader tracks and echoes
    /// back). Being one ahead of the true last-sent value doesn't matter —
    /// this only feeds a threshold-based heuristic, not exact arithmetic.
    pub fn next_frame_number(&self) -> u32 {
        self.frame_number
    }

    /// Packetize one encoded access unit. Returns the shards to send, in order.
    pub fn packetize(&mut self, encoded_data: &[u8], is_key_frame: bool, rtp_timestamp: u32) -> Vec<Vec<u8>> {
        let frame_number = self.frame_number;
        self.frame_number = self.frame_number.wrapping_add(1);
        let requested_shard_payload_size = REQUESTED_PACKET_SIZE - NV_VIDEO_PACKET_SIZE;
        let packet_data_len = VIDEO_FRAME_HEADER_SIZE + encoded_data.len();

        let last_shard_size = packet_data_len % requested_shard_payload_size;
        let last_shard_size = if last_shard_size == 0 {
            requested_shard_payload_size
        } else {
            last_shard_size
        };

        let mut frame_header = [0u8; VIDEO_FRAME_HEADER_SIZE];
        frame_header[0] = 0x01; // header_type
        frame_header[1..3].copy_from_slice(&0u16.to_le_bytes()); // frame_processing_latency: not measured in v1
        frame_header[3] = if is_key_frame { 2 } else { 1 }; // frame_type
        frame_header[4..8].copy_from_slice(&(last_shard_size as u32).to_le_bytes());

        let nr_data_shards = packet_data_len.div_ceil(requested_shard_payload_size).max(1);

        // Real Sunshine/GFE policy: self.fec_percentage of data shards,
        // floored at MIN_REQUIRED_FEC_PACKETS (recomputing the *advertised*
        // percentage upward to match when the floor kicks in — real
        // depacketizers trust this field, not a client-side recomputation,
        // so it must reflect what was actually generated) — except an
        // explicitly-configured 0 disables FEC entirely, bypassing the
        // floor too, for a true zero-overhead baseline (see
        // `configured_fec_percentage`'s doc comment). Otherwise capped so a
        // single FEC block never exceeds MAX_SHARDS_PER_FEC_BLOCK total
        // shards — see this module's doc comment for why frames needing
        // more than that just get less redundancy rather than a second
        // block.
        let (mut nr_parity_shards, mut fec_percentage) = if self.fec_percentage == 0 {
            (0, 0)
        } else {
            ((nr_data_shards * self.fec_percentage).div_ceil(100), self.fec_percentage)
        };
        if nr_parity_shards > 0 && nr_parity_shards < MIN_REQUIRED_FEC_PACKETS {
            nr_parity_shards = MIN_REQUIRED_FEC_PACKETS;
            fec_percentage = (100 * nr_parity_shards) / nr_data_shards;
        }
        nr_parity_shards = nr_parity_shards.min(MAX_SHARDS_PER_FEC_BLOCK.saturating_sub(nr_data_shards));

        let mut packets = Vec::with_capacity(nr_data_shards + nr_parity_shards);
        // Only populated when nr_parity_shards > 0 — the data shards'
        // payload bytes, fed to Reed-Solomon as-is (already fixed-size and
        // zero-padded, matching what real depacketizers reconstruct
        // against).
        let mut data_shard_payloads: Vec<Vec<u8>> = Vec::with_capacity(if nr_parity_shards > 0 { nr_data_shards } else { 0 });

        for shard_index in 0..nr_data_shards {
            let payload_start = shard_index * requested_shard_payload_size;
            let payload_len = requested_shard_payload_size.min(packet_data_len - payload_start);
            // Every wire packet must be the same fixed size regardless of how
            // much real data the last shard actually holds — real
            // depacketizers (confirmed via moonlight-common-rust's
            // `VideoDepayloader::handle_packet`) reject any packet whose
            // length doesn't match the negotiated packet size outright.
            // Necessary for FEC too: Reed-Solomon requires every shard in a
            // block to be the same length. The last shard's short real
            // length is instead communicated via `last_payload_len` in the
            // frame header; the rest of its payload here stays zero-padded.
            let mut shard = vec![0u8; PAYLOAD_OFFSET + requested_shard_payload_size];

            write_rtp_header(&mut shard, self.sequence_number as u16, rtp_timestamp);

            let mut flags = RTP_FLAG_CONTAINS_PIC_DATA;
            if shard_index == 0 {
                flags |= RTP_FLAG_START_OF_FRAME;
            }
            if shard_index == nr_data_shards - 1 {
                flags |= RTP_FLAG_END_OF_FRAME;
            }
            let fec_info = encode_fec_info(nr_data_shards as u32, shard_index as u32, fec_percentage as u32);
            write_nv_video_packet(
                &mut shard[NV_PACKET_OFFSET..NV_PACKET_OFFSET + NV_VIDEO_PACKET_SIZE],
                self.sequence_number << 8,
                frame_number,
                flags,
                fec_info,
            );

            copy_header_and_data(
                &mut shard[PAYLOAD_OFFSET..PAYLOAD_OFFSET + payload_len],
                &frame_header,
                encoded_data,
                payload_start,
                payload_len,
            );

            self.sequence_number = self.sequence_number.wrapping_add(1);
            if nr_parity_shards > 0 {
                data_shard_payloads.push(shard[PAYLOAD_OFFSET..].to_vec());
            }
            packets.push(shard);
        }

        if nr_parity_shards > 0 {
            // Standard Reed-Solomon matrix — unlike audio's, no custom
            // parity matrix needed here, confirmed against
            // moonlight-common-rust's own `create_video_reed_solomon`. Cached
            // by shard-count pair — see `reed_solomon_cache`'s doc comment.
            let reed_solomon = self.reed_solomon_cache.entry((nr_data_shards, nr_parity_shards)).or_insert_with(|| {
                fec_rs::ReedSolomon::new(nr_data_shards, nr_parity_shards)
                    .expect("nr_data_shards/nr_parity_shards are always > 0 and their sum is capped at MAX_SHARDS_PER_FEC_BLOCK")
            });
            let mut parity_payloads: Vec<Vec<u8>> = (0..nr_parity_shards).map(|_| vec![0u8; requested_shard_payload_size]).collect();
            reed_solomon
                .encode_sep(&data_shard_payloads, &mut parity_payloads)
                .expect("uniform-length data shards, parity count matches ReedSolomon::new");

            for (parity_index, parity_payload) in parity_payloads.into_iter().enumerate() {
                let mut shard = vec![0u8; PAYLOAD_OFFSET + requested_shard_payload_size];

                // FEC packets carry timestamp 0, not `rtp_timestamp` — matches
                // moonlight-common-rust's reference payloader.
                write_rtp_header(&mut shard, self.sequence_number as u16, 0);

                let shard_index = nr_data_shards + parity_index;
                // No START_OF_FRAME/END_OF_FRAME on FEC packets — matches the
                // reference (`VideoHeaderFlags::CONTAINS_VIDEO_DATA` alone).
                let fec_info = encode_fec_info(nr_data_shards as u32, shard_index as u32, fec_percentage as u32);
                write_nv_video_packet(
                    &mut shard[NV_PACKET_OFFSET..NV_PACKET_OFFSET + NV_VIDEO_PACKET_SIZE],
                    self.sequence_number << 8,
                    frame_number,
                    RTP_FLAG_CONTAINS_PIC_DATA,
                    fec_info,
                );
                shard[PAYLOAD_OFFSET..].copy_from_slice(&parity_payload);

                self.sequence_number = self.sequence_number.wrapping_add(1);
                packets.push(shard);
            }
        }

        packets
    }
}

fn write_rtp_header(buf: &mut [u8], sequence_number: u16, timestamp: u32) {
    buf[0] = 0x90;
    buf[1] = 0; // packet_type
    buf[2..4].copy_from_slice(&sequence_number.to_be_bytes());
    buf[4..8].copy_from_slice(&timestamp.to_be_bytes());
    buf[8..12].copy_from_slice(&0u32.to_be_bytes()); // ssrc
}

fn write_nv_video_packet(buf: &mut [u8], stream_packet_index: u32, frame_index: u32, flags: u8, fec_info: u32) {
    buf[0..4].copy_from_slice(&stream_packet_index.to_le_bytes());
    buf[4..8].copy_from_slice(&frame_index.to_le_bytes());
    buf[8] = flags;
    buf[9] = 0; // reserved
    buf[10] = 0x10; // multi_fec_flags
    buf[11] = 0; // multi_fec_blocks: always block 0 of 1 (no FEC blocking in v1)
    buf[12..16].copy_from_slice(&fec_info.to_le_bytes());
}

/// Copy bytes from the logical `[frame_header ++ encoded_data]` stream into
/// `dst`, without materializing the concatenation (a payload chunk can
/// straddle the boundary between the two).
fn copy_header_and_data(dst: &mut [u8], frame_header: &[u8; VIDEO_FRAME_HEADER_SIZE], encoded_data: &[u8], offset: usize, len: usize) {
    let total = VIDEO_FRAME_HEADER_SIZE + encoded_data.len();
    let end = (offset + len).min(total);
    let mut written = 0;

    if offset < VIDEO_FRAME_HEADER_SIZE {
        let header_end = VIDEO_FRAME_HEADER_SIZE.min(end);
        let n = header_end - offset;
        dst[written..written + n].copy_from_slice(&frame_header[offset..header_end]);
        written += n;
        if end > VIDEO_FRAME_HEADER_SIZE {
            let n = end - VIDEO_FRAME_HEADER_SIZE;
            dst[written..written + n].copy_from_slice(&encoded_data[..n]);
        }
    } else {
        let data_start = offset - VIDEO_FRAME_HEADER_SIZE;
        let data_end = end - VIDEO_FRAME_HEADER_SIZE;
        dst[..data_end - data_start].copy_from_slice(&encoded_data[data_start..data_end]);
    }
}

/// Sends already-packetized shards to a client over UDP. The client's
/// address isn't known upfront (there's no connection setup on this
/// unreliable-UDP stream) — it announces itself with a `PING` datagram after
/// `PLAY`, same NAT-punch pattern real Sunshine/moonshine use, and we learn
/// its address from that. One shared socket for the whole server (bound
/// once, not per-launch) — see `udp_sender`'s doc comment for why: multiple
/// concurrent sessions are multiplexed onto it by learned per-`ping_token`
/// destination address, not by each getting its own port.
pub struct VideoSender {
    inner: MultiClientUdpSender,
}

impl VideoSender {
    pub async fn bind(bind_addr: IpAddr, port: u16) -> Result<Self, String> {
        Ok(Self { inner: MultiClientUdpSender::bind(bind_addr, port, "video").await? })
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
    pub fn drain_pending(&self, ping_token: [u8; 16]) {
        self.inner.forget(ping_token);
    }

    /// Blocks until `ping_token`'s `PING` datagram arrives (or returns
    /// immediately if already learned), recording its address for
    /// subsequent sends. Call once after `PLAY`, before frames start
    /// flowing. Keyed by `ping_token`, not `client_ip`/`RunningSession::
    /// generation` — see `crate::udp_sender`'s doc comment for why: it's
    /// stable across a Login -> User handoff, and unambiguous even when two
    /// concurrent clients share a source address.
    pub async fn wait_for_client(&self, ping_token: [u8; 16]) -> Result<SocketAddr, String> {
        self.inner.wait_for_client(ping_token).await
    }

    pub async fn send_shards(&self, ping_token: [u8; 16], shards: &[Vec<u8>]) -> Result<(), String> {
        for shard in shards {
            self.inner.send_to(ping_token, shard).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_shard_frame_has_start_and_end_flags() {
        let mut packetizer = VideoPacketizer::new();
        let encoded = vec![0xAB; 100]; // well under one shard's payload capacity
        let shards = packetizer.packetize(&encoded, true, 1000);
        // 1 data shard + MIN_REQUIRED_FEC_PACKETS parity shards (20% of 1 rounds
        // down to 0, so the floor applies).
        assert_eq!(shards.len(), 1 + MIN_REQUIRED_FEC_PACKETS);

        let shard = &shards[0];
        // Every shard is the same fixed size regardless of real payload length
        // (real depacketizers reject packets that don't match exactly).
        assert_eq!(shard.len(), PAYLOAD_OFFSET + REQUESTED_PACKET_SIZE - NV_VIDEO_PACKET_SIZE);
        assert_eq!(shard[0], 0x90); // RTP version/flags byte
        let flags = shard[NV_PACKET_OFFSET + 8];
        assert_eq!(flags, RTP_FLAG_CONTAINS_PIC_DATA | RTP_FLAG_START_OF_FRAME | RTP_FLAG_END_OF_FRAME);

        // frame_type byte inside the VideoFrameHeader (start of payload) should say "keyframe".
        assert_eq!(shard[PAYLOAD_OFFSET + 3], 2);

        // FEC (parity) shards: CONTAINS_PIC_DATA only, no START/END-OF-FRAME.
        for fec_shard in &shards[1..] {
            let flags = fec_shard[NV_PACKET_OFFSET + 8];
            assert_eq!(flags, RTP_FLAG_CONTAINS_PIC_DATA);
        }
    }

    #[test]
    fn multi_shard_frame_splits_correctly_and_increments_sequence() {
        let mut packetizer = VideoPacketizer::new();
        let payload_capacity = REQUESTED_PACKET_SIZE - NV_VIDEO_PACKET_SIZE;
        let encoded = vec![0xCD; payload_capacity * 2 + 10]; // spans 3 data shards
        let shards = packetizer.packetize(&encoded, false, 2000);
        // 3 data shards + MIN_REQUIRED_FEC_PACKETS parity shards (20% of 3
        // rounds up to 1, below the floor).
        assert_eq!(shards.len(), 3 + MIN_REQUIRED_FEC_PACKETS);

        let flags = |i: usize| shards[i][NV_PACKET_OFFSET + 8];
        assert_eq!(flags(0) & RTP_FLAG_START_OF_FRAME, RTP_FLAG_START_OF_FRAME);
        assert_eq!(flags(0) & RTP_FLAG_END_OF_FRAME, 0);
        assert_eq!(flags(1) & (RTP_FLAG_START_OF_FRAME | RTP_FLAG_END_OF_FRAME), 0);
        assert_eq!(flags(2) & RTP_FLAG_END_OF_FRAME, RTP_FLAG_END_OF_FRAME);

        // RTP sequence numbers increment by 1 across shards.
        let seq = |i: usize| u16::from_be_bytes([shards[i][2], shards[i][3]]);
        assert_eq!(seq(1), seq(0) + 1);
        assert_eq!(seq(2), seq(1) + 1);

        // frame_index (NvVideoPacket) is the same (first call -> 1) on every shard.
        for shard in &shards {
            let frame_index = u32::from_le_bytes(shard[NV_PACKET_OFFSET + 4..NV_PACKET_OFFSET + 8].try_into().unwrap());
            assert_eq!(frame_index, 1);
        }

        // Every shard (including the last) is the same fixed size.
        let fixed_shard_len = PAYLOAD_OFFSET + REQUESTED_PACKET_SIZE - NV_VIDEO_PACKET_SIZE;
        for shard in &shards {
            assert_eq!(shard.len(), fixed_shard_len);
        }

        // Reassembling payloads from all shards, truncated to the real data
        // length (the last shard's tail is zero-padding, not real data),
        // must reproduce [frame_header ++ encoded_data].
        let mut reassembled = Vec::new();
        for shard in &shards {
            reassembled.extend_from_slice(&shard[PAYLOAD_OFFSET..]);
        }
        reassembled.truncate(VIDEO_FRAME_HEADER_SIZE + encoded.len());
        assert_eq!(&reassembled[VIDEO_FRAME_HEADER_SIZE..], &encoded[..]);
    }

    #[test]
    fn sequence_number_and_frame_number_persist_across_packetize_calls() {
        let mut packetizer = VideoPacketizer::new();
        let shards1 = packetizer.packetize(&[0u8; 10], true, 0);
        let shards2 = packetizer.packetize(&[0u8; 10], false, 0);
        let seq = |shards: &[Vec<u8>], i: usize| u16::from_be_bytes([shards[i][2], shards[i][3]]);
        // shards1's own data + FEC shards all consume sequence numbers before
        // shards2 starts.
        assert_eq!(seq(&shards2, 0), seq(&shards1, 0) + shards1.len() as u16);

        let frame_index = |shards: &[Vec<u8>], i: usize| {
            u32::from_le_bytes(shards[i][NV_PACKET_OFFSET + 4..NV_PACKET_OFFSET + 8].try_into().unwrap())
        };
        assert_eq!(frame_index(&shards1, 0), 1);
        assert_eq!(frame_index(&shards2, 0), 2);
    }

    /// Drops a data shard and reconstructs it from the parity shards using
    /// the *standard* Reed-Solomon matrix (unlike audio's, video needs no
    /// custom parity matrix — see this module's doc comment) — proves the
    /// generated parity shards are actually usable for recovery, not just
    /// correctly shaped on the wire.
    #[test]
    fn fec_parity_shards_reconstruct_a_dropped_data_shard() {
        let mut packetizer = VideoPacketizer::new();
        let payload_capacity = REQUESTED_PACKET_SIZE - NV_VIDEO_PACKET_SIZE;
        let encoded = vec![0xEFu8; payload_capacity * 2 + 10]; // 3 data shards
        let shards = packetizer.packetize(&encoded, true, 5000);
        let nr_data_shards = 3;
        let nr_parity_shards = shards.len() - nr_data_shards;
        assert_eq!(nr_parity_shards, MIN_REQUIRED_FEC_PACKETS);

        let mut rs_shards: Vec<Option<Vec<u8>>> = shards.iter().map(|shard| Some(shard[PAYLOAD_OFFSET..].to_vec())).collect();
        let dropped = rs_shards[1].take().expect("shard was present before dropping");

        let reed_solomon = fec_rs::ReedSolomon::new(nr_data_shards, nr_parity_shards).expect("valid Reed-Solomon configuration");
        reed_solomon.reconstruct_data(&mut rs_shards).expect("reconstruct dropped data shard from parity shards");
        assert_eq!(rs_shards[1].as_deref(), Some(dropped.as_slice()));
    }
}
