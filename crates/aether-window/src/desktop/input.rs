use aether_kinds::{keycode, mouse_button};
use winit::event::{MouseButton as WinitMouseButton, MouseScrollDelta};
use winit::keyboard::KeyCode;

/// Translate a winit `KeyCode` into the engine's stable named-key u32
/// space (`aether_kinds::keycode`). Returns `None` for any key the
/// engine doesn't name yet — the event then drops at the source rather
/// than leaking winit's unstable discriminants onto the wire. Adding
/// a new key is a paired change: a constant in `aether-kinds::keycode`
/// plus an arm here.
pub(super) fn map_winit_keycode(k: KeyCode) -> Option<u32> {
    Some(match k {
        KeyCode::KeyA => keycode::KEY_A,
        KeyCode::KeyB => keycode::KEY_B,
        KeyCode::KeyC => keycode::KEY_C,
        KeyCode::KeyD => keycode::KEY_D,
        KeyCode::KeyE => keycode::KEY_E,
        KeyCode::KeyF => keycode::KEY_F,
        KeyCode::KeyG => keycode::KEY_G,
        KeyCode::KeyH => keycode::KEY_H,
        KeyCode::KeyI => keycode::KEY_I,
        KeyCode::KeyJ => keycode::KEY_J,
        KeyCode::KeyK => keycode::KEY_K,
        KeyCode::KeyL => keycode::KEY_L,
        KeyCode::KeyM => keycode::KEY_M,
        KeyCode::KeyN => keycode::KEY_N,
        KeyCode::KeyO => keycode::KEY_O,
        KeyCode::KeyP => keycode::KEY_P,
        KeyCode::KeyQ => keycode::KEY_Q,
        KeyCode::KeyR => keycode::KEY_R,
        KeyCode::KeyS => keycode::KEY_S,
        KeyCode::KeyT => keycode::KEY_T,
        KeyCode::KeyU => keycode::KEY_U,
        KeyCode::KeyV => keycode::KEY_V,
        KeyCode::KeyW => keycode::KEY_W,
        KeyCode::KeyX => keycode::KEY_X,
        KeyCode::KeyY => keycode::KEY_Y,
        KeyCode::KeyZ => keycode::KEY_Z,
        KeyCode::Digit0 => keycode::KEY_0,
        KeyCode::Digit1 => keycode::KEY_1,
        KeyCode::Digit2 => keycode::KEY_2,
        KeyCode::Digit3 => keycode::KEY_3,
        KeyCode::Digit4 => keycode::KEY_4,
        KeyCode::Digit5 => keycode::KEY_5,
        KeyCode::Digit6 => keycode::KEY_6,
        KeyCode::Digit7 => keycode::KEY_7,
        KeyCode::Digit8 => keycode::KEY_8,
        KeyCode::Digit9 => keycode::KEY_9,
        KeyCode::Backquote => keycode::KEY_BACKQUOTE,
        KeyCode::Space => keycode::KEY_SPACE,
        KeyCode::Escape => keycode::KEY_ESCAPE,
        KeyCode::Enter => keycode::KEY_ENTER,
        KeyCode::Tab => keycode::KEY_TAB,
        KeyCode::Backspace => keycode::KEY_BACKSPACE,
        KeyCode::ArrowLeft => keycode::KEY_LEFT,
        KeyCode::ArrowRight => keycode::KEY_RIGHT,
        KeyCode::ArrowUp => keycode::KEY_UP,
        KeyCode::ArrowDown => keycode::KEY_DOWN,
        KeyCode::ShiftLeft => keycode::KEY_SHIFT_LEFT,
        KeyCode::ShiftRight => keycode::KEY_SHIFT_RIGHT,
        KeyCode::ControlLeft => keycode::KEY_CTRL_LEFT,
        KeyCode::ControlRight => keycode::KEY_CTRL_RIGHT,
        KeyCode::AltLeft => keycode::KEY_ALT_LEFT,
        KeyCode::AltRight => keycode::KEY_ALT_RIGHT,
        KeyCode::Delete => keycode::KEY_DELETE,
        KeyCode::Home => keycode::KEY_HOME,
        KeyCode::End => keycode::KEY_END,
        KeyCode::PageUp => keycode::KEY_PAGE_UP,
        KeyCode::PageDown => keycode::KEY_PAGE_DOWN,
        _ => return None,
    })
}

/// A committed-text / composition signal lifted out of winit's event
/// types so [`text_input_gate`]'s dedupe logic is unit-testable without
/// a winit event loop — the same pure-helper factoring as
/// [`map_winit_keycode`]. The desktop `KeyboardInput` / `Ime` arms
/// translate their winit events into this before feeding the gate.
pub(super) enum TextSource {
    /// A layout-resolved character run from `KeyEvent.text` on a physical
    /// key press. Suppressed while an IME composition is active.
    KeyText(String),
    /// A `Ime::Preedit`. `active` is `true` for a non-empty preedit
    /// (composition open) and `false` for the synthetic empty preedit
    /// winit sends to clear it.
    Preedit { active: bool },
    /// A `Ime::Commit` — the composed string is final.
    Commit(String),
    /// A `Ime::Disabled` — composition ended without a commit.
    Disabled,
}

