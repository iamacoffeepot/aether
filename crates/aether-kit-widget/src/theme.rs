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
    /// The neutral end of the severity scale — an informational notice, a
    /// confirmation, a bar on a toast that reports rather than complains. A
    /// cool blue-grey, so it reads as neither a warning nor something to
    /// press. It exists because the three severities have to be *three*
    /// colours: a notice drawn in the outline role is a notice nobody sees,
    /// and one drawn in the accent claims to be the primary action.
    pub info: Rgba,
    /// Validation-warning outline role, and the warning end of the severity
    /// scale a notice is coloured by.
    pub warning: Rgba,
    /// Validation-error outline role, and the error end of that same scale.
    pub error: Rgba,
    /// The current item of a list, a tab strip, a segmented control, or
    /// a dropdown — a *state*, never an affordance. Distinct from
    /// `accent` so a chosen row and a pressable button never share a
    /// look (one meaning per visual token).
    pub selection: Rgba,
    /// Text/iconography drawn on top of a `selection`-filled row.
    pub selection_text: Rgba,
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
    /// Font size, in pixels, for a title — the one line that names
    /// what the screen shows ([`TextRole::Title`]).
    pub title_size_pixels: f32,
    /// Font size, in pixels, for a section heading
    /// ([`TextRole::Heading`]).
    pub heading_size_pixels: f32,
    /// Font size, in pixels, for a caption or hint, one step under the
    /// body size ([`TextRole::Caption`]).
    pub caption_size_pixels: f32,
    /// The spacing unit, in pixels. Every gap a layout draws is a whole
    /// number of these (the 4-pixel grid: one unit between a label and
    /// its field, two between rows, four between groups) — see
    /// [`Theme::space`]. `pad` and `gap` are the two most common
    /// multiples, kept as fields for the widgets that draw with them.
    pub space_unit_pixels: f32,
    /// Session-scoped font id to draw label/value text with.
    /// Placeholder `0` here — the panel root stamps the real id
    /// once its `load_font_result` arrives.
    pub font_id: u32,
}

/// Which step of the type scale a run of text is set at. A screen shows
/// hierarchy by size before anything else, so every text a widget draws
/// names its role rather than a pixel size; the theme resolves the size
/// ([`Theme::text_size_pixels`]). `Body` is the default and what every
/// stock widget drew before roles existed.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextRole {
    /// The one line that names what the screen shows.
    Title,
    /// A section heading.
    Heading,
    /// Control labels, values, list rows — the reading size.
    #[default]
    Body,
    /// A hint, a unit, an empty-state line — quieter than body.
    Caption,
}

impl Theme {
    /// The font size this theme sets `role` at.
    #[must_use]
    pub fn text_size_pixels(&self, role: TextRole) -> f32 {
        match role {
            TextRole::Title => self.title_size_pixels,
            TextRole::Heading => self.heading_size_pixels,
            TextRole::Body => self.label_size_pixels,
            TextRole::Caption => self.caption_size_pixels,
        }
    }

    /// `steps` spacing units, in pixels — the only way a layout should
    /// produce a gap, so every gap on the screen lands on the grid.
    #[must_use]
    pub fn space(&self, steps: u8) -> f32 {
        self.space_unit_pixels * f32::from(steps)
    }

    /// This theme with every metric multiplied by `factor` and every
    /// colour untouched — how a consumer takes the display's scale
    /// factor without restating the scale.
    #[must_use]
    pub fn scaled(self, factor: f32) -> Self {
        Self {
            pad: self.pad * factor,
            gap: self.gap * factor,
            row_height: self.row_height * factor,
            label_size_pixels: self.label_size_pixels * factor,
            value_size_pixels: self.value_size_pixels * factor,
            title_size_pixels: self.title_size_pixels * factor,
            heading_size_pixels: self.heading_size_pixels * factor,
            caption_size_pixels: self.caption_size_pixels * factor,
            space_unit_pixels: self.space_unit_pixels * factor,
            ..self
        }
    }

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
        // A cool blue-grey, well away from the warm accent: the severity a
        // reader should read and not act on.
        info: Rgba::from_srgb8(0x6b, 0x99, 0xcc, 0xff),
        warning: Rgba::from_srgb8(0xe2, 0xa8, 0x4a, 0xff),
        error: Rgba::from_srgb8(0xe0, 0x62, 0x58, 0xff),
        // `#3b4330` — the raised surface lifted toward the accent: a
        // chosen row reads as lit, not as a button.
        selection: Rgba::from_srgb8(0x3b, 0x43, 0x30, 0xff),
        // Ink on a selected row stays the primary text.
        selection_text: Rgba::from_srgb8(0xe6, 0xe4, 0xd6, 0xff),
        pad: 8.0,
        gap: 6.0,
        row_height: 24.0,
        label_size_pixels: 14.0,
        value_size_pixels: 14.0,
        // The Material type scale at 1×: title 22, heading 16, body 14,
        // caption 12.
        title_size_pixels: 22.0,
        heading_size_pixels: 16.0,
        caption_size_pixels: 12.0,
        space_unit_pixels: 4.0,
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
