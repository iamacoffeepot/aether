//! Chaining short segments into strokes.
//!
//! A level set arrives as one tiny segment per triangle it crosses —
//! hundreds of thousands of them, in no order. A drawing needs the
//! opposite: long continuous lines that a pen could actually travel.
//! Welding is what converts one into the other, and it is the single
//! step that decides whether the output reads as a drawing or as
//! stippling: styling a welded line tapers it once at each true end,
//! while styling the raw segments tapers every one of them and the
//! whole image turns to dust.
//!
//! Endpoints that should join are bit-identical by construction (see
//! `Mesh::crossing`), so the index is an exact hash on quantised
//! coordinates rather than a nearest-neighbour search.

use core::iter;

use aether_math::Vec3;

use crate::feature::{Curve3, SurfacePoint};
use crate::math3::hash64;

/// Quantisation grid for endpoint matching. Fine enough that two distinct
/// crossings never collide, coarse enough to absorb a last-bit difference.
const GRID: f32 = 1e-5;

pub type Cell = (i64, i64, i64);

/// Which welding cell a point falls in. Public because stroke identity
/// is asked of the same quantisation — a curve's endpoints are what
/// name it (`easel::program::sight::curve_id`), and they have to be
/// quantised the way the weld quantised them or two spellings of one
/// joint become two curves.
pub fn cell(p: Vec3) -> Cell {
    ((p.x / GRID).round() as i64, (p.y / GRID).round() as i64, (p.z / GRID).round() as i64)
}

/// Which segments touch each welding cell.
///
/// A `HashMap<Cell, Vec<u32>>` is the obvious shape and the wrong one on
/// this path: the silhouette welds twenty thousand segments *every frame*,
/// so the obvious shape means forty thousand one- or two-element `Vec`
/// allocations and a `SipHash` of twenty-four key bytes per endpoint, all of
/// it thrown away before the next eye. This is the same index with the
/// allocations counted first — one probe table over the cells, and every
/// cell's segments packed into one shared run.
///
/// Cells are entered in endpoint order and each run keeps that order, so
/// the partner `grow` finds at a junction is the one the map handed back.
/// Endpoints are numbered `segment * 2 + end` throughout, so an endpoint's
/// segment is `endpoint / 2` and its opposite is `endpoint ^ 1`.
struct Endpoints {
    /// Which cell each endpoint fell in, as a dense index. Quantising and
    /// hashing happen once, here — the join phase then asks only which
    /// cell an endpoint is in and whether two endpoints agree, both of
    /// which this answers by lookup.
    cells: Vec<u32>,
    /// Where each dense cell's segments start in `ids`, with the usual
    /// one-past-the-end sentinel.
    starts: Vec<u32>,
    ids: Vec<u32>,
}

