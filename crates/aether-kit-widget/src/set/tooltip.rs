// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]
// A tooltip holds a handful of lines; the `usize as f32` for a line's pixel
// offset cannot lose precision at any plate a reader would tolerate.
#![allow(clippy::cast_precision_loss)]

//! The tooltip: an anchored plate that says what the thing under the pointer
//! *is*.
//!
//! It exists for the owner's round-1 note 21 — "no tooltips on hovering stats
//! for left panel like what is health, what is each stat, etc. Where does it
//! come from?" — and it is shaped by the two notes that followed it: round-2
//! note 13, "tooltip text breaks up weirdly instead of fitting into a neat
//! box", and note 24, "tooltips should probably be formatted to divide text
//! better based upon UI principles (like dividers)". So the plate is
//! **measured**: every line wraps at one reading width, the box is exactly as
//! wide as its longest wrapped line and exactly as tall as the lines it
//! holds, and a section boundary is a rule rather than a blank line. Round-2
//! note 1 — "too much vertical padding above the text" — is why the padding
//! is one spacing unit and the lines are placed by
//! [`text_origin_y`] rather than by an em added at
//! the draw site.
//!
//! The plate draws in the **overlay** ([`WidgetDrawList::overlay`]) so it
//! stands over the rows under it, and the root's clip subtraction keeps their
//! glyphs from printing through it — the answer to round-1 note 16, "pop ups
//! have tree text overlay where they should take priority", with no draw
//! layer anywhere.
//!
//! # Who decides it is showing
//!
//! The host. A tooltip is a *hover* answer, and hover dwell, the row the
//! pointer is resting on, and the words for it are all the host's knowledge —
//! so the widget takes the finished lines and nothing else. Visibility rides
//! the lane every stock widget already has:
//! [`WidgetControlState::visible`](crate::WidgetControlState) through
//! [`SetWidgetState`], which the root can flip without disturbing anything
//! else the tooltip holds. A tooltip with no sections likewise draws nothing,
//! so a host that would rather send empty content than toggle a flag gets the
//! same result. There is deliberately no third `shown` field: one meaning,
//! one token.
//!
//! The plate is placed beside the widget's own assigned
//! [`WidgetFrame`] — the anchor is the *thing being
//! explained*, so a host points the tooltip at a row by giving it that row's
//! rectangle — and kept inside [`TooltipConfig::bounds`], flipping to the
//! other side of the anchor rather than hanging off the region's edge (see
//! [`place_plate`]).

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_math::Rgba;
use aether_text::FontMetricsResult;
use serde::{Deserialize, Serialize};

use crate::set::placement::{PlacementBounds, PlacementSide, place_plate};
use crate::set::{
    WidgetDefaults, accept_font_metrics_result, apply_text_theme, approx_text_width, measured_text_width,
    pump_text_font_metrics, push_rect_border, quad, reply_if_hidden, reveal_wrap_width, text_origin_y, wrap_to_width,
};
use crate::state::{InteractionState, emit_state_changed};
use crate::text_edit::FontMetricsAdapter;
use crate::theme::{SetTheme, TextRole, Theme};
use crate::{Collect, SetWidgetState, WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame};

/// One block of a tooltip's text, drawn with a rule between it and the next.
/// Sections are how a tooltip divides what it is saying — the name, the
/// sentence, where the number came from — instead of running three unrelated
/// facts together as one paragraph. Schema-only; nested in [`TooltipConfig`].
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct TooltipSection {
    /// The section's lines as the host wrote them. Each is wrapped to the
    /// plate's measure, so a line is a thought rather than a row of pixels;
    /// an empty section draws nothing at all, rule included.
    pub lines: Vec<String>,
}

/// `aether.kit.widget.tooltip.config` — an anchored plate explaining the
/// thing the pointer is on. `sections` are drawn in order with a rule between
/// them, wrapped at `max_width_pixels` (`0` takes the kit's reading measure,
/// [`reveal_wrap_width`]); `side` is the side of the anchor the plate prefers
/// and `bounds` is the region it must stay inside, which it flips across the
/// anchor to honour. The widget's assigned
/// [`WidgetFrame`] is the anchor.
///
/// Hidden (`state.visible = false`) or sectionless, it draws nothing — which
/// is how a host says the pointer has moved on.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.tooltip.config")]
pub struct TooltipConfig {
    pub sections: Vec<TooltipSection>,
    /// The widest the text may run before it wraps. `0` (the default) takes
    /// [`reveal_wrap_width`] at the caption size — the same reading measure
    /// the hover reveal plate uses, so the two look like one kit.
    #[serde(default)]
    pub max_width_pixels: f32,
    /// The side of the anchor the plate prefers.
    #[serde(default)]
    pub side: PlacementSide,
    /// The region the plate must stay inside, in the same window pixels the
    /// anchor frame is assigned in. A widget cannot ask the window how big it
    /// is, so the host that owns the region names it here.
    #[serde(default)]
    pub bounds: PlacementBounds,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

/// One wrapped line of the plate, with the type role it is set at.
struct PlateLine {
    text: String,
    role: TextRole,
}

impl PlateLine {
    fn size_pixels(&self, theme: &Theme) -> f32 {
        theme.text_size_pixels(self.role)
    }

