//! Stage two: which parts of each curve survive.
//!
//! Two independent reasons a point is not drawn, kept apart because only
//! one of them can be ghosted:
//!
//! - **Back-facing.** The point is on the far side of the surface. Applies
//!   to hatching only — a silhouette grazes by definition and would fail
//!   any such test.
//! - **Occluded.** The point is real and front-facing, but something else
//!   stands between it and the eye.

use core::mem;

use aether_math::Vec3;

use crate::feature::{Curve3, FeatureClass, SurfacePoint};
use crate::mesh::Mesh;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Hidden runs are dropped.
    Opaque,
    /// Hidden runs are kept and styled as ghosts — the x-ray look.
    Ghost,
}

/// How far a ray is lifted off the surface before the occlusion question is
/// asked.
///
/// It belongs to whichever mesh the *point* came from, not to the one being
/// cast against. A silhouette solved on the coarse mesh sits up to a coarse
/// cell away from the fine surface it is then tested against, so the fine
/// mesh's own bias leaves it inside its own subject: every outline comes
/// back chopped into dashes by self-occlusion, which reads as a rendering
/// bug rather than a decimation one.
pub fn hidden(mesh: &Mesh, eye: Vec3, point: &SurfacePoint, bias: f32) -> bool {
    let to_eye = eye - point.probe;
    let distance = to_eye.length();

    mesh.occluded(point.probe + point.normal * bias, to_eye / distance, distance)
}

fn is_drawn(mesh: &Mesh, eye: Vec3, point: &SurfacePoint, class: FeatureClass, mode: Mode, bias: f32) -> bool {
    if !matches!(class, FeatureClass::Silhouette | FeatureClass::Decal) {
        let to_eye = (eye - point.probe).normalize();
        if point.normal.dot(to_eye) < 0.02 {
            return false;
        }
    }

    mode == Mode::Ghost || !hidden(mesh, eye, point, bias)
}

/// Split `curve` into the runs that survive, preserving order.
///
/// `bias` lifts each occlusion ray off the surface and is the caller's to
/// supply, because it belongs to the mesh the curve was extracted from
/// rather than to `mesh`, the one being cast against. The two are the same
/// for hatch and crease and differ for a silhouette solved coarse.
pub fn runs(
    mesh: &Mesh,
    eye: Vec3,
    curve: &Curve3,
    keep: &dyn Fn(&SurfacePoint) -> bool,
    mode: Mode,
    stride: usize,
    bias: f32,
) -> Vec<Curve3> {
    // The occlusion verdict is sampled every `stride` points and held in
    // between. `keep` is not — it is a per-point tone test with no ray
    // behind it, so sampling it would band the hatch thresholds.
    let stride = stride.max(1);
    let mut sampled = true;
    let flags: Vec<bool> = curve
        .points
        .iter()
        .enumerate()
        .map(|(index, p)| {
            if index % stride == 0 {
                sampled = is_drawn(mesh, eye, p, curve.class, mode, bias);
            }
            keep(p) && sampled
        })
        .collect();

    if flags.iter().all(|&f| f) {
        return vec![curve.clone()];
    }

    let mut segments: Vec<Vec<SurfacePoint>> = Vec::new();
    let mut current: Vec<SurfacePoint> = Vec::new();
    for (point, &on) in curve.points.iter().zip(&flags) {
        if on {
            current.push(*point);
        } else if !current.is_empty() {
            segments.push(mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }

    let survivors: Vec<Curve3> =
        segments.into_iter().filter(|s| s.len() >= 2).map(|points| Curve3 { points, ..curve.clone() }).collect();

    whole_or_nothing(curve, survivors)
}

/// How much of an authored mark has to survive for any of it to be drawn.
const CHART_COVERAGE: f32 = 0.35;

/// An authored mark goes whole or not at all.
///
/// Shortened is fine — a lid running behind a fringe should end where the
/// hair starts — but shattered is not: past forty degrees the far eye
/// survives only as fragments between hair strands, and four disconnected
/// crumbs where an eye should be read as dirt on the paper rather than as a
/// feature partly hidden.
///
/// Applied inside `runs` rather than left to the caller, because the rule
/// needs the original curve's length and that is the one thing a caller
/// holding only the survivors has lost.
fn whole_or_nothing(curve: &Curve3, survivors: Vec<Curve3>) -> Vec<Curve3> {
    if !curve.authored {
        return survivors;
    }

    let kept: usize = survivors.iter().map(|run| run.points.len()).sum();
    if (kept as f32) < curve.points.len() as f32 * CHART_COVERAGE {
        return Vec::new();
    }

    survivors
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::feature::Pen;

    fn curve(points: usize, authored: bool) -> Curve3 {
        Curve3 {
            points: vec![SurfacePoint::on_surface(Vec3::splat(0.0), Vec3::new(0.0, 0.0, 1.0)); points],
            class: FeatureClass::Decal,
            pen: Pen::Ink,
            seed: 0,
            authored,
        }
    }

    /// `curve` cut into `runs` pieces of `each` points — what the split
    /// hands back when a mark crosses something that hides parts of it.
    fn split(of: &Curve3, runs: usize, each: usize) -> Vec<Curve3> {
        (0..runs).map(|_| Curve3 { points: of.points[..each].to_vec(), ..of.clone() }).collect()
    }

    /// Tripwire: an authored mark is allowed to be shortened and not to be
    /// shattered, and an extracted one is allowed both.
    ///
    /// Three cases because the rule has three edges and dropping any one of
    /// them is silent. Shattered-and-dropped is the failure it exists for —
    /// past forty degrees the far eye survives only as crumbs between hair
    /// strands, and four disconnected specks read as dirt on the paper.
    /// Shortened-and-kept rules out a floor so eager that a lid running
    /// behind a fringe vanishes whole, which looks exactly like the eye
    /// never being drawn. And a hatch line crossing behind an ear *should*
    /// arrive as two runs, so the rule must not reach it.
    #[test]
    fn an_authored_mark_survives_shortening_but_not_shattering() {
        let mark = curve(100, true);
        let shattered = split(&mark, 3, 8);
        let shortened = split(&mark, 1, 60);

        assert!(whole_or_nothing(&mark, shattered.clone()).is_empty(), "24 of 100 points is dirt, not an eye");
        assert_eq!(whole_or_nothing(&mark, shortened).len(), 1, "60 of 100 points is a mark ending at the hair");

        let hatch = curve(100, false);
        assert_eq!(whole_or_nothing(&hatch, shattered).len(), 3, "an extracted curve is happy to be cut up");
    }
}
