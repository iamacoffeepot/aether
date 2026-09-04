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
    /// The four rungs of the **rarity ladder** — the ink a name is written in
    /// when the thing it names carries a tier. `rarity_common` is the plain
    /// ink; the three above it are a cool blue, a yellow and a warm gold, the
    /// register a reader of loot lists already knows.
    ///
    /// They are inks, never fills: a tier is said by the colour of the *name*,
    /// so a list can carry four tiers without four plates fighting the
    /// selection for the row. Each clears 4.5 against the raised surface and
    /// 3.0 against every fill a row can draw under it — the hover wash and the
    /// selection included — so the ladder survives the row it lands on being
    /// chosen or pointed at ([`TextInk`]).
    pub rarity_common: Rgba,
    /// One step up the rarity ladder — a cool blue.
    pub rarity_uncommon: Rgba,
    /// Two steps up the rarity ladder — a yellow.
    pub rarity_rare: Rgba,
    /// The top of the rarity ladder — a warm gold.
    pub rarity_legendary: Rgba,
    /// The five inks of the **hue set** — the palette a vocabulary told apart
    /// by colour rather than by rank writes its names in: a damage type, a
    /// faction, a category tag. `hue_plain` is the neutral member, and the four
    /// beside it are a warm, a cool, a bright and a violet.
    ///
    /// A **set**, not a ladder: the rarity rungs are ordered and these are not,
    /// so a host maps its own vocabulary onto them in one function and nothing
    /// here claims a warm tag outranks a cool one. Like the ladder they are
    /// inks and never fills, and each is chosen to clear 4.5 against the raised
    /// surface and 3.0 against every fill a row can draw under it — the hover
    /// wash and the selection included — so a run of tags stays readable on the
    /// row a reader is pointing at ([`TextInk`]).
    pub hue_warm: Rgba,
    /// The cool member of the hue set.
    pub hue_cool: Rgba,
    /// The bright member of the hue set.
    pub hue_bright: Rgba,
    /// The violet member of the hue set.
    pub hue_violet: Rgba,
    /// The neutral member of the hue set — the tag that names no colour of its
    /// own, quieter than the primary ink so a run of coloured tags is what the
    /// eye lands on.
    pub hue_plain: Rgba,
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

/// Which named ink a run of text is written in. [`TextRole`]'s partner: the
/// role resolves the *size* a run is set at, this resolves the *colour*, and
/// the theme owns both so a consumer names a meaning rather than a value
/// ([`Theme::text_ink`]).
///
/// It exists because a row is more than one run. A list row's name and its
/// trailing amount, a dropdown option and the row it stands in — before this,
/// one ink covered the whole row, so "this run muted, that one in the tag's
/// colour" could not be said at all and a name could not carry its own tier
/// (the studio's gaps 27 and 31). `Inherited` is the default and is what every
/// run drew before the field existed.
///
/// The rarity rungs are a **generic four-step ladder**, not a game's
/// vocabulary: anything with a tier — a drop, a tier list, a plan — writes its
/// names in them. What the four rungs *mean* belongs to the host; what they
/// look like, and that each stays legible on every fill a row draws under it,
/// belongs to the theme.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextInk {
    /// Whatever the run would have been written in without an ink: the primary
    /// ink at most roles, the muted ink at [`TextRole::Caption`], and a widget
    /// is free to override it further (a selected list row keeps
    /// `selection_text`). The default.
    #[default]
    Inherited,
    /// The muted ink, whatever the role's size — the "and why it is here" half
    /// of a row, quieter than the name in front of it.
    Muted,
    /// The accent. A **run**, never a plate: the accent means the primary
    /// action and a screen that plates four things in it has spent the token,
    /// but one lettered run — a tag, a match, a live value — is the token used
    /// once and read once.
    Accent,
    /// The plain rung of the rarity ladder.
    RarityCommon,
    /// One step up the rarity ladder.
    RarityUncommon,
    /// Two steps up the rarity ladder.
    RarityRare,
    /// The top of the rarity ladder.
    RarityLegendary,
    /// The warm member of the hue set.
    HueWarm,
    /// The cool member of the hue set.
    HueCool,
    /// The bright member of the hue set.
    HueBright,
    /// The violet member of the hue set.
    HueViolet,
    /// The neutral member of the hue set.
    HuePlain,
}

/// The contrast ratio a control's own face — a tonal plate, a stroke around an
/// outlined one — has to clear against the surface it stands on. WCAG 2.2
/// §1.4.11's non-text minimum: below it a reader cannot see where the control
/// is, which is exactly what a `Cancel` that disappears into a dialog plate is.
const FACE_CONTRAST_TARGET: f32 = 3.0;

