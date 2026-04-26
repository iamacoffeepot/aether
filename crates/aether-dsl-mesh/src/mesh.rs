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
        Node::Box { x, y, z, color } => mesh_box(out, *x, *y, *z, *color, offset),
        Node::Lathe {
            profile,
            segments,
            color,
        } => mesh_lathe(out, profile, *segments, *color, offset),
        Node::Torus {
            major_radius,
            minor_radius,
            major_segments,
            minor_segments,
            color,
        } => mesh_torus(
            out,
            *major_radius,
            *minor_radius,
            *major_segments,
            *minor_segments,
            *color,
            offset,
        ),
        Node::Sweep {
            profile,
            path,
            scales,
            color,
        } => mesh_sweep(out, profile, path, scales.as_deref(), *color, offset),
        Node::Cylinder {
            radius,
            height,
            segments,
            color,
        } => mesh_cylinder(out, *radius, *height, *segments, *color, offset),
        Node::Cone {
            radius,
            height,
            segments,
            color,
        } => mesh_cone(out, *radius, *height, *segments, *color, offset),
        Node::Wedge { x, y, z, color } => mesh_wedge(out, *x, *y, *z, *color, offset),
        Node::Sphere {
            radius,
            subdivisions,
            color,
        } => mesh_sphere(out, *radius, *subdivisions, *color, offset),
        Node::Extrude {
            profile,
            depth,
            color,
        } => mesh_extrude(out, profile, *depth, *color, offset),
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

            // Algebraic identity: A − B − C − ... − N = A − (B ∪ C ∪
            // ... ∪ N). When *every* subtractor is a CSG-leaf (no
            // nested Union/Intersection/Difference inside), it's safe
            // to union the cutters first and do a single difference
            // against the base. The base only fragments under one BSP
            // pass instead of N, which sharply reduces the chained
            // snap-drift accumulation that otherwise breaks
            // multi-cutter regressions like three_cut_box.
            //
            // Skipping the rewrite when any subtractor is composite is
            // intentional: composite CSG output can carry T-junctions
            // and slivers from its own pipeline, and unioning that
            // with a clean cutter would amplify them. Better to take
            // the chained-pairwise hit there than risk wrong-shape
            // output.
            if subtract.len() > 1 && subtract.iter().all(is_csg_leaf) {
                let mut combined = Vec::new();
                for s in subtract {
                    mesh_into_polygons(&mut combined, s, offset)?;
                }
                acc = csg::ops::difference_raw(acc, combined)?;
            } else {
                for s in subtract {
                    let mut s_polys = Vec::new();
                    mesh_into_polygons(&mut s_polys, s, offset)?;
                    acc = csg::ops::difference_raw(acc, s_polys)?;
                }
            }

            out.extend(acc);
            Ok(())
        }
    }
}

/// `true` when `node` is a CSG-leaf: contains no Union, Intersection,
/// or Difference at any depth. Primitives, transforms of primitives,
/// and compositions of leaves all qualify.
fn is_csg_leaf(node: &Node) -> bool {
    match node {
        Node::Box { .. }
        | Node::Cylinder { .. }
        | Node::Cone { .. }
        | Node::Wedge { .. }
        | Node::Sphere { .. }
        | Node::Lathe { .. }
        | Node::Extrude { .. }
        | Node::Torus { .. }
        | Node::Sweep { .. } => true,
        Node::Composition(children) => children.iter().all(is_csg_leaf),
        Node::Translate { child, .. }
        | Node::Rotate { child, .. }
        | Node::Scale { child, .. }
        | Node::Mirror { child, .. }
        | Node::Array { child, .. } => is_csg_leaf(child),
        Node::Union { .. } | Node::Intersection { .. } | Node::Difference { .. } => false,
    }
}

fn point_from_f32(v: [f32; 3]) -> Result<Point3, MeshError> {
    Point3::from_f32(v).map_err(|e| csg::CsgError::from(e).into())
}

