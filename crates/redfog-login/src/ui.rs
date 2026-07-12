//! Pure-software rendering for the login screen — no GPU, no windowing
//! system, no compositor at all (see design.md's rationale: `redfog-login`
//! is entirely first-party code, so it can produce its own frames directly
//! instead of needing a real Wayland compositor just to host one small
//! form). `tiny-skia` draws shapes/gradients; `embedded-graphics`'s
//! built-in bitmap fonts draw text — both pure Rust, no GPU, no font file
//! to fetch or embed (unlike a proper TTF renderer, this trades smooth
//! anti-aliased type for zero new binary assets and zero system-font
//! lookup — a deliberate simplicity tradeoff for a small, fixed-layout
//! form, not a general-purpose text renderer).

use std::sync::Mutex;

use embedded_graphics::{
    mono_font::{ascii, MonoTextStyle},
    pixelcolor::Rgb888,
    prelude::*,
    text::Text,
};
use tiny_skia::{Color, LinearGradient, Paint, Path, PathBuilder, Pixmap, Point, Rect, Transform};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Username,
    Password,
    None,
}

#[derive(Clone)]
pub struct LoginUiState {
    pub width: u32,
    pub height: u32,
    pub username: String,
    pub password: String,
    pub focus: Focus,
    /// Display names — the operator-configured presets' `name`s, plus a
    /// trailing literal `"Custom"` entry, matching the old dropdown.
    pub sessions: Vec<String>,
    pub selected_session: usize,
    pub session_dropdown_open: bool,
    pub error_msg: Option<String>,
    pub cursor_pos: (f64, f64),
    /// Toggled roughly every 500ms by the caller to blink the text caret.
    pub caret_blink_on: bool,
}

impl LoginUiState {
    pub fn new(width: u32, height: u32, sessions: Vec<String>) -> Self {
        Self {
            width,
            height,
            username: String::new(),
            password: String::new(),
            focus: Focus::Username,
            sessions,
            selected_session: 0,
            session_dropdown_open: false,
            error_msg: None,
            cursor_pos: (width as f64 / 2.0, height as f64 / 2.0),
            caret_blink_on: true,
        }
    }
}

/// A rectangular hit region computed during layout, reused by both
/// rendering and click hit-testing so the two can never drift apart.
#[derive(Clone)]
pub struct Layout {
    pub username_field: Rect,
    pub password_field: Rect,
    /// The closed control (shows the current selection + a chevron) —
    /// clicking it toggles [`LoginUiState::session_dropdown_open`].
    pub session_box: Rect,
    /// One row per option in the popup list — only meaningful (non-empty)
    /// while the dropdown is open; empty otherwise, so a click can never
    /// hit a row that isn't actually visible.
    pub session_options: Vec<Rect>,
    pub login_button: Rect,
}

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgba8(r, g, b, 255)
}

fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
    Color::from_rgba8(r, g, b, a)
}

const BG_TOP: (u8, u8, u8) = (10, 16, 28);
const BG_BOTTOM: (u8, u8, u8) = (20, 33, 56);
const CARD_FILL: (u8, u8, u8, u8) = (19, 27, 46, 235);
const CARD_BORDER: (u8, u8, u8) = (42, 58, 85);
const ACCENT: (u8, u8, u8) = (76, 141, 255);
const ACCENT_DIM: (u8, u8, u8) = (37, 99, 235);
const TEXT_PRIMARY: (u8, u8, u8) = (230, 233, 239);
const TEXT_SECONDARY: (u8, u8, u8) = (154, 165, 184);
const FIELD_FILL: (u8, u8, u8) = (27, 39, 64);
const FIELD_BORDER: (u8, u8, u8) = (42, 58, 85);
const ERROR: (u8, u8, u8) = (255, 107, 129);

