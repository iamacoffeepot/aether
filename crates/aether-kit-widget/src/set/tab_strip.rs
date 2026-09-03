// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The tab strip: one row of tabs selecting one of several parallel content
//! sets.
//!
//! Each tab is sized to its label plus padding — never equal thirds of the
//! row — and the selected tab is marked by an underline along its bottom
//! edge, nothing more: every tab keeps the same raised fill, so the strip
//! reads as a row of places with one marked rather than a row of buttons with
//! one lit, and hover stays the only fill change the pointer causes. A press
//! selects; a focused Left/Right moves the selection and clamps at the ends.
//! The strip owns nothing but the choice: which content the selected tab
//! shows is the root's business.
//!
//! Sizing a tab to its label needs the label's real width, so the strip
//! drives the same single-flight
//! [`FontMetricsRequest`](aether_text::FontMetricsRequest) the text controls
//! do. Everything downstream of the widths — the hit buckets as much as the
//! draw — reads them from one place, so a press always lands in the tab the
//! reader sees under the pointer.
//!
//! Those chips are one of the two shapes a strip takes
//! ([`TabStripConfig::style`]). The other is [`TabStripStyle::Filled`] —
//! Material 3's primary tabs, and the owner's round-8 note 14: "the tab
//! buttons are good but they don't feel like typical tabs … like they aren't
//! small buttons in the section but buttons that take the space and feel more
//! dominant." A filled strip divides its whole frame between its tabs with
//! nothing between them — each keeping its own label plus its pads and the
//! leftover shared equally, so a row with room for every word cuts none of
//! them and the widest tab is the first to give width up when the room runs
//! out — draws no chrome under any of them, marks the
//! current one with an accent underline the width of its tab, and runs a
//! hairline rule in the outline role under the row — so the strip *is* the
//! top edge of the content it switches rather than a set of controls placed
//! on it. It asks for no width of its own: it takes the frame it is given.
//!
//! The chips' widths are then **fitted into the strip's own frame**, because a
//! strip in a resizable pane is regularly handed less room than its tabs ask
//! for. Laying them out at their natural widths anyway does not make the row
//! wider — it runs the last tab off the right edge for the root's slot clip
//! to slice, and the reader sees that one tab short of its right-hand padding
//! while every tab before it looks correct. So the widest tabs give up the
//! shortfall instead, each label is elided into the width its tab got, and
//! the run is centered there: the padding either side of a label is equal on
//! every tab at every strip width. The strip also reports the row it *wanted*
//! as its intrinsic size, so a layout can size the slot and skip the fit
//! entirely.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::keycode::{KEY_LEFT, KEY_RIGHT};
use aether_kinds::mouse_button;
use aether_kinds::{Key, MouseButton, MouseButtonRelease, MouseMove};
use aether_math::Rgba;
use aether_text::FontMetricsResult;

use crate::set::{
    WidgetDefaults, accept_font_metrics_result, apply_text_theme, centered_text_x, clamp_option_index, elide_to_width,
    even_split_widths, fit_row_widths, measured_text_width, pointer_wash, pump_text_font_metrics,
    push_control_outlines, quad, release_left, reply_if_hidden, slot_at_local_x, spread_row_widths, text_origin_y,
};
use crate::state::{InteractionState, emit_state_changed};
use crate::text_edit::FontMetricsAdapter;
use crate::theme::{SetTheme, Theme, ThemeState};
use crate::{
    Collect, HoverLost, SetWidgetState, TabSelected, TabStripConfig, TabStripStyle, WidgetControlState, WidgetDrawItem,
    WidgetDrawList, WidgetFrame,
};

/// Thickness, in pixels, of the selected tab's bottom-edge underline — the
/// second of the two marks the selected tab carries.
const UNDERLINE_THICKNESS: f32 = 2.0;

/// Slack, in pixels, the elision budget allows for the round trip through the
/// layout arithmetic.
///
/// A tab's natural width is `2·pad + run`, and taking the pads back off it
/// does not land on exactly `run` again in binary floating point. Charged
/// against the exact remainder, a tab sized to its own label elides it by a
/// fraction of a pixel — `Build` came back as `Bu…` in a tab laid out at
/// `Build`'s own width. Half a pixel is far under a glyph and far over the
/// error, so the budget carries it.
const FIT_SLACK: f32 = 0.5;

/// Thickness, in pixels, of the hairline a filled strip rules its whole
/// bottom edge with — the line that makes the row the content's top edge
/// rather than a band floating above it.
const RULE_THICKNESS: f32 = 1.0;

/// The tab strip widget. Holds its labels and selected tab plus the cached
/// theme / frame, the per-tab pointer state, and the single-flight
/// font-metrics adapter the tab widths are measured against.
pub struct TabStripWidget {
    labels: Vec<String>,
    selected_index: usize,
    /// Which of the two shapes the row is drawn in.
    style: TabStripStyle,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    pressed_tab: Option<usize>,
    hovered_tab: Option<usize>,
    /// Single-flight exact metrics for the active theme font.
    font_metrics: FontMetricsAdapter,
}

