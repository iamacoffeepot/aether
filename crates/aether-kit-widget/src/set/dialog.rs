// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The dialog: the plate a modal stands on, with a title, a rule under it,
//! and a body the host lays its own controls into.
//!
//! It exists for the owner's round-5 notes 10 and 12 — "the modal should have
//! a title and probably a bar below it or some basic layout/format" and
//! "modals should be resizable as well". A modal built without either is a
//! rectangle of controls with no name on it: the reader has to infer what
//! they opened from what is inside it, and cannot make it bigger when what is
//! inside does not fit.
//!
//! # What it owns, and what it does not
//!
//! It owns the **chrome and the geometry**: the plate, the title row set at
//! [`TextRole::Heading`] and measured, the hairline under it, the minimum
//! size that keeps the title from clipping, and the body rectangle left over.
//! It reports that geometry up as [`DialogPlaced`] so the host can frame its
//! slot children inside the body and hand the plate to its peers as an
//! occluder.
//!
//! It does **not** host children. That is the [`popover`](super::popover)
//! module's rule and the reason it is a module rather than a widget: pointer
//! and keyboard routing, hit rectangles, focus traversal, and drag capture
//! live in the root's [`Focus`](crate::focus::Focus) table over the root's own
//! direct children, so a widget that owned its children's input would be a
//! second input root inside a widget. A dialog is the same shape one step
//! further along — it draws the plate and says where the body is, and the
//! host's own children stand on it.
//!
//! It also does not dismiss itself. Light dismiss and Escape are
//! [`Popover::press`](super::popover::Popover::press) and
//! [`Popover::key`](super::popover::Popover::key), which the host already
//! owns for every other plate on the screen; a dialog that answered Escape
//! itself would be a second dismissal rule for the reader to learn.
//!
//! # Resizing
//!
//! By the handle the kit already has. The host frames a
//! [`SplitterWidget`](super::SplitterWidget) with `bare: true` over the
//! plate's right edge, another over its bottom edge, and a third over the
//! bottom-right corner (`SplitterAxis::Corner`), and re-frames the dialog on
//! each `SplitterMoved`. `bare` is the point: the edge of a plate is
//! something the reader can already see, so the pointer's resize shape is the
//! whole signal and a line lighting under it is one more thing on the screen.
//! The dialog clamps whatever size it is handed, and reports what it actually
//! took, so a splitter dragged past the minimum moves nothing rather than
//! cutting the title in half.
//!
//! # The plate's own layout
//!
//! Every band is derived from the type scale and the spacing grid, never from
//! a row height applied to everything:
//!
//! ```text
//!  ┌──────────────────────────────────┐
//!  │  pad                             │
//!  │  Title                 heading   │  title row = heading + 2 units
//!  │  ─────────────────────────────── │  rule band = 1 unit, hairline, 1 unit
//!  │                                  │
//!  │  body (reported to the host)     │
//!  │                                  │
//!  │  pad                             │
//!  └──────────────────────────────────┘
//! ```

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_text::FontMetricsResult;
use serde::{Deserialize, Serialize};

use crate::set::placement::PlacementBounds;
use crate::set::{
    WidgetDefaults, accept_font_metrics_result, apply_text_theme, measured_text_width, pump_text_font_metrics,
    push_rect_border, quad, reply_if_hidden, text_origin_y,
};
use crate::state::{InteractionState, emit_state_changed};
use crate::text_edit::FontMetricsAdapter;
use crate::theme::{SetTheme, TextRole, Theme};
use crate::{Collect, SetWidgetState, WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame};

/// The plate's inset, in spacing units — two, which is the least a control
/// inside a plate may sit from its edge.
const PAD_UNITS: u8 = 2;

/// The hairline the plate's ring and its title rule are drawn at.
const RULE_THICKNESS: f32 = 1.0;

