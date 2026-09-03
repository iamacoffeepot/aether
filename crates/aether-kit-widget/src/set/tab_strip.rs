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
//! Those widths are then **fitted into the strip's own frame**, because a
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
use aether_text::FontMetricsResult;

use crate::set::{
    WidgetDefaults, accept_font_metrics_result, apply_text_theme, centered_text_x, clamp_option_index, elide_to_width,
    even_split_widths, fit_row_widths, measured_text_width, pump_text_font_metrics, push_control_outlines, quad,
    release_left, reply_if_hidden, slot_at_local_x, text_origin_y,
};
use crate::state::{InteractionState, emit_state_changed};
use crate::text_edit::FontMetricsAdapter;
use crate::theme::{SetTheme, Theme, ThemeState};
use crate::{
    Collect, HoverLost, SetWidgetState, TabSelected, TabStripConfig, WidgetControlState, WidgetDrawItem,
    WidgetDrawList, WidgetFrame,
};

/// Thickness, in pixels, of the selected tab's bottom-edge underline — the
/// second of the two marks the selected tab carries.
const UNDERLINE_THICKNESS: f32 = 2.0;

/// The tab strip widget. Holds its labels and selected tab plus the cached
/// theme / frame, the per-tab pointer state, and the single-flight
/// font-metrics adapter the tab widths are measured against.
pub struct TabStripWidget {
    labels: Vec<String>,
    selected_index: usize,
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
        let gap = self.theme.space(1);
        self.natural_tab_widths().map_or_else(
            || even_split_widths(self.labels.len(), self.frame.width, gap),
            |natural| fit_row_widths(natural, self.frame.width, gap),
        )
    }

    fn tab_at_pointer_x(&self, pointer_x: f32) -> Option<usize> {
        slot_at_local_x(&self.tab_widths(), self.theme.space(1), pointer_x - self.frame.x)
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
        let intrinsic = self.natural_row_width().map(|width| [width, self.theme.row_height]);
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { intrinsic, items: self.draw_items(), overlay: Vec::new() });
        }
    }
}

impl TabStripWidget {
    /// The strip's local draw: one raised tab per label, an underline under the
    /// selected one, and the focus / validation outlines.
    fn draw_items(&self) -> Vec<WidgetDrawItem> {
        let height = self.frame.height;
        let gap = self.theme.space(1);
        let size = self.theme.label_size_pixels;
        let text_y = text_origin_y(0.0, height, size);
        let metrics = self.font_metrics.resolved();

        let mut items = Vec::new();
        let mut left = 0.0;
        for (index, (label, tab_width)) in self.labels.iter().zip(self.tab_widths()).enumerate() {
            let selected = index == self.selected_index;
            let theme_state = self.tab_theme_state(index);
            items.push(quad(left, 0.0, tab_width, height, self.theme.fill(self.theme.surface_raised, theme_state)));

            if selected {
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

            // The run the tab actually has room for, centered in it. A tab at
            // its natural width holds its whole label and centering puts
            // exactly one pad either side; a tab the fit shrank holds an
            // elided label and centers that, so the margins stay equal
            // whatever width the tab ended up with. The interim even split has
            // no measurement to elide or center against, so it stays
            // left-padded until the widths settle.
            let (run, run_x) = metrics.map_or_else(
                || (label.clone(), left + self.theme.pad),
                |metrics| {
                    let measure = |run: &str| measured_text_width(metrics, run, size);
                    let run = elide_to_width(label, self.theme.pad.mul_add(-2.0, tab_width), measure);
                    let run_x = left + centered_text_x(tab_width, measure(&run));
                    (run, run_x)
                },
            );
            if !run.is_empty() {
                items.push(WidgetDrawItem::Text {
                    x: run_x,
                    y: text_y,
                    font_id: self.theme.font_id,
                    text: run,
                    size_pixels: size,
                    color: self.theme.fill(self.theme.text_primary, theme_state),
                    clip: None,
                });
            }

            left += tab_width + gap;
        }

        push_control_outlines(&mut items, self.frame.width, height, &self.state, &self.theme);
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use aether_kinds::{CachedFontMetrics, FontMetrics, GlyphAdvance};
    use alloc::vec;

    use crate::set::ELLIPSIS;

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

    fn strip(labels: usize, selected_index: usize) -> TabStripWidget {
        TabStripWidget {
            labels: (0..labels).map(|index| format!("tab-{index}")).collect(),
            selected_index,
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
