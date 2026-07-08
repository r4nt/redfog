[38;5;3mModified regular file .gitignore:[39m
    ...
[2m[38;5;1m  13[0m [2m[38;5;2m  13[0m: # Contains mutation testing data
[2m[38;5;1m  14[0m [2m[38;5;2m  14[0m: **/mutants.out*/
[2m[38;5;1m  15[0m [2m[38;5;2m  15[0m: 
     [38;5;2m  16[39m: [4m[38;5;2m# Fetched + locally-patched dependencies (see scripts/fetch-patched-deps.sh[24m[39m
     [38;5;2m  17[39m: [4m[38;5;2m# and patches/*.patch) — not vendored into git, fetched on demand instead.[24m[39m
     [38;5;2m  18[39m: [4m[38;5;2m/vendor/[24m[39m
     [38;5;2m  19[39m: [4m[38;5;2m[24m[39m
[2m[38;5;1m  16[0m [2m[38;5;2m  20[0m: # RustRover
[2m[38;5;1m  17[0m [2m[38;5;2m  21[0m: #  JetBrains specific template is maintained in a separate JetBrains.gitignore that can
[2m[38;5;1m  18[0m [2m[38;5;2m  22[0m: #  be found at https://github.com/github/gitignore/blob/main/Global/JetBrains.gitignore
    ...
[38;5;3mModified regular file Cargo.lock:[39m
    ...
[2m[38;5;1m2827[0m [2m[38;5;2m2827[0m: [[package]]
[2m[38;5;1m2828[0m [2m[38;5;2m2828[0m: name = "moonlight-common"
[2m[38;5;1m2829[0m [2m[38;5;2m2829[0m: version = "0.1.0"
[38;5;1m2830[39m     : [4m[38;5;1msource = "git+https://github.com/MrCreativ3001/moonlight-common-rust#06f0d2efbb4e1c769cdd8f8d5a92e00fc192842b"[24m[39m
[2m[38;5;1m2831[0m [2m[38;5;2m2830[0m: dependencies = [
[2m[38;5;1m2832[0m [2m[38;5;2m2831[0m:  "aes",
[2m[38;5;1m2833[0m [2m[38;5;2m2832[0m:  "aes-gcm",
    ...
[2m[38;5;1m3795[0m [2m[38;5;2m3794[0m: ]
[2m[38;5;1m3796[0m [2m[38;5;2m3795[0m: 
[2m[38;5;1m3797[0m [2m[38;5;2m3796[0m: [[package]]
     [38;5;2m3797[39m: [4m[38;5;2mname = "redfog-test-ux"[24m[39m
     [38;5;2m3798[39m: [4m[38;5;2mversion = "0.1.0"[24m[39m
     [38;5;2m3799[39m: [4m[38;5;2mdependencies = [[24m[39m
     [38;5;2m3800[39m: [4m[38;5;2m "eframe",[24m[39m
     [38;5;2m3801[39m: [4m[38;5;2m][24m[39m
     [38;5;2m3802[39m: [4m[38;5;2m[24m[39m
     [38;5;2m3803[39m: [4m[38;5;2m[[package]][24m[39m
[2m[38;5;1m3798[0m [2m[38;5;2m3804[0m: name = "redox_syscall"
[2m[38;5;1m3799[0m [2m[38;5;2m3805[0m: version = "0.3.5"
[2m[38;5;1m3800[0m [2m[38;5;2m3806[0m: source = "registry+https://github.com/rust-lang/crates.io-index"
    ...
[38;5;3mModified regular file Cargo.toml:[39m
    ...
[2m[38;5;1m   4[0m [2m[38;5;2m   4[0m:     "crates/kwin-input",
[2m[38;5;1m   5[0m [2m[38;5;2m   5[0m:     "crates/kwin-viewer",
[2m[38;5;1m   6[0m [2m[38;5;2m   6[0m:     "crates/redfog-login",
     [38;5;2m   7[39m: [4m[38;5;2m    "crates/redfog-test-ux",[24m[39m
[2m[38;5;1m   7[0m [2m[38;5;2m   8[0m:     "crates/redfog-core",
[2m[38;5;1m   8[0m [2m[38;5;2m   9[0m:     "crates/redfog-moonlight",
[2m[38;5;1m   9[0m [2m[38;5;2m  10[0m:     "crates/redfog-server",
[2m[38;5;1m  10[0m [2m[38;5;2m  11[0m: ]
[2m[38;5;1m  11[0m [2m[38;5;2m  12[0m: resolver = "2"
     [38;5;2m  13[39m: [4m[38;5;2m[24m[39m
     [38;5;2m  14[39m: [4m[38;5;2m# Dev-only test dependency (redfog-moonlight's integration tests/examples),[24m[39m
     [38;5;2m  15[39m: [4m[38;5;2m# never shipped in our own server. Patches upstream bugs in its RTSP[24m[39m
     [38;5;2m  16[39m: [4m[38;5;2m# Transport-header parsing (wrong delimiter, wrong port fallback constant,[24m[39m
     [38;5;2m  17[39m: [4m[38;5;2m# didn't handle port ranges) that made the ENet control channel unable to[24m[39m
     [38;5;2m  18[39m: [4m[38;5;2m# connect whenever server_port differed from 47998 — confirmed live. A fix[24m[39m
     [38;5;2m  19[39m: [4m[38;5;2m# will be proposed upstream separately.[24m[39m
     [38;5;2m  20[39m: [4m[38;5;2m#[24m[39m
     [38;5;2m  21[39m: [4m[38;5;2m# Not vendored in git (GPL-3.0-or-later, and we don't want that source[24m[39m
     [38;5;2m  22[39m: [4m[38;5;2m# checked into this repo's history): run `./scripts/fetch-patched-deps.sh`[24m[39m
     [38;5;2m  23[39m: [4m[38;5;2m# once to fetch the pinned commit into `vendor/` (gitignored) and apply[24m[39m
     [38;5;2m  24[39m: [4m[38;5;2m# `patches/moonlight-common-rust-rtsp-port-parsing.patch`.[24m[39m
     [38;5;2m  25[39m: [4m[38;5;2m[patch."https://github.com/MrCreativ3001/moonlight-common-rust"][24m[39m
     [38;5;2m  26[39m: [4m[38;5;2mmoonlight-common = { path = "vendor/moonlight-common-rust" }[24m[39m
[38;5;3mModified regular file crates/redfog-moonlight/src/session.rs:[39m
    ...
[2m[38;5;1m  24[0m [2m[38;5;2m  24[0m:     pub bind_addr: IpAddr,
[2m[38;5;1m  25[0m [2m[38;5;2m  25[0m:     pub video_port: u16,
[2m[38;5;1m  26[0m [2m[38;5;2m  26[0m:     pub audio_port: u16,
     [38;5;2m  27[39m: [4m[38;5;2m    /// Command to run for the Login stage streamed on `/launch`, before the[24m[39m
     [38;5;2m  28[39m: [4m[38;5;2m    /// user has authenticated (e.g. `["target/release/redfog-login"]`).[24m[39m
     [38;5;2m  29[39m: [4m[38;5;2m    /// Overridable so tests can swap in a purpose-built stand-in instead of[24m[39m
     [38;5;2m  30[39m: [4m[38;5;2m    /// the real login GUI.[24m[39m
     [38;5;2m  31[39m: [4m[38;5;2m    pub login_app: Vec<String>,[24m[39m
[2m[38;5;1m  27[0m [2m[38;5;2m  32[0m:     /// Command to run for the real desktop session once login succeeds
[2m[38;5;1m  28[0m [2m[38;5;2m  33[0m:     /// (e.g. `["plasmashell", "--no-respawn"]`).
[2m[38;5;1m  29[0m [2m[38;5;2m  34[0m:     pub user_app: Vec<String>,
    ...
[2m[38;5;1m 159[0m [2m[38;5;2m 164[0m: 
[2m[38;5;1m 160[0m [2m[38;5;2m 165[0m:     fn spawn_session(&self, kind: SessionType, width: u32, height: u32) -> Result<RunningSession, String> {
[2m[38;5;1m 161[0m [2m[38;5;2m 166[0m:         let (socket_name, payload): (&str, Vec<String>) = match &kind {
[38;5;1m 162[39m     : [38;5;1m            SessionType::Login => ("redfog-login-0", [4mvec!["target/release/redfog-login"[24m.[4mto_string[24m()[4m][24m),[39m
     [38;5;2m 167[39m: [38;5;2m            SessionType::Login => ("redfog-login-0", [4mself.config.login_app[24m.[4mclone[24m()),[39m
[2m[38;5;1m 163[0m [2m[38;5;2m 168[0m:             SessionType::User(_) => ("redfog-user-0", self.config.user_app.clone()),
[2m[38;5;1m 164[0m [2m[38;5;2m 169[0m:         };
[2m[38;5;1m 165[0m [2m[38;5;2m 170[0m: 
    ...
[2m[38;5;1m 609[0m [2m[38;5;2m 614[0m:         };
[2m[38;5;1m 610[0m [2m[38;5;2m 615[0m:         let fwd = &session.input_forwarder;
[2m[38;5;1m 611[0m [2m[38;5;2m 616[0m:         match event {
[38;5;1m 612[39m [38;5;2m 617[39m:             InputEvent::KeyDown { keycode } => [4m[38;5;2m{[24m[39m
     [38;5;2m 618[39m: [4m[38;5;2m                tracing::debug!("forwarding KeyDown keycode={keycode}");[24m[39m
[38;5;1m 612[39m [38;5;2m 619[39m: [4m[38;5;2m                [24m[39mfwd.fake_input.keyboard_key(keycode, 1)[4m[38;5;1m,[38;5;2m[24m[39m
[38;5;1m 612[39m [38;5;2m 620[39m: [4m[38;5;2m            }[24m[39m
[38;5;1m 613[39m [38;5;2m 621[39m:             InputEvent::KeyUp { keycode } => [4m[38;5;2m{[24m[39m
     [38;5;2m 622[39m: [4m[38;5;2m                tracing::debug!("forwarding KeyUp keycode={keycode}");[24m[39m
[38;5;1m 613[39m [38;5;2m 623[39m: [4m[38;5;2m                [24m[39mfwd.fake_input.keyboard_key(keycode, 0)[4m[38;5;1m,[38;5;2m[24m[39m
[38;5;1m 613[39m [38;5;2m 624[39m: [4m[38;5;2m            }[24m[39m
[2m[38;5;1m 614[0m [2m[38;5;2m 625[0m:             InputEvent::MouseMoveRelative { dx, dy } => {
[2m[38;5;1m 615[0m [2m[38;5;2m 626[0m:                 tracing::debug!("forwarding MouseMoveRelative dx={dx} dy={dy}");
[2m[38;5;1m 616[0m [2m[38;5;2m 627[0m:                 fwd.fake_input.pointer_motion(dx as f64, dy as f64)
    ...
[38;5;3mModified regular file crates/redfog-moonlight/tests/connection_integration.rs:[39m
    ...
[2m[38;5;1m   4[0m [2m[38;5;2m   4[0m: //! state — never touches a real `redfog-server` that might already be
[2m[38;5;1m   5[0m [2m[38;5;2m   5[0m: //! running on the default ports/runtime dir) and drives it through the exact
[2m[38;5;1m   6[0m [2m[38;5;2m   6[0m: //! scenario that was broken and fixed earlier in this project: connect,
[38;5;1m   7[39m [38;5;2m   7[39m: //! [4m[38;5;2msend input, hand off Login->User while streaming stays alive, [24m[39mclose the[4m[38;5;2m[24m[39m
[38;5;1m   7[39m [38;5;2m   8[39m: [4m[38;5;2m//![24m[39m window without a clean disconnect, reconnect.
[2m[38;5;1m   8[0m [2m[38;5;2m   9[0m: //!
[38;5;1m   9[39m     : [38;5;1m//! Uses `[4mglxgears[24m` as [4mthe "Desktop"[24m app[4m instead[24m [4mof[24m `[4mplasmashell[24m` — it[4m[24m[39m
[38;5;1m  10[39m     : [4m[38;5;1m//![24m [4mrenders[24m continuously[4m,[24m guaranteeing [4ma steady stream of [24mframes without[39m
[38;5;1m  11[39m     : [38;5;1m//! waiting on screen damage from [4manything[24m else [4m(an[24m [4midle[24m [4mdesktop[24m [4monly[24m[39m
[38;5;1m  12[39m     : [38;5;1m//! [4mredraws[24m [4monce[24m a [4mminute[24m [4m— see [24mthis [4mproject[24m's[4m KSplash/damage-source[24m[39m
[38;5;1m  13[39m     : [4m[38;5;1m//! investigation for why that matters for testing)[24m.[39m
     [38;5;2m  10[39m: [38;5;2m//! Uses `[4mredfog-test-ux[24m` [4m(a purpose-built stand-in, see that crate) [24mas [4mboth[24m[39m
     [38;5;2m  11[39m: [4m[38;5;2m//! the Login and User stage instead of the real login GUI or an external[24m app[4m[24m[39m
     [38;5;2m  12[39m: [4m[38;5;2m//![24m [4mlike[24m `[4mglxgears[24m` — it [4mrepaints[24m continuously [4m([24mguaranteeing frames without[39m
     [38;5;2m  13[39m: [38;5;2m//! waiting on screen damage from [4msomething[24m else[4m)[24m [4mand[24m [4mlogs[24m [4mevery[24m [4mmouse/key[24m[39m
     [38;5;2m  14[39m: [38;5;2m//! [4mevent[24m [4mit[24m [4mreceives to stdout in [24ma [4mformat[24m this [4mtest greps for, which is[24m[39m
     [38;5;2m  15[39m: [4m[38;5;2m//! direct proof input reached the session rather than just "the client[24m's[4m[24m[39m
     [38;5;2m  16[39m: [4m[38;5;2m//! send didn't error". It exits on 'Q', giving the test a deterministic way[24m[39m
     [38;5;2m  17[39m: [4m[38;5;2m//! to trigger the Login->User handoff instead of racing on the shared[24m[39m
     [38;5;2m  18[39m: [4m[38;5;2m//! global `/tmp/trigger-login` file `redfog-login` itself uses[24m.[39m
[2m[38;5;1m  14[0m [2m[38;5;2m  19[0m: //!
[38;5;1m  15[39m     : [38;5;1m//! Requires: `cargo build --workspace` first (spawns[4m[24m[39m
[38;5;1m  16[39m     : [4m[38;5;1m//![24m `target/debug/redfog-server` directly rather than depending on [4mit[24m as [4ma[24m[39m
[38;5;1m  17[39m     : [4m[38;5;1m//! crate[24m, since [4mthat[24m would [4mbe[24m a dependency cycle).[39m
     [38;5;2m  20[39m: [38;5;2m//! Requires: `cargo build --workspace` first (spawns `target/debug/[4m[24m[39m
     [38;5;2m  21[39m: [4m[38;5;2m//! [24mredfog-server` [4mand `target/debug/redfog-test-ux` [24mdirectly rather than[4m[24m[39m
     [38;5;2m  22[39m: [4m[38;5;2m//![24m depending on [4mthem[24m as [4mcrates[24m, since [4mredfog-server depends on[24m[39m
     [38;5;2m  23[39m: [4m[38;5;2m//! redfog-moonlight, which[24m would [4mmake[24m [4mit [24ma dependency cycle).[39m
[2m[38;5;1m  18[0m [2m[38;5;2m  24[0m: 
[2m[38;5;1m  19[0m [2m[38;5;2m  25[0m: use std::io::{BufRead, BufReader};
[2m[38;5;1m  20[0m [2m[38;5;2m  26[0m: use std::os::unix::process::CommandExt;
    ...
[2m[38;5;1m  29[0m [2m[38;5;2m  35[0m: use moonlight_common::http::pair::PairPin;
[2m[38;5;1m  30[0m [2m[38;5;2m  36[0m: use moonlight_common::http::{ClientIdentifier, ClientSecret};
[2m[38;5;1m  31[0m [2m[38;5;2m  37[0m: use moonlight_common::stream::audio::AudioConfig;
[38;5;1m  32[39m [38;5;2m  38[39m: use moonlight_common::stream::control::[4m[38;5;2m{[24m[39m
[38;5;1m  32[39m [38;5;2m  39[39m: [4m[38;5;2m    [24m[39mActiveGamepads[4m[38;5;2m, KeyAction, KeyCode, KeyFlags, KeyModifiers, MouseButton, MouseButtonAction,[24m[39m
[38;5;1m  32[39m [38;5;2m  40[39m: [4m[38;5;2m}[24m[39m;
[2m[38;5;1m  33[0m [2m[38;5;2m  41[0m: use moonlight_common::stream::proto::control::input_batcher::ClientInputEvent;
[2m[38;5;1m  34[0m [2m[38;5;2m  42[0m: use moonlight_common::stream::tokio::MoonlightStream;
[2m[38;5;1m  35[0m [2m[38;5;2m  43[0m: use moonlight_common::stream::video::{ColorRange, ColorSpace, VideoCapabilities, VideoFormats};
[2m[38;5;1m  36[0m [2m[38;5;2m  44[0m: use moonlight_common::stream::{AesIv, AesKey, EncryptionFlags, MoonlightStreamSettings, StreamingConfig};
[2m[38;5;1m  37[0m [2m[38;5;2m  45[0m: 
[2m[38;5;1m  38[0m [2m[38;5;2m  46[0m: use redfog_moonlight::tls::ServerIdentity;
[2m[38;5;1m  39[0m [2m[38;5;2m  47[0m: 
     [38;5;2m  48[39m: [4m[38;5;2m/// Windows VK code for 'Q' — what a real client sends; our server's[24m[39m
     [38;5;2m  49[39m: [4m[38;5;2m/// `vk_to_evdev` translates it to the Linux evdev keycode KWin's[24m[39m
     [38;5;2m  50[39m: [4m[38;5;2m/// fake-input protocol expects.[24m[39m
     [38;5;2m  51[39m: [4m[38;5;2mconst VK_Q: i16 = 0x51;[24m[39m
     [38;5;2m  52[39m: [4m[38;5;2m[24m[39m
[2m[38;5;1m  40[0m [2m[38;5;2m  53[0m: fn pick_free_port() -> u16 {
[2m[38;5;1m  41[0m [2m[38;5;2m  54[0m:     std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
[2m[38;5;1m  42[0m [2m[38;5;2m  55[0m: }
[2m[38;5;1m  43[0m [2m[38;5;2m  56[0m: 
[38;5;1m  44[39m [38;5;2m  57[39m: fn [4m[38;5;1mredfog_server_binary[38;5;2mworkspace_binary[24m[39m([4m[38;5;2mname: &str[24m[39m) -> PathBuf {
[2m[38;5;1m  45[0m [2m[38;5;2m  58[0m:     // CARGO_MANIFEST_DIR is crates/redfog-moonlight; the workspace target
[38;5;1m  46[39m     : [38;5;1m    // dir is two levels up. redfog-server can[4m't[24m be a dev-dependency of this[4m[24m[39m
[38;5;1m  47[39m     : [4m[38;5;1m    //[24m crate ([4mit[24m depends on redfog-moonlight itself), so there's no[4m[24m[39m
[38;5;1m  48[39m     : [4m[38;5;1m    //[24m CARGO_BIN_EXE_* env var for [4mit[24m — locate the [4mbinary[24m directly instead.[39m
[38;5;1m  49[39m     : [38;5;1m    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/[4mredfog-server[24m");[39m
[38;5;1m  50[39m     : [38;5;1m    assert!([4m[24m[39m
[38;5;1m  51[39m     : [4m[38;5;1m        [24mpath.exists(),[4m[24m[39m
[38;5;1m  52[39m     : [4m[38;5;1m       [24m "[4mredfog-server[24m binary not found at {path:?} — run `cargo build --workspace` first"[4m[24m[39m
[38;5;1m  53[39m     : [4m[38;5;1m    [24m);[39m
     [38;5;2m  59[39m: [38;5;2m    // dir is two levels up. [4mNeither [24mredfog-server [4mnor redfog-test-ux [24mcan be[4m[24m[39m
     [38;5;2m  60[39m: [4m[38;5;2m    //[24m a dev-dependency of this crate ([4mredfog-server[24m depends on[4m[24m[39m
     [38;5;2m  61[39m: [4m[38;5;2m    //[24m redfog-moonlight itself), so there's no CARGO_BIN_EXE_* env var for[4m[24m[39m
     [38;5;2m  62[39m: [4m[38;5;2m    //[24m [4mthem[24m — locate the [4mbinaries[24m directly instead.[39m
     [38;5;2m  63[39m: [38;5;2m    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join([4mformat!([24m"../../target/debug/[4m{name}[24m")[4m)[24m;[39m
     [38;5;2m  64[39m: [38;5;2m    assert!(path.exists(), "[4m{name}[24m binary not found at {path:?} — run `cargo build --workspace` first");[39m
[2m[38;5;1m  54[0m [2m[38;5;2m  65[0m:     path
[2m[38;5;1m  55[0m [2m[38;5;2m  66[0m: }
[2m[38;5;1m  56[0m [2m[38;5;2m  67[0m: 
[2m[38;5;1m  57[0m [2m[38;5;2m  68[0m: /// Kills the whole process group on drop (the child is spawned as its own
[2m[38;5;1m  58[0m [2m[38;5;2m  69[0m: /// group leader via `process_group(0)`), so the
[38;5;1m  59[39m [38;5;2m  70[39m: /// dbus-run-session/pipewire/wireplumber/kwin_wayland/[4m[38;5;1mglxgears[38;5;2mredfog-test-ux[24m[39m tree
[2m[38;5;1m  60[0m [2m[38;5;2m  71[0m: /// underneath doesn't leak past the test.
[2m[38;5;1m  61[0m [2m[38;5;2m  72[0m: struct ServerProcess {
[2m[38;5;1m  62[0m [2m[38;5;2m  73[0m:     child: Child,
    ...
[2m[38;5;1m  79[0m [2m[38;5;2m  90[0m: }
[2m[38;5;1m  80[0m [2m[38;5;2m  91[0m: 
[2m[38;5;1m  81[0m [2m[38;5;2m  92[0m: impl TestServer {
[38;5;1m  82[39m     : [4m[38;5;1m    fn stdout_contains(&self, needle: &str) -> bool {[24m[39m
[38;5;1m  83[39m     : [4m[38;5;1m        self.stdout_lines.lock().unwrap().iter().any(|line| line.contains(needle))[24m[39m
[38;5;1m  84[39m     : [4m[38;5;1m    }[24m[39m
[38;5;1m  85[39m     : [4m[38;5;1m}[24m[39m
[38;5;1m  86[39m     : [4m[38;5;1m[24m[39m
[38;5;1m  87[39m     : [4m[38;5;1mimpl TestServer {[24m[39m
[2m[38;5;1m  88[0m [2m[38;5;2m  93[0m:     fn spawn() -> Self {
[2m[38;5;1m  89[0m [2m[38;5;2m  94[0m:         let runtime_dir = std::env::temp_dir().join(format!("redfog-it-runtime-{}", uuid::Uuid::new_v4()));
[2m[38;5;1m  90[0m [2m[38;5;2m  95[0m:         std::fs::create_dir_all(&runtime_dir).unwrap();
    ...
[2m[38;5;1m  96[0m [2m[38;5;2m 101[0m:         let control_port = pick_free_port();
[2m[38;5;1m  97[0m [2m[38;5;2m 102[0m:         let audio_port = pick_free_port();
[2m[38;5;1m  98[0m [2m[38;5;2m 103[0m: 
[38;5;1m  99[39m [38;5;2m 104[39m:         let [4m[38;5;2mtest_ux = workspace_binary("redfog-test-ux");[24m[39m
     [38;5;2m 105[39m: [4m[38;5;2m        let test_ux = test_ux.to_str().unwrap();[24m[39m
     [38;5;2m 106[39m: [4m[38;5;2m[24m[39m
[38;5;1m  99[39m [38;5;2m 107[39m: [4m[38;5;2m        let [24m[39mmut cmd = Command::new([4m[38;5;1mredfog_server_binary[38;5;2mworkspace_binary[24m[39m([4m[38;5;2m"redfog-server"[24m[39m));
[2m[38;5;1m 100[0m [2m[38;5;2m 108[0m:         cmd.env("REDFOG_RUNTIME_DIR", &runtime_dir)
[2m[38;5;1m 101[0m [2m[38;5;2m 109[0m:             .env("REDFOG_HTTP_PORT", http_port.to_string())
[2m[38;5;1m 102[0m [2m[38;5;2m 110[0m:             .env("REDFOG_HTTPS_PORT", https_port.to_string())
[2m[38;5;1m 103[0m [2m[38;5;2m 111[0m:             .env("REDFOG_RTSP_PORT", rtsp_port.to_string())
[2m[38;5;1m 104[0m [2m[38;5;2m 112[0m:             .env("REDFOG_VIDEO_PORT", video_port.to_string())
[2m[38;5;1m 105[0m [2m[38;5;2m 113[0m:             .env("REDFOG_CONTROL_PORT", control_port.to_string())
[2m[38;5;1m 106[0m [2m[38;5;2m 114[0m:             .env("REDFOG_AUDIO_PORT", audio_port.to_string())
[38;5;1m 107[39m [38;5;2m 115[39m:             .env("[4m[38;5;2mREDFOG_LOGIN_APP", test_ux)[24m[39m
[38;5;1m 107[39m [38;5;2m 116[39m: [4m[38;5;2m            .env("[24m[39mREDFOG_USER_APP", [4m[38;5;1m"glxgears"[38;5;2mtest_ux[24m[39m)
[2m[38;5;1m 108[0m [2m[38;5;2m 117[0m:             .env("RUST_LOG", "redfog_moonlight=debug,redfog_server=debug")
[2m[38;5;1m 109[0m [2m[38;5;2m 118[0m:             .stdout(Stdio::piped())
[2m[38;5;1m 110[0m [2m[38;5;2m 119[0m:             .stderr(Stdio::piped())
[2m[38;5;1m 111[0m [2m[38;5;2m 120[0m:             // Own process group so Drop can kill the whole tree (dbus-run-
[2m[38;5;1m 112[0m [2m[38;5;2m 121[0m:             // session -> redfog-server -> pipewire/wireplumber/kwin_wayland
[38;5;1m 113[39m [38;5;2m 122[39m:             // -> [4m[38;5;1mglxgears/[24m[39mredfog-[4m[38;5;1mlogin[38;5;2mtest-ux[24m[39m) with one signal.
[2m[38;5;1m 114[0m [2m[38;5;2m 123[0m:             .process_group(0);
[2m[38;5;1m 115[0m [2m[38;5;2m 124[0m: 
[2m[38;5;1m 116[0m [2m[38;5;2m 125[0m:         let mut child = cmd.spawn().expect("spawn redfog-server");
[2m[38;5;1m 117[0m [2m[38;5;2m 126[0m: 
[38;5;1m 118[39m     : [4m[38;5;1m        // Mirror both streams into this test's own output (visible on[24m[39m
[38;5;1m 119[39m     : [4m[38;5;1m        // failure / with `cargo test -- --nocapture`) and keep a copy of[24m[39m
[38;5;1m 120[39m     : [4m[38;5;1m        // stdout lines so assertions can grep it later (e.g. to confirm[24m[39m
[38;5;1m 121[39m     : [4m[38;5;1m        // mouse input actually reached the input forwarder).[24m[39m
[2m[38;5;1m 122[0m [2m[38;5;2m 127[0m:         let stdout_lines = Arc::new(Mutex::new(Vec::<String>::new()));
[2m[38;5;1m 123[0m [2m[38;5;2m 128[0m:         {
[2m[38;5;1m 124[0m [2m[38;5;2m 129[0m:             let stdout = child.stdout.take().unwrap();
    ...
[2m[38;5;1m 159[0m [2m[38;5;2m 164[0m:             stdout_lines,
[2m[38;5;1m 160[0m [2m[38;5;2m 165[0m:         }
[2m[38;5;1m 161[0m [2m[38;5;2m 166[0m:     }
     [38;5;2m 167[39m: [4m[38;5;2m[24m[39m
     [38;5;2m 168[39m: [4m[38;5;2m    fn stdout_contains(&self, needle: &str) -> bool {[24m[39m
     [38;5;2m 169[39m: [4m[38;5;2m        self.stdout_lines.lock().unwrap().iter().any(|line| line.contains(needle))[24m[39m
     [38;5;2m 170[39m: [4m[38;5;2m    }[24m[39m
     [38;5;2m 171[39m: [4m[38;5;2m[24m[39m
     [38;5;2m 172[39m: [4m[38;5;2m    fn count_stdout(&self, needle: &str) -> usize {[24m[39m
     [38;5;2m 173[39m: [4m[38;5;2m        self.stdout_lines.lock().unwrap().iter().filter(|line| line.contains(needle)).count()[24m[39m
     [38;5;2m 174[39m: [4m[38;5;2m    }[24m[39m
     [38;5;2m 175[39m: [4m[38;5;2m[24m[39m
     [38;5;2m 176[39m: [4m[38;5;2m    async fn wait_for_stdout(&self, needle: &str, timeout: Duration) {[24m[39m
     [38;5;2m 177[39m: [4m[38;5;2m        let deadline = tokio::time::Instant::now() + timeout;[24m[39m
     [38;5;2m 178[39m: [4m[38;5;2m        while !self.stdout_contains(needle) {[24m[39m
     [38;5;2m 179[39m: [4m[38;5;2m            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {needle:?} in redfog-server's output");[24m[39m
     [38;5;2m 180[39m: [4m[38;5;2m            tokio::time::sleep(Duration::from_millis(50)).await;[24m[39m
     [38;5;2m 181[39m: [4m[38;5;2m        }[24m[39m
     [38;5;2m 182[39m: [4m[38;5;2m    }[24m[39m
     [38;5;2m 183[39m: [4m[38;5;2m[24m[39m
     [38;5;2m 184[39m: [4m[38;5;2m    /// Waits for `needle`'s occurrence count to exceed `baseline` — unlike[24m[39m
     [38;5;2m 185[39m: [4m[38;5;2m    /// `wait_for_stdout`, safe to reuse the same needle across multiple[24m[39m
     [38;5;2m 186[39m: [4m[38;5;2m    /// checkpoints in one test, since stdout is a growing, never-cleared[24m[39m
     [38;5;2m 187[39m: [4m[38;5;2m    /// log and a plain "does it appear" check would trivially pass on a[24m[39m
     [38;5;2m 188[39m: [4m[38;5;2m    /// stale match from earlier.[24m[39m
     [38;5;2m 189[39m: [4m[38;5;2m    async fn wait_for_new_stdout(&self, needle: &str, baseline: usize, timeout: Duration) {[24m[39m
     [38;5;2m 190[39m: [4m[38;5;2m        let deadline = tokio::time::Instant::now() + timeout;[24m[39m
     [38;5;2m 191[39m: [4m[38;5;2m        while self.count_stdout(needle) <= baseline {[24m[39m
     [38;5;2m 192[39m: [4m[38;5;2m            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for a new {needle:?} in redfog-server's output");[24m[39m
     [38;5;2m 193[39m: [4m[38;5;2m            tokio::time::sleep(Duration::from_millis(50)).await;[24m[39m
     [38;5;2m 194[39m: [4m[38;5;2m        }[24m[39m
     [38;5;2m 195[39m: [4m[38;5;2m    }[24m[39m
[2m[38;5;1m 162[0m [2m[38;5;2m 196[0m: }
[2m[38;5;1m 163[0m [2m[38;5;2m 197[0m: 
[2m[38;5;1m 164[0m [2m[38;5;2m 198[0m: fn default_stream_settings() -> MoonlightStreamSettings {
    ...
[2m[38;5;1m 194[0m [2m[38;5;2m 228[0m:     }
[2m[38;5;1m 195[0m [2m[38;5;2m 229[0m: }
[2m[38;5;1m 196[0m [2m[38;5;2m 230[0m: 
[38;5;1m 197[39m [38;5;2m 231[39m: /// [4m[38;5;1mPolls `stream` for up to `duration`, returning (video_frames, video_bytes,[24m[39m
[38;5;1m 198[39m     : [4m[38;5;1m/// audio_frames, audio_bytes).[24m[39m
[38;5;1m 199[39m [38;5;2m 231[39m: [4m[38;5;1masync fn collect_frames(stream: &MoonlightStream, duration: Duration) -> (usize, usize, usize, usize) {[38;5;2m`send_input` can fail with `NotConnected` if called before the control[24m[39m
     [38;5;2m 232[39m: [4m[38;5;2m/// channel's own ENet handshake (a handful of round trips, separate from[24m[39m
     [38;5;2m 233[39m: [4m[38;5;2m/// the RTSP handshake `MoonlightStream::connect` waits for) has finished —[24m[39m
     [38;5;2m 234[39m: [4m[38;5;2m/// confirmed live. Retry briefly instead of requiring callers to guess a[24m[39m
     [38;5;2m 235[39m: [4m[38;5;2m/// safe fixed delay.[24m[39m
     [38;5;2m 236[39m: [4m[38;5;2masync fn send_input_retrying(stream: &MoonlightStream, event: ClientInputEvent) {[24m[39m
     [38;5;2m 237[39m: [4m[38;5;2m    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);[24m[39m
     [38;5;2m 238[39m: [4m[38;5;2m    loop {[24m[39m
     [38;5;2m 239[39m: [4m[38;5;2m        match stream.send_input(event.clone()) {[24m[39m
     [38;5;2m 240[39m: [4m[38;5;2m            Ok(()) => return,[24m[39m
     [38;5;2m 241[39m: [4m[38;5;2m            Err(e) if tokio::time::Instant::now() < deadline => {[24m[39m
     [38;5;2m 242[39m: [4m[38;5;2m                eprintln!("send_input not ready yet ({e}), retrying");[24m[39m
     [38;5;2m 243[39m: [4m[38;5;2m                tokio::time::sleep(Duration::from_millis(100)).await;[24m[39m
     [38;5;2m 244[39m: [4m[38;5;2m            }[24m[39m
     [38;5;2m 245[39m: [4m[38;5;2m            Err(e) => panic!("send_input failed after retrying: {e}"),[24m[39m
     [38;5;2m 246[39m: [4m[38;5;2m        }[24m[39m
     [38;5;2m 247[39m: [4m[38;5;2m    }[24m[39m
     [38;5;2m 248[39m: [4m[38;5;2m}[24m[39m
     [38;5;2m 249[39m: [4m[38;5;2m[24m[39m
     [38;5;2m 250[39m: [4m[38;5;2masync fn send_key(stream: &MoonlightStream, vk: i16, down: bool) {[24m[39m
     [38;5;2m 251[39m: [4m[38;5;2m    send_input_retrying([24m[39m
     [38;5;2m 252[39m: [4m[38;5;2m        stream,[24m[39m
     [38;5;2m 253[39m: [4m[38;5;2m        ClientInputEvent::Keyboard {[24m[39m
     [38;5;2m 254[39m: [4m[38;5;2m            action: if down { KeyAction::Down } else { KeyAction::Up },[24m[39m
     [38;5;2m 255[39m: [4m[38;5;2m            flags: KeyFlags::empty(),[24m[39m
     [38;5;2m 256[39m: [4m[38;5;2m            key_code: KeyCode(vk),[24m[39m
     [38;5;2m 257[39m: [4m[38;5;2m            modifiers: KeyModifiers::empty(),[24m[39m
     [38;5;2m 258[39m: [4m[38;5;2m        },[24m[39m
     [38;5;2m 259[39m: [4m[38;5;2m    )[24m[39m
     [38;5;2m 260[39m: [4m[38;5;2m    .await;[24m[39m
     [38;5;2m 261[39m: [4m[38;5;2m}[24m[39m
     [38;5;2m 262[39m: [4m[38;5;2m[24m[39m
     [38;5;2m 263[39m: [4m[38;5;2m/// Continuously polls `stream` for video/audio frames until `stop` resolves,[24m[39m
     [38;5;2m 264[39m: [4m[38;5;2m/// tracking the longest gap between consecutive video frames — used to[24m[39m
     [38;5;2m 265[39m: [4m[38;5;2m/// prove streaming never stalls across the Login->User handoff (the video-[24m[39m
     [38;5;2m 266[39m: [4m[38;5;2m/// continuity bug fixed earlier reset RTP state on every handoff and froze[24m[39m
     [38;5;2m 267[39m: [4m[38;5;2m/// the stream instead).[24m[39m
     [38;5;2m 268[39m: [4m[38;5;2masync fn poll_frames_tracking_gaps(stream: &MoonlightStream, stop: impl std::future::Future<Output = ()>) -> (usize, Duration) {[24m[39m
[38;5;1m 199[39m [38;5;2m 269[39m: [4m[38;5;2m    tokio::pin!(stop);[24m[39m
[2m[38;5;1m 200[0m [2m[38;5;2m 270[0m:     let mut video_frames = 0usize;
[38;5;1m 201[39m     : [38;5;1m    let mut [4mvideo_bytes = 0usize;[24m[39m
[38;5;1m 202[39m     : [4m[38;5;1m    let mut audio_frames = 0usize;[24m[39m
[38;5;1m 203[39m     : [4m[38;5;1m    let mut audio_bytes = 0usize;[24m[39m
[38;5;1m 204[39m     : [4m[38;5;1m[24m[39m
[38;5;1m 205[39m     : [4m[38;5;1m    let deadline = [24mtokio::time::Instant::[4mnow() + duration[24m;[39m
     [38;5;2m 271[39m: [38;5;2m    let mut [4mlast_frame_at: Option<[24mtokio::time::Instant[4m> = None;[24m[39m
     [38;5;2m 272[39m: [4m[38;5;2m    let mut max_gap = Duration[24m::[4mZERO[24m;[39m
[2m[38;5;1m 206[0m [2m[38;5;2m 273[0m:     loop {
[2m[38;5;1m 207[0m [2m[38;5;2m 274[0m:         tokio::select! {
[38;5;1m 208[39m [38;5;2m 275[39m:             _ = [4m[38;5;1mtokio::time::sleep_until(deadline)[38;5;2m&mut stop[24m[39m => break,
[2m[38;5;1m 209[0m [2m[38;5;2m 276[0m:             frame = stream.poll_video_frame() => {
[38;5;1m 210[39m     : [38;5;1m                [4mmatch[24m [4mframe {[24m[39m
[38;5;1m 211[39m     : [4m[38;5;1m                    Ok(frame) => {[24m[39m
[38;5;1m 212[39m     : [4m[38;5;1m                        video_frames += 1;[24m[39m
[38;5;1m 213[39m     : [4m[38;5;1m                        video_bytes += frame.raw().len();[24m[39m
[38;5;1m 214[39m     : [4m[38;5;1m                    }[24m[39m
[38;5;1m 215[39m     : [4m[38;5;1m                    Err(_) =>[24m break[4m,[24m[39m
     [38;5;2m 277[39m: [38;5;2m                [4mif[24m [4mframe.is_err() {[24m[39m
     [38;5;2m 278[39m: [4m[38;5;2m                   [24m break[4m;[24m[39m
[2m[38;5;1m 216[0m [2m[38;5;2m 279[0m:                 }
[38;5;1m 217[39m     : [38;5;1m            [4m}[24m[39m
[38;5;1m 218[39m     : [4m[38;5;1m            frame = stream.poll_audio_frame() => {[24m[39m
[38;5;1m 219[39m     : [4m[38;5;1m                match frame {[24m[39m
[38;5;1m 220[39m     : [4m[38;5;1m                    Ok(frame) => {[24m[39m
[38;5;1m 221[39m     : [4m[38;5;1m                        audio_frames += 1[24m;[39m
[38;5;1m 222[39m     : [38;5;1m                  [4m      audio_bytes +[24m= [4mframe[24m.[4mbuffer[24m.[4mlen[24m();[39m
[38;5;1m 223[39m     : [4m[38;5;1m                    }[24m[39m
[38;5;1m 224[39m     : [4m[38;5;1m                    Err(_) => break,[24m[39m
     [38;5;2m 280[39m: [38;5;2m            [4m    let now = tokio::time::Instant::now()[24m;[39m
     [38;5;2m 281[39m: [38;5;2m                [4mif[24m [4mlet[24m [4mSome(last) = last_frame_at {[24m[39m
     [38;5;2m 282[39m: [4m[38;5;2m                    max_gap [24m= [4mmax_gap[24m.[4mmax(now[24m.[4mduration_since[24m([4mlast[24m)[4m)[24m;[39m
[2m[38;5;1m 225[0m [2m[38;5;2m 283[0m:                 }
     [38;5;2m 284[39m: [4m[38;5;2m                last_frame_at = Some(now);[24m[39m
     [38;5;2m 285[39m: [4m[38;5;2m                video_frames += 1;[24m[39m
[2m[38;5;1m 226[0m [2m[38;5;2m 286[0m:             }
     [38;5;2m 287[39m: [4m[38;5;2m            _ = stream.poll_audio_frame() => {}[24m[39m
[2m[38;5;1m 227[0m [2m[38;5;2m 288[0m:         }
[2m[38;5;1m 228[0m [2m[38;5;2m 289[0m:     }
[38;5;1m 229[39m [38;5;2m 290[39m:     (video_frames, [4m[38;5;1mvideo_bytes, audio_frames, audio_bytes[38;5;2mmax_gap[24m[39m)
[2m[38;5;1m 230[0m [2m[38;5;2m 291[0m: }
[2m[38;5;1m 231[0m [2m[38;5;2m 292[0m: 
[2m[38;5;1m 232[0m [2m[38;5;2m 293[0m: #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    ...
[2m[38;5;1m 266[0m [2m[38;5;2m 327[0m:     let codec_support = host.server_codec_mode_support().await.expect("codec support");
[2m[38;5;1m 267[0m [2m[38;5;2m 328[0m:     settings.adjust_for_server(server_version, &gfe_version, codec_support).expect("settings compatible");
[2m[38;5;1m 268[0m [2m[38;5;2m 329[0m: 
[38;5;1m 269[39m [38;5;2m 330[39m:     // ---- First connection[4m[38;5;2m: the Login stage.[24m[39m ----
[2m[38;5;1m 270[0m [2m[38;5;2m 331[0m:     let stream_config = host
[2m[38;5;1m 271[0m [2m[38;5;2m 332[0m:         .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
[2m[38;5;1m 272[0m [2m[38;5;2m 333[0m:         .await
    ...
[2m[38;5;1m 276[0m [2m[38;5;2m 337[0m:         .await
[2m[38;5;1m 277[0m [2m[38;5;2m 338[0m:         .expect("first stream must connect");
[2m[38;5;1m 278[0m [2m[38;5;2m 339[0m: 
[38;5;1m 279[39m     : [38;5;1m    [4m// The "Desktop"[24m [4mapp[24m starts on [4mthe Login compositor (redfog-login);[24m[39m
[38;5;1m 280[39m     : [4m[38;5;1m    // simulate [24ma [4msuccessful[24m [4mlogin via the same headless test-automation[24m[39m
[38;5;1m 281[39m     : [4m[38;5;1m    // trigger redfog-login itself supports, rather than driving its GUI.[24m[39m
[38;5;1m 282[39m     : [4m[38;5;1m    std::fs::write("/tmp/trigger-login", "").expect("write trigger-login");[24m[39m
[38;5;1m 283[39m     : [4m[38;5;1m    // Give the Login->User handoff (compositor teardown + glxgears spawn)[24m[39m
[38;5;1m 284[39m     : [4m[38;5;1m    // [24mtime to [4mcomplete[24m [4mbefore[24m [4mexpecting[24m [4mglxgears'[24m [4mframes[24m [4mspecifically.[24m[39m
[38;5;1m 285[39m     : [38;5;1m    [4mtokio::time::sleep[24m(Duration::from_secs(3))[4m.await[24m;[39m
[38;5;1m 286[39m     : [38;5;1m[39m
[38;5;1m 287[39m     : [38;5;1m    [4mlet[24m ([4mvideo_frames, video_bytes, audio_frames, _audio_bytes[24m)[4m =[24m [4mcollect_frames[24m(&stream, Duration::from_secs(5)).await;[39m
[38;5;1m 288[39m     : [4m[38;5;1m    assert!(video_frames > 0, "expected glxgears video frames on the first connection, got {video_frames}");[24m[39m
[38;5;1m 289[39m     : [4m[38;5;1m    assert!(video_bytes > 0, "expected nonzero video bytes on the first connection");[24m[39m
[38;5;1m 290[39m     : [4m[38;5;1m    assert!(audio_frames > 0, "expected audio frames on the first connection, got {audio_frames}");[24m[39m
     [38;5;2m 340[39m: [38;5;2m    [4mserver.wait_for_stdout("TESTUX[redfog-login-0]: started", Duration::from_secs(10)).await;[24m[39m
     [38;5;2m 341[39m: [4m[38;5;2m[24m[39m
     [38;5;2m 342[39m: [4m[38;5;2m    // ---- Simulated client mouse movement + key press, verified by proof[24m[39m
     [38;5;2m 343[39m: [4m[38;5;2m    // it actually reached the Login-stage session, not just that the[24m[39m
     [38;5;2m 344[39m: [4m[38;5;2m    // client's send didn't error. Absolute, targeting the window's likely[24m[39m
     [38;5;2m 345[39m: [4m[38;5;2m    // center — a small relative move from an unknown starting cursor[24m[39m
     [38;5;2m 346[39m: [4m[38;5;2m    // position may never land inside test-ux's (non-fullscreen) window at[24m[39m
     [38;5;2m 347[39m: [4m[38;5;2m    // all, so it'd never see the event even though the compositor correctly[24m[39m
     [38;5;2m 348[39m: [4m[38;5;2m    // received and forwarded it. ----[24m[39m
     [38;5;2m 349[39m: [4m[38;5;2m    send_input_retrying([24m[39m
     [38;5;2m 350[39m: [4m[38;5;2m        &stream,[24m[39m
     [38;5;2m 351[39m: [4m[38;5;2m        ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },[24m[39m
     [38;5;2m 352[39m: [4m[38;5;2m    )[24m[39m
     [38;5;2m 353[39m: [4m[38;5;2m    .await;[24m[39m
     [38;5;2m 354[39m: [4m[38;5;2m    server.wait_for_stdout("TESTUX[redfog-login-0]: pointer_moved", Duration::from_secs(5)).await;[24m[39m
     [38;5;2m 355[39m: [4m[38;5;2m[24m[39m
     [38;5;2m 356[39m: [4m[38;5;2m    // A window only gets *keyboard* focus from a click, not just pointer[24m[39m
     [38;5;2m 357[39m: [4m[38;5;2m    // hover — confirmed live: sending a key press right after the mouse[24m[39m
     [38;5;2m 358[39m: [4m[38;5;2m    // move above (no click) reached fake_input and got forwarded[24m[39m
     [38;5;2m 359[39m: [4m[38;5;2m    // server-side, but test-ux never saw it.[24m[39m
     [38;5;2m 360[39m: [4m[38;5;2m    send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Press, button: MouseButton::Left }).await;[24m[39m
     [38;5;2m 361[39m: [4m[38;5;2m    send_input_retrying(&stream, ClientInputEvent::MouseButton { action: MouseButtonAction::Release, button: MouseButton::Left }).await;[24m[39m
     [38;5;2m 362[39m: [4m[38;5;2m    tokio::time::sleep(Duration::from_millis(200)).await;[24m[39m
     [38;5;2m 363[39m: [4m[38;5;2m[24m[39m
     [38;5;2m 364[39m: [4m[38;5;2m    send_key(&stream, VK_Q.wrapping_add(1) /* VK_R, an arbitrary non-exit key */, true).await;[24m[39m
     [38;5;2m 365[39m: [4m[38;5;2m    server.wait_for_stdout("TESTUX[redfog-login-0]: key_pressed", Duration::from_secs(5)).await;[24m[39m
     [38;5;2m 366[39m: [4m[38;5;2m[24m[39m
     [38;5;2m 367[39m: [4m[38;5;2m    // ---- Confirm streaming works before touching the handoff, and start[24m[39m
     [38;5;2m 368[39m: [4m[38;5;2m    // tracking frame gaps from here through the handoff below. ----[24m[39m
     [38;5;2m 369[39m: [4m[38;5;2m    let (login_frames, _) = poll_frames_tracking_gaps(&stream, tokio::time::sleep(Duration::from_secs(2))).await;[24m[39m
     [38;5;2m 370[39m: [4m[38;5;2m    assert!(login_frames > 0, "expected video frames from the Login-stage test UX");[24m[39m
     [38;5;2m 371[39m: [4m[38;5;2m[24m[39m
     [38;5;2m 372[39m: [4m[38;5;2m    // ---- Trigger the Login->User handoff deterministically (redfog-test-ux[24m[39m
     [38;5;2m 373[39m: [4m[38;5;2m    // exits on 'Q', same `--exit-with-session` trigger a real login success[24m[39m
     [38;5;2m 374[39m: [4m[38;5;2m    // uses) while continuously polling frames, proving the stream survives[24m[39m
     [38;5;2m 375[39m: [4m[38;5;2m    // the handoff instead of stalling — the exact bug fixed earlier, where[24m[39m
     [38;5;2m 376[39m: [4m[38;5;2m    // resetting RTP sequence/frame-index state on every compositor handoff[24m[39m
     [38;5;2m 377[39m: [4m[38;5;2m    // froze the video permanently. ----[24m[39m
     [38;5;2m 378[39m: [4m[38;5;2m    send_key(&stream, VK_Q, true).await;[24m[39m
     [38;5;2m 379[39m: [4m[38;5;2m    send_key(&stream, VK_Q, false).await;[24m[39m
     [38;5;2m 380[39m: [4m[38;5;2m    let (handoff_frames, max_gap) = poll_frames_tracking_gaps(&stream, async {[24m[39m
     [38;5;2m 381[39m: [4m[38;5;2m        server.wait_for_stdout("TESTUX[redfog-user-0]: started", Duration::from_secs(15)).await;[24m[39m
     [38;5;2m 382[39m: [4m[38;5;2m        // A little settle time after the User[24m [4mstage[24m starts[4m, so the next[24m[39m
     [38;5;2m 383[39m: [4m[38;5;2m        // input-verification step lands[24m on a [4msession[24m [4mthat's fully up.[24m[39m
     [38;5;2m 384[39m: [4m[38;5;2m        tokio::[24mtime[4m::sleep(Duration::from_millis(500)).await;[24m[39m
     [38;5;2m 385[39m: [4m[38;5;2m    })[24m[39m
     [38;5;2m 386[39m: [4m[38;5;2m    .await;[24m[39m
     [38;5;2m 387[39m: [4m[38;5;2m    assert!(handoff_frames > 0, "expected video frames[24m to [4mkeep[24m [4mflowing[24m [4macross[24m [4mthe[24m [4mLogin->User[24m [4mhandoff");[24m[39m
     [38;5;2m 388[39m: [38;5;2m    [4massert![24m([4m[24m[39m
     [38;5;2m 389[39m: [4m[38;5;2m        max_gap < [24mDuration::from_secs(3)[4m,[24m[39m
     [38;5;2m 390[39m: [4m[38;5;2m        "video stalled for {max_gap:?} across the handoff — the video-continuity bug is back"[24m[39m
     [38;5;2m 391[39m: [4m[38;5;2m    [24m);[39m
     [38;5;2m 392[39m: [38;5;2m[39m
     [38;5;2m 393[39m: [38;5;2m    [4m// ---- Confirm input reaches the new, User-stage session too. Absolute,[24m[39m
     [38;5;2m 394[39m: [4m[38;5;2m    // not relative — this is a brand new KWin compositor instance[24m ([4ma[24m[39m
     [38;5;2m 395[39m: [4m[38;5;2m    // separate process from the Login stage's[24m)[4m, so the cursor's position[24m[39m
     [38;5;2m 396[39m: [4m[38;5;2m    // here is unknown/undefined, same reasoning as the Login-stage move[24m[39m
     [38;5;2m 397[39m: [4m[38;5;2m    // above. ----[24m[39m
     [38;5;2m 398[39m: [4m[38;5;2m   [24m [4msend_input_retrying[24m([4m[24m[39m
     [38;5;2m 399[39m: [4m[38;5;2m        [24m&stream,[4m[24m[39m
     [38;5;2m 400[39m: [4m[38;5;2m        ClientInputEvent::MouseMoveAbsolute { x: 640, y: 360, reference_width: 1280, reference_height: 720 },[24m[39m
     [38;5;2m 401[39m: [4m[38;5;2m    )[24m[39m
     [38;5;2m 402[39m: [4m[38;5;2m    .await;[24m[39m
     [38;5;2m 403[39m: [4m[38;5;2m    server.wait_for_stdout("TESTUX[redfog-user-0]: pointer_moved",[24m Duration::from_secs(5)).await;[39m
[2m[38;5;1m 291[0m [2m[38;5;2m 404[0m: 
[2m[38;5;1m 292[0m [2m[38;5;2m 405[0m:     // ---- Simulate closing the window: drop the stream without any clean
[2m[38;5;1m 293[0m [2m[38;5;2m 406[0m:     // RTSP TEARDOWN / control-channel disconnect, exactly like a closed
    ...
[2m[38;5;1m 296[0m [2m[38;5;2m 409[0m:     tokio::time::sleep(Duration::from_millis(500)).await;
[2m[38;5;1m 297[0m [2m[38;5;2m 410[0m: 
[2m[38;5;1m 298[0m [2m[38;5;2m 411[0m:     // ---- Reconnect: a brand new stream_config/AES key, same client. This
[38;5;1m 299[39m     : [38;5;1m    // is the retake path (server state is still `Streaming` from the[4m[24m[39m
[38;5;1m 300[39m     : [4m[38;5;1m    //[24m abandoned first connection) — the exact scenario that was broken[4m[24m[39m
[38;5;1m 301[39m     : [4m[38;5;1m    //[24m (stale queued PING misrouting the stream[4m)[24m and fixed. ----[39m
     [38;5;2m 412[39m: [38;5;2m    // is the retake path (server state is still `Streaming` [4mon the User[24m[39m
     [38;5;2m 413[39m: [4m[38;5;2m    // stage [24mfrom the abandoned first connection) — the exact scenario that[4m[24m[39m
     [38;5;2m 414[39m: [4m[38;5;2m    //[24m was broken (stale queued PING misrouting the stream[4m,[24m and [4mthe new[24m[39m
     [38;5;2m 415[39m: [4m[38;5;2m    // peer's own control connection getting caught by its own stale-peer[24m[39m
     [38;5;2m 416[39m: [4m[38;5;2m    // disconnect sweep) and [24mfixed. ----[39m
[2m[38;5;1m 302[0m [2m[38;5;2m 417[0m:     let stream_config = host
[2m[38;5;1m 303[0m [2m[38;5;2m 418[0m:         .start_stream(1, &settings, AesKey::new_random(&RustCryptoBackend).expect("aes key"), AesIv(1), "")
[2m[38;5;1m 304[0m [2m[38;5;2m 419[0m:         .await
    ...
[2m[38;5;1m 307[0m [2m[38;5;2m 422[0m:         .await
[2m[38;5;1m 308[0m [2m[38;5;2m 423[0m:         .expect("reconnect stream must connect");
[2m[38;5;1m 309[0m [2m[38;5;2m 424[0m: 
[38;5;1m 310[39m     : [38;5;1m    let (video_frames, [4mvideo_bytes, audio_frames, _audio_bytes[24m) = [4mcollect_frames[24m(&stream, Duration::from_secs(5)).await;[39m
[38;5;1m 311[39m     : [38;5;1m    assert!(video_frames > 0, "expected [4mglxgears [24mvideo frames after reconnect, got {video_frames} (this is the bug that was fixed[4m:[24m [4ma[24m stale[4m queued PING from the abandoned connection misrouting the stream)");[24m[39m
[38;5;1m 312[39m     : [4m[38;5;1m    assert!(video_bytes > 0, "expected nonzero video bytes after reconnect");[24m[39m
[38;5;1m 313[39m     : [4m[38;5;1m    assert!(audio_frames > 0, "expected audio frames after reconnect, got {audio_frames}");[24m[39m
[38;5;1m 314[39m     : [4m[38;5;1m[24m[39m
[38;5;1m 315[39m     : [4m[38;5;1m    // ---- Simulated client mouse[24m [4mmovement[24m [4mover[24m [4mthe[24m [4mreconnected[24m control[4m[24m[39m
[38;5;1m 316[39m     : [4m[38;5;1m    //[24m channel — [4mnot just "the client's send didn't error",[24m [4mbut[24m [4mthat[24m [4mthe[24m[39m
[38;5;1m 317[39m     : [38;5;1m    // [4mevent[24m actually [4mreached[24m [4mour[24m [4minput[24m [4mforwarder[24m server[4m-side[24m. [4m----[24m[39m
[38;5;1m 318[39m     : [38;5;1m    stream[4m[24m[39m
[38;5;1m 319[39m     : [4m[38;5;1m        .send_input([24mClientInputEvent::MouseMoveRelative { delta_x: [4m5[24m, delta_y: 0 })[4m[24m[39m
[38;5;1m 320[39m     : [4m[38;5;1m        [24m.[4mexpect("send_input must succeed on the reconnected control channel")[24m;[39m
[38;5;1m 321[39m     : [38;5;1m    [4mtokio::time::sleep[24m(Duration::[4mfrom_millis[24m([4m300)).await;[24m[39m
[38;5;1m 322[39m     : [4m[38;5;1m[24m[39m
[38;5;1m 323[39m     : [4m[38;5;1m    assert!([24m[39m
[38;5;1m 324[39m     : [4m[38;5;1m        server.stdout_contains("forwarding MouseMoveRelative dx=[24m5[4m dy=0"[24m)[4m,[24m[39m
[38;5;1m 325[39m     : [4m[38;5;1m        "expected the server to log forwarding the simulated mouse move"[24m[39m
[38;5;1m 326[39m     : [4m[38;5;1m    [24m);[39m
     [38;5;2m 425[39m: [38;5;2m    let (video_frames, [4m_[24m) = [4mpoll_frames_tracking_gaps[24m(&stream, [4mtokio::time::sleep([24mDuration::from_secs(5))[4m)[24m.await;[39m
     [38;5;2m 426[39m: [38;5;2m    assert!(video_frames > 0, "expected video frames after reconnect, got {video_frames} (this is the bug that was fixed[4m)");[24m[39m
     [38;5;2m 427[39m: [4m[38;5;2m[24m[39m
     [38;5;2m 428[39m: [4m[38;5;2m    // ---- Input must still work after reconnect (validates the generation-[24m[39m
     [38;5;2m 429[39m: [4m[38;5;2m    //[24m [4mbased[24m stale[4m-peer[24m [4mdisconnect[24m [4mfix[24m [4min[24m [4mthe[24m control channel[4m). Uses[24m[39m
     [38;5;2m 430[39m: [4m[38;5;2m    // wait_for_new_stdout, not wait_for_stdout[24m — [4m"TESTUX[redfog-user-0]: pointer_moved"[24m[39m
     [38;5;2m 431[39m: [4m[38;5;2m    // already appeared once above (pre-reconnect), and stdout is a growing[24m[39m
     [38;5;2m 432[39m: [4m[38;5;2m    // log, so a plain "does it appear" check would[24m [4mtrivially[24m [4mpass[24m [4mwithout[24m[39m
     [38;5;2m 433[39m: [38;5;2m    // [4mthis[24m actually [4mproving[24m [4manything[24m [4mnew[24m [4marrived.[24m[39m
     [38;5;2m 434[39m: [4m[38;5;2m    let pointer_moved_before_reconnect =[24m server.[4mcount_stdout("TESTUX[redfog-user-0]:[24m [4mpointer_moved");[24m[39m
     [38;5;2m 435[39m: [38;5;2m    [4msend_input_retrying(&[24mstream[4m, [24mClientInputEvent::MouseMoveRelative { delta_x: [4m9[24m, delta_y: 0 }).[4mawait[24m;[39m
     [38;5;2m 436[39m: [38;5;2m    [4mserver[24m[39m
     [38;5;2m 437[39m: [4m[38;5;2m        .wait_for_new_stdout[24m([4m"TESTUX[redfog-user-0]: pointer_moved", pointer_moved_before_reconnect, [24mDuration::[4mfrom_secs[24m(5))[4m[24m[39m
     [38;5;2m 438[39m: [4m[38;5;2m        .await[24m;[39m
[2m[38;5;1m 327[0m [2m[38;5;2m 439[0m: }
[38;5;3mModified regular file crates/redfog-server/src/main.rs:[39m
    ...
[2m[38;5;1m  45[0m [2m[38;5;2m  45[0m:         .split_whitespace()
[2m[38;5;1m  46[0m [2m[38;5;2m  46[0m:         .map(str::to_string)
[2m[38;5;1m  47[0m [2m[38;5;2m  47[0m:         .collect();
     [38;5;2m  48[39m: [4m[38;5;2m    let login_app: Vec<String> = std::env::var("REDFOG_LOGIN_APP")[24m[39m
     [38;5;2m  49[39m: [4m[38;5;2m        .unwrap_or_else(|_| "target/release/redfog-login".to_string())[24m[39m
     [38;5;2m  50[39m: [4m[38;5;2m        .split_whitespace()[24m[39m
     [38;5;2m  51[39m: [4m[38;5;2m        .map(str::to_string)[24m[39m
     [38;5;2m  52[39m: [4m[38;5;2m        .collect();[24m[39m
[2m[38;5;1m  48[0m [2m[38;5;2m  53[0m: 
[2m[38;5;1m  49[0m [2m[38;5;2m  54[0m:     let bind_addr: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
[2m[38;5;1m  50[0m [2m[38;5;2m  55[0m:     let hostname = gethostname::gethostname().to_string_lossy().to_string();
    ...
[2m[38;5;1m  57[0m [2m[38;5;2m  62[0m:         bind_addr,
[2m[38;5;1m  58[0m [2m[38;5;2m  63[0m:         video_port,
[2m[38;5;1m  59[0m [2m[38;5;2m  64[0m:         audio_port,
     [38;5;2m  65[39m: [4m[38;5;2m        login_app,[24m[39m
[2m[38;5;1m  60[0m [2m[38;5;2m  66[0m:         user_app,
[2m[38;5;1m  61[0m [2m[38;5;2m  67[0m:         bitrate_kbps: 10_000,
[2m[38;5;1m  62[0m [2m[38;5;2m  68[0m:     });
    ...
[38;5;3mAdded regular file crates/redfog-test-ux/Cargo.toml:[39m
     [38;5;2m   1[39m: [4m[38;5;2m[package][24m[39m
     [38;5;2m   2[39m: [4m[38;5;2mname = "redfog-test-ux"[24m[39m
     [38;5;2m   3[39m: [4m[38;5;2mversion = "0.1.0"[24m[39m
     [38;5;2m   4[39m: [4m[38;5;2medition = "2021"[24m[39m
     [38;5;2m   5[39m: [4m[38;5;2m[24m[39m
     [38;5;2m   6[39m: [4m[38;5;2m[dependencies][24m[39m
     [38;5;2m   7[39m: [4m[38;5;2meframe = "0.27"[24m[39m
[38;5;3mAdded regular file crates/redfog-test-ux/src/main.rs:[39m
     [38;5;2m   1[39m: [4m[38;5;2m//! Minimal test-only stand-in for `redfog-login`/the real desktop session,[24m[39m
     [38;5;2m   2[39m: [4m[38;5;2m//! used by `redfog-moonlight`'s self-contained integration test instead of[24m[39m
     [38;5;2m   3[39m: [4m[38;5;2m//! driving the real login GUI or an external app like `glxgears`.[24m[39m
     [38;5;2m   4[39m: [4m[38;5;2m//![24m[39m
     [38;5;2m   5[39m: [4m[38;5;2m//! Repaints continuously (guarantees a steady stream of frames regardless of[24m[39m
     [38;5;2m   6[39m: [4m[38;5;2m//! input activity — an idle desktop only redraws once a minute, which is[24m[39m
     [38;5;2m   7[39m: [4m[38;5;2m//! useless for testing) and logs every mouse/key event it receives to[24m[39m
     [38;5;2m   8[39m: [4m[38;5;2m//! stdout in a format the test can grep for, giving direct proof that input[24m[39m
     [38;5;2m   9[39m: [4m[38;5;2m//! sent through the control channel actually reached this session — not[24m[39m
     [38;5;2m  10[39m: [4m[38;5;2m//! just that the client's send didn't error. Exits on 'Q', the same[24m[39m
     [38;5;2m  11[39m: [4m[38;5;2m//! `--exit-with-session` trigger `redfog-login` uses on a successful login,[24m[39m
     [38;5;2m  12[39m: [4m[38;5;2m//! so the test can drive the Login->User handoff deterministically instead[24m[39m
     [38;5;2m  13[39m: [4m[38;5;2m//! of racing on a shared global file.[24m[39m
     [38;5;2m  14[39m: [4m[38;5;2m//![24m[39m
     [38;5;2m  15[39m: [4m[38;5;2m//! Runs as both the Login and User stage in the integration test (see[24m[39m
     [38;5;2m  16[39m: [4m[38;5;2m//! `SessionConfig::login_app`/`user_app`); tags its log lines with the[24m[39m
     [38;5;2m  17[39m: [4m[38;5;2m//! Wayland socket name (`redfog-login-0`/`redfog-user-0`, set as[24m[39m
     [38;5;2m  18[39m: [4m[38;5;2m//! `WAYLAND_DISPLAY` by KWin for its session child) so the test can tell[24m[39m
     [38;5;2m  19[39m: [4m[38;5;2m//! which stage produced them. Deliberately not a `--label=` CLI arg:[24m[39m
     [38;5;2m  20[39m: [4m[38;5;2m//! `kwin_wayland --exit-with-session <cmd> -- <args>` does NOT pass[24m[39m
     [38;5;2m  21[39m: [4m[38;5;2m//! `<args>` through to `<cmd>` — confirmed live, `--no-respawn` never[24m[39m
     [38;5;2m  22[39m: [4m[38;5;2m//! reached `plasmashell` either, a pre-existing silent no-op.[24m[39m
     [38;5;2m  23[39m: [4m[38;5;2m[24m[39m
     [38;5;2m  24[39m: [4m[38;5;2muse eframe::egui;[24m[39m
     [38;5;2m  25[39m: [4m[38;5;2m[24m[39m
     [38;5;2m  26[39m: [4m[38;5;2mfn main() -> Result<(), eframe::Error> {[24m[39m
     [38;5;2m  27[39m: [4m[38;5;2m    let label = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "default".to_string());[24m[39m
     [38;5;2m  28[39m: [4m[38;5;2m[24m[39m
     [38;5;2m  29[39m: [4m[38;5;2m    println!("TESTUX[{label}]: started");[24m[39m
     [38;5;2m  30[39m: [4m[38;5;2m[24m[39m
     [38;5;2m  31[39m: [4m[38;5;2m    let options = eframe::NativeOptions {[24m[39m
     [38;5;2m  32[39m: [4m[38;5;2m        viewport: egui::ViewportBuilder::default()[24m[39m
     [38;5;2m  33[39m: [4m[38;5;2m            .with_title("Redfog Test UX")[24m[39m
     [38;5;2m  34[39m: [4m[38;5;2m            .with_inner_size([400.0, 300.0]),[24m[39m
     [38;5;2m  35[39m: [4m[38;5;2m        ..Default::default()[24m[39m
     [38;5;2m  36[39m: [4m[38;5;2m    };[24m[39m
     [38;5;2m  37[39m: [4m[38;5;2m    eframe::run_native([24m[39m
     [38;5;2m  38[39m: [4m[38;5;2m        "Redfog Test UX",[24m[39m
     [38;5;2m  39[39m: [4m[38;5;2m        options,[24m[39m
     [38;5;2m  40[39m: [4m[38;5;2m        Box::new(|_cc| Box::new(TestUxApp { label, last_pos: None })),[24m[39m
     [38;5;2m  41[39m: [4m[38;5;2m    )[24m[39m
     [38;5;2m  42[39m: [4m[38;5;2m}[24m[39m
     [38;5;2m  43[39m: [4m[38;5;2m[24m[39m
     [38;5;2m  44[39m: [4m[38;5;2mstruct TestUxApp {[24m[39m
     [38;5;2m  45[39m: [4m[38;5;2m    label: String,[24m[39m
     [38;5;2m  46[39m: [4m[38;5;2m    last_pos: Option<egui::Pos2>,[24m[39m
     [38;5;2m  47[39m: [4m[38;5;2m}[24m[39m
     [38;5;2m  48[39m: [4m[38;5;2m[24m[39m
     [38;5;2m  49[39m: [4m[38;5;2mimpl eframe::App for TestUxApp {[24m[39m
     [38;5;2m  50[39m: [4m[38;5;2m    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {[24m[39m
     [38;5;2m  51[39m: [4m[38;5;2m        // egui only repaints on input by default. Streaming needs a steady[24m[39m
     [38;5;2m  52[39m: [4m[38;5;2m        // stream of Wayland surface commits regardless of user interaction —[24m[39m
     [38;5;2m  53[39m: [4m[38;5;2m        // KWin's screencast only pushes a PipeWire frame when a client[24m[39m
     [38;5;2m  54[39m: [4m[38;5;2m        // commits a new buffer, so without this the capture pipeline sends[24m[39m
     [38;5;2m  55[39m: [4m[38;5;2m        // one frame and then stalls (see redfog-login's identical comment).[24m[39m
     [38;5;2m  56[39m: [4m[38;5;2m        ctx.request_repaint_after(std::time::Duration::from_millis(33));[24m[39m
     [38;5;2m  57[39m: [4m[38;5;2m[24m[39m
     [38;5;2m  58[39m: [4m[38;5;2m        let label = &self.label;[24m[39m
     [38;5;2m  59[39m: [4m[38;5;2m        ctx.input(|i| {[24m[39m
     [38;5;2m  60[39m: [4m[38;5;2m            for event in &i.events {[24m[39m
     [38;5;2m  61[39m: [4m[38;5;2m                match event {[24m[39m
     [38;5;2m  62[39m: [4m[38;5;2m                    egui::Event::PointerMoved(pos) => {[24m[39m
     [38;5;2m  63[39m: [4m[38;5;2m                        let (dx, dy) = match self.last_pos {[24m[39m
     [38;5;2m  64[39m: [4m[38;5;2m                            Some(last) => (pos.x - last.x, pos.y - last.y),[24m[39m
     [38;5;2m  65[39m: [4m[38;5;2m                            // No prior position to diff against (the very[24m[39m
     [38;5;2m  66[39m: [4m[38;5;2m                            // first move) — still log it, just as an[24m[39m
     [38;5;2m  67[39m: [4m[38;5;2m                            // absolute position with a nominal zero delta,[24m[39m
     [38;5;2m  68[39m: [4m[38;5;2m                            // rather than silently swallowing it.[24m[39m
     [38;5;2m  69[39m: [4m[38;5;2m                            None => (0.0, 0.0),[24m[39m
     [38;5;2m  70[39m: [4m[38;5;2m                        };[24m[39m
     [38;5;2m  71[39m: [4m[38;5;2m                        println!("TESTUX[{label}]: pointer_moved dx={dx} dy={dy} x={} y={}", pos.x, pos.y);[24m[39m
     [38;5;2m  72[39m: [4m[38;5;2m                        self.last_pos = Some(*pos);[24m[39m
     [38;5;2m  73[39m: [4m[38;5;2m                    }[24m[39m
     [38;5;2m  74[39m: [4m[38;5;2m                    egui::Event::PointerButton { button, pressed, .. } => {[24m[39m
     [38;5;2m  75[39m: [4m[38;5;2m                        println!("TESTUX[{label}]: pointer_button button={button:?} pressed={pressed}");[24m[39m
     [38;5;2m  76[39m: [4m[38;5;2m                    }[24m[39m
     [38;5;2m  77[39m: [4m[38;5;2m                    egui::Event::Key { key, pressed: true, repeat: false, .. } => {[24m[39m
     [38;5;2m  78[39m: [4m[38;5;2m                        println!("TESTUX[{label}]: key_pressed key={key:?}");[24m[39m
     [38;5;2m  79[39m: [4m[38;5;2m                        if *key == egui::Key::Q {[24m[39m
     [38;5;2m  80[39m: [4m[38;5;2m                            println!("TESTUX[{label}]: exiting on Q");[24m[39m
     [38;5;2m  81[39m: [4m[38;5;2m                            std::process::exit(0);[24m[39m
     [38;5;2m  82[39m: [4m[38;5;2m                        }[24m[39m
     [38;5;2m  83[39m: [4m[38;5;2m                    }[24m[39m
     [38;5;2m  84[39m: [4m[38;5;2m                    _ => {}[24m[39m
     [38;5;2m  85[39m: [4m[38;5;2m                }[24m[39m
     [38;5;2m  86[39m: [4m[38;5;2m            }[24m[39m
     [38;5;2m  87[39m: [4m[38;5;2m        });[24m[39m
     [38;5;2m  88[39m: [4m[38;5;2m[24m[39m
     [38;5;2m  89[39m: [4m[38;5;2m        egui::CentralPanel::default().show(ctx, |ui| {[24m[39m
     [38;5;2m  90[39m: [4m[38;5;2m            ui.heading(format!("Test UX [{label}]"));[24m[39m
     [38;5;2m  91[39m: [4m[38;5;2m            ui.label("Press Q to exit (simulates login success / session end)");[24m[39m
     [38;5;2m  92[39m: [4m[38;5;2m        });[24m[39m
     [38;5;2m  93[39m: [4m[38;5;2m    }[24m[39m
     [38;5;2m  94[39m: [4m[38;5;2m}[24m[39m
[38;5;3mAdded regular file patches/moonlight-common-rust-rtsp-port-parsing.patch:[39m
     [38;5;2m   1[39m: [4m[38;5;2m--- a/src/stream/proto/mod.rs[24m[39m
     [38;5;2m   2[39m: [4m[38;5;2m+++ b/src/stream/proto/mod.rs[24m[39m
     [38;5;2m   3[39m: [4m[38;5;2m@@ -557,7 +557,7 @@[24m[39m
     [38;5;2m   4[39m: [4m[38;5;2m                             let ip = self.rtsp.target_addr().addr.ip();[24m[39m
     [38;5;2m   5[39m: [4m[38;5;2m                             let addr = SocketAddr::new([24m[39m
     [38;5;2m   6[39m: [4m[38;5;2m                                 ip,[24m[39m
     [38;5;2m   7[39m: [4m[38;5;2m-                                control_setup.port.unwrap_or(DEFAULT_VIDEO_PORT),[24m[39m
     [38;5;2m   8[39m: [4m[38;5;2m+                                control_setup.port.unwrap_or(crate::stream::control::DEFAULT_CONTROL_PORT),[24m[39m
     [38;5;2m   9[39m: [4m[38;5;2m                             );[24m[39m
     [38;5;2m  10[39m: [4m[38;5;2m [24m[39m
     [38;5;2m  11[39m: [4m[38;5;2m                             // Sdp is initialized by now[24m[39m
     [38;5;2m  12[39m: [4m[38;5;2m--- a/src/stream/proto/rtsp/moonlight.rs[24m[39m
     [38;5;2m  13[39m: [4m[38;5;2m+++ b/src/stream/proto/rtsp/moonlight.rs[24m[39m
     [38;5;2m  14[39m: [4m[38;5;2m@@ -185,9 +185,12 @@[24m[39m
     [38;5;2m  15[39m: [4m[38;5;2m         // https://github.com/moonlight-stream/moonlight-common-c/blob/b126e481a195fdc7152d211def17190e3434bcce/src/RtspConnection.c#L705[24m[39m
     [38;5;2m  16[39m: [4m[38;5;2m         let mut port = None;[24m[39m
     [38;5;2m  17[39m: [4m[38;5;2m         if let Some((_, attributes)) = response.options.iter().find(|(key, _)| key == "Transport") {[24m[39m
     [38;5;2m  18[39m: [4m[38;5;2m-            for attribute in attributes.split(':') {[24m[39m
     [38;5;2m  19[39m: [4m[38;5;2m+            for attribute in attributes.split(';') {[24m[39m
     [38;5;2m  20[39m: [4m[38;5;2m                 if let Some(value) = attribute.trim().strip_prefix("server_port=") {[24m[39m
     [38;5;2m  21[39m: [4m[38;5;2m-                    port = match value.parse::<u16>() {[24m[39m
     [38;5;2m  22[39m: [4m[38;5;2m+                    // "server_port=X-Y" is a port range (RTP/RTCP-style);[24m[39m
     [38;5;2m  23[39m: [4m[38;5;2m+                    // GameStream servers only ever use the first number.[24m[39m
     [38;5;2m  24[39m: [4m[38;5;2m+                    let first = value.split('-').next().unwrap_or(value);[24m[39m
     [38;5;2m  25[39m: [4m[38;5;2m+                    port = match first.parse::<u16>() {[24m[39m
     [38;5;2m  26[39m: [4m[38;5;2m                         Ok(value) => Some(value),[24m[39m
     [38;5;2m  27[39m: [4m[38;5;2m                         Err(err) => {[24m[39m
     [38;5;2m  28[39m: [4m[38;5;2m                             warn!(error = ?err, "failed to parse port in a audio/video/control stream setup response");[24m[39m
[38;5;3mAdded executable file scripts/fetch-patched-deps.sh:[39m
     [38;5;2m   1[39m: [4m[38;5;2m#!/usr/bin/env bash[24m[39m
     [38;5;2m   2[39m: [4m[38;5;2m# Fetches and patches dependencies that need local fixes not yet upstream,[24m[39m
     [38;5;2m   3[39m: [4m[38;5;2m# without vendoring GPL source into this repo's git history. Run this once[24m[39m
     [38;5;2m   4[39m: [4m[38;5;2m# before building/testing (idempotent — skips anything already fetched).[24m[39m
     [38;5;2m   5[39m: [4m[38;5;2m#[24m[39m
     [38;5;2m   6[39m: [4m[38;5;2m# See patches/*.patch for what's applied and why.[24m[39m
     [38;5;2m   7[39m: [4m[38;5;2mset -euo pipefail[24m[39m
     [38;5;2m   8[39m: [4m[38;5;2m[24m[39m
     [38;5;2m   9[39m: [4m[38;5;2mrepo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"[24m[39m
     [38;5;2m  10[39m: [4m[38;5;2mvendor_dir="$repo_root/vendor"[24m[39m
     [38;5;2m  11[39m: [4m[38;5;2mmkdir -p "$vendor_dir"[24m[39m
     [38;5;2m  12[39m: [4m[38;5;2m[24m[39m
     [38;5;2m  13[39m: [4m[38;5;2mfetch_and_patch() {[24m[39m
     [38;5;2m  14[39m: [4m[38;5;2m    local name="$1" url="$2" commit="$3" patch="$4"[24m[39m
     [38;5;2m  15[39m: [4m[38;5;2m    local dest="$vendor_dir/$name"[24m[39m
     [38;5;2m  16[39m: [4m[38;5;2m[24m[39m
     [38;5;2m  17[39m: [4m[38;5;2m    if [ -d "$dest" ]; then[24m[39m
     [38;5;2m  18[39m: [4m[38;5;2m        echo "[$name] already present at $dest, skipping (delete it to re-fetch)"[24m[39m
     [38;5;2m  19[39m: [4m[38;5;2m        return[24m[39m
     [38;5;2m  20[39m: [4m[38;5;2m    fi[24m[39m
     [38;5;2m  21[39m: [4m[38;5;2m[24m[39m
     [38;5;2m  22[39m: [4m[38;5;2m    echo "[$name] cloning $url @ $commit..."[24m[39m
     [38;5;2m  23[39m: [4m[38;5;2m    git clone --quiet "$url" "$dest"[24m[39m
     [38;5;2m  24[39m: [4m[38;5;2m    git -C "$dest" checkout --quiet "$commit"[24m[39m
     [38;5;2m  25[39m: [4m[38;5;2m    rm -rf "$dest/.git"[24m[39m
     [38;5;2m  26[39m: [4m[38;5;2m[24m[39m
     [38;5;2m  27[39m: [4m[38;5;2m    echo "[$name] applying $patch..."[24m[39m
     [38;5;2m  28[39m: [4m[38;5;2m    git -C "$dest" apply --quiet "$repo_root/$patch" 2>/dev/null \[24m[39m
     [38;5;2m  29[39m: [4m[38;5;2m        || (cd "$dest" && patch -p1 --quiet < "$repo_root/$patch")[24m[39m
     [38;5;2m  30[39m: [4m[38;5;2m[24m[39m
     [38;5;2m  31[39m: [4m[38;5;2m    echo "[$name] ready."[24m[39m
     [38;5;2m  32[39m: [4m[38;5;2m}[24m[39m
     [38;5;2m  33[39m: [4m[38;5;2m[24m[39m
     [38;5;2m  34[39m: [4m[38;5;2m# GPL-3.0-or-later, dev-only (redfog-moonlight's integration tests/examples;[24m[39m
     [38;5;2m  35[39m: [4m[38;5;2m# never shipped in our own server). Patches two upstream bugs in its RTSP[24m[39m
     [38;5;2m  36[39m: [4m[38;5;2m# Transport-header parsing (wrong delimiter, wrong port fallback constant,[24m[39m
     [38;5;2m  37[39m: [4m[38;5;2m# didn't handle port ranges) that made the ENet control channel unable to[24m[39m
     [38;5;2m  38[39m: [4m[38;5;2m# connect whenever server_port differed from 47998 — confirmed live. A fix[24m[39m
     [38;5;2m  39[39m: [4m[38;5;2m# will be proposed upstream separately.[24m[39m
     [38;5;2m  40[39m: [4m[38;5;2mfetch_and_patch \[24m[39m
     [38;5;2m  41[39m: [4m[38;5;2m    "moonlight-common-rust" \[24m[39m
     [38;5;2m  42[39m: [4m[38;5;2m    "https://github.com/MrCreativ3001/moonlight-common-rust" \[24m[39m
     [38;5;2m  43[39m: [4m[38;5;2m    "06f0d2efbb4e1c769cdd8f8d5a92e00fc192842b" \[24m[39m
     [38;5;2m  44[39m: [4m[38;5;2m    "patches/moonlight-common-rust-rtsp-port-parsing.patch"[24m[39m
