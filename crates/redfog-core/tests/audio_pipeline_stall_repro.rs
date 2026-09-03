//! Fast-iterating reproduction harness for a real, still-open bug: some
//! sessions' `pipewiresrc`-based audio pipeline (see
//! `redfog_core::make_audio_pipeline`) delivers exactly one buffer and then
//! never again, even though the underlying PipeWire stream keeps delivering
//! real data forever after (confirmed live via `pw-top` showing continuous
//! WAIT/BUSY activity on the capture node, matching the producer's own
//! cadence, for the whole time the pipeline sits stalled). The full
//! app-level repro (redfog-server + a real Moonlight client + KWin + a
//! browser playing audio, with a manual login/logout cycle to force a fresh
//! session each time) takes minutes per attempt and only occasionally
//! reproduces it. This rebuilds just the pieces that actually matter — a
//! real PipeWire loopback, a synthetic always-on producer standing in for a
//! real app playing audio, and the exact same `make_audio_pipeline`
//! production code — cycled rapidly to get many attempts per minute instead
//! of per several minutes.
//!
//! Every iteration tears down and rebuilds *everything* (loopback,
//! producer, capture pipeline) from scratch, deliberately mirroring
//! production's actual shape: every reported stall so far has come from a
//! *fresh* compositor spawn (`handoff_to_user`'s "no backgrounded session,
//! spawning a fresh user compositor" branch — see `session.rs`), i.e. a
//! brand-new `AudioLoopback` and a brand-new audio-producing app (Chrome)
//! every single time, never a long-lived one reused across many capture
//! attempts. An earlier version of this harness reused one long-lived
//! loopback+producer across many capture attempts and never reproduced the
//! stall in 30 tries — consistent with (not proof of) the stall being a
//! cold-start race against something in the *freshly created* loopback/
//! producer settling, not a property of the capture side alone.
//!
//! Confirmed *not* enough on its own: 150/150 iterations of the
//! fresh-everything design above still came back clean with zero CPU load
//! and zero delay. Every real repro so far has happened on a machine also
//! running KWin compositing, NVENC encoding, video capture, and a real
//! Chrome (a much heavier PipeWire client than this harness's bare
//! `pipewiresink`) all contending for the CPU at once — `REDFOG_REPRO_
//! LOAD_THREADS` exists to test whether that contention, not just fresh-vs-
//! reused timing, is the actual missing ingredient.
//!
//! Not a pass/fail assertion — this is reproducing an intermittent bug, not
//! checking a fixed invariant, so a "failure" here (a stall) is data, not a
//! bug in the test. `#[ignore]`d so normal `cargo test` runs skip it; run
//! manually:
//!
//!   cargo test -p redfog-core --test audio_pipeline_stall_repro -- --ignored --nocapture
//!
//! Tunable via env vars (all optional):
//!   REDFOG_REPRO_ITERATIONS   how many fresh loopback+producer+capture
//!                             cycles to run (default 20 — each one spawns
//!                             real subprocesses, so this is slower per
//!                             iteration than the old reused-loopback
//!                             design)
//!   REDFOG_REPRO_DELAY_MS     sleep this long between the producer
//!                             reaching Playing and building/starting this
//!                             iteration's `make_audio_pipeline` — the same
//!                             knob as `REDFOG_AUDIO_PIPELINE_DELAY_MS` in
//!                             the real server, extracted here so its
//!                             effect on the stall rate can actually be
//!                             measured across many attempts instead of
//!                             guessed at from a handful of manual repros
//!                             (default 0)
//!   REDFOG_REPRO_OBSERVE_MS   how long to let each pipeline run before
//!                             tearing it down and counting packets
//!                             (default 1500 — a healthy pipeline delivers
//!                             ~5ms Opus frames, so this should see ~300)
//!   REDFOG_REPRO_LOAD_THREADS how many busy-spin threads to keep running
//!                             for the whole test, to manufacture the kind
//!                             of CPU/scheduling contention a real KWin +
//!                             NVENC + Chrome session creates (default 0 —
//!                             off; try `nproc` to fully saturate the
//!                             machine)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gstreamer::prelude::*;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// A synthetic always-on audio producer standing in for a real app (Chrome,
/// a game, ...) — plays a continuous tone straight into `sink_name` (unlike
/// a real app, which relies on `AudioLoopback::spawn`'s `default.
/// configured.audio.sink` redirect instead of naming a target explicitly —
/// but an fd-connected `pipewiresink` client apparently doesn't get routed
/// by that same default-node policy the way a real PulseAudio-compat client
/// does, confirmed by this producer's audio never actually reaching the
/// loopback without an explicit `target-object` here). `sync=false`:
/// without it, `pipewiresink`'s buffer pool never activates at all in this
/// otherwise-empty headless graph (confirmed live — `on_param_changed:
/// waiting for pool to become active`, forever). What matters for
/// reproducing the stall is real continuous data reaching the capture side,
/// not which routing mechanism got it there.
fn spawn_synthetic_producer(pipewire_socket_path: &str, sink_name: &str) -> gstreamer::Pipeline {
    let pipeline = gstreamer::parse_launch(&format!(
        "audiotestsrc is-live=true wave=sine \
         ! audioconvert ! audioresample \
         ! audio/x-raw,format=S16LE,channels=2,rate=48000 \
         ! pipewiresink name=producer_sink client-name=repro-synthetic-producer target-object={sink_name} sync=false"
    ))
    .expect("producer pipeline parse failed")
    .dynamic_cast::<gstreamer::Pipeline>()
    .unwrap();
    let fd = redfog_core::open_pipewire_fd(pipewire_socket_path).expect("open_pipewire_fd for synthetic producer");
    pipeline.by_name("producer_sink").unwrap().set_property("fd", fd);
    pipeline.set_state(gstreamer::State::Playing).expect("producer set Playing");
    pipeline
}