    /// How tall this line's own box is. A line box is its type size times the
    /// leading, so a caption line takes less room than a body line and the
    /// plate is the sum of what it actually holds.
    fn height(&self, theme: &Theme) -> f32 {
        self.size_pixels(theme) * LINE_LEADING
    }

    fn ink(&self, theme: &Theme) -> Rgba {
        match self.role {
            TextRole::Caption => theme.text_muted,
            _ => theme.text_primary,
        }
    }
}

/// The plate's padding, in spacing units — its whole inset, every edge alike.
/// One unit, because the round-2 note about the first tooltip was that there
/// was too much space above the text and none of it meant anything.
const PAD_UNITS: u8 = 1;

/// How tall one line's box is, as a multiple of its own type size.
const LINE_LEADING: f32 = 1.4;

/// The tooltip widget. Holds the sections it was given plus the cached theme,
/// frame, and font metrics it measures them with.
pub struct TooltipWidget {
    sections: Vec<TooltipSection>,
    max_width_pixels: f32,
    side: PlacementSide,
    bounds: PlacementBounds,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    /// Single-flight exact metrics for the active theme font. The plate is
    /// sized to its longest wrapped line, so it wants real advances; before
    /// they land it wraps and measures by the crate's per-character
    /// approximation, exactly as the content-sized rows do.
    font_metrics: FontMetricsAdapter,
}

impl TooltipWidget {
    /// One line's width at `size_pixels`: measured once the font's metrics
    /// resolve, approximated for the frame or two before that.
    fn text_width(&self, text: &str, size_pixels: f32) -> f32 {
        self.font_metrics.resolved().map_or_else(
            || approx_text_width(text.chars().count(), size_pixels),
            |metrics| measured_text_width(metrics, text, size_pixels),
        )
    }

    /// The measure the text wraps at: the configured maximum, or the kit's
    /// reading width at the caption size when the config asks for none.
    fn wrap_width(&self) -> f32 {
        if self.max_width_pixels.is_finite() && self.max_width_pixels > 0.0 {
            self.max_width_pixels
        } else {
            reveal_wrap_width(self.theme.caption_size_pixels)
        }
    }

    /// The sections wrapped into plate lines, one `Vec` per section, empty
    /// sections dropped so they cannot leave a rule with nothing under it.
    ///
    /// The tooltip's **first line is its title** — the name of the thing
    /// being explained — and takes the reading size and the primary ink;
    /// every other line is caption-role and muted. `TextRole::Title` is
    /// deliberately not used: that is the size a *screen's* one title is set
    /// at, and a 22-pixel line on a hover plate is a headline, not a name.
    fn wrapped_sections(&self) -> Vec<Vec<PlateLine>> {
        let measure_width = self.wrap_width();
        let mut first_line = true;
        let mut sections = Vec::new();
        for section in &self.sections {
            let mut lines = Vec::new();
            for source in &section.lines {
                let role = if first_line {
                    TextRole::Body
                } else {
                    TextRole::Caption
                };
                first_line = false;
                let size = self.theme.text_size_pixels(role);
                lines.extend(
                    wrap_to_width(source, measure_width, |run| self.text_width(run, size))
                        .into_iter()
                        .map(|text| PlateLine { text, role }),
                );
            }
            if !lines.is_empty() {
                sections.push(lines);
            }
        }
        sections
    }

    /// The plate's own size: as wide as its longest wrapped line plus one
    /// unit either side, and as tall as the line boxes it holds plus the
    /// rules between its sections and that same unit top and bottom. Nothing
    /// is rounded up to a row height — a plate padded to a grid is the "too
    /// much vertical padding" the note was about.
    fn plate_size(&self, sections: &[Vec<PlateLine>]) -> [f32; 2] {
        let pad = self.theme.space(PAD_UNITS);
        let widest = sections
            .iter()
            .flatten()
            .map(|line| self.text_width(&line.text, line.size_pixels(&self.theme)))
            .fold(0.0_f32, f32::max);
        let ink: f32 = sections.iter().flatten().map(|line| line.height(&self.theme)).sum();
        let rules = sections.len().saturating_sub(1) as f32 * self.rule_band();
        [pad.mul_add(2.0, widest), pad.mul_add(2.0, ink + rules)]
    }

