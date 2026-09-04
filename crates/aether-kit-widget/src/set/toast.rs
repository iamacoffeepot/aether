// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]
// A region holds a handful of notices of a handful of lines; the `usize as
// f32` for a plate's pixel offset cannot lose precision at any standing count
// a reader would tolerate.
#![allow(clippy::cast_precision_loss)]

//! The toast region: the one place a refusal or a confirmation appears.
//!
//! The owner's round-2 note 28 is three complaints in one — "refusals need
//! their own place they pop up that indicates like, red or orange … the place
//! where it is relative to other elements is not obvious, and it lingers way
//! too long — should go away after a certain amount of time". So a notice is
//! not drawn where there happened to be room: it goes to **one region** the
//! reader learns once, it is coloured by what it is, and it leaves on its
//! own. Round-3 notes 11 and 12 are the two corrections that followed —
//! "toaster text cuts off too soon, not large enough, can be fatter
//! vertically" (so a notice **wraps** at the region's width and the plate
//! grows down, instead of eliding), and "color of toaster tab (yellow) is the
//! same as … everything else, so it's non-indicative" (so the severity bar is
//! [`Theme::info`] / [`Theme::warning`] / [`Theme::error`] and never the
//! accent).
//!
//! Anything can raise one: [`ToastNotice`] is an ordinary mail, so a
//! save result, a planner refusal, and a confirmation all arrive through the
//! same door and land in the same place.
//!
//! Round-4 note 15 — "toast text can be larger" — is [`ToastConfig::role`]:
//! the region names a step on the theme's type scale and the theme resolves
//! the size, rather than the region carrying a pixel size of its own. The
//! plate follows the role it is given, line box and wrap measure alike.
//!
//! # How a widget tells the time
//!
//! It counts the frames it is asked to draw. Widgets never subscribe to the
//! frame stage — the root does, and the root's per-frame
//! [`Collect`] is the only regular pulse that
//! reaches a child, so a notice's life is counted in `Collect`s and
//! [`ToastConfig::lifetime_frames`] says how many. Ageing happens before the
//! draw and before the hidden-widget branch, so a hidden region still runs
//! its clock down rather than saving up a stack of stale refusals.
//!
//! The plates draw in the **overlay** ([`WidgetDrawList::overlay`]): a notice
//! stands over the primary view rather than taking a row away from the
//! controls, and the root's clip subtraction keeps the glyphs under it from
//! printing through — no draw layer anywhere.

use alloc::string::String;
use alloc::vec::Vec;
use core::mem;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_math::Rgba;
use aether_text::FontMetricsResult;
use serde::{Deserialize, Serialize};

use crate::set::{
    WidgetDefaults, accept_font_metrics_result, apply_text_theme, approx_text_width, measured_text_width,
    pump_text_font_metrics, push_rect_border, quad, reply_if_hidden, text_origin_y, wrap_to_width,
};
use crate::state::{InteractionState, emit_state_changed};
use crate::text_edit::FontMetricsAdapter;
use crate::theme::{SetTheme, TextRole, Theme};
use crate::{Collect, SetWidgetState, WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame};

/// What a notice is: a report, a caution, or a failure. The three are drawn
/// in three colours that mean those three things and nothing else — the
/// severity is the whole reason a notice region beats a line of text
/// wherever there was room.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToastSeverity {
    /// It worked, or here is something you asked to be told. A cool blue-grey
    /// bar ([`Theme::info`]).
    #[default]
    Info,
    /// It worked partly, or it will not work for long. An orange bar
    /// ([`Theme::warning`]).
    Warning,
    /// It did not work. A red bar ([`Theme::error`]).
    Error,
}

impl ToastSeverity {
    /// The ink this severity's bar is drawn in. Never `accent`: the primary
    /// action and a failure must not share a colour.
    fn bar_ink(self, theme: &Theme) -> Rgba {
        match self {
            Self::Info => theme.info,
            Self::Warning => theme.warning,
            Self::Error => theme.error,
        }
    }
}

