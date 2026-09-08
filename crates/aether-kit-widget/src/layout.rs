//! Screen layout: the regions, columns, and rows a widget's
//! [`WidgetFrame`] comes from.
//!
//! The panel root assigns each child a rectangle and nothing else, so
//! before this module every consumer hand-computed those rectangles —
//! hundreds of lines of `x + pad`, `width - 2.0 * pad`, `y += 28.0`
//! per screen, with the design intent buried inside the arithmetic.
//! The primitives here are that arithmetic named, in the order a screen
//! is actually designed:
//!
//! 1. **Regions first.** [`dock`] splits the window into a fixed-extent
//!    pane and the viewport it sits beside. A tool pane belongs *next
//!    to* the thing it operates on, never floating over it — an overlay
//!    hides the very content the controls act on, and the viewport can
//!    no longer be sized honestly. Deciding the side and the extent up
//!    front is what makes the remaining space a known quantity.
//! 2. **Rows on the grid.** A [`Column`] stacks [`Row`]s down a region
//!    with one `gap` between them, and the same `gap` between the cells
//!    of a row. Feed that `gap` from
//!    [`Theme::space`](crate::theme::Theme::space) and every space on the
//!    screen is a whole number of spacing units — the alignment a reader
//!    perceives as "designed" is mostly just that.
//! 3. **Controls sized to content.** A cell is [`Cell::Fixed`] at the
//!    width its content needs, [`Cell::Measured`] at the width its
//!    content reported, or [`Cell::Share`] of what is left. Three
//!    buttons are three `Fixed` cells, not equal thirds of the pane:
//!    equal thirds size a control to its container, which stretches
//!    "OK" to 120 pixels and shrinks "Regenerate terrain" to a clipped
//!    stub in the same row.
//! 4. **Every cell names its slot.** A [`Cell`] carries the caller's own
//!    identifier for what it places — a mailbox id, an enum, an index —
//!    and [`Placed`] hands each frame back beside that identifier. A
//!    positional result would make every consumer keep a second parallel
//!    vector in the same order as the rows it built, and a slot skipped
//!    between the two silently shifts every frame after it onto the
//!    wrong widget.
//!
//! Nothing here is an actor and nothing here sends mail. It is pure
//! arithmetic over `f32` rectangles, so a consumer computes a whole
//! screen's frames in one place, asserts them in a unit test, and only
//! then mails them down.
//!
//! Degenerate input is clamped, never panicked on and never propagated:
//! a negative, NaN or infinite length becomes zero, and a NaN or
//! infinite position becomes zero. A layout fed a not-yet-known window
//! size collapses to empty rectangles instead of poisoning every frame
//! downstream with NaN.

use alloc::vec;
use alloc::vec::Vec;

use aether_math::Vec2;

use crate::WidgetFrame;