impl TabStripWidget {
    /// The width each tab asks for, left to right: its label's measured width
    /// plus one `pad` either side. `None` until the theme font's metrics
    /// resolve.
    fn natural_tab_widths(&self) -> Option<Vec<f32>> {
        let size = self.theme.label_size_pixels;
        let metrics = self.font_metrics.resolved()?;
        Some(
            self.labels
                .iter()
                .map(|label| self.theme.pad.mul_add(2.0, measured_text_width(metrics, label, size)))
                .collect(),
        )
    }

    /// The width the whole strip asks for: every tab at its natural width with
    /// one `gap` between them. What a layout needs to size the strip's slot so
    /// no tab is ever shrunk.
    fn natural_row_width(&self) -> Option<f32> {
        self.natural_tab_widths().map(|widths| {
            #[allow(clippy::cast_precision_loss)]
            let gaps = (widths.len().max(1) - 1) as f32;
            gaps.mul_add(self.theme.space(1), widths.iter().sum())
        })
    }

    /// Per-tab pixel widths, left to right: the natural widths fitted into the
    /// strip's own frame, so padding a label off its tab's left edge *is*
    /// centering it and no tab is laid out past the frame's right edge.
    ///
    /// The fit is what the owner's note was about. A strip narrower than its
    /// tabs used to lay them out at their natural widths regardless, which
    /// does not widen the strip — it runs the last tab off the right edge for
    /// the root's slot clip to slice, so `Search` alone lost the padding to
    /// the right of its run while every tab before it looked right.
    /// [`fit_row_widths`] shrinks the widest tabs instead, and the draw elides
    /// each label into the width its tab actually got.
    ///
    /// Until the metrics resolve there is no honest width to lay out from, so
    /// the strip splits the row evenly as an interim — the layout that holds
    /// for the frame or two between the first `Collect` and the metrics
    /// reply, never the intended look. Every tab reflows to its own label the
    /// moment the measurement lands.
    fn tab_widths(&self) -> Vec<f32> {
        let gap = self.tab_gap();
        match self.style {
            // Filled tabs partition the frame, but they partition it by what
            // is *in* them: each keeps its own label plus its pads and the
            // leftover is shared equally, so a strip with room for every word
            // cuts none of them. An even split does not have that property —
            // at the studio's pane it gave `Build` three times the width of
            // its word while `Equipment` elided in the share beside it. Only
            // when the labels do not fit at all does the water-fill shrink
            // the widest, which is when the longest label is also the right
            // one to cut. The even split survives as the interim, for the
            // frame or two before the measurement lands.
            TabStripStyle::Filled => self.natural_tab_widths().map_or_else(
                || even_split_widths(self.labels.len(), self.frame.width, gap),
                |natural| spread_row_widths(natural, self.frame.width, gap),
            ),
            TabStripStyle::Chips => self.natural_tab_widths().map_or_else(
                || even_split_widths(self.labels.len(), self.frame.width, gap),
                |natural| fit_row_widths(natural, self.frame.width, gap),
            ),
        }
    }

    /// The space between two tabs. Chips are separate targets with a gap that
    /// belongs to neither; filled tabs divide one bar, so there is nothing
    /// between them and every pixel of the strip belongs to a tab.
    fn tab_gap(&self) -> f32 {
        match self.style {
            TabStripStyle::Chips => self.theme.space(1),
            TabStripStyle::Filled => 0.0,
        }
    }

    /// The size the strip reports up.
    ///
    /// A chip row asks for the width at which no tab has to shrink. A filled
    /// row asks for no width at all — it divides whatever frame it is given,
    /// so there is nothing for a layout to size a slot to; the non-finite
    /// component is how [`WidgetDrawList::intrinsic`] says "nothing on this
    /// axis", and the row height is still worth reporting.
    fn intrinsic(&self) -> Option<[f32; 2]> {
        match self.style {
            TabStripStyle::Chips => self.natural_row_width().map(|width| [width, self.theme.row_height]),
            TabStripStyle::Filled => Some([f32::NAN, self.theme.row_height]),
        }
    }

    fn tab_at_pointer_x(&self, pointer_x: f32) -> Option<usize> {
        slot_at_local_x(&self.tab_widths(), self.tab_gap(), pointer_x - self.frame.x)
    }

    /// Select the tab under the pointer. Returns the new index only when the
    /// selection actually changed — a press on the selected tab still marks it
    /// pressed, and still reports nothing.
    fn select_at(&mut self, pointer_x: f32) -> Option<usize> {
        if !self.state.can_mutate() {
            return None;
        }
        let tab = self.tab_at_pointer_x(pointer_x)?;
        self.pressed_tab = Some(tab);
        if tab == self.selected_index {
            return None;
        }
        self.selected_index = tab;
        Some(tab)
    }

    /// Move the selection one tab, clamping at either end. Returns the new
    /// index only when the clamp did not swallow the move.
    fn step(&mut self, forward: bool) -> Option<usize> {
        if !self.state.can_mutate() || self.labels.is_empty() {
            return None;
        }
        let next = if forward {
            (self.selected_index + 1).min(self.labels.len() - 1)
        } else {
            self.selected_index.saturating_sub(1)
        };
        if next == self.selected_index {
            return None;
        }
        self.selected_index = next;
        Some(next)
    }

