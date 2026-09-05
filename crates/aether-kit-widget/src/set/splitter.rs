// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The splitter: the drag handle on the edge between two regions.
//!
//! It exists for the owner's round-1 note 10 — "left panel should be
//! horizontally resizable by pulling it to the right to some maximum width.
//! Same with ascendancy window" — and for the two notes that shaped its look:
//! round-2 note 19, "resize highlight on left bar too wide, could be
//! smaller", and round-3 note 3, which asked for the affordance back after it
//! was removed. So the handle is a **thin mark**, two logical pixels of
//! [`Theme::accent`] lit only while the pointer is on it or a drag is live,
//! over a hit strip as wide as the host cares to make the slot — the target
//! is generous, the mark is not.
//!
//! # What it does not do
//!
//! It does not set the pointer shape. Round-2 note 7 asked for a resize
//! cursor on a resizable edge and round-3 note 4 asked for no cursor where
//! the gesture is obvious, and both of those are the *host's* judgement about
//! its own screen — so the widget reports [`SplitterHover`] up and the root
//! decides whether to mail `aether.window.set_cursor`. A widget never talks
//! to the window cap.
//!
//! It also asks for no new pointer routing. A left press on a pointer-
//! eligible child already gives that child the root's **drag capture**, and
//! capture lasts exactly as long as the button is held — which is exactly the
//! life of a resize drag. (The modal grab an open dropdown holds is the wrong
//! tool here: it outranks capture and persists across releases, so it would
//! have to be handed back explicitly for a gesture that is over when the
//! button comes up.)
//!
//! # The three axes
//!
//! A splitter reports one scalar, and [`SplitterAxis`] says which pointer
//! motion moves it: `Horizontal` for a vertical edge dragged left and right
//! (a docked pane's width), `Vertical` for a horizontal edge dragged up and
//! down (a console's height), and `Corner` for a plate resized by one side
//! length from its corner — the mean of both axes, because a square plate
//! invites a diagonal drag and taking one axis alone makes half of every drag
//! do nothing.

use alloc::vec::Vec;
use core::mem;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::{MouseButton, MouseButtonRelease, MouseMove, mouse_button};
use serde::{Deserialize, Serialize};

use crate::set::{WidgetDefaults, quad, reply_if_hidden};
use crate::state::{InteractionState, emit_state_changed};
use crate::theme::Theme;
use crate::{
    Collect, HoverGained, HoverLost, SetWidgetState, WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame,
};

/// Which pointer motion moves a splitter's position.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SplitterAxis {
    /// Left and right: a vertical edge between two side-by-side regions, the
    /// docked pane's case.
    #[default]
    Horizontal,
    /// Up and down: a horizontal edge between two stacked regions.
    Vertical,
    /// Both at once, averaged: one side length of a square plate dragged by
    /// its corner.
    Corner,
}

impl SplitterAxis {
    /// How far this axis reads a pointer that has travelled `(dx, dy)` from
    /// where the drag began.
    fn travel(self, dx: f32, dy: f32) -> f32 {
        match self {
            Self::Horizontal => dx,
            Self::Vertical => dy,
            Self::Corner => (dx + dy) * 0.5,
        }
    }
}