    /// How much vertical room one section rule takes: the hairline itself
    /// plus one spacing unit either side, the same band a menu's separator
    /// occupies, so a divider between two blocks of text reads as a division
    /// and not as a line of its own.
    fn rule_band(&self) -> f32 {
        self.theme.space(1).mul_add(2.0, RULE_THICKNESS)
    }

    /// The plate, in the widget's own local coordinates. Empty while the
    /// tooltip has nothing to say or no room to say it in.
    fn overlay_items(&self) -> Vec<WidgetDrawItem> {
        let sections = self.wrapped_sections();
        if sections.is_empty() {
            return Vec::new();
        }
        let [width, height] = self.plate_size(&sections);
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return Vec::new();
        }

        let origin =
            place_plate(PlacementBounds::from(&self.frame), width, height, self.side, self.theme.space(1), self.bounds);
        let (left, top) = (origin[0] - self.frame.x, origin[1] - self.frame.y);
        let pad = self.theme.space(PAD_UNITS);

        let mut items = Vec::new();
        items.push(quad(left, top, width, height, self.theme.surface_raised));
        push_rect_border(&mut items, left, top, width, height, RULE_THICKNESS, self.theme.outline);

        let mut line_top = top + pad;
        for (index, lines) in sections.iter().enumerate() {
            if index > 0 {
                items.push(quad(
                    left + pad,
                    line_top + self.theme.space(1),
                    (pad.mul_add(-2.0, width)).max(0.0),
                    RULE_THICKNESS,
                    self.theme.outline,
                ));
                line_top += self.rule_band();
            }
            for line in lines {
                let size = line.size_pixels(&self.theme);
                let line_height = line.height(&self.theme);
                items.push(WidgetDrawItem::Text {
                    x: left + pad,
                    y: text_origin_y(line_top, line_height, size),
                    font_id: self.theme.font_id,
                    text: line.text.clone(),
                    size_pixels: size,
                    color: line.ink(&self.theme),
                    clip: None,
                });
                line_top += line_height;
            }
        }
        items
    }
}

/// The hairline a plate's ring and its section rules are drawn at.
const RULE_THICKNESS: f32 = 1.0;

impl WidgetDefaults for TooltipWidget {
    fn widget_frame(&mut self) -> &mut WidgetFrame {
        &mut self.frame
    }

    fn widget_theme(&mut self) -> &mut Theme {
        &mut self.theme
    }

    fn widget_state(&mut self) -> &mut InteractionState {
        &mut self.state
    }

    /// Nothing to cancel: a tooltip is read, never operated.
    fn cancel_activation(&mut self) {}
}