/// Composition gate for the `TextInput` stream: update `composing` and
/// return the text to publish (`Some`) or nothing (`None`). The bug it
/// guards is a character delivered twice — once as `KeyEvent.text`, once
/// as `Ime::Commit` — when both fire for one keystroke: while a
/// composition is open, `KeyText` is dropped and the commit is the single
/// source of truth. `Preedit { active }` opens or closes the gate without
/// publishing (the in-flight text rides `ImePreedit` instead); `Disabled`
/// closes it. Winit-free so the dedupe is testable without a winit application.
///
/// `KeyText` also strips control characters before publishing. Winit
/// reports a named key's (Backspace / Enter / Tab / Escape / Delete) text
/// representation as a C0 control character, and those keys already
/// arrive as `Key` scancode edges — publishing the control character too
/// would double-report the keystroke as printable text (e.g. Backspace
/// inserting a glyph on the same frame its scancode edge deletes one).
/// The strip is per-character rather than whole-event: winit documents
/// that `KeyEvent.text` can carry a dead-key char followed by a resolved
/// character, so a run is not guaranteed to be a single glyph, and
/// dropping only the control chars preserves any printable characters
/// riding alongside them.
pub(super) fn text_input_gate(composing: &mut bool, source: TextSource) -> Option<String> {
    match source {
        TextSource::KeyText(text) => (!*composing)
            .then(|| text.chars().filter(|c| !c.is_control()).collect::<String>())
            .filter(|t| !t.is_empty()),
        TextSource::Preedit { active } => {
            *composing = active;
            None
        }
        TextSource::Commit(text) => {
            *composing = false;
            Some(text)
        }
        TextSource::Disabled => {
            *composing = false;
            None
        }
    }
}

/// Convert winit's byte-index IME cursor span to the wire vocabulary.
#[allow(clippy::cast_possible_truncation)]
pub(super) fn ime_cursor_span(cursor: Option<(usize, usize)>) -> (Option<u32>, Option<u32>) {
    cursor.map_or((None, None), |(begin, end)| (Some(begin as u32), Some(end as u32)))
}

/// Scroll lines are normalized to pixels at this rate. A tuning knob,
/// not cap config — the wheel kind carries pixel-space deltas so a
/// consumer never sees the winit line/pixel distinction.
const PIXELS_PER_SCROLL_LINE: f32 = 40.0;

/// Map winit's mouse button to the engine's `mouse_button` constant
/// space. `Other(n)` maps to `None` — the caller pushes no mail,
/// mirroring the unmapped-key contract in `keycode`.
pub(super) fn map_mouse_button(button: WinitMouseButton) -> Option<u32> {
    match button {
        WinitMouseButton::Left => Some(mouse_button::LEFT),
        WinitMouseButton::Right => Some(mouse_button::RIGHT),
        WinitMouseButton::Middle => Some(mouse_button::MIDDLE),
        WinitMouseButton::Back => Some(mouse_button::BACK),
        WinitMouseButton::Forward => Some(mouse_button::FORWARD),
        WinitMouseButton::Other(_) => None,
    }
}

