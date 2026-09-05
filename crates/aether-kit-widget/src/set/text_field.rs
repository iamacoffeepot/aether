// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (the full rationale is on the same allow in `lib.rs`).
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
//! [`FontMetricsRequest`](aether_text::FontMetricsRequest) for its
//! theme's font and measures against the resolved
//! [`CachedFontMetrics`]. Until then it falls
//! back to the proportional approximation as a bounded font-warm-up placement.
//!
//! Editing keys resolve through the set's shared `edit_command` vocabulary:
//! Backspace and Delete, Home and End, character / word / line-edge caret
//! motion, and select-all, copy, cut, and paste on Ctrl *or* Cmd. Enter is the
//! field's own — it commits. Page Up and Page Down have no meaning in one line
//! and are ignored.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_clipboard::{GetClipboardTextResult, SetClipboardTextResult};
use aether_kinds::keycode::KEY_ENTER;
use aether_kinds::{
    CachedFontMetrics, ImePreedit, Key, Modifiers, MouseButton, MouseButtonRelease, MouseMove, TextInput,
};
use aether_text::FontMetricsResult;
use alloc::string::String;

use crate::set::defaults::WidgetDefaults;
use crate::set::{
    SingleLineEdit, accept_clipboard_paste, apply_text_control_state, apply_text_theme, arm_text_drag, edit_command,
    pump_text_font_metrics, release_left, reply_single_line_edit, report_clipboard_copy, run_edit_key,
    single_line_hit_byte, text_control_theme_state, update_text_modifiers,
};
use crate::state::InteractionState;
use crate::text_edit::{EditPolicy, FontMetricsAdapter, TextEditState, TextSpan};
use crate::theme::{SetTheme, Theme, ThemeState};
use crate::{Collect, SetWidgetState, TextCommitted, TextFieldConfig, WidgetControlState, WidgetFrame};

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
    /// Whether a clipboard read is outstanding; a second paste chord while one
    /// is in flight is dropped rather than queued.
    paste_pending: bool,
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
        single_line_hit_byte(
            self.edit.value(),
            self.font_metrics.resolved(),
            self.theme.value_size_pixels,
            event_x - self.frame.x - self.theme.pad,
        )
    }

    fn theme_state(&self) -> ThemeState {
        text_control_theme_state(&self.state, self.dragging)
    }

    fn apply_control_state(&mut self, ctx: &WasmCtx<'_>, next: WidgetControlState) {
        apply_text_control_state(ctx, &mut self.state, &mut self.edit, &mut self.dragging, next);
    }
}

impl WidgetDefaults for TextFieldWidget {
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
        self.edit.clear_composition();
    }
}

/// A text-field widget. Spawned inline by a panel root with a
/// [`TextFieldConfig`]; reports [`TextCommitted`] up when Enter commits.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send it
/// its `TextFieldConfig` again to reset its contents or theme in place.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
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
            paste_pending: false,
            font_metrics: FontMetricsAdapter::new(desired_font_id),
        })
    }

    /// Kick off the font-metrics request for the initial theme font (inline
    /// children now run `wire`).
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
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
        self.dragging = false;
        self.paste_pending = false;
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

    /// Editing keys go through the set's shared vocabulary; Enter is the
    /// field's own, committing the current contents up to the panel root.
    ///
    /// Nothing is suppressed on repeat — a held Backspace arrives as a stream
    /// of `Key` presses and every one of them deletes.
    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        if !self.state.is_available() {
            return;
        }
        if key.code == KEY_ENTER {
            if self.state.can_mutate()
                && let Some(parent) = ctx.parent()
            {
                parent.send(&TextCommitted { text: String::from(self.edit.value()) });
            }
            return;
        }
        if let Some(command) = edit_command(key.code, self.modifiers) {
            run_edit_key(ctx, &mut self.edit, &mut self.paste_pending, command, self.state.can_mutate());
        }
    }

    /// Settle an outstanding clipboard read into the buffer.
    #[handler::single]
    fn on_get_clipboard_text_result(&mut self, _ctx: &mut WasmCtx<'_>, result: GetClipboardTextResult) {
        let (policy, mutable) = (self.policy(), self.state.can_mutate());
        accept_clipboard_paste(&mut self.paste_pending, &mut self.edit, policy, mutable, result);
    }

    #[handler::single]
    #[allow(clippy::unused_self)]
    fn on_set_clipboard_text_result(&mut self, _ctx: &mut WasmCtx<'_>, result: SetClipboardTextResult) {
        report_clipboard_copy(&result);
    }

    /// A left press places the caret at the pointer and arms a drag; other
    /// buttons are ignored.
    #[handler::single]
    fn on_mouse_button(&mut self, _ctx: &mut WasmCtx<'_>, press: MouseButton) {
        let Some(event_x) = arm_text_drag(&self.state, &mut self.dragging, press) else {
            return;
        };
        self.edit.place_caret(self.hit_byte(event_x));
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
        release_left(&mut self.dragging, false, release);
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
                tracing::warn!(target: "aether_kit_widget", %error, "text field font metrics failed");
                self.font_metrics.accept_reply(None)
            }
        };
        if pump_deferred {
            self.pump_font_metrics(ctx);
        }
    }

    /// Reply the field's local draw: a box, any selection band, the text plus a
    /// preedit at the caret, the composition underline / cursor, the caret when
    /// focused, and a focus ring — plus, while the pointer is over a field whose
    /// contents are too wide for the box, the overlay plate that shows the whole
    /// string.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        reply_single_line_edit(
            ctx,
            SingleLineEdit::new(
                &self.edit.displayed(),
                self.font_metrics.resolved(),
                &self.theme,
                &self.state,
                self.theme_state(),
                &self.frame,
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WidgetControlState;

    fn field() -> TextFieldWidget {
        TextFieldWidget {
            edit: TextEditState::new(String::new()),
            max_chars: 0,
            theme: Theme::DEFAULT,
            state: InteractionState::new(WidgetControlState::default()),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 100.0, height: 24.0 },
            modifiers: Modifiers::default(),
            dragging: false,
            paste_pending: false,
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