/// A tooltip plate. Spawned inline by a panel root with a [`TooltipConfig`];
/// it reports nothing up, because a plate that explains something has no
/// value of its own.
///
/// # Agent
/// Not loaded directly — the root spawns it as an inline child and re-sends
/// `TooltipConfig` to change what it says and what it is anchored beside.
/// Hide it with `aether.kit.widget.set_state`.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for TooltipWidget {
    type Config = TooltipConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.tooltip";

    fn init(config: TooltipConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let desired_font_id = config.theme.font_id;
        Ok(TooltipWidget {
            sections: config.sections,
            max_width_pixels: config.max_width_pixels,
            side: config.side,
            bounds: config.bounds,
            theme: config.theme,
            state: InteractionState::new(config.state),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            font_metrics: FontMetricsAdapter::new(desired_font_id),
        })
    }

    /// Ask for the theme font's metrics; the plate is sized to its own text,
    /// so it wants real advances as soon as there are any.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    /// Replace what the tooltip says and where it stands, in place. This is
    /// the mail a host sends on every hover change, so it resets nothing the
    /// root owns.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: TooltipConfig) {
        self.sections = config.sections;
        self.max_width_pixels = config.max_width_pixels;
        self.side = config.side;
        self.bounds = config.bounds;
        self.font_metrics.set_desired(config.theme.font_id);
        self.theme = config.theme;
        if self.state.replace(config.state) {
            emit_state_changed(ctx, &self.state);
        }
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    /// Update external availability — the lane a host shows and hides the
    /// plate through.
    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        if self.state.replace(set.state) {
            emit_state_changed(ctx, &self.state);
        }
    }

    /// Restyle: adopt the fanned theme and request metrics for its font.
    #[handler::single]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        apply_text_theme(ctx, &mut self.font_metrics, &mut self.theme, set.theme);
    }

    /// Install a font-metrics reply; the next `Collect` measures the plate
    /// against real advances.
    #[handler::single]
    fn on_font_metrics_result(&mut self, ctx: &mut WasmCtx<'_>, result: FontMetricsResult) {
        accept_font_metrics_result(ctx, &mut self.font_metrics, result);
    }

    /// Reply the plate as **overlay** and nothing as ordinary items: a
    /// tooltip is entirely outside its own slot, since its slot is the thing
    /// it is explaining.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { intrinsic: None, items: Vec::new(), overlay: self.overlay_items() });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn tooltip(sections: Vec<TooltipSection>) -> TooltipWidget {
        TooltipWidget {
            sections,
            max_width_pixels: 120.0,
            side: PlacementSide::Below,
            bounds: PlacementBounds { x: 0.0, y: 0.0, width: 360.0, height: 300.0 },
            theme: Theme::DEFAULT,
            frame: WidgetFrame { x: 20.0, y: 100.0, width: 200.0, height: 20.0 },
            state: InteractionState::new(WidgetControlState::default()),
            font_metrics: FontMetricsAdapter::new(0),
        }
    }

    fn section(lines: &[&str]) -> TooltipSection {
        TooltipSection { lines: lines.iter().map(|line| String::from(*line)).collect() }
    }

    #[test]
    fn the_plate_is_exactly_its_wrapped_lines_plus_one_unit_of_padding() {
        // Tripwire: the owner's "too much vertical padding above the text" and
        // "breaks up weirdly instead of fitting into a neat box" are the same
        // defect — a plate whose height is not the sum of the line boxes it
        // holds. A rule between sections adds its own band and nothing else.
        let widget = tooltip(vec![
            section(&["Life"]),
            section(&["Your health pool. Hits come off it, and the character dies when it empties."]),
        ]);
        let sections = widget.wrapped_sections();
        assert_eq!(sections.len(), 2, "both sections survive");
        assert!(sections[1].len() > 1, "the sentence wrapped onto its own lines");

        let ink: f32 = sections.iter().flatten().map(|line| line.height(&widget.theme)).sum();
        let [_, height] = widget.plate_size(&sections);
        let pad = widget.theme.space(PAD_UNITS);
        let expected = pad.mul_add(2.0, ink + widget.rule_band());
        assert!((height - expected).abs() < f32::EPSILON, "{height} is not its lines plus its rule and padding");
    }

    #[test]
    fn the_first_line_is_the_name_and_the_rest_is_caption_ink() {
        // Tripwire: the tooltip answers "what is this" before it explains it,
        // and the explanation has to read as quieter than the name. One role
        // for both would flatten the plate back into a paragraph.
        let widget = tooltip(vec![section(&["Life"]), section(&["A pool."])]);
        let sections = widget.wrapped_sections();
        assert_eq!(sections[0][0].role, TextRole::Body);
        assert_eq!(sections[0][0].ink(&widget.theme), widget.theme.text_primary);
        assert_eq!(sections[1][0].role, TextRole::Caption);
        assert_eq!(sections[1][0].ink(&widget.theme), widget.theme.text_muted);
    }

    #[test]
    fn an_empty_section_leaves_no_rule_behind_and_nothing_to_say_draws_nothing() {
        // Tripwire: a rule with no text under it is a plate that looks broken;
        // a plate with no text at all is chrome a reader never asked for.
        let widget = tooltip(vec![section(&["Life"]), TooltipSection::default()]);
        assert_eq!(widget.wrapped_sections().len(), 1);

        let quiet = tooltip(Vec::new());
        assert!(quiet.overlay_items().is_empty());
    }

    #[test]
    fn the_plate_stands_clear_of_its_anchor_and_inside_the_bounds() {
        // Tripwire: the plate must never cover the row it explains, and never
        // reach past the region the host allowed it — the two failures that
        // make a tooltip worse than none.
        let mut widget = tooltip(vec![section(&["Life"]), section(&["A pool of health."])]);
        widget.frame = WidgetFrame { x: 20.0, y: 280.0, width: 200.0, height: 20.0 };
        let items = widget.overlay_items();
        let plate = items.first().expect("a plate is drawn");
        let WidgetDrawItem::Quad { y, height, .. } = plate else {
            panic!("the plate leads with its fill: {plate:?}");
        };
        // Local coordinates: the frame's own origin is zero.
        assert!(y + height <= 0.0, "the plate flipped above the row it is about: {y} + {height}");
        assert!(y + widget.frame.y >= 0.0, "and stayed inside the region: {y}");
    }
}
