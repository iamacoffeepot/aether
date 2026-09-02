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
//! [`place_plate_avoiding`]).
//!
//! # What a hover card needs on top of that
//!
//! A card over a canvas is a tooltip with four more demands, and each is one
//! field (the studio's gap 18). [`TooltipLine::ink`] distinguishes a line
//! *within* its role — the line a search matched, the stat that is not being
//! counted — which inking by role alone cannot do. [`TooltipConfig::avoid`]
//! names the rectangles the plate should keep off, the first outranking the
//! rest, so a card gets clear of the thing it is about before it considers the
//! rest of the frame. [`TooltipConfig::max_height_pixels`] bounds the plate
//! and makes it **shed** trailing whole entries rather than overprint,
//! reporting the count as [`TooltipShed`] so the host can word the tail. And
//! [`TooltipConfig::hanging_indent_pixels`] insets the continuation rows of a
//! wrapped line, so a two-row stat reads as one stat.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_math::Rgba;
use aether_text::FontMetricsResult;
use serde::{Deserialize, Serialize};

use crate::set::placement::{PlacementBounds, PlacementSide, place_plate_avoiding};
use crate::set::{
    WidgetDefaults, accept_font_metrics_result, apply_text_theme, approx_text_width, measured_text_width,
    pump_text_font_metrics, push_rect_border, quad, reply_if_hidden, reveal_wrap_width, text_origin_y,
    wrap_to_width_hanging,
};
use crate::state::{InteractionState, emit_state_changed};
use crate::text_edit::FontMetricsAdapter;
use crate::theme::{SetTheme, TextRole, Theme};
use crate::{Collect, SetWidgetState, WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame};

/// One line of a tooltip section, as the host wrote it: the words, and the
/// two presentation escapes a hover card needs (the studio's gap 18).
///
/// Both escapes are `None` by default, which is the kit's own rule — the
/// plate's first line is the name and is set at [`TextRole::Body`] in the
/// primary ink, every line after it at [`TextRole::Caption`] in the muted
/// one. A host overrides `ink` for the lines it needs to *distinguish*: which
/// line of a card the reader's search matched, or which stat is not being
/// counted. Inking by role alone collapses both distinctions, because a role
/// carries one ink. Schema-only; nested in [`TooltipSection`].
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct TooltipLine {
    pub text: String,
    /// The type step this line is set at, or `None` for the kit's rule.
    #[serde(default)]
    pub role: Option<TextRole>,
    /// The ink this line is drawn in, or `None` for the role's own ink.
    #[serde(default)]
    pub ink: Option<Rgba>,
}

impl From<String> for TooltipLine {
    fn from(text: String) -> Self {
        Self { text, role: None, ink: None }
    }
}

impl From<&str> for TooltipLine {
    fn from(text: &str) -> Self {
        Self::from(String::from(text))
    }
}

/// One block of a tooltip's text, drawn with a rule between it and the next.
/// Sections are how a tooltip divides what it is saying — the name, the
/// sentence, where the number came from — instead of running three unrelated
/// facts together as one paragraph. Schema-only; nested in [`TooltipConfig`].
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct TooltipSection {
    /// The section's lines as the host wrote them. Each is wrapped to the
    /// plate's measure, so a line is a thought rather than a row of pixels;
    /// an empty section draws nothing at all, rule included. A whole line is
    /// also the unit the shed ladder drops
    /// ([`TooltipConfig::max_height_pixels`]), so a plate out of room never
    /// ends a sentence halfway.
    pub lines: Vec<TooltipLine>,
}

impl TooltipSection {
    /// A section from anything a line can be written as — plain strings for
    /// the common case, [`TooltipLine`]s where a line needs its own ink.
    ///
    /// ```ignore
    /// TooltipSection::new(["Life", "Your health pool."])
    /// ```
    #[must_use]
    pub fn new<I, L>(lines: I) -> Self
    where
        I: IntoIterator<Item = L>,
        L: Into<TooltipLine>,
    {
        Self { lines: lines.into_iter().map(Into::into).collect() }
    }
}

