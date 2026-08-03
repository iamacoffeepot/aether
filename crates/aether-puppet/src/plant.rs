//! Dropping a chart mark onto the face.
//!
//! A mark is authored flat, in the model's frontal plane, and has to end up
//! on a surface. How it gets there is the difference between a face and a
//! mess.
//!
//! **Not per point.** One ray per point makes the line follow every strand
//! it crosses, so a brow drawn over the fringe steps in and out of the hair
//! and stops reading as one stroke.
//!
//! **Not one ray either.** A single centre ray fixes that and buys a
//! different problem: the mark becomes a flat plate, and a plate has to
//! stand off the surface by more than its own relief or its ends sink into
//! the face. Across an eye that relief is 0.05, and a plate floating 0.05
//! proud of a socket slides most of an eye-width across it by the time she
//! is three-quarters on.
//!
//! **A trimmed least-squares plane is the middle term.** It carries the
//! surface's tilt, so the standoff only has to clear whatever the fit
//! rejected — for a brow that is the fringe, and for an eye it is nothing
//! much. Trimmed, because the fringe is precisely the outlier a brow must
//! ignore.
//!
//! And the pen and the probe part company here. The ink goes on the plane;
//! whether it can be seen is asked of the skin directly underneath. Without
//! that split a mark sitting fractionally inside the head shatters into
//! shards as she turns, because the occlusion question about a point inside
//! a head comes back yes, in patches.

use aether_math::{Vec2, Vec3};

use crate::chart::Mark;
use crate::feature::SurfacePoint;
use crate::mesh::{Crossing, Mesh};

use core::mem;

/// Steepest plane a mark is allowed to be planted on.
///
/// A face is not a cliff. Past this the fit has found a direction the mark
/// barely spans and is reporting the noise in it as a slope.
const MAX_SLOPE: f32 = 2.0;

/// How many trimmed hits a plane fit needs before it is worth solving. Under
/// this the median plane is the better answer whatever the moments say.
const MIN_HITS: usize = 6;

/// Straight into the face. Every planting ray runs along it, so one plane
/// per mark means one frontal direction for the whole chart — and the
/// chart's own turn gate probes the surface along the same line, so the two
/// cannot disagree about which way is into her.
pub const INTO: Vec3 = Vec3::new(0.0, 0.0, -1.0);

/// Plant a mark, splitting it wherever there is no surface under it.
///
/// A point with no surface beneath it at all is not drawn. A decal on a
/// face needs a face: past the silhouette the lash flick has nothing to be
/// a mark *on*, and drawing it anyway sends a stroke off into the
/// background — which is what the flick was doing at three-quarters, on the
/// far eye, where it clears her cheek entirely.
pub fn mark(mesh: &Mesh, mark: &Mark, front: f32) -> Vec<Vec<SurfacePoint>> {
    let cast = cast(mesh, &mark.points, front);
    let Some(plane) = plane_under(&cast) else {
        return Vec::new();
    };

    let mut runs = Vec::new();
    let mut current: Vec<SurfacePoint> = Vec::new();
    for ((p, hit), &weight) in mark.points.iter().zip(&cast).zip(&mark.weights) {
        match hit {
            Some(surface) => {
                current.push(SurfacePoint {
                    pos: on(plane, *p, mark.standoff),
                    normal: surface.normal,
                    probe: surface.pos,
                    weight,
                });
            }
            None if current.len() >= 2 => runs.push(mem::take(&mut current)),
            None => current.clear(),
        }
    }
    if current.len() >= 2 {
        runs.push(current);
    }

    runs
}

/// Chart points planted the same way marks are — same rays, same trimmed
/// plane, same standoff — returned per point rather than split into runs.
///
/// What a paint layer needs: it clips to a closed outline, and an outline
/// broken into runs is not closed. `None` only when nothing exists under
/// the whole set, so a fully hidden eye plants nothing and its paint
/// correctly vanishes with its ink.
pub fn points(mesh: &Mesh, points: &[Vec2], front: f32, standoff: f32) -> Option<Vec<Vec3>> {
    let plane = plane_under(&cast(mesh, points, front))?;

    Some(points.iter().map(|&p| on(plane, p, standoff)).collect())
}

