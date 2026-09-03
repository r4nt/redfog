//! Full-compositor-stack counterpart to `redfog-core`'s own
//! `audio_pipeline_stall_repro` test — see that file's doc comment for the
//! bug this is chasing (some sessions' `pipewiresrc`-based audio pipeline
//! delivers exactly one buffer and then never again, despite PipeWire
//! itself continuing to deliver real data forever after) and for what's
//! already been ruled out with it: 300 fresh-loopback-and-producer attempts
//! (150 at zero load, 150 with all cores saturated by busy-spin threads)
//! against a bare `HeadlessRuntime` + a synthetic `pipewiresink` producer
//! came back with zero stalls. Every real repro so far has come from a
//! *fresh* `handoff_to_user` compositor spawn, so this rebuilds that whole
//! path for real instead of approximating it: a real `kwin_wayland`
//! session (same code as production's `spawn_session`/`handoff_to_user`),
//! with a real audio-producing app running *inside* it (connecting via
//! PulseAudio-compat, the same route a real Chrome takes — not this
//! project's own fd-based `pipewiresrc`/`pipewiresink` trick, which the
//! redfog-core harness had to use since it never spawns a compositor at
//! all). If this reproduces the stall and the bare-loopback harness
//! couldn't, that pins the missing ingredient on something specific to the
//! real KWin/session-spawn path (or to a real PulseAudio-compat client)
//! rather than PipeWire's own capture-side machinery in isolation.
//!
//! A *fresh `HeadlessRuntime` per iteration* (not one shared across all of
//! them) turned out to be load-bearing, not incidental: an earlier version
//! of this harness started PipeWire/wireplumber once and looped many KWin
//! sessions against that one already-settled instance — every stall across
//! three separate batches (2 without a `queue` after `pipewiresrc`, 1 with
//! one — see the `queue` experiment below) landed on iteration 0 specifically
//! (the only iteration racing a *just-started* PipeWire/wireplumber), and
//! zero stalls hit any of the other ~75 iterations combined, all of which
//! ran against an already-warm instance. Production never reuses a
//! PipeWire instance across sessions either — `redfog-pipewire-session`
//! spawns a brand-new dedicated one per session — so every real session is
//! actually an "iteration 0" by this measure. This version matches that:
//! fresh `HeadlessRuntime`, fresh KWin, fresh loopback, every iteration.
//!
//! Also tested directly against this harness: `ANALYSIS.md`'s "Option 1"
//! fix (insert a `queue max-size-buffers=4 leaky=downstream` right after
//! `pipewiresrc`, on the theory that missing thread decoupling starves
//! `pipewiresrc`'s buffer pool) — 1/35 stalled with the queue in place,
//! statistically indistinguishable from the ~5% baseline without it, and
//! diagnostics on that stall showed the exact same signature as every
//! other one (the producer's link into the loopback sink had vanished from
//! `pw-link -l` entirely, not merely "renegotiated"). Refutes that theory;
//! the queue was reverted, never landed in `redfog-core`.
//!
//! Not a pass/fail assertion for the same reason as the redfog-core
//! harness: reproducing an intermittent bug, not checking a fixed
//! invariant. `#[ignore]`d; run manually:
//!
//!   cargo test -p kwin-capture --test audio_pipeline_stall_repro_kwin -- --ignored --nocapture --test-threads=1
//!
//! `--test-threads=1` matters here even though this is the only test in the
//! binary — cargo still reserves it, and a stray parallel run would collide
//! on `spawn_user_compositor_direct`'s hardcoded "redfog-user-0" socket
//! name the same way two of this project's own tests already have (see
//! `capture_integration.rs`'s `KillCompositorOnDrop` comment).
//!
//! Tunable via env vars (all optional):
//!   REDFOG_REPRO_ITERATIONS   how many fresh KWin-session cycles to run
//!                             (default 15 — each spawns a real
//!                             kwin_wayland + Xwayland + audio producer, so
//!                             this is much slower per iteration than
//!                             either redfog-core repro design)
//!   REDFOG_REPRO_OBSERVE_MS   how long to let each capture pipeline run
//!                             before tearing it down and counting packets
//!                             (default 3000 — generous, since unlike the
//!                             synthetic-producer harnesses, the real
//!                             producer app itself also needs a moment to
//!                             start inside the freshly-spawned session)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gstreamer::prelude::*;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Captures everything a live investigation of this bug has needed so far
/// (see redfog's own project memory/commit history for the manual version
/// of this exact sequence), fully automatically, the moment a stall is
/// detected — so a 4-second repro becomes a full diagnostic capture instead
/// of just a hit/miss count. Called *before* tearing anything down, so the
/// stalled pipeline/thread is still live and inspectable.
fn dump_stall_diagnostics(pipewire_socket_path: &str, iteration: u64) {
    eprintln!("audio_pipeline_stall_repro_kwin: iteration {iteration}: STALL — dumping diagnostics");

    for (label, cmd, args) in [
        ("pw-dump", "pw-dump", &[][..]),
        ("pw-link -l", "pw-link", &["-l"][..]),
        // 3 samples, ~1s apart in batch mode — confirms (or not) that
        // PipeWire itself is still actively driving this session's capture
        // node even though our own pipewiresrc has gone quiet, the same way
        // every manual investigation of this bug has had to check by hand.
        ("pw-top -b -n 3", "pw-top", &["-b", "-n", "3"][..]),
    ] {
        match std::process::Command::new(cmd).args(args).env("PIPEWIRE_REMOTE", pipewire_socket_path).output() {
            Ok(out) => eprintln!(
                "--- {label} (iteration {iteration}) ---\n{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
            Err(e) => eprintln!("--- {label} (iteration {iteration}) failed to run: {e} ---"),
        }
    }

    // The stalled `pipewiresrc` streaming thread's own kernel stack — no
    // `sudo` needed here (unlike investigating a separate `redfog-server`
    // process live): this thread lives in *our own* process, and a process
    // can read its own `/proc/self/task/*/stack` without elevated
    // privileges. GStreamer always names this thread after its source
    // pad's element, which `audio_pipeline_description` hardcodes to
    // `audiosrc` — same name every iteration regardless of generation.
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
        eprintln!("--- couldn't read /proc/self/task (iteration {iteration}) ---");
        return;
    };
    for entry in entries.flatten() {
        let task_dir = entry.path();
        let comm = std::fs::read_to_string(task_dir.join("comm")).unwrap_or_default();
        if comm.trim() != "audiosrc:src" {
            continue;
        }
        let tid = task_dir.file_name().unwrap().to_string_lossy().to_string();
        let wchan = std::fs::read_to_string(task_dir.join("wchan")).unwrap_or_else(|e| format!("<unreadable: {e}>"));
        let stack = std::fs::read_to_string(task_dir.join("stack")).unwrap_or_else(|e| format!("<unreadable: {e}>"));
        eprintln!("--- audiosrc:src thread TID={tid} (iteration {iteration}) ---\nwchan: {wchan}\nstack:\n{stack}");
    }
}

