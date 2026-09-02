//! Where an overlay plate stands: beside the thing it is about, and inside
//! the region it is allowed to cover.
//!
//! [`layout`](crate::layout) tiles a window into regions that never overlap.
//! This is the other half — the arithmetic for the plates that deliberately
//! do overlap: a tooltip beside the row it explains, a popover over the
//! primary view. Both want the same two rules, so both get them from here
//! rather than from a copy each: put the plate on the side the caller asked
//! for, **flip to the other side of the anchor** when that side would run
//! past the bounds, and clamp what is left into the bounds so the plate is
//! never half off the region.
//!
//! A plate over a canvas wants one thing more: to keep off the furniture that
//! is already on it. [`place_plate_avoiding`] adds a list of rectangles the
//! plate would rather not cover and picks the side that covers the least of
//! them, ranking the first entry above all the rest — the caller's lever for
//! "this one is the thing the plate is about".
//!
//! Pure `f32` geometry — no actor, no mail — so a consumer computes a plate's
//! rectangle and asserts it in a unit test before anything is drawn.
//!
//! The bounds are data, not something a widget can look up: a widget never
//! talks to the window cap, so the region a plate must stay inside arrives on
//! the config that asked for the plate.

use serde::{Deserialize, Serialize};

use crate::WidgetFrame;

/// A rectangle in the same window-pixel space a [`WidgetFrame`] is assigned
/// in — an anchor to stand beside, or the region a plate must stay inside.
/// Schema-only: it is always a nested field of a widget's config, never a
/// mail payload of its own.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Default)]
pub struct PlacementBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl PlacementBounds {
    /// This rectangle with every degenerate value clamped away: a NaN
    /// position becomes zero and a negative or NaN length becomes zero, the
    /// same rule [`layout`](crate::layout) applies, so a plate placed before
    /// the window size is known collapses instead of poisoning every
    /// downstream coordinate with NaN.
    #[must_use]
    pub fn sane(self) -> Self {
        let finite = |value: f32| {
            if value.is_finite() {
                value
            } else {
                0.0
            }
        };
        let length = |value: f32| finite(value).max(0.0);
        Self { x: finite(self.x), y: finite(self.y), width: length(self.width), height: length(self.height) }
    }

    fn right(self) -> f32 {
        self.x + self.width
    }

    fn bottom(self) -> f32 {
        self.y + self.height
    }

    /// The area this rectangle and `other` share, in square pixels. Zero when
    /// they only touch — an edge in common is not a cover.
    #[must_use]
    fn overlap_area(self, other: Self) -> f32 {
        let (a, b) = (self.sane(), other.sane());
        let width = a.right().min(b.right()) - a.x.max(b.x);
        let height = a.bottom().min(b.bottom()) - a.y.max(b.y);
        if width <= 0.0 || height <= 0.0 {
            return 0.0;
        }
        width * height
    }
}

impl From<&WidgetFrame> for PlacementBounds {
    fn from(frame: &WidgetFrame) -> Self {
        Self { x: frame.x, y: frame.y, width: frame.width, height: frame.height }
    }
}

/// Which side of its anchor a plate prefers to stand on. The preference is
/// honoured whenever the plate fits there; otherwise it flips to the opposite
/// side, which is the one place the plate is guaranteed not to cover the
/// anchor it is about.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlacementSide {
    /// Under the anchor — the tooltip default, because a plate under a row
    /// leaves the row itself readable.
    #[default]
    Below,
    /// Over the anchor.
    Above,
    /// To the anchor's right.
    Right,
    /// To the anchor's left.
    Left,
}

impl PlacementSide {
    /// The side a flip lands on.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Below => Self::Above,
            Self::Above => Self::Below,
            Self::Right => Self::Left,
            Self::Left => Self::Right,
        }
    }

    /// Whether this side displaces the plate along y (`Below` / `Above`)
    /// rather than along x.
    const fn is_vertical(self) -> bool {
        matches!(self, Self::Below | Self::Above)
    }

    /// Whether the plate is placed *after* the anchor on its axis, which is
    /// what decides whether the gap is added to the anchor's far edge or
    /// subtracted from its near one.
    const fn is_after(self) -> bool {
        matches!(self, Self::Below | Self::Right)
    }

    /// The sides a plate will try, in order, when it is allowed to move off
    /// its preference to get clear of an obstacle: this side, its opposite,
    /// then the other axis. Preference first, so a plate with nothing in its
    /// way lands exactly where [`place_plate`] would put it.
    const fn candidates(self) -> [Self; 4] {
        match self {
            Self::Below => [Self::Below, Self::Above, Self::Right, Self::Left],
            Self::Above => [Self::Above, Self::Below, Self::Right, Self::Left],
            Self::Right => [Self::Right, Self::Left, Self::Below, Self::Above],
            Self::Left => [Self::Left, Self::Right, Self::Below, Self::Above],
        }
    }
}

