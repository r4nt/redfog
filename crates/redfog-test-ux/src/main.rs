//! Minimal test-only stand-in for `redfog-login`/the real desktop session,
//! used by `redfog-moonlight`'s self-contained integration test instead of
//! driving the real login GUI or an external app like `glxgears`.
//!
//! Repaints continuously (guarantees a steady stream of frames regardless of
//! input activity — an idle desktop only redraws once a minute, which is
//! useless for testing) and logs every mouse/key event it receives to
//! stdout in a format the test can grep for, giving direct proof that input
//! sent through the control channel actually reached this session — not
//! just that the client's send didn't error. Exits on 'Q', the same
//! `--exit-with-session` trigger `redfog-login` uses on a successful login,
//! so the test can drive the Login->User handoff deterministically instead
//! of racing on a shared global file.
//!
//! Runs as both the Login and User stage in the integration test (see
//! `SessionConfig::login_app`/`user_app`); tags its log lines with the
//! Wayland socket name (`redfog-login-0`/`redfog-user-0`, set as
//! `WAYLAND_DISPLAY` by KWin for its session child) so the test can tell
//! which stage produced them. Deliberately not a `--label=` CLI arg:
//! `kwin_wayland --exit-with-session <cmd> -- <args>` does NOT pass
//! `<args>` through to `<cmd>` — confirmed live, `--no-respawn` never
//! reached `plasmashell` either, a pre-existing silent no-op.

use eframe::egui;

fn main() -> Result<(), eframe::Error> {
    let label = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "default".to_string());

    println!("TESTUX[{label}]: started");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Redfog Test UX")
            .with_inner_size([400.0, 300.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Redfog Test UX",
        options,
        Box::new(|_cc| Box::new(TestUxApp { label, last_pos: None })),
    )
}

struct TestUxApp {
    label: String,
    last_pos: Option<egui::Pos2>,
}

impl eframe::App for TestUxApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // egui only repaints on input by default. Streaming needs a steady
        // stream of Wayland surface commits regardless of user interaction —
        // KWin's screencast only pushes a PipeWire frame when a client
        // commits a new buffer, so without this the capture pipeline sends
        // one frame and then stalls (see redfog-login's identical comment).
        ctx.request_repaint_after(std::time::Duration::from_millis(33));

        let label = &self.label;
        ctx.input(|i| {
            for event in &i.events {
                match event {
                    egui::Event::PointerMoved(pos) => {
                        let (dx, dy) = match self.last_pos {
                            Some(last) => (pos.x - last.x, pos.y - last.y),
                            // No prior position to diff against (the very
                            // first move) — still log it, just as an
                            // absolute position with a nominal zero delta,
                            // rather than silently swallowing it.
                            None => (0.0, 0.0),
                        };
                        println!("TESTUX[{label}]: pointer_moved dx={dx} dy={dy} x={} y={}", pos.x, pos.y);
                        self.last_pos = Some(*pos);
                    }
                    egui::Event::PointerButton { button, pressed, .. } => {
                        println!("TESTUX[{label}]: pointer_button button={button:?} pressed={pressed}");
                    }
                    egui::Event::Key { key, pressed: true, repeat: false, .. } => {
                        println!("TESTUX[{label}]: key_pressed key={key:?}");
                        if *key == egui::Key::Q {
                            println!("TESTUX[{label}]: exiting on Q");
                            std::process::exit(0);
                        }
                    }
                    _ => {}
                }
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(format!("Test UX [{label}]"));
            ui.label("Press Q to exit (simulates login success / session end)");
        });
    }
}
