// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (the full rationale is on the same allow in `lib.rs`).
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
//!
//! [`TooltipLine::icon`] is the fifth: a mark drawn **before** the line's
//! words, because some things are recognized by their colour and shape before
//! they are read — an instilled gem is its icon first and its name second. The
//! host registers the image through `aether.render.create_texture` and hands
//! the plate the id; the plate scales it to the line's own cap band, keeps its
//! aspect, and takes the room for it out of that line's measure, so an icon
//! makes a line wrap earlier rather than run past the box.
//!
//! # How a wrapped line and a paragraph differ
//!
//! Round-4 note 19: "the spaces after a line break are a bit weird. I think
//! I'd prefer if it was aligned to the first line of text and then new
//! paragraphs just had a break (empty line)." So the plate keeps two rules,
//! and they are opposites on purpose:
//!
//! - A line that **wrapped** is one thought that ran out of measure. Its
//!   continuation rows start flush with its first row — the kit's default,
//!   [`TooltipConfig::hanging_indent_pixels`] at `0` — because a wrapped
//!   sentence indented in the middle reads as a new item beginning.
//! - A **new paragraph** is a new thought, and takes a blank row. A
//!   [`TooltipLine`] with no words in it is exactly that blank row, and so is
//!   a blank line inside one line's own text: `"first\n\nsecond"` is two
//!   paragraphs with one empty row between them.
//!
//! Neither is a section: a [`TooltipSection`] boundary is a **rule**, which is
//! round-2 note 24's answer and stays. A blank row divides two paragraphs of
//! one block; a rule divides two blocks.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_math::Rgba;
use aether_text::FontMetricsResult;
use serde::{Deserialize, Serialize};

use crate::set::placement::{PlacementBounds, PlacementSide, place_plate_avoiding};
use crate::set::{
    WidgetDefaults, accept_font_metrics_result, apply_text_theme, approx_text_width, measured_text_width,
    pump_text_font_metrics, push_rect_border, quad, reply_if_hidden, reveal_wrap_width, text_baseline_y,
    text_cap_height, text_origin_y, wrap_to_width_hanging,
};
use crate::state::{InteractionState, emit_state_changed};
use crate::text_edit::FontMetricsAdapter;
use crate::theme::{SetTheme, TextRole, Theme};
use crate::{Collect, SetWidgetState, WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame};

/// A mark drawn inline at the head of a [`TooltipLine`], before its words.
///
/// It exists because some things are recognized by their colour and shape
/// before they are read at all — an instilled gem, a rarity, a damage type —
/// and a card that names them in words makes the reader translate back. The
/// host owns the texture: it registers the image once through
/// `aether.render.create_texture` and hands the tooltip the session id it got
/// back, along with the texture's **own** pixel size, which is what the plate
/// preserves the aspect of. The widget draws it and nothing else — it never
/// creates, updates, or destroys a texture.
///
/// The drawn size is not `width_pixels` × `height_pixels`: the icon is scaled
/// to the line's own cap band ([`text_cap_height`]) with its aspect kept, so a
/// 64-pixel icon and a 16-pixel one both stand exactly as tall as the capitals
/// beside them. Schema-only; nested in [`TooltipLine`].
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Default)]
pub struct TooltipIcon {
    /// The session-scoped texture id `aether.render.create_texture` replied
    /// with. Non-owning: the host that made it keeps it alive.
    pub texture_id: u32,
    /// The texture's own width in pixels — the numerator of the aspect the
    /// scaled icon keeps, not the width it is drawn at.
    pub width_pixels: f32,
    /// The texture's own height in pixels.
    pub height_pixels: f32,
}

/// The `[width, height]` `icon` is drawn at on a line set at `size_pixels`:
/// the line's cap band tall, aspect preserved. `None` for an icon whose
/// declared size is not a positive, finite rectangle — a plate would rather
/// draw the words alone than a mark of unknowable shape.
fn scaled_icon(icon: TooltipIcon, size_pixels: f32) -> Option<[f32; 2]> {
    let (declared_width, declared_height) = (icon.width_pixels, icon.height_pixels);
    if !declared_width.is_finite() || !declared_height.is_finite() || declared_width <= 0.0 || declared_height <= 0.0 {
        return None;
    }
    let height = text_cap_height(size_pixels);
    let width = height * (declared_width / declared_height);
    (height.is_finite() && height > 0.0 && width.is_finite() && width > 0.0).then_some([width, height])
}