/// A length along one axis, sanitized. Negative, NaN and infinite all
/// collapse to zero.
///
/// Infinity is the one that does not stay in its own cell, which is why
/// the `f32::max` fold that catches NaN is not enough on its own: a
/// `Cell::share(id, f32::INFINITY)` makes a row's total weight infinite, so
/// that cell's own width is `inf / inf` — NaN — and the placement walk's
/// `x += cell_width` then carries the NaN into every later cell's origin
/// in the row. Rejecting it here is the same boundary
/// `PlacementBounds::sane` and `scroll`'s `finite_or_zero` reject it at.
fn extent(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

/// A position along one axis, sanitized. Negative is meaningful — a
/// region may legitimately start left of or above the origin — so only
/// the values that name no position are corrected: NaN, and an infinity
/// that would otherwise be handed down as every frame's `x` or `y`.
fn coord(value: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

/// Which edge of the window a pane is docked against.
///
/// Answers *where does the fixed-size furniture go* — the first question
/// of a screen, asked before any widget exists. Naming the side commits
/// the pane to an edge and turns the rest of the window into a viewport
/// with a known size, rather than a backdrop something floats over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockSide {
    Left,
    Right,
    Top,
    Bottom,
}

/// The two regions [`dock`] splits a window into. They tile the window
/// exactly: no overlap, no gutter between them. A gutter is padding
/// *inside* one of them ([`inset`]), so that the region a consumer is
/// handed is the region it may draw in.
#[derive(Debug, Clone)]
pub struct Docked {
    /// The fixed-extent pane, flush against the docked side.
    pub pane: WidgetFrame,
    /// Everything the pane did not take — the primary content area.
    /// Zero-sized along the docked axis when the pane filled the window.
    pub viewport: WidgetFrame,
}

/// Split `window` into a pane of `pane_extent` pixels along `side` and
/// the viewport that remains.
///
/// This is how a screen gets its regions, and it is deliberately the
/// only way: a consumer that wants a 320-pixel inspector beside a 3D
/// view writes one `dock` call and then never computes the viewport's
/// width again, so the two can never drift apart. `pane_extent` is
/// clamped into `0..=window` along the docked axis, so an oversized
/// pane takes the whole window and leaves a zero-width viewport instead
/// of a negative one.
#[must_use]
pub fn dock(window: WidgetFrame, side: DockSide, pane_extent: f32) -> Docked {
    let x = coord(window.x);
    let y = coord(window.y);
    let width = extent(window.width);
    let height = extent(window.height);

    let vertical = matches!(side, DockSide::Top | DockSide::Bottom);
    let leading = matches!(side, DockSide::Left | DockSide::Top);

    // The docked axis is the only one the split touches; the other axis is
    // the window's own on both regions.
    let along = if vertical {
        height
    } else {
        width
    };
    let taken = extent(pane_extent).min(along);
    let left = along - taken;
    let (pane_offset, viewport_offset) = if leading {
        (0.0, taken)
    } else {
        (left, 0.0)
    };

    if vertical {
        Docked {
            pane: WidgetFrame { x, y: y + pane_offset, width, height: taken },
            viewport: WidgetFrame { x, y: y + viewport_offset, width, height: left },
        }
    } else {
        Docked {
            pane: WidgetFrame { x: x + pane_offset, y, width: taken, height },
            viewport: WidgetFrame { x: x + viewport_offset, y, width: left, height },
        }
    }
}

/// One cell of a [`Row`]: the slot it places, and what sizes it.
///
/// Answers *what sizes this control* — a number the caller already has,
/// the width the content itself reported, or the space left over. A
/// checkbox or a square icon is [`Cell::Fixed`]; a button or a tab strip
/// that measured its own label is [`Cell::Measured`]; a text field, a
/// list, or a value readout is the [`Cell::Share`] that absorbs the
/// remainder. A row of three buttons leaves its remainder empty at the
/// right, which is why a row here never comes out as equal thirds
/// unless a designer actually asked for equal thirds.
///
/// `T` is the caller's own name for the slot — the mailbox id of the
/// child that goes in it, a screen's own `enum`, an index into its spec
/// list. It rides through untouched and comes back on the matching
/// [`Placed`] frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cell<T> {
    /// Exactly this many pixels wide, whatever the column's width is,
    /// and never shrunk: a square icon, a fixed-width numeric field, a
    /// gutter the whole screen aligns on.
    Fixed { id: T, pixels: f32 },
    /// The width the content itself reported — a widget's
    /// `WidgetDrawList::intrinsic`, a measured label plus its padding.
    /// It gets exactly `natural` while the row has room for every
    /// measured cell, and shrinks in proportion with its measured
    /// siblings (never below `min`) when it does not, so an over-full
    /// row of labels degrades together instead of the last one running
    /// off the edge.
    Measured { id: T, natural: f32, min: f32 },
    /// A weight over the width remaining once every other cell and
    /// every inter-cell gap is subtracted, never narrower than `min`.
    /// Weights are relative, not fractions: `share(1.0)` next to
    /// `share(2.0)` splits the remainder one-third / two-thirds, and a
    /// lone `share(1.0)` takes all of it. `min` is what keeps a text
    /// field beside a long button from collapsing to a sliver in a
    /// narrow pane — the row overflows honestly instead.
    Share { id: T, weight: f32, min: f32 },
}

impl<T> Cell<T> {
    /// A cell of exactly `pixels` wide.
    #[must_use]
    pub fn fixed(id: T, pixels: f32) -> Self {
        Self::Fixed { id, pixels }
    }