    fn tab_theme_state(&self, index: usize) -> ThemeState {
        if self.pressed_tab == Some(index) {
            ThemeState::Pressed
        } else if self.hovered_tab == Some(index) {
            ThemeState::Hover
        } else if self.state.control().enabled {
            ThemeState::Normal
        } else {
            ThemeState::Disabled
        }
    }

    fn adopt_control_state(&mut self, next: WidgetControlState) -> bool {
        if !self.state.replace(next) {
            return false;
        }
        if !self.state.can_mutate() {
            self.pressed_tab = None;
        }
        if !self.state.is_available() {
            self.hovered_tab = None;
        }
        true
    }

    fn apply_control_state(&mut self, ctx: &WasmCtx<'_>, next: WidgetControlState) {
        if self.adopt_control_state(next) {
            emit_state_changed(ctx, &self.state);
        }
    }

    fn emit(ctx: &WasmCtx<'_>, selected: usize) {
        if let Some(parent) = ctx.parent() {
            #[allow(clippy::cast_possible_truncation)]
            let index = selected as u32;
            parent.send(&TabSelected { index });
        }
    }
}

impl WidgetDefaults for TabStripWidget {
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
        self.pressed_tab = None;
    }
}

/// A tab strip. Spawned inline by a panel root with a [`TabStripConfig`];
/// reports [`TabSelected`] on a change of tab.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send
/// it its `TabStripConfig` again to replace the labels or the selection in
/// place.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for TabStripWidget {
    type Config = TabStripConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.tab_strip";

    fn init(config: TabStripConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let desired_font_id = config.theme.font_id;
        Ok(TabStripWidget {
            selected_index: clamp_option_index(config.initial_index, config.labels.len()),
            labels: config.labels,
            style: config.style,
            theme: config.theme,
            state: InteractionState::new(config.state),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            pressed_tab: None,
            hovered_tab: None,
            font_metrics: FontMetricsAdapter::new(desired_font_id),
        })
    }

    /// Kick off the font-metrics request for the initial theme font; the tab
    /// widths depend on it.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    /// Replace the labels / selection / theme in place from a re-sent config,
    /// and request metrics for the new theme font.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: TabStripConfig) {
        self.selected_index = clamp_option_index(config.initial_index, config.labels.len());
        self.labels = config.labels;
        self.style = config.style;
        self.font_metrics.set_desired(config.theme.font_id);
        self.theme = config.theme;
        self.pressed_tab = None;
        self.hovered_tab = None;
        self.apply_control_state(ctx, config.state);
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    /// Update external availability without changing the tabs.
    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        self.apply_control_state(ctx, set.state);
    }

    /// Restyle: adopt the fanned theme and request metrics for its font.
    #[handler::single]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        apply_text_theme(ctx, &mut self.font_metrics, &mut self.theme, set.theme);
    }

    /// Install a font-metrics reply; the next `Collect` lays the tabs out
    /// against their real label widths.
    #[handler::single]
    fn on_font_metrics_result(&mut self, ctx: &mut WasmCtx<'_>, result: FontMetricsResult) {
        accept_font_metrics_result(ctx, &mut self.font_metrics, result);
    }

    /// Leaving the strip clears the per-tab hover as well as the widget's.
    #[handler::single]
    fn on_hover_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: HoverLost) {
        self.state.set_hovered(false);
        self.hovered_tab = None;
    }

    /// A left press selects the tab under the pointer.
    #[handler::single]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if press.button == mouse_button::LEFT
            && let Some(selected) = self.select_at(press.x)
        {
            Self::emit(ctx, selected);
        }
    }

    #[handler::single]
    fn on_mouse_button_release(&mut self, _ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        release_left(&mut self.pressed_tab, None, release);
    }

    #[handler::single]
    fn on_mouse_move(&mut self, _ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        if self.state.is_available() {
            self.hovered_tab = self.tab_at_pointer_x(moved.x);
        }
    }

    /// Left / Right move the selection while the strip holds focus.
    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        let forward = match key.code {
            KEY_LEFT => false,
            KEY_RIGHT => true,
            _ => return,
        };
        if let Some(selected) = self.step(forward) {
            Self::emit(ctx, selected);
        }
    }

    /// Reply the strip's local draw, plus the width its tabs ask for.
    ///
    /// The intrinsic is how a host stops the strip from having to shrink at
    /// all: it is the whole row at its natural widths, so a layout that sizes
    /// the strip's slot to it never hands down a frame the tabs have to be
    /// fitted into.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { intrinsic: self.intrinsic(), items: self.draw_items(), overlay: Vec::new() });
        }
    }
}

impl TabStripWidget {
    /// The strip's local draw in the shape its style asks for, plus the
    /// focus / validation outlines both shapes share.
    fn draw_items(&self) -> Vec<WidgetDrawItem> {
        let mut items = match self.style {
            TabStripStyle::Chips => self.chip_items(),
            TabStripStyle::Filled => self.filled_items(),
        };
        push_control_outlines(&mut items, self.frame.width, self.frame.height, &self.state, &self.theme);
        items
    }

