//! Standalone probe: spawns a real `kwin_wayland` virtual compositor session
//! directly (no broker, no redfog-moonlight), builds a video pipeline against
//! it, plays it, pauses it (the same `set_state(Paused)` used to background a
//! session for the login screen), then plays it again (the same "resume"
//! transition) — and reports whether frames actually resume flowing.
//!
//! Exists to isolate a live-confirmed bug (see
//! `crates/redfog-moonlight/tests/pause_resume.rs` and project memory) away
//! from all RTSP/broker/session-manager complexity: a `pipewiresrc`
//! element's internal streaming task appears to never restart on a second
//! Paused->Playing transition, even though `set_state`/`get_state` both
//! eventually report the pipeline reached `Playing`.
//!
//! Must run as its own process (not in-process inside a test binary):
//! `ensure_private_dbus_session` re-execs the *entire current process image*
//! under `dbus-run-session`, which would restart a shared test binary from
//! scratch rather than just this one probe.
//!
//! Prints exactly one of these final lines, then exits 0:
//!   PROBE_RESULT: RESUMED    (frames flowed again after Paused->Playing)
//!   PROBE_RESULT: STUCK      (no frames within the post-resume deadline)
//! Exits non-zero with a `PROBE_ERROR: ...` line on any setup failure.

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
        SessionType::User("pause-resume-probe".to_string()),
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
    let pipeline = {
        let count = count.clone();
        redfog_core::make_pipeline(compositor.video_source(), "pause-resume-probe", frame_store, move |_changed| {
            count.fetch_add(1, Ordering::SeqCst);
        })
    };

    if let Err(e) = pipeline.set_state(gst::State::Playing) {
        println!("PROBE_ERROR: initial set_state(Playing) failed: {e}");
        std::process::exit(2);
    }

    if !wait_for_count_above(&count, 0, Duration::from_secs(15)) {
        println!("PROBE_ERROR: no frames arrived within 15s of the initial Playing transition");
        std::process::exit(2);
    }
    println!("PROBE: initial Playing transition produced frames ({} so far)", count.load(Ordering::SeqCst));

    // The exact transition `background_or_discard` uses to hand the display
    // back to the login screen.
    if let Err(e) = pipeline.set_state(gst::State::Paused) {
        println!("PROBE_ERROR: set_state(Paused) failed: {e}");
        std::process::exit(2);
    }
    std::thread::sleep(Duration::from_secs(2));

    let count_before_resume = count.load(Ordering::SeqCst);

    // The exact transition `start_streaming`'s `is_resume=true` path uses.
    let set_state_result = pipeline.set_state(gst::State::Playing);
    let (query_result, current, pending) = pipeline.state(gst::ClockTime::from_seconds(5));
    println!(
        "PROBE: resume set_state returned {set_state_result:?}, 5s state query returned {query_result:?}, current={current:?}, pending={pending:?}"
    );

    let resumed = wait_for_count_above(&count, count_before_resume, Duration::from_secs(10));

    // Report the result *before* attempting any teardown — tearing down a
    // pipeline whose `pipewiresrc` is stuck in this exact state is itself a
    // separate known risk (see `handoff_to_user`'s doc comment: it's been
    // confirmed to hang/crash the compositor), so don't let that teardown
    // risk swallow the one thing this probe exists to report.
    if resumed {
        println!("PROBE_RESULT: RESUMED");
    } else {
        println!("PROBE_RESULT: STUCK");
    }

    // Best-effort, non-blocking only from here — no `set_state(Null)` (known
    // to be able to hang against exactly this stuck state) and no
    // `terminate()` (waits on the child). The whole process exits right
    // after anyway, which takes every one of our own threads with it
    // instantly; only the `kwin_wayland` child needs an explicit, bounded
    // kill so it doesn't leak as an orphan.
    compositor.kill_best_effort();
    std::process::exit(if resumed { 0 } else { 1 });
}
