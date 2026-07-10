//! Reusable single-line plain-text editing state (issue 2924).
//!
//! [`TextEditState`] owns a committed `String`, a selection expressed as an
//! `anchor` and an active `caret` (both byte offsets), and an in-flight IME
//! composition (`preedit` plus an optional [`TextSpan`] cursor span). Every
//! stored offset is normalized to a UTF-8 `char` boundary, so a multi-byte
//! character is never split by a caret, a selection edge, or a preedit span.
//!
//! The state exposes semantic edits only — replace the selection, insert under
//! an [`EditPolicy`], delete backward or forward, move by character or document
//! edge with optional selection extension, select all, and update or clear the
//! composition. It interprets no physical key codes, no focus or availability,
//! no clipboard, and no multiline layout; a widget maps its input mail onto
//! these operations and owns those concerns itself.
//!
//! [`SingleLineLayout`] turns a [`CachedFontMetrics`] measurement into an
//! ordered byte-boundary / x-position table in one linear pass, so a caret's
//! pixel x and a pointer's nearest caret boundary are exact lookups rather than
//! a repeated prefix rescan.

use alloc::string::String;
use alloc::vec::Vec;

use aether_kinds::CachedFontMetrics;

/// How an insert is filtered and capped before it lands. A single-line control
/// drops line breaks; `max_chars` bounds the committed character count (`0` =
/// uncapped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditPolicy {
    /// Strip `\n` / `\r` from inserted text, so a paste or a stray newline
    /// cannot break the single-line invariant.
    pub single_line: bool,
    /// Maximum committed character count; `0` leaves the field uncapped. A whole
    /// insert that would exceed the cap is rejected rather than truncated.
    pub max_chars: u32,
}

/// A half-open byte span in edited or preedit text. [`TextEditState`] normalizes
/// spans before storing them; named fields keep the unit and endpoint ordering
/// explicit at the public API.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextSpan {
    /// Inclusive byte offset where the span begins.
    pub start_byte: usize,
    /// Exclusive byte offset where the span ends.
    pub end_byte: usize,
}

impl TextSpan {
    /// Construct a span from byte endpoints. Consumers that pass it into
    /// [`TextEditState`] may use arbitrary endpoints; the state normalizes them
    /// before storing the span.
    #[must_use]
    pub const fn new(start_byte: usize, end_byte: usize) -> Self {
        Self { start_byte, end_byte }
    }

    /// Whether both endpoints name the same caret position.
    #[must_use]
    pub const fn is_collapsed(self) -> bool {
        self.start_byte == self.end_byte
    }
}

impl Default for EditPolicy {
    fn default() -> Self {
        Self { single_line: true, max_chars: 0 }
    }
}

/// A single-line plain-text editing core: committed text, a selection, and an
/// IME composition, all on `char` boundaries.
#[derive(Debug, Clone, Default)]
pub struct TextEditState {
    text: String,
    /// The fixed end of the selection — where an extend-movement pivots from.
    anchor: usize,
    /// The moving end of the selection — where the caret is drawn and where a
    /// collapsed selection sits.
    caret: usize,
    /// The in-flight IME composition, empty when none is active.
    preedit: String,
    /// The IME's reported cursor/selection span as byte offsets into `preedit`,
    /// `None` when the IME gives no span.
    preedit_cursor: Option<TextSpan>,
}

