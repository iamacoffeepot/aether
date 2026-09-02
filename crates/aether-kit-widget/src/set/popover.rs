//! The popover: a plate that stands over the primary view holding controls
//! of its own, dismissed by a press outside it or by Escape.
//!
//! It exists for the owner's round-3 note 16 — "settings should be its own
//! pop-up inline window? Panel?" — and it has to satisfy round-1 note 16 at
//! the same time: "pop ups have tree text overlay where they should take
//! priority". The second is why the plate's draws go in the **overlay**
//! ([`WidgetDrawList`](crate::WidgetDrawList)'s
//! `overlay`): the root emits every overlay fill after every ordinary draw
//! *and* cuts the ordinary text under it out of what it sends, so a popover
//! covers what it stands over by not drawing it. There is no draw layer and
//! no z-index anywhere on this path; the hierarchy is the order.
//!
//! # Why this is a module and not a widget
//!
//! A popover **hosts other children**, and in this kit hosting interactive
//! children is a *root's* job, not a widget's. Pointer and keyboard routing,
//! hit rectangles, focus traversal, and drag capture all live in the root's
//! [`Focus`](crate::focus::Focus) table over the root's own direct children
//! ([`WidgetPanel`](crate::WidgetPanel) is the worked example). The two
//! container widgets in the set are the shape of that rule: `ScrollWidget`
//! re-frames and re-composites its content and keeps only a *wheel* hit table
//! of its own, and the compositing `Widget` node does not route input at all.
//! A `PopoverWidget` that owned its children's input would be a second input
//! root inside a widget — which is exactly what
//! [`EditorShell`](crate::EditorShell) is for, one level up.
//!
//! What is actually shared between two screens' popovers is therefore not an
//! actor. It is three decisions:
//!
//! - **where the plate stands** — [`place_plate`], the same flip-and-clamp
//!   rule the tooltip uses,
//! - **what the plate looks like** — [`Popover::plate_items`], a raised plate
//!   inside a hairline ring, and
//! - **when it goes away** — [`Popover::press`] and [`Popover::key`]: a press
//!   outside the plate, or Escape.
//!
//! So [`Popover`] is a plain value a root owns beside its `Focus`. The root
//! keeps spawning and framing the popover's children the way it spawns every
//! other child; the popover says whether they are up, where they are, and
//! what closes them.
//!
//! ```ignore
//! // In the root's press handler, before any other routing:
//! if self.settings.press(press.x, press.y) {
//!     // The popover closed. Hide its children with `SetWidgetState`.
//! }
//! ```

use alloc::vec::Vec;

use aether_kinds::keycode::KEY_ESCAPE;

use crate::WidgetDrawItem;
use crate::set::placement::{PlacementBounds, PlacementSide, place_plate};
use crate::set::{push_rect_border, quad};
use crate::theme::Theme;

/// The hairline a popover's ring is drawn at.
const RING_THICKNESS: f32 = 1.0;

/// One popover: whether it is up, and the plate it occupies while it is.
///
/// A root owns one of these per popover it can raise. It is pure state and
/// geometry — no mail, no actor — so a screen's dismissal rules are unit
/// testable without a running engine.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Popover {
    plate: Option<PlacementBounds>,
}

impl Popover {
    /// A closed popover.
    #[must_use]
    pub const fn new() -> Self {
        Self { plate: None }
    }

    /// Raise the popover on an explicit plate, in the root's own window
    /// pixels. Use this when the host has already decided the rectangle (a
    /// settings panel centred on the viewport); [`Self::open_beside`] is the
    /// anchored case.
    pub fn open(&mut self, plate: PlacementBounds) {
        self.plate = Some(plate);
    }

    /// Raise the popover beside `anchor` — the control that opened it —
    /// sized `width` × `height`, kept inside `bounds` and flipped to the
    /// other side of the anchor rather than hanging off the region's edge.
    /// Returns the plate it took.
    pub fn open_beside(
        &mut self,
        anchor: PlacementBounds,
        width: f32,
        height: f32,
        side: PlacementSide,
        gap: f32,
        bounds: PlacementBounds,
    ) -> PlacementBounds {
        let [x, y] = place_plate(anchor, width, height, side, gap, bounds);
        let plate = PlacementBounds { x, y, width, height }.sane();
        self.plate = Some(plate);
        plate
    }

    /// Close it. Reports whether it had been up, so a caller can fan the
    /// close (hiding the children, ending a grab) on the edge only.
    pub fn close(&mut self) -> bool {
        let was_open = self.plate.is_some();
        self.plate = None;
        was_open
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.plate.is_some()
    }

    /// The plate it occupies, or `None` while it is closed. A root frames the
    /// popover's children inside this rectangle and reports it to whatever
    /// else is drawing under it.
    #[must_use]
    pub const fn plate(&self) -> Option<PlacementBounds> {
        self.plate
    }

