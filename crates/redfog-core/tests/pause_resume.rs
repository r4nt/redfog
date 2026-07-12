//! Isolates the "resume gets stuck" bug (and its fix) away from all
//! RTSP/broker/session-manager complexity: runs a standalone probe example
//! that just spawns a real `kwin_wayland` virtual compositor, plays its
//! video pipeline, pauses it, then resumes it — and checks whether frames
//! actually flow again.
//!
//! Requires `kwin_wayland` to be installed — skips with a clear message
//! (not a failure) if it isn't, same convention as the sudo-gated tests in
//! `redfog-moonlight/tests/connection_integration.rs`.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

fn kwin_wayland_installed() -> bool {
    Command::new("kwin_wayland").arg("--version").output().is_ok()
}

fn probe_binary(name: &str) -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for profile in ["debug", "release"] {
        let path = manifest_dir.join(format!("../../target/{profile}/examples/{name}"));
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Runs `probe_name`, returning its exit status and captured stdout lines.
/// Bounded and orphan-proof — see the inline comments for why each of those
/// matters here specifically.
fn run_probe(probe_name: &str) -> (ExitStatus, Vec<String>) {
    run_probe_with_timeout(probe_name, Duration::from_secs(60))
}

/// Same as `run_probe`, but with a caller-chosen timeout — for probes whose
/// own internal bounded waits legitimately add up to more than the default
/// 60s (e.g. several real resume cycles with multi-second sustained
/// observation windows each).
fn run_probe_with_timeout(probe_name: &str, timeout: Duration) -> (ExitStatus, Vec<String>) {
    let probe = probe_binary(probe_name).unwrap_or_else(|| {
        panic!("{probe_name} example not built — run `cargo build --example {probe_name} -p redfog-core` (or --release) first")
    });

    // Isolated from any real redfog-server/broker that might already be
    // running on this machine — same pattern as
    // `connection_integration.rs`'s `runtime_dir`.
    let runtime_dir = std::env::temp_dir().join(format!("redfog-{probe_name}-{}", std::process::id()));

    let mut child = Command::new(&probe)
        .env("REDFOG_RUNTIME_DIR", &runtime_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        // Own process group: the probe's own `kwin_wayland` grandchild
        // otherwise survives a plain `child.kill()` on timeout (a lone
        // SIGKILL doesn't reach descendants) — confirmed live, an earlier
        // iteration of this test left exactly such an orphan behind.
        .process_group(0)
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn {probe:?}: {e}"));

    let stdout = child.stdout.take().expect("piped stdout");
    let reader_probe_name = probe_name.to_string();
    let output_lines = std::thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        let mut lines = Vec::new();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            println!("[{reader_probe_name}] {line}");
            lines.push(line);
        }
        lines
    });

    // Generous but bounded — the probe's own internal waits top out well
    // under this; a real hang here means the probe itself wedged somewhere
    // outside its own bounded waits, not just a slow machine.
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() >= deadline {
            // A lone `child.kill()` only reaches the probe process itself,
            // not its own `kwin_wayland` grandchild — confirmed live, an
            // earlier iteration of this test left exactly such an orphan
            // behind. `process_group(0)` above makes the probe its own
            // group leader, so the group id equals its pid.
            let pgid = nix::unistd::Pid::from_raw(-(child.id() as i32));
            let _ = nix::sys::signal::kill(pgid, nix::sys::signal::Signal::SIGKILL);
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&runtime_dir);
            panic!("{probe_name} did not exit within {timeout:?} — it wedged somewhere outside its own bounded waits");
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    let lines = output_lines.join().expect("output reader thread panicked");
    let _ = std::fs::remove_dir_all(&runtime_dir);
    (status, lines)
}

#[test]
#[ignore = "documents a known, now-fixed-around bug (see this test's own doc comment), kept as historical/regression \
            evidence for the naive approach specifically — not part of the normal green baseline; run explicitly \
            with `cargo test -- --ignored`"]