/// The top-left corner of a `width` × `height` plate standing `gap` pixels
/// off `anchor` on `side`, kept inside `bounds`.
///
/// The plate flips to [`PlacementSide::opposite`] when the preferred side
/// would put it past the bounds and the opposite side would not — a tooltip
/// on the last row of a pane stands above that row rather than half under the
/// pane's bottom edge. When neither side fits (a plate taller than the region
/// it lives in) the preferred side is kept and clamped, because clamping the
/// side the caller asked for is more predictable than flipping to an equally
/// bad one.
///
/// The cross axis starts flush with the anchor — a tooltip's left edge lines
/// up with its row's left edge — and is then clamped the same way.
#[must_use]
pub fn place_plate(
    anchor: PlacementBounds,
    width: f32,
    height: f32,
    side: PlacementSide,
    gap: f32,
    bounds: PlacementBounds,
) -> [f32; 2] {
    let anchor = anchor.sane();
    let bounds = bounds.sane();
    let plate = PlacementBounds { x: 0.0, y: 0.0, width, height }.sane();
    let gap = if gap.is_finite() {
        gap.max(0.0)
    } else {
        0.0
    };

    let (main_extent, cross_origin, cross_extent) = if side.is_vertical() {
        (plate.height, anchor.x, plate.width)
    } else {
        (plate.width, anchor.y, plate.height)
    };
    let (main_low, main_high, cross_low, cross_high) = if side.is_vertical() {
        (bounds.y, bounds.bottom(), bounds.x, bounds.right())
    } else {
        (bounds.x, bounds.right(), bounds.y, bounds.bottom())
    };

    let preferred = main_origin(side, anchor, main_extent, gap);
    let main = if fits(preferred, main_extent, main_low, main_high) {
        preferred
    } else {
        let flipped = main_origin(side.opposite(), anchor, main_extent, gap);
        if fits(flipped, main_extent, main_low, main_high) {
            flipped
        } else {
            preferred
        }
    };

    let main = clamp_into(main, main_extent, main_low, main_high);
    let cross = clamp_into(cross_origin, cross_extent, cross_low, cross_high);
    if side.is_vertical() {
        [cross, main]
    } else {
        [main, cross]
    }
}

/// [`place_plate`], then moved off its preferred side when that would cover
/// something the caller named in `avoid`.
///
/// Each of the four sides is placed by the ordinary flip-and-clamp rule, and
/// the one that covers the least of `avoid` wins. **The first entry outranks
/// every other**: a candidate is judged first on how much of `avoid[0]` it
/// covers and only then on the total it covers of the rest. That ordering is
/// the caller's lever — a hover card's first obstacle is the thing the card is
/// *about*, which it must stay attached to and never cover, while the rest of
/// the frame's furniture is a preference. Ties go to the earlier candidate, so
/// the preferred side is kept whenever moving would buy nothing, and an empty
/// `avoid` is exactly [`place_plate`].
///
/// Nothing here guarantees a clear placement: a plate larger than the gaps
/// between the obstacles covers something whichever side it takes, and the
/// least-covering side is then the honest answer rather than a refusal to
/// draw.
#[must_use]
pub fn place_plate_avoiding(
    anchor: PlacementBounds,
    width: f32,
    height: f32,
    side: PlacementSide,
    gap: f32,
    bounds: PlacementBounds,
    avoid: &[PlacementBounds],
) -> [f32; 2] {
    let placed = |side: PlacementSide| place_plate(anchor, width, height, side, gap, bounds);
    if avoid.is_empty() {
        return placed(side);
    }

    let cost = |origin: [f32; 2]| {
        let plate = PlacementBounds { x: origin[0], y: origin[1], width, height };
        let first = avoid.first().map_or(0.0, |rect| plate.overlap_area(*rect));
        let rest: f32 = avoid.iter().skip(1).map(|rect| plate.overlap_area(*rect)).sum();
        (first, rest)
    };

    side.candidates()
        .into_iter()
        .map(|candidate| {
            let origin = placed(candidate);
            (cost(origin), origin)
        })
        .reduce(|best, next| {
            let better = next.0.0.total_cmp(&best.0.0).then_with(|| next.0.1.total_cmp(&best.0.1)).is_lt();
            if better {
                next
            } else {
                best
            }
        })
        .map_or_else(|| placed(side), |(_, origin)| origin)
}

/// Where `side` puts the plate's near edge on the axis it displaces along,
/// before any clamping.
fn main_origin(side: PlacementSide, anchor: PlacementBounds, extent: f32, gap: f32) -> f32 {
    let (near, far) = if side.is_vertical() {
        (anchor.y, anchor.bottom())
    } else {
        (anchor.x, anchor.right())
    };
    if side.is_after() {
        far + gap
    } else {
        near - gap - extent
    }
}

fn fits(origin: f32, extent: f32, low: f32, high: f32) -> bool {
    origin >= low && origin + extent <= high
}