/// `aether.kit.widget.splitter.config` — the drag handle between two
/// regions. `position_pixels` is the scalar the host resizes with (a pane's
/// width, a plate's side), held between `min_pixels` and `max_pixels`;
/// `axis` says which pointer motion moves it, and `inverted` flips the
/// direction for a region anchored to the far edge — a plate pinned to the
/// bottom-right grows as its top-left corner is dragged *up and left*, so its
/// handle counts travel the other way.
///
/// The widget's assigned [`WidgetFrame`] is the
/// hit strip; the lit mark is two logical pixels inside it, so the target can
/// be as generous as the host likes without the affordance becoming a column.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.splitter.config")]
pub struct SplitterConfig {
    pub axis: SplitterAxis,
    pub min_pixels: f32,
    pub max_pixels: f32,
    /// Where the split stands now. Re-send the config to move it from the
    /// host's side (a menu command that resets a pane's width); the widget
    /// clamps whatever it is given.
    pub position_pixels: f32,
    /// The region grows as the pointer travels toward the origin rather than
    /// away from it.
    #[serde(default)]
    pub inverted: bool,
    /// A bare handle draws no mark while the pointer is on it. An edge the
    /// reader can already see (the border of a plate) needs none: the
    /// pointer's resize shape is the whole signal, and a line lighting under
    /// it is one more thing on the screen. A bare handle still reports every
    /// hover and move.
    #[serde(default)]
    pub bare: bool,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

/// `aether.kit.widget.splitter.moved` — the split's new position, clamped
/// into the configured range, streamed while the pointer drags. There is no
/// preview/commit split: a region resize is applied as it happens, which is
/// the whole feedback the gesture has.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[kind(name = "aether.kit.widget.splitter.moved")]
pub struct SplitterMoved {
    pub position_pixels: f32,
}

/// `aether.kit.widget.splitter.hover` — the pointer entered (`true`) or left
/// (`false`) the handle's strip. The host decides what to do with it: on a
/// screen where the edge is the only resizable thing, mail
/// `aether.window.set_cursor` with the axis's resize icon; on one where the
/// gesture is already obvious, do nothing. A widget never sets the cursor
/// itself.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.kit.widget.splitter.hover")]
pub struct SplitterHover {
    pub entered: bool,
}

/// A live drag: where the pointer went down, and what the position was then.
/// Both are needed because the position follows the pointer's *travel*, not
/// its absolute location — grabbing the strip anywhere along its width must
/// not jump the split to the pointer.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Drag {
    origin_x: f32,
    origin_y: f32,
    start_position_pixels: f32,
}

/// The splitter widget. Holds the split's position and range plus the cached
/// theme, frame, and live drag.
pub struct SplitterWidget {
    axis: SplitterAxis,
    min_pixels: f32,
    max_pixels: f32,
    position_pixels: f32,
    inverted: bool,
    bare: bool,
    drag: Option<Drag>,
    /// A leave the widget has not reported yet, because the pointer walked
    /// off the strip with the button still down. See [`SplitterWidget::leave`].
    leave_pending: bool,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
}

impl SplitterWidget {
    /// `position` held inside the configured range. A range whose ends are
    /// crossed or non-finite collapses to the low end rather than propagating
    /// a NaN split every region downstream would be laid out from.
    fn clamped(&self, position: f32) -> f32 {
        let low = if self.min_pixels.is_finite() {
            self.min_pixels
        } else {
            0.0
        };
        let high = if self.max_pixels.is_finite() {
            self.max_pixels.max(low)
        } else {
            low
        };
        if position.is_finite() {
            position.clamp(low, high)
        } else {
            low
        }
    }

    /// The position a pointer at `(x, y)` asks for, given where the drag
    /// started. `None` while no drag is live.
    fn dragged_position(&self, x: f32, y: f32) -> Option<f32> {
        let drag = self.drag?;
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let (dx, dy) = (x - drag.origin_x, y - drag.origin_y);
        let travel = self.axis.travel(dx, dy);
        let travel = if self.inverted {
            -travel
        } else {
            travel
        };
        Some(self.clamped(drag.start_position_pixels + travel))
    }

    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.frame.x
            && x <= self.frame.x + self.frame.width
            && y >= self.frame.y
            && y <= self.frame.y + self.frame.height
    }

    /// How thick the lit mark is: half a spacing unit, which is the owner's
    /// two logical pixels on the default four-pixel grid and scales with a
    /// theme scaled for the display.
    fn mark_thickness(&self) -> f32 {
        (self.theme.space_unit_pixels * MARK_UNIT_RATIO).max(1.0)
    }

    /// The handle's mark, in the widget's own local coordinates. Nothing at
    /// rest: the strip is invisible until the pointer is on it or a drag is
    /// live, so a resizable edge is a signifier that appears when it is
    /// relevant rather than a permanent rule down the screen.
    fn draw_items(&self) -> Vec<WidgetDrawItem> {
        let (width, height) = (self.frame.width, self.frame.height);
        let lit = self.state.hovered() || self.drag.is_some();
        let sized = width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0;
        if self.bare || !lit || !sized || !self.state.can_mutate() {
            return Vec::new();
        }
        let thickness = self.mark_thickness();
        let ink = self.theme.accent;
        match self.axis {
            SplitterAxis::Horizontal => {
                alloc::vec![quad((width - thickness) * 0.5, 0.0, thickness.min(width), height, ink)]
            }
            SplitterAxis::Vertical => {
                alloc::vec![quad(0.0, (height - thickness) * 0.5, width, thickness.min(height), ink)]
            }
            // The corner grip is the two edges the drag pulls, drawn as an L
            // so the mark says which corner it is rather than lighting the
            // whole square.
            SplitterAxis::Corner => alloc::vec![
                quad(0.0, 0.0, width, thickness.min(height), ink),
                quad(0.0, 0.0, thickness.min(width), height, ink),
            ],
        }
    }

    /// Report a hover edge up so the host can answer it with a cursor.
    fn report_hover(ctx: &WasmCtx<'_>, entered: bool) {
        if let Some(parent) = ctx.parent() {
            parent.send(&SplitterHover { entered });
        }
    }

    /// The pointer is on the strip. Reports whether the enter goes up — an
    /// unavailable strip takes no hover at all — and cancels any leave a live
    /// drag had deferred, because the pointer came back before the gesture
    /// ended and the crossing it would have reported never completed.
    fn enter(&mut self) -> bool {
        self.state.set_hovered(true);
        self.leave_pending = false;
        self.state.hovered()
    }

    /// The pointer left the strip. Reports whether the leave goes up **now**.
    ///
    /// It does not while a drag is live: the pointer wanders off a four-pixel
    /// strip within the first few pixels of every resize, and a host that put
    /// a resize cursor on the edge must keep it for the whole gesture rather
    /// than flicker it back on the first frame. The leave is remembered
    /// instead and goes up when the drag ends ([`Self::take_owed_leave`]), so
    /// exactly one `entered: false` is reported for exactly one crossing.
    fn leave(&mut self) -> bool {
        self.state.set_hovered(false);
        if self.drag.is_some() {
            self.leave_pending = true;
            return false;
        }
        true
    }

    /// Whether a deferred leave is now owed: the drag is over and the pointer
    /// left the strip while it was live. Taking it clears it, so a release and
    /// the collect after it cannot both report the same crossing.
    fn take_owed_leave(&mut self) -> bool {
        self.drag.is_none() && mem::take(&mut self.leave_pending)
    }
}

