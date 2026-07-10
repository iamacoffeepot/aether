// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The single-line text field (issue 2660, reworked in issue 2924).
//!
//! The field is a thin actor over the reusable
//! [`TextEditState`]: committed
//! `TextInput` replaces the active selection at the caret; the editing keys the
//! field handles (Backspace, Left, Right, Enter) delete, move —
//! extending the selection while Shift is held — or commit; a pointer press
//! places the caret and a drag extends the selection; an in-flight IME
//! composition (`ImePreedit`) renders underlined at the active selection with
//! its reported cursor span. Every caret / selection / preedit offset lands on a
//! `char` boundary, so a multi-byte character is never split.
//!
//! Caret placement and pointer hit-testing are exact once the font's metrics
//! settle: the field drives a single-flight
//! [`FontMetricsRequest`](aether_capabilities::text::FontMetricsRequest) for its
//! theme's font and measures against the resolved
//! [`CachedFontMetrics`]. Until then it falls
//! back to the proportional approximation as a bounded font-warm-up placement.
//!
//! The chassis keycode vocabulary also includes Delete, Home, End, Page Up,
//! and Page Down. `TextFieldWidget` does not yet implement behavior for those
//! keys.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_capabilities::text::FontMetricsResult;
use aether_kinds::keycode::{KEY_BACKSPACE, KEY_ENTER, KEY_LEFT, KEY_RIGHT};
use aether_kinds::{
    CachedFontMetrics, ImePreedit, Key, Modifiers, MouseButton, MouseButtonRelease, MouseMove, TextInput, mouse_button,
};

use crate::widget::set::{
    APPROX_ADVANCE_RATIO, apply_text_control_state, apply_text_theme, approx_text_width, pump_text_font_metrics,
    push_control_outlines, quad, release_text_drag, reply_if_hidden, text_control_theme_state, text_origin_y,
    update_text_modifiers,
};
use crate::widget::state::InteractionState;
use crate::widget::text_edit::{EditPolicy, FontMetricsAdapter, SingleLineLayout, TextEditState, TextSpan};
use crate::widget::theme::{SetTheme, Theme, ThemeState};
use crate::widget::{
    Collect, FocusGained, FocusLost, HoverGained, HoverLost, SetWidgetState, TextCommitted, TextFieldConfig,
    WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame,
};

/// A single-line editable string. Holds the reusable editing state, the
/// character cap, the latest modifiers, whether a pointer drag is live, and the
/// single-flight font-metrics adapter that feeds exact caret placement.
pub struct TextFieldWidget {
    edit: TextEditState,
    /// Maximum character count (`0` = uncapped).
    max_chars: u32,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    modifiers: Modifiers,
    /// Whether a left-button drag is in progress (a pointer move only extends
    /// the selection while the button is held, never on a bare hover).
    dragging: bool,
    /// Single-flight exact metrics for the active theme font.
    font_metrics: FontMetricsAdapter,
}

impl TextFieldWidget {
    /// The insert policy the field enforces: single-line, capped at
    /// `max_chars`.
    fn policy(&self) -> EditPolicy {
        EditPolicy { single_line: true, max_chars: self.max_chars }
    }

    /// Start a font-metrics request when one is due (single-flight; a duplicate
    /// desired id coalesces onto the outstanding flight).
    fn pump_font_metrics(&mut self, ctx: &mut WasmCtx<'_>) {
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    /// The `char`-boundary byte offset a pointer at window `event_x` lands on —
    /// exact against the metric table when resolved, else the proportional
    /// approximation.
    fn hit_byte(&self, event_x: f32) -> usize {
        let size = self.theme.value_size_pixels;
        let local_x = event_x - self.frame.x - self.theme.pad;
        let text = self.edit.value();
        if let Some(metrics) = self.font_metrics.resolved() {
            return SingleLineLayout::build(text, metrics, size).hit_test(local_x);
        }
        let advance = (size * APPROX_ADVANCE_RATIO).max(1.0);
        let index = if local_x <= 0.0 {
            0
        } else {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let rounded = (local_x / advance + 0.5) as usize;
            rounded.min(text.chars().count())
        };
        text.char_indices().nth(index).map_or(text.len(), |(byte, _)| byte)
    }

    fn theme_state(&self) -> ThemeState {
        text_control_theme_state(&self.state, self.dragging)
    }

    fn apply_control_state(&mut self, ctx: &WasmCtx<'_>, next: WidgetControlState) {
        apply_text_control_state(ctx, &mut self.state, &mut self.edit, &mut self.dragging, next);
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
        let desired_font_id = config.theme.font_id;
        Ok(TextFieldWidget {
            edit: TextEditState::new(config.initial),
            max_chars: config.max_chars,
            theme: config.theme,
            state: InteractionState::new(config.state),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            modifiers: Modifiers::default(),
            dragging: false,
            font_metrics: FontMetricsAdapter::new(desired_font_id),
        })
    }

    /// Kick off the font-metrics request for the initial theme font (inline
    /// children now run `wire`).
    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        self.pump_font_metrics(ctx);
    }