/// `aether.kit.widget.dialog.config` — the plate a modal stands on. The
/// widget's assigned [`WidgetFrame`] is the plate's rectangle; this says what
/// is written on it and how small it may get.
///
/// A dialog is re-framed, not re-configured, to resize: the host's splitters
/// write the frame. Re-send the config to rename it or to change the floor.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.dialog.config")]
pub struct DialogConfig {
    /// The one line naming what the reader opened, set at
    /// [`TextRole::Heading`]. An empty title draws no title row at all — the
    /// plate is then a bare frame, which is what a confirmation with nothing
    /// to name wants.
    pub title: String,
    /// The narrowest the plate may be drawn. `0` (the default) is the title's
    /// own floor alone: the plate never goes narrower than its title plus a
    /// pad each side once the font's advances land, because a modal whose
    /// name is cut in half is worse than one that refuses to shrink.
    #[serde(default)]
    pub min_width_pixels: f32,
    /// The shortest the plate may be drawn. `0` (the default) is the chrome's
    /// own floor: the title row, its rule, and the padding under it, which is
    /// a dialog with an empty body rather than one with a clipped title.
    #[serde(default)]
    pub min_height_pixels: f32,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

/// `aether.kit.widget.dialog.placed` — where the plate actually stands and
/// where its body is, in the same window pixels the frame was assigned in.
/// Reported whenever either changes, never every frame.
///
/// The host needs both. `body` is where it frames its own slot children, so
/// they land under the title rather than over it. `frame` is the plate as
/// *drawn* — which is the assigned frame grown to the minimum the title
/// needs — so the host can hand it to its peers as the rectangle they are
/// occluded by, and hang its resize splitters on the edges the reader sees.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Default)]
#[kind(name = "aether.kit.widget.dialog.placed")]
pub struct DialogPlaced {
    pub frame: PlacementBounds,
    pub body: PlacementBounds,
}

/// The dialog widget. Holds the title and the floor it was given plus the
/// cached theme, frame, and font metrics it measures the title with.
pub struct DialogWidget {
    title: String,
    min_width_pixels: f32,
    min_height_pixels: f32,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    /// The geometry last reported up, so [`DialogPlaced`] is an edge and not
    /// a mail every frame.
    placed: Option<DialogPlaced>,
    /// Single-flight exact metrics for the active theme font: the title's
    /// measured width is the plate's own width floor.
    font_metrics: FontMetricsAdapter,
}

impl DialogWidget {
    /// The plate's inset, every edge alike.
    fn pad(&self) -> f32 {
        self.theme.space(PAD_UNITS)
    }

    /// How tall the title's row is: its own type size plus one spacing unit
    /// above and below. Derived from the type scale, because a heading and a
    /// body row are different sizes and so are their rows.
    fn title_row_height(&self) -> f32 {
        if self.title.is_empty() {
            return 0.0;
        }
        self.theme.space(1).mul_add(2.0, self.theme.heading_size_pixels)
    }

    /// How much vertical room the rule under the title takes: the hairline
    /// plus one spacing unit either side — the same band the tooltip's section
    /// rules occupy, so a division reads the same wherever the kit draws one.
    /// Zero without a title, which is what there would be a rule under.
    fn rule_band(&self) -> f32 {
        if self.title.is_empty() {
            return 0.0;
        }
        self.theme.space(1).mul_add(2.0, RULE_THICKNESS)
    }

    /// How far down the plate the body starts: the top pad, the title row,
    /// and the rule's band.
    fn body_top(&self) -> f32 {
        self.pad() + self.title_row_height() + self.rule_band()
    }

    /// The title's measured pixel width, or `None` until the font's advances
    /// land. The floor it sets therefore arrives with the measurement rather
    /// than being guessed from a character count and then jumping.
    fn title_width(&self) -> Option<f32> {
        let metrics = self.font_metrics.resolved()?;
        (!self.title.is_empty())
            .then(|| measured_text_width(metrics, &self.title, self.theme.text_size_pixels(TextRole::Heading)))
    }

    /// The smallest plate this dialog may be drawn on: wide enough for its
    /// title and a pad each side, tall enough for the chrome above the body.
    /// The host's own floor raises either.
    fn min_size(&self) -> [f32; 2] {
        let floor = |value: f32| {
            if value.is_finite() {
                value.max(0.0)
            } else {
                0.0
            }
        };
        let title = self.title_width().map_or(0.0, |width| self.pad().mul_add(2.0, width));
        [floor(self.min_width_pixels).max(title), floor(self.min_height_pixels).max(self.body_top() + self.pad())]
    }

