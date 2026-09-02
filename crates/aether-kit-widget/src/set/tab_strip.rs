// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The tab strip: one row of tabs selecting one of several parallel content
//! sets.
//!
//! Each tab is sized to its label plus padding — never equal thirds of the
//! row — and the selected tab is marked twice, by the selection role and an
//! underline, so it is prominent at a glance. A press selects; a focused
//! Left/Right moves the selection and clamps at the ends. The strip owns
//! nothing but the choice: which content the selected tab shows is the
//! root's business.
//!
//! Sizing a tab to its label needs the label's real width, so the strip
//! drives the same single-flight
//! [`FontMetricsRequest`](aether_text::FontMetricsRequest) the text controls
//! do. Everything downstream of the widths — the hit buckets as much as the
//! draw — reads them from one place, so a press always lands in the tab the
//! reader sees under the pointer.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::keycode::{KEY_LEFT, KEY_RIGHT};
use aether_kinds::mouse_button;
use aether_kinds::{Key, MouseButton, MouseButtonRelease, MouseMove};
use aether_text::FontMetricsResult;

use crate::set::{
    WidgetDefaults, accept_font_metrics_result, apply_text_theme, clamp_option_index, even_split_widths,
    measured_text_width, pump_text_font_metrics, push_control_outlines, quad, release_left, reply_if_hidden,
    slot_at_local_x, text_origin_y,
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
    /// Per-tab pixel widths, left to right: each label's measured width plus
    /// one `pad` either side, so padding a label off its tab's left edge *is*
    /// centering it.
    ///
    /// Until the metrics resolve there is no honest width to lay out from, so
    /// the strip splits the row evenly as an interim — the layout that holds
    /// for the frame or two between the first `Collect` and the metrics
    /// reply, never the intended look. Every tab reflows to its own label the
    /// moment the measurement lands.
    fn tab_widths(&self) -> Vec<f32> {
        let size = self.theme.label_size_pixels;
        let metrics = self.font_metrics.resolved();
        let measured: Option<Vec<f32>> = self
            .labels
            .iter()
            .map(|label| metrics.map(|metrics| self.theme.pad.mul_add(2.0, measured_text_width(metrics, label, size))))
            .collect();
        measured.unwrap_or_else(|| even_split_widths(self.labels.len(), self.frame.width, self.theme.space(1)))
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

    /// Reply the strip's local draw: one filled tab per label, the selected
    /// one in the selection role under an underline, and the focus /
    /// validation outlines.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        let height = self.frame.height;
        let gap = self.theme.space(1);
        let size = self.theme.label_size_pixels;
        let text_y = text_origin_y(0.0, height, size);

        let mut items = Vec::new();
        let mut left = 0.0;
        for (index, (label, tab_width)) in self.labels.iter().zip(self.tab_widths()).enumerate() {
            let selected = index == self.selected_index;
            let theme_state = self.tab_theme_state(index);
            let base = if selected {
                self.theme.selection
            } else {
                self.theme.surface_raised
            };
            items.push(quad(left, 0.0, tab_width, height, self.theme.fill(base, theme_state)));

            if selected {
                // The second mark. The fill alone reads as "lit"; the underline
                // says "this one", and survives a theme whose selection sits
                // close to its raised surface.
                items.push(quad(
                    left,
                    height - UNDERLINE_THICKNESS,
                    tab_width,
                    UNDERLINE_THICKNESS,
                    self.theme.fill(self.theme.text_primary, theme_state),
                ));
            }

            if !label.is_empty() {
                let ink = if selected {
                    self.theme.selection_text
                } else {
                    self.theme.text_primary
                };
                items.push(WidgetDrawItem::Text {
                    // A measured tab is its label plus one pad each side, so
                    // the padded origin centers the label; the interim even
                    // split leaves it left-padded until the widths settle.
                    x: left + self.theme.pad,
                    y: text_y,
                    font_id: self.theme.font_id,
                    text: label.clone(),
                    size_pixels: size,
                    color: self.theme.fill(ink, theme_state),
                    clip: None,
                });
            }

            left += tab_width + gap;
        }

        push_control_outlines(&mut items, self.frame.width, height, &self.state, &self.theme);
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { intrinsic: None, items, overlay: Vec::new() });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
