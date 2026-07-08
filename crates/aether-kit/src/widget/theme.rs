//! Widget theme trunk types: a flat value of named visual tokens the
//! widget tier draws from instead of per-site color / metric literals.
//! [`Theme`] carries palette roles, interaction-state overlays, and
//! spacing/font metrics; it rides data-down through the two channels
//! the actor model already has — a widget's spawn `Config` embeds it,
//! and [`SetTheme`] re-fans it down live. There is no cascade, no
//! selectors, and no ambient ctx-carried theme: a one-off widget look
//! is an explicit override field in that widget's own config, not a
//! resolution rule here.
//!
//! [`Theme::fill`] is the one piece of owned logic — it composites an
//! interaction-state overlay over a base role color, so restyling a
//! role (e.g. `accent`) restyles its hover/pressed looks for free and
//! no widget invents an ad-hoc pressed color.

use serde::{Deserialize, Serialize};

/// Per-frame local interaction state a widget is in. Never serialized
/// — it's derived fresh each frame from input, not carried on the
/// wire. Passed by value to [`Theme::fill`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidgetState {
    Normal,
    Hover,
    Pressed,
    Disabled,
}

/// Convert one 8-bit sRGB channel to a linear-space float via the same
/// approximate `channel²` transfer the world mesher uses
/// (`crates/aether-kit/src/world/mesher/style.rs`, `hsl_to_linear_rgb`):
/// `(channel / 255)²`.
const fn srgb_channel_to_linear(channel: u8) -> f32 {
    let c = channel as f32 / 255.0;
    c * c
}

/// The shared visual language every widget draws from: palette role
/// tokens, interaction-state overlays, and spacing/font metrics. A
/// flat value, not a styling system — resolving a widget's actual draw
/// color is always an explicit `theme.fill(theme.some_role, state)`
/// call at the draw site, never implicit lookup or cascade.
///
/// Schema-only (no `Kind`): `Theme` is only ever a nested field inside
/// a widget's `Config` or inside [`SetTheme`], never a top-level mail
/// payload on its own, mirroring the established nested-struct
/// precedent `SolidQuad`
/// (`crates/aether-capabilities/src/render/kinds.rs`).
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Theme {
    /// Base background fill — panel / window backdrop.
    pub surface: [f32; 4],
    /// A surface lifted one step above `surface` — cards, rows,
    /// raised panels within a panel.
    pub surface_raised: [f32; 4],
    /// Borders, dividers, and control outlines.
    pub outline: [f32; 4],
    /// Primary text and iconography.
    pub text_primary: [f32; 4],
    /// De-emphasized text — captions, disabled labels, hints.
    pub text_muted: [f32; 4],
    /// The interactive / highlight role — buttons, active track
    /// fills, selection.
    pub accent: [f32; 4],
    /// Text/iconography drawn on top of an `accent`-filled surface.
    pub accent_text: [f32; 4],
    /// Role-agnostic lift composited over a base color on
    /// [`WidgetState::Hover`] — a subtle white brighten.
    pub hover_overlay: [f32; 4],
    /// Role-agnostic press composited over a base color on
    /// [`WidgetState::Pressed`] — a subtle black darken.
    pub pressed_overlay: [f32; 4],
    /// Alpha multiplier applied to a base color on
    /// [`WidgetState::Disabled`].
    pub disabled_alpha: f32,
    /// Inner padding, in pixels, a widget reserves between its
    /// border and its content.
    pub pad: f32,
    /// Spacing, in pixels, between sibling widgets in a layout.
    pub gap: f32,
    /// Height, in pixels, of one widget row (sliders, buttons,
    /// labeled rows).
    pub row_height: f32,
    /// Font size, in pixels, for a widget's label text.
    pub label_size_pixels: f32,
    /// Font size, in pixels, for a widget's value text (e.g. a
    /// slider's live numeric readout).
    pub value_size_pixels: f32,
    /// Session-scoped font id to draw label/value text with.
    /// Placeholder `0` here — the panel root stamps the real id
    /// once its `load_font_result` arrives.
    pub font_id: u32,
}

impl Theme {
    /// Resolve a widget's actual draw color: `base` unchanged for
    /// [`WidgetState::Normal`]; `hover_overlay` / `pressed_overlay`
    /// alpha-composited over `base` as standard src-over (the overlay
    /// as source, `base`'s own alpha preserved — overlays shift
    /// color/luminance, they never punch holes in an opaque surface)
    /// for `Hover` / `Pressed`; `base`'s alpha scaled by
    /// `disabled_alpha` for `Disabled`.
    #[must_use]
    pub fn fill(&self, base: [f32; 4], state: WidgetState) -> [f32; 4] {
        match state {
            WidgetState::Normal => base,
            WidgetState::Hover => Self::composite_over(base, self.hover_overlay),
            WidgetState::Pressed => Self::composite_over(base, self.pressed_overlay),
            WidgetState::Disabled => [base[0], base[1], base[2], base[3] * self.disabled_alpha],
        }
    }