/// Spawns `count` busy-spin threads that never yield voluntarily, to
/// manufacture real CPU/scheduling contention on this machine for as long
/// as the calling test runs — see this file's own doc comment for why:
/// 150/150 clean iterations with zero load suggests the stall needs
/// contention a quiet, single-purpose test process doesn't create on its
/// own. Not joined — the whole test process exits at the end of `main`
/// regardless of what these are doing, same "fine to leave a thread
/// running" trade-off as everywhere else in this codebase's teardown code.
fn spawn_cpu_load(count: u64) {
    for _ in 0..count {
        std::thread::spawn(|| {
            let mut x: u64 = 0;
            loop {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                std::hint::black_box(x);
            }
        });
    }
}

#[test]
#[ignore = "needs a real PipeWire instance and takes tens of seconds to minutes; run manually, see this file's own doc comment"]
fn fresh_loopback_and_producer_per_iteration() {
    redfog_test_cleanup::ensure_active();
    gstreamer::init().expect("gst::init");

    let iterations = env_u64("REDFOG_REPRO_ITERATIONS", 20);
    let delay_ms = env_u64("REDFOG_REPRO_DELAY_MS", 0);
    let observe_ms = env_u64("REDFOG_REPRO_OBSERVE_MS", 1500);
    let load_threads = env_u64("REDFOG_REPRO_LOAD_THREADS", 0);
    // Generous: a healthy pipeline blows past this within the first ~25ms
    // (5ms Opus frames), while every stall seen live delivered exactly 1
    // buffer total and then nothing for the rest of the session.
    const STALL_THRESHOLD: u64 = 5;

    // Started before `HeadlessRuntime::start` deliberately — a real KWin +
    // NVENC + Chrome session is already under contention before its own
    // per-session `pipewire`/`wireplumber` instance even starts, not just
    // during our own capture pipeline's construction.
    if load_threads > 0 {
        spawn_cpu_load(load_threads);
    }

    let runtime_dir = std::env::temp_dir().join(format!("redfog-audio-stall-repro-{}", std::process::id()));
    std::fs::create_dir_all(&runtime_dir).unwrap();
    let headless_runtime = redfog_core::HeadlessRuntime::start(&runtime_dir).unwrap();
    let pipewire_socket_path = headless_runtime.pipewire_socket.to_str().unwrap().to_string();

    eprintln!(
        "audio_pipeline_stall_repro: {iterations} iterations, REDFOG_REPRO_DELAY_MS={delay_ms}, \
         REDFOG_REPRO_OBSERVE_MS={observe_ms}, REDFOG_REPRO_LOAD_THREADS={load_threads}"
    );

    let mut stalls = 0u64;
    for i in 0..iterations {
        // Fresh every iteration — see this file's own doc comment for why
        // that's the load-bearing part of this harness's design, not
        // incidental.
        let loopback = redfog_core::AudioLoopback::spawn(&format!("repro-{i}"), &pipewire_socket_path).expect("AudioLoopback::spawn");
        let producer = spawn_synthetic_producer(&pipewire_socket_path, &loopback.sink_name);

        if delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }

        let count = Arc::new(AtomicU64::new(0));
        let count_cb = count.clone();
        let pipeline = redfog_core::make_audio_pipeline(&loopback, &format!("repro-audio-gen-{i}"), move |_packet| {
            count_cb.fetch_add(1, Ordering::Relaxed);
        });
        pipeline.set_state(gstreamer::State::Playing).expect("audio pipeline set Playing");

        std::thread::sleep(Duration::from_millis(observe_ms));

        let n = count.load(Ordering::Relaxed);
        let verdict = if n <= STALL_THRESHOLD { "STALL" } else { "ok" };
        if n <= STALL_THRESHOLD {
            stalls += 1;
        }
        eprintln!("audio_pipeline_stall_repro: iteration {i}: {n} packets in {observe_ms}ms — {verdict}");

        let _ = pipeline.set_state(gstreamer::State::Null);
        let _ = producer.set_state(gstreamer::State::Null);
        // `loopback` drops here, killing this iteration's `pw-loopback` —
        // best-effort, matching every other teardown in this codebase (see
        // `kill_best_effort`'s doc comment).
    }

    eprintln!(
        "audio_pipeline_stall_repro: {stalls}/{iterations} stalled ({:.0}%)",
        100.0 * stalls as f64 / iterations as f64
    );
}