struct BackgroundCache {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

static BACKGROUND_CACHE: Mutex<Option<BackgroundCache>> = Mutex::new(None);

/// Fills `pixmap` (already sized `w`x`h`) with the background gradient,
/// computing it once per distinct `(w, h)` and `copy_from_slice`-ing the
/// cached bytes on every later call instead of re-evaluating the gradient
/// shader every time — confirmed live via a timing benchmark (since
/// removed) that the shader evaluation, not anything else in `render()`,
/// was the overwhelming majority of every frame's cost (roughly 95%, both
/// in debug and release builds) despite the background being pixel-for-
/// pixel identical across every single call for the whole life of the
/// process (`width`/`height` are fixed at `LoginUiState::new` and never
/// change). A plain memory copy is dramatically cheaper than gradient math
/// evaluated per pixel.
fn fill_cached_background(pixmap: &mut Pixmap, w: u32, h: u32) {
    let mut cache = BACKGROUND_CACHE.lock().unwrap();
    let stale = !matches!(&*cache, Some(bg) if bg.width == w && bg.height == h);
    if stale {
        let mut bg_pixmap = Pixmap::new(w, h).expect("non-zero canvas size");
        let shader = LinearGradient::new(
            Point::from_xy(0.0, 0.0),
            Point::from_xy(0.0, h as f32),
            vec![
                tiny_skia::GradientStop::new(0.0, rgb(BG_TOP.0, BG_TOP.1, BG_TOP.2)),
                tiny_skia::GradientStop::new(1.0, rgb(BG_BOTTOM.0, BG_BOTTOM.1, BG_BOTTOM.2)),
            ],
            tiny_skia::SpreadMode::Pad,
            Transform::identity(),
        )
        .expect("gradient with 2 stops always builds");
        let mut paint = Paint::default();
        paint.shader = shader;
        bg_pixmap.fill_rect(Rect::from_xywh(0.0, 0.0, w as f32, h as f32).unwrap(), &paint, Transform::identity(), None);
        *cache = Some(BackgroundCache { width: w, height: h, rgba: bg_pixmap.data().to_vec() });
    }
    pixmap.data_mut().copy_from_slice(&cache.as_ref().unwrap().rgba);
}

fn rounded_rect_path(rect: Rect, radius: f32) -> Path {
    let mut pb = PathBuilder::new();
    let (l, t, r, b) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    let radius = radius.min((r - l) / 2.0).min((b - t) / 2.0);
    pb.move_to(l + radius, t);
    pb.line_to(r - radius, t);
    pb.quad_to(r, t, r, t + radius);
    pb.line_to(r, b - radius);
    pb.quad_to(r, b, r - radius, b);
    pb.line_to(l + radius, b);
    pb.quad_to(l, b, l, b - radius);
    pb.line_to(l, t + radius);
    pb.quad_to(l, t, l + radius, t);
    pb.close();
    pb.finish().expect("rounded rect path always builds")
}

fn fill_rounded_rect(pixmap: &mut Pixmap, rect: Rect, radius: f32, color: Color) {
    let path = rounded_rect_path(rect, radius);
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, Transform::identity(), None);
}