    /// The run tab `index` has room for and the local x it is drawn at, in
    /// the cell that starts at `left` and is `tab_width` wide.
    ///
    /// A tab at its natural width holds its whole label and centering puts
    /// exactly one pad either side; a tab that had to give width up holds an
    /// elided label and centers that, so the margins stay equal whatever
    /// width the tab ended up with — an even share of a narrow filled strip
    /// included. The interim even split of a chip row has no measurement to
    /// elide or center against, so it stays left-padded until the widths
    /// settle. `None` when there is nothing to draw.
    fn tab_run(&self, label: &str, left: f32, tab_width: f32) -> Option<(String, f32)> {
        let size = self.theme.label_size_pixels;
        let (run, run_x) = self.font_metrics.resolved().map_or_else(
            || (label.into(), left + self.theme.pad),
            |metrics| {
                let measure = |run: &str| measured_text_width(metrics, run, size);
                let run = elide_to_width(label, self.theme.pad.mul_add(-2.0, tab_width) + FIT_SLACK, measure);
                let run_x = left + centered_text_x(tab_width, measure(&run));
                (run, run_x)
            },
        );
        (!run.is_empty()).then_some((run, run_x))
    }

    /// One label drawn in `ink`, at the row's shared baseline.
    fn label_item(&self, run: String, run_x: f32, ink: Rgba) -> WidgetDrawItem {
        let size = self.theme.label_size_pixels;
        WidgetDrawItem::Text {
            x: run_x,
            y: text_origin_y(0.0, self.frame.height, size),
            font_id: self.theme.font_id,
            text: run,
            size_pixels: size,
            color: ink,
            clip: None,
        }
    }

    /// The chip row: one raised tab per label with a gap between them, and an
    /// underline under the selected one.
    fn chip_items(&self) -> Vec<WidgetDrawItem> {
        let height = self.frame.height;
        let gap = self.tab_gap();

        let mut items = Vec::new();
        let mut left = 0.0;
        for (index, (label, tab_width)) in self.labels.iter().zip(self.tab_widths()).enumerate() {
            let theme_state = self.tab_theme_state(index);
            items.push(quad(left, 0.0, tab_width, height, self.theme.fill(self.theme.surface_raised, theme_state)));

            if index == self.selected_index {
                // The only mark. A tab is a place, not a row of a list: filling
                // the selected one turns the strip into a wall of buttons with
                // one lit, while an underline on a plain tab reads as "you are
                // here" at a glance. Hover keeps its own overlay, so the
                // pointer still says which tab it is over.
                items.push(quad(
                    left,
                    height - UNDERLINE_THICKNESS,
                    tab_width,
                    UNDERLINE_THICKNESS,
                    self.theme.fill(self.theme.text_primary, theme_state),
                ));
            }

            if let Some((run, run_x)) = self.tab_run(label, left, tab_width) {
                items.push(self.label_item(run, run_x, self.theme.fill(self.theme.text_primary, theme_state)));
            }

            left += tab_width + gap;
        }
        items
    }