/// How far a derived face may be carried toward the colour it borrows.
///
/// The floor keeps a face that already clears the target from collapsing back
/// onto its start, so the rung still reads as *tinted*; the ceiling keeps it
/// from arriving at the colour itself, which is what would make a tonal plate
/// the filled plate under a second name. A role too near the surface in
/// luminance to reach the target inside that range stops at the ceiling — as
/// far as this ladder goes — rather than pretending it got there.
const FACE_MIX_FLOOR: f32 = 0.12;
const FACE_MIX_CEILING: f32 = 0.6;

/// The offset in the WCAG contrast-ratio formula, `(L1 + 0.05) / (L2 + 0.05)`.
const CONTRAST_OFFSET: f32 = 0.05;

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

    /// The colour this theme writes `ink` in, at `role`.
    ///
    /// `role` is consulted only for [`TextInk::Inherited`], which is the point
    /// of the pair: a caption is quieter than a body run by construction, and
    /// every other ink says its colour outright and keeps it whatever size the
    /// run is set at. A widget that inks a run differently again — a selected
    /// list row in `selection_text` — layers that over an `Inherited` run and
    /// leaves a named ink alone, because the reason a name is written in a
    /// rarity colour does not stop applying when its row is chosen.
    #[must_use]
    pub fn text_ink(&self, ink: TextInk, role: TextRole) -> Rgba {
        match ink {
            TextInk::Inherited => match role {
                TextRole::Caption => self.text_muted,
                TextRole::Title | TextRole::Heading | TextRole::Body => self.text_primary,
            },
            TextInk::Muted => self.text_muted,
            TextInk::Accent => self.accent,
            TextInk::RarityCommon => self.rarity_common,
            TextInk::RarityUncommon => self.rarity_uncommon,
            TextInk::RarityRare => self.rarity_rare,
            TextInk::RarityLegendary => self.rarity_legendary,
            TextInk::HueWarm => self.hue_warm,
            TextInk::HueCool => self.hue_cool,
            TextInk::HueBright => self.hue_bright,
            TextInk::HueViolet => self.hue_violet,
            TextInk::HuePlain => self.hue_plain,
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

    /// A colour's relative luminance, WCAG 2.2's weighted sum. The channels of
    /// an [`Rgba`] are already linear (`Rgba::from_srgb8` converts on the way
    /// in), so there is no decode step to do first.
    #[must_use]
    pub fn relative_luminance(color: Rgba) -> f32 {
        0.2126_f32.mul_add(color.r, 0.7152_f32.mul_add(color.g, 0.0722 * color.b))
    }

    /// The WCAG contrast ratio between two colours: `1.0` for a pair that is
    /// the same colour, `21.0` for black against white. Public because it is
    /// the only honest answer to "does this face read on that plate" — the
    /// question a palette is tuned against, and the one a tripwire over a
    /// palette asserts instead of eyeballing.
    #[must_use]
    pub fn contrast_ratio(first: Rgba, second: Rgba) -> f32 {
        let (first, second) = (Self::relative_luminance(first), Self::relative_luminance(second));
        (first.max(second) + CONTRAST_OFFSET) / (first.min(second) + CONTRAST_OFFSET)
    }

    /// A **tonal** plate in `role`: the raised surface carried toward it until
    /// the plate clears the 3.0 face-contrast target against that same surface,
    /// keeping the surface's own alpha.
    ///
    /// This is the quiet middle of the emphasis ladder — louder than an
    /// outline, quieter than a filled plate — and it is derived rather than
    /// stored because a stored token would be a second thing to restyle: a
    /// theme that moves its `accent` moves every tonal plate with it, the
    /// same way [`Self::fill`] moves every hover.
    ///
    /// The mix is **computed from the target** rather than fixed, because a
    /// fixed mix fixes a distance and not a legibility. At the flat 22% this
    /// carried before, the neutral tonal plate cleared 2.67 against the raised
    /// surface and the danger one 1.79 — and a dialog draws its plate in
    /// `surface_raised`, the very surface this is derived from, so a tonal
    /// `Cancel` on one read as lettering on the plate rather than as a button
    /// (the owner's round-11 note 10). Deriving the mix fixes the *ratio*
    /// instead, so both tones and any restyled role land on one visible step.
    ///
    /// It is deliberately **not** `selection`. A chosen row and a secondary
    /// verb must not share a look (one meaning per visual token), which a
    /// tonal button reusing the selection role would break the moment the two
    /// stood side by side.
    #[must_use]
    pub fn tonal(&self, role: Rgba) -> Rgba {
        self.carried_to_face_contrast(self.surface_raised, role)
    }

    /// The stroke an **outlined** control draws around itself: the `outline`
    /// role carried toward the primary ink until it clears the same 3.0
    /// face-contrast target against the raised surface.
    ///
    /// `outline` on its own is the *divider* token — the hairline between two
    /// list rows, the rule under a dialog's title — and a divider is meant to
    /// be nearly invisible: this theme's clears 1.29 against the raised
    /// surface. Borrowed unchanged as a button's border it made the outlined
    /// rung and the text rung one face at a glance, which is half of the
    /// owner's round-11 note 4 — two row verbs at different emphases that read
    /// alike. A control's edge and a content divider are two meanings, so they
    /// are two tokens; this one is still *derived* from `outline`, so a
    /// restyled divider still carries the edge with it.
    #[must_use]
    pub fn edge(&self) -> Rgba {
        self.carried_to_face_contrast(self.outline, self.text_primary)
    }

    /// `start` carried toward `toward` by the smallest mix that clears
    /// [`FACE_CONTRAST_TARGET`] against the raised surface, clamped into the
    /// mix range and keeping `start`'s own alpha.
    ///
    /// Luminance and [`Rgba::lerp`] are both linear in the channels, so the
    /// mix that lands on a target luminance is solved rather than searched:
    /// `L(t) = L(start) + t * (L(toward) - L(start))`. The target sits on
    /// whichever side of the surface `toward` lies, so a light theme — where a
    /// face is carried *down* from a bright plate — resolves the same way a
    /// dark one does.
    fn carried_to_face_contrast(&self, start: Rgba, toward: Rgba) -> Rgba {
        let surface = Self::relative_luminance(self.surface_raised);
        let (from, to) = (Self::relative_luminance(start), Self::relative_luminance(toward));
        let target = if to >= surface {
            FACE_CONTRAST_TARGET.mul_add(surface + CONTRAST_OFFSET, -CONTRAST_OFFSET)
        } else {
            (surface + CONTRAST_OFFSET) / FACE_CONTRAST_TARGET - CONTRAST_OFFSET
        };
        let span = to - from;
        let mix = if span.abs() > f32::EPSILON {
            ((target - from) / span).clamp(FACE_MIX_FLOOR, FACE_MIX_CEILING)
        } else {
            FACE_MIX_CEILING
        };
        let blended = start.lerp(toward, mix);
        Rgba::new(blended.r, blended.g, blended.b, start.a)
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
        // The rarity ladder. `common` is the primary ink — an untiered name is
        // written exactly as any other name is — and the three above it are
        // lifted well past their "natural" saturation on purpose: each has to
        // stay legible on the *brightest* fill a row draws, which is a
        // selected row under the pointer, so a deep gold that reads on the
        // plate would vanish there.
        rarity_common: Rgba::from_srgb8(0xe6, 0xe4, 0xd6, 0xff),
        rarity_uncommon: Rgba::from_srgb8(0x9f, 0xc0, 0xff, 0xff),
        rarity_rare: Rgba::from_srgb8(0xf2, 0xd7, 0x5c, 0xff),
        rarity_legendary: Rgba::from_srgb8(0xe5, 0xb3, 0x71, 0xff),
        // The hue set, measured against the four fills a row draws (raised,
        // raised + hover, selection, selection + hover) — the worst of the four
        // is a chosen row under the pointer, and each of these clears 3.0 there
        // and 4.5 on the plate: warm 3.33 / 8.98, cool 3.76 / 10.13, bright
        // 5.10 / 13.73, violet 3.41 / 9.18, plain 3.91 / 10.53. The saturated
        // "natural" pick for each hue — a fire orange, a chaos purple — lands
        // near 2.8 on that fill, which is why every one of these is lifted.
        hue_warm: Rgba::from_srgb8(0xff, 0xb0, 0x8a, 0xff),
        hue_cool: Rgba::from_srgb8(0x8a, 0xd8, 0xff, 0xff),
        hue_bright: Rgba::from_srgb8(0xff, 0xef, 0x9e, 0xff),
        hue_violet: Rgba::from_srgb8(0xd4, 0xb8, 0xff, 0xff),
        hue_plain: Rgba::from_srgb8(0xd6, 0xd2, 0xc4, 0xff),
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
    fn a_derived_face_clears_the_contrast_target_on_the_plate_it_stands_on() {
        // Tripwire: the owner's round-11 note 10 — `Cancel` blending into the
        // New item dialog's plate. A dialog draws its plate in
        // `surface_raised`, so a tonal button on one is `tonal(role)` against
        // exactly that colour; at the old flat 22% mix it measured 2.67 for
        // the neutral tone and 1.79 for danger, both under the 3.0 a control's
        // own face needs to be seen. The rule is the ratio, so this holds for
        // a restyled accent and for either tone, which a pinned mix did not.
        // The mix is solved in `f32`, so a face that lands exactly on the
        // target measures back a few parts in ten million under it. The
        // tolerance is that rounding and nothing else — it is orders of
        // magnitude below the 1.2 the old fixed mix fell short by.
        let floor = FACE_CONTRAST_TARGET - 1e-4;
        let theme = Theme::DEFAULT;
        for role in [theme.accent, theme.error, theme.info, theme.warning] {
            let ratio = Theme::contrast_ratio(theme.tonal(role), theme.surface_raised);
            assert!(ratio >= floor, "a tonal plate reads at only {ratio} on the plate under it");
        }

        let edge = Theme::contrast_ratio(theme.edge(), theme.surface_raised);
        assert!(edge >= floor, "an outlined control's stroke reads at only {edge}");
        assert!(
            Theme::contrast_ratio(theme.outline, theme.surface_raised) < FACE_CONTRAST_TARGET,
            "the divider role is still the quiet hairline; the edge is a second token, not a rename of it",
        );
    }

    /// Assert every ink of one named vocabulary reads on every fill a list row
    /// can draw under it: 4.5 on the plate, where it is body text, and 3.0 on
    /// each of the four — the plain surface, the hover wash, the selection, and
    /// the selection under the pointer.
    fn assert_every_ink_reads_on_every_row_fill(inks: &[TextInk]) {
        let theme = Theme::DEFAULT;
        let fills = [
            theme.surface_raised,
            theme.fill(theme.surface_raised, ThemeState::Hover),
            theme.selection,
            theme.fill(theme.selection, ThemeState::Hover),
        ];

        for &ink in inks {
            let color = theme.text_ink(ink, TextRole::Body);
            assert!(
                Theme::contrast_ratio(color, theme.surface_raised) >= 4.5,
                "{ink:?} is body text on the plate and does not clear 4.5 there",
            );
            for fill in fills {
                let ratio = Theme::contrast_ratio(color, fill);
                assert!(ratio >= 3.0, "{ink:?} reads at only {ratio} on one of the row's own fills");
            }
        }
    }

    #[test]
    fn every_rarity_ink_reads_on_every_fill_a_row_can_draw_under_it() {
        // Tripwire: a rarity ink is chosen for its hue, and a hue picked on a
        // white page or against the plate alone goes illegible the moment its
        // row is pointed at or chosen — the two fills a list row spends most
        // of its life on. A deep gold-brown, the obvious choice for the top
        // rung, measures 2.4 on a selected row under the pointer. This is what
        // stops the next palette edit from shipping one.
        assert_every_ink_reads_on_every_row_fill(&[
            TextInk::RarityCommon,
            TextInk::RarityUncommon,
            TextInk::RarityRare,
            TextInk::RarityLegendary,
        ]);
    }

    #[test]
    fn every_hue_ink_reads_on_every_fill_a_row_can_draw_under_it() {
        // Tripwire: the hue set is the one vocabulary picked for hue *first* —
        // a fire tag is orange because fire is orange — and the saturated
        // version of every one of these measures around 2.8 on a chosen row
        // under the pointer, which is the fill a tag run spends its life on in
        // a list a reader is scanning. The five shipped here are each lifted
        // off their natural saturation for exactly that reason, and an edit
        // that puts the "right" orange back fails here rather than on a screen.
        assert_every_ink_reads_on_every_row_fill(&[
            TextInk::HueWarm,
            TextInk::HueCool,
            TextInk::HueBright,
            TextInk::HueViolet,
            TextInk::HuePlain,
        ]);
    }

    #[test]
    fn a_tonal_plate_never_arrives_at_the_role_it_borrows() {
        // Tripwire: the mix is solved from a contrast target, and a target
        // reachable only past the role itself would resolve the tonal rank
        // into the filled one — one ladder rung wearing another's face, which
        // is the defect the whole ladder exists to avoid. The ceiling is what
        // stops it, and a raised target that quietly ate the ceiling would
        // show up here rather than on a screen.
        let theme = Theme::DEFAULT;
        let surface = Theme::relative_luminance(theme.surface_raised);
        for role in [theme.accent, theme.error, theme.info, theme.warning] {
            let plate = Theme::relative_luminance(theme.tonal(role));
            assert!(theme.tonal(role) != role, "the tonal rank resolved to the filled rank for {role:?}");
            assert!(
                plate > surface && plate < Theme::relative_luminance(role),
                "the tonal plate for {role:?} left the span between the surface and the role",
            );
        }
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
