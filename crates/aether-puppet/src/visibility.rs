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

    segments.into_iter().filter(|s| s.len() >= 2).map(|points| Curve3 { points, ..curve.clone() }).collect()
}