fn cast(mesh: &Mesh, points: &[Vec2], front: f32) -> Vec<Option<Crossing>> {
    points.iter().map(|p| mesh.hit(Vec3::new(p.x, p.y, front), INTO)).collect()
}

/// A chart point lifted onto the fitted plane, `standoff` proud of it.
fn on(plane: Vec3, p: Vec2, standoff: f32) -> Vec3 {
    Vec3::new(p.x, p.y, plane.x * p.x + plane.y * p.y + plane.z + standoff)
}

/// The trimmed plane a mark rests on, as `z = ax + by + c`.
///
/// The trim is a median-absolute-deviation window, which is what makes it
/// survive the fringe: a mean and a standard deviation are both dragged by
/// the very outliers being rejected, and a brow crosses enough hair to drag
/// them a long way.
fn plane_under(cast: &[Option<Crossing>]) -> Option<Vec3> {
    let hits: Vec<Vec3> = cast.iter().flatten().map(|c| c.pos).collect();

    let mut depths: Vec<f32> = hits.iter().map(|h| h.z).collect();
    if depths.is_empty() {
        return None;
    }
    depths.sort_unstable_by(f32::total_cmp);
    let median = depths[depths.len() / 2];
    let mut spread: Vec<f32> = depths.iter().map(|d| (d - median).abs()).collect();
    spread.sort_unstable_by(f32::total_cmp);
    let tolerance = spread[spread.len() / 2].max(1e-4) * 3.0;

    let kept: Vec<Vec3> = hits.into_iter().filter(|h| (h.z - median).abs() <= tolerance).collect();

    // A flat plane at the median is the fallback, and it is the right one:
    // it is what the single-centre-ray plate would have been, which is a
    // worse mark but never a wrong one.
    Some(fit(&kept).unwrap_or_else(|| Vec3::new(0.0, 0.0, median)))
}

