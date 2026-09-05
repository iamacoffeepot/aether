// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! Multiline measured text editing with a fixed whole-line viewport.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_clipboard::{GetClipboardTextResult, SetClipboardTextResult};
use aether_kinds::keycode::{KEY_DOWN, KEY_ENTER, KEY_UP};
use aether_kinds::{
    CachedFontMetrics, ImePreedit, Key, Modifiers, MouseButton, MouseButtonRelease, MouseMove, TextInput, mouse_button,
};
use aether_text::FontMetricsResult;

use crate::set::defaults::WidgetDefaults;
use crate::set::{
    accept_clipboard_paste, apply_text_control_state, apply_text_theme, approx_text_width, edit_command,
    pump_text_font_metrics, push_control_outlines, quad, release_left, reply_with_draw_items, report_clipboard_copy,
    run_edit_key, single_line_hit_byte, text_baseline_y, text_control_theme_state, text_origin_y,
    update_text_modifiers,
};
use crate::state::InteractionState;
use crate::text_edit::{EditPolicy, FontMetricsAdapter, SingleLineLayout, TextEditState, TextSpan};
use crate::theme::{SetTheme, Theme, ThemeState};
use crate::{
    Collect, FocusGained, FocusLost, SetWidgetState, TextAreaConfig, TextCommitted, WidgetControlState, WidgetDrawItem,
    WidgetFrame,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextLine {
    start_byte: usize,
    end_byte: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerticalDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnterAction {
    Ignore,
    InsertNewline,
    Commit,
}

fn text_lines(text: &str) -> Vec<TextLine> {
    let mut lines = Vec::new();
    let mut start_byte = 0;
    for (byte, ch) in text.char_indices() {
        if ch == '\n' {
            lines.push(TextLine { start_byte, end_byte: byte });
            start_byte = byte + ch.len_utf8();
        }
    }
    lines.push(TextLine { start_byte, end_byte: text.len() });
    lines
}

fn line_index_for_byte(lines: &[TextLine], byte: usize) -> usize {
    lines.iter().position(|line| byte <= line.end_byte).unwrap_or_else(|| lines.len().saturating_sub(1))
}

fn span_on_line(span: TextSpan, line: TextLine, text_len: usize) -> Option<TextSpan> {
    let start_byte = span.start_byte.max(line.start_byte).min(line.end_byte);
    let end_byte = span.end_byte.min(line.end_byte).max(start_byte);
    if start_byte < end_byte {
        return Some(TextSpan::new(start_byte - line.start_byte, end_byte - line.start_byte));
    }
    let selects_newline = line.end_byte < text_len && span.start_byte <= line.end_byte && span.end_byte > line.end_byte;
    selects_newline.then_some(TextSpan::new(line.end_byte - line.start_byte, line.end_byte - line.start_byte))
}

/// A multiline text editor. It delegates UTF-8-safe mutation, selection, and
/// IME state to [`TextEditState`] and owns only line layout, vertical motion,
/// whole-line scrolling, and the Enter/Ctrl+Enter policy.
pub struct TextAreaWidget {
    edit: TextEditState,
    max_chars: u32,
    rows: u32,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    modifiers: Modifiers,
    dragging: bool,
    /// Whether a clipboard read is outstanding; a second paste chord while one
    /// is in flight is dropped rather than queued.
    paste_pending: bool,
    preferred_x_pixels: Option<f32>,
    scroll_top: usize,
    font_metrics: FontMetricsAdapter,
}

impl TextAreaWidget {
    fn policy(&self) -> EditPolicy {
        EditPolicy { single_line: false, max_chars: self.max_chars }
    }

    fn visible_rows(&self) -> usize {
        usize::try_from(self.rows.max(1)).unwrap_or(usize::MAX)
    }

    fn theme_state(&self) -> ThemeState {
        text_control_theme_state(&self.state, self.dragging)
    }

    fn apply_control_state(&mut self, ctx: &WasmCtx<'_>, next: WidgetControlState) {
        apply_text_control_state(ctx, &mut self.state, &mut self.edit, &mut self.dragging, next);
    }

    fn pump_font_metrics(&mut self, ctx: &mut WasmCtx<'_>) {
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    fn reconcile_scroll(&mut self) {
        let lines = text_lines(self.edit.value());
        let caret_line = line_index_for_byte(&lines, self.edit.caret());
        let visible = self.visible_rows();
        if caret_line < self.scroll_top {
            self.scroll_top = caret_line;
        } else if caret_line >= self.scroll_top.saturating_add(visible) {
            self.scroll_top = caret_line.saturating_add(1).saturating_sub(visible);
        }
        self.scroll_top = self.scroll_top.min(lines.len().saturating_sub(visible.min(lines.len())));
    }

    fn after_horizontal_or_edit(&mut self) {
        self.preferred_x_pixels = None;
        self.reconcile_scroll();
    }

    fn enter_action(&self) -> EnterAction {
        if !self.state.can_mutate() {
            EnterAction::Ignore
        } else if self.modifiers.ctrl || self.modifiers.meta {
            EnterAction::Commit
        } else {
            EnterAction::InsertNewline
        }
    }

    /// The pixel x of the caret `byte` bytes into `line_text`.
    ///
    /// Approximated per character until the font's advances land, the same
    /// degradation [`single_line_hit_byte`] makes in the other direction and
    /// the single-line draw path already makes for its own caret. A text
    /// control whose font never resolves — the theme names one the host never
    /// loaded — has to stay usable rather than stop answering the pointer.
    fn line_caret_x(&self, line_text: &str, byte: usize) -> f32 {
        self.font_metrics.resolved().map_or_else(
            || approx_text_width(line_text[..byte].chars().count(), self.theme.value_size_pixels),
            |metrics| SingleLineLayout::build(line_text, metrics, self.theme.value_size_pixels).caret_x(byte),
        )
    }

    fn move_vertical(&mut self, direction: VerticalDirection, extend: bool) {
        let (target_byte, desired_x) = {
            let text = self.edit.value();
            let lines = text_lines(text);
            let current_index = line_index_for_byte(&lines, self.edit.caret());
            let target_index = match direction {
                VerticalDirection::Up => current_index.checked_sub(1),
                VerticalDirection::Down => current_index.checked_add(1).filter(|index| *index < lines.len()),
            };
            let Some(target_index) = target_index else {
                return;
            };

            let current = lines[current_index];
            let local_caret = self.edit.caret().min(current.end_byte) - current.start_byte;
            let desired_x = self
                .preferred_x_pixels
                .unwrap_or_else(|| self.line_caret_x(&text[current.start_byte..current.end_byte], local_caret));

            let target_line = lines[target_index];
            let landed = single_line_hit_byte(
                &text[target_line.start_byte..target_line.end_byte],
                self.font_metrics.resolved(),
                self.theme.value_size_pixels,
                desired_x,
            );
            (target_line.start_byte + landed, desired_x)
        };

        if extend {
            self.edit.extend_to(target_byte);
        } else {
            self.edit.place_caret(target_byte);
        }
        self.preferred_x_pixels = Some(desired_x);
        self.reconcile_scroll();
    }

    fn scroll_drag_edge(&mut self, event_y: f32) {
        let lines = text_lines(self.edit.value());
        let max_first = lines.len().saturating_sub(self.visible_rows().min(lines.len()));
        if event_y < self.frame.y {
            self.scroll_top = self.scroll_top.saturating_sub(1);
        } else if event_y >= self.frame.y + self.frame.height {
            self.scroll_top = self.scroll_top.saturating_add(1).min(max_first);
        }
    }

    fn hit_byte(&self, event_x: f32, event_y: f32) -> usize {
        let lines = text_lines(self.edit.value());
        let visible = self.visible_rows();
        let local_y = event_y - self.frame.y;
        let row_height = self.theme.row_height.max(1.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let row = if local_y <= 0.0 {
            0
        } else {
            (local_y / row_height) as usize
        }
        .min(visible.saturating_sub(1));
        let line_index = self.scroll_top.saturating_add(row).min(lines.len().saturating_sub(1));
        let line = lines[line_index];
        line.start_byte
            + single_line_hit_byte(
                &self.edit.value()[line.start_byte..line.end_byte],
                self.font_metrics.resolved(),
                self.theme.value_size_pixels,
                event_x - self.frame.x - self.theme.pad,
            )
    }

    fn push_line_band(&self, items: &mut Vec<WidgetDrawItem>, layout: &SingleLineLayout, span: TextSpan, row_top: f32) {
        let x0 = self.theme.pad + layout.caret_x(span.start_byte);
        let x1 = self.theme.pad + layout.caret_x(span.end_byte);
        let height = self.theme.pad.mul_add(-2.0, self.theme.row_height).max(1.0);
        items.push(quad(x0, row_top + self.theme.pad, (x1 - x0).max(1.0), height, self.theme.accent));
    }

    fn line_layout(&self, text: &str) -> Option<SingleLineLayout> {
        self.font_metrics.resolved().map(|metrics| SingleLineLayout::build(text, metrics, self.theme.value_size_pixels))
    }

    fn draw_items(&self) -> Vec<WidgetDrawItem> {
        let width = self.frame.width;
        let height = self.frame.height;
        let size = self.theme.value_size_pixels;
        let row_height = self.theme.row_height;
        let theme_state = self.theme_state();
        let displayed = self.edit.displayed();
        let lines = text_lines(&displayed.text);
        let first = self.scroll_top.min(lines.len().saturating_sub(1));
        let end = first.saturating_add(self.visible_rows()).min(lines.len());
        let mut items = vec![quad(0.0, 0.0, width, height, self.theme.fill(self.theme.surface_raised, theme_state))];

        for (visible_index, line) in lines[first..end].iter().copied().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let row_top = visible_index as f32 * row_height;
            let line_text = &displayed.text[line.start_byte..line.end_byte];
            let layout = self.line_layout(line_text);

            if let Some(layout) = &layout {
                if let Some(span) =
                    displayed.selection_span.and_then(|span| span_on_line(span, line, displayed.text.len()))
                {
                    self.push_line_band(&mut items, layout, span, row_top);
                }
                if let Some(span) = displayed
                    .preedit_cursor_span
                    .filter(|span| !span.is_collapsed())
                    .and_then(|span| span_on_line(span, line, displayed.text.len()))
                {
                    self.push_line_band(&mut items, layout, span, row_top);
                }
            }

            if !line_text.is_empty() {
                items.push(WidgetDrawItem::Text {
                    x: self.theme.pad,
                    y: text_origin_y(row_top, row_height, size),
                    font_id: self.theme.font_id,
                    text: String::from(line_text),
                    size_pixels: size,
                    color: self.theme.fill(self.theme.text_primary, theme_state),
                    clip: None,
                });
            }

            let Some(layout) = &layout else {
                continue;
            };
            if let Some(span) = displayed.preedit_span.and_then(|span| span_on_line(span, line, displayed.text.len())) {
                let x0 = self.theme.pad + layout.caret_x(span.start_byte);
                let x1 = self.theme.pad + layout.caret_x(span.end_byte);
                items.push(quad(
                    x0,
                    text_baseline_y(row_top, row_height, size),
                    (x1 - x0).max(1.0),
                    1.0,
                    self.theme.accent,
                ));
                if let Some(cursor) = displayed
                    .preedit_cursor_span
                    .filter(|cursor| cursor.is_collapsed())
                    .filter(|cursor| cursor.start_byte >= line.start_byte && cursor.end_byte <= line.end_byte)
                {
                    let local = cursor.end_byte - line.start_byte;
                    let cursor_x = self.theme.pad + layout.caret_x(local);
                    items.push(quad(
                        cursor_x,
                        row_top + self.theme.pad,
                        1.0,
                        self.theme.pad.mul_add(-2.0, row_height).max(1.0),
                        self.theme.accent,
                    ));
                }
            }
            if self.state.focused()
                && !displayed.composing
                && displayed.caret_byte >= line.start_byte
                && displayed.caret_byte <= line.end_byte
            {
                let local = displayed.caret_byte - line.start_byte;
                let caret_x = self.theme.pad + layout.caret_x(local);
                items.push(quad(
                    caret_x,
                    row_top + self.theme.pad,
                    1.0,
                    self.theme.pad.mul_add(-2.0, row_height).max(1.0),
                    self.theme.accent,
                ));
            }
        }
        push_control_outlines(&mut items, width, height, &self.state, &self.theme);
        items
    }
}

impl WidgetDefaults for TextAreaWidget {
    fn widget_frame(&mut self) -> &mut WidgetFrame {
        &mut self.frame
    }

    fn widget_theme(&mut self) -> &mut Theme {
        &mut self.theme
    }

    fn widget_state(&mut self) -> &mut InteractionState {
        &mut self.state
    }

    fn cancel_activation(&mut self) {
        self.dragging = false;
        self.paste_pending = false;
        self.preferred_x_pixels = None;
        self.edit.clear_composition();
    }
}

/// Multiline text area with a fixed whole-line viewport.
///
/// # Agent
/// Not loaded directly — a panel spawns it from [`TextAreaConfig`]. Plain
/// Enter inserts a newline; Ctrl+Enter emits [`TextCommitted`] to the parent.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for TextAreaWidget {
    type Config = TextAreaConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.text_area";

    fn init(config: TextAreaConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let mut area = Self {
            edit: TextEditState::new(config.initial),
            max_chars: config.max_chars,
            rows: config.rows,
            font_metrics: FontMetricsAdapter::new(config.theme.font_id),
            theme: config.theme,
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            state: InteractionState::new(config.state),
            modifiers: Modifiers::default(),
            dragging: false,
            paste_pending: false,
            preferred_x_pixels: None,
            scroll_top: 0,
        };
        area.reconcile_scroll();
        Ok(area)
    }

    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        self.pump_font_metrics(ctx);
    }

    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: TextAreaConfig) {
        self.edit = TextEditState::new(config.initial);
        self.max_chars = config.max_chars;
        self.rows = config.rows;
        self.font_metrics.set_desired(config.theme.font_id);
        self.theme = config.theme;
        self.dragging = false;
        self.paste_pending = false;
        self.preferred_x_pixels = None;
        self.scroll_top = 0;
        self.apply_control_state(ctx, config.state);
        self.reconcile_scroll();
        self.pump_font_metrics(ctx);
    }

    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        self.apply_control_state(ctx, set.state);
    }

    #[handler::single]
    //noinspection DuplicatedCode -- actor macros require one handler per type; the implementation is shared.
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        apply_text_theme(ctx, &mut self.font_metrics, &mut self.theme, set.theme);
    }

    #[handler::single]
    fn on_focus_gained(&mut self, _ctx: &mut WasmCtx<'_>, gained: FocusGained) {
        self.state.gain_focus(gained.keyboard);
        self.reconcile_scroll();
    }

    #[handler::single]
    fn on_focus_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: FocusLost) {
        self.state.lose_focus();
        self.dragging = false;
        self.paste_pending = false;
        self.preferred_x_pixels = None;
        self.edit.clear_composition();
    }

    #[handler::single]
    fn on_text_input(&mut self, _ctx: &mut WasmCtx<'_>, input: TextInput) {
        if !self.state.can_mutate() {
            return;
        }
        self.edit.clear_composition();
        if self.edit.insert(&input.text, self.policy()) {
            self.after_horizontal_or_edit();
        }
    }

    /// Vertical motion and the Enter policy are the area's own; every other
    /// editing key resolves through the set's shared vocabulary, so Home/End
    /// reach the ends of the *line* the caret is on and word motion, Delete,
    /// and the Ctrl-or-Cmd clipboard chords work here exactly as in a field.
    /// A repeated press is another edit, never a suppressed repeat.
    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        if !self.state.is_available() {
            return;
        }
        let extend = self.modifiers.shift;
        match key.code {
            KEY_UP => self.move_vertical(VerticalDirection::Up, extend),
            KEY_DOWN => self.move_vertical(VerticalDirection::Down, extend),
            KEY_ENTER => match self.enter_action() {
                EnterAction::Ignore => {}
                EnterAction::Commit => {
                    if let Some(parent) = ctx.parent() {
                        parent.send(&TextCommitted { text: String::from(self.edit.value()) });
                    }
                }
                EnterAction::InsertNewline => {
                    if self.edit.insert("\n", self.policy()) {
                        self.after_horizontal_or_edit();
                    }
                }
            },
            code => {
                if let Some(command) = edit_command(code, self.modifiers) {
                    run_edit_key(ctx, &mut self.edit, &mut self.paste_pending, command, self.state.can_mutate());
                    self.after_horizontal_or_edit();
                }
            }
        }
    }

    /// Settle an outstanding clipboard read into the buffer.
    #[handler::single]
    fn on_get_clipboard_text_result(&mut self, _ctx: &mut WasmCtx<'_>, result: GetClipboardTextResult) {
        let (policy, mutable) = (self.policy(), self.state.can_mutate());
        if accept_clipboard_paste(&mut self.paste_pending, &mut self.edit, policy, mutable, result) {
            self.after_horizontal_or_edit();
        }
    }

    #[handler::single]
    #[allow(clippy::unused_self)]
    fn on_set_clipboard_text_result(&mut self, _ctx: &mut WasmCtx<'_>, result: SetClipboardTextResult) {
        report_clipboard_copy(&result);
    }

    #[handler::single]
    fn on_mouse_button(&mut self, _ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if press.button != mouse_button::LEFT || !self.state.is_available() {
            return;
        }
        self.dragging = true;
        self.preferred_x_pixels = None;
        self.edit.place_caret(self.hit_byte(press.x, press.y));
        self.reconcile_scroll();
    }

    #[handler::single]
    fn on_mouse_move(&mut self, _ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        if !self.dragging || !self.state.is_available() {
            return;
        }
        self.scroll_drag_edge(moved.y);
        self.edit.extend_to(self.hit_byte(moved.x, moved.y));
        self.preferred_x_pixels = None;
    }

    #[handler::single]
    fn on_mouse_button_release(&mut self, _ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        release_left(&mut self.dragging, false, release);
    }

    #[handler::single]
    fn on_modifiers(&mut self, _ctx: &mut WasmCtx<'_>, modifiers: Modifiers) {
        update_text_modifiers(&self.state, &mut self.modifiers, modifiers);
    }

    #[handler::single]
    fn on_ime_preedit(&mut self, _ctx: &mut WasmCtx<'_>, preedit: ImePreedit) {
        if !self.state.can_mutate() {
            return;
        }
        let cursor = preedit
            .cursor_begin
            .zip(preedit.cursor_end)
            .map(|(begin, end)| TextSpan::new(begin as usize, end as usize));
        self.edit.set_composition(preedit.text, cursor);
        self.preferred_x_pixels = None;
    }

    #[handler::single]
    fn on_font_metrics_result(&mut self, ctx: &mut WasmCtx<'_>, result: FontMetricsResult) {
        let pump_deferred = match result {
            FontMetricsResult::Ok { metrics } => self.font_metrics.accept_reply(Some(CachedFontMetrics::new(&metrics))),
            FontMetricsResult::Err { error } => {
                tracing::warn!(target: "aether_kit_widget", %error, "text area font metrics failed");
                self.font_metrics.accept_reply(None)
            }
        };
        if pump_deferred {
            self.pump_font_metrics(ctx);
        }
    }

    #[handler::single]
    //noinspection DuplicatedCode -- actor macros require one collect handler per concrete widget type.
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        reply_with_draw_items(ctx, &self.state, || self.draw_items());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_kinds::{FontMetrics, GlyphAdvance};

    use crate::set::APPROX_ADVANCE_RATIO;

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
            ],
        })
    }

    fn area(text: &str, rows: u32) -> TextAreaWidget {
        built(text, rows, Some(variable_metrics()))
    }

    /// An area whose font never resolved — the theme names a font
    /// `aether.text` cannot load, so the reply is an error and the adapter
    /// installs nothing.
    fn unmeasured_area(text: &str, rows: u32) -> TextAreaWidget {
        built(text, rows, None)
    }

    #[allow(clippy::cast_precision_loss)] // test rows are tiny exact integers
    fn built(text: &str, rows: u32, metrics: Option<CachedFontMetrics>) -> TextAreaWidget {
        let mut font_metrics = FontMetricsAdapter::new(7);
        assert_eq!(font_metrics.take_pending_request(), Some(7));
        assert!(!font_metrics.accept_reply(metrics));
        let mut area = TextAreaWidget {
            edit: TextEditState::new(String::from(text)),
            max_chars: 0,
            rows,
            theme: Theme { value_size_pixels: 100.0, ..Theme::DEFAULT },
            frame: WidgetFrame {
                x: 10.0,
                y: 20.0,
                width: 400.0,
                height: Theme::DEFAULT.row_height * rows.max(1) as f32,
            },
            state: InteractionState::new(WidgetControlState::default()),
            modifiers: Modifiers::default(),
            dragging: false,
            paste_pending: false,
            preferred_x_pixels: None,
            scroll_top: 0,
            font_metrics,
        };
        area.state.gain_focus(true);
        area.reconcile_scroll();
        area
    }

    #[test]
    fn line_indexing_preserves_empty_and_trailing_rows() {
        assert_eq!(
            text_lines("a\n\n"),
            vec![
                TextLine { start_byte: 0, end_byte: 1 },
                TextLine { start_byte: 2, end_byte: 2 },
                TextLine { start_byte: 3, end_byte: 3 },
            ]
        );
    }

    #[test]
    fn enter_policy_inserts_newlines_and_reserves_ctrl_enter_for_commit() {
        let mut area = area("terrain", 2);
        assert_eq!(area.enter_action(), EnterAction::InsertNewline);
        assert!(area.edit.insert("\n", area.policy()));
        assert_eq!(area.edit.value(), "terrain\n");

        area.modifiers.ctrl = true;
        assert_eq!(area.enter_action(), EnterAction::Commit);

        let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
        area.state.replace(read_only);
        assert_eq!(area.enter_action(), EnterAction::Ignore);
    }

    #[test]
    fn vertical_motion_preserves_measured_x_across_a_short_line() {
        let mut area = area("imx\ni\nimx", 3);
        area.edit.place_caret(3);
        area.move_vertical(VerticalDirection::Down, false);
        assert_eq!(area.edit.caret(), 5, "short middle line clamps at its end");
        area.move_vertical(VerticalDirection::Down, false);
        assert_eq!(
            area.edit.caret(),
            area.edit.value().len(),
            "the original measured x recovers on the longer third line"
        );
    }

    #[test]
    fn shift_vertical_selection_replaces_on_type_without_splitting_utf8() {
        let mut area = area("éx\nmi", 2);
        area.edit.place_caret(2);
        area.move_vertical(VerticalDirection::Down, true);
        let selected = area.edit.selection();
        assert_eq!(selected.start_byte, 2);
        assert!(selected.end_byte > selected.start_byte);
        assert!(area.edit.value().is_char_boundary(selected.end_byte));
        assert!(area.edit.insert("Q", area.policy()));
        assert_eq!(area.edit.value(), "éQi");
    }

    #[test]
    fn an_unmeasured_area_still_places_a_caret_and_moves_it_vertically() {
        // A text field whose font never resolves stays usable: its hit test
        // falls back to the per-character approximation. The area used to have
        // no fallback at all, so a press placed no caret and armed no drag and
        // Up/Down did nothing — the reader could type into it but never click
        // into it or move through it.
        let mut area = unmeasured_area("imx\ni\nimx", 3);
        let advance = area.theme.value_size_pixels * APPROX_ADVANCE_RATIO;
        let byte = area.hit_byte(
            advance.mul_add(2.0, area.frame.x + area.theme.pad),
            area.theme.row_height.mul_add(1.5, area.frame.y),
        );
        assert_eq!(byte, 5, "the second row, past the end of its one short character");

        area.edit.place_caret(byte);
        area.move_vertical(VerticalDirection::Up, false);
        assert_eq!(area.edit.caret(), 1, "and the approximated x carries up to the row above");
    }

    #[test]
    fn scroll_window_tracks_caret_in_both_directions() {
        let mut area = area("zero\none\ntwo\nthree", 2);
        assert_eq!(area.scroll_top, 2, "end caret reveals the final two rows");
        area.edit.move_to_start(false);
        area.reconcile_scroll();
        assert_eq!(area.scroll_top, 0, "document start reveals the first rows");
    }

    #[test]
    fn multiline_selection_draws_one_measured_band_per_covered_row() {
        let mut area = area("im\ni", 2);
        area.edit.move_to_start(false);
        area.edit.extend_to(area.edit.value().len());
        let items = area.draw_items();
        let accent_bands: Vec<_> = items
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Quad { x, y, width, color, .. } if *color == Theme::DEFAULT.accent && *width > 1.0 => {
                    Some((*x, *y, *width))
                }
                _ => None,
            })
            .collect();
        assert!(
            accent_bands.iter().any(|(_, y, width)| *y == 8.0 && *width == 100.0),
            "first row uses measured i+m width"
        );
        assert!(
            accent_bands.iter().any(|(_, y, width)| *y == 32.0 && *width == 20.0),
            "second row uses measured i width"
        );
    }
}
