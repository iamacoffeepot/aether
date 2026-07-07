// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `runtime/widget.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The single-line text field (issue 2660).
//!
//! Committed text (`TextInput`, already layout- and IME-resolved) inserts at
//! the caret; the editing keys the substrate emits scancodes for — Backspace,
//! Left, Right, Enter — move the caret or commit; an in-flight IME composition
//! (`ImePreedit`) renders as a trailing underlined span. The caret is a
//! byte-offset into the string and every move lands on a `char` boundary, so a
//! multi-byte character is never split. There is no selection in v1.
//!
//! Home / End are not handled: the substrate's `keycode` space has no
//! `KEY_HOME` / `KEY_END` scancode yet (only Backspace / arrows / Enter /
//! Tab), so those edits await the keycodes landing.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::keycode::{KEY_BACKSPACE, KEY_ENTER, KEY_LEFT, KEY_RIGHT};
use aether_kinds::{ImePreedit, Key, Modifiers, TextInput};

use crate::runtime::widgets::{approx_text_width, push_border, quad, text_origin_y};
use crate::theme::{SetTheme, Theme, WidgetState};
use crate::widgets::{
    Collect, FocusGained, FocusLost, TextCommitted, TextFieldConfig, WidgetDrawItem,
    WidgetDrawList, WidgetFrame,
};

/// A single-line editable string. Holds the text, a byte-offset caret, the
/// character cap, the latest modifiers, and the in-flight IME preedit span.
pub struct TextFieldWidget {
    text: String,
    /// Caret position as a byte offset into `text`, always on a `char`
    /// boundary.
    cursor: usize,
    /// Maximum character count (`0` = uncapped).
    max_chars: u32,
    theme: Theme,
    frame: WidgetFrame,
    focused: bool,
    modifiers: Modifiers,
    /// The current IME composition, drawn underlined after the committed text;
    /// empty when no composition is active.
    preedit: String,
}

impl TextFieldWidget {
    /// Whether inserting `count` more characters would exceed the cap.
    fn would_overflow(&self, count: usize) -> bool {
        self.max_chars > 0 && self.text.chars().count() + count > self.max_chars as usize
    }

    /// Insert `s` at the caret, advancing the caret past it. Rejected whole if
    /// it would exceed the character cap. Owned editing op — unit-tested.
    fn insert(&mut self, s: &str) {
        if s.is_empty() || self.would_overflow(s.chars().count()) {
            return;
        }
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Delete the character before the caret (Backspace), stepping the caret
    /// back one `char`. No-op at the start.
    fn backspace(&mut self) {
        let Some(prev) = self.text[..self.cursor].chars().next_back() else {
            return;
        };
        let start = self.cursor - prev.len_utf8();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Move the caret one `char` left. No-op at the start.
    fn move_left(&mut self) {
        if let Some(prev) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= prev.len_utf8();
        }
    }

    /// Move the caret one `char` right. No-op at the end.
    fn move_right(&mut self) {
        if let Some(next) = self.text[self.cursor..].chars().next() {
            self.cursor += next.len_utf8();
        }
    }

    /// The character count before the caret — the caret's column, for its
    /// approximate pixel placement.
    fn chars_before_cursor(&self) -> usize {
        self.text[..self.cursor].chars().count()
    }
}

/// A text-field widget. Spawned inline by a panel root with a
/// [`TextFieldConfig`]; reports [`TextCommitted`] up when Enter commits.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send it
/// its `TextFieldConfig` again to reset its contents or theme in place.
#[actor(instanced)]
impl WasmActor for TextFieldWidget {
    type Config = TextFieldConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.text_field";

    fn init(config: TextFieldConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let cursor = config.initial.len();
        Ok(TextFieldWidget {
            text: config.initial,
            cursor,
            max_chars: config.max_chars,
            theme: config.theme,
            frame: WidgetFrame {
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 0.0,
            },
            focused: false,
            modifiers: Modifiers::default(),
            preedit: String::new(),
        })
    }

    /// Reset the contents / cap / theme in place from a re-sent config.
    #[handler::single]
    fn on_config(&mut self, _ctx: &mut WasmCtx<'_>, config: TextFieldConfig) {
        self.cursor = config.initial.len();
        self.text = config.initial;
        self.max_chars = config.max_chars;
        self.theme = config.theme;
        self.preedit.clear();
    }

    /// Restyle: adopt the fanned theme.
    #[handler::single]
    fn on_set_theme(&mut self, _ctx: &mut WasmCtx<'_>, set: SetTheme) {
        self.theme = set.theme;
    }

    /// Cache the layout rect the root assigned.
    #[handler::single]
    fn on_frame(&mut self, _ctx: &mut WasmCtx<'_>, frame: WidgetFrame) {
        self.frame = frame;
    }

    /// Take keyboard focus (draw the caret and focus ring).
    #[handler::single]
    fn on_focus_gained(&mut self, _ctx: &mut WasmCtx<'_>, _gained: FocusGained) {
        self.focused = true;
    }

    /// Release keyboard focus.
    #[handler::single]
    fn on_focus_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: FocusLost) {
        self.focused = false;
    }

    /// Insert committed text at the caret. `TextInput` is already resolved
    /// through the layout and IME, so it inserts verbatim.
    #[handler::single]
    fn on_text_input(&mut self, _ctx: &mut WasmCtx<'_>, input: TextInput) {
        self.insert(&input.text);
    }

