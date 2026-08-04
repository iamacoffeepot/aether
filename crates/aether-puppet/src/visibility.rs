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

/// The half of the verdict that costs no ray: the point is on the near
/// side of the surface. A silhouette grazes by definition and a decal is
/// planted on a fitted plane, so neither is asked — which is a property of
/// the curve, so [`drawn`] asks it once per curve rather than per point.
fn faces_eye(eye: Vec3, point: &SurfacePoint) -> bool {
    point.normal.dot((eye - point.probe).normalize()) >= 0.02
}

/// Which points of `span` the eye can actually see, written one verdict per
/// point into `seen`.
///
/// A ray every `stride` points, and the verdict held in between — except
/// across a window whose two ends disagree, where every skipped point is
/// cast after all. The held verdict is right in the interior of a run and
/// wrong only near its ends, which is exactly where being wrong shows:
/// holding it there moves the end of a stroke up to `stride - 1` points
/// off the edge the stroke actually disappears behind.
///
/// One pass, carrying the previous sample in a pair of locals and writing
/// into the caller's flags. The window it is closing is always the one
/// behind the sample just cast, so the sample list it used to collect
/// first was a frame's worth of allocation for two values at a time.
fn visible(mesh: &Mesh, eye: Vec3, span: &[SurfacePoint], stride: usize, bias: f32, seen: &mut [bool]) {
    // The last point is sampled whether or not it lands on the grid, so
    // the final window has an end to be refined against like any other.
    let last = span.len() - 1;
    let mut previous = (0, !hidden(mesh, eye, &span[0], bias));
    seen[0] = previous.1;
    if last == 0 {
        return;
    }

    let mut at = stride;
    loop {
        let sample = at.min(last);
        let verdict = !hidden(mesh, eye, &span[sample], bias);
        seen[sample] = verdict;

        let (from, before) = previous;
        if before == verdict {
            seen[from..sample].fill(before);
        } else {
            for (point, seen) in span[from + 1..sample].iter().zip(&mut seen[from + 1..sample]) {
                *seen = !hidden(mesh, eye, point, bias);
            }
        }

        if sample == last {
            return;
        }
        previous = (sample, verdict);
        at += stride;
    }
}

/// Which points of `curve` are drawn at all, one flag per point.
///
/// The rayless tests run per point — `keep` is a tone test with no ray
/// behind it, so sampling it would band the hatch thresholds, and the
/// facing test is a dot product. Only the occlusion ray is strided, and
/// only inside a stretch that has already passed both, so nothing is cast
/// for a point that is dropped either way.
fn drawn<K: Fn(&SurfacePoint) -> bool + ?Sized>(
    mesh: &Mesh,
    eye: Vec3,
    curve: &Curve3,
    keep: &K,
    mode: Mode,
    stride: usize,
    bias: f32,
) -> Vec<bool> {
    let grazes = matches!(curve.class, FeatureClass::Silhouette | FeatureClass::Decal);
    let mut flags: Vec<bool> = curve.points.iter().map(|p| keep(p) && (grazes || faces_eye(eye, p))).collect();
    if mode == Mode::Ghost {
        return flags;
    }

    let mut from = 0;
    while from < flags.len() {
        if !flags[from] {
            from += 1;
            continue;
        }
        let to = flags[from..].iter().position(|&on| !on).map_or(flags.len(), |offset| from + offset);
        visible(mesh, eye, &curve.points[from..to], stride, bias, &mut flags[from..to]);
        from = to;
    }

    flags
}

