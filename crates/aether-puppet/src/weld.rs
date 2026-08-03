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

use std::collections::HashMap;

use aether_math::Vec3;

use crate::feature::{Curve3, SurfacePoint};

/// Quantisation grid for endpoint matching. Fine enough that two distinct
/// crossings never collide, coarse enough to absorb a last-bit difference.
const GRID: f32 = 1e-5;

fn cell(p: Vec3) -> (i64, i64, i64) {
    ((p.x / GRID).round() as i64, (p.y / GRID).round() as i64, (p.z / GRID).round() as i64)
}

/// Join `segments` end to end into as few polylines as possible.
///
/// Junctions where three or more segments meet — common on a
/// reconstruction, where the silhouette pinches — are resolved by taking
/// whichever partner is found first and leaving the rest to start their
/// own stroke. An illustrator lifts the pen at a junction too.
pub fn weld(segments: Vec<[SurfacePoint; 2]>) -> Vec<Vec<SurfacePoint>> {
    let mut index: HashMap<(i64, i64, i64), Vec<u32>> = HashMap::with_capacity(segments.len() * 2);
    for (i, segment) in segments.iter().enumerate() {
        for end in segment {
            index.entry(cell(end.pos)).or_default().push(i as u32);
        }
    }

    let mut used = vec![false; segments.len()];
    let mut out = Vec::new();

    for start in 0..segments.len() {
        if used[start] {
            continue;
        }
        used[start] = true;

        let mut line = vec![segments[start][0], segments[start][1]];
        grow(&segments, &index, &mut used, &mut line);
        line.reverse();
        grow(&segments, &index, &mut used, &mut line);

        out.push(line);
    }

    out
}

/// Extend `line` forward off its tail for as long as an unused segment
/// shares that endpoint.
fn grow(
    segments: &[[SurfacePoint; 2]],
    index: &HashMap<(i64, i64, i64), Vec<u32>>,
    used: &mut [bool],
    line: &mut Vec<SurfacePoint>,
) {
    loop {
        let tail = *line.last().expect("non-empty line");
        let Some(candidates) = index.get(&cell(tail.pos)) else {
            return;
        };

        let next = candidates.iter().copied().find(|&i| !used[i as usize]);
        let Some(next) = next else {
            return;
        };

        used[next as usize] = true;
        let segment = segments[next as usize];
        let far = if cell(segment[0].pos) == cell(tail.pos) {
            segment[1]
        } else {
            segment[0]
        };
        line.push(far);
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
