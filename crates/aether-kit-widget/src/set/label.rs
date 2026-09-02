// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The static label (issue 2660).
//!
//! Non-interactive text — the trivial widget. It is not focus-eligible (the
//! root's focus register skips it, so a press on a label clears focus like a
//! press on the background) and it acts on no input except the hover edges the
//! root derives; it only draws its configured text each `Collect`.
//!
//! The label is where the screen's type scale surfaces: its
//! [`TextRole`] picks the size the theme sets that step at, and `Caption`
//! additionally draws in the muted ink, so hierarchy is a property of the
//! configured role rather than a pixel size chosen at the call site.
//!
//! The label measures its run: it drives the same single-flight
//! [`FontMetricsRequest`](aether_text::FontMetricsRequest) the measured text
//! controls do and lays out against the resolved
//! [`aether_kinds::CachedFontMetrics`]. An unmeasured run falls back to
//! `Start` rather than to a guessed width. The measurement is what a
//! non-`Start` [`TextAlign`] places the run by, and it is also what tells the
//! label its text does not fit: a run wider than the frame reveals itself
//! whole on a raised overlay plate while the pointer is over it, so text
//! clipped by a narrow column is readable without resizing anything.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, Mail, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::CachedFontMetrics;
use aether_math::Rgba;
use aether_text::FontMetricsResult;

use crate::set::{
    apply_static_control_state, overflow_reveal_items, pump_text_font_metrics, reply_if_hidden, text_origin_y,
};
use crate::state::{InteractionState, emit_state_changed};
use crate::text_edit::{FontMetricsAdapter, SingleLineLayout};
use crate::theme::{SetTheme, TextRole, Theme};
use crate::{
    Collect, HoverGained, HoverLost, LabelConfig, SetWidgetState, TextAlign, WidgetDrawItem, WidgetDrawList,
    WidgetFrame,
};

/// A static text label. Holds the text plus the cached theme / frame.
pub struct LabelWidget {
    text: String,
    /// Which step of the theme's type scale the text is set at.
    role: TextRole,
    /// Where the run sits in the assigned frame.
    align: TextAlign,
    theme: Theme,
    frame: WidgetFrame,
    /// Read-only and validation are inapplicable to a static label; visibility
    /// and enabled still control absence and muted presentation consistently.
    state: InteractionState,
    /// Single-flight exact metrics for the active theme font: the run's width
    /// places a non-`Start` alignment and decides whether the text overflows.
    font_metrics: FontMetricsAdapter,
}

impl LabelWidget {
    /// Start a font-metrics request when one is due. Every label measures: a
    /// `Start` label draws flush left either way, but the width is also how it
    /// knows its text is wider than its slot and owes the hover reveal.
    fn pump_font_metrics(&mut self, ctx: &mut WasmCtx<'_>) {
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    /// The run's measured width at `size_pixels`, or `None` while the metrics
    /// are still in flight.
    fn measured_width(&self, size_pixels: f32) -> Option<f32> {
        self.font_metrics.resolved().map(|metrics| SingleLineLayout::build(&self.text, metrics, size_pixels).width())
    }

    /// The hover reveal: the whole run on a raised plate, drawn from the
    /// label's own origin, whenever the pointer is over a label whose text
    /// overflows its frame. Empty otherwise.
    fn overflow_overlay(&self, size_pixels: f32, text_x: f32, text_width: Option<f32>) -> Vec<WidgetDrawItem> {
        if !self.state.hovered() {
            return Vec::new();
        }
        text_width.map_or_else(Vec::new, |text_width| {
            overflow_reveal_items(
                &self.theme,
                &self.text,
                text_x,
                text_width,
                size_pixels,
                self.theme.fill(self.ink(), self.state.theme_state(false)),
                &self.frame,
            )
        })
    }

    /// The ink the role reads in: a caption is a quieter aside, so it draws
    /// muted; every other step is primary text.
    fn ink(&self) -> Rgba {
        if self.role == TextRole::Caption {
            self.theme.text_muted
        } else {
            self.theme.text_primary
        }
    }
}

/// The local x a run `text_width` pixels wide sits at inside a `frame_width`
/// frame under `align`. `None` for the width means the run has not been
/// measured yet: an unmeasured run draws at the start rather than at a guessed
/// offset that would visibly jump once the metrics land.
fn align_x(align: TextAlign, frame_width: f32, text_width: Option<f32>) -> f32 {
    let Some(text_width) = text_width else {
        return 0.0;
    };
    match align {
        TextAlign::Start => 0.0,
        TextAlign::Center => ((frame_width - text_width) * 0.5).max(0.0),
        TextAlign::End => (frame_width - text_width).max(0.0),
    }
}

/// A label widget. Spawned inline by a panel root with a [`LabelConfig`];
/// draws its text and reports nothing up.
///
/// The label declares its own hover handlers rather than adopting
/// [`WidgetDefaults`](crate::set::WidgetDefaults): it needs the `#[fallback]`
/// below to absorb the raw pointer mail its hover eligibility earns it, and
/// the `#[actor]` macro cannot emit both an adopted set and a fallback (the
/// set delegation moves the mail the fallback tail then reads).
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send it
/// its `LabelConfig` again to change the text or theme in place.
#[actor(instanced, composable)]
impl WasmActor for LabelWidget {
    type Config = LabelConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.label";