/// Floor `byte` to a `char` boundary of `text`, clamping to `text.len()` first.
fn floor_boundary(text: &str, mut byte: usize) -> usize {
    if byte >= text.len() {
        return text.len();
    }
    while byte > 0 && !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

impl TextEditState {
    /// A fresh editor over `initial`, caret and anchor collapsed at the end.
    #[must_use]
    pub fn new(initial: String) -> Self {
        let end = initial.len();
        Self { text: initial, anchor: end, caret: end, preedit: String::new(), preedit_cursor: None }
    }

    /// The committed text (never includes the in-flight preedit).
    #[must_use]
    pub fn value(&self) -> &str {
        &self.text
    }

    /// The active caret as a byte offset into [`value`](Self::value).
    #[must_use]
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// The selection as a sorted byte span; both endpoints match when the
    /// selection is collapsed to a caret.
    #[must_use]
    pub fn selection(&self) -> TextSpan {
        if self.anchor <= self.caret {
            TextSpan::new(self.anchor, self.caret)
        } else {
            TextSpan::new(self.caret, self.anchor)
        }
    }

    /// Whether any text is selected (the anchor and caret differ).
    #[must_use]
    pub fn has_selection(&self) -> bool {
        self.anchor != self.caret
    }

    /// The in-flight IME composition, empty when none is active.
    #[must_use]
    pub fn preedit(&self) -> &str {
        &self.preedit
    }

    /// The IME cursor/selection span as byte offsets into
    /// [`preedit`](Self::preedit), `None` when the IME reported none.
    #[must_use]
    pub fn preedit_cursor(&self) -> Option<TextSpan> {
        self.preedit_cursor
    }

    /// The filtered form of `s` under `policy` — line breaks dropped for a
    /// single-line control. Returns `s` unchanged when nothing is filtered.
    fn filtered(policy: EditPolicy, s: &str) -> String {
        if policy.single_line && s.contains(['\n', '\r']) {
            s.chars().filter(|c| *c != '\n' && *c != '\r').collect()
        } else {
            String::from(s)
        }
    }

    /// Replace the active selection with `s` under `policy`, moving the caret
    /// past the inserted text. The whole insert is rejected (returns `false`)
    /// when its filtered form is empty or would push the committed character
    /// count past the cap.
    pub fn insert(&mut self, s: &str, policy: EditPolicy) -> bool {
        let filtered = Self::filtered(policy, s);
        if filtered.is_empty() {
            return false;
        }
        let selection = self.selection();
        let start = selection.start_byte;
        let end = selection.end_byte;
        if policy.max_chars > 0 {
            let removed = self.text[start..end].chars().count();
            let added = filtered.chars().count();
            let after = self.text.chars().count() - removed + added;
            if after > policy.max_chars as usize {
                return false;
            }
        }
        self.text.replace_range(start..end, &filtered);
        let point = start + filtered.len();
        self.anchor = point;
        self.caret = point;
        true
    }

    /// Delete the selection if any, else the character before the caret
    /// (Backspace). A no-op at the document start with no selection.
    pub fn delete_backward(&mut self) {
        if self.has_selection() {
            self.delete_selection();
            return;
        }
        let Some(prev) = self.text[..self.caret].chars().next_back() else {
            return;
        };
        let start = self.caret - prev.len_utf8();
        self.text.replace_range(start..self.caret, "");
        self.anchor = start;
        self.caret = start;
    }

    /// Delete the selection if any, else the character after the caret
    /// (Delete). A no-op at the document end with no selection.
    pub fn delete_forward(&mut self) {
        if self.has_selection() {
            self.delete_selection();
            return;
        }
        let Some(next) = self.text[self.caret..].chars().next() else {
            return;
        };
        let end = self.caret + next.len_utf8();
        self.text.replace_range(self.caret..end, "");
    }

    fn delete_selection(&mut self) {
        let selection = self.selection();
        let start = selection.start_byte;
        let end = selection.end_byte;
        self.text.replace_range(start..end, "");
        self.anchor = start;
        self.caret = start;
    }

    /// Move the caret one character left. `extend` keeps the anchor fixed
    /// (growing the selection); otherwise a non-collapsed selection collapses to
    /// its left edge and a collapsed caret steps one character.
    pub fn move_left(&mut self, extend: bool) {
        if !extend && self.has_selection() {
            let start = self.selection().start_byte;
            self.anchor = start;
            self.caret = start;
            return;
        }
        if let Some(prev) = self.text[..self.caret].chars().next_back() {
            self.caret -= prev.len_utf8();
        }
        if !extend {
            self.anchor = self.caret;
        }
    }

    /// Move the caret one character right. `extend` keeps the anchor fixed;
    /// otherwise a non-collapsed selection collapses to its right edge and a
    /// collapsed caret steps one character.
    pub fn move_right(&mut self, extend: bool) {
        if !extend && self.has_selection() {
            let end = self.selection().end_byte;
            self.anchor = end;
            self.caret = end;
            return;
        }
        if let Some(next) = self.text[self.caret..].chars().next() {
            self.caret += next.len_utf8();
        }
        if !extend {
            self.anchor = self.caret;
        }
    }

    /// Move the caret to the document start. `extend` keeps the anchor fixed.
    pub fn move_to_start(&mut self, extend: bool) {
        self.caret = 0;
        if !extend {
            self.anchor = 0;
        }
    }

    /// Move the caret to the document end. `extend` keeps the anchor fixed.
    pub fn move_to_end(&mut self, extend: bool) {
        self.caret = self.text.len();
        if !extend {
            self.anchor = self.caret;
        }
    }

    /// Select the whole document, anchor at the start and caret at the end.
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    /// Collapse the selection to a caret at `byte` (a pointer click), normalized
    /// to a `char` boundary.
    pub fn place_caret(&mut self, byte: usize) {
        let point = floor_boundary(&self.text, byte);
        self.anchor = point;
        self.caret = point;
    }

    /// Extend the selection so the active caret lands at `byte` (a pointer
    /// drag), normalized to a `char` boundary; the anchor stays put.
    pub fn extend_to(&mut self, byte: usize) {
        self.caret = floor_boundary(&self.text, byte);
    }

    /// Set the IME composition to `text` with an optional byte-offset cursor
    /// span. Empty `text` clears the composition. The span is normalized to
    /// `char` boundaries of `text` and ordered, and dropped entirely when it
    /// falls outside the composition.
    pub fn set_composition(&mut self, text: String, cursor: Option<TextSpan>) {
        if text.is_empty() {
            self.clear_composition();
            return;
        }
        let cursor = cursor.and_then(|cursor| {
            if cursor.start_byte > text.len() || cursor.end_byte > text.len() {
                return None;
            }
            let start_byte = floor_boundary(&text, cursor.start_byte);
            let end_byte = floor_boundary(&text, cursor.end_byte);
            Some(if start_byte <= end_byte {
                TextSpan::new(start_byte, end_byte)
            } else {
                TextSpan::new(end_byte, start_byte)
            })
        });
        self.preedit = text;
        self.preedit_cursor = cursor;
    }

    /// Clear any in-flight IME composition.
    pub fn clear_composition(&mut self) {
        self.preedit.clear();
        self.preedit_cursor = None;
    }
}

/// One caret stop in a laid-out single line: a `char`-boundary byte offset and
/// the pixel x the caret sits at there.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CaretStop {
    byte: usize,
    x: f32,
}

