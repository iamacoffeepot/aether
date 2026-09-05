// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (the full rationale is on the same allow in `lib.rs`).
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
//!
//! That measurement is also what the label reports up as its
//! [`WidgetDrawList::intrinsic`], so a consumer sizing a column of labels
//! sizes it to the words rather than to a share of the row.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, Mail, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::CachedFontMetrics;
use aether_math::Rgba;
use aether_text::FontMetricsResult;

use crate::set::{
    RevealPlate, apply_static_control_state, elide_to_width, overflow_reveal_items, pump_text_font_metrics,
    reply_if_hidden, text_origin_y,
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

    /// The run this label draws and the width it comes to: the whole text
    /// while it fits its frame, and the text **elided** with the kit's
    /// ellipsis when it does not.
    ///
    /// The label was the one text widget that let the root's slot clip do
    /// this cut, and a clip cuts mid-glyph: a stat row read `Physical damage
    /// mitigat`, which is a name that ends oddly rather than a name that was
    /// too long. The list and the button already cut with the mark that says
    /// so ([`elide_to_width`]), and this is the same rule in the last place
    /// it was missing.
    ///
    /// It is a **backstop, not a layout**. A column sized from the label's
    /// reported intrinsic — which stays the *whole* run's width, so it is
    /// still the number a consumer sizes to — never reaches this, and the
    /// hover reveal still carries the whole text on its plate. What this
    /// changes is only what a column too narrow for its words looks like.
    ///
    /// `None` for the width until the metrics land: an unmeasured run draws
    /// whole and flush at the start, the frame or two before there is a width
    /// to cut against, exactly as its alignment does.
    fn drawn_run(&self, size_pixels: f32, measured: Option<f32>) -> (String, Option<f32>) {
        let (Some(metrics), Some(measured)) = (self.font_metrics.resolved(), measured) else {
            return (self.text.clone(), None);
        };
        if measured <= self.frame.width {
            return (self.text.clone(), Some(measured));
        }

        let measure = |run: &str| SingleLineLayout::build(run, metrics, size_pixels).width();
        let run = elide_to_width(&self.text, self.frame.width, measure);
        let width = measure(&run);
        (run, Some(width))
    }

    /// The hover reveal: the whole run on a raised plate hung at the label's
    /// own origin, whenever the pointer is over a label whose text overflows
    /// its frame. Empty otherwise.
    ///
    /// The run sits one `pad` inside that plate rather than at the label's own
    /// alignment origin. The plate is its own box, and the alignment origin is
    /// `0.0` for the default `Start` — which would draw the glyphs under the
    /// one-pixel ring the plate is framed by, and leave a box the width
    /// accounts for a left margin it never lays.
    fn overflow_overlay(&self, size_pixels: f32, text_width: Option<f32>) -> Vec<WidgetDrawItem> {
        if !self.state.hovered() || text_width.is_none() {
            return Vec::new();
        }
        let Some(metrics) = self.font_metrics.resolved() else {
            return Vec::new();
        };
        let measure = |run: &str| SingleLineLayout::build(run, metrics, size_pixels).width();
        overflow_reveal_items(
            &RevealPlate {
                theme: &self.theme,
                text: &self.text,
                text_x: self.theme.pad,
                size_pixels,
                ink: self.theme.fill(self.ink(), self.state.theme_state(false)),
                content_width: self.frame.width,
                row_height: self.frame.height,
            },
            &measure,
        )
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
    /// The same measurement is reported as the draw list's `intrinsic`
    /// (gap 16) — `[measured width, theme row height]`, with no pad either
    /// side, because a label reserves none. A layout that wants a column to
    /// fit the words in it has the number the label already computed, instead
    /// of a share of the row or a character-count estimate. It is `None` until
    /// the theme font's metrics resolve, exactly as the button's is: a slot
    /// sized from a guess would resize the moment the real advances landed.
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
        // The drawn run may be cut; `measured` is the whole run's width and
        // stays that, because it is what the reveal fires on and what the
        // intrinsic reports. The alignment places what is actually drawn.
        let (run, drawn) = self.drawn_run(size, measured);
        let text_x = align_x(self.align, self.frame.width, drawn);

        let mut items: Vec<WidgetDrawItem> = Vec::new();
        if !run.is_empty() {
            items.push(WidgetDrawItem::Text {
                x: text_x,
                y: text_origin_y(0.0, self.frame.height, size),
                font_id: self.theme.font_id,
                text: run,
                size_pixels: size,
                color: self.theme.fill(self.ink(), self.state.theme_state(false)),
                clip: None,
            });
        }

        let overlay = self.overflow_overlay(size, measured);
        let intrinsic = measured.map(|text_width| [text_width, self.theme.row_height]);
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { content_height: None, intrinsic, items, overlay });
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

    use crate::set::ELLIPSIS;

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
        assert!(wide.overflow_overlay(size, width(&wide)).is_empty(), "an un-hovered label reveals nothing");
        wide.state.set_hovered(true);
        assert!(!wide.overflow_overlay(size, width(&wide)).is_empty(), "hovering an overflowing run reveals it");

        let mut narrow = measured_label("mm", 200.0);
        narrow.state.set_hovered(true);
        assert!(narrow.overflow_overlay(size, width(&narrow)).is_empty(), "a run that fits reveals nothing");

        let mut unmeasured = measured_label("mmmmmmmmmmmmmmmm", 40.0);
        unmeasured.state.set_hovered(true);
        assert!(
            unmeasured.overflow_overlay(size, None).is_empty(),
            "an unmeasured run has no width to raise a plate to",
        );
    }

    #[test]
    fn the_reveal_plate_lays_the_margin_its_own_width_accounts_for() {
        // The plate is `pad + longest + pad` wide, so a run drawn at the
        // label's own alignment origin — `0.0` for the default `Start` — sits
        // under the one-pixel ring on the left while the width still reserves
        // a margin there.
        let theme = &Theme::DEFAULT;
        let size = theme.label_size_pixels;
        let mut label = measured_label("mmmmmmmmmmmmmmmm", 40.0);
        label.state.set_hovered(true);

        let items = label.overflow_overlay(size, label.measured_width(size));
        let (plate_width, run_x) = items.iter().fold((0.0_f32, None), |(width, x), item| match item {
            WidgetDrawItem::Quad { width: plate, .. } => (width.max(*plate), x),
            WidgetDrawItem::Text { x: run, .. } => (width, x.or(Some(*run))),
            _ => (width, x),
        });
        let run_x = run_x.expect("the plate carries the run it reveals");

        assert_eq!(run_x, theme.pad, "the run starts one pad inside the plate, clear of its ring");
        let longest = label.measured_width(size).expect("measured");
        assert_eq!(plate_width - (run_x + longest), run_x, "the margin at each end of the plate is the same one");
    }

    #[test]
    fn a_run_wider_than_its_frame_is_cut_with_the_mark_that_says_so() {
        // Tripwire: **the label cuts its own run; the slot clip is only the
        // backstop.** A clip cuts mid-glyph, so a stat row in a column
        // narrower than its words read `Physical damage mitigat` — a name
        // that ends oddly rather than a name that was too long. The list and
        // the button already elide; this was the last widget that did not.
        //
        // The two things the cut must not touch are asserted with it: the
        // reported width stays the *whole* run's, because that is what a
        // consumer sizes a column from and what the reveal fires on, and the
        // reveal still carries the whole text.
        let size = Theme::DEFAULT.label_size_pixels;
        let mut label = measured_label("mmmmmmmmmmmmmmmm", 40.0);
        let measured = label.measured_width(size);
        let (run, drawn) = label.drawn_run(size, measured);

        assert!(measured.expect("measured") > label.frame.width, "the fixture's run really does overflow");
        assert_ne!(run, label.text, "the whole run was pushed at a frame that cannot hold it");
        assert!(run.ends_with(ELLIPSIS), "a cut run carries the mark saying it was cut: {run:?}");
        assert!(
            drawn.expect("measured") <= label.frame.width,
            "the drawn run {run:?} is still wider than the frame it was cut to",
        );
        assert_eq!(
            label.measured_width(size),
            measured,
            "the reported width is the whole run's — a column sizes to the words, not to the cut",
        );

        label.state.set_hovered(true);
        assert!(
            !label.overflow_overlay(size, measured).is_empty(),
            "the reveal is what makes the cut text readable, so it must still fire",
        );

        // A run that fits is untouched, and an unmeasured one is drawn whole
        // rather than cut against a width nobody has yet.
        let fits = measured_label("mm", 200.0);
        assert_eq!(fits.drawn_run(size, fits.measured_width(size)), (String::from("mm"), fits.measured_width(size)));
        assert_eq!(label.drawn_run(size, None), (label.text.clone(), None));
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
