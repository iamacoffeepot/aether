//! The concrete widget set (issue 2660): five plain `#[actor(instanced)]`
//! types a panel root spawns as inline children and drives by mail in four
//! lanes — config / style / layout-frame data-down, value events-up — over the
//! ADR-0117 draw-compositing protocol.
//!
//! - [`SliderWidget`] — a horizontal value slider, dragged or
//!   arrow-nudged.
//! - [`TextFieldWidget`] — a single-line editable string.
//! - [`RadioGroupWidget`] — a vertical list of exclusive options.
//! - [`ButtonWidget`] — a momentary push button.
//! - [`LabelWidget`] — static, non-interactive text.
//!
//! Each caches its assigned [`WidgetFrame`](crate::widget::WidgetFrame) rect
//! and its [`Theme`], answers every
//! [`Collect`](crate::widget::Collect) with a
//! [`WidgetDrawList`](crate::widget::WidgetDrawList) drawn in its own local
//! coordinates (colors resolved through [`Theme::fill`](crate::widget::theme::Theme::fill)),
//! and reports value changes up to its parent. Widgets never subscribe to
//! input; the root forwards it — see [`super::focus::Focus`] and
//! [`super::panel::WidgetPanel`].
//!
//! Inline children run `wire` after `init`, like loaded actors, but still rely
//! on the root's first `WidgetFrame` for layout and the first `Collect` for
//! their first draw.

pub mod button;
pub mod label;
pub mod radio;
pub mod slider;
pub mod text_field;

pub use button::ButtonWidget;
pub use label::LabelWidget;
pub use radio::RadioGroupWidget;
pub use slider::SliderWidget;
pub use text_field::TextFieldWidget;

use alloc::vec::Vec;

use aether_math::Rgba;

use crate::widget::WidgetDrawItem;
use crate::widget::state::InteractionState;
use crate::widget::theme::Theme;

/// A flat-colored quad in a widget's own local coordinates — the shared
/// constructor the widgets build their chrome from.
pub(crate) fn quad(x: f32, y: f32, width: f32, height: f32, color: Rgba) -> WidgetDrawItem {
    WidgetDrawItem::Quad {
        x,
        y,
        width,
        height,
        color,
        clip: None,
    }
}

/// Push a `thickness`-pixel border ring around the `width` × `height` local
/// rect — four thin quads (top, bottom, left, right). A focused widget draws
/// this from `theme.accent` so the focus ring reads without the root holding
/// any per-widget-type visual knowledge.
pub(crate) fn push_border(
    items: &mut Vec<WidgetDrawItem>,
    width: f32,
    height: f32,
    thickness: f32,
    color: Rgba,
) {
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
    items.push(quad(
        inset,
        inset + inner_height - thickness,
        inner_width,
        thickness,
        color,
    ));
    items.push(quad(inset, inset, thickness, inner_height, color));
    items.push(quad(
        inset + inner_width - thickness,
        inset,
        thickness,
        inner_height,
        color,
    ));
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
            if validation.is_some() { 2.0 } else { 0.0 },
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
