// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value, clippy::cast_precision_loss)]

//! Horizontal exclusive segmented control (issue 2926).

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::keycode::{KEY_LEFT, KEY_RIGHT};
use aether_kinds::mouse_button;
use aether_kinds::{Key, MouseButton, MouseButtonRelease, MouseMove};

use crate::set::{clamp_option_index, push_control_outlines, quad, release_left, reply_if_hidden, text_origin_y};
use crate::state::{InteractionState, emit_state_changed};
use crate::theme::{SetTheme, Theme, ThemeState};
use crate::{
    Collect, FocusGained, FocusLost, HoverGained, HoverLost, SegmentedConfig, SegmentedSelected, SetWidgetState,
    WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame,
};

#[derive(Debug, Clone, Copy)]
enum SegmentDirection {
    Previous,
    Next,
}

/// A horizontal row of equal-width exclusive choices.
pub struct SegmentedWidget {
    options: Vec<String>,
    selected: usize,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    pressed_segment: Option<usize>,
    hovered_segment: Option<usize>,
}

impl SegmentedWidget {
    fn segment_at_local_x(local_x: f32, width: f32, option_count: usize) -> Option<usize> {
        if option_count == 0
            || !local_x.is_finite()
            || !width.is_finite()
            || width <= 0.0
            || local_x < 0.0
            || local_x >= width
        {
            return None;
        }
        let segment_width = width / option_count as f32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let index = (local_x / segment_width) as usize;
        (index < option_count).then_some(index)
    }

    fn segment_at_pointer_x(&self, pointer_x: f32) -> Option<usize> {
        Self::segment_at_local_x(pointer_x - self.frame.x, self.frame.width, self.options.len())
    }

    fn select_at(&mut self, pointer_x: f32) -> Option<usize> {
        if !self.state.can_mutate() {
            return None;
        }
        let segment = self.segment_at_pointer_x(pointer_x)?;
        self.pressed_segment = Some(segment);
        if segment == self.selected {
            return None;
        }
        self.selected = segment;
        Some(segment)
    }

    fn step(&mut self, direction: SegmentDirection) -> Option<usize> {
        if !self.state.can_mutate() || self.options.is_empty() {
            return None;
        }
        let next = match direction {
            SegmentDirection::Previous => self.selected.saturating_sub(1),
            SegmentDirection::Next => (self.selected + 1).min(self.options.len() - 1),
        };
        if next == self.selected {
            return None;
        }
        self.selected = next;
        Some(next)
    }

    fn adopt_control_state(&mut self, next: WidgetControlState) -> bool {
        if !self.state.replace(next) {
            return false;
        }
        if !self.state.can_mutate() {
            self.pressed_segment = None;
        }
        if !self.state.is_available() {
            self.hovered_segment = None;
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
            parent.send(&SegmentedSelected { index });
        }
    }
}

/// A segmented widget. Spawned inline by a panel root with a
/// [`SegmentedConfig`]; reports [`SegmentedSelected`] on selection changes.
#[actor(instanced)]
impl WasmActor for SegmentedWidget {
    type Config = SegmentedConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.segmented";