/// Kills this iteration's whole KWin session on drop (including panics) —
/// `kill_best_effort()` alone only signals `kwin_wayland` itself, not
/// Xwayland or the `--exit-with-session` producer app it spawned, which
/// otherwise survive to collide with the *next* iteration's identically-
/// named socket — see `capture_integration.rs`'s identical guard for the
/// same reasoning, applied per-iteration here instead of per-test.
struct KillCompositorOnDrop(Option<session_backend::SpawnedCompositor>);
impl Drop for KillCompositorOnDrop {
    fn drop(&mut self) {
        if let Some(mut compositor) = self.0.take() {
            compositor.kill_best_effort();
        }
        redfog_test_cleanup::kill_descendants_named("kwin_wayland");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a real GPU/KWin session and takes minutes; run manually, see this file's own doc comment"]
async fn fresh_kwin_session_with_real_audio_producer() {
    redfog_test_cleanup::ensure_active();
    let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("info").try_init();
    gstreamer::init().expect("gst::init");

    let iterations = env_u64("REDFOG_REPRO_ITERATIONS", 15);
    let observe_ms = env_u64("REDFOG_REPRO_OBSERVE_MS", 3000);
    // Generous: a healthy pipeline blows past this within the first ~25ms
    // (5ms Opus frames) once real audio is flowing, while every stall seen
    // live delivered exactly 1 buffer total and then nothing for the rest
    // of the session.
    const STALL_THRESHOLD: u64 = 5;

    std::env::set_var("REDFOG_ALWAYS_SOFTWARE", "0");
    let _dbus_session = redfog_core::ensure_private_dbus_session();

    eprintln!("audio_pipeline_stall_repro_kwin: {iterations} iterations, REDFOG_REPRO_OBSERVE_MS={observe_ms}");

    let mut stalls = 0u64;
    for i in 0..iterations {
        eprintln!("audio_pipeline_stall_repro_kwin: iteration {i}: starting a fresh PipeWire instance + KWin session with a real audio producer...");
        // Fresh every iteration, not shared across the loop — see this
        // file's own doc comment for why that's the load-bearing part of
        // this harness's design, not incidental.
        let runtime_dir = std::env::temp_dir().join(format!("redfog-audio-stall-repro-kwin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::env::set_var("REDFOG_RUNTIME_DIR", &runtime_dir);
        let headless_runtime = redfog_core::HeadlessRuntime::start(&runtime_dir).unwrap();
        let pipewire_socket_path = headless_runtime.pipewire_socket.to_str().unwrap().to_string();
        // `gst-launch-1.0 ... ! pulsesink`, not our own fd-based
        // `pipewiresink` trick — this connects the same way a real Chrome
        // does (PulseAudio-compat, ambient routing via `default.
        // configured.audio.sink`), which the redfog-core harness's
        // synthetic producer never exercised. Run via `sh -c` with the
        // whole pipeline as one string — `CompositorSession::spawn` passes
        // each of these on as its own argv entry to KWin's `--exit-with-
        // session`, and KWin's own re-forwarding to the payload apparently
        // doesn't preserve that split faithfully (confirmed live: passing
        // gst-launch-1.0's own args directly produced "pipeline could not
        // be constructed: empty pipeline not allowed").
        let compositor = session_backend::spawn_user_compositor_direct(
            session_backend::Backend::Kwin,
            "user",
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "exec gst-launch-1.0 audiotestsrc is-live=true wave=sine ! audioconvert ! pulsesink".to_string(),
            ],
            1280,
            720,
            60,
        )
        .expect("spawn_user_compositor_direct");
        let mut guard = KillCompositorOnDrop(Some(compositor));

        // Same ordering as production's `handoff_to_user` fresh-spawn
        // branch: `AudioLoopback::spawn` right after the compositor, no
        // deliberate extra wait for the producer app to actually start
        // making sound — the race (if any) is exactly this window.
        let loopback = redfog_core::AudioLoopback::spawn(&format!("repro-kwin-{i}"), &pipewire_socket_path).expect("AudioLoopback::spawn");

        let count = Arc::new(AtomicU64::new(0));
        let count_cb = count.clone();
        let pipeline =
            redfog_core::make_audio_pipeline(&loopback, &format!("repro-kwin-audio-gen-{i}"), move |_packet| {
                count_cb.fetch_add(1, Ordering::Relaxed);
            });
        pipeline.set_state(gstreamer::State::Playing).expect("audio pipeline set Playing");

        std::thread::sleep(Duration::from_millis(observe_ms));

        let n = count.load(Ordering::Relaxed);
        let verdict = if n <= STALL_THRESHOLD { "STALL" } else { "ok" };
        if n <= STALL_THRESHOLD {
            stalls += 1;
        }
        eprintln!("audio_pipeline_stall_repro_kwin: iteration {i}: {n} packets in {observe_ms}ms — {verdict}");
        if n <= STALL_THRESHOLD {
            dump_stall_diagnostics(&pipewire_socket_path, i);
        }

        let _ = pipeline.set_state(gstreamer::State::Null);
        drop(loopback);
        guard.0.take().unwrap().terminate();
        drop(guard);
    }

    eprintln!(
        "audio_pipeline_stall_repro_kwin: {stalls}/{iterations} stalled ({:.0}%)",
        100.0 * stalls as f64 / iterations as f64
    );
}