fn stroke_rounded_rect(pixmap: &mut Pixmap, rect: Rect, radius: f32, color: Color, width: f32) {
    let path = rounded_rect_path(rect, radius);
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    let stroke = tiny_skia::Stroke { width, ..Default::default() };
    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

/// Draws `text` at `(x, y)` (top-left of the text's bounding box) in
/// `color` using an `embedded-graphics` bitmap font — see the module doc
/// comment for why this isn't a smooth-typography TTF renderer.
///
/// Writes directly into the pixmap's raw pixel buffer instead of routing
/// each glyph pixel through `Pixmap::fill_rect` (a `Paint`/`Rect`/
/// `Transform` plus a full pass through tiny-skia's general-purpose
/// anti-aliased path rasterizer, just to set one fully-opaque pixel) — this
/// was the dominant cost in `render()` by far, confirmed live via a timing
/// benchmark (`examples/render_bench.rs`, since removed): well over half
/// of every render, on a screen that's mostly text. Safe to skip
/// premultiplication math entirely: every color this module ever draws is
/// fully opaque (`alpha=255`), where premultiplied and straight RGBA are
/// identical.
fn draw_text(pixmap: &mut Pixmap, text: &str, x: i32, y: i32, color: (u8, u8, u8), big: bool) {
    struct PixmapTarget<'a> {
        width: i32,
        height: i32,
        pixels: &'a mut [tiny_skia::PremultipliedColorU8],
    }
    impl embedded_graphics::geometry::OriginDimensions for PixmapTarget<'_> {
        fn size(&self) -> embedded_graphics::geometry::Size {
            embedded_graphics::geometry::Size::new(self.width as u32, self.height as u32)
        }
    }
    impl embedded_graphics::draw_target::DrawTarget for PixmapTarget<'_> {
        type Color = Rgb888;
        type Error = std::convert::Infallible;
        fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
        where
            I: IntoIterator<Item = embedded_graphics::Pixel<Self::Color>>,
        {
            for embedded_graphics::Pixel(point, color) in pixels {
                if point.x < 0 || point.y < 0 || point.x >= self.width || point.y >= self.height {
                    continue;
                }
                let idx = point.y as usize * self.width as usize + point.x as usize;
                self.pixels[idx] = tiny_skia::PremultipliedColorU8::from_rgba(color.r(), color.g(), color.b(), 255).expect("alpha=255 always valid");
            }
            Ok(())
        }
    }

    let font = if big { &ascii::FONT_10X20 } else { &ascii::FONT_9X15 };
    let style = MonoTextStyle::new(font, Rgb888::new(color.0, color.1, color.2));
    let (width, height) = (pixmap.width() as i32, pixmap.height() as i32);
    let mut target = PixmapTarget { width, height, pixels: pixmap.pixels_mut() };
    let _ = Text::new(text, embedded_graphics::geometry::Point::new(x, y + font.baseline as i32), style).draw(&mut target);
}

fn text_width(text: &str, big: bool) -> i32 {
    let font = if big { &ascii::FONT_10X20 } else { &ascii::FONT_9X15 };
    (text.chars().count() as u32 * font.character_size.width) as i32
}

/// Renders the whole frame and returns it alongside the hit-test regions
/// used for click handling (see [`Layout`]) — computed together so
/// rendering and hit-testing can never disagree about where things are.
pub fn render(state: &LoginUiState) -> (Pixmap, Layout) {
    let (w, h) = (state.width, state.height);
    let mut pixmap = Pixmap::new(w.max(1), h.max(1)).expect("non-zero canvas size");
    fill_cached_background(&mut pixmap, w.max(1), h.max(1));

    // Centered card.
    let card_w = (w as f32 * 0.34).clamp(360.0, 560.0);
    let card_h = 430.0f32.min(h as f32 * 0.8);
    let card_x = (w as f32 - card_w) / 2.0;
    let card_y = (h as f32 - card_h) / 2.0;
    let card_rect = Rect::from_xywh(card_x, card_y, card_w, card_h).unwrap();
    fill_rounded_rect(&mut pixmap, card_rect, 20.0, rgba(CARD_FILL.0, CARD_FILL.1, CARD_FILL.2, CARD_FILL.3));
    stroke_rounded_rect(&mut pixmap, card_rect, 20.0, rgb(CARD_BORDER.0, CARD_BORDER.1, CARD_BORDER.2), 1.0);

    let content_x = card_x + 40.0;
    let content_w = card_w - 80.0;
    let mut cy = card_y + 44.0;

    let heading = "REDFOG";
    let heading_w = text_width(heading, true) as f32;
    draw_text(&mut pixmap, heading, (card_x + (card_w - heading_w) / 2.0) as i32, cy as i32, ACCENT, true);
    cy += 34.0;
    let subtitle = "Sign in to start a session";
    let subtitle_w = text_width(subtitle, false) as f32;
    draw_text(&mut pixmap, subtitle, (card_x + (card_w - subtitle_w) / 2.0) as i32, cy as i32, TEXT_SECONDARY, false);
    cy += 46.0;

    let field_h = 44.0;
    draw_text(&mut pixmap, "Username", content_x as i32, cy as i32, TEXT_SECONDARY, false);
    cy += 22.0;
    let username_field = Rect::from_xywh(content_x, cy, content_w, field_h).unwrap();
    draw_field(&mut pixmap, username_field, &state.username, false, state.focus == Focus::Username, state.caret_blink_on);
    cy += field_h + 20.0;

    draw_text(&mut pixmap, "Password", content_x as i32, cy as i32, TEXT_SECONDARY, false);
    cy += 22.0;
    let password_field = Rect::from_xywh(content_x, cy, content_w, field_h).unwrap();
    draw_field(&mut pixmap, password_field, &state.password, true, state.focus == Focus::Password, state.caret_blink_on);
    cy += field_h + 22.0;

    draw_text(&mut pixmap, "Session", content_x as i32, cy as i32, TEXT_SECONDARY, false);
    cy += 22.0;
    let session_box = Rect::from_xywh(content_x, cy, content_w, field_h).unwrap();
    draw_dropdown_box(&mut pixmap, session_box, &state.sessions[state.selected_session], state.session_dropdown_open);
    cy += field_h + 22.0;

    if let Some(err) = &state.error_msg {
        draw_text(&mut pixmap, err, content_x as i32, cy as i32, ERROR, false);
        cy += 26.0;
    }

    let button_h = 46.0;
    let login_button = Rect::from_xywh(content_x, cy, content_w, button_h).unwrap();
    draw_button(&mut pixmap, login_button, "LOG IN");

    // Drawn last (after the button/error text) so it visually floats on
    // top of whatever it happens to overlap below it, matching how a real
    // dropdown popup behaves — its hit-test regions take priority over
    // everything else for the same reason (see Layout::hit_test).
    let session_options = if state.session_dropdown_open {
        draw_dropdown_popup(&mut pixmap, session_box, &state.sessions, state.selected_session, state.cursor_pos)
    } else {
        Vec::new()
    };

    draw_cursor(&mut pixmap, state.cursor_pos);

    (pixmap, Layout { username_field, password_field, session_box, session_options, login_button })
}

