// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The single-line text field (issue 2660, reworked in issue 2924).
//!
//! The field is a thin actor over the reusable
//! [`TextEditState`]: committed
//! `TextInput` replaces the active selection at the caret; the editing keys the
//! substrate emits scancodes for (Backspace, Left, Right, Enter) delete, move —
//! extending the selection while Shift is held — or commit; a pointer press
//! places the caret and a drag extends the selection; an in-flight IME
//! composition (`ImePreedit`) renders underlined at the active selection with
//! its reported cursor span. Every caret / selection / preedit offset lands on a
//! `char` boundary, so a multi-byte character is never split.
//!
//! Caret placement and pointer hit-testing are exact once the font's metrics
//! settle: the field drives a single-flight [`FontMetricsRequest`] for its
//! theme's font and measures against the resolved
//! [`CachedFontMetrics`]. Until then it falls
//! back to the proportional approximation as a bounded font-warm-up placement.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_capabilities::TextCapability;
use aether_capabilities::text::{FontMetricsRequest, FontMetricsResult, FontRef};
use aether_kinds::keycode::{KEY_BACKSPACE, KEY_ENTER, KEY_LEFT, KEY_RIGHT};
use aether_kinds::{
    CachedFontMetrics, ImePreedit, Key, Modifiers, MouseButton, MouseButtonRelease, MouseMove,
    TextInput, mouse_button,
};

use crate::widget::set::{
    APPROX_ADVANCE_RATIO, approx_text_width, push_border, quad, text_origin_y,
};
use crate::widget::text_edit::{EditPolicy, SingleLineLayout, TextEditState, TextSpan};
use crate::widget::theme::{SetTheme, Theme, WidgetState};
use crate::widget::{
    Collect, FocusGained, FocusLost, TextCommitted, TextFieldConfig, WidgetDrawItem,
    WidgetDrawList, WidgetFrame,
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
    focused: bool,
    modifiers: Modifiers,
    /// Whether a left-button drag is in progress (a pointer move only extends
    /// the selection while the button is held, never on a bare hover).
    dragging: bool,
    /// The latest font id the theme asks metrics for.
    desired_font_id: u32,
    /// The font id whose metrics are currently installed.
    current_font_id: Option<u32>,
    /// The font id of the one outstanding `FontMetricsRequest`, if any.
    inflight_font_id: Option<u32>,
    /// The resolved metrics for `current_font_id`, `None` until the first reply
    /// installs.
    metrics: Option<CachedFontMetrics>,
}

/// The committed editor state projected into the one string rendered this
/// frame, with every owned span remapped into that string's byte offsets.
struct DisplayedEdit {
    text: String,
    caret_byte: usize,
    selection_span: Option<TextSpan>,
    preedit_span: Option<TextSpan>,
    preedit_cursor_span: Option<TextSpan>,
    composing: bool,
}

impl TextFieldWidget {
    /// The insert policy the field enforces: single-line, capped at
    /// `max_chars`.
    fn policy(&self) -> EditPolicy {
        EditPolicy {
            single_line: true,
            max_chars: self.max_chars,
        }
    }

    /// Adopt a desired font id. Metrics are only usable for the id that
    /// produced them, so a change returns placement to the warm-up fallback
    /// until the replacement request resolves.
    fn set_desired_font_id(&mut self, desired_font_id: u32) {
        if self.desired_font_id == desired_font_id {
            return;
        }
        self.desired_font_id = desired_font_id;
        self.current_font_id = None;
        self.metrics = None;
    }

    /// Metrics are exact only while their installed font is still the theme's
    /// desired font. The id check keeps stale tables out of hit testing and all
    /// selection / composition / caret geometry.
    fn resolved_metrics(&self) -> Option<&CachedFontMetrics> {
        (self.current_font_id == Some(self.desired_font_id))
            .then_some(self.metrics.as_ref())
            .flatten()
    }

    /// The font id a pump should request now, or `None` to stay put: only when
    /// no request is outstanding and the desired id lacks current metrics.
    /// Font id zero is valid; absence is represented by `Option` instead of a
    /// numeric sentinel. Pure over the adapter fields.
    fn pending_request(&self) -> Option<u32> {
        if self.inflight_font_id.is_some() {
            return None;
        }
        if self.current_font_id == Some(self.desired_font_id) && self.metrics.is_some() {
            return None;
        }
        Some(self.desired_font_id)
    }

    /// Reserve the next request — the id to fetch (marking it in-flight) or
    /// `None` if none is due. Pure over the adapter fields; the caller sends the
    /// wire request.
    fn take_pending_request(&mut self) -> Option<u32> {
        let id = self.pending_request()?;
        self.inflight_font_id = Some(id);
        Some(id)
    }