/// Split `curve` into the runs that survive, preserving order.
///
/// `bias` lifts each occlusion ray off the surface and is the caller's to
/// supply, because it belongs to the mesh the curve was extracted from
/// rather than to `mesh`, the one being cast against. The two are the same
/// for hatch and crease and differ for a silhouette solved coarse.
/// `keep` is taken by generic reference rather than as a `&dyn Fn` so the
/// common predicate — the caller that keeps every point, tone having been
/// gated once at load — compiles to nothing. As a trait object it was an
/// indirect call per point of the drawing, two hundred thousand of them a
/// frame, to be told `true`.
pub fn runs<K: Fn(&SurfacePoint) -> bool + ?Sized>(
    mesh: &Mesh,
    eye: Vec3,
    curve: &Curve3,
    keep: &K,
    mode: Mode,
    stride: usize,
    bias: f32,
) -> Vec<Curve3> {
    let flags = drawn(mesh, eye, curve, keep, mode, stride.max(1), bias);

    // Both ends of the range are worth naming, and one count answers
    // both. A curve wholly on the far side of the figure is the common
    // case at any eye — half the hatching is behind her — and it has
    // nothing to split, so it should not be walked again to discover
    // that.
    let seen = flags.iter().filter(|&&on| on).count();
    if seen == flags.len() {
        return vec![curve.clone()];
    }
    if seen == 0 {
        return Vec::new();
    }

    // Straight into the surviving curves: the run of points and the curve
    // that carries it are built together, so a drawing's worth of points
    // is moved once rather than gathered into a list of runs and moved
    // again.
    //
    // Each survivor spells its fields out rather than filling them from
    // `..curve.clone()`. The struct-update form reads better and costs a
    // full copy of the source curve's points per run — allocated, memcpy'd
    // and dropped unread, because `points` overwrites it — which on a
    // drawing split into a thousand runs a frame is most of the split.
    let mut survivors: Vec<Curve3> = Vec::new();
    let mut current: Vec<SurfacePoint> = Vec::new();
    let close = |current: &mut Vec<SurfacePoint>, survivors: &mut Vec<Curve3>| {
        if current.len() >= 2 {
            survivors.push(Curve3 {
                points: mem::take(current),
                class: curve.class,
                pen: curve.pen,
                seed: curve.seed,
                authored: curve.authored,
            });
        } else {
            current.clear();
        }
    };
    for (point, &on) in curve.points.iter().zip(&flags) {
        if on {
            current.push(*point);
        } else if !current.is_empty() {
            close(&mut current, &mut survivors);
        }
    }
    close(&mut current, &mut survivors);

    whole_or_nothing(curve, survivors)
}

/// How much of an authored mark has to survive for any of it to be drawn.
///
/// The ink pass compares the field's own coverage plane against this
/// same figure in its vertex stage (ADR-0172), so the rule has one
/// number wherever it is applied.
pub const CHART_COVERAGE: f32 = 0.35;

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

    /// A wall standing one unit in front of the drawing over `x < 0`, so a
    /// line laid across `x = 0` is hidden on one side of it and clear on
    /// the other with the edge between two known points.
    fn wall() -> Mesh {
        let obj = b"v -10 -10 1\nv 0 -10 1\nv 0 10 1\nv -10 10 1\nf 1 2 3\nf 1 3 4\n";

        Mesh::from_obj_bytes(obj, 0).expect("two triangles are a mesh")
    }

    /// Twenty-one points stepping across the wall's edge. Points 0..=9 are
    /// behind it and 10..=20 clear it, so a correct split hands back one
    /// run of eleven.
    fn across_the_edge() -> Curve3 {
        let points = (0..21)
            .map(|at| SurfacePoint::on_surface(Vec3::new(-0.95 + 0.1 * at as f32, 0.0, 0.0), Vec3::new(0.0, 0.0, 1.0)))
            .collect();

        Curve3 { points, class: FeatureClass::Silhouette, pen: Pen::Ink, seed: 0, authored: false }
    }

    fn curve(points: usize, authored: bool) -> Curve3 {
        Curve3 {
            points: vec![SurfacePoint::on_surface(Vec3::splat(0.0), Vec3::new(0.0, 0.0, 1.0)); points],
            class: FeatureClass::Decal,
            pen: Pen::Ink,
            seed: 0,
            authored,
        }
    }

    /// Tripwire: a stroke ends on the edge it disappears behind, whatever
    /// the sampling stride.
    ///
    /// The strided verdict is held between samples, and no stride but 1
    /// puts a sample on point 10 — so without the refinement the run
    /// starts at the next sample instead and the drawing loses the points
    /// between, which is the chopped stroke end this exists to catch.
    ///
    /// Three strides because the sampling has three shapes: 1 skips
    /// nothing, 3 leaves the curve's last point off the sample grid, and 4
    /// lands it on the grid — and the last point is sampled either way, so
    /// a stride that divides the curve must not sample it twice.
    #[test]
    fn a_run_ends_on_the_occluding_edge_whatever_the_stride() {
        let (wall, curve, eye) = (wall(), across_the_edge(), Vec3::new(0.0, 0.0, 10.0));

        for stride in [1, 3, 4] {
            let split = runs(&wall, eye, &curve, &|_| true, Mode::Opaque, stride, 1e-4);

            assert_eq!(split.len(), 1, "stride {stride}: one clear stretch, so one run");
            assert_eq!(split[0].points.len(), 11, "stride {stride}: the run starts where the wall ends");
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
