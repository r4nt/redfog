//! Starts a dedicated `pipewire`+`wireplumber`+`pipewire-pulse` trio rooted
//! at `<runtime_dir>` (via [`redfog_core::HeadlessRuntime::start`]), then
//! execs into the rest of the session's own launch chain — see
//! `redfog-broker-protocol::SpawnedSession`'s doc comment for why each
//! session gets its own instance now instead of sharing `redfog-server`'s
//! old process-wide one.
//!
//! Usage: `redfog-pipewire-session <runtime_dir> -- <command> [args...]`
//!
//! A separate small binary, not a function call inlined into whatever spawns
//! it, for the same reason as `redfog-session-init`: `HeadlessRuntime::start`
//! blocks until PipeWire/wireplumber are actually ready, which only makes
//! sense to do *before* execing into the rest of the chain (KWin,
//! concretely) — and `exec()` replaces the entire process image, so this
//! can't be a step partway through some other binary's own startup.
//!
//! Meant to run *after* `redfog-session-init`'s own privilege drop (i.e. as
//! the session's target user, not root) — `HeadlessRuntime::start`'s own
//! `hide_real_audio_devices` sandboxing works fine unprivileged (it never
//! needs to re-expose anything the way GPU sandboxing does), so there's no
//! reason for this to run any more privileged than the compositor it's
//! standing infrastructure up for.

use std::os::unix::process::CommandExt;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: redfog-pipewire-session <runtime_dir> -- <command> [args...]";
    if args.len() < 3 || args[1] != "--" {
        eprintln!("{usage}");
        std::process::exit(2);
    }
    let runtime_dir = &args[0];
    let (command, command_args) = args[2..].split_first().expect("checked len above");

    let headless_runtime = match redfog_core::HeadlessRuntime::start(runtime_dir) {
        Ok(hr) => hr,
        Err(e) => {
            eprintln!("redfog-pipewire-session: failed to start PipeWire for {runtime_dir}: {e}");
            std::process::exit(1);
        }
    };
    // Deliberately not dropped: `HeadlessRuntime::drop` kills its own
    // pipewire/wireplumber/pipewire-pulse children, but this process is
    // about to exec() into the rest of the session (KWin) — they need to
    // keep running as siblings for the whole session's lifetime, not die
    // the moment this process image gets replaced. `maybe_die_with_parent`
    // (applied to each of them inside `HeadlessRuntime::start` itself) is
    // what ties their lifetime to this process's real termination instead.
    std::mem::forget(headless_runtime);

    let err = std::process::Command::new(command).args(command_args).exec();
    eprintln!("redfog-pipewire-session: failed to exec {command}: {err}");
    std::process::exit(1);
}