/// `aether.kit.widget.toast.notice` — raise one transient notice in the
/// region. Send it to the toast widget from anywhere: the region stacks it
/// newest-first, wraps it at the region's width, and drops it on its own
/// after [`ToastConfig::lifetime_frames`] frames. An empty `text` raises
/// nothing, because a plate with no line on it is a flash a reader cannot
/// read.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.toast.notice")]
pub struct ToastNotice {
    pub severity: ToastSeverity,
    pub text: String,
}

/// `aether.kit.widget.toast.config` — the region transient notices appear
/// in. `max_standing` is how many stand at once before the oldest leaves to
/// make room, and `lifetime_frames` is how long each one stays, counted in
/// the root's per-frame `Collect`s (240 at the desktop's sixty a second is
/// four seconds — long enough to read a line, short enough that a stale
/// refusal is never mistaken for a description of the screen).
///
/// The widget's assigned [`WidgetFrame`] is the
/// region: notices stack down from its top edge at its width, so a host puts
/// notices where it wants them by placing the slot.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.toast.config")]
pub struct ToastConfig {
    /// How many notices stand at once. Zero means the region is a sink: it
    /// accepts notices and shows none, which is what a screen with the region
    /// switched off should do rather than growing an unbounded backlog.
    pub max_standing: u32,
    /// How many `Collect` frames a notice stands for before it leaves. Zero
    /// keeps every notice until the cap pushes it out.
    pub lifetime_frames: u32,
    /// The type step a notice's line is set at. [`TextRole::Body`] — the
    /// reading size, which is what the region drew before this field existed —
    /// unless a host asks for another.
    ///
    /// Round-4 note 15 is one word long: "toast text can be larger." The size
    /// is a *theme* fact, not a toast fact — the kit has one type scale and a
    /// widget names its step on it rather than carrying a pixel size of its
    /// own — so the region takes a role and the theme resolves it. The whole
    /// plate follows: the line box, the wrap measure, and therefore how far
    /// down the region the stack reaches.
    #[serde(default)]
    pub role: TextRole,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

impl Default for ToastConfig {
    fn default() -> Self {
        Self {
            max_standing: DEFAULT_MAX_STANDING,
            lifetime_frames: DEFAULT_LIFETIME_FRAMES,
            role: TextRole::default(),
            theme: Theme::default(),
            state: WidgetControlState::default(),
        }
    }
}

/// `aether.kit.widget.toast.region_changed` — the standing stack changed:
/// one arrived, one aged out, or the cap pushed one off the end. `standing`
/// is how many are up now and `height_pixels` how far down the region they
/// reach, so a host that has to tell another actor what is covered (a tree
/// view being drawn under the notices) reports that rectangle without
/// re-deriving the stack's geometry. Emitted on the edge only, never every
/// frame.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[kind(name = "aether.kit.widget.toast.region_changed")]
pub struct ToastRegionChanged {
    pub standing: u32,
    pub height_pixels: f32,
}

/// How many notices stand at once by default. A fourth would be a column of
/// text down the primary view; the oldest leaves to make room.
const DEFAULT_MAX_STANDING: u32 = 3;

/// How many frames a notice stands for by default — four seconds at sixty a
/// second.
const DEFAULT_LIFETIME_FRAMES: u32 = 240;

/// The plate's padding, in spacing units — the owner asked for one unit
/// around the line, and a notice is one line's worth of chrome or it is a
/// dialog.
const PAD_UNITS: u8 = 1;

/// How far clear of the severity bar the line starts, in spacing units.
///
/// Round-8 note 17: "toaster left text padding can be increased a tad. Feels
/// kinda like there's not enough breathing room." One unit put the first
/// glyph four pixels from three pixels of colour, and the two read as one
/// crowded edge; two units separate the bar from the sentence it is
/// colouring. It is the *left* inset alone — the bar already occupies the
/// plate's left edge, so matching the right pad to it would push the line
/// off-centre rather than balance it — and it is charged against the wrap
/// measure, so the plate still grows from the text it actually holds.
const TEXT_INSET_UNITS: u8 = 2;

/// How tall one wrapped line's box is, as a multiple of its own type size.
const LINE_LEADING: f32 = 1.4;

/// The hairline a plate's ring is drawn at.
const RING_THICKNESS: f32 = 1.0;

/// One notice, standing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Standing {
    severity: ToastSeverity,
    text: String,
    /// Frames left before it goes on its own; `None` for a region whose
    /// lifetime is zero, where only the cap removes a notice.
    frames_left: Option<u32>,
}

