//! Mesh a typed AST into a triangle list.
//!
//! Full v1 vocabulary per ADR-0026 + ADR-0051: primitives `box`,
//! `cylinder`, `cone`, `wedge`, `sphere`, `lathe`, `extrude`, `torus`,
//! `sweep` (with optional per-waypoint `:scales` and parallel-transport
//! framing); structural ops `composition`, `translate`, `rotate`,
//! `scale`, `mirror`, `array`. The `MeshError::NotYetImplemented`
//! variant is retained as an escape hatch for future vocabulary
//! additions but is unreachable from any v1 input.
//!
//! Convention: every primitive winds CCW from outside (normal =
//! `(b - a) × (c - a)` points outward). Verified by per-primitive
//! face-normal-direction tests.

use crate::ast::{Axis, Node};
use crate::csg;
use csg::plane::Plane3;
use csg::point::Point3;
use csg::polygon::Polygon as CsgPolygon;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Triangle {
    pub vertices: [[f32; 3]; 3],
    pub color: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    #[error("node kind not yet supported by mesher iteration 1: {0}")]
    NotYetImplemented(&'static str),
    #[error("CSG operation failed: {0}")]
    Csg(#[from] csg::CsgError),
}

/// Wire entry: evaluate `node` polygon-domain, run the cleanup pipeline
/// (with CDT triangulation), then fan back to wire `Triangle`s.
///
/// Cleanup runs **once** at the root, not after every CSG op — chained
/// `(difference A B C)` flows raw BSP polygons between steps and
/// triangulates a single time at the very end. Skipping the per-op
/// triangle round-trip is also what avoids the sliver-normal flip bug
/// (`from_triangle` re-deriving the plane on a sliver triangle picks
/// up the wrong sign for `n_z`).
pub fn mesh(node: &Node) -> Result<Vec<Triangle>, MeshError> {
    let mut polys = Vec::new();
    mesh_into_polygons(&mut polys, node, [0.0, 0.0, 0.0])?;
    Ok(csg::polygons_to_triangles(&csg::cleanup::run(polys)))
}

/// Polygon-domain entry: same composition as [`mesh`], but the cleanup
/// tail returns n-gon boundary loops (`cleanup::run_to_loops`). The
/// public polygon API in `crate::polygon` is the consumer.
pub fn mesh_polygons_internal(node: &Node) -> Result<Vec<CsgPolygon>, MeshError> {
    let mut polys = Vec::new();
    mesh_into_polygons(&mut polys, node, [0.0, 0.0, 0.0])?;
    Ok(csg::cleanup::run_to_loops(polys))
}

/// Recursive AST evaluator in polygon domain. Primitives still emit
/// triangles internally; `wrap_triangles_into` lifts them into
/// `CsgPolygon` once. Boolean ops use the `_raw` BSP entries — no
/// cleanup runs between chained ops, so chained CSG composes a single
/// polygon stream and only the root entry triggers the cleanup pass.
fn mesh_into_polygons(
    out: &mut Vec<CsgPolygon>,
    node: &Node,
    offset: [f32; 3],
) -> Result<(), MeshError> {
    match node {
        Node::Box { x, y, z, color } => {
            let mut tris = Vec::new();
            mesh_box(&mut tris, *x, *y, *z, *color, offset);
            wrap_triangles_into(out, &tris)
        }
        Node::Lathe {
            profile,
            segments,
            color,
        } => {
            let mut tris = Vec::new();
            mesh_lathe(&mut tris, profile, *segments, *color, offset);
            wrap_triangles_into(out, &tris)
        }
        Node::Torus {
            major_radius,
            minor_radius,
            major_segments,
            minor_segments,
            color,
        } => {
            let mut tris = Vec::new();
            mesh_torus(
                &mut tris,
                *major_radius,
                *minor_radius,
                *major_segments,
                *minor_segments,
                *color,
                offset,
            );
            wrap_triangles_into(out, &tris)
        }
        Node::Sweep {
            profile,
            path,
            scales,
            color,
        } => {
            let mut tris = Vec::new();
            mesh_sweep(&mut tris, profile, path, scales.as_deref(), *color, offset);
            wrap_triangles_into(out, &tris)
        }
        Node::Cylinder {
            radius,
            height,
            segments,
            color,
        } => {
            let mut tris = Vec::new();
            mesh_cylinder(&mut tris, *radius, *height, *segments, *color, offset);
            wrap_triangles_into(out, &tris)
        }
        Node::Cone {
            radius,
            height,
            segments,
            color,
        } => {
            let mut tris = Vec::new();
            mesh_cone(&mut tris, *radius, *height, *segments, *color, offset);
            wrap_triangles_into(out, &tris)
        }
        Node::Wedge { x, y, z, color } => {
            let mut tris = Vec::new();
            mesh_wedge(&mut tris, *x, *y, *z, *color, offset);
            wrap_triangles_into(out, &tris)
        }
        Node::Sphere {
            radius,
            subdivisions,
            color,
        } => {
            let mut tris = Vec::new();
            mesh_sphere(&mut tris, *radius, *subdivisions, *color, offset);
            wrap_triangles_into(out, &tris)
        }
        Node::Extrude {
            profile,
            depth,
            color,
        } => {
            let mut tris = Vec::new();
            mesh_extrude(&mut tris, profile, *depth, *color, offset);
            wrap_triangles_into(out, &tris)
        }
        Node::Composition(children) => {
            for child in children {
                mesh_into_polygons(out, child, offset)?;
            }
            Ok(())
        }
        Node::Translate {
            offset: delta,
            child,
        } => {
            let combined = [
                offset[0] + delta[0],
                offset[1] + delta[1],
                offset[2] + delta[2],
            ];
            mesh_into_polygons(out, child, combined)
        }
        Node::Rotate { axis, angle, child } => {
            let mut local = Vec::new();
            mesh_into_polygons(&mut local, child, [0.0, 0.0, 0.0])?;
            let n = normalize_or_default(*axis, [0.0, 1.0, 0.0]);
            for poly in &local {
                if let Some(transformed) = transform_polygon(poly, |v| {
                    let r = rotate_axis_angle(v, n, *angle);
                    [r[0] + offset[0], r[1] + offset[1], r[2] + offset[2]]
                })? {
                    out.push(transformed);
                }
            }
            Ok(())
        }
        Node::Scale { factor, child } => {
            let mut local = Vec::new();
            mesh_into_polygons(&mut local, child, [0.0, 0.0, 0.0])?;
            for poly in &local {
                if let Some(transformed) = transform_polygon(poly, |v| {
                    [
                        v[0] * factor[0] + offset[0],
                        v[1] * factor[1] + offset[1],
                        v[2] * factor[2] + offset[2],
                    ]
                })? {
                    out.push(transformed);
                }
            }
            Ok(())
        }
        Node::Mirror { axis, child } => {
            let mut local = Vec::new();
            mesh_into_polygons(&mut local, child, [0.0, 0.0, 0.0])?;
            for poly in &local {
                if let Some(mirrored) = mirror_polygon(poly, *axis, offset)? {
                    out.push(mirrored);
                }
            }
            Ok(())
        }
        Node::Array {
            count,
            spacing,
            child,
        } => {
            let mut local = Vec::new();
            mesh_into_polygons(&mut local, child, [0.0, 0.0, 0.0])?;
            for i in 0..*count {
                let f = i as f32;
                let dx = offset[0] + spacing[0] * f;
                let dy = offset[1] + spacing[1] * f;
                let dz = offset[2] + spacing[2] * f;
                for poly in &local {
                    if let Some(translated) =
                        transform_polygon(poly, |v| [v[0] + dx, v[1] + dy, v[2] + dz])?
                    {
                        out.push(translated);
                    }
                }
            }
            Ok(())
        }
        Node::Union { children } => {
            let mut acc: Option<Vec<CsgPolygon>> = None;
            for child in children {
                let mut child_polys = Vec::new();
                mesh_into_polygons(&mut child_polys, child, offset)?;
                acc = Some(match acc {
                    Some(prev) => csg::ops::union_raw(prev, child_polys)?,
                    None => child_polys,
                });
            }
            if let Some(result) = acc {
                out.extend(result);
            }
            Ok(())
        }
        Node::Intersection { children } => {
            let mut acc: Option<Vec<CsgPolygon>> = None;
            for child in children {
                let mut child_polys = Vec::new();
                mesh_into_polygons(&mut child_polys, child, offset)?;
                acc = Some(match acc {
                    Some(prev) => csg::ops::intersection_raw(prev, child_polys)?,
                    None => child_polys,
                });
            }
            if let Some(result) = acc {
                out.extend(result);
            }
            Ok(())
        }
        Node::Difference { base, subtract } => {
            let mut acc = Vec::new();
            mesh_into_polygons(&mut acc, base, offset)?;
            for s in subtract {
                let mut s_polys = Vec::new();
                mesh_into_polygons(&mut s_polys, s, offset)?;
                acc = csg::ops::difference_raw(acc, s_polys)?;
            }
            out.extend(acc);
            Ok(())
        }
    }
}

/// Wrap each triangle in `tris` as a single-triangle [`CsgPolygon`] and
/// append to `out`. Out-of-range vertices surface as
/// [`MeshError::Csg`] rather than silent drops, matching the historic
/// behavior at the CSG-input boundary (ADR-0054 ±256 unit cap).
fn wrap_triangles_into(out: &mut Vec<CsgPolygon>, tris: &[Triangle]) -> Result<(), MeshError> {
    for t in tris {
        let v0 = Point3::from_f32(t.vertices[0]).map_err(csg::CsgError::from)?;
        let v1 = Point3::from_f32(t.vertices[1]).map_err(csg::CsgError::from)?;
        let v2 = Point3::from_f32(t.vertices[2]).map_err(csg::CsgError::from)?;
        if let Some(p) = CsgPolygon::from_triangle(v0, v1, v2, t.color) {
            out.push(p);
        }
    }
    Ok(())
}

fn point_from_f32(v: [f32; 3]) -> Result<Point3, MeshError> {
    Point3::from_f32(v).map_err(|e| csg::CsgError::from(e).into())
}

/// Apply `xform` to every vertex of `poly`, re-derive the plane from
/// three non-collinear transformed vertices. Returns `None` if any
/// vertex falls outside the fixed-point range *or* no non-degenerate
/// triple exists in the transformed polygon (genuinely degenerate);
/// errors out via [`MeshError::Csg`] only for the range case so the
/// caller can distinguish.
///
/// The cleanup pipeline emits n-gon loops with T-junction repairs
/// inserted as collinear interior vertices, so `from_points` on the
/// first three vertices isn't enough — they may be collinear along an
/// edge. [`derive_plane_robust`] walks the vertex ring until it finds
/// a non-degenerate triple.
fn transform_polygon<F>(poly: &CsgPolygon, xform: F) -> Result<Option<CsgPolygon>, MeshError>
where
    F: Fn([f32; 3]) -> [f32; 3],
{
    let mut new_verts = Vec::with_capacity(poly.vertices.len());
    for v in &poly.vertices {
        let f = v.to_f32();
        let t = xform(f);
        new_verts.push(point_from_f32(t)?);
    }
    if new_verts.len() < 3 {
        return Ok(None);
    }
    let Some(plane) = derive_plane_robust(&new_verts) else {
        return Ok(None);
    };
    Ok(Some(CsgPolygon {
        vertices: new_verts,
        plane,
        color: poly.color,
    }))
}

/// Find three non-collinear vertices in `verts` and return the plane
/// they define, or `None` if the polygon is fully degenerate. Anchors
/// on `verts[0]` and walks forward looking for a `(v0, vi, vi+1)`
/// triple that gives a non-degenerate plane.
fn derive_plane_robust(verts: &[Point3]) -> Option<Plane3> {
    if verts.len() < 3 {
        return None;
    }
    let v0 = verts[0];
    for i in 1..verts.len() - 1 {
        let plane = Plane3::from_points(v0, verts[i], verts[i + 1]);
        if !plane.is_degenerate() {
            return Some(plane);
        }
    }
    None
}

/// Mirror `poly` across `axis`, then translate by `offset`. Reflection
/// inverts winding; reverse the vertex list and re-derive the plane so
/// downstream classification still treats the polygon as outward-CCW.
fn mirror_polygon(
    poly: &CsgPolygon,
    axis: Axis,
    offset: [f32; 3],
) -> Result<Option<CsgPolygon>, MeshError> {
    let mut new_verts = Vec::with_capacity(poly.vertices.len());
    for v in &poly.vertices {
        let f = v.to_f32();
        let m = match axis {
            Axis::X => [-f[0], f[1], f[2]],
            Axis::Y => [f[0], -f[1], f[2]],
            Axis::Z => [f[0], f[1], -f[2]],
        };
        let t = [m[0] + offset[0], m[1] + offset[1], m[2] + offset[2]];
        new_verts.push(point_from_f32(t)?);
    }
    if new_verts.len() < 3 {
        return Ok(None);
    }
    new_verts.reverse();
    let Some(plane) = derive_plane_robust(&new_verts) else {
        return Ok(None);
    };
    Ok(Some(CsgPolygon {
        vertices: new_verts,
        plane,
        color: poly.color,
    }))
}

/// Emit 12 triangles (6 quad faces) for an axis-aligned box of size
/// `(x, y, z)` centered at `(0, 0, 0)` then translated by `offset`.
///
/// Faces wound CCW from outside, so `(b - a) × (c - a)` points outward.
fn mesh_box(out: &mut Vec<Triangle>, x: f32, y: f32, z: f32, color: u32, offset: [f32; 3]) {
    let hx = x * 0.5;
    let hy = y * 0.5;
    let hz = z * 0.5;
    let [ox, oy, oz] = offset;

    // Eight corners, named by sign per axis.
    let nnn = [ox - hx, oy - hy, oz - hz];
    let pnn = [ox + hx, oy - hy, oz - hz];
    let npn = [ox - hx, oy + hy, oz - hz];
    let ppn = [ox + hx, oy + hy, oz - hz];
    let nnp = [ox - hx, oy - hy, oz + hz];
    let pnp = [ox + hx, oy - hy, oz + hz];
    let npp = [ox - hx, oy + hy, oz + hz];
    let ppp = [ox + hx, oy + hy, oz + hz];

    let push = |out: &mut Vec<Triangle>, a, b, c| {
        out.push(Triangle {
            vertices: [a, b, c],
            color,
        });
    };

    // -Z face (looking toward +z): nnn, npn, ppn, pnn — CCW from outside (-z side)
    push(out, nnn, npn, ppn);
    push(out, nnn, ppn, pnn);
    // +Z face: nnp, pnp, ppp, npp — CCW from +z side
    push(out, nnp, pnp, ppp);
    push(out, nnp, ppp, npp);
    // -X face: nnn, nnp, npp, npn
    push(out, nnn, nnp, npp);
    push(out, nnn, npp, npn);
    // +X face: pnn, ppn, ppp, pnp
    push(out, pnn, ppn, ppp);
    push(out, pnn, ppp, pnp);
    // -Y face: nnn, pnn, pnp, nnp
    push(out, nnn, pnn, pnp);
    push(out, nnn, pnp, nnp);
    // +Y face: npn, npp, ppp, ppn
    push(out, npn, npp, ppp);
    push(out, npn, ppp, ppn);
}

/// Revolve `profile` (list of `(x, y)` points, x = radius, y = height)
/// around the Y axis with `segments` divisions. Profile vertices with
/// `x == 0` collapse to a single point on the axis — the surrounding
/// quads degenerate to triangles, which fills caps and apex points
/// correctly without a separate cap pass.
///
/// Faces are wound CCW from outside (normal = `(b - a) × (c - a)` points
/// away from the Y axis), matching the box mesher's convention so the
/// substrate's render pipeline shows them right-side-out.
fn mesh_lathe(
    out: &mut Vec<Triangle>,
    profile: &[[f32; 2]],
    segments: u32,
    color: u32,
    offset: [f32; 3],
) {
    if profile.len() < 2 || segments < 3 {
        return;
    }
    let segments = segments as usize;
    let two_pi = std::f32::consts::TAU;
    let cos_sin: Vec<(f32, f32)> = (0..segments)
        .map(|i| {
            let theta = two_pi * (i as f32) / (segments as f32);
            (theta.cos(), theta.sin())
        })
        .collect();

    // For each profile-edge band (k → k+1) and each angular slice (i → i+1):
    //   a = P[k]_i, b = P[k+1]_i, c = P[k]_(i+1), d = P[k+1]_(i+1)
    // Triangulate as (a, b, c) + (c, b, d). Verified outward-facing by
    // expanding (b - a) × (c - a); for a cylinder the normal collapses
    // to the radial direction at θ_i.
    let revolve = |radius: f32, height: f32, i: usize| -> [f32; 3] {
        let (cos, sin) = cos_sin[i % segments];
        [
            offset[0] + radius * cos,
            offset[1] + height,
            offset[2] + radius * sin,
        ]
    };

    for k in 0..profile.len() - 1 {
        let r0 = profile[k][0];
        let y0 = profile[k][1];
        let r1 = profile[k + 1][0];
        let y1 = profile[k + 1][1];
        for i in 0..segments {
            let j = (i + 1) % segments;
            let a = revolve(r0, y0, i);
            let b = revolve(r1, y1, i);
            let c = revolve(r0, y0, j);
            let d = revolve(r1, y1, j);
            push_unless_degenerate(out, a, b, c, color);
            push_unless_degenerate(out, c, b, d, color);
        }
    }
}

fn push_unless_degenerate(
    out: &mut Vec<Triangle>,
    a: [f32; 3],
    b: [f32; 3],
    c: [f32; 3],
    color: u32,
) {
    // Skip triangles with two coincident vertices — happens when a
    // profile point sits on the rotation axis (x=0) and adjacent
    // angular samples collapse to the same point.
    if approx_eq(a, b) || approx_eq(b, c) || approx_eq(a, c) {
        return;
    }
    out.push(Triangle {
        vertices: [a, b, c],
        color,
    });
}

fn approx_eq(a: [f32; 3], b: [f32; 3]) -> bool {
    const EPS: f32 = 1e-6;
    (a[0] - b[0]).abs() < EPS && (a[1] - b[1]).abs() < EPS && (a[2] - b[2]).abs() < EPS
}

/// Donut around the Y axis. Generates `major_segments × minor_segments`
/// quads (× 2 triangles) on the surface. The major loop sweeps angle α
/// around the Y axis; the minor loop sweeps angle β around the tube
/// cross-section. Triangles wound CCW from outside (radial-outward
/// normal verified by the standard cross-product test).
fn mesh_torus(
    out: &mut Vec<Triangle>,
    major_radius: f32,
    minor_radius: f32,
    major_segments: u32,
    minor_segments: u32,
    color: u32,
    offset: [f32; 3],
) {
    if major_segments < 3 || minor_segments < 3 {
        return;
    }
    let m = major_segments as usize;
    let n = minor_segments as usize;
    let two_pi = std::f32::consts::TAU;
    // P(i, j) = vertex at major angle α_i, minor angle β_j.
    let position = |i: usize, j: usize| -> [f32; 3] {
        let alpha = two_pi * (i as f32) / (m as f32);
        let beta = two_pi * (j as f32) / (n as f32);
        let cos_a = alpha.cos();
        let sin_a = alpha.sin();
        let cos_b = beta.cos();
        let sin_b = beta.sin();
        let r = major_radius + minor_radius * cos_b;
        [
            offset[0] + r * cos_a,
            offset[1] + minor_radius * sin_b,
            offset[2] + r * sin_a,
        ]
    };
    for i in 0..m {
        let i_next = (i + 1) % m;
        for j in 0..n {
            let j_next = (j + 1) % n;
            // a = P(i, j), b = P(i+1, j), c = P(i, j+1), d = P(i+1, j+1).
            // Both i and j are angular here (unlike lathe where k was a
            // profile-height index), so the natural (a, b, c) winding
            // gives an inward normal. Flip to (a, c, b) + (c, d, b) for
            // outward-facing.
            let a = position(i, j);
            let b = position(i_next, j);
            let c = position(i, j_next);
            let d = position(i_next, j_next);
            out.push(Triangle {
                vertices: [a, c, b],
                color,
            });
            out.push(Triangle {
                vertices: [c, d, b],
                color,
            });
        }
    }
}

/// Sweep a 2D `profile` polygon along a 3D `path`. At each path
/// waypoint the profile is oriented perpendicular to the local tangent.
/// Adjacent rings are stitched into quads (triangulated).
///
/// Path representation choice: a polyline (list of (x, y, z) points).
/// ADR-0026 doesn't pin this — bezier / catmull-rom would be smoother
/// but require more careful tessellation. Polyline is the simplest
/// correct choice for v1.
///
/// Frame computation: at each waypoint we compute a tangent T (forward
/// difference at endpoints, central elsewhere), then pick a "right"
/// vector R perpendicular to T using world up `(0, 1, 0)` as a
/// reference. If T is nearly parallel to up we fall back to
/// `(1, 0, 0)`. The profile's local (x, y) maps to (R, U) where
/// U = T × R. This isn't a rotation-minimizing frame (parallel
/// transport would be) but it's stable enough for short paths like
/// teapot spouts.
///
/// Caps are NOT generated — the swept surface is open at both ends.
/// For closed tubes, end the path on a small profile (or composition
/// with a separate cap primitive).
fn mesh_sweep(
    out: &mut Vec<Triangle>,
    profile: &[[f32; 2]],
    path: &[[f32; 3]],
    scales: Option<&[f32]>,
    color: u32,
    offset: [f32; 3],
) {
    if profile.len() < 3 || path.len() < 2 {
        return;
    }
    // ADR-0051 requires `:scales` length to equal `path` length; the
    // parser enforces it (`SweepScalesLengthMismatch`). The defensive
    // length check here is a backstop in case a caller constructs the
    // `Node::Sweep` AST directly.
    let scales = match scales {
        Some(s) if s.len() == path.len() => Some(s),
        _ => None,
    };
    let n = profile.len();

    // Compute a tangent at each waypoint.
    let mut tangents: Vec<[f32; 3]> = Vec::with_capacity(path.len());
    for k in 0..path.len() {
        let prev = if k == 0 { path[k] } else { path[k - 1] };
        let next = if k == path.len() - 1 {
            path[k]
        } else {
            path[k + 1]
        };
        let t = normalize_or_default(
            [next[0] - prev[0], next[1] - prev[1], next[2] - prev[2]],
            [0.0, 0.0, 1.0],
        );
        tangents.push(t);
    }

    // Build the profile ring at each waypoint using a parallel-transport
    // frame: the first frame is seeded from world up; each subsequent
    // frame is the previous frame rotated by the smallest angle that
    // takes the previous tangent onto the current one. This keeps the
    // cross-section's orientation continuous along the curve, avoiding
    // the visible "twist" you get from picking each frame independently
    // off a fixed reference. Without this, paths with tangents that
    // approach world-up flip the up reference between adjacent
    // waypoints and the tube reads as having varying diameter.
    let mut rings: Vec<Vec<[f32; 3]>> = Vec::with_capacity(path.len());
    let t0 = tangents[0];
    let up_ref = if t0[1].abs() > 0.95 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    let mut r = normalize_or_default(cross(up_ref, t0), [1.0, 0.0, 0.0]);
    let mut u = cross(t0, r);
    for (k, p) in path.iter().enumerate() {
        let t = tangents[k];
        if k > 0 {
            let prev_t = tangents[k - 1];
            let axis = cross(prev_t, t);
            let axis_len_sq = axis[0] * axis[0] + axis[1] * axis[1] + axis[2] * axis[2];
            if axis_len_sq > 1e-12 {
                let dot = (prev_t[0] * t[0] + prev_t[1] * t[1] + prev_t[2] * t[2]).clamp(-1.0, 1.0);
                let angle = dot.acos();
                let axis_n = normalize_or_default(axis, [0.0, 1.0, 0.0]);
                r = rotate_axis_angle(r, axis_n, angle);
                u = rotate_axis_angle(u, axis_n, angle);
            }
        }
        let scale = scales.map(|s| s[k]).unwrap_or(1.0);
        let mut ring = Vec::with_capacity(n);
        for pt in profile {
            let sx = pt[0] * scale;
            let sy = pt[1] * scale;
            let world = [
                offset[0] + p[0] + sx * r[0] + sy * u[0],
                offset[1] + p[1] + sx * r[1] + sy * u[1],
                offset[2] + p[2] + sx * r[2] + sy * u[2],
            ];
            ring.push(world);
        }
        rings.push(ring);
    }

    // Stitch adjacent rings.
    for k in 0..rings.len() - 1 {
        let r0 = &rings[k];
        let r1 = &rings[k + 1];
        for i in 0..n {
            let j = (i + 1) % n;
            let a = r0[i];
            let b = r1[i];
            let c = r0[j];
            let d = r1[j];
            push_unless_degenerate(out, a, b, c, color);
            push_unless_degenerate(out, c, b, d, color);
        }
    }
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize_or_default(v: [f32; 3], fallback: [f32; 3]) -> [f32; 3] {
    let len_sq = v[0] * v[0] + v[1] * v[1] + v[2] * v[2];
    if len_sq < 1e-12 {
        return fallback;
    }
    let inv = 1.0 / len_sq.sqrt();
    [v[0] * inv, v[1] * inv, v[2] * inv]
}

/// Rotate `v` around unit axis `n` by `angle` radians (Rodrigues' formula).
/// `n` MUST be normalized — caller's responsibility.
fn rotate_axis_angle(v: [f32; 3], n: [f32; 3], angle: f32) -> [f32; 3] {
    let c = angle.cos();
    let s = angle.sin();
    let dot = n[0] * v[0] + n[1] * v[1] + n[2] * v[2];
    let kx = cross(n, v);
    [
        v[0] * c + kx[0] * s + n[0] * dot * (1.0 - c),
        v[1] * c + kx[1] * s + n[1] * dot * (1.0 - c),
        v[2] * c + kx[2] * s + n[2] * dot * (1.0 - c),
    ]
}

/// Cylinder of `radius` and total `height`, centered on the Y axis at
/// `offset`. Implemented as a lathe of a 4-point profile so the side
/// + cap winding matches the rest of the lathed primitives.
fn mesh_cylinder(
    out: &mut Vec<Triangle>,
    radius: f32,
    height: f32,
    segments: u32,
    color: u32,
    offset: [f32; 3],
) {
    let h = height * 0.5;
    let profile = [[0.0, -h], [radius, -h], [radius, h], [0.0, h]];
    mesh_lathe(out, &profile, segments, color, offset);
}

/// Cone of `radius` and total `height`, base on the -Y side and apex
/// on the +Y side, centered at `offset`. Implemented as a lathe.
fn mesh_cone(
    out: &mut Vec<Triangle>,
    radius: f32,
    height: f32,
    segments: u32,
    color: u32,
    offset: [f32; 3],
) {
    let h = height * 0.5;
    let profile = [[0.0, -h], [radius, -h], [0.0, h]];
    mesh_lathe(out, &profile, segments, color, offset);
}

/// UV sphere of `radius`, centered at `offset`. `subdivisions` controls
/// both the number of latitude rings (between poles, exclusive) and the
/// number of longitude segments. Implemented as a lathe of a half-circle
/// profile from south pole to north pole; pole quads degenerate naturally.
fn mesh_sphere(
    out: &mut Vec<Triangle>,
    radius: f32,
    subdivisions: u32,
    color: u32,
    offset: [f32; 3],
) {
    if subdivisions < 3 {
        return;
    }
    let n = subdivisions as usize;
    let mut profile: Vec<[f32; 2]> = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let theta = -std::f32::consts::FRAC_PI_2 + (i as f32) * std::f32::consts::PI / (n as f32);
        profile.push([radius * theta.cos(), radius * theta.sin()]);
    }
    mesh_lathe(out, &profile, subdivisions, color, offset);
}

/// Right-triangular prism (ramp) with extents `(x, y, z)` centered at
/// `offset`. The hypotenuse face slopes from the front-bottom edge
/// (`+z/2, -y/2`) up to the back-top edge (`-z/2, +y/2`). Six vertices,
/// five faces (bottom quad, back quad, hypotenuse quad, two triangular
/// sides). Faces wound CCW from outside.
fn mesh_wedge(out: &mut Vec<Triangle>, x: f32, y: f32, z: f32, color: u32, offset: [f32; 3]) {
    let hx = x * 0.5;
    let hy = y * 0.5;
    let hz = z * 0.5;
    let [ox, oy, oz] = offset;
    let a = [ox - hx, oy - hy, oz - hz]; // back-bottom-left
    let b = [ox + hx, oy - hy, oz - hz]; // back-bottom-right
    let c = [ox - hx, oy - hy, oz + hz]; // front-bottom-left
    let d = [ox + hx, oy - hy, oz + hz]; // front-bottom-right
    let e = [ox - hx, oy + hy, oz - hz]; // back-top-left
    let f = [ox + hx, oy + hy, oz - hz]; // back-top-right

    let push = |out: &mut Vec<Triangle>, p, q, r| {
        out.push(Triangle {
            vertices: [p, q, r],
            color,
        });
    };

    // Bottom (-Y): a, b, d, c going CCW viewed from -Y
    push(out, a, b, d);
    push(out, a, d, c);
    // Back (-Z): a, e, f, b going CCW viewed from -Z
    push(out, a, e, f);
    push(out, a, f, b);
    // Left side (-X): a, c, e
    push(out, a, c, e);
    // Right side (+X): b, f, d
    push(out, b, f, d);
    // Hypotenuse (+Y/+Z): c, d, f, e
    push(out, c, d, f);
    push(out, c, f, e);
}

/// Extrude a 2D `profile` polygon along Z by `depth`. Generates side-wall
/// quads + two cap polygons triangulated by a fan from vertex 0.
///
/// The profile is interpreted as listed CCW when viewed from +Z. The back
/// cap (at `z = depth`) keeps the original winding (normal +Z); the
/// front cap (at `z = 0`) reverses the winding (normal -Z). Side walls
/// stitch each profile edge `p_i → p_{i+1}` between the two cap planes.
///
/// **Caller's contract**: `profile` must be convex for the fan
/// triangulation to produce a correct cap. Concave profiles will tile
/// the cap with overlapping triangles — a future ear-clipping pass
/// would lift this restriction. The v1 vocabulary is convex-only by
/// convention (per ADR-0026's primitive set).
fn mesh_extrude(
    out: &mut Vec<Triangle>,
    profile: &[[f32; 2]],
    depth: f32,
    color: u32,
    offset: [f32; 3],
) {
    if profile.len() < 3 || depth <= 0.0 {
        return;
    }
    let n = profile.len();
    let [ox, oy, oz] = offset;
    let base = |i: usize| -> [f32; 3] { [ox + profile[i][0], oy + profile[i][1], oz] };
    let top = |i: usize| -> [f32; 3] { [ox + profile[i][0], oy + profile[i][1], oz + depth] };

    // Side walls. For edge p_i → p_{i+1}, the quad corners CCW from
    // outside are (base i, base i+1, top i+1, top i). Triangulate as
    // (a, b, c) + (a, c, d) — outward normal verified for CCW profiles.
    for i in 0..n {
        let j = (i + 1) % n;
        let a = base(i);
        let b = base(j);
        let c = top(j);
        let d = top(i);
        push_unless_degenerate(out, a, b, c, color);
        push_unless_degenerate(out, a, c, d, color);
    }

    // Back cap (z = depth, normal +Z): fan from vertex 0 in original
    // winding.
    for i in 1..n - 1 {
        push_unless_degenerate(out, top(0), top(i), top(i + 1), color);
    }
    // Front cap (z = 0, normal -Z): reverse winding.
    for i in 1..n - 1 {
        push_unless_degenerate(out, base(0), base(i + 1), base(i), color);
    }
}
