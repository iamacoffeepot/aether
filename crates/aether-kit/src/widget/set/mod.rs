//! The concrete widget set: plain `#[actor(instanced)]`
//! types a panel root spawns as inline children and drives by mail in four
//! lanes — config / style / layout-frame data-down, value events-up — over the
//! ADR-0117 draw-compositing protocol.
//!
//! - [`SliderWidget`] — a horizontal value slider, dragged or
//!   arrow-nudged.
//! - [`TextFieldWidget`] — a single-line editable string.
//! - [`TextAreaWidget`] — a multiline measured editor with line scrolling.
//! - [`RadioGroupWidget`] — a vertical list of exclusive options.
//! - [`ButtonWidget`] — a momentary push button.
//! - [`LabelWidget`] — static, non-interactive text.
//! - [`ImageWidget`] — a static, non-interactive borrowed texture.
//! - [`VirtualListWidget`] — a fixed-row virtualized item list.
//! - [`ToggleWidget`] — a boolean switch.
//! - [`SegmentedWidget`] — a horizontal exclusive choice.
//! - [`NumericWidget`] — a typed and steppable bounded number.
//!
//! Each caches its assigned [`WidgetFrame`] rect
//! and its [`Theme`], answers every
//! [`Collect`](crate::widget::Collect) with a
//! [`WidgetDrawList`] drawn in its own local
//! coordinates (colors resolved through [`Theme::fill`]),
//! and reports value changes up to its parent. Widgets never subscribe to
//! input; the root forwards it — see [`super::focus::Focus`] and
//! [`super::panel::WidgetPanel`].
//!
//! Inline children run `wire` after `init`, like loaded actors, but still rely
//! on the root's first `WidgetFrame` for layout and the first `Collect` for
//! their first draw.

pub mod button;
pub mod image;
pub mod label;
pub mod numeric;
pub mod radio;
pub mod segmented;
pub mod slider;
pub mod text_area;
pub mod text_field;
pub mod toggle;
pub mod virtual_list;

pub use button::ButtonWidget;
pub use image::ImageWidget;
pub use label::LabelWidget;
pub use numeric::NumericWidget;
pub use radio::RadioGroupWidget;
pub use segmented::SegmentedWidget;
pub use slider::SliderWidget;
pub use text_area::TextAreaWidget;
pub use text_field::TextFieldWidget;
pub use toggle::ToggleWidget;
pub use virtual_list::VirtualListWidget;

use alloc::vec::Vec;

use aether_actor::WasmCtx;
use aether_kinds::keycode::{KEY_ENTER, KEY_SPACE};
use aether_kinds::{CachedFontMetrics, Modifiers, MouseButton, MouseButtonRelease, mouse_button};
use aether_math::Rgba;
use aether_text::{FontMetricsRequest, FontRef, TextCapability};

use crate::widget::state::{InteractionState, emit_state_changed};
use crate::widget::text_edit::{DisplayedEdit, FontMetricsAdapter, SingleLineLayout, TextEditState};
use crate::widget::theme::{Theme, ThemeState};
use crate::widget::{WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyboardArm {
    Enter,
    Space,
}

#[derive(Debug, Default)]
struct ActivationArms {
    pointer_pressed: bool,
    keyboard_arm: Option<KeyboardArm>,
}

impl ActivationArms {
    fn contains(frame: &WidgetFrame, x: f32, y: f32) -> bool {
        x >= frame.x && x <= frame.x + frame.width && y >= frame.y && y <= frame.y + frame.height
    }

    fn press_pointer(&mut self, frame: &WidgetFrame, eligible: bool, x: f32, y: f32) {
        if eligible && Self::contains(frame, x, y) {
            self.pointer_pressed = true;
        }
    }

    fn press_mouse_button(&mut self, frame: &WidgetFrame, eligible: bool, press: MouseButton) {
        if press.button == mouse_button::LEFT {
            self.press_pointer(frame, eligible, press.x, press.y);
        }
    }

    fn release_pointer(&mut self, frame: &WidgetFrame, eligible: bool, x: f32, y: f32) -> bool {
        let activates = eligible && self.pointer_pressed && Self::contains(frame, x, y);
        self.pointer_pressed = false;
        activates
    }

    fn press_key(&mut self, eligible: bool, code: u32) -> bool {
        if !eligible || self.keyboard_arm.is_some() {
            return false;
        }
        match code {
            KEY_ENTER => {
                self.keyboard_arm = Some(KeyboardArm::Enter);
                true
            }
            KEY_SPACE => {
                self.keyboard_arm = Some(KeyboardArm::Space);
                false
            }
            _ => false,
        }
    }

    fn release_key(&mut self, eligible: bool, code: u32) -> bool {
        match (code, self.keyboard_arm) {
            (KEY_ENTER, Some(KeyboardArm::Enter)) => {
                self.keyboard_arm = None;
                false
            }
            (KEY_SPACE, Some(KeyboardArm::Space)) => {
                self.keyboard_arm = None;
                eligible
            }
            _ => false,
        }
    }

    fn pressed(&self) -> bool {
        self.pointer_pressed || self.keyboard_arm == Some(KeyboardArm::Space)
    }

    fn clear(&mut self) {
        self.pointer_pressed = false;
        self.keyboard_arm = None;
    }
}

fn text_control_theme_state(state: &InteractionState, dragging: bool) -> ThemeState {
    if state.focused() {
        state.supporting_theme_state(dragging)
    } else {
        state.theme_state(dragging)
    }
}

fn apply_text_control_state(
    ctx: &WasmCtx<'_>,
    state: &mut InteractionState,
    edit: &mut TextEditState,
    dragging: &mut bool,
    next: WidgetControlState,
) {
    if state.replace(next) {
        if !state.can_mutate() {
            edit.clear_composition();
        }
        if !state.is_available() {
            *dragging = false;
        }
        emit_state_changed(ctx, state);
    }
}

fn pump_text_font_metrics(ctx: &mut WasmCtx<'_>, font_metrics: &mut FontMetricsAdapter) {
    if let Some(id) = font_metrics.take_pending_request() {
        ctx.actor::<TextCapability>().send(&FontMetricsRequest { font: FontRef::Id(id) });
    }
}

fn apply_text_theme(ctx: &mut WasmCtx<'_>, font_metrics: &mut FontMetricsAdapter, theme: &mut Theme, next: Theme) {
    font_metrics.set_desired(next.font_id);
    *theme = next;
    pump_text_font_metrics(ctx, font_metrics);
}

fn release_left<T>(pressed: &mut T, released: T, release: MouseButtonRelease) {
    if release.button == mouse_button::LEFT {
        *pressed = released;
    }
}

fn arm_text_drag(state: &InteractionState, dragging: &mut bool, press: MouseButton) -> Option<f32> {
    if press.button != mouse_button::LEFT || !state.is_available() {
        return None;
    }
    *dragging = true;
    Some(press.x)
}

fn update_text_modifiers(state: &InteractionState, modifiers: &mut Modifiers, next: Modifiers) {
    if state.is_available() {
        *modifiers = next;
    }
}

fn apply_static_control_state(ctx: &WasmCtx<'_>, state: &mut InteractionState, next: WidgetControlState) {
    if state.replace(next) {
        emit_state_changed(ctx, state);
    }
}

fn clamp_option_index(index: u32, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        (index as usize).min(len - 1)
    }
}