/// An ordered byte-boundary / x-position table for one line of text at a fixed
/// draw size, built once from a [`CachedFontMetrics`]. Caret lookup and pointer
/// hit-testing are exact table reads, so neither rescans the string.
#[derive(Debug, Clone)]
pub struct SingleLineLayout {
    stops: Vec<CaretStop>,
}

impl SingleLineLayout {
    /// Build the table for `text` at `size_pixels`, accumulating each glyph's
    /// measured advance in a single left-to-right pass.
    #[must_use]
    pub fn build(text: &str, metrics: &CachedFontMetrics, size_pixels: f32) -> Self {
        let mut stops = Vec::with_capacity(text.chars().count() + 1);
        let mut x = 0.0;
        let mut byte = 0;
        stops.push(CaretStop { byte, x });
        let mut buf = [0u8; 4];
        for ch in text.chars() {
            let glyph = ch.encode_utf8(&mut buf);
            x += metrics.measure(glyph, size_pixels);
            byte += ch.len_utf8();
            stops.push(CaretStop { byte, x });
        }
        Self { stops }
    }

    /// The pixel x of the caret at `byte`, or the line's full extent when `byte`
    /// is not a recorded boundary (caller-normalized offsets always match a
    /// stop).
    #[must_use]
    pub fn caret_x(&self, byte: usize) -> f32 {
        self.stops.iter().find(|stop| stop.byte == byte).map_or_else(|| self.width(), |stop| stop.x)
    }

    /// The `char`-boundary byte offset whose caret sits nearest `x` — the stop
    /// on the far side of every segment midpoint `x` has passed. Ties (exactly
    /// at a midpoint) round toward the later boundary.
    #[must_use]
    pub fn hit_test(&self, x: f32) -> usize {
        let mut byte = self.stops.first().map_or(0, |stop| stop.byte);
        for pair in self.stops.windows(2) {
            let midpoint = (pair[0].x + pair[1].x) * 0.5;
            if x >= midpoint {
                byte = pair[1].byte;
            } else {
                break;
            }
        }
        byte
    }