fn draw_field(pixmap: &mut Pixmap, rect: Rect, value: &str, masked: bool, focused: bool, caret_blink_on: bool) {
    fill_rounded_rect(pixmap, rect, 10.0, rgb(FIELD_FILL.0, FIELD_FILL.1, FIELD_FILL.2));
    let border = if focused { ACCENT } else { FIELD_BORDER };
    stroke_rounded_rect(pixmap, rect, 10.0, rgb(border.0, border.1, border.2), if focused { 2.0 } else { 1.0 });

    let display: String = if masked { "\u{2022}".repeat(value.chars().count()) } else { value.to_string() };
    // embedded-graphics's ASCII fonts don't include U+2022 — a plain
    // asterisk renders identically for a masked password field and stays
    // within the embedded ASCII font's glyph set.
    let display = if masked { "*".repeat(value.chars().count()) } else { display };
    let text_y = rect.top() as i32 + (rect.height() as i32 - 15) / 2;
    draw_text(pixmap, &display, rect.left() as i32 + 14, text_y, TEXT_PRIMARY, false);

    if focused && caret_blink_on {
        let caret_x = rect.left() + 14.0 + text_width(&display, false) as f32 + 2.0;
        let mut paint = Paint::default();
        paint.set_color(rgb(TEXT_PRIMARY.0, TEXT_PRIMARY.1, TEXT_PRIMARY.2));
        pixmap.fill_rect(Rect::from_xywh(caret_x, rect.top() + 10.0, 2.0, rect.height() - 20.0).unwrap(), &paint, Transform::identity(), None);
    }
}

const DROPDOWN_ROW_H: f32 = 40.0;

