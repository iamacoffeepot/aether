// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! Typed, steppable numeric editor (issue 2926).
//!
//! The visible edit buffer is deliberately separate from the last committed
//! number. Invalid intermediates remain visible without emitting a numeric
//! value; valid edits preview their clamped/snapped value without rewriting the
//! buffer. Enter or blur commits and canonicalizes, while an invalid commit
//! reverts. Up/Down step and commit immediately. Clipboard edits use the same
//! selection and parse paths as typed edits.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_capabilities::clipboard::{GetClipboardTextResult, SetClipboardTextResult};
use aether_capabilities::text::{FontMetricsRequest, FontMetricsResult, FontRef};
use aether_capabilities::{ClipboardCapability, ClipboardMailboxExt, TextCapability};
use aether_kinds::keycode::{
    KEY_A, KEY_BACKSPACE, KEY_C, KEY_DOWN, KEY_ENTER, KEY_LEFT, KEY_RIGHT, KEY_UP, KEY_V, KEY_X,
};
use aether_kinds::{
    CachedFontMetrics, ImePreedit, Key, Modifiers, MouseButton, MouseButtonRelease, MouseMove, TextInput,
};
use alloc::string::{String, ToString};

use crate::widget::set::{
    arm_text_drag, release_left, reply_with_draw_items, single_line_edit_draw_items, single_line_hit_byte,
};
use crate::widget::state::{InteractionState, emit_state_changed};
use crate::widget::text_edit::{EditPolicy, TextEditState, TextSpan};
use crate::widget::theme::{SetTheme, Theme, ThemeState};
use crate::widget::{
    Collect, FocusGained, FocusLost, HoverGained, HoverLost, NumericChanged, NumericConfig, SetWidgetState,
    WidgetControlState, WidgetFrame,
};

/// Retained edit-buffer bound; comfortably exceeds every canonical finite
/// `f32` literal while preventing unbounded typed or pasted intermediates.
const NUMERIC_EDIT_MAX_CHARS: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq)]
struct NumericEmission {
    value: f32,
    committed: bool,
}

#[derive(Debug, Clone, Copy)]
struct NumericBounds {
    min: f32,
    max: f32,
}

