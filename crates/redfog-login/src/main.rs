use eframe::egui;

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
        Box::new(|_cc| Box::new(LoginApp::default())),
    )
}

struct LoginApp {
    username: String,
    password: String,
    error_msg: Option<String>,
}

impl Default for LoginApp {
    fn default() -> Self {
        Self {
            username: String::new(),
            password: String::new(),
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
                        
                        egui::Grid::new("login_form")
                            .num_columns(2)
                            .spacing([10.0, 10.0])
                            .show(ui, |ui| {
                                ui.label("Username:");
                                ui.text_edit_singleline(&mut self.username);
                                ui.end_row();

                                ui.label("Password:");
                                ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
                                ui.end_row();
                            });
                        ui.add_space(15.0);

                        if let Some(ref err) = self.error_msg {
                            ui.colored_label(egui::Color32::LIGHT_RED, err);
                            ui.add_space(8.0);
                        }

                        if ui.add_sized([300.0, 30.0], egui::Button::new("Login")).clicked() {
                            if self.username.trim().is_empty() {
                                self.error_msg = Some("Username cannot be empty".to_string());
                            } else {
                                // Fake auth: accept any non-empty username/password
                                std::process::exit(0);
                            }
                        }
                    });
            });
        });
    }
}
