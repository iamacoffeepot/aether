//! The concrete widget set (issue 2660): five plain `#[actor(instanced)]`
//! types a panel root spawns as inline children and drives by mail in four
//! lanes — config / style / layout-frame data-down, value events-up — over the
//! ADR-0117 draw-compositing protocol.
//!
//! - [`slider::SliderWidget`] — a horizontal value slider, dragged or
//!   arrow-nudged.
//! - [`text_field::TextFieldWidget`] — a single-line editable string.
//! - [`radio::RadioGroupWidget`] — a vertical list of exclusive options.
//! - [`button::ButtonWidget`] — a momentary push button.
//! - [`label::LabelWidget`] — static, non-interactive text.
//!
//! Each caches its assigned [`WidgetFrame`](crate::widgets::WidgetFrame) rect
//! and its [`Theme`](crate::theme::Theme), answers every
//! [`Collect`](crate::widgets::Collect) with a
//! [`WidgetDrawList`](crate::widgets::WidgetDrawList) drawn in its own local
//! coordinates (colors resolved through [`Theme::fill`](crate::theme::Theme::fill)),
//! and reports value changes up to its parent. Widgets never subscribe to
//! input; the root forwards it — see [`super::focus::Focus`] and
//! [`super::widget_panel::WidgetPanel`].
//!
//! An inline child receives only `init` (no `wire`, and `init` cannot mail),
//! so a widget does nothing at boot beyond building its state; the root's
//! first `WidgetFrame` gives it its rect and the first `Collect` its first
//! draw.

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

use crate::widgets::WidgetDrawItem;

/// A flat-colored quad in a widget's own local coordinates — the shared
/// constructor the widgets build their chrome from.
pub(crate) fn quad(x: f32, y: f32, width: f32, height: f32, color: [f32; 4]) -> WidgetDrawItem {
    WidgetDrawItem::Quad {
        x,
        y,
        width,
        height,
        color,
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
    color: [f32; 4],
) {
    items.push(quad(0.0, 0.0, width, thickness, color));
    items.push(quad(0.0, height - thickness, width, thickness, color));
    items.push(quad(0.0, 0.0, thickness, height, color));
    items.push(quad(width - thickness, 0.0, thickness, height, color));
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