/// How much of a line's measure its icon takes: the scaled icon plus one
/// spacing unit of clear space before the words. Zero when there is no icon,
/// so a line without one is measured exactly as it was.
fn icon_footprint(icon: Option<TooltipIcon>, size_pixels: f32, gap: f32) -> f32 {
    icon.and_then(|icon| scaled_icon(icon, size_pixels)).map_or(0.0, |[width, _]| width + gap)
}

/// One line of a tooltip section, as the host wrote it: the words, the icon
/// that stands before them, and the two presentation escapes a hover card
/// needs (the studio's gap 18).
///
/// Every option is `None` by default, which is the kit's own rule — the
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
    /// The mark drawn at the head of this line, before its words: the icon of
    /// the thing the line is about, when the thing is recognized by its colour
    /// and shape faster than by its name.
    ///
    /// It takes the line's first row only — a wrapped line is one thought, and
    /// one thought has one icon — and the words start one spacing unit after
    /// it. The line's measure shrinks by that footprint, so an icon makes a
    /// line wrap earlier rather than run past the plate, and the continuation
    /// rows are inset to the words' own start, so a wrapped line reads as one
    /// entry indented under its icon. An icon on a line with **no words** is a
    /// paragraph break and draws nothing: a break is a break.
    #[serde(default)]
    pub icon: Option<TooltipIcon>,
}

