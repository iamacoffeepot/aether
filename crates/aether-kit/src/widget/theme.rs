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

use aether_math::Rgba;

/// Per-frame local interaction state a widget is in. Never serialized
/// — it's derived fresh each frame from input, not carried on the
/// wire. Passed by value to [`Theme::fill`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeState {
    Normal,
    Hover,
    Pressed,
    Disabled,
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
/// (`crates/aether-render/src/kinds.rs`).
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Theme {
    /// Base background fill — panel / window backdrop.
    pub surface: Rgba,
    /// A surface lifted one step above `surface` — cards, rows,
    /// raised panels within a panel.
    pub surface_raised: Rgba,
    /// Borders, dividers, and control outlines.
    pub outline: Rgba,
    /// Primary text and iconography.
    pub text_primary: Rgba,
    /// De-emphasized text — captions, disabled labels, hints.
    pub text_muted: Rgba,
    /// The interactive / highlight role — buttons, active track
    /// fills, selection.
    pub accent: Rgba,
    /// Text/iconography drawn on top of an `accent`-filled surface.
    pub accent_text: Rgba,
    /// Role-agnostic lift composited over a base color on
    /// [`ThemeState::Hover`] — a subtle white brighten.
    pub hover_overlay: Rgba,
    /// Role-agnostic press composited over a base color on
    /// [`ThemeState::Pressed`] — a subtle black darken.
    pub pressed_overlay: Rgba,
    /// Alpha multiplier applied to a base color on
    /// [`ThemeState::Disabled`].
    pub disabled_alpha: f32,
    /// Validation-warning outline role.
    pub warning: Rgba,
    /// Validation-error outline role.
    pub error: Rgba,
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
    /// [`ThemeState::Normal`]; `hover_overlay` / `pressed_overlay`
    /// alpha-composited over `base` as standard src-over (the overlay
    /// as source, `base`'s own alpha preserved — overlays shift
    /// color/luminance, they never punch holes in an opaque surface)
    /// for `Hover` / `Pressed`; `base`'s alpha scaled by
    /// `disabled_alpha` for `Disabled`.
    #[must_use]
    pub fn fill(&self, base: Rgba, state: ThemeState) -> Rgba {
        match state {
            ThemeState::Normal => base,
            ThemeState::Hover => Self::composite_over(base, self.hover_overlay),
            ThemeState::Pressed => Self::composite_over(base, self.pressed_overlay),
            ThemeState::Disabled => Rgba::new(base.r, base.g, base.b, base.a * self.disabled_alpha),
        }
    }

    /// Standard src-over blend of `overlay` atop `base`, preserving
    /// `base`'s own alpha channel: each RGB channel lerps toward the
    /// overlay's channel by the overlay's alpha.
    fn composite_over(base: Rgba, overlay: Rgba) -> Rgba {
        let blended = base.lerp(overlay, overlay.a);
        Rgba::new(blended.r, blended.g, blended.b, base.a)
    }

    /// The compiled-in default theme, seeded from the settled
    /// workbench chrome palette (the Moor-family dark UI). Overlay
    /// and metric defaults are neutral starting points, free to tune
    /// during widget-set bring-up.
    pub const DEFAULT: Self = Self {
        // `#191b15` — workbench `--bg`.
        surface: Rgba::from_srgb8(0x19, 0x1b, 0x15, 0xff),
        // `#20231b` — workbench `--panel`.
        surface_raised: Rgba::from_srgb8(0x20, 0x23, 0x1b, 0xff),
        // `#32362a` — workbench `--line`.
        outline: Rgba::from_srgb8(0x32, 0x36, 0x2a, 0xff),
        // `#e6e4d6` — workbench `--ink`.
        text_primary: Rgba::from_srgb8(0xe6, 0xe4, 0xd6, 0xff),
        // `#9aa08c` — workbench `--dim`.
        text_muted: Rgba::from_srgb8(0x9a, 0xa0, 0x8c, 0xff),
        // `#a8c97a` — workbench `--accent`.
        accent: Rgba::from_srgb8(0xa8, 0xc9, 0x7a, 0xff),
        // `#191b15` — dark ink on the light accent (= `surface`).
        accent_text: Rgba::from_srgb8(0x19, 0x1b, 0x15, 0xff),
        hover_overlay: Rgba::new(1.0, 1.0, 1.0, 0.08),
        pressed_overlay: Rgba::new(0.0, 0.0, 0.0, 0.12),
        disabled_alpha: 0.4,
        warning: Rgba::from_srgb8(0xe2, 0xa8, 0x4a, 0xff),
        error: Rgba::from_srgb8(0xe0, 0x62, 0x58, 0xff),
        pad: 8.0,
        gap: 6.0,
        row_height: 24.0,
        label_size_pixels: 14.0,
        value_size_pixels: 14.0,
        font_id: 0,
    };
}

impl Default for Theme {
    fn default() -> Self {
        Self::DEFAULT
    }
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
        let base = Rgba::new(0.2, 0.4, 0.6, 0.8);
        assert_eq!(theme.fill(base, ThemeState::Normal), base);
    }

    #[test]
    fn fill_hover_composites_overlay_src_over_preserving_base_alpha() {
        // Tripwire: Hover blends hover_overlay over base as src-over —
        // each RGB channel lerps toward the overlay by the overlay's
        // alpha, and base's own alpha survives untouched.
        let theme = Theme { hover_overlay: Rgba::new(1.0, 0.0, 0.0, 0.5), ..Theme::DEFAULT };
        let base = Rgba::new(0.0, 1.0, 0.0, 1.0);
        assert_eq!(theme.fill(base, ThemeState::Hover), Rgba::new(0.5, 0.5, 0.0, 1.0));
    }

    #[test]
    fn fill_disabled_scales_only_alpha() {
        // Tripwire: Disabled must scale the alpha channel by
        // disabled_alpha and leave RGB untouched.
        let theme = Theme { disabled_alpha: 0.4, ..Theme::DEFAULT };
        let base = Rgba::new(0.1, 0.2, 0.3, 1.0);
        assert_eq!(theme.fill(base, ThemeState::Disabled), Rgba::new(0.1, 0.2, 0.3, 0.4));
    }
}
