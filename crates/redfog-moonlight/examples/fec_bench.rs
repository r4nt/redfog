//! Standalone throughput benchmark for `fec_rs::ReedSolomon`, isolating
//! its raw computational cost from everything else (network, GPU capture,
//! tokio scheduling) that the real `video.rs`/`audio.rs` packetization
//! path runs alongside it — see the "3-4x more CPU on an older machine"
//! investigation this was written for: `/proc`-level per-thread sampling
//! (scripts/profile-cpu.py) showed the gap concentrated almost entirely
//! in `tokio-rt-worker` (where FEC encoding runs) even after matching
//! bitrate between the two machines, and `fec-rs`'s Galois-field multiply
//! (galois.rs) is log/exp-table-lookup based, not SIMD — a classic
//! cache-latency-sensitive pattern. This benchmark directly measures
//! `fec-rs`'s own encode throughput, decoupled from everything else, so a
//! cross-machine comparison isolates that one variable cleanly rather
//! than inferring it from full-system profiling noise.
//!
//! Shard size (1008 bytes) and default counts mirror `video.rs`'s real
//! parameters (`REQUESTED_PACKET_SIZE - NV_VIDEO_PACKET_SIZE` = 1024 - 16)
//! and a representative ~20KB encoded frame at the default 20% FEC
//! ratio — override via args to match a specific real frame size/ratio
//! instead of guessing.
//!
//! Usage:
//!   cargo run --release -p redfog-moonlight --example fec_bench -- [data_shards] [parity_shards] [shard_size] [iterations]
//!   cargo run --release -p redfog-moonlight --example fec_bench -- 20 4 1008 2000
//!
//! Run with --release: FEC in production always runs in a release build,
//! and the debug/release throughput gap for table-lookup-heavy code like
//! this is large enough to make a debug-build comparison meaningless.

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let data_shards: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(20);
    let parity_shards: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(4);
    let shard_size: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1008);
    let iterations: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2000);

    println!(
        "fec_bench: data_shards={data_shards} parity_shards={parity_shards} shard_size={shard_size} iterations={iterations}"
    );
    println!(
        "  (≈{}KB per encoded \"frame\", {}% FEC ratio)",
        (data_shards * shard_size) / 1024,
        (100 * parity_shards) / data_shards.max(1)
    );

    let reed_solomon = fec_rs::ReedSolomon::new(data_shards, parity_shards).expect("valid Reed-Solomon configuration");

    // Deterministic pseudo-random payload, not all-zeros -- a real
    // encoded video frame isn't a uniform buffer, and some GF(256)
    // multiply implementations could plausibly special-case zero (unlikely
    // here given fec-rs is table-lookup-based either way, but no reason to
    // risk measuring an unrepresentative fast path).
    let mut seed: u64 = 0x2545F4914F6CDD1D;
    let mut next_byte = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed & 0xff) as u8
    };
    // Exact same shape as video.rs's real call: data shards as an
    // immutable slice, parity shards as a separate mutable one
    // (`encode_sep`, not a combined `.encode()`).
    let data_shard_payloads: Vec<Vec<u8>> = (0..data_shards).map(|_| (0..shard_size).map(|_| next_byte()).collect()).collect();
    let mut parity_payloads: Vec<Vec<u8>> = (0..parity_shards).map(|_| vec![0u8; shard_size]).collect();

    // Warm-up (page faults, allocator, cache population) — not timed.
    for _ in 0..10 {
        reed_solomon.encode_sep(&data_shard_payloads, &mut parity_payloads).expect("encode_sep");
    }

    let start = Instant::now();
    for _ in 0..iterations {
        reed_solomon.encode_sep(&data_shard_payloads, &mut parity_payloads).expect("encode_sep");
    }
    let elapsed = start.elapsed();

    let per_op = elapsed / iterations as u32;
    let mb_per_frame = (data_shards * shard_size) as f64 / 1024.0 / 1024.0;
    let throughput_mb_s = mb_per_frame / per_op.as_secs_f64();

    println!("total: {elapsed:?} for {iterations} encodes");
    println!("per-encode: {per_op:?}");
    println!("throughput: {throughput_mb_s:.1} MB/s of source data encoded");
}