/// The closed control: a field-styled box showing the current selection
/// and a chevron that flips direction to hint whether clicking it opens
/// or closes the popup.
fn draw_dropdown_box(pixmap: &mut Pixmap, rect: Rect, selected_name: &str, open: bool) {
    fill_rounded_rect(pixmap, rect, 10.0, rgb(FIELD_FILL.0, FIELD_FILL.1, FIELD_FILL.2));
    let border = if open { ACCENT } else { FIELD_BORDER };
    stroke_rounded_rect(pixmap, rect, 10.0, rgb(border.0, border.1, border.2), if open { 2.0 } else { 1.0 });

    let text_y = rect.top() as i32 + (rect.height() as i32 - 15) / 2;
    draw_text(pixmap, selected_name, rect.left() as i32 + 14, text_y, TEXT_PRIMARY, false);

    // Chevron: a small filled triangle, pointing down when closed (hints
    // "click to open") and up when open (hints "click to close").
    let cx = rect.right() - 24.0;
    let cy = rect.top() + rect.height() / 2.0;
    let mut pb = PathBuilder::new();
    if open {
        pb.move_to(cx - 6.0, cy + 3.0);
        pb.line_to(cx + 6.0, cy + 3.0);
        pb.line_to(cx, cy - 4.0);
    } else {
        pb.move_to(cx - 6.0, cy - 3.0);
        pb.line_to(cx + 6.0, cy - 3.0);
        pb.line_to(cx, cy + 4.0);
    }
    pb.close();
    let path = pb.finish().expect("chevron path always builds");
    let mut paint = Paint::default();
    paint.set_color(rgb(TEXT_SECONDARY.0, TEXT_SECONDARY.1, TEXT_SECONDARY.2));
    paint.anti_alias = true;
    pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, Transform::identity(), None);
}

/// The popup list of options, floating directly below `anchor` — drawn
/// (and hit-tested, see [`Layout::hit_test`]) on top of everything else,
/// same as any real dropdown menu. Highlights whichever row `cursor_pos`
/// currently sits over, in addition to marking the actual selection.
fn draw_dropdown_popup(pixmap: &mut Pixmap, anchor: Rect, options: &[String], selected: usize, cursor_pos: (f64, f64)) -> Vec<Rect> {
    let gap = 6.0;
    let popup_h = DROPDOWN_ROW_H * options.len().max(1) as f32;
    let popup_rect = Rect::from_xywh(anchor.left(), anchor.bottom() + gap, anchor.width(), popup_h).unwrap();

    // Cheap flat drop shadow: tiny-skia has no blur, so a slightly larger,
    // darker rect offset behind the panel reads as "elevated" at this size.
    let shadow_rect = Rect::from_xywh(popup_rect.left(), popup_rect.top() + 3.0, popup_rect.width(), popup_rect.height()).unwrap();
    fill_rounded_rect(pixmap, shadow_rect, 10.0, rgba(0, 0, 0, 90));

    fill_rounded_rect(pixmap, popup_rect, 10.0, rgb(FIELD_FILL.0, FIELD_FILL.1, FIELD_FILL.2));
    stroke_rounded_rect(pixmap, popup_rect, 10.0, rgb(ACCENT.0, ACCENT.1, ACCENT.2), 1.0);

    let mut rows = Vec::with_capacity(options.len());
    for (i, name) in options.iter().enumerate() {
        let row_rect = Rect::from_xywh(popup_rect.left(), popup_rect.top() + DROPDOWN_ROW_H * i as f32, popup_rect.width(), DROPDOWN_ROW_H).unwrap();
        let hovered = rect_contains(&row_rect, cursor_pos.0 as f32, cursor_pos.1 as f32);
        if i == selected || hovered {
            let inset = Rect::from_xywh(row_rect.left() + 4.0, row_rect.top() + 3.0, row_rect.width() - 8.0, row_rect.height() - 6.0).unwrap();
            let fill = if i == selected { ACCENT_DIM } else { FIELD_BORDER };
            fill_rounded_rect(pixmap, inset, 7.0, rgb(fill.0, fill.1, fill.2));
        }
        let text_color = if i == selected { (255, 255, 255) } else { TEXT_PRIMARY };
        let text_y = row_rect.top() as i32 + (row_rect.height() as i32 - 15) / 2;
        draw_text(pixmap, name, row_rect.left() as i32 + 14, text_y, text_color, false);
        rows.push(row_rect);
    }
    rows
}