    /// A cell at the width its content reported, with no shrink floor.
    #[must_use]
    pub fn measured(id: T, natural: f32) -> Self {
        Self::Measured { id, natural, min: 0.0 }
    }

    /// A cell taking `weight` of what the row has left, with no floor.
    #[must_use]
    pub fn share(id: T, weight: f32) -> Self {
        Self::Share { id, weight, min: 0.0 }
    }

    /// Floor this cell's width at `min` pixels.
    ///
    /// A [`Cell::Fixed`] is already its own floor and is returned
    /// unchanged, so a caller can apply the same floor across a row it
    /// built from mixed sources without special-casing.
    #[must_use]
    pub fn at_least(self, min: f32) -> Self {
        match self {
            Self::Fixed { id, pixels } => Self::Fixed { id, pixels },
            Self::Measured { id, natural, .. } => Self::Measured { id, natural, min },
            Self::Share { id, weight, .. } => Self::Share { id, weight, min },
        }
    }

    /// The slot this cell places.
    #[must_use]
    pub fn id(&self) -> &T {
        match self {
            Self::Fixed { id, .. } | Self::Measured { id, .. } | Self::Share { id, .. } => id,
        }
    }

    /// This cell's contribution to a row's rigid width — its own pixels
    /// for a fixed cell, zero for one that is sized from what is left.
    fn rigid_width(&self) -> f32 {
        match *self {
            Self::Fixed { pixels, .. } => extent(pixels),
            Self::Measured { .. } | Self::Share { .. } => 0.0,
        }
    }

    /// What this cell asks of the width left over: the number its share
    /// is proportional to, and the floor it will not go below. A
    /// measured cell divides in proportion to the pixels it reported, a
    /// share cell in proportion to its relative weight; a fixed cell is
    /// not sized from the remainder at all.
    fn claim(&self) -> Option<Claim> {
        match *self {
            Self::Fixed { .. } => None,
            Self::Measured { natural, min, .. } => {
                Some(Claim { key: extent(natural).max(extent(min)), floor: extent(min) })
            }
            Self::Share { weight, min, .. } => Some(Claim { key: extent(weight), floor: extent(min) }),
        }
    }
}

/// One cell's claim on the width a row has left: the number its share is
/// proportional to (a measured cell's natural width, a share cell's
/// weight), and the floor it will not be cut below.
#[derive(Debug, Clone, Copy)]
struct Claim {
    key: f32,
    floor: f32,
}

/// Split `budget` across `claims` in proportion to their keys, giving no
/// claim less than its floor.
///
/// Proportional-with-a-floor cannot be done in one pass: pinning one
/// claim at its floor takes width out of the pool the rest divide, which
/// can push a second claim under *its* floor. So each pass pins every
/// claim the current split starves and re-divides what is left among the
/// claims still free — at most one pass per claim, since a pass that
/// pins nothing is the answer.
///
/// The floors win even when they do not fit: a row whose floors exceed
/// its width overflows, which is the same honest overflow a row of
/// oversized fixed cells produces rather than silently cutting a control
/// below the size its content needs.
fn distribute(budget: f32, claims: &[Claim]) -> Vec<f32> {
    let mut widths = vec![0.0_f32; claims.len()];
    let mut pinned = vec![false; claims.len()];

    for _ in 0..=claims.len() {
        let mut floors = 0.0_f32;
        let mut keys = 0.0_f32;
        for (claim, at_floor) in claims.iter().zip(&pinned) {
            if *at_floor {
                floors += claim.floor;
            } else {
                keys += claim.key;
            }
        }
        let pool = extent(budget - floors);

        let mut starved = false;
        for (index, claim) in claims.iter().enumerate() {
            if pinned[index] {
                widths[index] = claim.floor;
                continue;
            }
            widths[index] = if keys > 0.0 {
                pool * claim.key / keys
            } else {
                0.0
            };
            if widths[index] < claim.floor {
                pinned[index] = true;
                widths[index] = claim.floor;
                starved = true;
            }
        }

        if !starved {
            break;
        }
    }

    widths
}