/// `origin` moved the least distance that puts a run of `extent` inside
/// `low..=high`. A run wider than the span sits at `low`: cutting the far end
/// off keeps the beginning of the text readable, and cutting the near end off
/// would not.
fn clamp_into(origin: f32, extent: f32, low: f32, high: f32) -> f32 {
    if extent >= high - low {
        return low;
    }
    origin.clamp(low, high - extent)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDS: PlacementBounds = PlacementBounds { x: 0.0, y: 0.0, width: 400.0, height: 300.0 };

    #[test]
    fn a_plate_that_would_run_past_the_bounds_flips_to_the_other_side_of_its_anchor() {
        // Tripwire: the flip is the whole reason placement is shared. A
        // tooltip on the last row of a pane must stand *above* that row, not
        // clamped up over the row it is about, and a plate that fits below
        // must not flip anyway.
        let row = |y: f32| PlacementBounds { x: 20.0, y, width: 100.0, height: 20.0 };

        let [_, top] = place_plate(row(40.0), 120.0, 80.0, PlacementSide::Below, 4.0, BOUNDS);
        assert!((top - 64.0).abs() < f32::EPSILON, "a plate that fits stays under its row: {top}");

        let [_, flipped] = place_plate(row(260.0), 120.0, 80.0, PlacementSide::Below, 4.0, BOUNDS);
        assert!((flipped - 176.0).abs() < f32::EPSILON, "and one that does not stands above it: {flipped}");
        assert!(flipped + 80.0 <= 260.0, "clear of the row it explains: {flipped}");

        let [right, _] = place_plate(row(40.0), 120.0, 80.0, PlacementSide::Right, 4.0, BOUNDS);
        assert!((right - 124.0).abs() < f32::EPSILON, "a horizontal side displaces along x: {right}");
        let [left, _] = place_plate(
            PlacementBounds { x: 330.0, y: 40.0, width: 60.0, height: 20.0 },
            120.0,
            80.0,
            PlacementSide::Right,
            4.0,
            BOUNDS,
        );
        assert!((left - 206.0).abs() < f32::EPSILON, "and flips to the left when the right runs out: {left}");
    }

    #[test]
    fn a_plate_is_clamped_inside_the_bounds_on_both_axes() {
        // Tripwire: the cross axis starts flush with the anchor, so a row near
        // the region's right edge would push the plate out of the region
        // entirely without this clamp — the "popup over the tree" the bounds
        // exist to prevent.
        let [x, y] = place_plate(
            PlacementBounds { x: 380.0, y: 10.0, width: 20.0, height: 20.0 },
            120.0,
            80.0,
            PlacementSide::Below,
            4.0,
            BOUNDS,
        );
        assert!((x - 280.0).abs() < f32::EPSILON, "pulled back inside the right edge: {x}");
        assert!(y >= 0.0 && y + 80.0 <= 300.0, "and still inside vertically: {y}");

        let [tall_x, tall_y] = place_plate(
            PlacementBounds { x: 20.0, y: 100.0, width: 20.0, height: 20.0 },
            500.0,
            400.0,
            PlacementSide::Below,
            4.0,
            BOUNDS,
        );
        assert!(
            (tall_x - BOUNDS.x).abs() < f32::EPSILON && (tall_y - BOUNDS.y).abs() < f32::EPSILON,
            "a plate larger than its bounds starts at their origin: {tall_x}, {tall_y}",
        );
    }

    #[test]
    fn a_plate_takes_the_side_that_covers_the_least_of_what_it_was_told_to_avoid() {
        // Tripwire: the ranking is the whole point of `avoid`. A hover card
        // must get off the thing it is about first — that is what keeps it
        // attached to its own node — and only then off the rest of the frame,
        // so a candidate clear of `avoid[0]` outranks one that is merely clear
        // of more total area.
        let anchor = PlacementBounds { x: 180.0, y: 140.0, width: 20.0, height: 20.0 };
        let card = |avoid: &[PlacementBounds]| {
            place_plate_avoiding(anchor, 120.0, 80.0, PlacementSide::Below, 4.0, BOUNDS, avoid)
        };

        assert_eq!(card(&[]), place_plate(anchor, 120.0, 80.0, PlacementSide::Below, 4.0, BOUNDS), "nothing to avoid");

        let band = PlacementBounds { x: 0.0, y: 160.0, width: 400.0, height: 120.0 };
        assert_eq!(card(&[band]), [180.0, 56.0], "below, left and right all sit in the band, so the plate goes above");

        let over_the_top = PlacementBounds { x: 150.0, y: 40.0, width: 200.0, height: 120.0 };
        assert_eq!(
            card(&[band, over_the_top]),
            [180.0, 56.0],
            "and it stays above even though that is the only side the second obstacle covers",
        );
    }

    #[test]
    fn degenerate_geometry_collapses_instead_of_propagating_nan() {
        // Tripwire: a plate placed before the window size is known must not
        // hand NaN to every draw downstream.
        let [x, y] = place_plate(
            PlacementBounds { x: f32::NAN, y: 10.0, width: -5.0, height: f32::INFINITY },
            f32::NAN,
            80.0,
            PlacementSide::Below,
            f32::NAN,
            BOUNDS,
        );
        assert!(x.is_finite() && y.is_finite(), "{x}, {y}");
    }
}