/// Normalize a winit scroll delta to pixel-space `(delta_x, delta_y)`.
/// Line deltas scale by `PIXELS_PER_SCROLL_LINE`; pixel deltas cast
/// `f64 → f32` directly, matching the `CursorMoved` position cast.
pub(super) fn normalize_wheel(delta: MouseScrollDelta) -> (f32, f32) {
    match delta {
        MouseScrollDelta::LineDelta(x, y) => (x * PIXELS_PER_SCROLL_LINE, y * PIXELS_PER_SCROLL_LINE),
        // Realistic scroll deltas stay well inside f32 mantissa.
        #[allow(clippy::cast_possible_truncation)]
        MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;

    // Tripwire: the composition gate must never publish a character twice.
    // A plain keystroke with no IME active publishes its `KeyEvent.text`.
    #[test]
    fn gate_publishes_keytext_when_not_composing() {
        let mut composing = false;
        let out = text_input_gate(&mut composing, TextSource::KeyText("a".to_owned()));
        assert_eq!(out.as_deref(), Some("a"));
        assert!(!composing, "a bare keystroke opens no composition");
    }

    // Tripwire: while an IME composition is open, raw `KeyEvent.text` is
    // suppressed — the commit is the single source of truth, so the
    // committed character is not also emitted from the physical key.
    #[test]
    fn gate_suppresses_keytext_during_composition_and_commit_wins() {
        let mut composing = false;
        // A non-empty preedit opens the composition.
        assert_eq!(text_input_gate(&mut composing, TextSource::Preedit { active: true }), None);
        assert!(composing);
        // Raw key text arriving mid-composition is dropped.
        assert_eq!(text_input_gate(&mut composing, TextSource::KeyText("a".to_owned())), None);
        // The commit publishes the composed text and closes the gate.
        let out = text_input_gate(&mut composing, TextSource::Commit("\u{5416}".to_owned()));
        assert_eq!(out.as_deref(), Some("\u{5416}"));
        assert!(!composing, "commit closes the composition");
    }

    // Tripwire: text must not stay suppressed after a composition ends.
    // The synthetic empty preedit and `Disabled` both clear `composing`,
    // so a subsequent keystroke publishes again.
    #[test]
    fn gate_clears_composing_on_empty_preedit_and_disabled() {
        let mut composing = true;
        assert_eq!(text_input_gate(&mut composing, TextSource::Preedit { active: false }), None);
        assert!(!composing, "empty synthetic preedit clears the gate");

        composing = true;
        assert_eq!(text_input_gate(&mut composing, TextSource::Disabled), None);
        assert!(!composing, "Disabled clears the gate");

        // A keystroke after the clear publishes normally.
        assert_eq!(text_input_gate(&mut composing, TextSource::KeyText("z".to_owned())).as_deref(), Some("z"),);
    }

    // Tripwire: a named key's control-char text representation must never
    // publish as `TextInput` — Backspace's scancode edge is the sole
    // delete signal, so a published `"\u{8}"` would re-insert a glyph on
    // the same frame the edge deletes one.
    #[test]
    fn gate_suppresses_pure_backspace_keytext() {
        let mut composing = false;
        let out = text_input_gate(&mut composing, TextSource::KeyText("\u{8}".to_owned()));
        assert_eq!(out, None);
        assert!(!composing, "a suppressed control char opens no composition");
    }

    // Tripwire: Enter and Tab carry the same C0-control-char shape as
    // Backspace and must be suppressed identically.
    #[test]
    fn gate_suppresses_pure_enter_and_tab_keytext() {
        let mut composing = false;
        assert_eq!(text_input_gate(&mut composing, TextSource::KeyText("\r".to_owned())), None);
        assert_eq!(text_input_gate(&mut composing, TextSource::KeyText("\t".to_owned())), None);
    }

    // Tripwire: a run mixing a printable character with a control
    // character strips only the control char, pinning strip-per-char over
    // whole-event drop (winit can pair a dead-key char with a resolved
    // character in one `KeyEvent.text`).
    #[test]
    fn gate_strips_control_char_from_mixed_keytext() {
        let mut composing = false;
        let out = text_input_gate(&mut composing, TextSource::KeyText("a\u{8}".to_owned()));
        assert_eq!(out.as_deref(), Some("a"));
    }

    #[test]
    fn ime_cursor_span_preserves_present_and_absent_byte_offsets() {
        assert_eq!(ime_cursor_span(Some((2, 5))), (Some(2), Some(5)));
        assert_eq!(ime_cursor_span(None), (None, None));
    }

    #[test]
    fn map_mouse_button_covers_named_buttons() {
        assert_eq!(map_mouse_button(WinitMouseButton::Left), Some(mouse_button::LEFT));
        assert_eq!(map_mouse_button(WinitMouseButton::Right), Some(mouse_button::RIGHT));
        assert_eq!(map_mouse_button(WinitMouseButton::Middle), Some(mouse_button::MIDDLE));
        assert_eq!(map_mouse_button(WinitMouseButton::Back), Some(mouse_button::BACK));
        assert_eq!(map_mouse_button(WinitMouseButton::Forward), Some(mouse_button::FORWARD));
    }

    #[test]
    fn map_mouse_button_other_produces_no_mail() {
        assert_eq!(map_mouse_button(WinitMouseButton::Other(9)), None);
    }

    #[test]
    fn map_winit_keycode_covers_backquote() {
        assert_eq!(map_winit_keycode(KeyCode::Backquote), Some(keycode::KEY_BACKQUOTE));
    }

    // Tripwire: the five text-editing navigation keys must translate to
    // their paired stable `aether_kinds::keycode` constant — the desktop
    // window actor's sole bridge from winit's unstable `KeyCode` discriminants
    // onto the wire.
    #[test]
    fn map_winit_keycode_covers_editing_navigation_keys() {
        let cases = [
            (KeyCode::Delete, keycode::KEY_DELETE),
            (KeyCode::Home, keycode::KEY_HOME),
            (KeyCode::End, keycode::KEY_END),
            (KeyCode::PageUp, keycode::KEY_PAGE_UP),
            (KeyCode::PageDown, keycode::KEY_PAGE_DOWN),
        ];
        for (winit_code, expected) in cases {
            assert_eq!(map_winit_keycode(winit_code), Some(expected));
        }
    }

    #[test]
    fn normalize_wheel_scales_line_delta() {
        let (x, y) = normalize_wheel(MouseScrollDelta::LineDelta(1.0, -2.0));
        assert_eq!(x, PIXELS_PER_SCROLL_LINE);
        assert_eq!(y, -2.0 * PIXELS_PER_SCROLL_LINE);
    }

    #[test]
    fn normalize_wheel_passes_pixel_delta_through() {
        let delta = MouseScrollDelta::PixelDelta(PhysicalPosition::new(12.0, -3.0));
        let (x, y) = normalize_wheel(delta);
        assert_eq!(x, 12.0);
        assert_eq!(y, -3.0);
    }
}