    /// Standard src-over blend of `overlay` atop `base`, preserving
    /// `base`'s own alpha channel: each RGB channel lerps toward the
    /// overlay's channel by the overlay's alpha.
    fn composite_over(base: [f32; 4], overlay: [f32; 4]) -> [f32; 4] {
        let a = overlay[3];
        [
            (overlay[0] - base[0]).mul_add(a, base[0]),
            (overlay[1] - base[1]).mul_add(a, base[1]),
            (overlay[2] - base[2]).mul_add(a, base[2]),
            base[3],
        ]
    }

    /// The compiled-in default theme, seeded from the settled
    /// workbench chrome palette (the Moor-family dark UI). Overlay
    /// and metric defaults are neutral starting points, free to tune
    /// during widget-set bring-up.
    pub const DEFAULT: Self = Self {
        // `#191b15` — workbench `--bg`.
        surface: [
            srgb_channel_to_linear(0x19),
            srgb_channel_to_linear(0x1b),
            srgb_channel_to_linear(0x15),
            1.0,
        ],
        // `#20231b` — workbench `--panel`.
        surface_raised: [
            srgb_channel_to_linear(0x20),
            srgb_channel_to_linear(0x23),
            srgb_channel_to_linear(0x1b),
            1.0,
        ],
        // `#32362a` — workbench `--line`.
        outline: [
            srgb_channel_to_linear(0x32),
            srgb_channel_to_linear(0x36),
            srgb_channel_to_linear(0x2a),
            1.0,
        ],
        // `#e6e4d6` — workbench `--ink`.
        text_primary: [
            srgb_channel_to_linear(0xe6),
            srgb_channel_to_linear(0xe4),
            srgb_channel_to_linear(0xd6),
            1.0,
        ],
        // `#9aa08c` — workbench `--dim`.
        text_muted: [
            srgb_channel_to_linear(0x9a),
            srgb_channel_to_linear(0xa0),
            srgb_channel_to_linear(0x8c),
            1.0,
        ],
        // `#a8c97a` — workbench `--accent`.
        accent: [
            srgb_channel_to_linear(0xa8),
            srgb_channel_to_linear(0xc9),
            srgb_channel_to_linear(0x7a),
            1.0,
        ],
        // `#191b15` — dark ink on the light accent (= `surface`).
        accent_text: [
            srgb_channel_to_linear(0x19),
            srgb_channel_to_linear(0x1b),
            srgb_channel_to_linear(0x15),
            1.0,
        ],
        hover_overlay: [1.0, 1.0, 1.0, 0.08],
        pressed_overlay: [0.0, 0.0, 0.0, 0.12],
        disabled_alpha: 0.4,
        pad: 8.0,
        gap: 6.0,
        row_height: 24.0,
        label_size_pixels: 14.0,
        value_size_pixels: 14.0,
        font_id: 0,
    };
}

/// `aether.kit.widget.set_theme` — re-fan a live theme change down to
/// a panel root's widget children. Fire-and-forget; because the
/// widget surface redraws every tick (immediate mode), the next frame
/// draws with the new tokens — one frame of restyle latency, no
/// invalidation.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.set_theme")]
pub struct SetTheme {
    pub theme: Theme,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_normal_is_identity() {
        // Tripwire: Normal must return the base color untouched — no
        // overlay, no alpha scaling.
        let theme = Theme::DEFAULT;
        let base = [0.2, 0.4, 0.6, 0.8];
        assert_eq!(theme.fill(base, WidgetState::Normal), base);
    }

    #[test]
    fn fill_hover_composites_overlay_src_over_preserving_base_alpha() {
        // Tripwire: Hover blends hover_overlay over base as src-over —
        // each RGB channel lerps toward the overlay by the overlay's
        // alpha, and base's own alpha survives untouched.
        let theme = Theme {
            hover_overlay: [1.0, 0.0, 0.0, 0.5],
            ..Theme::DEFAULT
        };
        let base = [0.0, 1.0, 0.0, 1.0];
        assert_eq!(theme.fill(base, WidgetState::Hover), [0.5, 0.5, 0.0, 1.0]);
    }

    #[test]
    fn fill_disabled_scales_only_alpha() {
        // Tripwire: Disabled must scale the alpha channel by
        // disabled_alpha and leave RGB untouched.
        let theme = Theme {
            disabled_alpha: 0.4,
            ..Theme::DEFAULT
        };
        let base = [0.1, 0.2, 0.3, 1.0];
        assert_eq!(
            theme.fill(base, WidgetState::Disabled),
            [0.1, 0.2, 0.3, 0.4]
        );
    }
}