/// One horizontal band of a [`Column`]: a height, and the cells laid
/// across it left to right.
///
/// A row is the unit a screen is read in — a label and its field, a
/// name and its value, a strip of buttons. Height is per-row rather
/// than per-cell because the eye aligns on the band, not on the
/// individual control; a taller control in a row means a taller row.
#[derive(Debug, Clone, PartialEq)]
pub struct Row<T> {
    /// The band's height in pixels. Usually
    /// [`Theme::row_height`](crate::theme::Theme::row_height), or a
    /// multiple of it for a text area or a list.
    pub height: f32,
    /// The cells across the band, in draw order left to right.
    pub cells: Vec<Cell<T>>,
}

impl<T> Row<T> {
    /// A row of one full-width cell — the common case: a heading, a
    /// slider, a text field that spans the pane.
    #[must_use]
    pub fn single(id: T, height: f32) -> Self {
        Self { height, cells: vec![Cell::share(id, 1.0)] }
    }

    /// A row of explicit cells. Reach for this when the band holds more
    /// than one control, and give each control the [`Cell`] that states
    /// what sizes it.
    #[must_use]
    pub fn cells(height: f32, cells: Vec<Cell<T>>) -> Self {
        Self { height, cells }
    }
}

/// A vertical stack of [`Row`]s at a fixed width — how a docked pane's
/// interior is described.
///
/// The column owns the one `gap` used both between rows and between the
/// cells of a row, so a screen has a single spacing rhythm rather than a
/// per-call-site literal. Take it from
/// [`Theme::space`](crate::theme::Theme::space) and every space lands on
/// the grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Column {
    /// Top-left corner of the first row, in the same pixel space as the
    /// frames it produces. Typically the top-left of an [`inset`] pane.
    pub origin: Vec2,
    /// The width every row is laid out across.
    pub width: f32,
    /// The single spacing unit, applied between adjacent rows and
    /// between adjacent cells within a row.
    pub gap: f32,
}

/// What [`Column::place`] produced: each slot's frame, and how much
/// vertical space they took.
#[derive(Debug, Clone)]
pub struct Placed<T> {
    /// One `(slot, frame)` pair per cell, in row-then-cell order — row
    /// 0's cells left to right, then row 1's, and so on. The slot is
    /// the identifier the caller put on the [`Cell`], so a consumer
    /// walks this list and addresses each frame's owner directly
    /// instead of zipping against a parallel vector it has to keep in
    /// the same order.
    pub frames: Vec<(T, WidgetFrame)>,
    /// Total occupied height, including the gaps between rows but not
    /// any trailing gap. This is what a caller stacks a second column
    /// below, or hands a scroll container as its content extent.
    pub height: f32,
}

impl<T: PartialEq> Placed<T> {
    /// The frame placed for `id`, or `None` when no cell named it.
    #[must_use]
    pub fn frame(&self, id: &T) -> Option<&WidgetFrame> {
        self.frames.iter().find(|(slot, _)| slot == id).map(|(_, frame)| frame)
    }
}

impl Column {
    /// Lay `rows` down the column and return one frame per cell.
    ///
    /// Rows stack top to bottom from [`Column::origin`] with `gap`
    /// between them; cells run left to right with the same `gap`
    /// between them. Each row's share cells divide what is left of
    /// [`Column::width`] *after* its fixed cells and all its gaps are
    /// subtracted — so adding a fixed button to a row narrows its text
    /// field by exactly the button plus one gap, with no second place to
    /// keep the two in agreement.
    ///
    /// A row with no share cells leaves its remainder empty at the
    /// right rather than stretching anything to fill it; that empty
    /// space is the point of a content-sized row. A row whose fixed
    /// cells, measured floors and share floors exceed the width
    /// overflows the column's right edge, which is reported honestly
    /// rather than by silently shrinking a control below the size its
    /// content needs.
    ///
    /// Within a row the order of service is rigid, then measured, then
    /// share: a [`Cell::Fixed`] is never touched, the
    /// [`Cell::Measured`] cells take what they reported (shrinking
    /// together toward their floors only when what they reported does
    /// not fit), and the [`Cell::Share`] cells divide whatever survives
    /// that.
    #[must_use]
    pub fn place<T: Clone>(&self, rows: &[Row<T>]) -> Placed<T> {
        let origin_x = coord(self.origin.x);
        let origin_y = coord(self.origin.y);
        let width = extent(self.width);
        let gap = extent(self.gap);

        let mut frames = Vec::new();
        let mut height = 0.0_f32;

        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                height += gap;
            }
            let y = origin_y + height;
            let row_height = extent(row.height);
            let widths = row_widths(row, width, gap);