/// Least squares `z = ax + by + c`, returned as `(a, b, c)`.
///
/// Solved on centred moments rather than raw ones, and refused when the
/// points do not span both directions. A mouth dash or a lower-lid dash is
/// a short nearly-horizontal run, so its points share a `y` to within
/// rounding — fit a full plane through that and the `b` term is dividing by
/// the noise. The result is a plane standing almost on edge, which lays the
/// mark down as a stroke shooting off across the whole face.
fn fit(points: &[Vec3]) -> Option<Vec3> {
    if points.len() < MIN_HITS {
        return None;
    }

    let n = points.len() as f32;
    let mean = points.iter().fold(Vec3::splat(0.0), |a, &p| a + p) / n;
    let (mut xx, mut yy, mut xy, mut xz, mut yz) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for p in points {
        let (dx, dy, dz) = (p.x - mean.x, p.y - mean.y, p.z - mean.z);
        xx += dx * dx;
        yy += dy * dy;
        xy += dx * dy;
        xz += dx * dz;
        yz += dy * dz;
    }

    // Scaled against the spread itself, not an absolute epsilon: a mark a
    // few hundredths of a unit across has moments down in the 1e-6 range, so
    // any fixed floor waves through systems that are singular in every way
    // that matters here.
    let determinant = xx * yy - xy.powi(2);
    if determinant <= xx * yy * 1e-3 {
        return None;
    }

    let (a, b) = ((xz * yy - yz * xy) / determinant, (yz * xx - xz * xy) / determinant);
    if a.abs() > MAX_SLOPE || b.abs() > MAX_SLOPE {
        return None;
    }

    Some(Vec3::new(a, b, mean.z - a * mean.x - b * mean.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Depth of the skin a mark is meant to rest on, and of the fringe
    /// standing proud of it over the mark's outer end.
    const SKIN: f32 = 0.50;
    const FRINGE: f32 = 0.62;

    /// The surface hits under one mark: a lattice spanning the mark in both
    /// directions, with `fringe` putting hair over its outer third — a brow
    /// crossing her hair, which is the case the trim exists for.
    fn hits(fringe: bool) -> Vec<Vec3> {
        (0..6)
            .flat_map(|column| {
                (0..4).map(move |row| {
                    let (x, y) = (-0.1 + 0.04 * column as f32, -0.05 + 0.0333 * row as f32);
                    let z = if fringe && x > 0.03 {
                        FRINGE
                    } else {
                        SKIN
                    };

                    Vec3::new(x, y, z)
                })
            })
            .collect()
    }

    /// The same hits collapsed onto one row — a lower-lid dash, whose
    /// points share a `y` to within rounding.
    fn dash() -> Vec<Vec3> {
        hits(true).into_iter().map(|p| Vec3::new(p.x, 0.0, p.z)).collect()
    }

    /// Where the fitted plane puts the mark's outer end.
    fn at_outer(plane: Vec3) -> f32 {
        plane.x * 0.1 + plane.z
    }

    /// Tripwire: a degenerate run refuses the plane fit, so it lies down
    /// flat instead of standing on edge.
    ///
    /// A lower-lid dash spans the mark in `x` and nothing in `y`, so the
    /// `yy` moment is pure rounding noise and the `b` term divides by it.
    /// Without the conditioning check the fit comes back a plane standing
    /// almost vertical and plants the dash as a stroke shooting across the
    /// whole face — a failure with no error behind it. Both halves matter:
    /// the second rules out a guard so strict that no mark is ever fitted,
    /// which looks identical to the feature working and is not.
    #[test]
    fn a_degenerate_run_refuses_the_plane_fit() {
        assert!(fit(&dash()).is_none(), "a run with no y extent cannot support a full plane");
        assert!(fit(&hits(true)).is_some(), "the same hits spanning both directions can");
    }

    /// Tripwire: the trim rejects the fringe rather than tilting toward it.
    ///
    /// A third of a brow's rays land on hair. A plain least-squares fit is
    /// dragged by exactly those, so the plane tips and the outer end of the
    /// brow floats off the forehead by more than the standoff it was given
    /// — which is why the fit is trimmed, and why the trim is a
    /// median-absolute-deviation window rather than a standard deviation, a
    /// statistic the same outliers would drag. Both numbers come out of the
    /// hit geometry, so the pair drifts if either the trim or the fit
    /// changes.
    #[test]
    fn the_trimmed_plane_ignores_what_stands_proud_of_the_skin() {
        let cast: Vec<Option<Crossing>> = hits(true)
            .into_iter()
            .map(|pos| Some(Crossing { pos, normal: Vec3::new(0.0, 0.0, 1.0), strength: 0.0 }))
            .collect();

        let untrimmed = at_outer(fit(&hits(true)).expect("the lattice spans both directions"));
        let trimmed = at_outer(plane_under(&cast).expect("hits exist, so a plane exists"));

        assert!(untrimmed > SKIN + 0.04, "an untrimmed fit is dragged toward the fringe; got {untrimmed}");
        assert!((trimmed - SKIN).abs() < 0.01, "the trimmed plane rests on the skin at {SKIN}, got {trimmed}");
    }

    /// Tripwire: a mark whose rays all miss the subject plants nothing.
    ///
    /// A decal on a face needs a face. Past the silhouette the lash flick
    /// has nothing to be a mark *on*, and a plane fitted through no hits at
    /// all would put it wherever the fallback happened to land — in front
    /// of the background, as a stroke floating off her cheek.
    #[test]
    fn a_mark_over_nothing_plants_nothing() {
        assert!(plane_under(&[None, None, None]).is_none(), "no surface under a mark is no plane");
    }
}