    /// The plate as drawn: the assigned frame, grown from its top-left to the
    /// minimum. It grows rather than clipping because the alternative is a
    /// title cut in half — and the host is told what it took
    /// ([`DialogPlaced`]), so a plate that grew past its region is visible to
    /// the one thing that can move it.
    fn plate(&self) -> PlacementBounds {
        let [min_width, min_height] = self.min_size();
        let frame = PlacementBounds::from(&self.frame).sane();
        PlacementBounds { width: frame.width.max(min_width), height: frame.height.max(min_height), ..frame }
    }

    /// The rectangle the host lays its own children into, in window pixels:
    /// the plate less the chrome above it and one pad on the other three
    /// sides. A plate with no room left reports a zero-height body rather
    /// than a negative one.
    fn body(&self) -> PlacementBounds {
        let (plate, pad, top) = (self.plate(), self.pad(), self.body_top());
        PlacementBounds {
            x: plate.x + pad,
            y: plate.y + top,
            width: pad.mul_add(-2.0, plate.width).max(0.0),
            height: (plate.height - top - pad).max(0.0),
        }
    }

    /// The geometry this frame stands on.
    fn placement(&self) -> DialogPlaced {
        DialogPlaced { frame: self.plate(), body: self.body() }
    }

    /// The placement to report up, or `None` when it has not moved since the
    /// last report — the host re-frames its children off this, and doing that
    /// every frame for a plate that did not move is a relayout per tick.
    fn take_placement_change(&mut self) -> Option<DialogPlaced> {
        let placed = self.placement();
        if self.placed == Some(placed) {
            return None;
        }
        self.placed = Some(placed);
        Some(placed)
    }

    /// The plate, in the widget's own local coordinates. Empty for a
    /// degenerate frame — a dialog framed before the host knows its region is
    /// a plate with no size, and drawing a ring around nothing is worse than
    /// drawing nothing.
    fn overlay_items(&self) -> Vec<WidgetDrawItem> {
        let plate = self.plate();
        if plate.width <= 0.0 || plate.height <= 0.0 {
            return Vec::new();
        }
        let (width, height, pad) = (plate.width, plate.height, self.pad());

        let mut items = Vec::with_capacity(7);
        items.push(quad(0.0, 0.0, width, height, self.theme.surface_raised));
        push_rect_border(&mut items, 0.0, 0.0, width, height, RULE_THICKNESS, self.theme.outline);
        if self.title.is_empty() {
            return items;
        }

        let size = self.theme.text_size_pixels(TextRole::Heading);
        items.push(WidgetDrawItem::Text {
            x: pad,
            y: text_origin_y(pad, self.title_row_height(), size),
            font_id: self.theme.font_id,
            text: self.title.clone(),
            size_pixels: size,
            color: self.theme.text_primary,
            clip: None,
        });
        items.push(quad(
            pad,
            pad + self.title_row_height() + self.theme.space(1),
            pad.mul_add(-2.0, width).max(0.0),
            RULE_THICKNESS,
            self.theme.outline,
        ));
        items
    }
}

impl WidgetDefaults for DialogWidget {
    fn widget_frame(&mut self) -> &mut WidgetFrame {
        &mut self.frame
    }

    fn widget_theme(&mut self) -> &mut Theme {
        &mut self.theme
    }

    fn widget_state(&mut self) -> &mut InteractionState {
        &mut self.state
    }

    /// Nothing to cancel: a dialog is a plate, and everything on it is the
    /// host's own child.
    fn cancel_activation(&mut self) {}
}

/// A modal's plate. Spawned inline by a panel root with a [`DialogConfig`];
/// reports [`DialogPlaced`] whenever the plate or its body moves.
///
/// # Agent
/// Not loaded directly — the root spawns it as an inline child, frames it
/// with the rectangle the plate should occupy, and re-frames it to resize.
/// Hide it with `aether.kit.widget.set_state`. Register its slot *before* the
/// children standing on it and raise them into the overlay lane
/// (`Composite::set_slot_overlay`), so the plate arrives under its own
/// contents and over the screen it covers.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for DialogWidget {
    type Config = DialogConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.dialog";