impl Standing {
    /// This notice one frame older, or nothing when its time is up.
    fn aged(self) -> Option<Self> {
        match self.frames_left {
            None => Some(self),
            Some(frames_left) => {
                let frames_left = frames_left.checked_sub(1)?;
                (frames_left > 0).then_some(Self { frames_left: Some(frames_left), ..self })
            }
        }
    }
}

/// The toast region widget. Holds the standing notices newest-first plus the
/// cached theme, frame, and font metrics it wraps them against.
pub struct ToastWidget {
    /// Newest first, so the plate a reader's eye lands on is the one that
    /// just happened.
    standing: Vec<Standing>,
    max_standing: usize,
    lifetime_frames: u32,
    /// The type step every notice's line is set at.
    role: TextRole,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    /// Single-flight exact metrics for the active theme font: the wrap is the
    /// whole point of the region, so it wants real advances.
    font_metrics: FontMetricsAdapter,
}

impl ToastWidget {
    /// The pixel size the region's configured role comes to in the live
    /// theme. One lookup, so the wrap, the line box, and the draw cannot be
    /// set at three different sizes.
    fn size_pixels(&self) -> f32 {
        self.theme.text_size_pixels(self.role)
    }

    /// One line's width at the region's own type size: measured once the
    /// font's metrics resolve, approximated for the frame or two before that.
    fn text_width(&self, text: &str) -> f32 {
        let size = self.size_pixels();
        self.font_metrics.resolved().map_or_else(
            || approx_text_width(text.chars().count(), size),
            |metrics| measured_text_width(metrics, text, size),
        )
    }

    /// The width a notice's text has to run in: the region minus the severity
    /// bar, the inset that holds the line clear of it, and the plate's own
    /// padding at the right edge.
    fn text_width_budget(&self) -> f32 {
        (self.frame.width - self.text_left() - self.theme.space(PAD_UNITS)).max(0.0)
    }

    /// The local x a notice's line starts at: past the severity bar and the
    /// inset that keeps it off the colour.
    fn text_left(&self) -> f32 {
        self.bar_width() + self.theme.space(TEXT_INSET_UNITS)
    }

    /// The severity bar's width. Derived from the spacing unit rather than
    /// fixed, so a theme scaled for a dense display scales the bar with it.
    fn bar_width(&self) -> f32 {
        (self.theme.space_unit_pixels * BAR_UNIT_RATIO).max(1.0)
    }

    /// One line's box height at the region's own type size.
    fn line_height(&self) -> f32 {
        self.size_pixels() * LINE_LEADING
    }

    /// One notice wrapped to the region's width. Never elided: the round-3
    /// note is that a cut-off notice says less than nothing, so the plate
    /// grows downward instead.
    fn wrapped(&self, notice: &Standing) -> Vec<String> {
        wrap_to_width(&notice.text, self.text_width_budget(), |run| self.text_width(run))
    }

    /// The plate heights of the standing notices, newest first.
    fn plate_heights(&self) -> Vec<f32> {
        let pad = self.theme.space(PAD_UNITS);
        self.standing
            .iter()
            .map(|notice| pad.mul_add(2.0, self.wrapped(notice).len().max(1) as f32 * self.line_height()))
            .collect()
    }

    /// How far down the region the whole standing stack reaches — the
    /// rectangle a host reports as covered. Zero when nothing stands.
    fn stack_height(&self) -> f32 {
        let heights = self.plate_heights();
        if heights.is_empty() {
            return 0.0;
        }
        ((heights.len() - 1) as f32).mul_add(self.theme.space(1), heights.iter().sum::<f32>())
    }