fn draw_button(pixmap: &mut Pixmap, rect: Rect, label: &str) {
    let shader = LinearGradient::new(
        Point::from_xy(rect.left(), rect.top()),
        Point::from_xy(rect.left(), rect.bottom()),
        vec![
            tiny_skia::GradientStop::new(0.0, rgb(ACCENT.0, ACCENT.1, ACCENT.2)),
            tiny_skia::GradientStop::new(1.0, rgb(ACCENT_DIM.0, ACCENT_DIM.1, ACCENT_DIM.2)),
        ],
        tiny_skia::SpreadMode::Pad,
        Transform::identity(),
    )
    .expect("gradient with 2 stops always builds");
    let path = rounded_rect_path(rect, 10.0);
    let mut paint = Paint::default();
    paint.shader = shader;
    paint.anti_alias = true;
    pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, Transform::identity(), None);

    let label_w = text_width(label, false);
    let text_x = rect.left() as i32 + (rect.width() as i32 - label_w) / 2;
    let text_y = rect.top() as i32 + (rect.height() as i32 - 15) / 2;
    draw_text(pixmap, label, text_x, text_y, (255, 255, 255), false);
}

/// A simple filled arrow, since there's no compositor left to draw a real
/// cursor for us — the previous KWin/gst-wayland-display backends drew
/// this for free as part of whatever desktop environment was rendering;
/// here it's just another shape we draw ourselves each frame.
fn draw_cursor(pixmap: &mut Pixmap, pos: (f64, f64)) {
    let (x, y) = (pos.0 as f32, pos.1 as f32);
    let mut pb = PathBuilder::new();
    pb.move_to(x, y);
    pb.line_to(x, y + 18.0);
    pb.line_to(x + 4.5, y + 14.0);
    pb.line_to(x + 7.5, y + 20.5);
    pb.line_to(x + 10.0, y + 19.0);
    pb.line_to(x + 7.0, y + 12.7);
    pb.line_to(x + 12.5, y + 12.5);
    pb.close();
    let path = pb.finish().expect("cursor arrow path always builds");
    let mut outline = Paint::default();
    outline.set_color(Color::BLACK);
    outline.anti_alias = true;
    let stroke = tiny_skia::Stroke { width: 1.5, ..Default::default() };
    pixmap.stroke_path(&path, &outline, &stroke, Transform::identity(), None);
    let mut fill = Paint::default();
    fill.set_color(Color::WHITE);
    fill.anti_alias = true;
    pixmap.fill_path(&path, &fill, tiny_skia::FillRule::Winding, Transform::identity(), None);
}

fn rect_contains(rect: &Rect, x: f32, y: f32) -> bool {
    x >= rect.left() && x < rect.right() && y >= rect.top() && y < rect.bottom()
}

impl Layout {
    /// Which widget (if any) contains `(x, y)` — used to resolve a mouse
    /// click into a focus change / segment selection / button press.
    pub fn hit_test(&self, x: f64, y: f64) -> Hit {
        let (x, y) = (x as f32, y as f32);
        // The popup (if open) is drawn on top of everything else, so it
        // must win hit-testing too — a click that lands where the popup
        // visually covers the button/fields below it must hit the popup,
        // not whatever's underneath.
        for (i, row) in self.session_options.iter().enumerate() {
            if rect_contains(row, x, y) {
                return Hit::SessionOption(i);
            }
        }
        if rect_contains(&self.session_box, x, y) {
            return Hit::SessionToggle;
        }
        if rect_contains(&self.username_field, x, y) {
            return Hit::Username;
        }
        if rect_contains(&self.password_field, x, y) {
            return Hit::Password;
        }
        if rect_contains(&self.login_button, x, y) {
            return Hit::LoginButton;
        }
        Hit::None
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Hit {
    Username,
    Password,
    /// Click on the closed control — toggles `session_dropdown_open`.
    SessionToggle,
    /// Click on an open row — selects it and closes the popup.
    SessionOption(usize),
    LoginButton,
    /// Also what a click outside the popup means while it's open — the
    /// caller should treat this as "close the dropdown without changing
    /// the selection" in that case, matching how real dropdown menus
    /// close on any click-away.
    None,
}
