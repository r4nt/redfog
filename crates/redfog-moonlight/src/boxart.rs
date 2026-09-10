//! Generates the PNG served from `/appasset` — a simple desktop/monitor
//! icon with this machine's hostname underneath, so a real client's app
//! grid shows something recognizable per-server instead of a blank/
//! generic tile. Same pure-Rust rendering approach `redfog-login::ui`
//! already uses (tiny-skia for shapes, `embedded-graphics`'s built-in
//! bitmap fonts for text — no TTF file to fetch/embed, no GPU) rather than
//! a second, independent rendering stack; not literally shared code since
//! `redfog-login` is a separate binary crate with no reusable library
//! surface for this, but deliberately the same technique.

use embedded_graphics::mono_font::{ascii, MonoTextStyle};
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Text;
use tiny_skia::{Color, Paint, Path, PathBuilder, Pixmap, Rect, Transform};

const WIDTH: u32 = 300;
const HEIGHT: u32 = 400;

const BG: (u8, u8, u8) = (18, 22, 30);
// Light bezel against the dark background — deliberately high-contrast.
// An earlier version used a dark gray bezel (60,66,78) against this same
// background: barely distinguishable at full size, and indistinguishable
// once a real client downscales it for a small grid tile, leaving only the
// bright blue screen fill visible ("just a huge blue rectangle" — the
// literal, live-reported symptom this fixes). The bezel is now the
// dominant, unmistakable shape; the screen is a secondary accent inside it.
const BEZEL: (u8, u8, u8) = (225, 228, 235);
const SCREEN: (u8, u8, u8) = (58, 110, 196);
const TEXT: (u8, u8, u8) = (225, 228, 235);

fn rgb(c: (u8, u8, u8)) -> Color {
    Color::from_rgba8(c.0, c.1, c.2, 255)
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
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    pixmap.fill_path(&rounded_rect_path(rect, radius), &paint, tiny_skia::FillRule::Winding, Transform::identity(), None);
}

/// Same direct-pixel-write approach as `redfog-login::ui::draw_text` — see
/// that function's doc comment for why (skips tiny-skia's general path
/// rasterizer for solid, fully-opaque glyph pixels). This runs once per
/// process at startup, not per-frame, so the performance reasoning that
/// motivated it there doesn't really apply here — kept anyway to reuse the
/// exact same, already-proven `embedded-graphics` `DrawTarget` glue rather
/// than inventing a second way to blit these bitmap fonts onto a `Pixmap`.
fn draw_text_centered(pixmap: &mut Pixmap, text: &str, center_x: i32, y: i32, color: (u8, u8, u8)) {
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

    // Longer hostnames drop to the smaller font before ever truncating —
    // real hostnames are short enough in practice that this is mostly a
    // defensive fallback, not something expected to bite often.
    let big_width = text.chars().count() as i32 * ascii::FONT_10X20.character_size.width as i32;
    let use_big = big_width <= WIDTH as i32 - 20;
    let font = if use_big { &ascii::FONT_10X20 } else { &ascii::FONT_9X15 };
    let char_w = font.character_size.width as i32;

    // Truncate with an ellipsis if it still doesn't fit at the small font
    // — better than clipping mid-glyph off the edge of the canvas.
    let max_chars = ((WIDTH as i32 - 20) / char_w).max(1) as usize;
    let shown: std::borrow::Cow<str> = if text.chars().count() > max_chars {
        let truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{truncated}…").into()
    } else {
        text.into()
    };

    let text_w = shown.chars().count() as i32 * char_w;
    let x = center_x - text_w / 2;

    let style = MonoTextStyle::new(font, Rgb888::new(color.0, color.1, color.2));
    let (width, height) = (pixmap.width() as i32, pixmap.height() as i32);
    let mut target = PixmapTarget { width, height, pixels: pixmap.pixels_mut() };
    let _ = Text::new(&shown, embedded_graphics::geometry::Point::new(x, y + font.baseline as i32), style).draw(&mut target);
}

/// Renders the box-art PNG for `hostname` — a plain monitor shape (bezel +
/// screen + stand + base) with the hostname centered underneath. Called
/// once at server startup (see `main.rs`), not per-request: the result is
/// cached as plain bytes on `PairingServer`, since the hostname never
/// changes for the life of the process.
///
/// Sized to stay legible once a real client shrinks this down for a grid
/// tile, not just at full resolution: the bezel is the largest, highest-
/// contrast shape on the canvas (light against the dark background — see
/// `BEZEL`'s doc comment), the screen sits inside it as a same-shape,
/// smaller accent rather than a separate competing rectangle, and the
/// stand/base are wide enough to survive downscaling instead of the thin
/// slivers an earlier version used.
pub fn generate(hostname: &str) -> Vec<u8> {
    let mut pixmap = Pixmap::new(WIDTH, HEIGHT).expect("non-zero canvas size");
    pixmap.fill(rgb(BG));

    let bezel_w = 240.0;
    let bezel_h = 160.0;
    let bezel_x = (WIDTH as f32 - bezel_w) / 2.0;
    let bezel_y = 60.0;
    let bezel_radius = 14.0;
    fill_rounded_rect(&mut pixmap, Rect::from_xywh(bezel_x, bezel_y, bezel_w, bezel_h).unwrap(), bezel_radius, rgb(BEZEL));

    let inset = 16.0;
    fill_rounded_rect(
        &mut pixmap,
        Rect::from_xywh(bezel_x + inset, bezel_y + inset, bezel_w - inset * 2.0, bezel_h - inset * 2.0).unwrap(),
        bezel_radius * 0.5,
        rgb(SCREEN),
    );

    let stand_w = 28.0;
    let stand_h = 30.0;
    let stand_x = (WIDTH as f32 - stand_w) / 2.0;
    let stand_y = bezel_y + bezel_h - 4.0; // slight overlap so it reads as one solid piece, not two touching rects
    fill_rounded_rect(&mut pixmap, Rect::from_xywh(stand_x, stand_y, stand_w, stand_h).unwrap(), 4.0, rgb(BEZEL));

    let base_w = 130.0;
    let base_h = 14.0;
    let base_x = (WIDTH as f32 - base_w) / 2.0;
    let base_y = stand_y + stand_h;
    fill_rounded_rect(&mut pixmap, Rect::from_xywh(base_x, base_y, base_w, base_h).unwrap(), 6.0, rgb(BEZEL));

    draw_text_centered(&mut pixmap, hostname, WIDTH as i32 / 2, (base_y + 40.0) as i32, TEXT);

    pixmap.encode_png().expect("encoding a freshly-drawn pixmap to PNG never fails")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_a_decodable_png_for_a_short_hostname() {
        let png = generate("curie");
        assert!(!png.is_empty());
        assert_eq!(&png[..8], &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A], "not a valid PNG signature");
    }

    /// Guards the truncation path specifically — a hostname long enough to
    /// overflow even the small font must still produce a valid image, not
    /// panic or silently draw off-canvas.
    #[test]
    fn generates_a_decodable_png_for_a_very_long_hostname() {
        let png = generate("this-is-a-deliberately-very-long-hostname-to-test-truncation");
        assert!(!png.is_empty());
        assert_eq!(&png[..8], &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A], "not a valid PNG signature");
    }
}