    /// The line's full pixel extent (the x of its last caret stop).
    #[must_use]
    pub fn width(&self) -> f32 {
        self.stops.last().map_or(0.0, |stop| stop.x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_kinds::{FontMetrics, GlyphAdvance};

    fn state(text: &str) -> TextEditState {
        TextEditState::new(String::from(text))
    }

    fn uncapped() -> EditPolicy {
        EditPolicy { single_line: true, max_chars: 0 }
    }

    #[test]
    fn new_collapses_caret_at_the_end() {
        let s = state("abc");
        assert_eq!(s.caret(), 3);
        assert_eq!(s.selection(), TextSpan::new(3, 3));
        assert!(!s.has_selection());
    }

    #[test]
    fn empty_and_boundary_edits_are_no_ops() {
        let mut s = state("");
        s.delete_backward();
        s.delete_forward();
        s.move_left(false);
        s.move_right(false);
        assert_eq!(s.value(), "");
        assert_eq!(s.caret(), 0);

        // Insert of an all-filtered (empty after filtering) string is rejected.
        assert!(!s.insert("\n\r", uncapped()));
        assert_eq!(s.value(), "");
    }

    #[test]
    fn ascii_insert_advances_the_caret() {
        let mut s = state("ac");
        s.place_caret(1);
        assert!(s.insert("b", uncapped()));
        assert_eq!(s.value(), "abc");
        assert_eq!(s.caret(), 2);
    }

    #[test]
    fn multibyte_insert_delete_stay_on_char_boundaries() {
        let mut s = state("aéb"); // a(1) é(2) b(1)
        assert_eq!(s.caret(), 4);
        s.delete_backward(); // over 'b'
        assert_eq!(s.value(), "aé");
        s.delete_backward(); // over 'é' (two bytes)
        assert_eq!(s.value(), "a");
        assert_eq!(s.caret(), 1);
        // Insert a multibyte char in the middle.
        s.place_caret(0);
        assert!(s.insert("ü", uncapped()));
        assert_eq!(s.value(), "üa");
        assert_eq!(s.caret(), 2);
    }

    #[test]
    fn delete_forward_removes_a_whole_multibyte_char() {
        let mut s = state("é!");
        s.move_to_start(false);
        s.delete_forward();
        assert_eq!(s.value(), "!");
        assert_eq!(s.caret(), 0);
        s.delete_forward();
        assert_eq!(s.value(), "");
        s.delete_forward(); // no-op at end
        assert_eq!(s.value(), "");
    }

    #[test]
    fn collapse_versus_extended_movement() {
        let mut s = state("abcd");
        s.move_to_start(false);
        s.move_right(true); // select "a"
        s.move_right(true); // select "ab"
        assert_eq!(s.selection(), TextSpan::new(0, 2));
        assert!(s.has_selection());
        // A non-extending left collapses to the selection's left edge.
        s.move_left(false);
        assert_eq!(s.caret(), 0);
        assert!(!s.has_selection());
        // Re-select then a non-extending right collapses to the right edge.
        s.select_all();
        assert_eq!(s.selection(), TextSpan::new(0, 4));
        s.move_right(false);
        assert_eq!(s.caret(), 4);
        assert!(!s.has_selection());
    }

    #[test]
    fn extended_movement_backward_keeps_the_anchor() {
        let mut s = state("abc");
        // Caret at end; extend left twice selects "bc" backward.
        s.move_left(true);
        s.move_left(true);
        assert_eq!(s.selection(), TextSpan::new(1, 3));
        assert_eq!(s.caret(), 1);
        // Document-edge extend to start selects the whole run.
        s.move_to_start(true);
        assert_eq!(s.selection(), TextSpan::new(0, 3));
        assert_eq!(s.caret(), 0);
    }

    #[test]
    fn typing_replaces_the_active_selection() {
        let mut s = state("hello");
        s.move_to_start(false);
        s.move_right(true);
        s.move_right(true); // select "he"
        assert!(s.insert("HE", uncapped()));
        assert_eq!(s.value(), "HEllo");
        assert_eq!(s.caret(), 2);
        assert!(!s.has_selection());
    }

    #[test]
    fn multibyte_movement_and_selection_keep_byte_boundaries() {
        let mut s = state("aéb"); // boundaries: 0, 1, 3, 4
        s.move_left(true);
        assert_eq!(s.selection(), TextSpan::new(3, 4));
        assert_eq!(s.caret(), 3);
        s.move_left(true);
        assert_eq!(s.selection(), TextSpan::new(1, 4));
        assert_eq!(s.caret(), 1);
        s.move_right(true);
        assert_eq!(s.selection(), TextSpan::new(3, 4));
        assert_eq!(s.caret(), 3);
    }

    #[test]
    fn mid_codepoint_place_and_extend_floor_before_replacement() {
        let mut s = state("éx"); // boundaries: 0, 2, 3
        s.place_caret(1);
        assert_eq!(s.caret(), 0, "a click inside é floors before the char");
        s.extend_to(2);
        assert_eq!(s.selection(), TextSpan::new(0, 2));
        assert!(s.insert("Z", uncapped()));
        assert_eq!(s.value(), "Zx");
        assert_eq!(s.caret(), 1);

        s.place_caret(2);
        s.extend_to(usize::MAX);
        assert_eq!(s.selection(), TextSpan::new(2, 2));
    }

    #[test]
    fn select_all_then_replace_clears_and_retypes() {
        let mut s = state("old");
        s.select_all();
        assert!(s.insert("new", uncapped()));
        assert_eq!(s.value(), "new");
        // Backspace over a whole selection deletes it, not one char.
        s.select_all();
        s.delete_backward();
        assert_eq!(s.value(), "");
    }

    #[test]
    fn whole_insert_respects_the_character_cap() {
        let policy = EditPolicy { single_line: true, max_chars: 3 };
        let mut s = state("ab");
        assert!(s.insert("c", policy), "fills to the cap");
        assert_eq!(s.value(), "abc");
        assert!(!s.insert("d", policy), "past-cap insert is rejected whole");
        assert_eq!(s.value(), "abc");
        // A multi-char insert that would overflow is rejected whole, not
        // truncated.
        let mut fresh = state("a");
        assert!(!fresh.insert("bcd", policy));
        assert_eq!(fresh.value(), "a");
        // Replacing a selection frees cap room: select "abc", insert 3 chars.
        let mut full = state("abc");
        full.select_all();
        assert!(full.insert("xyz", policy));
        assert_eq!(full.value(), "xyz");
    }

    #[test]
    fn single_line_policy_filters_line_breaks() {
        let policy = EditPolicy { single_line: true, max_chars: 0 };
        let mut s = state("");
        assert!(s.insert("a\nb\r\nc", policy));
        assert_eq!(s.value(), "abc");

        let multiline = EditPolicy { single_line: false, max_chars: 0 };
        let mut m = state("");
        assert!(m.insert("a\nb", multiline));
        assert_eq!(m.value(), "a\nb");
    }

    #[test]
    fn composition_normalizes_and_clears() {
        let mut s = state("");
        // A span landing mid-`char` floors to the char boundary and orders.
        s.set_composition(String::from("éà"), Some(TextSpan::new(3, 1)));
        assert_eq!(s.preedit(), "éà");
        assert_eq!(s.preedit_cursor(), Some(TextSpan::new(0, 2)));
        // An out-of-range span is dropped, not clamped into a lie.
        s.set_composition(String::from("x"), Some(TextSpan::new(0, 9)));
        assert_eq!(s.preedit_cursor(), None);
        // Empty text clears the composition.
        s.set_composition(String::new(), Some(TextSpan::new(0, 0)));
        assert_eq!(s.preedit(), "");
        assert_eq!(s.preedit_cursor(), None);
    }

    /// A 1000-upm table with three different advances so caret x and hit-test
    /// midpoints are not the same everywhere (a monospace table would hide a
    /// per-glyph advance bug).
    fn variable_metrics() -> CachedFontMetrics {
        CachedFontMetrics::new(&FontMetrics {
            units_per_em: 1000.0,
            ascent: 800.0,
            descent: -200.0,
            line_gap: 0.0,
            default_advance: 500.0,
            advances: vec![
                GlyphAdvance { codepoint: u32::from('i'), advance_units: 200.0 },
                GlyphAdvance { codepoint: u32::from('m'), advance_units: 800.0 },
                GlyphAdvance { codepoint: u32::from('x'), advance_units: 500.0 },
            ],
        })
    }

    #[test]
    fn layout_caret_x_tracks_variable_advances() {
        let metrics = variable_metrics();
        let layout = SingleLineLayout::build("imx", &metrics, 100.0);
        // Advances at 100px / 1000upm: i=20, m=80, x=50.
        assert_eq!(layout.caret_x(0), 0.0);
        assert_eq!(layout.caret_x(1), 20.0); // after 'i'
        assert_eq!(layout.caret_x(2), 100.0); // after 'm'
        assert_eq!(layout.caret_x(3), 150.0); // after 'x'
        assert_eq!(layout.width(), 150.0);
    }

    #[test]
    fn layout_hit_test_rounds_to_the_nearest_boundary_midpoint() {
        let metrics = variable_metrics();
        let layout = SingleLineLayout::build("imx", &metrics, 100.0);
        // Stops at x = 0, 20, 100, 150; midpoints at 10, 60, 125.
        assert_eq!(layout.hit_test(-5.0), 0);
        assert_eq!(layout.hit_test(9.0), 0);
        assert_eq!(layout.hit_test(10.0), 1, "a tie rounds to the later boundary");
        assert_eq!(layout.hit_test(59.0), 1);
        assert_eq!(layout.hit_test(61.0), 2);
        assert_eq!(layout.hit_test(124.0), 2);
        assert_eq!(layout.hit_test(126.0), 3);
        assert_eq!(layout.hit_test(999.0), 3, "past the end clamps to the last stop");
    }
}
