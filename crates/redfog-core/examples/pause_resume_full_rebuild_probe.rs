//! The actual fix, verified in isolation: request a fresh PipeWire
//! stream/node from the *same*, already-open capture session
//! (`CompositorSession::reconnect_capture`, backed by `stream_output`
//! against our already-existing `wl_output` — touches nothing about the
//! output itself, unlike an earlier tried-and-reverted approach that fully
//! disconnected and reconnected the whole capture session, which did
//! unstick video but was a real output-hotplug event disrupting other
//! Wayland clients) and build an entirely fresh pipeline (source +
//! downstream, nothing reused) against it. The old pipeline's teardown is
//! abandoned on a background thread rather than waited on, same reasoning
//! as elsewhere this session: its own Paused->Playing already failed to
//! complete, so its Null transition may never return either.
//!
//! Same output contract as `pause_resume_probe`.

use gstreamer as gst;
use gst::prelude::*;
use redfog_core::{CompositorSession, Frame, SessionType};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn wait_for_count_above(counter: &AtomicU64, floor: u64, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if counter.load(Ordering::SeqCst) > floor {
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

    let mut compositor = match CompositorSession::spawn(
        SessionType::User("pause-resume-full-rebuild-probe".to_string()),
        "redfog-user-0",
        1280,
        720,
        1.0,
        &[],
    ) {
        Ok(c) => c,
        Err(e) => {
            println!("PROBE_ERROR: CompositorSession::spawn failed: {e}");
            std::process::exit(2);
        }
    };

    let frame_store: Arc<Mutex<Option<Frame>>> = Arc::new(Mutex::new(None));
    let count = Arc::new(AtomicU64::new(0));
    let old_pipeline = {
        let count = count.clone();
        redfog_core::make_pipeline(compositor.video_source(), "pause-resume-full-rebuild-probe-gen0", frame_store.clone(), move |_changed| {
            count.fetch_add(1, Ordering::SeqCst);
        })
    };

    if let Err(e) = old_pipeline.set_state(gst::State::Playing) {
        println!("PROBE_ERROR: initial set_state(Playing) failed: {e}");
        std::process::exit(2);
    }

    if !wait_for_count_above(&count, 0, Duration::from_secs(15)) {
        println!("PROBE_ERROR: no frames arrived within 15s of the initial Playing transition");
        std::process::exit(2);
    }
    println!("PROBE: initial Playing transition produced frames ({} so far)", count.load(Ordering::SeqCst));

    if let Err(e) = old_pipeline.set_state(gst::State::Paused) {
        println!("PROBE_ERROR: set_state(Paused) failed: {e}");
        std::process::exit(2);
    }
    std::thread::sleep(Duration::from_secs(2));

    let count_before_resume = count.load(Ordering::SeqCst);

    if let Err(e) = compositor.reconnect_capture() {
        println!("PROBE_ERROR: reconnect_capture failed: {e}");
        std::process::exit(2);
    }
    println!("PROBE: got a fresh stream, new node_id={}", compositor.pipewire_node_id);

    // Abandon the old pipeline's teardown — don't wait on it, don't let it
    // block reporting this probe's actual result.
    std::thread::spawn(move || {
        let _ = old_pipeline.set_state(gst::State::Null);
    });

    let new_count = Arc::new(AtomicU64::new(0));
    let new_pipeline = {
        let new_count = new_count.clone();
        redfog_core::make_pipeline(compositor.video_source(), "pause-resume-full-rebuild-probe-gen1", frame_store, move |_changed| {
            new_count.fetch_add(1, Ordering::SeqCst);
        })
    };

    let set_state_result = new_pipeline.set_state(gst::State::Playing);
    let (query_result, current, pending) = new_pipeline.state(gst::ClockTime::from_seconds(5));
    println!(
        "PROBE: fresh pipeline set_state returned {set_state_result:?}, 5s state query returned {query_result:?}, current={current:?}, pending={pending:?}"
    );

    let _ = count_before_resume; // old pipeline's counter isn't used for the resumed check — new_pipeline has its own
    let resumed = wait_for_count_above(&new_count, 0, Duration::from_secs(10));

    if resumed {
        println!("PROBE_RESULT: RESUMED");
    } else {
        println!("PROBE_RESULT: STUCK");
    }

    let _ = new_pipeline.set_state(gst::State::Null);
    compositor.kill_best_effort();
    std::process::exit(if resumed { 0 } else { 1 });
}