    /// What the region owes its host after a change: how many notices stand
    /// and how far down they reach.
    fn region_changed(&self) -> ToastRegionChanged {
        ToastRegionChanged {
            standing: u32::try_from(self.standing.len()).unwrap_or(u32::MAX),
            height_pixels: self.stack_height(),
        }
    }

    /// Raise a notice: newest on top, oldest off the end when the cap is
    /// reached. Reports whether the stack changed at all — an empty notice or
    /// a region with no room changes nothing and stays silent.
    fn raise(&mut self, notice: ToastNotice) -> bool {
        if notice.text.is_empty() || self.max_standing == 0 {
            return false;
        }
        let frames_left = (self.lifetime_frames > 0).then_some(self.lifetime_frames);
        self.standing.insert(0, Standing { severity: notice.severity, text: notice.text, frames_left });
        self.standing.truncate(self.max_standing);
        true
    }

    /// Age every standing notice by one frame and drop the ones whose time is
    /// up. Reports whether any left.
    fn age(&mut self) -> bool {
        let before = self.standing.len();
        // `mem::take` rather than `retain`: ageing consumes each notice and
        // hands back either an older one or nothing, so the expiry rule lives
        // in one place ([`Standing::aged`]).
        let aged: Vec<Standing> = mem::take(&mut self.standing).into_iter().filter_map(Standing::aged).collect();
        self.standing = aged;
        self.standing.len() != before
    }

    /// Drop every standing notice, reporting whether there was one. A region
    /// that becomes unavailable is not a region that keeps a backlog.
    fn clear(&mut self) -> bool {
        let had = !self.standing.is_empty();
        self.standing.clear();
        had
    }

    /// The plates, in the widget's own local coordinates: a raised plate
    /// inside a hairline ring, the severity bar down its left edge, and the
    /// wrapped line boxes beside it. Newest at the region's top.
    fn overlay_items(&self) -> Vec<WidgetDrawItem> {
        let width = self.frame.width;
        if self.standing.is_empty() || !width.is_finite() || width <= 0.0 {
            return Vec::new();
        }
        let pad = self.theme.space(PAD_UNITS);
        let bar = self.bar_width();
        let line_height = self.line_height();
        let size = self.size_pixels();

        let mut items = Vec::new();
        let mut top = 0.0;
        for (notice, height) in self.standing.iter().zip(self.plate_heights()) {
            items.push(quad(0.0, top, width, height, self.theme.surface_raised));
            items.push(quad(0.0, top, bar, height, notice.severity.bar_ink(&self.theme)));
            push_rect_border(&mut items, 0.0, top, width, height, RING_THICKNESS, self.theme.outline);
            let mut line_top = top + pad;
            for line in self.wrapped(notice) {
                items.push(WidgetDrawItem::Text {
                    x: self.text_left(),
                    y: text_origin_y(line_top, line_height, size),
                    font_id: self.theme.font_id,
                    text: line,
                    size_pixels: size,
                    color: self.theme.text_primary,
                    clip: None,
                });
                line_top += line_height;
            }
            top += height + self.theme.space(1);
        }
        items
    }
}

/// The severity bar's width as a fraction of the spacing unit — three
/// quarters of it, which is a bar at the default four-pixel grid: wide enough
/// to carry a colour, narrow enough to stay an edge rather than a column.
const BAR_UNIT_RATIO: f32 = 0.75;

/// Emit a region-changed report when a mutation actually changed the stack.
fn report(ctx: &WasmCtx<'_>, changed: bool, region: ToastRegionChanged) {
    if !changed {
        return;
    }
    if let Some(parent) = ctx.parent() {
        parent.send(&region);
    }
}

impl WidgetDefaults for ToastWidget {
    fn widget_frame(&mut self) -> &mut WidgetFrame {
        &mut self.frame
    }

    fn widget_theme(&mut self) -> &mut Theme {
        &mut self.theme
    }

    fn widget_state(&mut self) -> &mut InteractionState {
        &mut self.state
    }

    /// Nothing to cancel: a notice is read, never operated.
    fn cancel_activation(&mut self) {}
}

