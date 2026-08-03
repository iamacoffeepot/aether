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

pub fn hidden(mesh: &Mesh, eye: Vec3, point: &SurfacePoint) -> bool {
    let to_eye = eye - point.probe;
    let distance = to_eye.length();

    mesh.occluded(point.probe + point.normal * 2e-4, to_eye / distance, distance)
}

fn is_drawn(mesh: &Mesh, eye: Vec3, point: &SurfacePoint, class: FeatureClass, mode: Mode) -> bool {
    if !matches!(class, FeatureClass::Silhouette | FeatureClass::Decal) {
        let to_eye = (eye - point.probe).normalize();
        if point.normal.dot(to_eye) < 0.02 {
            return false;
        }
    }

    mode == Mode::Ghost || !hidden(mesh, eye, point)
}

/// Split `curve` into the runs that survive, preserving order.
pub fn runs(mesh: &Mesh, eye: Vec3, curve: &Curve3, keep: &dyn Fn(&SurfacePoint) -> bool, mode: Mode) -> Vec<Curve3> {
    let flags: Vec<bool> = curve.points.iter().map(|p| keep(p) && is_drawn(mesh, eye, p, curve.class, mode)).collect();

    if flags.iter().all(|&f| f) {
        return vec![curve.clone()];
    }

    let mut segments: Vec<Vec<SurfacePoint>> = Vec::new();
    let mut current: Vec<SurfacePoint> = Vec::new();
    for (point, &on) in curve.points.iter().zip(&flags) {
        if on {
            current.push(*point);
        } else if !current.is_empty() {
            segments.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }

    segments.into_iter().filter(|s| s.len() >= 2).map(|points| Curve3 { points, ..curve.clone() }).collect()
}