    fn init(config: DialogConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let desired_font_id = config.theme.font_id;
        Ok(DialogWidget {
            title: config.title,
            min_width_pixels: config.min_width_pixels,
            min_height_pixels: config.min_height_pixels,
            theme: config.theme,
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            state: InteractionState::new(config.state),
            placed: None,
            font_metrics: FontMetricsAdapter::new(desired_font_id),
        })
    }

    /// Ask for the theme font's metrics; the title's own width is the plate's
    /// floor, so it wants real advances as soon as there are any.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    /// Rename the dialog or change its floor in place.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: DialogConfig) {
        self.title = config.title;
        self.min_width_pixels = config.min_width_pixels;
        self.min_height_pixels = config.min_height_pixels;
        self.font_metrics.set_desired(config.theme.font_id);
        self.theme = config.theme;
        if self.state.replace(config.state) {
            emit_state_changed(ctx, &self.state);
        }
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    /// Update external availability — the lane a host raises and drops the
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

    /// Install a font-metrics reply; the next `Collect` measures the title
    /// against real advances, which can raise the plate's own floor.
    #[handler::single]
    fn on_font_metrics_result(&mut self, ctx: &mut WasmCtx<'_>, result: FontMetricsResult) {
        accept_font_metrics_result(ctx, &mut self.font_metrics, result);
    }

