//! Ad-hoc visual check for `ui::render` — renders one frame to a PNG so the
//! design can be reviewed without wiring up the full process/socket
//! plumbing. Not part of the real binary.
//!
//! Usage: cargo run -p redfog-login --example render_preview -- /tmp/preview.png

#[path = "../src/ui.rs"]
mod ui;

fn main() {
    let base_path = std::env::args().nth(1).unwrap_or_else(|| "/tmp/redfog-login-preview.png".to_string());
    let mut state = ui::LoginUiState::new(1920, 1080, vec!["KDE Plasma".to_string(), "Sway".to_string(), "Custom".to_string()]);
    state.username = "klimek".to_string();
    state.password = "hunter2".to_string();
    state.selected_session = 1;
    state.focus = ui::Focus::Password;
    state.cursor_pos = (1120.0, 640.0);
    let (pixmap, _layout) = ui::render(&state);
    pixmap.save_png(&base_path).expect("failed to save preview PNG");
    println!("wrote {base_path}");

    state.session_dropdown_open = true;
    state.cursor_pos = (958.0, 705.0); // hovering the "Custom" row
    let (pixmap, _layout) = ui::render(&state);
    let open_path = base_path.replace(".png", "-open.png");
    pixmap.save_png(&open_path).expect("failed to save preview PNG");
    println!("wrote {open_path}");
    state.session_dropdown_open = false;

    state.keyboard_dropdown_open = true;
    state.selected_layout = 2; // German
    state.cursor_pos = (958.0, 620.0); // hovering a row
    let (pixmap, _layout) = ui::render(&state);
    let kbd_path = base_path.replace(".png", "-keyboard-open.png");
    pixmap.save_png(&kbd_path).expect("failed to save preview PNG");
    println!("wrote {kbd_path}");
    state.keyboard_dropdown_open = false;

    // Resume + Log Out state: the typed username already has a running
    // session.
    state.username_running = Some(true);
    state.cursor_pos = (1120.0, 640.0);
    let (pixmap, _layout) = ui::render(&state);
    let resume_path = base_path.replace(".png", "-resume.png");
    pixmap.save_png(&resume_path).expect("failed to save preview PNG");
    println!("wrote {resume_path}");
}
