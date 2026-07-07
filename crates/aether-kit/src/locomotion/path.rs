//! Click-to-move pathfinding for the [`Locomotion`](super::Locomotion) actor:
//! 8-connected A* over the walkable tile grid, a line-of-sight string-puller
//! that collapses the tile path to the fewest octimeter waypoints, and the
//! per-tick step helpers that glide the mover along it. All integer-only and
//! grid-bounded, so the whole path stays deterministic.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, VecDeque};

use super::{GRID_TILES, TileMap, tile_center_octimeters};
use crate::{OCTIMETERS_PER_TILE, TILE_BITS};

/// 8-connected A* on the walkable tile grid. Returns the waypoint tiles
/// from just past `start` through `goal`, or `None` if unreachable.
/// Iterative and bounded by the grid — never recursive.
pub(super) fn astar(
    map: &TileMap,
    start: (i32, i32),
    goal: (i32, i32),
) -> Option<VecDeque<(i32, i32)>> {
    if !map.walkable(goal.0, goal.1) {
        return None;
    }
    // Octile distance: cardinal = 10, diagonal = 14 (≈ 10·√2). Used as both
    // step cost and heuristic so a straight run is strictly cheaper than an
    // equal-length diagonal zigzag — paths hug the direct route.
    let octile = |a: (i32, i32), b: (i32, i32)| {
        let dx = (a.0 - b.0).abs();
        let dy = (a.1 - b.1).abs();
        let lo = dx.min(dy);
        14 * lo + 10 * (dx.max(dy) - lo)
    };
    let mut g_score = [i32::MAX; GRID_TILES];
    let mut came_from: [Option<(i32, i32)>; GRID_TILES] = [None; GRID_TILES];
    let mut open = BinaryHeap::new();
    g_score[TileMap::idx(start.0, start.1)] = 0;
    // Heap key (f, h, tile): ties in f break toward the smaller h (closer to
    // the goal), which keeps the path from drifting off the straight line.
    let h0 = octile(start, goal);
    open.push(Reverse((h0, h0, start)));
    while let Some(Reverse((_, _, cur))) = open.pop() {
        if cur == goal {
            let mut path = VecDeque::new();
            let mut node = goal;
            while node != start {
                path.push_front(node);
                node = came_from[TileMap::idx(node.0, node.1)]?;
            }
            return Some(path);
        }
        let cur_g = g_score[TileMap::idx(cur.0, cur.1)];
        for dz in -1..=1 {
            for dx in -1..=1 {
                if dx == 0 && dz == 0 {
                    continue;
                }
                let nb = (cur.0 + dx, cur.1 + dz);
                if !map.walkable(nb.0, nb.1) {
                    continue;
                }
                let step = if dx != 0 && dz != 0 { 14 } else { 10 };
                let nb_g = cur_g + step;
                let i = TileMap::idx(nb.0, nb.1);
                if nb_g < g_score[i] {
                    g_score[i] = nb_g;
                    came_from[i] = Some(cur);
                    let h = octile(nb, goal);
                    open.push(Reverse((nb_g + h, h, nb)));
                }
            }
        }
    }
    None
}

/// Smooth the A* tile path into the fewest octimeter waypoints that still
/// clear every wall — string-pulling by line of sight. The candidate points
/// are the actual sub-tile `start` (the mover's position), each interior
/// tile's center, and the snapped sub-tile `dest`. Walking them, each one is
/// dropped whenever the next is directly visible from the current anchor, so
/// a stretch with nothing in the way collapses to a single straight segment
/// and a corner survives only where a wall genuinely sits between the anchor
/// and the point past it. Anchoring on the real start/dest (not tile centers)
/// is what keeps an off-center straight line from kinking through a center.
pub(super) fn smooth_path(
    map: &TileMap,
    start: (i32, i32),
    tiles: &VecDeque<(i32, i32)>,
    dest: (i32, i32),
) -> VecDeque<(i32, i32)> {
    // Candidates: start, every tile center *before* the goal tile, then dest
    // (which replaces the goal tile's center — it sits inside that tile).
    let mut pts = Vec::with_capacity(tiles.len() + 1);
    pts.push(start);
    let interior = tiles.len().saturating_sub(1);
    pts.extend(
        tiles
            .iter()
            .take(interior)
            .map(|&(tx, tz)| (tile_center_octimeters(tx), tile_center_octimeters(tz))),
    );
    pts.push(dest);

    let mut path = VecDeque::new();
    let mut anchor = pts[0];
    for i in 1..pts.len() - 1 {
        // Keep pts[i] only when the point past it is occluded from the anchor —
        // then it's a real corner. Otherwise the anchor can see straight past
        // it, so drop it.
        if !los(map, anchor, pts[i + 1]) {
            path.push_back(pts[i]);
            anchor = pts[i];
        }
    }
    path.push_back(dest);
    path
}