    /// Editing keys: Backspace deletes, Left / Right move the caret, Enter
    /// commits the current contents up to the panel root.
    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        match key.code {
            KEY_BACKSPACE => self.backspace(),
            KEY_LEFT => self.move_left(),
            KEY_RIGHT => self.move_right(),
            KEY_ENTER => {
                if let Some(parent) = ctx.parent() {
                    parent.send(&TextCommitted {
                        text: self.text.clone(),
                    });
                }
            }
            _ => {}
        }
    }

    /// Cache the latest modifier state (Ctrl / Shift / …) so future
    /// chord-aware edits can consult it.
    #[handler::single]
    fn on_modifiers(&mut self, _ctx: &mut WasmCtx<'_>, modifiers: Modifiers) {
        self.modifiers = modifiers;
    }

    /// Track the in-flight IME composition. Empty text clears it.
    #[handler::single]
    fn on_ime_preedit(&mut self, _ctx: &mut WasmCtx<'_>, preedit: ImePreedit) {
        self.preedit = preedit.text;
    }

    /// Reply the field's local draw: a box, the text plus any preedit, a caret
    /// when focused, and a focus ring.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        let width = self.frame.width;
        let height = self.frame.height;
        let pad = self.theme.pad;
        let size = self.theme.value_size_pixels;
        let text_y = text_origin_y(0.0, height, size);

        let mut items: Vec<WidgetDrawItem> = Vec::new();
        items.push(quad(
            0.0,
            0.0,
            width,
            height,
            self.theme
                .fill(self.theme.surface_raised, WidgetState::Normal),
        ));
        let mut shown = self.text.clone();
        shown.push_str(&self.preedit);
        if !shown.is_empty() {
            items.push(WidgetDrawItem::Text {
                x: pad,
                y: text_y,
                font_id: self.theme.font_id,
                text: shown,
                size_pixels: size,
                color: self.theme.text_primary,
            });
        }
        if !self.preedit.is_empty() {
            // Underline the composition span: a thin bar under the preedit,
            // which trails the committed text.
            let committed_width = approx_text_width(self.text.chars().count(), size);
            let preedit_width = approx_text_width(self.preedit.chars().count(), size);
            items.push(quad(
                pad + committed_width,
                text_y + size,
                preedit_width,
                1.0,
                self.theme.accent,
            ));
        }
        if self.focused {
            let caret_x = pad + approx_text_width(self.chars_before_cursor(), size);
            let caret_height = pad.mul_add(-2.0, height).max(1.0);
            items.push(quad(caret_x, pad, 1.0, caret_height, self.theme.accent));
            push_border(&mut items, width, height, 2.0, self.theme.accent);
        }
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList {
                intrinsic: None,
                items,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(text: &str, max_chars: u32) -> TextFieldWidget {
        TextFieldWidget {
            cursor: text.len(),
            text: String::from(text),
            max_chars,
            theme: Theme::DEFAULT,
            frame: WidgetFrame {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 24.0,
            },
            focused: true,
            modifiers: Modifiers::default(),
            preedit: String::new(),
        }
    }

    #[test]
    fn insert_advances_the_caret_and_respects_the_cap() {
        let mut f = field("ab", 0);
        f.insert("c");
        assert_eq!(f.text, "abc");
        assert_eq!(f.cursor, 3);

        let mut capped = field("ab", 3);
        capped.insert("c");
        assert_eq!(capped.text, "abc", "fills to the cap");
        capped.insert("d");
        assert_eq!(
            capped.text, "abc",
            "a further insert past the cap is dropped"
        );
    }

    #[test]
    fn insert_in_the_middle_lands_at_the_caret() {
        let mut f = field("ac", 0);
        f.cursor = 1;
        f.insert("b");
        assert_eq!(f.text, "abc");
        assert_eq!(f.cursor, 2);
    }

    #[test]
    fn backspace_deletes_a_whole_multibyte_char() {
        // "é" is two bytes; the caret sits after it.
        let mut f = field("é", 0);
        assert_eq!(f.cursor, 2);
        f.backspace();
        assert_eq!(f.text, "");
        assert_eq!(f.cursor, 0);
        // Backspace at the start is a no-op.
        f.backspace();
        assert_eq!(f.cursor, 0);
    }

    #[test]
    fn caret_moves_on_char_boundaries_across_multibyte_text() {
        // "aéb": a(1) é(2) b(1). Caret starts at end (byte 4).
        let mut f = field("aéb", 0);
        assert_eq!(f.cursor, 4);
        f.move_left(); // over 'b'
        assert_eq!(f.cursor, 3);
        f.move_left(); // over 'é' (two bytes)
        assert_eq!(f.cursor, 1);
        f.move_left(); // over 'a'
        assert_eq!(f.cursor, 0);
        f.move_left(); // no-op at start
        assert_eq!(f.cursor, 0);
        f.move_right(); // over 'a'
        assert_eq!(f.cursor, 1);
        f.move_right(); // over 'é'
        assert_eq!(f.cursor, 3);
    }

    #[test]
    fn chars_before_cursor_counts_characters_not_bytes() {
        let mut f = field("aéb", 0);
        f.cursor = 3; // after 'a' and 'é'
        assert_eq!(f.chars_before_cursor(), 2);
    }
}