    /// Attribute a metrics reply to the outstanding flight (`Some` for `Ok`,
    /// `None` for `Err`), install it only when its id is still the desired id,
    /// and always clear the flight. Returns whether the settled flight was
    /// superseded and the caller should pump the deferred newer id. A current
    /// id's error deliberately returns `false`: retrying it immediately would
    /// form an unbounded request/reply loop.
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

    /// Start a font-metrics request when one is due (single-flight; a duplicate
    /// desired id coalesces onto the outstanding flight).
    fn pump_font_metrics(&mut self, ctx: &mut WasmCtx<'_>) {
        if let Some(id) = self.take_pending_request() {
            ctx.actor::<TextCapability>().send(&FontMetricsRequest {
                font: FontRef::Id(id),
            });
        }
    }

    /// Project committed text, selection, and preedit into one render string.
    /// While composing, the preedit replaces the active selection and its
    /// cursor span is translated from preedit-local into display byte offsets.
    fn displayed_edit(&self) -> DisplayedEdit {
        let selection = self.edit.selection();
        if self.edit.preedit().is_empty() {
            return DisplayedEdit {
                text: String::from(self.edit.value()),
                caret_byte: self.edit.caret(),
                selection_span: (!selection.is_collapsed()).then_some(selection),
                preedit_span: None,
                preedit_cursor_span: None,
                composing: false,
            };
        }

        let preedit = self.edit.preedit();
        let mut text = String::with_capacity(self.edit.value().len() + preedit.len());
        text.push_str(&self.edit.value()[..selection.start_byte]);
        text.push_str(preedit);
        text.push_str(&self.edit.value()[selection.end_byte..]);
        let span_end = selection.start_byte + preedit.len();
        let cursor = self
            .edit
            .preedit_cursor()
            .unwrap_or_else(|| TextSpan::new(preedit.len(), preedit.len()));
        DisplayedEdit {
            text,
            caret_byte: span_end,
            selection_span: None,
            preedit_span: Some(TextSpan::new(selection.start_byte, span_end)),
            preedit_cursor_span: Some(TextSpan::new(
                selection.start_byte + cursor.start_byte,
                selection.start_byte + cursor.end_byte,
            )),
            composing: true,
        }
    }

    /// The `char`-boundary byte offset a pointer at window `event_x` lands on —
    /// exact against the metric table when resolved, else the proportional
    /// approximation.
    fn hit_byte(&self, event_x: f32) -> usize {
        let size = self.theme.value_size_pixels;
        let local_x = event_x - self.frame.x - self.theme.pad;
        let text = self.edit.value();
        if let Some(metrics) = self.resolved_metrics() {
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
        text.char_indices()
            .nth(index)
            .map_or(text.len(), |(byte, _)| byte)
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
            frame: WidgetFrame {
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 0.0,
            },
            focused: false,
            modifiers: Modifiers::default(),
            dragging: false,
            desired_font_id,
            current_font_id: None,
            inflight_font_id: None,
            metrics: None,
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
        self.set_desired_font_id(config.theme.font_id);
        self.theme = config.theme;
        self.pump_font_metrics(ctx);
    }

    /// Restyle: adopt the fanned theme and request metrics for its font.
    #[handler::single]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        self.set_desired_font_id(set.theme.font_id);
        self.theme = set.theme;
        self.pump_font_metrics(ctx);
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
        self.dragging = false;
    }

    /// Insert committed text over the active selection. `TextInput` is already
    /// resolved through the layout and IME, so any composition ends here.
    #[handler::single]
    fn on_text_input(&mut self, _ctx: &mut WasmCtx<'_>, input: TextInput) {
        self.edit.clear_composition();
        self.edit.insert(&input.text, self.policy());
    }