/// Whether the straight segment between two octimeter points crosses only
/// walkable tiles — an integer grid traversal (Amanatides–Woo) over the
/// 1-tile interaction grid. Steps from boundary to boundary, comparing the
/// two axes' distances by cross-multiplication so it stays integer-only and
/// deterministic. Diagonal corner crossings are allowed (only the entered
/// tile is checked), matching `astar`'s 8-connected moves.
pub(super) fn los(map: &TileMap, a: (i32, i32), b: (i32, i32)) -> bool {
    let (mut x, mut z) = (a.0 >> TILE_BITS, a.1 >> TILE_BITS);
    let (xe, ze) = (b.0 >> TILE_BITS, b.1 >> TILE_BITS);
    if !map.walkable(x, z) {
        return false;
    }
    let (step_x, step_z) = ((b.0 - a.0).signum(), (b.1 - a.1).signum());
    let adx = i64::from((b.0 - a.0).abs());
    let adz = i64::from((b.1 - a.1).abs());
    // Octimeters from the start point to the next tile boundary on each axis;
    // each crossing then advances that axis's accumulator by one whole tile.
    let mut cx = match step_x {
        1 => i64::from(((x + 1) << TILE_BITS) - a.0),
        -1 => i64::from(a.0 - (x << TILE_BITS)),
        _ => 0,
    };
    let mut cz = match step_z {
        1 => i64::from(((z + 1) << TILE_BITS) - a.1),
        -1 => i64::from(a.1 - (z << TILE_BITS)),
        _ => 0,
    };
    let tile = i64::from(OCTIMETERS_PER_TILE);
    while x != xe || z != ze {
        // Step the axis whose next boundary is nearer (t = c / ad, compared as
        // cx·adz vs cz·adx); on an exact tie cross the corner diagonally. An
        // axis already at its end never steps.
        let (take_x, take_z) = if x == xe {
            (false, true)
        } else if z == ze {
            (true, false)
        } else {
            match (cx * adz).cmp(&(cz * adx)) {
                Ordering::Less => (true, false),
                Ordering::Greater => (false, true),
                Ordering::Equal => (true, true),
            }
        };
        if take_x {
            x += step_x;
            cx += tile;
        }
        if take_z {
            z += step_z;
            cz += tile;
        }
        if !map.walkable(x, z) {
            return false;
        }
    }
    true
}

/// Advance a point `speed` octimeters *along the straight line to* `target` —
/// the same Euclidean distance per tick in every direction (so a diagonal
/// doesn't run √2 faster than a cardinal). Each axis moves its share of the
/// step scaled by the true direction `(dx, dz) / |(dx, dz)|`, rounded to the
/// nearest octimeter, and the move snaps exactly onto `target` once within one
/// step. Integer-only via `isqrt` and recomputed from the live delta each
/// tick, so it stays deterministic and rounding never accumulates.
#[allow(clippy::cast_possible_truncation)]
pub(super) fn step_toward(cur: (i32, i32), target: (i32, i32), speed: i32) -> (i32, i32) {
    let dx = i64::from(target.0 - cur.0);
    let dz = i64::from(target.1 - cur.1);
    let dist = (dx * dx + dz * dz).isqrt();
    let speed = i64::from(speed);
    if dist <= speed {
        return target;
    }
    // Round speed·d / dist to nearest, away from zero on a tie.
    let round_div = |num: i64| {
        let half = dist / 2;
        if num >= 0 {
            (num + half) / dist
        } else {
            (num - half) / dist
        }
    };
    // |speed·d / dist| ≤ speed, so the result fits an i32 axis step.
    (
        cur.0 + round_div(speed * dx) as i32,
        cur.1 + round_div(speed * dz) as i32,
    )
}

/// Move `cur` toward `target` by at most `step` octimeters, never
/// overshooting.
pub(super) fn approach(cur: i32, target: i32, step: i32) -> i32 {
    match cur.cmp(&target) {
        Ordering::Less => (cur + step).min(target),
        Ordering::Greater => (cur - step).max(target),
        Ordering::Equal => cur,
    }
}