impl NumericBounds {
    fn from_config(min: f32, max: f32) -> Self {
        let min = if min.is_finite() {
            min
        } else {
            f32::MIN
        };
        let max = if max.is_finite() {
            max
        } else {
            f32::MAX
        };
        if min <= max {
            Self { min, max }
        } else {
            Self { min: max, max: min }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum StepDirection {
    Down,
    Up,
}

#[derive(Debug, PartialEq)]
struct CutEdit {
    copied: String,
    emission: Option<NumericEmission>,
}

/// A single-line numeric editor with an independent display buffer and
/// committed number.
pub struct NumericWidget {
    min: f32,
    max: f32,
    step: f32,
    committed_value: f32,
    edit: TextEditState,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    modifiers: Modifiers,
    dragging: bool,
    paste_pending: bool,
    desired_font_id: u32,
    current_font_id: Option<u32>,
    inflight_font_id: Option<u32>,
    metrics: Option<CachedFontMetrics>,
}

impl NumericWidget {
    fn configured(config: NumericConfig) -> Self {
        let desired_font_id = config.theme.font_id;
        let mut widget = Self {
            min: config.min,
            max: config.max,
            step: config.step,
            committed_value: 0.0,
            edit: TextEditState::default(),
            theme: config.theme,
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            state: InteractionState::new(config.state),
            modifiers: Modifiers::default(),
            dragging: false,
            paste_pending: false,
            desired_font_id,
            current_font_id: None,
            inflight_font_id: None,
            metrics: None,
        };
        let initial = widget.normalize(config.initial).or_else(|| widget.normalize(0.0)).unwrap_or(0.0);
        widget.committed_value = initial;
        widget.edit = TextEditState::new(Self::canonical(initial));
        widget
    }

    fn bounds(&self) -> NumericBounds {
        NumericBounds::from_config(self.min, self.max)
    }

    fn normalize(&self, raw: f32) -> Option<f32> {
        if !raw.is_finite() {
            return None;
        }
        let bounds = self.bounds();
        let clamped = raw.clamp(bounds.min, bounds.max);
        if !self.step.is_finite() || self.step <= 0.0 {
            return Some(clamped);
        }
        let steps = ((f64::from(clamped) - f64::from(bounds.min)) / f64::from(self.step)).round();
        let snapped = steps.mul_add(f64::from(self.step), f64::from(bounds.min));
        // The finite check below rejects an f64 result outside f32's range.
        #[allow(clippy::cast_possible_truncation)]
        let snapped = snapped as f32;
        Some(if snapped.is_finite() {
            snapped.clamp(bounds.min, bounds.max)
        } else {
            clamped
        })
    }

    fn canonical(value: f32) -> String {
        if value == 0.0 {
            String::from("0")
        } else {
            value.to_string()
        }
    }

    fn policy() -> EditPolicy {
        EditPolicy { single_line: true, max_chars: NUMERIC_EDIT_MAX_CHARS }
    }

    fn parsed_buffer(&self) -> Option<f32> {
        self.edit.value().parse::<f32>().ok().and_then(|value| self.normalize(value))
    }

    fn preview(&self) -> Option<NumericEmission> {
        self.parsed_buffer().map(|value| NumericEmission { value, committed: false })
    }

    fn insert_text(&mut self, text: &str) -> Option<NumericEmission> {
        self.edit.clear_composition();
        if self.edit.insert(text, Self::policy()) {
            self.preview()
        } else {
            None
        }
    }

    fn delete_backward(&mut self) -> Option<NumericEmission> {
        let before = String::from(self.edit.value());
        self.edit.clear_composition();
        self.edit.delete_backward();
        if self.edit.value() == before {
            None
        } else {
            self.preview()
        }
    }

    fn copy_selection(&self) -> Option<String> {
        let selection = self.edit.selection();
        (!selection.is_collapsed()).then(|| String::from(&self.edit.value()[selection.start_byte..selection.end_byte]))
    }

    fn cut_selection(&mut self) -> Option<CutEdit> {
        let copied = self.copy_selection()?;
        self.edit.clear_composition();
        self.edit.delete_backward();
        Some(CutEdit { copied, emission: self.preview() })
    }

    fn commit_buffer(&mut self) -> Option<NumericEmission> {
        let Some(value) = self.parsed_buffer() else {
            self.edit = TextEditState::new(Self::canonical(self.committed_value));
            return None;
        };
        self.committed_value = value;
        self.edit = TextEditState::new(Self::canonical(value));
        Some(NumericEmission { value, committed: true })
    }

    fn stepped(&mut self, direction: StepDirection) -> NumericEmission {
        let base = self.parsed_buffer().unwrap_or(self.committed_value);
        let amount = if self.step.is_finite() && self.step > 0.0 {
            self.step
        } else {
            1.0
        };
        let raw = match direction {
            StepDirection::Down => f64::from(base) - f64::from(amount),
            StepDirection::Up => f64::from(base) + f64::from(amount),
        };
        let bounds = self.bounds();
        let raw = if raw > f64::from(f32::MAX) {
            bounds.max
        } else if raw < f64::from(f32::MIN) {
            bounds.min
        } else {
            // The two guards above prove `raw` is inside f32's finite range.
            #[allow(clippy::cast_possible_truncation)]
            let raw = raw as f32;
            raw
        };
        let value = self.normalize(raw).unwrap_or(self.committed_value);
        self.committed_value = value;
        self.edit = TextEditState::new(Self::canonical(value));
        NumericEmission { value, committed: true }
    }

    fn emit(ctx: &WasmCtx<'_>, emission: NumericEmission) {
        if let Some(parent) = ctx.parent() {
            parent.send(&NumericChanged { value: emission.value, committed: emission.committed });
        }
    }

    fn set_desired_font_id(&mut self, desired_font_id: u32) {
        if self.desired_font_id == desired_font_id {
            return;
        }
        self.desired_font_id = desired_font_id;
        self.current_font_id = None;
        self.metrics = None;
    }

    fn resolved_metrics(&self) -> Option<&CachedFontMetrics> {
        (self.current_font_id == Some(self.desired_font_id)).then_some(self.metrics.as_ref()).flatten()
    }

    fn pending_request(&self) -> Option<u32> {
        if self.inflight_font_id.is_some()
            || (self.current_font_id == Some(self.desired_font_id) && self.metrics.is_some())
        {
            None
        } else {
            Some(self.desired_font_id)
        }
    }

    fn take_pending_request(&mut self) -> Option<u32> {
        let id = self.pending_request()?;
        self.inflight_font_id = Some(id);
        Some(id)
    }

    fn accept_reply(&mut self, metrics: Option<CachedFontMetrics>) -> bool {
        let Some(id) = self.inflight_font_id.take() else {
            return false;
        };
        if id != self.desired_font_id {
            return true;
        }
        if let Some(metrics) = metrics {
            self.current_font_id = Some(id);
            self.metrics = Some(metrics);
        }
        false
    }

    fn pump_font_metrics(&mut self, ctx: &mut WasmCtx<'_>) {
        if let Some(id) = self.take_pending_request() {
            ctx.actor::<TextCapability>().send(&FontMetricsRequest { font: FontRef::Id(id) });
        }
    }

    fn hit_byte(&self, event_x: f32) -> usize {
        single_line_hit_byte(
            self.edit.value(),
            self.resolved_metrics(),
            self.theme.value_size_pixels,
            event_x - self.frame.x - self.theme.pad,
        )
    }

    fn theme_state(&self) -> ThemeState {
        if self.state.focused() {
            self.state.supporting_theme_state(self.dragging)
        } else {
            self.state.theme_state(self.dragging)
        }
    }

    fn apply_control_state(&mut self, ctx: &WasmCtx<'_>, next: WidgetControlState) {
        if self.state.replace(next) {
            if !self.state.can_mutate() {
                self.edit.clear_composition();
                self.paste_pending = false;
            }
            if !self.state.is_available() {
                self.dragging = false;
            }
            emit_state_changed(ctx, &self.state);
        }
    }
}

/// A numeric editor. Spawned inline by a panel root with a [`NumericConfig`];
/// reports preview and committed [`NumericChanged`] events.
#[actor(instanced)]
impl WasmActor for NumericWidget {
    type Config = NumericConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.numeric";

    fn init(config: NumericConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self::configured(config))
    }

    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        self.pump_font_metrics(ctx);
    }

    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: NumericConfig) {
        self.min = config.min;
        self.max = config.max;
        self.step = config.step;
        self.set_desired_font_id(config.theme.font_id);
        self.theme = config.theme;
        let initial = self.normalize(config.initial).or_else(|| self.normalize(0.0)).unwrap_or(0.0);
        self.committed_value = initial;
        self.edit = TextEditState::new(Self::canonical(initial));
        self.dragging = false;
        self.paste_pending = false;
        self.apply_control_state(ctx, config.state);
        self.pump_font_metrics(ctx);
    }

    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        self.apply_control_state(ctx, set.state);
    }

    #[handler::single]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        self.set_desired_font_id(set.theme.font_id);
        self.theme = set.theme;
        self.pump_font_metrics(ctx);
    }

    #[handler::single]
    fn on_frame(&mut self, _ctx: &mut WasmCtx<'_>, frame: WidgetFrame) {
        self.frame = frame;
    }

    #[handler::single]
    fn on_focus_gained(&mut self, _ctx: &mut WasmCtx<'_>, _gained: FocusGained) {
        self.state.gain_focus();
    }

    #[handler::single]
    fn on_focus_lost(&mut self, ctx: &mut WasmCtx<'_>, _lost: FocusLost) {
        if self.state.can_mutate()
            && let Some(emission) = self.commit_buffer()
        {
            Self::emit(ctx, emission);
        }
        self.state.lose_focus();
        self.dragging = false;
        self.paste_pending = false;
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

    #[handler::single]
    fn on_text_input(&mut self, ctx: &mut WasmCtx<'_>, input: TextInput) {
        if self.state.can_mutate()
            && let Some(emission) = self.insert_text(&input.text)
        {
            Self::emit(ctx, emission);
        }
    }

    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        if !self.state.is_available() {
            return;
        }
        if self.modifiers.ctrl {
            match key.code {
                KEY_A => self.edit.select_all(),
                KEY_C => {
                    if let Some(text) = self.copy_selection() {
                        ctx.actor::<ClipboardCapability>().set_text(&text);
                    }
                }
                KEY_X if self.state.can_mutate() => {
                    if let Some(cut) = self.cut_selection() {
                        ctx.actor::<ClipboardCapability>().set_text(&cut.copied);
                        if let Some(emission) = cut.emission {
                            Self::emit(ctx, emission);
                        }
                    }
                }
                KEY_V if self.state.can_mutate() && !self.paste_pending => {
                    self.paste_pending = true;
                    ctx.actor::<ClipboardCapability>().get_text();
                }
                _ => {}
            }
            return;
        }
        let extend = self.modifiers.shift;
        match key.code {
            KEY_BACKSPACE if self.state.can_mutate() => {
                if let Some(emission) = self.delete_backward() {
                    Self::emit(ctx, emission);
                }
            }
            KEY_LEFT => self.edit.move_left(extend),
            KEY_RIGHT => self.edit.move_right(extend),
            KEY_ENTER if self.state.can_mutate() => {
                if let Some(emission) = self.commit_buffer() {
                    Self::emit(ctx, emission);
                }
            }
            KEY_DOWN if self.state.can_mutate() => Self::emit(ctx, self.stepped(StepDirection::Down)),
            KEY_UP if self.state.can_mutate() => Self::emit(ctx, self.stepped(StepDirection::Up)),
            _ => {}
        }
    }

    #[handler::single]
    fn on_mouse_button(&mut self, _ctx: &mut WasmCtx<'_>, press: MouseButton) {
        let Some(event_x) = arm_text_drag(&self.state, &mut self.dragging, press) else {
            return;
        };
        self.edit.place_caret(self.hit_byte(event_x));
    }

    #[handler::single]
    fn on_mouse_move(&mut self, _ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        if self.dragging && self.state.is_available() {
            let byte = self.hit_byte(moved.x);
            self.edit.extend_to(byte);
        }
    }

    #[handler::single]
    fn on_mouse_button_release(&mut self, _ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        release_left(&mut self.dragging, false, release);
    }

    #[handler::single]
    fn on_modifiers(&mut self, _ctx: &mut WasmCtx<'_>, modifiers: Modifiers) {
        if self.state.is_available() {
            self.modifiers = modifiers;
        }
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
    }

    #[handler::single]
    fn on_get_clipboard_text_result(&mut self, ctx: &mut WasmCtx<'_>, result: GetClipboardTextResult) {
        if !self.paste_pending {
            return;
        }
        self.paste_pending = false;
        match result {
            GetClipboardTextResult::Ok { text } if self.state.can_mutate() => {
                if let Some(emission) = self.insert_text(&text) {
                    Self::emit(ctx, emission);
                }
            }
            GetClipboardTextResult::Ok { .. } => {}
            GetClipboardTextResult::Err { error } => {
                tracing::warn!(target: "aether_kit", %error, "numeric clipboard paste failed");
            }
        }
    }

    #[handler::single]
    #[allow(clippy::unused_self)]
    fn on_set_clipboard_text_result(&mut self, _ctx: &mut WasmCtx<'_>, result: SetClipboardTextResult) {
        if let SetClipboardTextResult::Err { error } = result {
            tracing::warn!(target: "aether_kit", %error, "numeric clipboard copy failed");
        }
    }

    #[handler::single]
    fn on_font_metrics_result(&mut self, ctx: &mut WasmCtx<'_>, result: FontMetricsResult) {
        let pump_deferred = match result {
            FontMetricsResult::Ok { metrics } => self.accept_reply(Some(CachedFontMetrics::new(&metrics))),
            FontMetricsResult::Err { error } => {
                tracing::warn!(target: "aether_kit", %error, "numeric font metrics failed");
                self.accept_reply(None)
            }
        };
        if pump_deferred {
            self.pump_font_metrics(ctx);
        }
    }

    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        reply_with_draw_items(ctx, &self.state, || {
            single_line_edit_draw_items(
                &self.edit.displayed(),
                self.resolved_metrics(),
                &self.theme,
                &self.state,
                self.theme_state(),
                self.frame.width,
                self.frame.height,
            )
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric(min: f32, max: f32, step: f32, initial: f32) -> NumericWidget {
        NumericWidget::configured(NumericConfig {
            min,
            max,
            step,
            initial,
            theme: Theme::DEFAULT,
            state: WidgetControlState::default(),
        })
    }

    fn replace_buffer(widget: &mut NumericWidget, text: &str) -> Option<NumericEmission> {
        widget.edit.select_all();
        widget.insert_text(text)
    }

    #[test]
    fn invalid_intermediates_and_non_finite_values_stay_visible_without_events() {
        let mut widget = numeric(-10.0, 20.0, 0.5, 2.0);
        for invalid in ["", "-", ".", "NaN", "inf", "-inf"] {
            widget.edit.select_all();
            widget.edit.delete_backward();
            let emission = if invalid.is_empty() {
                widget.preview()
            } else {
                widget.insert_text(invalid)
            };
            assert_eq!(emission, None, "{invalid:?} is not a finite numeric preview");
            assert_eq!(widget.edit.value(), invalid);
        }
        assert_eq!(widget.committed_value, 2.0);
    }

    #[test]
    fn preview_clamps_and_snaps_without_rewriting_then_commit_canonicalizes() {
        let mut widget = numeric(0.0, 10.0, 0.5, 2.0);
        assert_eq!(replace_buffer(&mut widget, "7.26"), Some(NumericEmission { value: 7.5, committed: false }));
        assert_eq!(widget.edit.value(), "7.26", "preview preserves the authored buffer");
        assert_eq!(widget.commit_buffer(), Some(NumericEmission { value: 7.5, committed: true }));
        assert_eq!(widget.edit.value(), "7.5");

        assert_eq!(replace_buffer(&mut widget, "99"), Some(NumericEmission { value: 10.0, committed: false }));
        assert_eq!(widget.edit.value(), "99");
    }

    #[test]
    fn invalid_commit_reverts_to_the_last_canonical_value_without_event() {
        let mut widget = numeric(-10.0, 20.0, 0.5, 2.0);
        assert_eq!(replace_buffer(&mut widget, "-"), None);
        assert_eq!(widget.commit_buffer(), None);
        assert_eq!(widget.edit.value(), "2");
        assert_eq!(widget.committed_value, 2.0);
    }

    #[test]
    fn step_uses_the_current_valid_value_or_falls_back_to_committed() {
        let mut widget = numeric(-10.0, 20.0, 0.5, 2.0);
        assert_eq!(replace_buffer(&mut widget, "4.2"), Some(NumericEmission { value: 4.0, committed: false }));
        assert_eq!(widget.stepped(StepDirection::Up), NumericEmission { value: 4.5, committed: true });
        assert_eq!(widget.edit.value(), "4.5");

        assert_eq!(replace_buffer(&mut widget, "-"), None);
        assert_eq!(widget.stepped(StepDirection::Down), NumericEmission { value: 4.0, committed: true });
        assert_eq!(widget.edit.value(), "4");
    }

    #[test]
    fn copy_cut_and_paste_core_share_selection_and_preview_paths() {
        let mut widget = numeric(-10.0, 20.0, 0.5, 12.5);
        widget.edit.select_all();
        assert_eq!(widget.copy_selection().as_deref(), Some("12.5"));
        let cut = widget.cut_selection().expect("selected text cuts");
        assert_eq!(cut, CutEdit { copied: String::from("12.5"), emission: None });
        assert_eq!(widget.edit.value(), "");
        assert_eq!(widget.insert_text(&cut.copied), Some(NumericEmission { value: 12.5, committed: false }));
        assert_eq!(widget.edit.value(), "12.5");
    }

    #[test]
    fn typed_insertions_respect_internal_cap_and_normal_numeric_edits_continue() {
        let mut widget = numeric(-f32::MAX, f32::MAX, 0.0, 0.0);
        widget.edit.select_all();
        widget.edit.delete_backward();
        for _ in 0..40 {
            widget.insert_text("1");
        }
        assert_eq!(widget.edit.value().chars().count(), NUMERIC_EDIT_MAX_CHARS as usize);
        assert_eq!(widget.edit.value(), "11111111111111111111111111111111");

        assert_eq!(replace_buffer(&mut widget, "7.5"), Some(NumericEmission { value: 7.5, committed: false }));
        assert_eq!(widget.edit.value(), "7.5");
    }

    #[test]
    fn pointer_placement_and_selection_never_split_multibyte_text() {
        let mut widget = numeric(-10.0, 20.0, 0.5, 2.0);
        widget.frame = WidgetFrame { x: 10.0, y: 0.0, width: 100.0, height: 24.0 };
        widget.edit = TextEditState::new(String::from("1é2"));
        let byte = widget.hit_byte(widget.theme.value_size_pixels.mul_add(0.75, 10.0 + widget.theme.pad));
        assert!([0, 1, 3, 4].contains(&byte), "hit byte {byte} must be a UTF-8 boundary");
        widget.edit.place_caret(byte);
        widget.edit.move_right(true);
        let selection = widget.edit.selection();
        assert!(widget.edit.value().is_char_boundary(selection.start_byte));
        assert!(widget.edit.value().is_char_boundary(selection.end_byte));
    }

    #[test]
    fn canonical_zero_drops_negative_sign_and_reversed_bounds_are_safe() {
        assert_eq!(NumericWidget::canonical(-0.0), "0");
        let widget = numeric(10.0, -10.0, 1.0, 99.0);
        assert_eq!(widget.committed_value, 10.0);
        assert_eq!(widget.edit.value(), "10");
    }
}