    fn init(config: LabelConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let font_metrics = FontMetricsAdapter::new(config.theme.font_id);
        Ok(LabelWidget {
            text: config.text,
            role: config.role,
            align: config.align,
            theme: config.theme,
            state: InteractionState::new(config.state),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            font_metrics,
        })
    }

    /// Kick off the font-metrics request for the initial theme font (inline
    /// children run `wire`).
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        self.pump_font_metrics(ctx);
    }

    /// Change the text / role / alignment / theme in place from a re-sent
    /// config, and request metrics for the new theme font.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: LabelConfig) {
        self.text = config.text;
        self.role = config.role;
        self.align = config.align;
        self.font_metrics.set_desired(config.theme.font_id);
        self.theme = config.theme;
        if self.state.replace(config.state) {
            emit_state_changed(ctx, &self.state);
        }
        self.pump_font_metrics(ctx);
    }

    /// Update external availability without changing the label or theme.
    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        apply_static_control_state(ctx, &mut self.state, set.state);
    }

    /// Restyle: adopt the fanned theme and request metrics for its font.
    #[handler::single]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        self.font_metrics.set_desired(set.theme.font_id);
        self.theme = set.theme;
        self.pump_font_metrics(ctx);
    }

    /// Cache the layout rect the root assigned.
    #[handler::single]
    fn on_frame(&mut self, _ctx: &mut WasmCtx<'_>, frame: WidgetFrame) {
        self.frame = frame;
    }

    /// The pointer entered the label: an overflowing run now reveals itself.
    #[handler::single]
    fn on_hover_gained(&mut self, _ctx: &mut WasmCtx<'_>, _gained: HoverGained) {
        self.state.set_hovered(true);
    }

    /// The pointer left: the reveal goes away with it.
    #[handler::single]
    fn on_hover_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: HoverLost) {
        self.state.set_hovered(false);
    }

    /// Install a font-metrics reply and pump any deferred newer request. A
    /// stale reply (its font is no longer the desired one) is dropped.
    #[handler::single]
    fn on_font_metrics_result(&mut self, ctx: &mut WasmCtx<'_>, result: FontMetricsResult) {
        let pump_deferred = match result {
            FontMetricsResult::Ok { metrics } => self.font_metrics.accept_reply(Some(CachedFontMetrics::new(&metrics))),
            FontMetricsResult::Err { error } => {
                tracing::warn!(target: "aether_kit_widget", %error, "label font metrics failed");
                self.font_metrics.accept_reply(None)
            }
        };
        if pump_deferred {
            self.pump_font_metrics(ctx);
        }
    }

    /// Reply the label's local draw: its text at the size its role is set at,
    /// placed by its alignment (start-aligned until the run is measured) and
    /// inked muted for a caption, plus the hover reveal when the run does not
    /// fit.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        let size = self.theme.text_size_pixels(self.role);
        let measured = self.measured_width(size);
        let text_x = align_x(self.align, self.frame.width, measured);

        let mut items: Vec<WidgetDrawItem> = Vec::new();
        if !self.text.is_empty() {
            items.push(WidgetDrawItem::Text {
                x: text_x,
                y: text_origin_y(0.0, self.frame.height, size),
                font_id: self.theme.font_id,
                text: self.text.clone(),
                size_pixels: size,
                color: self.theme.fill(self.ink(), self.state.theme_state(false)),
                clip: None,
            });
        }

        let overlay = self.overflow_overlay(size, text_x, measured);
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { intrinsic: None, items, overlay });
        }
    }

    /// A hover-eligible label sits in the root's pointer hit table, so raw
    /// motion and presses over it are routed here. It acts on the hover edges
    /// the root derives, never on raw pointer mail, so the rest is dropped
    /// rather than warned about once per kind.
    #[allow(clippy::unused_self)] // the fallback ABI always receives the actor
    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_kinds::{FontMetrics, GlyphAdvance};
    use alloc::vec;

    /// A label with a resolved metric table whose every glyph advances half an
    /// em, so a run's width is `chars * size / 2` — enough to make "fits" and
    /// "overflows" exact without depending on a real font file.
    fn measured_label(text: &str, frame_width: f32) -> LabelWidget {
        let mut font_metrics = FontMetricsAdapter::new(0);
        font_metrics.take_pending_request();
        font_metrics.accept_reply(Some(CachedFontMetrics::new(&FontMetrics {
            units_per_em: 1000.0,
            ascent: 800.0,
            descent: -200.0,
            line_gap: 0.0,
            default_advance: 500.0,
            advances: vec![GlyphAdvance { codepoint: u32::from('m'), advance_units: 500.0 }],
        })));
        LabelWidget {
            text: String::from(text),
            role: TextRole::Body,
            align: TextAlign::Start,
            theme: Theme::DEFAULT,
            frame: WidgetFrame { x: 0.0, y: 0.0, width: frame_width, height: 24.0 },
            state: InteractionState::new(crate::WidgetControlState::default()),
            font_metrics,
        }
    }

    #[test]
    fn only_a_hovered_label_whose_run_overflows_raises_the_reveal() {
        // Tripwire: the reveal is what makes clipped label text readable, and
        // it must appear on exactly that case — a label that fits, or one
        // nobody is pointing at, raises a plate over its neighbours for
        // nothing.
        let size = Theme::DEFAULT.label_size_pixels;
        let width = |label: &LabelWidget| label.measured_width(size);

        let mut wide = measured_label("mmmmmmmmmmmmmmmm", 40.0);
        assert!(wide.overflow_overlay(size, 0.0, width(&wide)).is_empty(), "an un-hovered label reveals nothing");
        wide.state.set_hovered(true);
        assert!(!wide.overflow_overlay(size, 0.0, width(&wide)).is_empty(), "hovering an overflowing run reveals it");

        let mut narrow = measured_label("mm", 200.0);
        narrow.state.set_hovered(true);
        assert!(narrow.overflow_overlay(size, 0.0, width(&narrow)).is_empty(), "a run that fits reveals nothing");

        let mut unmeasured = measured_label("mmmmmmmmmmmmmmmm", 40.0);
        unmeasured.state.set_hovered(true);
        assert!(
            unmeasured.overflow_overlay(size, 0.0, None).is_empty(),
            "an unmeasured run has no width to raise a plate to",
        );
    }

    #[test]
    fn alignment_places_a_measured_run_and_falls_back_to_the_start_unmeasured() {
        assert_eq!(align_x(TextAlign::Start, 100.0, Some(30.0)), 0.0);
        assert_eq!(align_x(TextAlign::Center, 100.0, Some(30.0)), 35.0);
        assert_eq!(align_x(TextAlign::End, 100.0, Some(30.0)), 70.0);

        assert_eq!(align_x(TextAlign::Center, 100.0, None), 0.0, "an unmeasured run never guesses a width");
        assert_eq!(align_x(TextAlign::End, 100.0, None), 0.0);
    }

    #[test]
    fn a_run_wider_than_its_frame_stays_flush_with_the_left_edge() {
        // Overflow clips at the parent-owned slot's right edge; a negative
        // offset would instead push the head of the run out of the frame.
        assert_eq!(align_x(TextAlign::Center, 40.0, Some(120.0)), 0.0);
        assert_eq!(align_x(TextAlign::End, 40.0, Some(120.0)), 0.0);
        assert_eq!(align_x(TextAlign::End, 0.0, Some(0.0)), 0.0, "an unassigned frame has no right edge to hug");
    }
}