/// The toast region. Spawned inline by a panel root with a [`ToastConfig`];
/// anything mails it a [`ToastNotice`], and it reports
/// [`ToastRegionChanged`] up as its stack grows and shrinks.
///
/// # Agent
/// Not loaded directly — the root spawns it as an inline child. Its lineage
/// address takes a `ToastNotice` from any actor, so raising a notice by hand
/// over MCP is one `send_mail`.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for ToastWidget {
    type Config = ToastConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.toast";

    fn init(config: ToastConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let desired_font_id = config.theme.font_id;
        Ok(ToastWidget {
            standing: Vec::new(),
            max_standing: usize::try_from(config.max_standing).unwrap_or(usize::MAX),
            lifetime_frames: config.lifetime_frames,
            role: config.role,
            theme: config.theme,
            state: InteractionState::new(config.state),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            font_metrics: FontMetricsAdapter::new(desired_font_id),
        })
    }

    /// Ask for the theme font's metrics; the wrap wants real advances.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    /// Raise a notice. Fire-and-forget from anywhere.
    #[handler::single]
    fn on_notice(&mut self, ctx: &mut WasmCtx<'_>, notice: ToastNotice) {
        let changed = self.raise(notice);
        report(ctx, changed, self.region_changed());
    }

    /// Re-configure the region in place. A smaller cap takes effect at once —
    /// the oldest notices leave rather than standing past a limit the host
    /// has since lowered — and a changed lifetime applies to notices raised
    /// after it.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: ToastConfig) {
        self.max_standing = usize::try_from(config.max_standing).unwrap_or(usize::MAX);
        self.lifetime_frames = config.lifetime_frames;
        self.role = config.role;
        let before = self.standing.len();
        self.standing.truncate(self.max_standing);
        let changed = self.standing.len() != before;
        self.font_metrics.set_desired(config.theme.font_id);
        self.theme = config.theme;
        report(ctx, changed, self.region_changed());
        if self.state.replace(config.state) {
            emit_state_changed(ctx, &self.state);
        }
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    /// Update external availability. A region that is switched off drops what
    /// it was holding: a refusal shown minutes later, when the screen has
    /// moved on, is worse than one never shown.
    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        if self.state.replace(set.state) {
            emit_state_changed(ctx, &self.state);
        }
        if !self.state.is_available() {
            let changed = self.clear();
            report(ctx, changed, self.region_changed());
        }
    }

    /// Restyle: adopt the fanned theme and request metrics for its font.
    #[handler::single]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        apply_text_theme(ctx, &mut self.font_metrics, &mut self.theme, set.theme);
    }

    /// Install a font-metrics reply; the next `Collect` wraps against real
    /// advances.
    #[handler::single]
    fn on_font_metrics_result(&mut self, ctx: &mut WasmCtx<'_>, result: FontMetricsResult) {
        accept_font_metrics_result(ctx, &mut self.font_metrics, result);
    }

    /// Age the stack by one frame, then reply the plates as **overlay**. The
    /// ageing happens before the hidden branch, so a hidden region's clock
    /// still runs.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        let expired = self.age();
        report(ctx, expired, self.region_changed());
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList {
                content_height: None,
                intrinsic: None,
                items: Vec::new(),
                overlay: self.overlay_items(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(max_standing: usize, lifetime_frames: u32) -> ToastWidget {
        ToastWidget {
            standing: Vec::new(),
            max_standing,
            lifetime_frames,
            role: TextRole::Body,
            theme: Theme::DEFAULT,
            state: InteractionState::new(WidgetControlState::default()),
            frame: WidgetFrame { x: 400.0, y: 40.0, width: 320.0, height: 200.0 },
            font_metrics: FontMetricsAdapter::new(0),
        }
    }

    fn notice(text: &str) -> ToastNotice {
        ToastNotice { severity: ToastSeverity::Warning, text: String::from(text) }
    }

    #[test]
    fn a_notice_goes_on_its_own_and_the_stack_never_grows_past_its_cap() {
        // Tripwire: the owner's two complaints about the first toast were that
        // it lingered and that a burst turned into a column of text. A notice
        // whose life never runs out, or a stack that keeps every arrival, is
        // that defect back.
        let mut toasts = region(3, 4);
        assert!(toasts.raise(notice("not enough points")));
        for _ in 1..4 {
            assert!(!toasts.age(), "a notice inside its life keeps standing");
        }
        assert!(toasts.age(), "and is gone on the frame its life runs out");
        assert!(toasts.standing.is_empty());

        for index in 0..5 {
            assert!(toasts.raise(notice(&alloc::format!("notice {index}"))));
        }
        assert_eq!(toasts.standing.len(), 3, "the cap holds");
        assert_eq!(toasts.standing[0].text, "notice 4", "and the newest is on top");
        assert_eq!(toasts.standing[2].text, "notice 2", "the oldest left to make room");
    }

    #[test]
    fn an_empty_notice_and_a_capless_region_raise_nothing() {
        // Tripwire: a plate with no line on it is a flash a reader cannot
        // read, and a region configured to show none must not hoard.
        let mut toasts = region(3, 10);
        assert!(!toasts.raise(ToastNotice::default()));
        assert!(toasts.standing.is_empty());

        let mut off = region(0, 10);
        assert!(!off.raise(notice("refused")));
        assert!(off.standing.is_empty());
    }

    #[test]
    fn a_long_notice_wraps_and_its_plate_grows_instead_of_cutting_the_line_off() {
        // Tripwire: round-3's "toaster text cuts off too soon, not large
        // enough, can be fatter vertically". A plate fixed at one row height
        // is the elision the note rejects.
        let mut toasts = region(3, 0);
        assert!(toasts.raise(notice("short")));
        let short = toasts.plate_heights()[0];

        let mut toasts = region(3, 0);
        assert!(toasts.raise(notice(
            "Not enough passive points remain to allocate the whole path; the studio allocated as far as the \
             budget reached and stopped there.",
        )));
        let wrapped = toasts.wrapped(&toasts.standing[0]);
        assert!(wrapped.len() > 1, "the line wrapped: {wrapped:?}");
        assert!(toasts.plate_heights()[0] > short, "and the plate grew with it");
        for line in &wrapped {
            assert!(toasts.text_width(line) <= toasts.text_width_budget(), "{line:?} ran past the region's own width");
        }
    }

    #[test]
    fn the_configured_role_sets_the_line_box_and_the_wrap_together() {
        // Tripwire: round-4 note 15. One role has to reach every size the
        // plate is built from — a bigger line drawn at a body-sized line box
        // overprints its neighbour, and one wrapped at the body measure runs
        // past the region's width. The default is the reading size the region
        // drew at before the field existed.
        let text = "Not enough passive points remain to allocate the whole path.";
        let mut body = region(3, 0);
        assert_eq!(body.role, TextRole::Body, "the default is what the region always drew at");
        assert_eq!(body.size_pixels(), Theme::DEFAULT.label_size_pixels);
        assert!(body.raise(notice(text)));

        let mut heading = region(3, 0);
        heading.role = TextRole::Heading;
        assert!(heading.raise(notice(text)));

        assert!(heading.line_height() > body.line_height(), "a larger role takes a taller line box");
        assert!(
            heading.wrapped(&heading.standing[0]).len() >= body.wrapped(&body.standing[0]).len(),
            "and wraps no later than the smaller one at the same region width",
        );
        assert!(heading.plate_heights()[0] > body.plate_heights()[0], "so the plate grows with the type");
        for line in &heading.wrapped(&heading.standing[0]) {
            assert!(heading.text_width(line) <= heading.text_width_budget(), "{line:?} ran past the region's width");
        }
    }

    #[test]
    fn a_notices_line_starts_two_units_clear_of_the_severity_bar() {
        // Tripwire: the owner's round-8 note 17 — "toaster left text padding
        // can be increased a tad. Feels kinda like there's not enough
        // breathing room." The bar and the line were one spacing unit apart,
        // which at the four-pixel grid is three pixels of colour and four of
        // air before the first glyph. The inset has to reach the wrap too:
        // widening the gap without taking it out of the measure runs the last
        // word off the plate, which is the round-3 complaint this region was
        // rebuilt to answer.
        let mut toasts = region(3, 0);
        assert!(toasts.raise(notice(
            "Not enough passive points remain to allocate the whole path; the studio allocated as far as the \
             budget reached and stopped there.",
        )));
        let inset = toasts.theme.space(2);
        let lines: Vec<(f32, String)> = toasts
            .overlay_items()
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Text { x, text, .. } => Some((*x, text.clone())),
                _ => None,
            })
            .collect();
        assert!(lines.len() > 1, "the notice wrapped: {lines:?}");
        for (x, line) in &lines {
            assert!(
                (x - (toasts.bar_width() + inset)).abs() < f32::EPSILON,
                "{line:?} starts at {x}, not two units past the {} bar",
                toasts.bar_width(),
            );
            assert!(
                x + toasts.text_width(line) <= toasts.frame.width - toasts.theme.space(PAD_UNITS) + 1e-3,
                "{line:?} runs off the plate the wider inset left it",
            );
        }
    }

    #[test]
    fn the_severity_bar_is_never_the_accent() {
        // Tripwire: round-3's "color of toaster tab (yellow) is the same as
        // everything else, so it's non-indicative". Three severities must be
        // three colours, and none of them the primary action's.
        let theme = Theme::DEFAULT;
        let inks = [
            ToastSeverity::Info.bar_ink(&theme),
            ToastSeverity::Warning.bar_ink(&theme),
            ToastSeverity::Error.bar_ink(&theme),
        ];
        for ink in inks {
            assert_ne!(ink, theme.accent);
        }
        assert_ne!(inks[0], inks[1]);
        assert_ne!(inks[1], inks[2]);
        assert_ne!(inks[0], inks[2]);
    }

    #[test]
    fn the_reported_height_covers_every_standing_plate() {
        // Tripwire: the host reports this rectangle as covered so another
        // actor's text is not drawn under the notices. A height short of the
        // stack is that text printing through.
        let mut toasts = region(3, 0);
        for index in 0..3 {
            assert!(toasts.raise(notice(&alloc::format!("notice {index}"))));
        }
        let heights = toasts.plate_heights();
        let reported = toasts.region_changed();
        assert_eq!(reported.standing, 3);
        let stacked = toasts.theme.space(1).mul_add(2.0, heights.iter().sum::<f32>());
        assert!((reported.height_pixels - stacked).abs() < f32::EPSILON, "{reported:?} vs {stacked}");

        assert!(toasts.clear());
        assert_eq!(toasts.region_changed(), ToastRegionChanged { standing: 0, height_pixels: 0.0 });
    }

    #[test]
    fn plates_stack_downward_from_the_regions_top_newest_first() {
        // Tripwire: "the place where it is relative to other elements is not
        // obvious". One region, one direction, newest where the eye lands.
        let mut toasts = region(3, 0);
        assert!(toasts.raise(notice("older")));
        assert!(toasts.raise(notice("newer")));
        let items = toasts.overlay_items();
        let tops: Vec<f32> = items
            .iter()
            .filter_map(|item| match item {
                // The plate's own fill: full region width and taller than the
                // hairline rows of its ring.
                WidgetDrawItem::Quad { x, y, width, height, .. }
                    if (*width - toasts.frame.width).abs() < f32::EPSILON && *x == 0.0 && *height > RING_THICKNESS =>
                {
                    Some(*y)
                }
                _ => None,
            })
            .collect();
        assert_eq!(tops.len(), 2, "one plate fill per notice: {tops:?}");
        assert!((tops[0] - 0.0).abs() < f32::EPSILON, "the newest sits at the region's top: {tops:?}");
        assert!(tops[1] > tops[0], "and the older one below it: {tops:?}");
        let first_line = items.iter().find_map(|item| match item {
            WidgetDrawItem::Text { text, .. } => Some(text.clone()),
            _ => None,
        });
        assert_eq!(first_line.as_deref(), Some("newer"));
    }
}