    fn init(config: SegmentedConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let selected = clamp_option_index(config.initial_index, config.options.len());
        Ok(Self {
            options: config.options,
            selected,
            theme: config.theme,
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            state: InteractionState::new(config.state),
            pressed_segment: None,
            hovered_segment: None,
        })
    }

    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: SegmentedConfig) {
        self.selected = clamp_option_index(config.initial_index, config.options.len());
        self.options = config.options;
        self.theme = config.theme;
        self.pressed_segment = None;
        self.hovered_segment = None;
        self.apply_control_state(ctx, config.state);
    }

    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        self.apply_control_state(ctx, set.state);
    }

    #[handler::single]
    fn on_set_theme(&mut self, _ctx: &mut WasmCtx<'_>, set: SetTheme) {
        self.theme = set.theme;
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
    fn on_focus_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: FocusLost) {
        self.state.lose_focus();
        self.pressed_segment = None;
    }

    #[handler::single]
    fn on_hover_gained(&mut self, _ctx: &mut WasmCtx<'_>, _gained: HoverGained) {
        self.state.set_hovered(true);
    }

    #[handler::single]
    fn on_hover_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: HoverLost) {
        self.state.set_hovered(false);
        self.hovered_segment = None;
    }

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
        release_left(&mut self.pressed_segment, None, release);
    }

    #[handler::single]
    fn on_mouse_move(&mut self, _ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        if self.state.is_available() {
            self.hovered_segment = self.segment_at_pointer_x(moved.x);
        }
    }

    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        let direction = match key.code {
            KEY_LEFT => SegmentDirection::Previous,
            KEY_RIGHT => SegmentDirection::Next,
            _ => return,
        };
        if let Some(selected) = self.step(direction) {
            Self::emit(ctx, selected);
        }
    }

    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        let width = self.frame.width;
        let height = self.frame.height;
        let mut items = Vec::new();
        if !self.options.is_empty() {
            let segment_width = width / self.options.len() as f32;
            for (index, option) in self.options.iter().enumerate() {
                let x = index as f32 * segment_width;
                let selected = index == self.selected;
                let base = if selected {
                    self.theme.accent
                } else {
                    self.theme.surface_raised
                };
                let theme_state = if self.pressed_segment == Some(index) {
                    ThemeState::Pressed
                } else if self.hovered_segment == Some(index) {
                    ThemeState::Hover
                } else if !self.state.control().enabled {
                    ThemeState::Disabled
                } else {
                    ThemeState::Normal
                };
                items.push(quad(x, 0.0, segment_width, height, self.theme.fill(base, theme_state)));
                if index > 0 {
                    items.push(quad(x, 0.0, 1.0, height, self.theme.fill(self.theme.outline, theme_state)));
                }
                if !option.is_empty() {
                    let size = self.theme.label_size_pixels;
                    items.push(WidgetDrawItem::Text {
                        x: x + self.theme.pad,
                        y: text_origin_y(0.0, height, size),
                        font_id: self.theme.font_id,
                        text: option.clone(),
                        size_pixels: size,
                        color: self.theme.fill(
                            if selected {
                                self.theme.accent_text
                            } else {
                                self.theme.text_primary
                            },
                            theme_state,
                        ),
                        clip: None,
                    });
                }
            }
        }
        push_control_outlines(&mut items, width, height, &self.state, &self.theme);
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { intrinsic: None, items });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segmented(options: usize, selected: usize) -> SegmentedWidget {
        SegmentedWidget {
            options: (0..options).map(|index| format!("option-{index}")).collect(),
            selected,
            theme: Theme::DEFAULT,
            frame: WidgetFrame { x: 10.0, y: 0.0, width: 90.0, height: 24.0 },
            state: InteractionState::new(WidgetControlState::default()),
            pressed_segment: None,
            hovered_segment: None,
        }
    }

    #[test]
    fn empty_and_boundary_hits_are_explicit() {
        assert_eq!(SegmentedWidget::segment_at_local_x(0.0, 90.0, 0), None);
        assert_eq!(SegmentedWidget::segment_at_local_x(-0.1, 90.0, 3), None);
        assert_eq!(SegmentedWidget::segment_at_local_x(0.0, 90.0, 3), Some(0));
        assert_eq!(SegmentedWidget::segment_at_local_x(29.99, 90.0, 3), Some(0));
        assert_eq!(SegmentedWidget::segment_at_local_x(30.0, 90.0, 3), Some(1));
        assert_eq!(SegmentedWidget::segment_at_local_x(60.0, 90.0, 3), Some(2));
        assert_eq!(SegmentedWidget::segment_at_local_x(90.0, 90.0, 3), None);
    }

    #[test]
    fn pointer_bucketing_and_arrow_steps_clamp_at_the_ends() {
        let mut control = segmented(3, 0);
        assert_eq!(control.select_at(75.0), Some(2));
        assert_eq!(control.step(SegmentDirection::Next), None, "right at the end is clamped");
        assert_eq!(control.step(SegmentDirection::Previous), Some(1));
        assert_eq!(control.step(SegmentDirection::Previous), Some(0));
        assert_eq!(control.step(SegmentDirection::Previous), None, "left at the start is clamped");
    }

    #[test]
    fn initial_index_clamps_for_nonempty_and_empty_options() {
        assert_eq!(clamp_option_index(9, 3), 2);
        assert_eq!(clamp_option_index(9, 0), 0);
    }

    #[test]
    fn unavailable_and_read_only_states_block_pointer_and_key_mutation() {
        let mut control = segmented(3, 0);
        let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
        assert!(control.adopt_control_state(read_only));
        assert_eq!(control.select_at(75.0), None);
        assert_eq!(control.step(SegmentDirection::Next), None);
        assert_eq!(control.selected, 0);

        let disabled = WidgetControlState { enabled: false, ..WidgetControlState::default() };
        assert!(control.adopt_control_state(disabled));
        assert_eq!(control.select_at(75.0), None);
        assert_eq!(control.selected, 0);
    }
}