impl From<String> for TooltipLine {
    fn from(text: String) -> Self {
        Self { text, role: None, ink: None, icon: None }
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
    ///
    /// A line with **no words in it is a paragraph break** and draws one empty
    /// row (round-4 note 19), so a section whose lines are paragraphs is
    /// written with the blanks in it:
    /// `TooltipSection::new(["First.", "", "Second."])`. A blank at the very
    /// top or bottom of the plate is dropped, the same rule
    /// [`wrap_to_width_hanging`] applies
    /// inside one line — a break needs something on both sides of it to be
    /// a break.
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
    /// How far the continuation rows of a wrapped line are inset. `0` — the
    /// default, and what the kit's own plates use — is a **flush** block: a
    /// sentence that wrapped stays aligned with the row it started on, which
    /// is what round-4 note 19 asked for. A hanging indent is the opt-in for
    /// the one case that wants it: a list of stats, where an inset
    /// continuation makes a two-row stat read as one stat rather than as two
    /// lines that happen to be adjacent.
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
/// starts at (the icon's footprint, plus the hanging indent on a
/// continuation), the ink the host asked for if it asked for one, and the icon
/// this row draws before its words. A row with no `text` is a paragraph break:
/// it takes its line box and draws nothing in it.
struct PlateLine {
    text: String,
    role: TextRole,
    indent: f32,
    ink: Option<Rgba>,
    icon: Option<TooltipIcon>,
}

impl PlateLine {
    fn size_pixels(&self, theme: &Theme) -> f32 {
        theme.text_size_pixels(self.role)
    }

    /// The icon this row draws and the `[width, height]` it is drawn at, or
    /// `None` for a row with no icon or an unusable one.
    fn icon_draw(&self, theme: &Theme) -> Option<(u32, [f32; 2])> {
        let icon = self.icon?;
        scaled_icon(icon, self.size_pixels(theme)).map(|size| (icon.texture_id, size))
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

impl PlateEntry {
    /// Whether this entry is a paragraph break rather than words.
    fn is_break(&self) -> bool {
        self.lines.iter().all(|line| line.text.is_empty())
    }
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
    ///
    /// A source line with no words in it is a **paragraph break** (round-4
    /// note 19): it becomes one empty row, at the caption line box, rather
    /// than vanishing. It is not the title even when it comes first, and a
    /// break at either end of the plate is dropped — a break needs something
    /// on both sides of it to be one.
    fn wrapped_entries(&self) -> Vec<PlateEntry> {
        let measure_width = self.wrap_width();
        let mut first_line = true;
        let mut entries: Vec<PlateEntry> = Vec::new();
        for (index, section) in self.sections.iter().enumerate() {
            for source in &section.lines {
                let is_break = source.text.trim().is_empty();
                let role = source.role.unwrap_or(if first_line && !is_break {
                    TextRole::Body
                } else {
                    TextRole::Caption
                });
                first_line = first_line && is_break;
                let size = self.theme.text_size_pixels(role);
                let footprint = icon_footprint(source.icon, size, self.theme.space(1));
                let lines: Vec<PlateLine> = if is_break {
                    alloc::vec![PlateLine { text: String::new(), role, indent: 0.0, ink: source.ink, icon: None }]
                } else {
                    wrap_to_width_hanging(&source.text, measure_width - footprint, self.hanging_indent_pixels, |run| {
                        self.text_width(run, size)
                    })
                    .into_iter()
                    .enumerate()
                    .map(|(row, line)| PlateLine {
                        text: line.text,
                        role,
                        indent: footprint + line.indent_pixels,
                        ink: source.ink,
                        icon: (row == 0).then_some(source.icon).flatten(),
                    })
                    .collect()
                };
                if !lines.is_empty() {
                    entries.push(PlateEntry { section: index, lines });
                }
            }
        }
        while entries.first().is_some_and(PlateEntry::is_break) {
            entries.remove(0);
        }
        while entries.last().is_some_and(PlateEntry::is_break) {
            entries.pop();
        }
        entries
    }

    /// The plate's own size: as wide as its longest wrapped line (indent, and
    /// so any icon's footprint, included) plus one unit either side, and as
    /// tall as the line boxes it
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
                // The icon sits in the line's cap band: its bottom on the
                // baseline, its top where a capital's is, so it reads as part
                // of the line rather than as a picture beside it.
                if let Some((texture_id, [icon_width, icon_height])) = line.icon_draw(&self.theme) {
                    items.push(WidgetDrawItem::TexturedQuad {
                        texture_id,
                        x: left + pad,
                        y: text_baseline_y(line_top, line_height, size) - icon_height,
                        width: icon_width,
                        height: icon_height,
                        u0: 0.0,
                        v0: 0.0,
                        u1: 1.0,
                        v1: 1.0,
                        tint: Rgba::WHITE,
                        clip: None,
                    });
                }
                // A paragraph break takes its row and draws nothing in it.
                if !line.text.is_empty() {
                    items.push(WidgetDrawItem::Text {
                        x: left + pad + line.indent,
                        y: text_origin_y(line_top, line_height, size),
                        font_id: self.theme.font_id,
                        text: line.text.clone(),
                        size_pixels: size,
                        color: line.ink(&self.theme),
                        clip: None,
                    });
                }
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
/// Hide it with `aether.kit.widget.set_state`. A line's `icon` is a texture id
/// the host got from `aether.render.create_texture`; register the image first
/// and pass the texture's own pixel size, not the size you want it drawn at.
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
            parent.send(&WidgetDrawList { content_height: None, intrinsic: None, items: Vec::new(), overlay });
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
                TooltipLine {
                    text: String::from("the line the search hit"),
                    role: None,
                    ink: Some(matched),
                    icon: None,
                },
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
    fn an_icon_widens_the_plate_by_its_own_footprint_and_the_words_start_after_it() {
        // Tripwire: the icon has to take room *and* be paid for. A plate whose
        // width ignored the icon would draw the mark over its own left pad or
        // push the words past the box; a text x that ignored it would print the
        // words on top of the mark. Both are the same missing footprint.
        let icon = TooltipIcon { texture_id: 7, width_pixels: 64.0, height_pixels: 32.0 };
        let plain = tooltip(vec![section(&["Cold"])]);
        let marked = tooltip(vec![TooltipSection {
            lines: vec![TooltipLine { text: String::from("Cold"), role: None, ink: None, icon: Some(icon) }],
        }]);

        let size = marked.theme.text_size_pixels(TextRole::Body);
        let [icon_width, icon_height] = scaled_icon(icon, size).expect("a positive icon scales");
        assert!((icon_height - text_cap_height(size)).abs() < f32::EPSILON, "the icon stands in the cap band");
        assert!(icon_height.mul_add(-2.0, icon_width).abs() < 1e-3, "and keeps its own 2:1 aspect");

        let footprint = icon_width + marked.theme.space(1);
        let grew = marked.plate_size(&marked.wrapped_entries())[0] - plain.plate_size(&plain.wrapped_entries())[0];
        assert!((grew - footprint).abs() < 1e-3, "the plate grew by the icon and one gap: {grew} vs {footprint}");

        let (items, _) = marked.overlay_items();
        let drawn = items
            .iter()
            .find_map(|item| match item {
                WidgetDrawItem::TexturedQuad { texture_id, x, width, .. } => Some((*texture_id, *x, *width)),
                _ => None,
            })
            .expect("the icon is drawn");
        let words = items
            .iter()
            .find_map(|item| match item {
                WidgetDrawItem::Text { x, .. } => Some(*x),
                _ => None,
            })
            .expect("the words are drawn");
        assert_eq!(drawn.0, 7, "the host's own texture id, borrowed not owned");
        assert!((words - (drawn.1 + drawn.2 + marked.theme.space(1))).abs() < 1e-3, "the words start after the icon");
    }

    #[test]
    fn a_wrapped_line_with_an_icon_hangs_its_continuations_at_the_words_start() {
        // Tripwire: round-4 note 19's flush rule is about the *words*. With an
        // icon in front of them, a continuation row that started at the plate's
        // pad would run back under the mark, and the line would read as two.
        let icon = TooltipIcon { texture_id: 3, width_pixels: 32.0, height_pixels: 32.0 };
        let widget = tooltip(vec![TooltipSection {
            lines: vec![TooltipLine {
                text: String::from("Anger instilled, adding a good deal of fire damage to every hit"),
                role: None,
                ink: None,
                icon: Some(icon),
            }],
        }]);

        let entry = widget.wrapped_entries().remove(0);
        assert!(entry.lines.len() > 1, "the line wrapped");
        let footprint =
            scaled_icon(icon, entry.lines[0].size_pixels(&widget.theme)).expect("scales")[0] + widget.theme.space(1);
        assert!((entry.lines[0].indent - footprint).abs() < 1e-3, "the first row starts after the icon");
        assert!(
            entry.lines[1..].iter().all(|line| (line.indent - footprint).abs() < 1e-3),
            "and every continuation starts at that same x, not back under the mark",
        );
        assert!(entry.lines[1].icon.is_none(), "one thought, one icon: only the first row draws it");
        let size = entry.lines[1].size_pixels(&widget.theme);
        assert!(
            widget.text_width(&entry.lines[1].text, size) <= widget.wrap_width() - footprint,
            "and the measure shrank by the footprint, so the right edge did not move",
        );
    }

    #[test]
    fn a_blank_line_is_a_paragraph_break_worth_exactly_one_empty_row() {
        // Tripwire: round-4 note 19 — "new paragraphs just had a break (empty
        // line)". A blank line used to wrap to nothing and vanish, so a host
        // that wrote its paragraphs with the breaks in them got one run-on
        // block. The break has to cost one line box, draw nothing in it, and
        // not become the plate's title by arriving first.
        let run_on = tooltip(vec![section(&["Life", "Your health pool."])]);
        let broken = tooltip(vec![section(&["Life", "", "Your health pool."])]);

        let entries = broken.wrapped_entries();
        assert_eq!(entries.len(), 3, "the break is an entry of its own: {:?}", entry_text(&broken));
        assert!(entries[1].is_break());
        assert_eq!(entries[1].lines.len(), 1, "one row, not two and not none");
        assert_eq!(entries[0].lines[0].role, TextRole::Body, "the first words are still the name");
        assert_eq!(entries[2].lines[0].role, TextRole::Caption);

        let gap = entries[1].lines[0].height(&broken.theme);
        assert_eq!(gap, broken.theme.caption_size_pixels * LINE_LEADING, "a break is one caption line box");
        let grew = broken.plate_size(&entries)[1] - run_on.plate_size(&run_on.wrapped_entries())[1];
        assert!((grew - gap).abs() < f32::EPSILON, "the plate grew by exactly that row: {grew} vs {gap}");
        assert!(
            broken
                .overlay_items()
                .0
                .iter()
                .all(|item| !matches!(item, WidgetDrawItem::Text { text, .. } if text.is_empty())),
            "and the empty row draws no glyph run",
        );

        // A break inside one line's own text is the same break.
        let inline = tooltip(vec![section(&["Life\n\nYour health pool."])]);
        let inline_entries = inline.wrapped_entries();
        assert_eq!(inline_entries.len(), 1, "one source line is still one shed unit");
        assert_eq!(inline_entries[0].lines[1].text, "", "with an empty row between its paragraphs");
    }

    #[test]
    fn a_break_at_either_end_of_the_plate_is_not_a_break() {
        // Tripwire: the same rule `wrap_to_width_hanging` keeps inside one
        // line. A blank at the top or the bottom is padding nobody asked for,
        // and a leading one must not take the title's role with it.
        let padded = tooltip(vec![section(&["", "Life", "A pool.", ""])]);
        let entries = padded.wrapped_entries();
        assert_eq!(entry_text(&padded), vec![String::from("Life"), String::from("A pool.")]);
        assert_eq!(entries[0].lines[0].role, TextRole::Body, "the first words are the name, not the blank above it");
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