    /// The filled row (Material 3 primary tabs): the frame divided evenly,
    /// no plate under any tab, a rule under the whole strip, and an accent
    /// underline the width of the current tab.
    ///
    /// The rule goes down first so the underline paints over its own span:
    /// the current tab's mark is a continuation of the content's edge, lit,
    /// rather than a second line beside it. The accent is spent as a *mark*
    /// here and never as a fill — no tab is plated in it — so it still says
    /// "the live one" without claiming to be a button.
    fn filled_items(&self) -> Vec<WidgetDrawItem> {
        let height = self.frame.height;
        let gap = self.tab_gap();

        let mut items = alloc::vec![quad(
            0.0,
            height - RULE_THICKNESS,
            self.frame.width,
            RULE_THICKNESS,
            self.theme.fill(self.theme.outline, self.state.theme_state(false)),
        )];
        let mut left = 0.0;
        for (index, (label, tab_width)) in self.labels.iter().zip(self.tab_widths()).enumerate() {
            let theme_state = self.tab_theme_state(index);
            // No plate to carry the pointer's answer, so the wash is drawn as
            // the tab's whole background instead.
            if let Some(wash) = pointer_wash(&self.theme, theme_state) {
                items.push(quad(left, 0.0, tab_width, height, wash));
            }

            if index == self.selected_index {
                items.push(quad(
                    left,
                    height - UNDERLINE_THICKNESS,
                    tab_width,
                    UNDERLINE_THICKNESS,
                    self.theme.fill(self.theme.accent, theme_state),
                ));
            }

            if let Some((run, run_x)) = self.tab_run(label, left, tab_width) {
                items.push(self.label_item(run, run_x, self.theme.fill(self.theme.text_primary, theme_state)));
            }

            left += tab_width + gap;
        }
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use aether_kinds::{CachedFontMetrics, FontMetrics, GlyphAdvance};
    use alloc::vec;

    use crate::set::{ELLIPSIS, slot_left};

    /// A proportional advance table, so a tab's width is the label's own and
    /// two tabs with the same character count are not the same width.
    /// Advances are in units of a 1000-unit em; at the theme's 14-pixel label
    /// size a 500-unit glyph is 7 pixels.
    fn proportional_metrics() -> CachedFontMetrics {
        CachedFontMetrics::new(&FontMetrics {
            units_per_em: 1000.0,
            ascent: 800.0,
            descent: -200.0,
            line_gap: 0.0,
            default_advance: 500.0,
            advances: vec![
                GlyphAdvance { codepoint: u32::from('i'), advance_units: 200.0 },
                GlyphAdvance { codepoint: u32::from('l'), advance_units: 220.0 },
                GlyphAdvance { codepoint: u32::from('r'), advance_units: 330.0 },
                GlyphAdvance { codepoint: u32::from('q'), advance_units: 560.0 },
                GlyphAdvance { codepoint: u32::from('S'), advance_units: 640.0 },
                GlyphAdvance { codepoint: u32::from('…'), advance_units: 900.0 },
            ],
        })
    }

    /// The studio's own strip, measured: `Build · Skills · Sequences · Tree ·
    /// Library · Search` with the metrics resolved, framed at `width`.
    fn measured_strip(width: f32) -> TabStripWidget {
        let mut font_metrics = FontMetricsAdapter::new(0);
        assert_eq!(font_metrics.take_pending_request(), Some(0), "the strip asks for its theme font once");
        assert!(!font_metrics.accept_reply(Some(proportional_metrics())));
        TabStripWidget {
            labels: ["Build", "Skills", "Sequences", "Tree", "Library", "Search"]
                .into_iter()
                .map(String::from)
                .collect(),
            selected_index: 0,
            style: TabStripStyle::Chips,
            theme: Theme::DEFAULT,
            frame: WidgetFrame { x: 0.0, y: 0.0, width, height: 24.0 },
            state: InteractionState::new(WidgetControlState::default()),
            pressed_tab: None,
            hovered_tab: None,
            font_metrics,
        }
    }

    /// Each tab's drawn cell paired with the run inside it, as `(left edge,
    /// right edge, run left, run right)` — what the reader is looking at when
    /// they judge a tab's padding. The cell is intersected with the strip's
    /// frame the way the root's slot clip intersects it, so a tab laid out
    /// past the frame is measured as the reader sees it, not as it was
    /// requested. A cell too narrow to hold even an ellipsis has no run and
    /// is skipped: nothing is drawn there to be padded unevenly.
    fn drawn_tabs(strip: &TabStripWidget) -> Vec<(f32, f32, f32, f32)> {
        let metrics = strip.font_metrics.resolved().expect("measured");
        let size = strip.theme.label_size_pixels;
        let items = strip.draw_items();
        let runs: Vec<(f32, f32)> = items
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Text { x, text, size_pixels, .. } => {
                    Some((*x, x + measured_text_width(metrics, text, *size_pixels)))
                }
                _ => None,
            })
            .collect();
        assert!(size > 0.0 && runs.len() <= strip.labels.len());
        items
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Quad { x, width, height, .. } if *height == strip.frame.height => {
                    Some((x.max(0.0), (x + width).min(strip.frame.width)))
                }
                _ => None,
            })
            .filter_map(|(left, right)| {
                let run = runs.iter().find(|(run_left, _)| *run_left >= left && *run_left < right)?;
                Some((left, right, run.0, run.1))
            })
            .collect()
    }

    #[test]
    fn every_tab_pads_its_run_equally_however_narrow_the_strip_is() {
        // Tripwire: the owner's round-6 note — "Search tab button padding on
        // right of text isn't symmetric". `Search` is the *last* tab of a
        // strip in a pane too narrow for all six, and a strip that laid its
        // tabs out at their natural widths regardless ran that one tab off
        // the frame's right edge for the root's slot clip to cut, taking its
        // right-hand padding with it. Every tab before it looked correct,
        // which is what made the cause invisible from the screen. The frames
        // below bracket the studio's own pane: wider than the tabs need,
        // exactly what they need, and the widths a dragged-in pane reaches.
        let natural = measured_strip(0.0).natural_row_width().expect("measured");
        for width in [natural + 120.0, natural, natural - 8.0, 300.0, 220.0, 160.0] {
            let strip = measured_strip(width);
            for (index, (left, right, run_left, run_right)) in drawn_tabs(&strip).into_iter().enumerate() {
                let label = &strip.labels[index];
                let left_pad = run_left - left;
                let right_pad = right - run_right;
                assert!(
                    (left_pad - right_pad).abs() < 1.0,
                    "strip {width}: tab {index} {label:?} pads its run {left_pad} on the left and {right_pad} on the \
                     right (cell {left}..{right}, run {run_left}..{run_right})",
                );
            }
        }
    }

    #[test]
    fn a_strip_too_narrow_for_its_tabs_shrinks_the_widest_and_keeps_the_rest() {
        // Tripwire: the shrink has to land on the tabs that caused it. A
        // proportional scale would take pixels off `Tree`, which fits with
        // room to spare, to pay for `Sequences`, which does not — and a strip
        // that shrinks a tab it did not have to is a strip that elides a
        // label it did not have to.
        let natural = measured_strip(300.0).natural_tab_widths().expect("measured");
        let fitted = measured_strip(300.0).tab_widths();
        let (tree, sequences) = (3, 2);
        assert!(fitted[tree] == natural[tree] && fitted[0] == natural[0], "the short tabs keep their own widths");
        assert!(fitted[sequences] < natural[sequences], "the widest tab gives up the shortfall");
        let gaps = 5.0 * Theme::DEFAULT.space(1);
        assert!((fitted.iter().sum::<f32>() + gaps - 300.0).abs() < 1e-3, "the fitted row is exactly the frame");
    }

    #[test]
    fn a_shrunk_tab_elides_its_label_rather_than_letting_the_clip_slice_it() {
        // Tripwire: fitting the cells without cutting the runs to match just
        // moves the slice from the tab's edge to the glyph the text runs
        // past. An ellipsis says a label was cut; a half-drawn glyph says the
        // label ends oddly (the same rule the list's rows follow).
        let strip = measured_strip(300.0);
        let drawn: Vec<String> = strip
            .draw_items()
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(drawn[3], "Tree", "a tab that kept its width keeps its whole label");
        assert!(drawn[2].ends_with(ELLIPSIS) && drawn[2] != "Sequences", "the shrunk tab's label is marked as cut");
    }

    #[test]
    fn the_intrinsic_row_is_the_width_that_needs_no_fit() {
        // Tripwire: a layout sizes the strip's slot from this number, so it
        // has to be exactly the width at which `tab_widths` stops shrinking.
        let natural = measured_strip(0.0).natural_row_width().expect("measured");
        let strip = measured_strip(natural);
        assert_eq!(strip.tab_widths(), measured_strip(natural + 100.0).tab_widths(), "the intrinsic width fits whole");
    }

    /// The studio's own strip in the filled shape, measured, framed at
    /// `width`.
    fn filled_strip(width: f32) -> TabStripWidget {
        TabStripWidget { style: TabStripStyle::Filled, ..measured_strip(width) }
    }

    /// Every full-height quad a strip draws, as `(left, width, colour)` — the
    /// per-tab chrome a reader sees behind the labels.
    fn full_height_quads(strip: &TabStripWidget) -> Vec<(f32, f32, Rgba)> {
        strip
            .draw_items()
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Quad { x, width, height, color, .. } if *height == strip.frame.height => {
                    Some((*x, *width, *color))
                }
                _ => None,
            })
            .collect()
    }

    /// The strip's horizontal bars shorter than a tab: `(top, left, width,
    /// height, colour)`. The rule under the row and the current tab's
    /// underline are both in here.
    fn bars(strip: &TabStripWidget) -> Vec<(f32, f32, f32, f32, Rgba)> {
        strip
            .draw_items()
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Quad { x, y, width, height, color, .. } if *height < strip.frame.height => {
                    Some((*y, *x, *width, *height, *color))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn filled_tabs_take_the_whole_strip_and_leave_no_gap_between_them() {
        // Tripwire: the owner's round-8 note 14 — the tabs should "take the
        // space" rather than sit in it as small buttons. A filled row whose
        // tabs do not reach the frame's right edge is that note unfixed, and
        // a gap between two filled tabs is worse than a look: it is a press
        // in the middle of the bar that selects nothing. (How the width is
        // *divided* is the next test's business, not this one's.)
        for width in [720.0_f32, 400.0, 180.0] {
            let strip = filled_strip(width);
            let widths = strip.tab_widths();
            assert_eq!(widths.len(), strip.labels.len());
            assert!((widths.iter().sum::<f32>() - width).abs() < 1e-3, "strip {width}: {widths:?} is not the frame");
            for index in 0..strip.labels.len() {
                let left = slot_left(&widths, 0.0, index);
                assert_eq!(
                    strip.tab_at_pointer_x(widths[index].mul_add(0.5, left)),
                    Some(index),
                    "the middle of tab {index}"
                );
                assert_eq!(strip.tab_at_pointer_x(left + 0.001), Some(index), "and its very left edge");
            }
        }
    }

    /// The deployed studio's filled strip, measured: the five tabs of the
    /// owner's capture at the display scale they run it at (2×), framed at
    /// `width` physical pixels — so 630 is the ~315-logical-pixel pane the
    /// note came from.
    fn captured_strip(width: f32) -> TabStripWidget {
        let mut font_metrics = FontMetricsAdapter::new(0);
        assert_eq!(font_metrics.take_pending_request(), Some(0), "the strip asks for its theme font once");
        assert!(!font_metrics.accept_reply(Some(proportional_metrics())));
        TabStripWidget {
            labels: ["Build", "Skills", "Target", "Equipment", "Search"].into_iter().map(String::from).collect(),
            selected_index: 0,
            style: TabStripStyle::Filled,
            theme: Theme::DEFAULT.scaled(2.0),
            frame: WidgetFrame { x: 0.0, y: 0.0, width, height: 48.0 },
            state: InteractionState::new(WidgetControlState::default()),
            pressed_tab: None,
            hovered_tab: None,
            font_metrics,
        }
    }

    /// Every tab's drawn run, left to right, at the widths the strip laid out.
    fn drawn_runs(strip: &TabStripWidget) -> Vec<String> {
        let widths = strip.tab_widths();
        strip
            .labels
            .iter()
            .enumerate()
            .map(|(index, label)| {
                strip
                    .tab_run(label, slot_left(&widths, strip.tab_gap(), index), widths[index])
                    .expect("every tab here has room for at least its mark")
                    .0
            })
            .collect()
    }

    #[test]
    fn a_filled_strip_with_room_for_every_word_cuts_none_of_them() {
        // Tripwire: the deployed capture. Dividing the bar *evenly* gave
        // `Build` a share three times wider than its word while `Equipment`
        // elided to `Equipm…` in the share beside it — a strip cutting a
        // label it had room for, which no amount of "it is a filled tab"
        // excuses (§5: nothing is cut). Content first, slack shared: each tab
        // keeps its run plus its pads and the leftover is split equally.
        let strip = captured_strip(630.0);
        let natural = strip.natural_tab_widths().expect("measured");
        assert!(natural.iter().sum::<f32>() < strip.frame.width, "the pane really does have room for all five");

        let widths = strip.tab_widths();
        for (index, label) in strip.labels.iter().enumerate() {
            assert!(widths[index] >= natural[index] - 1e-3, "tab {index} {label:?} was squeezed under its own word");
        }
        assert_eq!(drawn_runs(&strip), strip.labels, "a strip with room for every word cut one of them");

        let slack: Vec<f32> = widths.iter().zip(&natural).map(|(width, natural)| width - natural).collect();
        assert!(slack.windows(2).all(|pair| (pair[0] - pair[1]).abs() < 1e-3), "the leftover is not shared: {slack:?}");
        assert!((widths.iter().sum::<f32>() - strip.frame.width).abs() < 1e-3, "and the row still fills the frame");
        assert!(widths[3] > widths[0], "the longest word takes the widest tab: {widths:?}");
    }

    #[test]
    fn a_filled_strip_short_of_room_shrinks_the_widest_word_first() {
        // Tripwire: the other half of the rule. Once the labels genuinely do
        // not fit, something has to give — and it must be the tab whose word
        // is longest, not whichever one the arithmetic reached last. A short
        // label that fits is never cut to pay for a long one.
        let strip = captured_strip(500.0);
        let natural = strip.natural_tab_widths().expect("measured");
        assert!(natural.iter().sum::<f32>() > strip.frame.width, "this frame is genuinely short of room");

        let widths = strip.tab_widths();
        let runs = drawn_runs(&strip);
        assert_eq!(widths[0], natural[0], "`Build` fits and keeps its own width");
        assert_eq!(runs[0], "Build", "so it is drawn whole");
        assert!(widths[3] < natural[3], "`Equipment`, the longest, gives the shortfall up");
        assert!(runs[3].ends_with(ELLIPSIS), "and says it was cut: {:?}", runs[3]);
        assert!((widths.iter().sum::<f32>() - strip.frame.width).abs() < 1e-3, "the shrunk row still fills the frame");
    }

    #[test]
    fn a_filled_strip_plates_no_tab_and_marks_the_current_one_with_an_accent_underline() {
        // Tripwire: the shape *is* the note. A filled row that kept the
        // chips' raised plate is a row of buttons again; one without the rule
        // under it floats over the content instead of being its top edge; and
        // an underline that is not the current tab's own width says nothing
        // about which tab is live.
        let strip = TabStripWidget { selected_index: 2, ..filled_strip(600.0) };
        let widths = strip.tab_widths();
        assert!(full_height_quads(&strip).is_empty(), "a filled tab carries no chrome of its own at rest");

        let expected_left = slot_left(&widths, 0.0, 2);
        assert_eq!(
            bars(&strip),
            alloc::vec![
                (strip.frame.height - RULE_THICKNESS, 0.0, strip.frame.width, RULE_THICKNESS, strip.theme.outline),
                (
                    strip.frame.height - UNDERLINE_THICKNESS,
                    expected_left,
                    widths[2],
                    UNDERLINE_THICKNESS,
                    strip.theme.accent,
                ),
            ],
            "the rule runs under the whole row and the accent underline is exactly the current tab",
        );
    }

    #[test]
    fn a_filled_tab_too_narrow_for_its_label_elides_it_inside_its_share() {
        // Tripwire: an even share is not a promise of room. `Sequences` in a
        // sixth of a narrow strip does not fit, and a run laid out at its
        // natural width would hang off both ends of its share for the next
        // tab's label to collide with — the same defect the chips' fit
        // fixed, arriving by the other door.
        let strip = filled_strip(240.0);
        let metrics = strip.font_metrics.resolved().expect("measured");
        let size = strip.theme.label_size_pixels;
        let widths = strip.tab_widths();
        let mut elided = 0;
        for (index, label) in strip.labels.iter().enumerate() {
            let left = slot_left(&widths, 0.0, index);
            let (run, run_x) = strip.tab_run(label, left, widths[index]).expect("every share here holds a mark");
            let run_width = measured_text_width(metrics, &run, size);
            assert!(run_x >= left - 1e-3, "tab {index}: {run:?} starts left of its share");
            assert!(run_x + run_width <= left + widths[index] + 1e-3, "tab {index}: {run:?} runs past its share");
            if run != *label {
                assert!(run.ends_with(ELLIPSIS), "tab {index}: a cut label carries the mark that says so: {run:?}");
                elided += 1;
            }
        }
        assert!(elided > 0, "a 240-pixel strip cannot hold all six labels whole; nothing elided");
    }

    #[test]
    fn a_filled_strip_asks_for_no_width_and_a_chip_row_asks_for_its_own() {
        // Tripwire: a host sizes a measured cell from the reported intrinsic.
        // A filled strip that reported the chips' natural row would have the
        // host reserve a content width for tabs that then divide whatever
        // they are given — the two would disagree at every frame — while a
        // chip row that stopped reporting one would be fitted forever.
        let filled = filled_strip(600.0).intrinsic().expect("a filled row still reports its height");
        assert!(!filled[0].is_finite(), "a filled row asks for no width: {filled:?}");
        assert_eq!(filled[1], Theme::DEFAULT.row_height, "and for one row of height");

        let chips = measured_strip(600.0).intrinsic().expect("a measured chip row reports one");
        assert_eq!(chips[0], measured_strip(600.0).natural_row_width().expect("measured"), "chips ask for their row");
    }

    fn strip(labels: usize, selected_index: usize) -> TabStripWidget {
        TabStripWidget {
            labels: (0..labels).map(|index| format!("tab-{index}")).collect(),
            selected_index,
            style: TabStripStyle::Chips,
            theme: Theme::DEFAULT,
            frame: WidgetFrame { x: 10.0, y: 0.0, width: 120.0, height: 24.0 },
            state: InteractionState::new(WidgetControlState::default()),
            pressed_tab: None,
            hovered_tab: None,
            font_metrics: FontMetricsAdapter::new(0),
        }
    }

    #[test]
    fn pointer_selection_and_arrow_steps_clamp_at_the_ends() {
        let mut strip = strip(3, 0);
        assert_eq!(strip.step(false), None, "left at the start is clamped");
        assert_eq!(strip.step(true), Some(1));
        assert_eq!(strip.step(true), Some(2));
        assert_eq!(strip.step(true), None, "right at the end is clamped");
        assert_eq!(strip.step(false), Some(1));
    }

    #[test]
    fn re_selecting_the_pressed_tab_reports_nothing_but_still_presses_it() {
        // Frame x is 10 and the pre-metrics split is even, so local 5 is tab 0.
        let mut strip = strip(3, 0);
        assert_eq!(strip.select_at(15.0), None, "no change, no TabSelected");
        assert_eq!(strip.pressed_tab, Some(0));
    }

    #[test]
    fn the_selected_tab_is_marked_by_its_underline_alone() {
        // Tripwire: filling the selected tab turns a strip of places into a row
        // of buttons with one lit — every tab keeps the raised surface, and the
        // underline is the whole mark. Hover is the only fill the pointer
        // changes, which a re-added selection fill would drown out.
        let strip = strip(3, 1);
        let items = strip.draw_items();
        let (tabs, underlines): (Vec<_>, Vec<_>) = items
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Quad { y, height, color, .. } => Some((*y, *height, *color)),
                WidgetDrawItem::Text { .. } | WidgetDrawItem::TexturedQuad { .. } => None,
            })
            .partition(|(_, height, _)| *height == strip.frame.height);
        assert_eq!(tabs.len(), 3, "one full-height fill per tab");
        assert!(
            tabs.iter().all(|(_, _, color)| *color == strip.theme.surface_raised),
            "the selected tab carries no fill of its own; fills were {tabs:?}",
        );
        assert_eq!(
            underlines,
            vec![(strip.frame.height - UNDERLINE_THICKNESS, UNDERLINE_THICKNESS, strip.theme.text_primary)],
            "exactly one underline, along the selected tab's bottom edge",
        );
        assert!(
            items
                .iter()
                .all(|item| !matches!(item, WidgetDrawItem::Text { color, .. } if *color != strip.theme.text_primary)),
            "every tab label reads in the primary ink; items were {items:?}",
        );
    }

    #[test]
    fn unavailable_and_read_only_states_block_pointer_and_key_selection() {
        let mut strip = strip(3, 0);
        assert!(strip.adopt_control_state(WidgetControlState { read_only: true, ..WidgetControlState::default() }));
        assert_eq!(strip.select_at(100.0), None);
        assert_eq!(strip.step(true), None);
        assert_eq!(strip.selected_index, 0);

        assert!(strip.adopt_control_state(WidgetControlState { enabled: false, ..WidgetControlState::default() }));
        assert_eq!(strip.select_at(100.0), None);
        assert_eq!(strip.selected_index, 0);
    }
}