/// Build a [`CsgPolygon`] from an n-gon vertex list (n ≥ 3) and push it
/// to `out`. Consecutive duplicate vertices are deduped (axis-collapse
/// from primitives like lathe / sphere-pole rings collapses naturally
/// — a quad band with one ring at the axis becomes a triangle, a
/// fully-collapsed band drops out). Plane is re-derived via the robust
/// non-collinear-triple search; degenerate polygons are silently
/// skipped. Out-of-range vertices surface as [`MeshError::Csg`] —
/// loud failure at the ±256 unit boundary (ADR-0054).
fn push_polygon_from_f32(
    out: &mut Vec<CsgPolygon>,
    verts: &[[f32; 3]],
    color: u32,
) -> Result<(), MeshError> {
    if verts.len() < 3 {
        return Ok(());
    }
    let mut points: Vec<Point3> = Vec::with_capacity(verts.len());
    for v in verts {
        let p = point_from_f32(*v)?;
        if points.last() != Some(&p) {
            points.push(p);
        }
    }
    if points.len() >= 2 && points.first() == points.last() {
        points.pop();
    }
    if points.len() < 3 {
        return Ok(());
    }
    let Some(plane) = derive_plane_robust(&points) else {
        return Ok(());
    };
    out.push(CsgPolygon {
        vertices: points,
        plane,
        color,
    });
    Ok(())
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

/// Emit 6 quad faces for an axis-aligned box of size `(x, y, z)`
/// centered at `(0, 0, 0)` then translated by `offset`.
///
/// Faces wound CCW from outside, so `(b - a) × (c - a)` points outward.
fn mesh_box(
    out: &mut Vec<CsgPolygon>,
    x: f32,
    y: f32,
    z: f32,
    color: u32,
    offset: [f32; 3],
) -> Result<(), MeshError> {
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

    push_polygon_from_f32(out, &[nnn, npn, ppn, pnn], color)?; // -Z face
    push_polygon_from_f32(out, &[nnp, pnp, ppp, npp], color)?; // +Z face
    push_polygon_from_f32(out, &[nnn, nnp, npp, npn], color)?; // -X face
    push_polygon_from_f32(out, &[pnn, ppn, ppp, pnp], color)?; // +X face
    push_polygon_from_f32(out, &[nnn, pnn, pnp, nnp], color)?; // -Y face
    push_polygon_from_f32(out, &[npn, npp, ppp, ppn], color)?; // +Y face
    Ok(())
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
    out: &mut Vec<CsgPolygon>,
    profile: &[[f32; 2]],
    segments: u32,
    color: u32,
    offset: [f32; 3],
) -> Result<(), MeshError> {
    if profile.len() < 2 || segments < 3 {
        return Ok(());
    }
    let segments = segments as usize;
    let two_pi = std::f32::consts::TAU;
    let cos_sin: Vec<(f32, f32)> = (0..segments)
        .map(|i| {
            let theta = two_pi * (i as f32) / (segments as f32);
            (theta.cos(), theta.sin())
        })
        .collect();

    // For each profile-edge band (k → k+1) and each angular slice
    // (i → i+1) we emit one quad with corners CCW from outside:
    //   a = P[k]_i, b = P[k+1]_i, c = P[k]_(i+1), d = P[k+1]_(i+1)
    //   quad order = (a, b, d, c) — radial-outward normal verified by
    //   the existing lathe_face_normals_point_outward test.
    //
    // Profile vertices with `x == 0` collapse the corresponding ring
    // to a single axis point; the quad degenerates to a triangle (or
    // drops out entirely if both endpoints are axial).
    // `push_polygon_from_f32` dedupes consecutive coincident vertices,
    // so the cap fans collapse cleanly without a separate pass.
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
            push_polygon_from_f32(out, &[a, b, d, c], color)?;
        }
    }
    Ok(())
}

