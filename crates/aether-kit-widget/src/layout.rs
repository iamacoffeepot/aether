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
//!    width its content needs, or [`Cell::Share`] of what is left. Three
//!    buttons are three `Fixed` cells, not equal thirds of the pane:
//!    equal thirds size a control to its container, which stretches
//!    "OK" to 120 pixels and shrinks "Regenerate terrain" to a clipped
//!    stub in the same row.
//!
//! Nothing here is an actor and nothing here sends mail. It is pure
//! arithmetic over `f32` rectangles, so a consumer computes a whole
//! screen's frames in one place, asserts them in a unit test, and only
//! then mails them down.
//!
//! Degenerate input is clamped, never panicked on and never propagated:
//! a negative or NaN length becomes zero, and a NaN position becomes
//! zero. A layout fed a not-yet-known window size collapses to empty
//! rectangles instead of poisoning every frame downstream with NaN.

use alloc::vec;
use alloc::vec::Vec;

use aether_math::Vec2;

use crate::WidgetFrame;

/// A length along one axis, sanitized. Negative and NaN both collapse to
/// zero: `f32::max` returns its non-NaN operand, which is what folds the
/// NaN case in without a separate branch.
fn extent(value: f32) -> f32 {
    value.max(0.0)
}

/// A position along one axis, sanitized. Negative is meaningful — a
/// region may legitimately start left of or above the origin — so only
/// NaN is corrected.
fn coord(value: f32) -> f32 {
    if value.is_nan() {
        0.0
    } else {
        value
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

/// How wide one cell of a [`Row`] is.
///
/// Answers *what sizes this control* — its content, or the space left
/// over. A button, a checkbox, a label, an icon are [`Cell::Fixed`] at
/// their intrinsic width; a text field, a list, or a value readout is
/// the one [`Cell::Share`] that absorbs the remainder. A row of three
/// buttons is three `Fixed` cells with the remainder left empty at the
/// right, which is why a row here never comes out as equal thirds
/// unless a designer actually asked for equal thirds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cell {
    /// Exactly this many pixels wide, whatever the column's width is.
    /// The intrinsic width of the content — a measured label plus its
    /// padding, a square icon, a fixed-width numeric field.
    Fixed(f32),
    /// A weight over the width remaining once every [`Cell::Fixed`] and
    /// every inter-cell gap is subtracted. Weights are relative, not
    /// fractions: `Share(1.0)` next to `Share(2.0)` splits the remainder
    /// one-third / two-thirds, and a lone `Share(1.0)` takes all of it.
    Share(f32),
}

/// One horizontal band of a [`Column`]: a height, and the cells laid
/// across it left to right.
///
/// A row is the unit a screen is read in — a label and its field, a
/// name and its value, a strip of buttons. Height is per-row rather
/// than per-cell because the eye aligns on the band, not on the
/// individual control; a taller control in a row means a taller row.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The band's height in pixels. Usually
    /// [`Theme::row_height`](crate::theme::Theme::row_height), or a
    /// multiple of it for a text area or a list.
    pub height: f32,
    /// The cells across the band, in draw order left to right.
    pub cells: Vec<Cell>,
}

impl Row {
    /// A row of one full-width cell — the common case: a heading, a
    /// slider, a text field that spans the pane.
    #[must_use]
    pub fn single(height: f32) -> Self {
        Self { height, cells: vec![Cell::Share(1.0)] }
    }

    /// A row of explicit cells. Reach for this when the band holds more
    /// than one control, and give each control the [`Cell`] that states
    /// what sizes it.
    #[must_use]
    pub fn cells(height: f32, cells: Vec<Cell>) -> Self {
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

/// What [`Column::place`] produced: the frames, and how much vertical
/// space they took.
#[derive(Debug, Clone)]
pub struct Placed {
    /// One frame per cell, in row-then-cell order — row 0's cells left
    /// to right, then row 1's, and so on. A consumer zips this against
    /// the same flat list of children it built the rows from.
    pub frames: Vec<WidgetFrame>,
    /// Total occupied height, including the gaps between rows but not
    /// any trailing gap. This is what a caller stacks a second column
    /// below, or hands a scroll container as its content extent.
    pub height: f32,
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
    /// cells exceed the width overflows the column's right edge, which
    /// is reported honestly rather than by silently shrinking a control
    /// below the size its content needs.
    #[must_use]
    pub fn place(&self, rows: &[Row]) -> Placed {
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

            // One gap between each adjacent pair, accumulated the same way
            // the placement walk below advances, so the two agree exactly.
            let gaps: f32 = row.cells.iter().skip(1).map(|_| gap).sum();
            let fixed: f32 = row.cells.iter().copied().map(Cell::fixed_width).sum();
            let shares: f32 = row.cells.iter().copied().map(Cell::share_weight).sum();
            let remainder = extent(width - gaps - fixed);

            let mut x = origin_x;
            for (position, cell) in row.cells.iter().enumerate() {
                if position > 0 {
                    x += gap;
                }
                let cell_width = match *cell {
                    Cell::Fixed(pixels) => extent(pixels),
                    Cell::Share(weight) if shares > 0.0 => remainder * extent(weight) / shares,
                    Cell::Share(_) => 0.0,
                };
                frames.push(WidgetFrame { x, y, width: cell_width, height: row_height });
                x += cell_width;
            }

            height += row_height;
        }

        Placed { frames, height }
    }
}

impl Cell {
    /// This cell's contribution to a row's fixed width; zero for a share.
    fn fixed_width(self) -> f32 {
        match self {
            Self::Fixed(pixels) => extent(pixels),
            Self::Share(_) => 0.0,
        }
    }