/// Discharge the hidden-widget branch of the always-reply compositing
/// protocol. Hidden controls retain their slot, so every `Collect` must still
/// produce one empty draw-list reply.
pub(super) fn reply_if_hidden(ctx: &WasmCtx<'_>, state: &InteractionState) -> bool {
    if state.is_visible() {
        return false;
    }
    if let Some(parent) = ctx.parent() {
        parent.send(&WidgetDrawList { intrinsic: None, items: Vec::new() });
    }
    true
}

fn reply_with_draw_items(
    ctx: &WasmCtx<'_>,
    state: &InteractionState,
    draw_items: impl FnOnce() -> Vec<WidgetDrawItem>,
) {
    if reply_if_hidden(ctx, state) {
        return;
    }
    if let Some(parent) = ctx.parent() {
        parent.send(&WidgetDrawList { intrinsic: None, items: draw_items() });
    }
}

/// A flat-colored quad in a widget's own local coordinates — the shared
/// constructor the widgets build their chrome from.
pub(crate) fn quad(x: f32, y: f32, width: f32, height: f32, color: Rgba) -> WidgetDrawItem {
    WidgetDrawItem::Quad { x, y, width, height, color, clip: None }
}

/// Push a `thickness`-pixel border ring around the `width` × `height` local
/// rect — four thin quads (top, bottom, left, right). A focused widget draws
/// this from `theme.accent` so the focus ring reads without the root holding
/// any per-widget-type visual knowledge.
pub(crate) fn push_border(items: &mut Vec<WidgetDrawItem>, width: f32, height: f32, thickness: f32, color: Rgba) {
    items.push(quad(0.0, 0.0, width, thickness, color));
    items.push(quad(0.0, height - thickness, width, thickness, color));
    items.push(quad(0.0, 0.0, thickness, height, color));
    items.push(quad(width - thickness, 0.0, thickness, height, color));
}

fn push_inset_border(
    items: &mut Vec<WidgetDrawItem>,
    width: f32,
    height: f32,
    inset: f32,
    thickness: f32,
    color: Rgba,
) {
    let inner_width = inset.mul_add(-2.0, width).max(0.0);
    let inner_height = inset.mul_add(-2.0, height).max(0.0);
    items.push(quad(inset, inset, inner_width, thickness, color));
    items.push(quad(inset, inset + inner_height - thickness, inner_width, thickness, color));
    items.push(quad(inset, inset, thickness, inner_height, color));
    items.push(quad(inset + inner_width - thickness, inset, thickness, inner_height, color));
}