/// `aether.kit.widget.tooltip.shed` — how many whole entries the plate had to
/// drop to fit [`TooltipConfig::max_height_pixels`], reported up to the host
/// on every change (`0` when a plate that was shedding fits again).
///
/// The host is the one that can do something about it: it knows what the
/// dropped entries said, so it is the one that can word the tail — "+3 more"
/// — or re-send a shorter card. The widget only reports the number, because
/// choosing the words is exactly the host's knowledge the tooltip deliberately
/// does not hold.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.tooltip.shed")]
pub struct TooltipShed {
    pub dropped: u32,
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
    /// The tallest the plate may stand. `0` (the default) is no budget at
    /// all. Over it, the plate **sheds**: it drops trailing whole entries —
    /// never part of one — until it fits, and reports how many went as
    /// [`TooltipShed`] so the host can word the tail.
    #[serde(default)]
    pub max_height_pixels: f32,
    /// How far the continuation rows of a wrapped line are inset. `0` (the
    /// default) is a flush block; a hanging indent makes a two-row stat read
    /// as one stat rather than as two lines that happen to be adjacent.
    #[serde(default)]
    pub hanging_indent_pixels: f32,
    /// The side of the anchor the plate prefers.
    #[serde(default)]
    pub side: PlacementSide,
    /// Rectangles the plate would rather not cover, in the same window pixels
    /// the anchor frame is assigned in — the thing being explained, its
    /// neighbours, the standing plates around it. **The first entry outranks
    /// the rest**: the plate gets clear of it before it considers any other,
    /// which is what keeps a hover card attached to its own subject. Empty
    /// (the default) places by the flip-and-clamp rule alone.
    #[serde(default)]
    pub avoid: Vec<PlacementBounds>,
    /// The region the plate must stay inside, in the same window pixels the
    /// anchor frame is assigned in. A widget cannot ask the window how big it
    /// is, so the host that owns the region names it here.
    #[serde(default)]
    pub bounds: PlacementBounds,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

/// One wrapped row of the plate: the type role it is set at, the indent it
/// starts at (zero, or the hanging indent on a continuation), and the ink the
/// host asked for if it asked for one.
struct PlateLine {
    text: String,
    role: TextRole,
    indent: f32,
    ink: Option<Rgba>,
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

