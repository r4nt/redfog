//! Sustained, multi-resume regression probe: catches both classes of bug
//! this project's resume work has hit that a single-resume,
//! single-frame-check test misses — severe post-resume throttling (frames
//! keep arriving, but at a tiny fraction of normal cadence) and a full
//! stall some seconds into an otherwise-successful resume (frames start,
//! then stop forever). Both were only found via live testing with a real
//! Moonlight client, continuous input/rendering (glxgears), and repeated
//! reconnects — this probe reproduces that same shape headlessly: several
//! resume cycles in a row, each followed by a sustained window of
//! continuous synthetic damage (a real `org_kde_kwin_fake_input` pointer
//! wiggle) while tracking the longest gap between consecutive frames,
//! rather than just whether at least one frame eventually showed up.
//!
//! Same kwin_wayland-gated harness convention as `pause_resume_probe`/
//! `pause_resume_full_rebuild_probe`.

use gst::prelude::*;
use gstreamer as gst;
use redfog_core::{CompositorSession, Frame, InputForwarder, InputSink, SessionType};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RESUME_CYCLES: usize = 3;
const SUSTAIN_WINDOW: Duration = Duration::from_secs(8);
const MAX_ACCEPTABLE_GAP: Duration = Duration::from_millis(1500);

fn wait_for_frames(frame_times: &Mutex<Vec<Instant>>, floor: usize, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if frame_times.lock().unwrap().len() > floor {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn main() {
    redfog_core::ensure_private_dbus_session();

    if let Err(e) = gst::init() {
        println!("PROBE_ERROR: gstreamer::init failed: {e}");
        std::process::exit(2);
    }

    let _headless_runtime = match redfog_core::HeadlessRuntime::start(redfog_core::default_runtime_dir()) {
        Ok(rt) => rt,
        Err(e) => {
            println!("PROBE_ERROR: HeadlessRuntime::start failed: {e}");
            std::process::exit(2);
        }
    };

    // `glxgears` as the actual payload (not just synthetic pointer-motion
    // damage) — matches the live repro that actually found this bug:
    // continuous *rendering*, not just a cheap 1px cursor nudge, turned out
    // to matter (see this file's own follow-up investigation notes).
    let mut compositor = match CompositorSession::spawn(
        SessionType::User("sustained-multi-resume-probe".to_string()),
        "redfog-user-0",
        1280,
        720,
        1.0,
        &["glxgears".to_string()],
    ) {
        Ok(c) => c,
        Err(e) => {
            println!("PROBE_ERROR: CompositorSession::spawn failed: {e}");
            std::process::exit(2);
        }
    };

    // Continuous synthetic damage for the probe's entire lifetime — a real
    // client generating continuous input/rendering is what actually exposed
    // both the throttling and the later full-stall bug live; a probe that
    // only checks "did any frame eventually arrive" (like the two existing
    // resume probes) missed both.
    let stop_input = Arc::new(AtomicBool::new(false));
    let input_thread = {
        let stop_input = stop_input.clone();
        let socket_path = compositor.socket_path.clone();
        std::thread::spawn(move || {
            let mut forwarder = match InputForwarder::connect(&socket_path) {
                Ok(f) => f,
                Err(e) => {
                    println!("PROBE_ERROR: InputForwarder::connect failed: {e}");
                    return;
                }
            };
            let mut dx = 1.0;
            while !stop_input.load(Ordering::Relaxed) {
                forwarder.pointer_motion(dx, 0.0);
                forwarder.flush();
                dx = -dx;
                std::thread::sleep(Duration::from_millis(16));
            }
        })
    };

    let frame_store: Arc<Mutex<Option<Frame>>> = Arc::new(Mutex::new(None));
    let frame_times: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let mut pipeline = {
        let frame_times = frame_times.clone();
        redfog_core::make_pipeline(compositor.video_source(), "sustained-multi-resume-probe-gen0", frame_store.clone(), move |_changed| {
            frame_times.lock().unwrap().push(Instant::now());
        })
    };

    if let Err(e) = pipeline.set_state(gst::State::Playing) {
        println!("PROBE_ERROR: initial set_state(Playing) failed: {e}");
        std::process::exit(2);
    }

    if !wait_for_frames(&frame_times, 0, Duration::from_secs(15)) {
        println!("PROBE_ERROR: no frames arrived within 15s of the initial Playing transition");
        std::process::exit(2);
    }
    println!("PROBE: initial session producing frames");

    let mut overall_ok = true;

    for cycle in 1..=RESUME_CYCLES {
        // A little idle time between cycles, same as a human pausing
        // between reconnects in the live repro.
        std::thread::sleep(Duration::from_millis(500));

        if let Err(e) = compositor.reconnect_capture() {
            println!("PROBE_ERROR: reconnect_capture failed on cycle {cycle}: {e}");
            std::process::exit(2);
        }

        let old_pipeline = pipeline;
        std::thread::spawn(move || {
            let _ = old_pipeline.set_state(gst::State::Null);
        });

        frame_times.lock().unwrap().clear();
        pipeline = {
            let frame_times = frame_times.clone();
            redfog_core::make_pipeline(
                compositor.video_source(),
                &format!("sustained-multi-resume-probe-gen{cycle}"),
                frame_store.clone(),
                move |_changed| {
                    frame_times.lock().unwrap().push(Instant::now());
                },
            )
        };

        let set_state_result = pipeline.set_state(gst::State::Playing);
        println!("PROBE: cycle {cycle}: resume set_state returned {set_state_result:?}");

        // Sustained observation window: track every frame arrival, not
        // just whether at least one showed up.
        let window_start = Instant::now();
        while Instant::now() < window_start + SUSTAIN_WINDOW {
            std::thread::sleep(Duration::from_millis(100));
        }

        let times = frame_times.lock().unwrap().clone();
        if times.is_empty() {
            println!("PROBE: cycle {cycle}: STUCK — zero frames in an {SUSTAIN_WINDOW:?} window");
            overall_ok = false;
            continue;
        }

        let mut max_gap = times[0].duration_since(window_start);
        for pair in times.windows(2) {
            let gap = pair[1].duration_since(pair[0]);
            if gap > max_gap {
                max_gap = gap;
            }
        }
        let tail_gap = (window_start + SUSTAIN_WINDOW).saturating_duration_since(*times.last().unwrap());
        if tail_gap > max_gap {
            max_gap = tail_gap;
        }

        println!("PROBE: cycle {cycle}: {} frames, max gap {max_gap:?}", times.len());
        if max_gap > MAX_ACCEPTABLE_GAP {
            println!("PROBE: cycle {cycle}: THROTTLED — max gap {max_gap:?} exceeds {MAX_ACCEPTABLE_GAP:?}");
            overall_ok = false;
        }
    }

    stop_input.store(true, Ordering::Relaxed);
    let _ = input_thread.join();

    println!("PROBE_RESULT: {}", if overall_ok { "SUSTAINED" } else { "DEGRADED" });

    let _ = pipeline.set_state(gst::State::Null);
    compositor.kill_best_effort();
    std::process::exit(if overall_ok { 0 } else { 1 });
}