            let mut x = origin_x;
            for (position, (cell, cell_width)) in row.cells.iter().zip(widths).enumerate() {
                if position > 0 {
                    x += gap;
                }
                frames.push((cell.id().clone(), WidgetFrame { x, y, width: cell_width, height: row_height }));
                x += cell_width;
            }

            height += row_height;
        }

        Placed { frames, height }
    }
}

/// One row's cell widths, left to right, across a column `width` wide
/// with `gap` between adjacent cells.
///
/// The measured cells and the share cells are two separate distributions
/// over what the rigid cells and the gaps left behind, because they
/// answer different questions: a measured cell asks for a width it
/// already knows and takes a cut only under pressure, while a share cell
/// exists to absorb whatever is going. Handing both to one weighted
/// split would make a 200-pixel label out-weigh a `share(1.0)` field two
/// hundred to one.
fn row_widths<T>(row: &Row<T>, width: f32, gap: f32) -> Vec<f32> {
    // One gap between each adjacent pair, accumulated the same way
    // the placement walk advances, so the two agree exactly.
    let gaps: f32 = row.cells.iter().skip(1).map(|_| gap).sum();
    let rigid: f32 = row.cells.iter().map(Cell::rigid_width).sum();
    let available = extent(width - gaps - rigid);

    let measured: Vec<Claim> =
        row.cells.iter().filter(|cell| matches!(cell, Cell::Measured { .. })).filter_map(Cell::claim).collect();
    let natural: f32 = measured.iter().map(|claim| claim.key).sum();
    let mut measured_widths = if natural <= available {
        measured.iter().map(|claim| claim.key).collect()
    } else {
        distribute(available, &measured)
    }
    .into_iter();

    let shares: Vec<Claim> =
        row.cells.iter().filter(|cell| matches!(cell, Cell::Share { .. })).filter_map(Cell::claim).collect();
    let mut share_widths = distribute(extent(available - natural.min(available)), &shares).into_iter();

    row.cells
        .iter()
        .map(|cell| match *cell {
            Cell::Fixed { pixels, .. } => extent(pixels),
            Cell::Measured { .. } => measured_widths.next().unwrap_or(0.0),
            Cell::Share { .. } => share_widths.next().unwrap_or(0.0),
        })
        .collect()
}