    /// Whether a window-pixel point is on the plate. A press here belongs to
    /// the popover's own children and must route to them as usual.
    #[must_use]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        let Some(plate) = self.plate else {
            return false;
        };
        x.is_finite()
            && y.is_finite()
            && x >= plate.x
            && x <= plate.x + plate.width
            && y >= plate.y
            && y <= plate.y + plate.height
    }

    /// Offer a press to the popover: a press **outside** an open plate
    /// dismisses it and reports `true`; a press on the plate, or a press
    /// while it is closed, changes nothing and reports `false`.
    ///
    /// This is the light-dismiss rule every platform's popover has, and it is
    /// the reason a root asks the popover before it routes a press anywhere
    /// else — the press that closes a popover is consumed by the closing, not
    /// also delivered to whatever was under it.
    pub fn press(&mut self, x: f32, y: f32) -> bool {
        if self.contains(x, y) {
            return false;
        }
        self.close()
    }

    /// Offer a key press: Escape dismisses an open popover and reports
    /// `true`; every other key, and every key while it is closed, reports
    /// `false` and is the focused child's as usual.
    pub fn key(&mut self, code: u32) -> bool {
        if code != KEY_ESCAPE {
            return false;
        }
        self.close()
    }

    /// The plate's own chrome, in the root's window pixels: a
    /// `surface_raised` fill inside a one-pixel `outline` ring — the same
    /// plate a dropdown's list and a menu's items wear, because a reader
    /// should not have to learn a second "this is standing over the screen"
    /// look. Empty while it is closed.
    ///
    /// Put these in the root's **overlay**, not its chrome: chrome flattens
    /// *before* the children, which is the wrong end for something that
    /// stands over them.
    #[must_use]
    pub fn plate_items(&self, theme: &Theme) -> Vec<WidgetDrawItem> {
        let Some(plate) = self.plate.map(PlacementBounds::sane) else {
            return Vec::new();
        };
        if plate.width <= 0.0 || plate.height <= 0.0 {
            return Vec::new();
        }
        let mut items = Vec::with_capacity(5);
        items.push(quad(plate.x, plate.y, plate.width, plate.height, theme.surface_raised));
        push_rect_border(&mut items, plate.x, plate.y, plate.width, plate.height, RING_THICKNESS, theme.outline);
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDS: PlacementBounds = PlacementBounds { x: 0.0, y: 0.0, width: 1024.0, height: 768.0 };

    fn opened() -> Popover {
        let mut popover = Popover::new();
        popover.open(PlacementBounds { x: 300.0, y: 200.0, width: 320.0, height: 240.0 });
        popover
    }

    #[test]
    fn a_press_outside_dismisses_and_a_press_on_the_plate_does_not() {
        // Tripwire: light dismiss is the whole contract. A popover that closed
        // on a press *inside* itself would shut on its own controls; one that
        // ignored a press outside would have to be closed by a button, which
        // is the modal dialog a popover exists not to be.
        let mut popover = opened();
        assert!(!popover.press(320.0, 220.0), "a press on the plate belongs to its children");
        assert!(popover.is_open());
        assert!(popover.press(100.0, 100.0), "a press outside dismisses");
        assert!(!popover.is_open());
        assert!(!popover.press(100.0, 100.0), "and a closed popover consumes nothing");
    }

    #[test]
    fn escape_dismisses_and_no_other_key_does() {
        // Tripwire: a root asks the popover before routing a key, so a
        // popover that claimed more than Escape would eat the focused child's
        // typing.
        let mut popover = opened();
        assert!(!popover.key(KEY_ESCAPE + 1));
        assert!(popover.is_open());
        assert!(popover.key(KEY_ESCAPE));
        assert!(!popover.is_open());
        assert!(!popover.key(KEY_ESCAPE), "a closed popover consumes no key either");
    }

    #[test]
    fn an_anchored_popover_stays_inside_the_region_it_was_given() {
        // Tripwire: the plate is where the root frames the popover's children,
        // so a plate half off the region places controls nobody can press.
        let mut popover = Popover::new();
        let plate = popover.open_beside(
            PlacementBounds { x: 980.0, y: 700.0, width: 40.0, height: 24.0 },
            320.0,
            240.0,
            PlacementSide::Below,
            4.0,
            BOUNDS,
        );
        assert!(plate.x >= BOUNDS.x && plate.x + plate.width <= BOUNDS.x + BOUNDS.width, "{plate:?}");
        assert!(plate.y >= BOUNDS.y && plate.y + plate.height <= BOUNDS.y + BOUNDS.height, "{plate:?}");
        assert_eq!(popover.plate(), Some(plate));
    }

    #[test]
    fn a_closed_popover_draws_nothing() {
        // Tripwire: the root appends these to its overlay every frame, and an
        // overlay fill cuts the text under it — so a plate drawn while the
        // popover is closed would silently delete a row of the screen.
        assert!(Popover::new().plate_items(&Theme::DEFAULT).is_empty());
        assert!(!opened().plate_items(&Theme::DEFAULT).is_empty());
    }
}