/// The lit mark's thickness as a fraction of the spacing unit — the owner's
/// two logical pixels at the default grid.
const MARK_UNIT_RATIO: f32 = 0.5;

impl WidgetDefaults for SplitterWidget {
    fn widget_frame(&mut self) -> &mut WidgetFrame {
        &mut self.frame
    }

    fn widget_theme(&mut self) -> &mut Theme {
        &mut self.theme
    }

    fn widget_state(&mut self) -> &mut InteractionState {
        &mut self.state
    }

    /// Drop a live drag. The split keeps wherever it had reached — a drag
    /// interrupted by losing focus is finished, not undone. A leave deferred
    /// by that drag stays owed and goes up on the next `Collect`.
    fn cancel_activation(&mut self) {
        self.drag = None;
    }
}

/// A splitter. Spawned inline by a panel root with a [`SplitterConfig`];
/// reports [`SplitterMoved`] as it is dragged and [`SplitterHover`] as the
/// pointer crosses it.
///
/// # Agent
/// Not loaded directly — the root spawns it as an inline child. Re-send
/// `SplitterConfig` to move the split from the host's side.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for SplitterWidget {
    type Config = SplitterConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.splitter";

    fn init(config: SplitterConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let mut widget = SplitterWidget {
            axis: config.axis,
            min_pixels: config.min_pixels,
            max_pixels: config.max_pixels,
            position_pixels: config.position_pixels,
            inverted: config.inverted,
            bare: config.bare,
            drag: None,
            leave_pending: false,
            theme: config.theme,
            state: InteractionState::new(config.state),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
        };
        widget.position_pixels = widget.clamped(widget.position_pixels);
        Ok(widget)
    }

    /// Replace the range and position in place. A live drag ends: the config
    /// is the host's own word about where the split is, and a drag still
    /// following the pointer would immediately overwrite it.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: SplitterConfig) {
        self.axis = config.axis;
        self.min_pixels = config.min_pixels;
        self.max_pixels = config.max_pixels;
        self.inverted = config.inverted;
        self.bare = config.bare;
        self.drag = None;
        self.theme = config.theme;
        self.position_pixels = self.clamped(config.position_pixels);
        if self.state.replace(config.state) {
            emit_state_changed(ctx, &self.state);
        }
    }

    /// Update external availability; a splitter that can no longer be dragged
    /// drops the drag it was holding.
    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        if self.state.replace(set.state) {
            emit_state_changed(ctx, &self.state);
        }
        if !self.state.can_mutate() {
            self.drag = None;
        }
    }

    /// Enter hover — and say so, so the host can put a resize pointer on it.
    /// A pointer that came back mid-drag cancels the leave that drag deferred.
    #[handler::single]
    fn on_hover_gained(&mut self, ctx: &mut WasmCtx<'_>, _gained: HoverGained) {
        if self.enter() {
            Self::report_hover(ctx, true);
        }
    }

    /// Leave hover — and say so, so the host can put the pointer back, unless
    /// a drag is still live and owns the cursor until the button comes up.
    #[handler::single]
    fn on_hover_lost(&mut self, ctx: &mut WasmCtx<'_>, _lost: HoverLost) {
        if self.leave() {
            Self::report_hover(ctx, false);
        }
    }

    /// A left press on the strip begins the drag. The root gives the child
    /// drag capture on the same press, so the moves that follow reach here
    /// even once the pointer has left the strip.
    #[handler::single]
    fn on_mouse_button(&mut self, _ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if press.button != mouse_button::LEFT || !self.state.can_mutate() || !self.contains(press.x, press.y) {
            return;
        }
        self.drag = Some(Drag { origin_x: press.x, origin_y: press.y, start_position_pixels: self.position_pixels });
    }

    /// Motion during a drag moves the split, clamped, and reports it only
    /// when it actually moved — a drag past the end of the range goes quiet
    /// instead of re-sending the same clamped value every frame.
    #[handler::single]
    fn on_mouse_move(&mut self, ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        let Some(position) = self.dragged_position(moved.x, moved.y) else {
            return;
        };
        if (position - self.position_pixels).abs() <= f32::EPSILON {
            return;
        }
        self.position_pixels = position;
        if let Some(parent) = ctx.parent() {
            parent.send(&SplitterMoved { position_pixels: position });
        }
    }

    /// The release ends the drag wherever it reached, and pays out the leave
    /// the drag deferred — a pointer that came up off the strip has left it,
    /// and the host is owed the edge that puts its cursor back.
    #[handler::single]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        if release.button != mouse_button::LEFT {
            return;
        }
        self.drag = None;
        if self.take_owed_leave() {
            Self::report_hover(ctx, false);
        }
    }

    /// Reply the handle's mark.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        // A drag cancelled by anything but a release — focus loss, the host
        // disabling the strip — leaves the same debt, and this is the first
        // moment after it with a ctx to pay it from.
        if self.take_owed_leave() {
            Self::report_hover(ctx, false);
        }
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList {
                content_height: None,
                intrinsic: None,
                items: self.draw_items(),
                overlay: Vec::new(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn splitter(axis: SplitterAxis, inverted: bool) -> SplitterWidget {
        SplitterWidget {
            axis,
            min_pixels: 280.0,
            max_pixels: 560.0,
            position_pixels: 360.0,
            inverted,
            bare: false,
            drag: None,
            leave_pending: false,
            theme: Theme::DEFAULT,
            frame: WidgetFrame { x: 358.0, y: 30.0, width: 4.0, height: 700.0 },
            state: InteractionState::new(WidgetControlState::default()),
        }
    }

    fn grabbed(axis: SplitterAxis, inverted: bool) -> SplitterWidget {
        let mut widget = splitter(axis, inverted);
        widget.drag = Some(Drag { origin_x: 360.0, origin_y: 400.0, start_position_pixels: widget.position_pixels });
        widget
    }

    #[test]
    fn a_drag_follows_the_pointers_travel_and_stops_at_both_ends_of_the_range() {
        // Tripwire: the range is the whole of what keeps a resizable pane from
        // eating its neighbour. A position taken from the pointer's absolute
        // x instead of its travel would also jump the split on the press.
        let widget = grabbed(SplitterAxis::Horizontal, false);
        assert_eq!(widget.dragged_position(420.0, 400.0), Some(420.0), "the split follows the travel");
        assert_eq!(widget.dragged_position(1200.0, 400.0), Some(560.0), "and stops at the maximum");
        assert_eq!(widget.dragged_position(0.0, 400.0), Some(280.0), "and at the minimum");
        assert_eq!(widget.dragged_position(420.0, 900.0), Some(420.0), "the cross axis moves nothing");
    }

    #[test]
    fn each_axis_reads_the_motion_it_names() {
        // Tripwire: `Corner` averages both axes because a square plate invites
        // a diagonal drag — taking one axis alone makes half of every drag do
        // nothing, which is what the inset's own maths avoided.
        assert_eq!(grabbed(SplitterAxis::Vertical, false).dragged_position(900.0, 440.0), Some(400.0));
        assert_eq!(grabbed(SplitterAxis::Corner, false).dragged_position(380.0, 420.0), Some(380.0));
        assert_eq!(
            grabbed(SplitterAxis::Corner, false).dragged_position(400.0, 400.0),
            Some(380.0),
            "one axis alone still moves a corner, by half its travel",
        );
        assert_eq!(
            grabbed(SplitterAxis::Corner, true).dragged_position(340.0, 380.0),
            Some(380.0),
            "and a plate anchored to the far edge grows as its corner is pulled back",
        );
    }

    #[test]
    fn a_degenerate_range_or_pointer_collapses_rather_than_poisoning_the_split() {
        // Tripwire: a NaN split lays out every region downstream from NaN.
        let mut widget = grabbed(SplitterAxis::Horizontal, false);
        assert_eq!(widget.dragged_position(f32::NAN, 400.0), None);
        widget.max_pixels = 100.0;
        assert_eq!(widget.dragged_position(900.0, 400.0), Some(280.0), "crossed ends collapse to the minimum");
        widget.min_pixels = f32::NAN;
        assert_eq!(widget.clamped(500.0), 100.0);
    }

    #[test]
    fn a_pointer_that_leaves_the_strip_mid_drag_holds_its_leave_until_the_button_comes_up() {
        // Tripwire: round-5 note 14 — "clicking the border of the left panel
        // it remains highlighted even after clicking other things". The
        // pointer leaves a four-pixel strip within the first few pixels of
        // every resize, so the crossing has to be *held*, not dropped: a leave
        // sent mid-drag flickers the host's resize cursor back, and a leave
        // never sent leaves the cursor — and, when the release lands
        // elsewhere, the mark — stuck on.
        let mut widget = grabbed(SplitterAxis::Horizontal, false);
        widget.enter();
        assert!(!widget.leave(), "the leave waits for the drag");
        assert!(!widget.take_owed_leave(), "and nothing is owed while the drag holds");
        assert_eq!(widget.draw_items().len(), 1, "the mark stays lit for the whole gesture");

        widget.drag = None;
        assert!(widget.take_owed_leave(), "the drag's end pays it out");
        assert!(!widget.take_owed_leave(), "exactly once");
        assert!(widget.draw_items().is_empty(), "and the strip goes unlit with it");
    }

    #[test]
    fn an_ordinary_crossing_reports_at_once_and_a_pointer_that_came_back_owes_nothing() {
        // Tripwire: the deferral is for a live drag only. Holding an ordinary
        // leave would keep a resize cursor on a pointer that has moved on, and
        // a pointer that returned mid-drag has not crossed out at all.
        let mut widget = splitter(SplitterAxis::Horizontal, false);
        assert!(widget.enter());
        assert!(widget.leave(), "with no drag behind it the leave goes up immediately");
        assert!(!widget.take_owed_leave());

        let mut dragging = grabbed(SplitterAxis::Horizontal, false);
        dragging.enter();
        assert!(!dragging.leave());
        assert!(dragging.enter(), "the pointer came back");
        dragging.drag = None;
        assert!(!dragging.take_owed_leave(), "so the crossing it deferred never completed");
    }

    #[test]
    fn the_mark_is_thin_and_lit_only_while_the_pointer_is_on_it() {
        // Tripwire: "resize highlight on left bar too wide" and "invisible at
        // rest, lit under the pointer". A mark drawn at the strip's full width,
        // or drawn at all when nothing is pointing at it, is that note back.
        let mut widget = splitter(SplitterAxis::Horizontal, false);
        assert!(widget.draw_items().is_empty(), "nothing is drawn at rest");

        widget.state.set_hovered(true);
        let items = widget.draw_items();
        assert_eq!(items.len(), 1);
        let WidgetDrawItem::Quad { width, height, color, .. } = items[0] else {
            panic!("the mark is a quad: {items:?}");
        };
        assert!((width - 2.0).abs() < f32::EPSILON, "two logical pixels of mark: {width}");
        assert!((height - widget.frame.height).abs() < f32::EPSILON, "down the whole edge: {height}");
        assert_eq!(color, widget.theme.accent);

        widget.state.set_hovered(false);
        widget.drag = Some(Drag { origin_x: 0.0, origin_y: 0.0, start_position_pixels: 360.0 });
        assert_eq!(widget.draw_items().len(), 1, "a live drag keeps the mark lit past the strip");
    }
}
