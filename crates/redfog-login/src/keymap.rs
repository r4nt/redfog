//! Real evdev-keycode -> text translation via `libxkbcommon` — the same
//! library KWin/Sway already use internally for exactly this (see their own
//! `org_kde_kwin_fake_input`/`uinput` input paths), applied explicitly here
//! since there's no compositor left to do it for us implicitly (see
//! `main.rs`'s module doc comment). Replaces an earlier hand-rolled,
//! always-US, shift-only evdev-to-ASCII table.

use xkbcommon::xkb;

pub struct Keymap {
    state: xkb::State,
}

impl Keymap {
    /// `layout` is an XKB layout code (e.g. `"us"`, `"de"`, `"fr"` — see
    /// `xkeyboard-config`'s `rules/evdev.lst` for the full set a given
    /// system actually recognizes). Falls back to `"us"` if `layout` fails
    /// to compile (e.g. a typo'd/unrecognized code) — logged, not fatal,
    /// since a bad layout code shouldn't take down the whole login screen.
    pub fn new(layout: &str) -> Self {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(&context, "", "pc105", layout, "", None, xkb::KEYMAP_COMPILE_NO_FLAGS).unwrap_or_else(|| {
            eprintln!("redfog-login: failed to compile XKB keymap for layout {layout:?}, falling back to \"us\"");
            xkb::Keymap::new_from_names(&context, "", "pc105", "us", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
                .expect("the \"us\" XKB layout should always be available")
        });
        Keymap { state: xkb::State::new(&keymap) }
    }

    /// Applies one key press/release (an evdev keycode, matching what
    /// `LoginInputEvent::KeyboardKey` already carries) to the tracked
    /// modifier state — must be called for *every* key event, including
    /// modifier keys and releases, or XKB's internal Shift/CapsLock/etc.
    /// tracking silently desyncs. Returns the resulting UTF-8 text for a
    /// *press* (empty for releases, and for presses that produce no text at
    /// all — modifier keys, arrows, dead keys awaiting a second keystroke,
    /// ...); callers should not assume this is ever exactly one character.
    pub fn key_event(&mut self, evdev_keycode: u32, pressed: bool) -> String {
        // XKB's keycodes are evdev keycodes plus a fixed historical offset
        // of 8 (inherited from the X11 protocol's 8-bit keycode range
        // reserving the first few values) — see `xkb::Keycode`'s own doc
        // comment for the same convention spelled out.
        let keycode = xkb::Keycode::new(evdev_keycode + 8);
        // Read the resulting text *before* updating state, per
        // `State::update_key`'s own documented convention — reading after
        // would reflect whatever state this press transitions *into*
        // rather than what the press itself should produce (matters for
        // modifier keys specifically: Shift's own "press" has no text, but
        // reading post-update could pick up a stale/wrong value for keys
        // depending on ordering).
        let text = if pressed { self.state.key_get_utf8(keycode) } else { String::new() };
        let direction = if pressed { xkb::KeyDirection::Down } else { xkb::KeyDirection::Up };
        self.state.update_key(keycode, direction);
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_Y: u32 = 21;
    const KEY_A: u32 = 30;
    const KEY_LEFTSHIFT: u32 = 42;

    #[test]
    fn us_layout_maps_evdev_y_to_y() {
        let mut km = Keymap::new("us");
        assert_eq!(km.key_event(KEY_Y, true), "y");
        km.key_event(KEY_Y, false);
    }

    /// The whole point of using real XKB instead of a hardcoded table:
    /// German QWERTZ swaps Y and Z relative to US QWERTY, at the *evdev
    /// keycode* level (same physical key position, different resulting
    /// letter) — confirms layout selection actually changes output, not
    /// just that construction succeeds.
    #[test]
    fn german_layout_maps_evdev_y_to_z() {
        let mut km = Keymap::new("de");
        assert_eq!(km.key_event(KEY_Y, true), "z");
        km.key_event(KEY_Y, false);
    }

    #[test]
    fn shift_is_tracked_across_calls() {
        let mut km = Keymap::new("us");
        assert_eq!(km.key_event(KEY_LEFTSHIFT, true), "");
        assert_eq!(km.key_event(KEY_A, true), "A");
        km.key_event(KEY_A, false);
        km.key_event(KEY_LEFTSHIFT, false);
        // Shift released — back to lowercase.
        assert_eq!(km.key_event(KEY_A, true), "a");
        km.key_event(KEY_A, false);
    }

    #[test]
    fn unknown_layout_falls_back_to_us() {
        let mut km = Keymap::new("this-is-not-a-real-layout-code");
        assert_eq!(km.key_event(KEY_Y, true), "y");
    }
}