    /// The host's own ink when it named one, otherwise the role's: the plate
    /// resolves an ink per line either way, so carrying one costs nothing and
    /// is the only way a line can be distinguished *within* a role.
    fn ink(&self, theme: &Theme) -> Rgba {
        self.ink.unwrap_or(match self.role {
            TextRole::Caption => theme.text_muted,
            _ => theme.text_primary,
        })
    }
}

/// One source line of one section, wrapped into the rows it occupies. This is
/// the unit the shed ladder drops: an entry goes whole or not at all, so a
/// reader never gets a stat that ends in "per".
struct PlateEntry {
    section: usize,
    lines: Vec<PlateLine>,
}

/// How many section boundaries a run of entries crosses — one rule each.
fn section_breaks(entries: &[PlateEntry]) -> usize {
    entries.windows(2).filter(|pair| pair[0].section != pair[1].section).count()
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
    max_height_pixels: f32,
    hanging_indent_pixels: f32,
    side: PlacementSide,
    avoid: Vec<PlacementBounds>,
    bounds: PlacementBounds,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    /// How many entries the last drawn plate shed, so the report up is edge
    /// triggered rather than a mail every frame.
    shed_count: u32,
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

    /// The sections wrapped into plate entries — one entry per source line,
    /// in section order — with empty sections contributing nothing, so they
    /// cannot leave a rule with nothing under it.
    ///
    /// The tooltip's **first line is its title** — the name of the thing
    /// being explained — and takes the reading size and the primary ink;
    /// every other line is caption-role and muted, unless the host named a
    /// role or an ink of its own on that line. `TextRole::Title` is
    /// deliberately not used: that is the size a *screen's* one title is set
    /// at, and a 22-pixel line on a hover plate is a headline, not a name.
    fn wrapped_entries(&self) -> Vec<PlateEntry> {
        let measure_width = self.wrap_width();
        let mut first_line = true;
        let mut entries = Vec::new();
        for (index, section) in self.sections.iter().enumerate() {
            for source in &section.lines {
                let role = source.role.unwrap_or(if first_line {
                    TextRole::Body
                } else {
                    TextRole::Caption
                });
                first_line = false;
                let size = self.theme.text_size_pixels(role);
                let lines: Vec<PlateLine> =
                    wrap_to_width_hanging(&source.text, measure_width, self.hanging_indent_pixels, |run| {
                        self.text_width(run, size)
                    })
                    .into_iter()
                    .map(|line| PlateLine { text: line.text, role, indent: line.indent_pixels, ink: source.ink })
                    .collect();
                if !lines.is_empty() {
                    entries.push(PlateEntry { section: index, lines });
                }
            }
        }
        entries
    }

    /// The plate's own size: as wide as its longest wrapped line (indent
    /// included) plus one unit either side, and as tall as the line boxes it
    /// holds plus the rules between its sections and that same unit top and
    /// bottom. Nothing is rounded up to a row height — a plate padded to a
    /// grid is the "too much vertical padding" the note was about.
    fn plate_size(&self, entries: &[PlateEntry]) -> [f32; 2] {
        let pad = self.theme.space(PAD_UNITS);
        let lines = || entries.iter().flat_map(|entry| entry.lines.iter());
        let widest = lines()
            .map(|line| line.indent + self.text_width(&line.text, line.size_pixels(&self.theme)))
            .fold(0.0_f32, f32::max);
        let ink: f32 = lines().map(|line| line.height(&self.theme)).sum();
        let rules = section_breaks(entries) as f32 * self.rule_band();
        [pad.mul_add(2.0, widest), pad.mul_add(2.0, ink + rules)]
    }

    /// Drop trailing whole entries until the plate fits
    /// [`TooltipConfig::max_height_pixels`], reporting how many went.
    ///
    /// The ladder only ever removes from the end, and only ever removes a
    /// whole entry: a card out of room loses its last stat, not the second
    /// half of a sentence. A section left with no entries takes its rule with
    /// it, because the rule is a boundary *between* entries and there is no
    /// longer one there. A budget too small for even the first entry sheds
    /// everything and the plate draws nothing — which is the honest bottom of
    /// the ladder, and the host, which is told the count, is the one that can
    /// say something shorter instead.
    fn shed(&self, entries: &mut Vec<PlateEntry>) -> u32 {
        let budget = self.max_height_pixels;
        if !budget.is_finite() || budget <= 0.0 {
            return 0;
        }
        let mut dropped = 0;
        while !entries.is_empty() && self.plate_size(entries)[1] > budget {
            entries.pop();
            dropped += 1;
        }
        dropped
    }

    /// How much vertical room one section rule takes: the hairline itself
    /// plus one spacing unit either side, the same band a menu's separator
    /// occupies, so a divider between two blocks of text reads as a division
    /// and not as a line of its own.
    fn rule_band(&self) -> f32 {
        self.theme.space(1).mul_add(2.0, RULE_THICKNESS)
    }

    /// The plate, in the widget's own local coordinates, and how many entries
    /// it had to shed to stand. Empty while the tooltip has nothing to say or
    /// no room to say it in.
    fn overlay_items(&self) -> (Vec<WidgetDrawItem>, u32) {
        let mut entries = self.wrapped_entries();
        let dropped = self.shed(&mut entries);
        if entries.is_empty() {
            return (Vec::new(), dropped);
        }
        let [width, height] = self.plate_size(&entries);
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return (Vec::new(), dropped);
        }

        let origin = place_plate_avoiding(
            PlacementBounds::from(&self.frame),
            width,
            height,
            self.side,
            self.theme.space(1),
            self.bounds,
            &self.avoid,
        );
        let (left, top) = (origin[0] - self.frame.x, origin[1] - self.frame.y);
        let pad = self.theme.space(PAD_UNITS);

        let mut items = Vec::new();
        items.push(quad(left, top, width, height, self.theme.surface_raised));
        push_rect_border(&mut items, left, top, width, height, RULE_THICKNESS, self.theme.outline);

        let mut line_top = top + pad;
        let mut section = entries.first().map_or(0, |entry| entry.section);
        for entry in &entries {
            if entry.section != section {
                items.push(quad(
                    left + pad,
                    line_top + self.theme.space(1),
                    (pad.mul_add(-2.0, width)).max(0.0),
                    RULE_THICKNESS,
                    self.theme.outline,
                ));
                line_top += self.rule_band();
                section = entry.section;
            }
            for line in &entry.lines {
                let size = line.size_pixels(&self.theme);
                let line_height = line.height(&self.theme);
                items.push(WidgetDrawItem::Text {
                    x: left + pad + line.indent,
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
        (items, dropped)
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
/// it reports no value up, because a plate that explains something has none of
/// its own — only [`TooltipShed`], which is about the plate rather than about
/// what it says.
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
            max_height_pixels: config.max_height_pixels,
            hanging_indent_pixels: config.hanging_indent_pixels,
            side: config.side,
            avoid: config.avoid,
            bounds: config.bounds,
            theme: config.theme,
            state: InteractionState::new(config.state),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            font_metrics: FontMetricsAdapter::new(desired_font_id),
            shed_count: 0,
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
        self.max_height_pixels = config.max_height_pixels;
        self.hanging_indent_pixels = config.hanging_indent_pixels;
        self.side = config.side;
        self.avoid = config.avoid;
        self.bounds = config.bounds;
        // New words shed on their own terms: forgetting the old count is what
        // makes the next collect report this card's tail, even when it happens
        // to lose as many entries as the last one did.
        self.shed_count = 0;
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
    /// it is explaining. A change in how much the plate had to shed is
    /// reported up first, so the host reads it before the frame it belongs to
    /// is drawn.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        let (overlay, dropped) = self.overlay_items();
        if self.shed_count != dropped {
            self.shed_count = dropped;
            if let Some(parent) = ctx.parent() {
                parent.send(&TooltipShed { dropped });
            }
        }
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { intrinsic: None, items: Vec::new(), overlay });
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
            max_height_pixels: 0.0,
            hanging_indent_pixels: 0.0,
            side: PlacementSide::Below,
            avoid: Vec::new(),
            bounds: PlacementBounds { x: 0.0, y: 0.0, width: 360.0, height: 300.0 },
            theme: Theme::DEFAULT,
            frame: WidgetFrame { x: 20.0, y: 100.0, width: 200.0, height: 20.0 },
            state: InteractionState::new(WidgetControlState::default()),
            font_metrics: FontMetricsAdapter::new(0),
            shed_count: 0,
        }
    }

    fn section(lines: &[&str]) -> TooltipSection {
        TooltipSection::new(lines.iter().copied())
    }

    /// The plate's entries as their drawn text, one string per entry — the
    /// unit the shed ladder works in.
    fn entry_text(widget: &TooltipWidget) -> Vec<String> {
        let mut entries = widget.wrapped_entries();
        widget.shed(&mut entries);
        entries.iter().map(|entry| entry.lines.iter().map(|line| line.text.as_str()).collect()).collect()
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
        let entries = widget.wrapped_entries();
        assert_eq!(entries.len(), 2, "both sections survive, one entry each");
        assert!(entries[1].lines.len() > 1, "the sentence wrapped onto its own lines");

        let ink: f32 = entries.iter().flat_map(|entry| entry.lines.iter()).map(|line| line.height(&widget.theme)).sum();
        let [_, height] = widget.plate_size(&entries);
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
        let entries = widget.wrapped_entries();
        assert_eq!(entries[0].lines[0].role, TextRole::Body);
        assert_eq!(entries[0].lines[0].ink(&widget.theme), widget.theme.text_primary);
        assert_eq!(entries[1].lines[0].role, TextRole::Caption);
        assert_eq!(entries[1].lines[0].ink(&widget.theme), widget.theme.text_muted);
    }

    #[test]
    fn a_line_that_names_its_own_ink_keeps_the_role_and_takes_that_ink() {
        // Tripwire: a card marks *which* of its lines the search matched and
        // which stat is not being counted, and both distinctions are inside one
        // role. An ink that overrode the role's size, or a role that overrode
        // the host's ink, would collapse one of them again.
        let matched = Rgba::new(1.0, 0.8, 0.2, 1.0);
        let widget = tooltip(vec![TooltipSection {
            lines: vec![
                TooltipLine::from("Life"),
                TooltipLine { text: String::from("the line the search hit"), role: None, ink: Some(matched) },
            ],
        }]);
        let entries = widget.wrapped_entries();
        assert_eq!(entries[0].lines[0].ink(&widget.theme), widget.theme.text_primary, "an unmarked line is unchanged");
        assert_eq!(entries[1].lines[0].role, TextRole::Caption, "the marked line keeps the role its place gives it");
        assert_eq!(entries[1].lines[0].ink(&widget.theme), matched, "and draws in the ink the host named");
    }

    #[test]
    fn a_wrapped_line_indents_its_continuations_and_nothing_else() {
        // Tripwire: the indent belongs to the continuation rows only. Indenting
        // the first row too is a margin, not a hanging indent, and a two-row
        // stat then reads as a block rather than as one stat.
        let mut widget = tooltip(vec![section(&["A stat line long enough to need a second row of its own"])]);
        widget.hanging_indent_pixels = 12.0;
        let entry = widget.wrapped_entries().remove(0);
        assert!(entry.lines.len() > 1, "the line wrapped");
        assert_eq!(entry.lines[0].indent, 0.0, "the first row starts at the margin");
        assert!(entry.lines[1..].iter().all(|line| line.indent == 12.0), "and every continuation is inset");

        let size = entry.lines[1].size_pixels(&widget.theme);
        assert!(
            widget.text_width(&entry.lines[1].text, size) <= widget.wrap_width() - 12.0,
            "a continuation wraps that much earlier, so the right edge does not move",
        );
    }

    #[test]
    fn an_empty_section_leaves_no_rule_behind_and_nothing_to_say_draws_nothing() {
        // Tripwire: a rule with no text under it is a plate that looks broken;
        // a plate with no text at all is chrome a reader never asked for.
        let widget = tooltip(vec![section(&["Life"]), TooltipSection::default()]);
        let entries = widget.wrapped_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(section_breaks(&entries), 0, "one surviving section crosses no boundary, so it draws no rule");

        let quiet = tooltip(Vec::new());
        assert!(quiet.overlay_items().0.is_empty());
    }

    #[test]
    fn a_plate_over_its_height_sheds_trailing_whole_entries_and_says_how_many() {
        // Tripwire: the shed is the difference between a card that says less
        // and a card that overprints its own bottom. It has to drop from the
        // end, drop whole entries — a reader must never get a stat ending in
        // "per" — and report the count, because only the host knows what the
        // dropped entries said.
        let mut widget = tooltip(vec![section(&["Life", "100 per level", "20 from gear", "5 from the tree"])]);
        assert_eq!(entry_text(&widget).len(), 4, "no budget sheds nothing");

        let full = widget.plate_size(&widget.wrapped_entries())[1];
        let last_two: f32 = widget
            .wrapped_entries()
            .iter()
            .skip(2)
            .flat_map(|entry| entry.lines.iter())
            .map(|line| line.height(&widget.theme))
            .sum();
        widget.max_height_pixels = full - last_two;

        let mut entries = widget.wrapped_entries();
        assert_eq!(widget.shed(&mut entries), 2, "two entries go, and the plate then fits");
        assert_eq!(entry_text(&widget), vec![String::from("Life"), String::from("100 per level")]);
        assert!(widget.plate_size(&entries)[1] <= widget.max_height_pixels);

        widget.max_height_pixels = 1.0;
        assert_eq!(widget.shed(&mut widget.wrapped_entries()), 4, "a budget nothing fits in sheds everything");
        assert!(widget.overlay_items().0.is_empty(), "and the plate draws nothing rather than one clipped row");
    }

    #[test]
    fn the_plate_stands_clear_of_its_anchor_and_inside_the_bounds() {
        // Tripwire: the plate must never cover the row it explains, and never
        // reach past the region the host allowed it — the two failures that
        // make a tooltip worse than none.
        let mut widget = tooltip(vec![section(&["Life"]), section(&["A pool of health."])]);
        widget.frame = WidgetFrame { x: 20.0, y: 280.0, width: 200.0, height: 20.0 };
        let (items, _) = widget.overlay_items();
        let plate = items.first().expect("a plate is drawn");
        let WidgetDrawItem::Quad { y, height, .. } = plate else {
            panic!("the plate leads with its fill: {plate:?}");
        };
        // Local coordinates: the frame's own origin is zero.
        assert!(y + height <= 0.0, "the plate flipped above the row it is about: {y} + {height}");
        assert!(y + widget.frame.y >= 0.0, "and stayed inside the region: {y}");
    }
}