    /// Reset the contents / cap / theme in place from a re-sent config, and
    /// request metrics for the new theme font.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: TextFieldConfig) {
        self.edit = TextEditState::new(config.initial);
        self.max_chars = config.max_chars;
        self.font_metrics.set_desired(config.theme.font_id);
        self.theme = config.theme;
        self.apply_control_state(ctx, config.state);
        self.pump_font_metrics(ctx);
    }

    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        self.apply_control_state(ctx, set.state);
    }

    /// Restyle: adopt the fanned theme and request metrics for its font.
    #[handler::single]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        apply_text_theme(ctx, &mut self.font_metrics, &mut self.theme, set.theme);
    }

    /// Cache the layout rect the root assigned.
    #[handler::single]
    fn on_frame(&mut self, _ctx: &mut WasmCtx<'_>, frame: WidgetFrame) {
        self.frame = frame;
    }

    /// Take keyboard focus (draw the caret and focus ring).
    #[handler::single]
    fn on_focus_gained(&mut self, _ctx: &mut WasmCtx<'_>, _gained: FocusGained) {
        self.state.gain_focus();
    }

    /// Release keyboard focus.
    #[handler::single]
    fn on_focus_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: FocusLost) {
        self.state.lose_focus();
        self.dragging = false;
        self.edit.clear_composition();
    }

    #[handler::single]
    fn on_hover_gained(&mut self, _ctx: &mut WasmCtx<'_>, _gained: HoverGained) {
        self.state.set_hovered(true);
    }

    #[handler::single]
    fn on_hover_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: HoverLost) {
        self.state.set_hovered(false);
    }

    /// Insert committed text over the active selection. `TextInput` is already
    /// resolved through the layout and IME, so any composition ends here.
    #[handler::single]
    fn on_text_input(&mut self, _ctx: &mut WasmCtx<'_>, input: TextInput) {
        if !self.state.can_mutate() {
            return;
        }
        self.edit.clear_composition();
        self.edit.insert(&input.text, self.policy());
    }

    /// Editing keys: Backspace deletes, Left / Right move the caret (extending
    /// the selection while Shift is held), Enter commits the current contents up
    /// to the panel root.
    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        if !self.state.is_available() {
            return;
        }
        let extend = self.modifiers.shift;
        match key.code {
            KEY_BACKSPACE if self.state.can_mutate() => self.edit.delete_backward(),
            KEY_LEFT => self.edit.move_left(extend),
            KEY_RIGHT => self.edit.move_right(extend),
            KEY_ENTER if self.state.can_mutate() => {
                if let Some(parent) = ctx.parent() {
                    parent.send(&TextCommitted { text: String::from(self.edit.value()) });
                }
            }
            _ => {}
        }
    }

    /// A left press places the caret at the pointer and arms a drag; other
    /// buttons are ignored.
    #[handler::single]
    fn on_mouse_button(&mut self, _ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if press.button != mouse_button::LEFT || !self.state.is_available() {
            return;
        }
        self.dragging = true;
        let byte = self.hit_byte(press.x);
        self.edit.place_caret(byte);
    }

    /// A move during a live drag extends the selection to the pointer.
    #[handler::single]
    fn on_mouse_move(&mut self, _ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        if !self.dragging || !self.state.is_available() {
            return;
        }
        let byte = self.hit_byte(moved.x);
        self.edit.extend_to(byte);
    }

    /// A left release ends the drag.
    #[handler::single]
    fn on_mouse_button_release(&mut self, _ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        release_text_drag(&mut self.dragging, release);
    }

    /// Cache the latest modifier state (Ctrl / Shift / …) so Shift-extended
    /// movement and future chord-aware edits can consult it.
    #[handler::single]
    fn on_modifiers(&mut self, _ctx: &mut WasmCtx<'_>, modifiers: Modifiers) {
        update_text_modifiers(&self.state, &mut self.modifiers, modifiers);
    }

    /// Track the in-flight IME composition at the active selection. Empty text
    /// clears it.
    #[handler::single]
    fn on_ime_preedit(&mut self, _ctx: &mut WasmCtx<'_>, preedit: ImePreedit) {
        if !self.state.can_mutate() {
            return;
        }
        let cursor = match (preedit.cursor_begin, preedit.cursor_end) {
            (Some(begin), Some(end)) => Some(TextSpan::new(begin as usize, end as usize)),
            _ => None,
        };
        self.edit.set_composition(preedit.text, cursor);
    }

    /// Install a font-metrics reply and pump any deferred newer request. A
    /// stale reply (its font is no longer the desired one) is dropped.
    #[handler::single]
    fn on_font_metrics_result(&mut self, ctx: &mut WasmCtx<'_>, result: FontMetricsResult) {
        let pump_deferred = match result {
            FontMetricsResult::Ok { metrics } => self.font_metrics.accept_reply(Some(CachedFontMetrics::new(&metrics))),
            FontMetricsResult::Err { error } => {
                tracing::warn!(target: "aether_kit", %error, "text field font metrics failed");
                self.font_metrics.accept_reply(None)
            }
        };
        if pump_deferred {
            self.pump_font_metrics(ctx);
        }
    }

    /// Reply the field's local draw: a box, any selection band, the text plus a
    /// preedit at the caret, the composition underline / cursor, the caret when
    /// focused, and a focus ring.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        let Some(parent) = ctx.parent() else {
            return;
        };
        let width = self.frame.width;
        let height = self.frame.height;
        let pad = self.theme.pad;
        let size = self.theme.value_size_pixels;
        let text_y = text_origin_y(0.0, height, size);
        let caret_height = pad.mul_add(-2.0, height).max(1.0);
        let theme_state = self.theme_state();

        let displayed = self.edit.displayed();

        // One measured layout per rendered string. Every geometry lookup below
        // reads this table; the warm-up fallback remains character-count based
        // only until the desired font's metrics settle.
        let layout =
            self.font_metrics.resolved().map(|metrics| SingleLineLayout::build(&displayed.text, metrics, size));
        let prefix_width = |byte: usize| {
            layout.as_ref().map_or_else(
                || approx_text_width(displayed.text[..byte].chars().count(), size),
                |layout| layout.caret_x(byte),
            )
        };

        let mut items: Vec<WidgetDrawItem> = Vec::new();
        items.push(quad(0.0, 0.0, width, height, self.theme.fill(self.theme.surface_raised, theme_state)));
        if let Some(span) = displayed.selection_span {
            let x0 = pad + prefix_width(span.start_byte);
            let x1 = pad + prefix_width(span.end_byte);
            items.push(quad(x0, pad, (x1 - x0).max(1.0), caret_height, self.theme.accent));
        }
        if let Some(span) = displayed.preedit_cursor_span.filter(|span| !span.is_collapsed()) {
            let x0 = pad + prefix_width(span.start_byte);
            let x1 = pad + prefix_width(span.end_byte);
            items.push(quad(x0, pad, (x1 - x0).max(1.0), caret_height, self.theme.accent));
        }
        if !displayed.text.is_empty() {
            items.push(WidgetDrawItem::Text {
                x: pad,
                y: text_y,
                font_id: self.theme.font_id,
                text: displayed.text.clone(),
                size_pixels: size,
                color: self.theme.fill(self.theme.text_primary, theme_state),
                clip: None,
            });
        }
        if let Some(span) = displayed.preedit_span {
            let x0 = pad + prefix_width(span.start_byte);
            let x1 = pad + prefix_width(span.end_byte);
            items.push(quad(x0, text_y + size, (x1 - x0).max(1.0), 1.0, self.theme.accent));
            if let Some(cursor) = displayed.preedit_cursor_span.filter(|cursor| cursor.is_collapsed()) {
                let cursor_x = pad + prefix_width(cursor.end_byte);
                items.push(quad(cursor_x, pad, 1.0, caret_height, self.theme.accent));
            }
        }
        if self.state.focused() && !displayed.composing {
            let caret_x = pad + prefix_width(displayed.caret_byte);
            items.push(quad(caret_x, pad, 1.0, caret_height, self.theme.accent));
        }
        push_control_outlines(&mut items, width, height, &self.state, &self.theme);
        parent.send(&WidgetDrawList { intrinsic: None, items });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::WidgetControlState;

    fn field() -> TextFieldWidget {
        TextFieldWidget {
            edit: TextEditState::new(String::new()),
            max_chars: 0,
            theme: Theme::DEFAULT,
            state: InteractionState::new(WidgetControlState::default()),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 100.0, height: 24.0 },
            modifiers: Modifiers::default(),
            dragging: false,
            font_metrics: FontMetricsAdapter::new(7),
        }
    }

    #[test]
    fn composition_projection_preserves_the_full_multibyte_cursor_span() {
        let mut field = field();
        field.edit = TextEditState::new(String::from("abécd"));
        field.edit.move_left(true);
        field.edit.move_left(true); // select `cd` at bytes 4..6
        field.edit.set_composition(String::from("üx"), Some(TextSpan::new(0, 2)));

        let displayed = field.edit.displayed();
        assert_eq!(displayed.text, "abéüx");
        assert_eq!(displayed.selection_span, None);
        assert_eq!(displayed.preedit_span, Some(TextSpan::new(4, 7)));
        assert_eq!(displayed.preedit_cursor_span, Some(TextSpan::new(4, 6)));
        assert_eq!(displayed.caret_byte, 7);
        assert!(displayed.composing);
    }
}
