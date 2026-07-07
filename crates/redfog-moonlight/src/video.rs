//! Video packetization + UDP sender (47998/udp).
//!
//! Moonlight's video wire format: each shard is `RTP header(12) ++
//! padding(4) ++ NvVideoPacket header(16) ++ payload`, where the payload
//! stream is logically `[8-byte VideoFrameHeader] ++ [H.264 Annex-B access
//! unit]` split into fixed-size chunks. Real Sunshine/moonshine additionally
//! wrap this in Reed-Solomon FEC parity shards and optional AES-GCM
//! encryption; this iteration sends redundancy=0 (no parity shards) and no
//! video encryption (matches moonshine's own conditionally-set
//! `EncryptionFlags::Video` bit), which is a valid degenerate case of the
//! same wire format, not a different one — a client that supports FEC at
//! all supports 0 parity shards.
//!
//! Layout derived from reading a known-working implementation's wire code
//! (not vendored), see the plan doc for context.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::UdpSocket;

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
        Self { sequence_number: 0, frame_number: 1 }
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
        let mut packets = Vec::with_capacity(nr_data_shards);

        for shard_index in 0..nr_data_shards {
            let payload_start = shard_index * requested_shard_payload_size;
            let payload_len = requested_shard_payload_size.min(packet_data_len - payload_start);
            // Every wire packet must be the same fixed size regardless of how
            // much real data the last shard actually holds — real
            // depacketizers (confirmed via moonlight-common-rust's
            // `VideoDepayloader::handle_packet`) reject any packet whose
            // length doesn't match the negotiated packet size outright, and
            // with 0 FEC redundancy a single dropped shard means the frame
            // can never reconstruct. The last shard's short real length is
            // instead communicated via `last_payload_len` in the frame
            // header; the rest of its payload here stays zero-padded.
            let mut shard = vec![0u8; PAYLOAD_OFFSET + requested_shard_payload_size];

            write_rtp_header(&mut shard, self.sequence_number as u16, rtp_timestamp);

            let mut flags = RTP_FLAG_CONTAINS_PIC_DATA;
            if shard_index == 0 {
                flags |= RTP_FLAG_START_OF_FRAME;
            }
            if shard_index == nr_data_shards - 1 {
                flags |= RTP_FLAG_END_OF_FRAME;
            }
            // fec_info encodes (shard_index | nr_data_shards << 10 | fec_percentage << 20);
            // with redundancy=0 the fec_percentage term is always 0.
            let fec_info = (shard_index as u32) << 12 | (nr_data_shards as u32) << 22;
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
            packets.push(shard);
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

/// Sends already-packetized shards to the client over UDP. The client's
/// address isn't known upfront (there's no connection setup on this
/// unreliable-UDP stream) — it announces itself with a `PING` datagram after
/// `PLAY`, same NAT-punch pattern real Sunshine/moonshine use, and we learn
/// its address from that.
pub struct VideoSender {
    socket: Arc<UdpSocket>,
    client_addr: std::sync::Mutex<Option<SocketAddr>>,
}

impl VideoSender {
    pub async fn bind(bind_addr: std::net::IpAddr, port: u16) -> Result<Self, String> {
        let socket = UdpSocket::bind((bind_addr, port))
            .await
            .map_err(|e| format!("failed to bind video udp {}:{}: {e}", bind_addr, port))?;
        Ok(Self {
            socket: Arc::new(socket),
            client_addr: std::sync::Mutex::new(None),
        })
    }

    /// Blocks until the client's `PING` datagram arrives, recording its
    /// address for subsequent sends. Call once after `PLAY`, before frames
    /// start flowing.
    pub async fn wait_for_client(&self) -> Result<SocketAddr, String> {
        let mut buf = [0u8; 1024];
        loop {
            let (len, addr) = self
                .socket
                .recv_from(&mut buf)
                .await
                .map_err(|e| format!("video udp recv failed: {e}"))?;
            if &buf[..len] == b"PING" {
                *self.client_addr.lock().unwrap() = Some(addr);
                return Ok(addr);
            }
            tracing::trace!("ignoring unexpected {len}-byte datagram on video port before PING");
        }
    }

    pub async fn send_shards(&self, shards: &[Vec<u8>]) -> Result<(), String> {
        let addr = self
            .client_addr
            .lock()
            .unwrap()
            .ok_or("video client address not yet known (wait_for_client not called/completed)")?;
        for shard in shards {
            self.socket
                .send_to(shard, addr)
                .await
                .map_err(|e| format!("video send failed: {e}"))?;
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
        assert_eq!(shards.len(), 1);

        let shard = &shards[0];
        // Every shard is the same fixed size regardless of real payload length
        // (real depacketizers reject packets that don't match exactly).
        assert_eq!(shard.len(), PAYLOAD_OFFSET + REQUESTED_PACKET_SIZE - NV_VIDEO_PACKET_SIZE);
        assert_eq!(shard[0], 0x90); // RTP version/flags byte
        let flags = shard[NV_PACKET_OFFSET + 8];
        assert_eq!(flags, RTP_FLAG_CONTAINS_PIC_DATA | RTP_FLAG_START_OF_FRAME | RTP_FLAG_END_OF_FRAME);

        // frame_type byte inside the VideoFrameHeader (start of payload) should say "keyframe".
        assert_eq!(shard[PAYLOAD_OFFSET + 3], 2);
    }

    #[test]
    fn multi_shard_frame_splits_correctly_and_increments_sequence() {
        let mut packetizer = VideoPacketizer::new();
        let payload_capacity = REQUESTED_PACKET_SIZE - NV_VIDEO_PACKET_SIZE;
        let encoded = vec![0xCD; payload_capacity * 2 + 10]; // spans 3 shards
        let shards = packetizer.packetize(&encoded, false, 2000);
        assert_eq!(shards.len(), 3);

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
        assert_eq!(seq(&shards2, 0), seq(&shards1, 0) + 1);

        let frame_index = |shards: &[Vec<u8>], i: usize| {
            u32::from_le_bytes(shards[i][NV_PACKET_OFFSET + 4..NV_PACKET_OFFSET + 8].try_into().unwrap())
        };
        assert_eq!(frame_index(&shards1, 0), 1);
        assert_eq!(frame_index(&shards2, 0), 2);
    }
}