    /// Editing keys: Backspace deletes, Left / Right move the caret (extending
    /// the selection while Shift is held), Enter commits the current contents up
    /// to the panel root.
    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        let extend = self.modifiers.shift;
        match key.code {
            KEY_BACKSPACE => self.edit.delete_backward(),
            KEY_LEFT => self.edit.move_left(extend),
            KEY_RIGHT => self.edit.move_right(extend),
            KEY_ENTER => {
                if let Some(parent) = ctx.parent() {
                    parent.send(&TextCommitted {
                        text: String::from(self.edit.value()),
                    });
                }
            }
            _ => {}
        }
    }

    /// A left press places the caret at the pointer and arms a drag; other
    /// buttons are ignored.
    #[handler::single]
    fn on_mouse_button(&mut self, _ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if press.button != mouse_button::LEFT {
            return;
        }
        self.dragging = true;
        let byte = self.hit_byte(press.x);
        self.edit.place_caret(byte);
    }

    /// A move during a live drag extends the selection to the pointer.
    #[handler::single]
    fn on_mouse_move(&mut self, _ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        if !self.dragging {
            return;
        }
        let byte = self.hit_byte(moved.x);
        self.edit.extend_to(byte);
    }

    /// A left release ends the drag.
    #[handler::single]
    fn on_mouse_button_release(&mut self, _ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        if release.button == mouse_button::LEFT {
            self.dragging = false;
        }
    }

    /// Cache the latest modifier state (Ctrl / Shift / …) so Shift-extended
    /// movement and future chord-aware edits can consult it.
    #[handler::single]
    fn on_modifiers(&mut self, _ctx: &mut WasmCtx<'_>, modifiers: Modifiers) {
        self.modifiers = modifiers;
    }

    /// Track the in-flight IME composition at the active selection. Empty text
    /// clears it.
    #[handler::single]
    fn on_ime_preedit(&mut self, _ctx: &mut WasmCtx<'_>, preedit: ImePreedit) {
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
            FontMetricsResult::Ok { metrics } => {
                self.accept_reply(Some(CachedFontMetrics::new(&metrics)))
            }
            FontMetricsResult::Err { error } => {
                tracing::warn!(target: "aether_kit", %error, "text field font metrics failed");
                self.accept_reply(None)
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
        let width = self.frame.width;
        let height = self.frame.height;
        let pad = self.theme.pad;
        let size = self.theme.value_size_pixels;
        let text_y = text_origin_y(0.0, height, size);
        let caret_height = pad.mul_add(-2.0, height).max(1.0);

        let displayed = self.displayed_edit();

        // One measured layout per rendered string. Every geometry lookup below
        // reads this table; the warm-up fallback remains character-count based
        // only until the desired font's metrics settle.
        let layout = self
            .resolved_metrics()
            .map(|metrics| SingleLineLayout::build(&displayed.text, metrics, size));
        let prefix_width = |byte: usize| {
            layout.as_ref().map_or_else(
                || approx_text_width(displayed.text[..byte].chars().count(), size),
                |layout| layout.caret_x(byte),
            )
        };

        let mut items: Vec<WidgetDrawItem> = Vec::new();
        items.push(quad(
            0.0,
            0.0,
            width,
            height,
            self.theme
                .fill(self.theme.surface_raised, WidgetState::Normal),
        ));
        if let Some(span) = displayed.selection_span {
            let x0 = pad + prefix_width(span.start_byte);
            let x1 = pad + prefix_width(span.end_byte);
            items.push(quad(
                x0,
                pad,
                (x1 - x0).max(1.0),
                caret_height,
                self.theme.accent,
            ));
        }
        if let Some(span) = displayed
            .preedit_cursor_span
            .filter(|span| !span.is_collapsed())
        {
            let x0 = pad + prefix_width(span.start_byte);
            let x1 = pad + prefix_width(span.end_byte);
            items.push(quad(
                x0,
                pad,
                (x1 - x0).max(1.0),
                caret_height,
                self.theme.accent,
            ));
        }
        if !displayed.text.is_empty() {
            items.push(WidgetDrawItem::Text {
                x: pad,
                y: text_y,
                font_id: self.theme.font_id,
                text: displayed.text.clone(),
                size_pixels: size,
                color: self.theme.text_primary,
            });
        }
        if let Some(span) = displayed.preedit_span {
            let x0 = pad + prefix_width(span.start_byte);
            let x1 = pad + prefix_width(span.end_byte);
            items.push(quad(
                x0,
                text_y + size,
                (x1 - x0).max(1.0),
                1.0,
                self.theme.accent,
            ));
            if let Some(cursor) = displayed
                .preedit_cursor_span
                .filter(|cursor| cursor.is_collapsed())
            {
                let cursor_x = pad + prefix_width(cursor.end_byte);
                items.push(quad(cursor_x, pad, 1.0, caret_height, self.theme.accent));
            }
        }
        if self.focused && !displayed.composing {
            let caret_x = pad + prefix_width(displayed.caret_byte);
            items.push(quad(caret_x, pad, 1.0, caret_height, self.theme.accent));
        }
        if self.focused {
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
    use aether_kinds::FontMetrics;

    /// A field with the given desired font id, no metrics resolved yet — the
    /// adapter's cold start.
    fn adapter_field(desired_font_id: u32) -> TextFieldWidget {
        TextFieldWidget {
            edit: TextEditState::new(String::new()),
            max_chars: 0,
            theme: Theme::DEFAULT,
            frame: WidgetFrame {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 24.0,
            },
            focused: false,
            modifiers: Modifiers::default(),
            dragging: false,
            desired_font_id,
            current_font_id: None,
            inflight_font_id: None,
            metrics: None,
        }
    }

    /// A minimal resolved metric table — enough to stand in for an installed
    /// font in the adapter transition tests.
    fn some_metrics() -> CachedFontMetrics {
        CachedFontMetrics::new(&FontMetrics {
            units_per_em: 1000.0,
            ascent: 800.0,
            descent: -200.0,
            line_gap: 0.0,
            default_advance: 500.0,
            advances: Vec::new(),
        })
    }

    #[test]
    fn composition_projection_preserves_the_full_multibyte_cursor_span() {
        let mut field = adapter_field(7);
        field.edit = TextEditState::new(String::from("abécd"));
        field.edit.move_left(true);
        field.edit.move_left(true); // select `cd` at bytes 4..6
        field
            .edit
            .set_composition(String::from("üx"), Some(TextSpan::new(0, 2)));

        let displayed = field.displayed_edit();
        assert_eq!(displayed.text, "abéüx");
        assert_eq!(displayed.selection_span, None);
        assert_eq!(displayed.preedit_span, Some(TextSpan::new(4, 7)));
        assert_eq!(displayed.preedit_cursor_span, Some(TextSpan::new(4, 6)));
        assert_eq!(displayed.caret_byte, 7);
        assert!(displayed.composing);
    }

    #[test]
    fn initial_load_requests_desired_once_then_coalesces() {
        let mut field = adapter_field(7);
        assert_eq!(field.take_pending_request(), Some(7));
        // The flight is outstanding, so a second pump requests nothing.
        assert_eq!(field.take_pending_request(), None);
    }

    #[test]
    fn a_resolved_reply_installs_and_stops_requesting() {
        let mut field = adapter_field(7);
        assert_eq!(field.take_pending_request(), Some(7));
        assert!(!field.accept_reply(Some(some_metrics())));
        assert_eq!(field.current_font_id, Some(7));
        assert!(field.metrics.is_some());
        // Desired is satisfied, so nothing more is requested.
        assert_eq!(field.take_pending_request(), None);
    }

    #[test]
    fn an_error_for_the_current_font_does_not_immediately_retry() {
        let mut field = adapter_field(7);
        assert_eq!(field.take_pending_request(), Some(7));
        assert!(
            !field.accept_reply(None),
            "the result handler must not pump the just-failed id"
        );
        assert_eq!(field.current_font_id, None);
        assert!(field.metrics.is_none());
        assert_eq!(field.inflight_font_id, None);
        // A later config/theme event may retry the id explicitly.
        assert_eq!(field.take_pending_request(), Some(7));
    }

    #[test]
    fn a_font_change_mid_flight_drops_the_stale_reply_and_defers_the_new_one() {
        let mut field = adapter_field(7);
        assert_eq!(field.take_pending_request(), Some(7));
        // The theme font changes while the request for 7 is outstanding.
        field.set_desired_font_id(9);
        // Still single-flight: no new request until the outstanding one settles.
        assert_eq!(field.take_pending_request(), None);
        // The reply for the superseded font 7 must not install as current.
        assert!(field.accept_reply(Some(some_metrics())));
        assert_eq!(field.current_font_id, None);
        assert!(field.metrics.is_none());
        // Now the deferred request for the latest desired id goes out and
        // installs.
        assert_eq!(field.take_pending_request(), Some(9));
        assert!(!field.accept_reply(Some(some_metrics())));
        assert_eq!(field.current_font_id, Some(9));
        assert!(field.metrics.is_some());
    }

    #[test]
    fn a_stale_error_pumps_the_deferred_newer_font() {
        let mut field = adapter_field(7);
        assert_eq!(field.take_pending_request(), Some(7));
        field.set_desired_font_id(9);
        assert!(field.accept_reply(None));
        assert_eq!(field.take_pending_request(), Some(9));
    }

    #[test]
    fn a_font_change_clears_installed_metrics_before_new_geometry() {
        let mut field = adapter_field(7);
        assert_eq!(field.take_pending_request(), Some(7));
        assert!(!field.accept_reply(Some(some_metrics())));
        assert!(field.resolved_metrics().is_some());

        field.set_desired_font_id(9);
        assert_eq!(field.current_font_id, None);
        assert!(field.metrics.is_none());
        assert!(field.resolved_metrics().is_none());
        assert_eq!(field.take_pending_request(), Some(9));
    }

    #[test]
    fn zero_is_a_valid_font_id_and_requests_metrics() {
        let mut field = adapter_field(0);
        assert_eq!(field.take_pending_request(), Some(0));
        assert!(!field.accept_reply(Some(some_metrics())));
        assert_eq!(field.current_font_id, Some(0));
        assert!(field.resolved_metrics().is_some());
    }
}