    /// This cell's contribution to a row's total share weight; zero for a
    /// fixed cell.
    fn share_weight(self) -> f32 {
        match self {
            Self::Share(weight) => extent(weight),
            Self::Fixed(_) => 0.0,
        }
    }
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
    // column by exactly one gap.
    #[test]
    fn share_cells_divide_the_width_left_after_fixed_cells_and_gaps() {
        let placed = column().place(&[Row::cells(24.0, vec![Cell::Fixed(80.0), Cell::Share(1.0), Cell::Share(2.0)])]);

        // 300 width - 2 gaps (16) - 80 fixed = 204 to share, split 1:2.
        assert_eq!(rect(&placed.frames[0]), [0.0, 0.0, 80.0, 24.0]);
        assert_eq!(rect(&placed.frames[1]), [88.0, 0.0, 68.0, 24.0]);
        assert_eq!(rect(&placed.frames[2]), [164.0, 0.0, 136.0, 24.0]);
        assert_eq!(placed.frames[2].x + placed.frames[2].width, 300.0);
    }

    // Tripwire: a content-sized button row must leave the leftover empty
    // rather than stretching to equal thirds — the whole reason `Fixed`
    // exists. Pins the right edge short of the column width.
    #[test]
    fn a_row_of_fixed_cells_leaves_the_remainder_empty_at_the_right() {
        let placed = column().place(&[Row::cells(24.0, vec![Cell::Fixed(60.0), Cell::Fixed(90.0), Cell::Fixed(40.0)])]);

        assert_eq!(rect(&placed.frames[0]), [0.0, 0.0, 60.0, 24.0]);
        assert_eq!(rect(&placed.frames[1]), [68.0, 0.0, 90.0, 24.0]);
        assert_eq!(rect(&placed.frames[2]), [166.0, 0.0, 40.0, 24.0]);
        assert_eq!(placed.frames[2].x + placed.frames[2].width, 206.0);
    }

    // Tripwire: rows carry a gap *between* them and none after the last, and
    // `height` is the occupied extent a caller stacks or scrolls against. An
    // off-by-one gap here silently mis-sizes every scroll extent.
    #[test]
    fn rows_stack_with_one_gap_between_and_report_the_occupied_height() {
        let placed = Column { origin: Vec2::new(10.0, 20.0), width: 200.0, gap: 8.0 }.place(&[
            Row::single(24.0),
            Row::single(24.0),
            Row::single(48.0),
        ]);

        assert_eq!(rect(&placed.frames[0]), [10.0, 20.0, 200.0, 24.0]);
        assert_eq!(rect(&placed.frames[1]), [10.0, 52.0, 200.0, 24.0]);
        assert_eq!(rect(&placed.frames[2]), [10.0, 84.0, 200.0, 48.0]);
        // 24 + 8 + 24 + 8 + 48, with no trailing gap.
        assert_eq!(placed.height, 112.0);
        assert_eq!(Column { origin: Vec2::new(10.0, 20.0), width: 200.0, gap: 8.0 }.place(&[]).height, 0.0);
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

        let placed = Column { origin: Vec2::new(f32::NAN, 5.0), width: -100.0, gap: -8.0 }
            .place(&[Row::cells(f32::NAN, vec![Cell::Fixed(-20.0), Cell::Share(f32::NAN)]), Row::single(-10.0)]);
        for frame in &placed.frames {
            assert_eq!(rect(frame), [0.0, 5.0, 0.0, 0.0]);
        }
        assert_eq!(placed.height, 0.0);

        assert_eq!(
            rect(&inset(WidgetFrame { x: 0.0, y: f32::NAN, width: -5.0, height: f32::NAN }, f32::NAN)),
            [0.0, 0.0, 0.0, 0.0]
        );
    }
}
