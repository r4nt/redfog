use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use eframe::egui;
use redfog_login_protocol::{LoginRequest, LoginResponse, SessionPreset};

/// The login screen's "Custom" entry sends this literal sentinel instead of
/// a preset name — see `SessionManager::handle_login_report`'s doc comment
/// for what it means (resolve the target user's own
/// `~/.config/redfog/session.toml` via the broker instead of a fixed
/// operator-defined preset).
const USER_CONFIGURED: &str = "user-configured";

/// Sends the entered credentials (and the chosen session name — either one
/// of `presets`' `name`s, or [`USER_CONFIGURED`] — see
/// `SessionManager::handle_login_report`) to `redfog-server` over
/// `REDFOG_LOGIN_SOCKET` and waits for the real PAM-backed verdict (via the
/// broker's `Authenticate` — see design.md's "Privilege separation: broker
/// vs. server"). Without this env var set (e.g. standalone use with no
/// broker configured), falls back to accepting any non-empty username, same
/// as this app's original no-op placeholder behavior — the session choice
/// is simply never reported in that case.
fn authenticate(username: &str, password: &str, session: &str) -> Result<(), String> {
    let Ok(socket_path) = std::env::var("REDFOG_LOGIN_SOCKET") else {
        if username.trim().is_empty() {
            return Err("Username cannot be empty".to_string());
        }
        return Ok(());
    };
    let stream = UnixStream::connect(&socket_path).map_err(|e| format!("failed to reach session server: {e}"))?;
    let mut writer = stream.try_clone().map_err(|e| format!("failed to reach session server: {e}"))?;
    let request = LoginRequest::Authenticate { username: username.to_string(), password: password.to_string(), session: session.to_string() };
    let mut line = serde_json::to_string(&request).expect("protocol types always serialize");
    line.push('\n');
    writer.write_all(line.as_bytes()).map_err(|e| format!("failed to reach session server: {e}"))?;

    let mut response_line = String::new();
    BufReader::new(stream)
        .read_line(&mut response_line)
        .map_err(|e| format!("failed to read response from session server: {e}"))?;
    let response: LoginResponse =
        serde_json::from_str(response_line.trim_end()).map_err(|e| format!("invalid response from session server: {e}"))?;
    match response {
        LoginResponse::Authenticate(result) => result,
    }
}

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Redfog Login")
            .with_inner_size([400.0, 300.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Redfog Login",
        options,
        Box::new(|_cc| Box::new(LoginApp::new())),
    )
}

struct LoginApp {
    username: String,
    password: String,
    /// The operator-configured session list — see
    /// `redfog_login_protocol::load_presets`'s doc comment for why this
    /// reads the same file `redfog-server` did directly, rather than
    /// fetching it over `REDFOG_LOGIN_SOCKET`.
    presets: Vec<SessionPreset>,
    /// Either one of `presets`' `name`s, or [`USER_CONFIGURED`] — see
    /// `authenticate`'s doc comment.
    session_name: String,
    error_msg: Option<String>,
}

impl LoginApp {
    fn new() -> Self {
        let path = std::env::var("REDFOG_SESSIONS_CONFIG").unwrap_or_else(|_| redfog_login_protocol::DEFAULT_SESSIONS_CONFIG_PATH.to_string());
        let presets = match redfog_login_protocol::load_presets(&path) {
            Ok(presets) => presets,
            Err(e) => {
                eprintln!("redfog-login: failed to load {path}: {e} — falling back to built-in defaults");
                redfog_login_protocol::default_presets()
            }
        };
        let session_name = presets.first().map(|p| p.name.clone()).unwrap_or_else(|| USER_CONFIGURED.to_string());
        Self {
            username: String::new(),
            password: String::new(),
            presets,
            session_name,
            error_msg: None,
        }
    }
}

impl eframe::App for LoginApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // egui only repaints on input by default. Streaming needs a steady
        // stream of Wayland surface commits regardless of user interaction —
        // KWin's screencast only pushes a PipeWire frame when a client
        // commits a new buffer, so without this the capture pipeline sends
        // one frame and then stalls forever.
        ctx.request_repaint_after(std::time::Duration::from_millis(33));

        // Support headless/automated testing trigger
        if std::path::Path::new("/tmp/trigger-login").exists() {
            let _ = std::fs::remove_file("/tmp/trigger-login");
            std::process::exit(0);
        }

        // Set a dark, clean look
        let mut visual = egui::Visuals::dark();
        visual.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(20, 20, 25);
        ctx.set_visuals(visual);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.heading(egui::RichText::new("REDFOG").strong().size(32.0).color(egui::Color32::from_rgb(255, 80, 80)));
                ui.label(egui::RichText::new("Enter system credentials to start session").weak());
                ui.add_space(20.0);

                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(30, 30, 35))
                    .rounding(8.0)
                    .inner_margin(20.0)
                    .show(ui, |ui| {
                        ui.set_max_width(300.0);
                        
                        let mut submit = false;

                        egui::Grid::new("login_form")
                            .num_columns(2)
                            .spacing([10.0, 10.0])
                            .show(ui, |ui| {
                                ui.label("Username:");
                                let username_resp = ui.text_edit_singleline(&mut self.username);
                                ui.end_row();

                                ui.label("Password:");
                                let password_resp = ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
                                ui.end_row();

                                // TextEdit doesn't submit on Enter by default —
                                // `lost_focus()` becomes true exactly when Enter
                                // commits the field, the standard egui idiom for
                                // "Enter submits the form".
                                if (username_resp.lost_focus() || password_resp.lost_focus())
                                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                                {
                                    submit = true;
                                }
                            });
                        ui.add_space(10.0);

                        ui.horizontal(|ui| {
                            ui.label("Session:");
                            let selected_text = if self.session_name == USER_CONFIGURED { "Custom" } else { &self.session_name };
                            egui::ComboBox::from_id_source("session_picker").selected_text(selected_text).show_ui(ui, |ui| {
                                for preset in &self.presets {
                                    ui.selectable_value(&mut self.session_name, preset.name.clone(), &preset.name);
                                }
                                ui.selectable_value(&mut self.session_name, USER_CONFIGURED.to_string(), "Custom")
                                    .on_hover_text("Reads ~/.config/redfog/session.toml");
                            });
                        });
                        ui.add_space(15.0);

                        if let Some(ref err) = self.error_msg {
                            ui.colored_label(egui::Color32::LIGHT_RED, err);
                            ui.add_space(8.0);
                        }

                        if ui.add_sized([300.0, 30.0], egui::Button::new("Login")).clicked() {
                            submit = true;
                        }

                        if submit {
                            match authenticate(&self.username, &self.password, &self.session_name) {
                                Ok(()) => std::process::exit(0),
                                Err(e) => {
                                    self.password.clear();
                                    self.error_msg = Some(e);
                                }
                            }
                        }
                    });
            });
        });
    }
}