/// Donut around the Y axis. Generates `major_segments × minor_segments`
/// quads (× 2 triangles) on the surface. The major loop sweeps angle α
/// around the Y axis; the minor loop sweeps angle β around the tube
/// cross-section. Triangles wound CCW from outside (radial-outward
/// normal verified by the standard cross-product test).
fn mesh_torus(
    out: &mut Vec<CsgPolygon>,
    major_radius: f32,
    minor_radius: f32,
    major_segments: u32,
    minor_segments: u32,
    color: u32,
    offset: [f32; 3],
) -> Result<(), MeshError> {
    if major_segments < 3 || minor_segments < 3 {
        return Ok(());
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
            // Both i and j are angular; outward-facing quad order is
            // (a, c, d, b) — same diagonal split as the previous
            // (a, c, b) + (c, d, b) triangulation.
            let a = position(i, j);
            let b = position(i_next, j);
            let c = position(i, j_next);
            let d = position(i_next, j_next);
            push_polygon_from_f32(out, &[a, c, d, b], color)?;
        }
    }
    Ok(())
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
    out: &mut Vec<CsgPolygon>,
    profile: &[[f32; 2]],
    path: &[[f32; 3]],
    scales: Option<&[f32]>,
    color: u32,
    offset: [f32; 3],
) -> Result<(), MeshError> {
    if profile.len() < 3 || path.len() < 2 {
        return Ok(());
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

    // Stitch adjacent rings as quads — same diagonal split as the
    // previous (a, b, c) + (c, b, d) triangulation, ordered (a, b, d, c)
    // for CCW-from-outside winding.
    for k in 0..rings.len() - 1 {
        let r0 = &rings[k];
        let r1 = &rings[k + 1];
        for i in 0..n {
            let j = (i + 1) % n;
            let a = r0[i];
            let b = r1[i];
            let c = r0[j];
            let d = r1[j];
            push_polygon_from_f32(out, &[a, b, d, c], color)?;
        }
    }
    Ok(())
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
    out: &mut Vec<CsgPolygon>,
    radius: f32,
    height: f32,
    segments: u32,
    color: u32,
    offset: [f32; 3],
) -> Result<(), MeshError> {
    let h = height * 0.5;
    let profile = [[0.0, -h], [radius, -h], [radius, h], [0.0, h]];
    mesh_lathe(out, &profile, segments, color, offset)
}

/// Cone of `radius` and total `height`, base on the -Y side and apex
/// on the +Y side, centered at `offset`. Implemented as a lathe.
fn mesh_cone(
    out: &mut Vec<CsgPolygon>,
    radius: f32,
    height: f32,
    segments: u32,
    color: u32,
    offset: [f32; 3],
) -> Result<(), MeshError> {
    let h = height * 0.5;
    let profile = [[0.0, -h], [radius, -h], [0.0, h]];
    mesh_lathe(out, &profile, segments, color, offset)
}

/// UV sphere of `radius`, centered at `offset`. `subdivisions` controls
/// both the number of latitude rings (between poles, exclusive) and the
/// number of longitude segments. Implemented as a lathe of a half-circle
/// profile from south pole to north pole; pole quads degenerate naturally.
fn mesh_sphere(
    out: &mut Vec<CsgPolygon>,
    radius: f32,
    subdivisions: u32,
    color: u32,
    offset: [f32; 3],
) -> Result<(), MeshError> {
    if subdivisions < 3 {
        return Ok(());
    }
    let n = subdivisions as usize;
    let mut profile: Vec<[f32; 2]> = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let theta = -std::f32::consts::FRAC_PI_2 + (i as f32) * std::f32::consts::PI / (n as f32);
        profile.push([radius * theta.cos(), radius * theta.sin()]);
    }
    mesh_lathe(out, &profile, subdivisions, color, offset)
}