/// Shrink `frame` by `by` pixels on all four sides.
///
/// Answers *where does the breathing room live* — in the container, once,
/// rather than in every child's own arithmetic. Dock a pane, inset it by
/// [`Theme::pad`](crate::theme::Theme::pad), and lay a [`Column`] in the
/// result; no row then needs to know that the pane has a border.
///
/// A frame smaller than twice the padding collapses toward its own centre
/// instead of inverting: each side shrinks by at most half the frame, so
/// the result is a zero-sized rectangle in the middle of the original,
/// never a rectangle outside it.
#[must_use]
pub fn inset(frame: WidgetFrame, by: f32) -> WidgetFrame {
    let width = extent(frame.width);
    let height = extent(frame.height);
    let padding = extent(by);
    let horizontal = padding.min(width * 0.5);
    let vertical = padding.min(height * 0.5);

    WidgetFrame {
        x: coord(frame.x) + horizontal,
        y: coord(frame.y) + vertical,
        width: width - horizontal - horizontal,
        height: height - vertical - vertical,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `WidgetFrame` is a wire kind without `PartialEq`, so compare the
    /// four numbers it carries.
    fn rect(frame: &WidgetFrame) -> [f32; 4] {
        [frame.x, frame.y, frame.width, frame.height]
    }

    fn column() -> Column {
        Column { origin: Vec2::new(0.0, 0.0), width: 300.0, gap: 8.0 }
    }

    // Tripwire: the share remainder is `width - gaps - fixed`. Pinning the
    // resulting pixel widths catches the classic drift where a cell gap or a
    // fixed cell is left out of the subtraction and the row overflows its
    // column by exactly one gap. It also pins the slot identity: the frames
    // come back under the names the cells were built with, in cell order.
    #[test]
    fn share_cells_divide_the_width_left_after_fixed_cells_and_gaps() {
        let placed = column().place(&[Row::cells(
            24.0,
            vec![Cell::fixed("icon", 80.0), Cell::share("field", 1.0), Cell::share("readout", 2.0)],
        )]);

        // 300 width - 2 gaps (16) - 80 fixed = 204 to share, split 1:2.
        assert_eq!(placed.frames.iter().map(|(id, _)| *id).collect::<Vec<_>>(), ["icon", "field", "readout"]);
        assert_eq!(rect(placed.frame(&"icon").expect("placed")), [0.0, 0.0, 80.0, 24.0]);
        assert_eq!(rect(placed.frame(&"field").expect("placed")), [88.0, 0.0, 68.0, 24.0]);
        assert_eq!(rect(placed.frame(&"readout").expect("placed")), [164.0, 0.0, 136.0, 24.0]);
        assert_eq!(placed.frames[2].1.x + placed.frames[2].1.width, 300.0);
        assert!(placed.frame(&"absent").is_none());
    }

    // Tripwire: a content-sized button row must leave the leftover empty
    // rather than stretching to equal thirds — the whole reason `Fixed`
    // exists. Pins the right edge short of the column width.
    #[test]
    fn a_row_of_fixed_cells_leaves_the_remainder_empty_at_the_right() {
        let placed =
            column().place(&[Row::cells(24.0, vec![Cell::fixed(0, 60.0), Cell::fixed(1, 90.0), Cell::fixed(2, 40.0)])]);

        assert_eq!(rect(&placed.frames[0].1), [0.0, 0.0, 60.0, 24.0]);
        assert_eq!(rect(&placed.frames[1].1), [68.0, 0.0, 90.0, 24.0]);
        assert_eq!(rect(&placed.frames[2].1), [166.0, 0.0, 40.0, 24.0]);
        assert_eq!(placed.frames[2].1.x + placed.frames[2].1.width, 206.0);
    }

    // Tripwire: a measured cell takes exactly what it reported while the row
    // fits, and the share beside it gets what is left — the arithmetic a
    // consumer would otherwise redo from a widget's `intrinsic`. Under
    // pressure the measured cells shrink *together* and stop at their floors,
    // which is the failure a single-pass proportional split gets wrong: it
    // starves the smaller cell to nothing while the larger one keeps most of
    // its width.
    #[test]
    fn measured_cells_take_what_they_reported_and_shrink_together_under_pressure() {
        let roomy = column().place(&[Row::cells(24.0, vec![Cell::measured("label", 90.0), Cell::share("field", 1.0)])]);
        assert_eq!(rect(&roomy.frames[0].1), [0.0, 0.0, 90.0, 24.0]);
        assert_eq!(rect(&roomy.frames[1].1), [98.0, 0.0, 202.0, 24.0], "the share takes 300 - 8 gap - 90");

        // 100 wide, one gap: 92 for two measured cells asking 150 together,
        // so each keeps 92/150 of what it asked — except that cuts the 50 to
        // 30.7, under its floor, so it pins at 40 and the other takes 52.
        let tight = Column { origin: Vec2::new(0.0, 0.0), width: 100.0, gap: 8.0 }.place(&[Row::cells(
            24.0,
            vec![Cell::measured("wide", 100.0), Cell::measured("narrow", 50.0).at_least(40.0)],
        )]);
        assert_eq!(rect(&tight.frames[0].1), [0.0, 0.0, 52.0, 24.0]);
        assert_eq!(rect(&tight.frames[1].1), [60.0, 0.0, 40.0, 24.0]);
    }

    // Tripwire: a share floor is the whole reason the variant carries one —
    // a field beside a long button must stop collapsing at its floor and let
    // the row overflow honestly, not shrink to a sliver no reader can type
    // in. Pins both the floored cell and the sibling that keeps the rest.
    #[test]
    fn a_share_floor_holds_and_the_row_overflows_rather_than_collapsing() {
        let placed = Column { origin: Vec2::new(0.0, 0.0), width: 120.0, gap: 8.0 }
            .place(&[Row::cells(24.0, vec![Cell::fixed("button", 100.0), Cell::share("field", 1.0).at_least(60.0)])]);

        assert_eq!(rect(&placed.frames[0].1), [0.0, 0.0, 100.0, 24.0]);
        assert_eq!(rect(&placed.frames[1].1), [108.0, 0.0, 60.0, 24.0]);
        assert_eq!(placed.frames[1].1.x + placed.frames[1].1.width, 168.0, "and says so by overflowing");
    }

    // Tripwire: rows carry a gap *between* them and none after the last, and
    // `height` is the occupied extent a caller stacks or scrolls against. An
    // off-by-one gap here silently mis-sizes every scroll extent.
    #[test]
    fn rows_stack_with_one_gap_between_and_report_the_occupied_height() {
        let placed = Column { origin: Vec2::new(10.0, 20.0), width: 200.0, gap: 8.0 }.place(&[
            Row::single("top", 24.0),
            Row::single("middle", 24.0),
            Row::single("bottom", 48.0),
        ]);

        assert_eq!(rect(&placed.frames[0].1), [10.0, 20.0, 200.0, 24.0]);
        assert_eq!(rect(&placed.frames[1].1), [10.0, 52.0, 200.0, 24.0]);
        assert_eq!(rect(&placed.frames[2].1), [10.0, 84.0, 200.0, 48.0]);
        // 24 + 8 + 24 + 8 + 48, with no trailing gap.
        assert_eq!(placed.height, 112.0);
        assert_eq!(Column { origin: Vec2::new(10.0, 20.0), width: 200.0, gap: 8.0 }.place::<u8>(&[]).height, 0.0);
    }

    // Tripwire: the pane and viewport must tile the window exactly on every
    // side, and an oversized pane must clamp rather than produce a negative
    // viewport width that a downstream `width - pad` turns into garbage.
    #[test]
    fn dock_tiles_the_window_and_clamps_an_oversized_pane() {
        let window = WidgetFrame { x: 0.0, y: 0.0, width: 1280.0, height: 720.0 };

        let right = dock(window.clone(), DockSide::Right, 320.0);
        assert_eq!(rect(&right.pane), [960.0, 0.0, 320.0, 720.0]);
        assert_eq!(rect(&right.viewport), [0.0, 0.0, 960.0, 720.0]);

        let left = dock(window.clone(), DockSide::Left, 320.0);
        assert_eq!(rect(&left.pane), [0.0, 0.0, 320.0, 720.0]);
        assert_eq!(rect(&left.viewport), [320.0, 0.0, 960.0, 720.0]);

        let bottom = dock(window.clone(), DockSide::Bottom, 180.0);
        assert_eq!(rect(&bottom.pane), [0.0, 540.0, 1280.0, 180.0]);
        assert_eq!(rect(&bottom.viewport), [0.0, 0.0, 1280.0, 540.0]);

        let top = dock(window.clone(), DockSide::Top, 180.0);
        assert_eq!(rect(&top.pane), [0.0, 0.0, 1280.0, 180.0]);
        assert_eq!(rect(&top.viewport), [0.0, 180.0, 1280.0, 540.0]);

        let oversized = dock(window, DockSide::Right, 5000.0);
        assert_eq!(rect(&oversized.pane), [0.0, 0.0, 1280.0, 720.0]);
        assert_eq!(rect(&oversized.viewport), [0.0, 0.0, 0.0, 720.0]);
    }

    // Tripwire: padding larger than half the frame must collapse to the
    // centre, not invert into a rectangle wider than what it padded.
    #[test]
    fn inset_pads_all_sides_and_collapses_rather_than_inverting() {
        let padded = inset(WidgetFrame { x: 10.0, y: 20.0, width: 300.0, height: 100.0 }, 12.0);
        assert_eq!(rect(&padded), [22.0, 32.0, 276.0, 76.0]);

        // Padding wider than the frame collapses that axis to the centre line
        // (x 10 + 8, zero width) while the roomy axis pads normally.
        let collapsed = inset(WidgetFrame { x: 10.0, y: 20.0, width: 16.0, height: 100.0 }, 30.0);
        assert_eq!(rect(&collapsed), [18.0, 50.0, 0.0, 40.0]);
    }

    // Tripwire: a layout is fed window sizes and theme metrics that can be
    // absent or not-yet-known. NaN and negatives must clamp to zero here
    // instead of propagating into every mailed frame.
    #[test]
    fn degenerate_input_clamps_to_zero_instead_of_propagating() {
        let docked =
            dock(WidgetFrame { x: f32::NAN, y: 0.0, width: f32::NAN, height: -400.0 }, DockSide::Left, f32::NAN);
        assert_eq!(rect(&docked.pane), [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(rect(&docked.viewport), [0.0, 0.0, 0.0, 0.0]);

        let placed = Column { origin: Vec2::new(f32::NAN, 5.0), width: -100.0, gap: -8.0 }.place(&[
            Row::cells(f32::NAN, vec![Cell::fixed(0, -20.0), Cell::share(1, f32::NAN).at_least(f32::NAN)]),
            Row::single(2, -10.0),
        ]);
        for (_, frame) in &placed.frames {
            assert_eq!(rect(frame), [0.0, 5.0, 0.0, 0.0]);
        }
        assert_eq!(placed.height, 0.0);

        assert_eq!(
            rect(&inset(WidgetFrame { x: 0.0, y: f32::NAN, width: -5.0, height: f32::NAN }, f32::NAN)),
            [0.0, 0.0, 0.0, 0.0]
        );
    }

    // Tripwire: infinity is the degenerate value NaN's `f32::max` fold does
    // not catch, and it does not stay in its own cell. A `Share(inf)` makes
    // the row's total weight infinite, so its own width is `inf / inf` = NaN,
    // and `x += NaN` then poisons every later cell's origin in that row — the
    // exact propagation the module header and the guide both promise cannot
    // happen. `Fixed(inf)` is the same hole with an infinite coordinate, and
    // an infinite `Measured` natural is the third door into it.
    #[test]
    fn an_infinite_extent_collapses_like_a_nan_one() {
        let placed = Column { origin: Vec2::new(0.0, 0.0), width: 400.0, gap: 8.0 }
            .place(&[Row::cells(24.0, vec![Cell::share(0, f32::INFINITY), Cell::fixed(1, 60.0)])]);
        assert_eq!(rect(&placed.frames[0].1), [0.0, 0.0, 0.0, 24.0], "an infinite weight takes no width");
        assert_eq!(rect(&placed.frames[1].1), [8.0, 0.0, 60.0, 24.0], "and the cell after it keeps its own origin");

        let fixed = Column { origin: Vec2::new(0.0, 0.0), width: 400.0, gap: 8.0 }
            .place(&[Row::cells(24.0, vec![Cell::fixed(0, f32::INFINITY), Cell::share(1, 1.0)])]);
        assert_eq!(rect(&fixed.frames[0].1), [0.0, 0.0, 0.0, 24.0]);
        assert_eq!(rect(&fixed.frames[1].1), [8.0, 0.0, 392.0, 24.0]);

        let measured = Column { origin: Vec2::new(0.0, 0.0), width: 400.0, gap: 8.0 }
            .place(&[Row::cells(24.0, vec![Cell::measured(0, f32::INFINITY), Cell::share(1, 1.0)])]);
        assert_eq!(rect(&measured.frames[0].1), [0.0, 0.0, 0.0, 24.0]);
        assert_eq!(rect(&measured.frames[1].1), [8.0, 0.0, 392.0, 24.0]);

        let unplaceable = Column { origin: Vec2::new(f32::INFINITY, 20.0), width: f32::INFINITY, gap: f32::INFINITY }
            .place(&[Row::single(0, f32::INFINITY)]);
        assert_eq!(rect(&unplaceable.frames[0].1), [0.0, 20.0, 0.0, 0.0]);
        assert_eq!(unplaceable.height, 0.0);

        assert_eq!(
            rect(
                &dock(
                    WidgetFrame { x: f32::INFINITY, y: 0.0, width: f32::INFINITY, height: 720.0 },
                    DockSide::Left,
                    f32::INFINITY,
                )
                .pane
            ),
            [0.0, 0.0, 0.0, 720.0],
        );
    }
}