/// The cell's bucket. Splitmix over a cheap fold of the three axes —
/// exact key comparison carries correctness, so this only has to scatter.
fn bucket(cell: Cell) -> u64 {
    hash64(
        (cell.0 as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
            ^ (cell.1 as u64).rotate_left(21)
            ^ (cell.2 as u64).rotate_left(42),
    )
}

impl Endpoints {
    fn index(segments: &[[SurfacePoint; 2]]) -> Self {
        let ends = segments.len() * 2;
        // Half full at worst — every endpoint its own cell — which keeps
        // the probe to about one step.
        let buckets = (ends * 2).next_power_of_two().max(2);
        let mut table: Vec<u32> = vec![0; buckets];
        let mut keys: Vec<Cell> = Vec::new();
        let mut counts: Vec<u32> = Vec::new();

        // Two passes so the runs can be packed: the first learns which
        // cells exist and how many endpoints each holds, the second fills
        // them in endpoint order. The probe table is scaffolding for the
        // first and is dropped with it.
        let mask = buckets - 1;
        let cells: Vec<u32> = segments
            .iter()
            .flatten()
            .map(|end| {
                let cell = cell(end.pos);
                let mut at = bucket(cell) as usize & mask;
                loop {
                    match table[at] {
                        0 => {
                            let dense = keys.len() as u32;
                            table[at] = dense + 1;
                            keys.push(cell);
                            counts.push(1);
                            return dense;
                        }
                        held if keys[held as usize - 1] == cell => {
                            counts[held as usize - 1] += 1;
                            return held - 1;
                        }
                        _ => at = (at + 1) & mask,
                    }
                }
            })
            .collect();

        let starts: Vec<u32> = counts
            .iter()
            .scan(0, |first, &count| {
                let start = *first;
                *first += count;
                Some(start)
            })
            .chain(iter::once(ends as u32))
            .collect();

        let mut ids = vec![0u32; ends];
        let mut cursor: Vec<u32> = starts[..counts.len()].to_vec();
        for (end, &at) in cells.iter().enumerate() {
            let slot = &mut cursor[at as usize];
            ids[*slot as usize] = (end / 2) as u32;
            *slot += 1;
        }

        Self { cells, starts, ids }
    }

    /// The segments touching the cell `endpoint` fell in, in the order
    /// they were entered.
    fn beside(&self, endpoint: usize) -> &[u32] {
        let dense = self.cells[endpoint] as usize;

        &self.ids[self.starts[dense] as usize..self.starts[dense + 1] as usize]
    }
}

/// Join `segments` end to end into as few polylines as possible.
///
/// Junctions where three or more segments meet — common on a
/// reconstruction, where the silhouette pinches — are resolved by taking
/// whichever partner is found first and leaving the rest to start their
/// own stroke. An illustrator lifts the pen at a junction too.
pub fn weld(segments: Vec<[SurfacePoint; 2]>) -> Vec<Vec<SurfacePoint>> {
    let index = Endpoints::index(&segments);

    let mut used = vec![false; segments.len()];
    let mut out = Vec::new();

    for start in 0..segments.len() {
        if used[start] {
            continue;
        }
        used[start] = true;

        // Grow off the far end, turn the line around, then grow off what
        // was the near end — so the tail is always a known endpoint and
        // never has to be recovered from its coordinates.
        let mut line = vec![segments[start][0], segments[start][1]];
        grow(&segments, &index, &mut used, &mut line, start * 2 + 1);
        line.reverse();
        grow(&segments, &index, &mut used, &mut line, start * 2);

        out.push(line);
    }

    out
}

/// Extend `line` forward off its tail for as long as an unused segment
/// shares that endpoint. `tail` is the endpoint the line currently ends on.
fn grow(
    segments: &[[SurfacePoint; 2]],
    index: &Endpoints,
    used: &mut [bool],
    line: &mut Vec<SurfacePoint>,
    mut tail: usize,
) {
    loop {
        let Some(next) = index.beside(tail).iter().copied().find(|&i| !used[i as usize]) else {
            return;
        };

        used[next as usize] = true;
        // Whichever of the joining segment's ends is not the one it was
        // met by. Cell identity is what "not the one" means, and the
        // dense cell index carries it exactly.
        let ends = next as usize * 2;
        let far = if index.cells[ends] == index.cells[tail] {
            ends + 1
        } else {
            ends
        };

        line.push(segments[next as usize][far & 1]);
        tail = far;
    }
}

/// Weld, then wrap each polyline as a curve carrying `template`'s class,
/// pen and seed.
pub fn curves(segments: Vec<[SurfacePoint; 2]>, template: &Curve3) -> Vec<Curve3> {
    weld(segments)
        .into_iter()
        .filter(|line| line.len() >= 2)
        .map(|points| Curve3 { points, ..template.clone() })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f32, y: f32) -> SurfacePoint {
        SurfacePoint::on_surface(Vec3::new(x, y, 0.0), Vec3::new(0.0, 0.0, 1.0))
    }

    /// Tripwire: a junction is resolved by the order the segments were
    /// handed in, and each stroke leaves with the first partner it has
    /// not already used.
    ///
    /// Four segments meeting at the origin — the shape a silhouette makes
    /// wherever it pinches, which on a reconstruction is everywhere. Any
    /// index that loses the order endpoints were entered in still welds
    /// four segments into two strokes and still passes a count-only
    /// check, while pairing the arms differently: the drawing then breaks
    /// into different strokes, and a stroke is what taper, wobble and the
    /// authored-mark coverage rule all apply to. So the arms each stroke
    /// leaves with are asserted, not just how many there are.
    #[test]
    fn a_junction_pairs_the_arms_it_was_given_first() {
        let (left, right, down, up) = (at(-1.0, 0.0), at(1.0, 0.0), at(0.0, -1.0), at(0.0, 1.0));
        let origin = at(0.0, 0.0);

        let lines = weld(vec![[left, origin], [origin, right], [down, origin], [origin, up]]);

        assert_eq!(lines.len(), 2, "four arms, two strokes: a junction lifts the pen");
        let ends: Vec<(f32, f32)> =
            lines.iter().map(|line| (line[0].pos.x + line[0].pos.y, line[2].pos.x + line[2].pos.y)).collect();
        assert_eq!(lines.iter().map(Vec::len).collect::<Vec<_>>(), vec![3, 3], "each stroke is two arms");
        assert_eq!(ends, vec![(1.0, -1.0), (1.0, -1.0)], "the first stroke takes left-right, the second down-up");
    }

    /// Tripwire: endpoints that land in the same cell weld even when their
    /// coordinates differ in the last bit.
    ///
    /// Crossings are bit-identical by construction, so this is the
    /// belt-and-braces half of that claim — the quantisation is what makes
    /// the index exact-match rather than a nearest-neighbour search, and a
    /// grid coarse enough to absorb a rounding difference is the reason it
    /// can be.
    #[test]
    fn a_last_bit_difference_still_closes_the_seam() {
        let joint = at(0.25, 0.5);
        let nudged = at(f32::from_bits(joint.pos.x.to_bits() + 1), 0.5);

        let lines = weld(vec![[at(0.0, 0.5), joint], [nudged, at(1.0, 0.5)]]);

        assert_eq!(lines.len(), 1, "one stroke: the seam closed");
        assert_eq!(lines[0].len(), 3, "three points, the shared end counted once");
    }
}