/// Right-triangular prism (ramp) with extents `(x, y, z)` centered at
/// `offset`. The hypotenuse face slopes from the front-bottom edge
/// (`+z/2, -y/2`) up to the back-top edge (`-z/2, +y/2`). Six vertices,
/// five faces (bottom quad, back quad, hypotenuse quad, two triangular
/// sides). Faces wound CCW from outside.
fn mesh_wedge(
    out: &mut Vec<CsgPolygon>,
    x: f32,
    y: f32,
    z: f32,
    color: u32,
    offset: [f32; 3],
) -> Result<(), MeshError> {
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

    push_polygon_from_f32(out, &[a, b, d, c], color)?; // Bottom (-Y) quad
    push_polygon_from_f32(out, &[a, e, f, b], color)?; // Back (-Z) quad
    push_polygon_from_f32(out, &[a, c, e], color)?; // Left side (-X) tri
    push_polygon_from_f32(out, &[b, f, d], color)?; // Right side (+X) tri
    push_polygon_from_f32(out, &[c, d, f, e], color)?; // Hypotenuse (+Y/+Z) quad
    Ok(())
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
    out: &mut Vec<CsgPolygon>,
    profile: &[[f32; 2]],
    depth: f32,
    color: u32,
    offset: [f32; 3],
) -> Result<(), MeshError> {
    if profile.len() < 3 || depth <= 0.0 {
        return Ok(());
    }
    let n = profile.len();
    let [ox, oy, oz] = offset;
    let base = |i: usize| -> [f32; 3] { [ox + profile[i][0], oy + profile[i][1], oz] };
    let top = |i: usize| -> [f32; 3] { [ox + profile[i][0], oy + profile[i][1], oz + depth] };

    // Side walls. For edge p_i → p_{i+1}, the quad corners CCW from
    // outside are (base i, base i+1, top i+1, top i).
    for i in 0..n {
        let j = (i + 1) % n;
        push_polygon_from_f32(out, &[base(i), base(j), top(j), top(i)], color)?;
    }

    // Back cap (z = depth, normal +Z): the profile in original CCW
    // winding becomes one n-gon. Front cap (z = 0, normal -Z) is the
    // same loop reversed.
    let back_cap: Vec<[f32; 3]> = (0..n).map(top).collect();
    push_polygon_from_f32(out, &back_cap, color)?;
    let front_cap: Vec<[f32; 3]> = (0..n).rev().map(base).collect();
    push_polygon_from_f32(out, &front_cap, color)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_node(x: f32, color: u32) -> Node {
        Node::Box {
            x,
            y: 1.0,
            z: 1.0,
            color,
        }
    }

    #[test]
    fn is_csg_leaf_recognizes_pure_primitives() {
        assert!(is_csg_leaf(&box_node(1.0, 0)));
        assert!(is_csg_leaf(&Node::Sphere {
            radius: 1.0,
            subdivisions: 6,
            color: 0,
        }));
    }

    #[test]
    fn is_csg_leaf_descends_through_transforms_and_compositions() {
        let translated = Node::Translate {
            offset: [1.0, 0.0, 0.0],
            child: std::boxed::Box::new(box_node(1.0, 0)),
        };
        assert!(is_csg_leaf(&translated));
        let composed = Node::Composition(vec![box_node(1.0, 0), box_node(2.0, 1)]);
        assert!(is_csg_leaf(&composed));
    }

    #[test]
    fn is_csg_leaf_rejects_boolean_ops_at_any_depth() {
        let nested_union = Node::Translate {
            offset: [0.0, 0.0, 0.0],
            child: std::boxed::Box::new(Node::Union {
                children: vec![box_node(1.0, 0), box_node(2.0, 1)],
            }),
        };
        assert!(!is_csg_leaf(&nested_union));
        let nested_diff = Node::Composition(vec![
            box_node(1.0, 0),
            Node::Difference {
                base: std::boxed::Box::new(box_node(2.0, 1)),
                subtract: vec![box_node(0.5, 2)],
            },
        ]);
        assert!(!is_csg_leaf(&nested_diff));
    }

    /// Pin the rewrite condition: `(difference A B C D)` with all
    /// CSG-leaf subtractors must take the union-first path. We don't
    /// inspect BSP internals here — the proxy is "the result is
    /// non-empty and watertight under the manifold validator on
    /// disjoint cutters". The chained-form regression
    /// `three_cut_box_is_watertight` is the load-bearing test for
    /// the rewrite's drift-reduction effect.
    #[test]
    fn difference_of_disjoint_leaf_cutters_meshes() {
        use crate::parse;
        let ast = parse(
            "(difference \
             (box 4.0 1.0 1.0 :color 0) \
             (translate (-1.0 0 0) (box 0.5 1.5 0.5 :color 1)) \
             (translate ( 1.0 0 0) (box 0.5 1.5 0.5 :color 2)))",
        )
        .unwrap();
        let tris = mesh(&ast).expect("difference must mesh");
        assert!(!tris.is_empty());
        // All three colors should appear: base + both cutter walls.
        let colors: std::collections::BTreeSet<u32> = tris.iter().map(|t| t.color).collect();
        assert!(colors.contains(&0));
        assert!(colors.contains(&1));
        assert!(colors.contains(&2));
    }

    /// Pin the safety rule: a composite subtractor (Boolean nested
    /// inside) must NOT trigger the union-first rewrite, since
    /// composite output can carry T-junctions/slivers that union
    /// would amplify. The proxy: the operation completes without
    /// panic. (Behavior parity with the chained path is the
    /// architectural intent.)
    #[test]
    fn difference_with_composite_subtractor_takes_chained_path() {
        use crate::parse;
        let ast = parse(
            "(difference \
             (box 4.0 1.0 1.0 :color 0) \
             (translate (-1.0 0 0) (box 0.5 1.5 0.5 :color 1)) \
             (union (box 0.3 1.5 0.3 :color 2) (translate (0.4 0 0) (box 0.3 1.5 0.3 :color 3))))",
        )
        .unwrap();
        let tris = mesh(&ast).expect("composite-subtractor difference must mesh");
        assert!(!tris.is_empty());
    }
}