    /// Reply the plate as **overlay** and nothing as ordinary items: a modal
    /// stands over the screen, and the overlay is the lane whose fill cuts the
    /// covered text out from under it. A moved plate is reported up first, so
    /// the host reads it before the frame it belongs to is drawn.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        if let Some(placed) = self.take_placement_change()
            && let Some(parent) = ctx.parent()
        {
            parent.send(&placed);
        }
        let overlay = self.overlay_items();
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { content_height: None, intrinsic: None, items: Vec::new(), overlay });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_kinds::{CachedFontMetrics, FontMetrics};

    fn dialog(title: &str, width: f32, height: f32) -> DialogWidget {
        DialogWidget {
            title: String::from(title),
            min_width_pixels: 0.0,
            min_height_pixels: 0.0,
            theme: Theme::DEFAULT,
            frame: WidgetFrame { x: 200.0, y: 120.0, width, height },
            state: InteractionState::new(WidgetControlState::default()),
            placed: None,
            font_metrics: FontMetricsAdapter::new(Theme::DEFAULT.font_id),
        }
    }

    /// The same dialog with a resolved metric table whose every glyph advances
    /// half an em, so the title's width is `chars * size / 2` — exact without
    /// depending on a real font file.
    fn measured(title: &str, width: f32, height: f32) -> DialogWidget {
        let mut widget = dialog(title, width, height);
        widget.font_metrics.take_pending_request();
        widget.font_metrics.accept_reply(Some(CachedFontMetrics::new(&FontMetrics {
            units_per_em: 1000.0,
            ascent: 800.0,
            descent: -200.0,
            line_gap: 0.0,
            default_advance: 500.0,
            advances: Vec::new(),
        })));
        widget
    }

    fn rule(widget: &DialogWidget) -> (f32, f32, f32) {
        let items = widget.overlay_items();
        let Some(&WidgetDrawItem::Quad { x, y, width, .. }) = items.last() else {
            panic!("a titled plate ends in its rule, a quad: {items:?}");
        };
        (x, y, width)
    }

    #[test]
    fn the_body_starts_under_the_title_and_its_rule_and_is_inset_on_the_other_three_sides() {
        // Tripwire: the body is where the host frames its own children, so a
        // rectangle that started at the plate's top would put every control in
        // the dialog on top of the title, and one that ignored the rule band
        // would put the first row on the hairline.
        let widget = measured("Import build", 400.0, 300.0);
        let plate = widget.plate();
        let body = widget.body();
        let pad = widget.pad();

        assert_eq!(body.x, plate.x + pad);
        assert_eq!(body.y, plate.y + pad + widget.title_row_height() + widget.rule_band());
        assert_eq!(body.width, pad.mul_add(-2.0, plate.width));
        assert_eq!(body.height, plate.height - (body.y - plate.y) - pad);

        let (rule_x, rule_y, rule_width) = rule(&widget);
        assert_eq!(rule_x, pad, "the rule is inset like everything else on the plate, not full-bleed");
        assert_eq!(rule_width, pad.mul_add(-2.0, plate.width));
        assert!(rule_y > pad + widget.title_row_height() - 1.0, "and it stands under the title, not through it");
        assert!(rule_y < body.y - plate.y, "and above the body");
    }

    #[test]
    fn an_untitled_plate_is_all_body() {
        // Tripwire: the title row and its rule are bands the *title* asks for.
        // Reserving them for a dialog with no name would leave a rule with
        // nothing above it and a body pushed down by an empty row.
        let widget = measured("", 400.0, 300.0);
        assert_eq!(widget.body().y, widget.plate().y + widget.pad());
        assert_eq!(widget.overlay_items().len(), 5, "a fill and its four-sided ring, and nothing else");
    }

    #[test]
    fn the_plate_grows_to_the_title_it_carries_and_to_the_hosts_own_floor() {
        // Tripwire: round-5 note 10 asked for a title, and a title is only a
        // title if the reader can read all of it — a plate narrower than its
        // own name cuts it. The floor arrives with the measurement: guessing
        // it from a character count would size the plate wrong and then jump.
        let unmeasured = dialog("A rather long dialog title", 40.0, 40.0);
        assert_eq!(unmeasured.plate().width, 40.0, "nothing is guessed before the advances land");

        let widget = measured("A rather long dialog title", 40.0, 40.0);
        let size = widget.theme.text_size_pixels(TextRole::Heading);
        let metrics = widget.font_metrics.resolved().expect("the test table is installed");
        let title = measured_text_width(metrics, &widget.title, size);
        let plate = widget.plate();
        assert_eq!(plate.width, widget.pad().mul_add(2.0, title), "the plate is at least its title plus a pad");
        assert_eq!(plate.height, widget.body_top() + widget.pad(), "and at least its own chrome, with an empty body");
        assert_eq!(plate.x, 200.0, "growth is from the top-left the host placed");
        assert_eq!(plate.y, 120.0);

        let mut floored = measured("Ok", 40.0, 40.0);
        floored.min_width_pixels = 320.0;
        floored.min_height_pixels = 240.0;
        assert_eq!(floored.plate().width, 320.0, "the host's own floor outranks the title's");
        assert_eq!(floored.plate().height, 240.0);

        let mut wide = measured("Ok", 640.0, 480.0);
        wide.min_width_pixels = 320.0;
        assert_eq!(wide.plate().width, 640.0, "and a plate past both floors keeps the size it was framed at");
    }

    #[test]
    fn the_placement_is_reported_on_the_edge_and_not_every_frame() {
        // Tripwire: the host re-frames its children off this mail. Sending it
        // every collect is a relayout per tick — which is exactly the loop
        // that made the studio's own splitter drag unusable.
        let mut widget = measured("Import build", 400.0, 300.0);
        assert_eq!(widget.take_placement_change(), Some(widget.placement()), "the first frame reports");
        assert_eq!(widget.take_placement_change(), None, "a plate that has not moved reports nothing");

        widget.frame.width = 420.0;
        let moved = widget.take_placement_change().expect("a resized plate reports again");
        assert_eq!(moved.frame.width, 420.0);
        assert_eq!(moved.body.width, widget.pad().mul_add(-2.0, 420.0));
        assert_eq!(widget.take_placement_change(), None);
    }

    #[test]
    fn a_plate_with_no_room_draws_nothing_and_reports_an_empty_body() {
        // Tripwire: a dialog framed before the host knows its region has no
        // size. A ring drawn around nothing is a hairline cross on the screen,
        // and a negative body would frame every child inside out.
        let widget = dialog("Import build", 0.0, 0.0);
        assert!(widget.overlay_items().is_empty());

        let squashed = measured("Import build", 400.0, 10.0);
        assert_eq!(squashed.body().height, 0.0, "the body bottoms out at nothing rather than going negative");
        assert!(squashed.plate().height >= squashed.body_top(), "and the plate keeps its own chrome");
    }
}