/// Draw validation and focus as orthogonal outlines. Validation owns the outer
/// ring; when both are present focus moves inward so neither signal covers the
/// other.
pub(super) fn push_control_outlines(
    items: &mut Vec<WidgetDrawItem>,
    width: f32,
    height: f32,
    state: &InteractionState,
    theme: &Theme,
) {
    let validation = state.validation_color(theme);
    if let Some(color) = validation {
        push_border(items, width, height, 2.0, color);
    }
    if state.focused() {
        push_inset_border(
            items,
            width,
            height,
            if validation.is_some() {
                2.0
            } else {
                0.0
            },
            2.0,
            theme.accent,
        );
    }
}

/// A rough per-character advance for caret placement and content sizing, as a
/// fraction of the font size. The exact-metric path (a `CachedFontMetrics`
/// measure) needs the font's metrics fanned down to the widget, which the
/// `Theme` does not carry in v1; this proportional approximation keeps caret
/// motion local and synchronous. The byte-offset caret *logic* (which the unit
/// tests pin) is exact regardless — only the pixel placement approximates.
pub(crate) const APPROX_ADVANCE_RATIO: f32 = 0.5;

/// The approximate pixel width of `char_count` characters at `size_pixels`,
/// using [`APPROX_ADVANCE_RATIO`].
pub(crate) fn approx_text_width(char_count: usize, size_pixels: f32) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let count = char_count as f32;
    count * size_pixels * APPROX_ADVANCE_RATIO
}

fn single_line_hit_byte(text: &str, metrics: Option<&CachedFontMetrics>, size_pixels: f32, local_x: f32) -> usize {
    if let Some(metrics) = metrics {
        return SingleLineLayout::build(text, metrics, size_pixels).hit_test(local_x);
    }
    let advance = (size_pixels * APPROX_ADVANCE_RATIO).max(1.0);
    let index = if local_x <= 0.0 {
        0
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rounded = (local_x / advance + 0.5) as usize;
        rounded.min(text.chars().count())
    };
    text.char_indices().nth(index).map_or(text.len(), |(byte, _)| byte)
}

fn single_line_edit_draw_items(
    displayed: &DisplayedEdit,
    metrics: Option<&CachedFontMetrics>,
    theme: &Theme,
    state: &InteractionState,
    theme_state: ThemeState,
    width: f32,
    height: f32,
) -> Vec<WidgetDrawItem> {
    let pad = theme.pad;
    let size = theme.value_size_pixels;
    let text_y = text_origin_y(0.0, height, size);
    let caret_height = pad.mul_add(-2.0, height).max(1.0);
    let layout = metrics.map(|metrics| SingleLineLayout::build(&displayed.text, metrics, size));
    let prefix_width = |byte: usize| {
        layout.as_ref().map_or_else(
            || approx_text_width(displayed.text[..byte].chars().count(), size),
            |layout| layout.caret_x(byte),
        )
    };

    let mut items = Vec::new();
    items.push(quad(0.0, 0.0, width, height, theme.fill(theme.surface_raised, theme_state)));
    if let Some(span) = displayed.selection_span {
        let x0 = pad + prefix_width(span.start_byte);
        let x1 = pad + prefix_width(span.end_byte);
        items.push(quad(x0, pad, (x1 - x0).max(1.0), caret_height, theme.accent));
    }
    if let Some(span) = displayed.preedit_cursor_span.filter(|span| !span.is_collapsed()) {
        let x0 = pad + prefix_width(span.start_byte);
        let x1 = pad + prefix_width(span.end_byte);
        items.push(quad(x0, pad, (x1 - x0).max(1.0), caret_height, theme.accent));
    }
    if !displayed.text.is_empty() {
        items.push(WidgetDrawItem::Text {
            x: pad,
            y: text_y,
            font_id: theme.font_id,
            text: displayed.text.clone(),
            size_pixels: size,
            color: theme.fill(theme.text_primary, theme_state),
            clip: None,
        });
    }
    if let Some(span) = displayed.preedit_span {
        let x0 = pad + prefix_width(span.start_byte);
        let x1 = pad + prefix_width(span.end_byte);
        items.push(quad(x0, text_y + size, (x1 - x0).max(1.0), 1.0, theme.accent));
        if let Some(cursor) = displayed.preedit_cursor_span.filter(|cursor| cursor.is_collapsed()) {
            let cursor_x = pad + prefix_width(cursor.end_byte);
            items.push(quad(cursor_x, pad, 1.0, caret_height, theme.accent));
        }
    }
    if state.focused() && !displayed.composing {
        let caret_x = pad + prefix_width(displayed.caret_byte);
        items.push(quad(caret_x, pad, 1.0, caret_height, theme.accent));
    }
    push_control_outlines(&mut items, width, height, state, theme);
    items
}

/// The `Screen`-space `DrawText` origin y that vertically centers a single
/// line of `size_pixels` text in a row `row_height` tall whose top is
/// `row_top` (widget-local). `aether.text` treats a `Screen` draw `origin`
/// as the line box's top-left and places the baseline one ascent below it,
/// so centering the em box keeps the glyph ink inside the row without the
/// font's exact ascent — which the theme does not fan to widgets (see
/// [`APPROX_ADVANCE_RATIO`]).
pub(crate) fn text_origin_y(row_top: f32, row_height: f32, size_pixels: f32) -> f32 {
    (row_height - size_pixels).mul_add(0.5, row_top)
}