fn kwin_video_pipeline_resumes_after_being_paused() {
    if !kwin_wayland_installed() {
        eprintln!("skipping kwin_video_pipeline_resumes_after_being_paused: kwin_wayland not installed");
        return;
    }
    let (status, lines) = run_probe("pause_resume_probe");

    // Exit code alone isn't the right signal here: `PROBE_RESULT: STUCK`
    // exits non-zero *on purpose* (it's the expected, reproducing outcome
    // for this specific naive approach), so only a missing `PROBE_RESULT`
    // line at all (a setup failure before the probe could even attempt the
    // transition) falls back to the generic status-based message.
    if !lines.iter().any(|l| l.starts_with("PROBE_RESULT:")) {
        panic!("pause_resume_probe exited with {status} before reporting a result: {lines:?}");
    }
    assert!(
        lines.iter().any(|l| l == "PROBE_RESULT: RESUMED"),
        "video pipeline did not resume producing frames after being paused and played again with the naive \
         (reuse-the-same-pipeline) approach — this is the known, now-worked-around limitation (see \
         `redfog_core::replace_video_source`/`CompositorSession::reconnect_capture`'s doc comments, and \
         `rebuild_for_resume` in redfog-moonlight/src/session.rs for the actual fix); probe output: {lines:?}"
    );
}

/// The actual fix, verified in isolation: reconnecting the KWin screencast
/// capture *and* rebuilding the whole video pipeline fresh (not reusing the
/// paused one, and not just swapping its source element — both were tried
/// first and still wedged identically) reliably resumes. This is what
/// `rebuild_for_resume` in `redfog-moonlight/src/session.rs` actually does
/// on a real resume; this test exercises the same mechanism without any of
/// the RTSP/broker/session-manager machinery around it.
#[test]
fn kwin_video_pipeline_resumes_after_reconnecting_capture_and_rebuilding_pipeline() {
    if !kwin_wayland_installed() {
        eprintln!("skipping kwin_video_pipeline_resumes_after_reconnecting_capture_and_rebuilding_pipeline: kwin_wayland not installed");
        return;
    }
    let (status, lines) = run_probe("pause_resume_full_rebuild_probe");

    if !lines.iter().any(|l| l.starts_with("PROBE_RESULT:")) {
        panic!("pause_resume_full_rebuild_probe exited with {status} before reporting a result: {lines:?}");
    }
    assert!(
        lines.iter().any(|l| l == "PROBE_RESULT: RESUMED"),
        "video pipeline did not resume producing frames after reconnecting capture and rebuilding the pipeline \
         fresh — this is supposed to be the fix (see rebuild_for_resume in redfog-moonlight/src/session.rs); \
         probe output: {lines:?}"
    );
}

/// Catches what the two tests above can't: both a severe post-resume
/// throttling regression and a later full-stall regression were only ever
/// found live, with a real client, continuous rendering (glxgears), and
/// several reconnects in a row — a probe that does one resume and checks
/// "did at least one frame eventually arrive" (like both tests above) is
/// blind to both. This one runs several resume cycles with continuous
/// synthetic input/damage the whole time and tracks the longest gap
/// between frames in each post-resume window, not just whether any frame
/// showed up at all. Takes tens of seconds by design (real time, not busy
/// looping) — run explicitly, not part of the default fast suite.
#[test]
#[ignore = "takes ~30s (several real resume cycles with sustained observation windows) — run explicitly with \
            `cargo test -- --ignored` as a deeper live-realism check beyond the fast single-resume tests above"]
fn kwin_video_pipeline_sustains_frame_delivery_across_several_resumes_under_continuous_damage() {
    if !kwin_wayland_installed() {
        eprintln!("skipping kwin_video_pipeline_sustains_frame_delivery_across_several_resumes_under_continuous_damage: kwin_wayland not installed");
        return;
    }
    let (status, lines) = run_probe_with_timeout("sustained_multi_resume_probe", Duration::from_secs(120));

    if !lines.iter().any(|l| l.starts_with("PROBE_RESULT:")) {
        panic!("sustained_multi_resume_probe exited with {status} before reporting a result: {lines:?}");
    }
    assert!(
        lines.iter().any(|l| l == "PROBE_RESULT: SUSTAINED"),
        "video frame delivery did not sustain across repeated resumes under continuous synthetic damage — either \
         a full stall (STUCK) or severe throttling (THROTTLED) in at least one post-resume window; see the \
         `PROBE:` lines above for which cycle and how bad; probe output: {lines:?}"
    );
}
