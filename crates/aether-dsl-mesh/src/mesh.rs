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
use aether_math::Vec3;
use csg::plane::Plane3;
use csg::point::Point3;
use csg::polygon::Polygon as CsgPolygon;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Triangle {
    pub vertices: [Vec3; 3],
    pub color: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    #[error("node kind not yet supported by mesher iteration 1: {0}")]
    NotYetImplemented(&'static str),
    #[error("CSG operation failed: {0}")]
    Csg(#[from] csg::CsgError),
}

/// Wire entry: evaluate `node` polygon-domain, run the cleanup +
/// CDT-tessellation pipeline, then fan back to wire `Triangle`s.
///
/// Runs [`crate::simplify::simplify`] as a pre-pass, so AABB-disjoint
/// CSG subexpressions and identity transforms collapse before they
/// reach the mesher. The simplified AST is semantically equivalent to
/// the input — every active rewrite preserves the meshed output
/// exactly — so callers see no behavior change beyond the speedup.
///
/// Cleanup runs **once** at the root, not after every CSG op — chained
/// `(difference A B C)` flows raw BSP polygons between steps and
/// triangulates a single time at the very end. Skipping the per-op
/// triangle round-trip is also what avoids the sliver-normal flip bug
/// (`from_triangle` re-deriving the plane on a sliver triangle picks
/// up the wrong sign for `n_z`).
pub fn mesh(node: &Node) -> Result<Vec<Triangle>, MeshError> {
    let simplified = crate::simplify::simplify(node);
    let mut polys = Vec::new();
    mesh_into_polygons(&mut polys, &simplified, Vec3::ZERO)?;
    Ok(csg::polygons_to_triangles(&csg::tessellate::run(polys)))
}

/// Polygon-domain entry: same composition as [`mesh`], but stops at
/// the n-gon boundary loops cleanup produces (no triangulation). The
/// public polygon API in `crate::polygon` is the consumer.
pub fn mesh_polygons_internal(node: &Node) -> Result<Vec<CsgPolygon>, MeshError> {
    let simplified = crate::simplify::simplify(node);
    let mut polys = Vec::new();
    mesh_into_polygons(&mut polys, &simplified, Vec3::ZERO)?;
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
    offset: Vec3,
) -> Result<(), MeshError> {
    match node {
        Node::Box { x, y, z, color } => mesh_box(out, *x, *y, *z, *color, offset),
        Node::Lathe {
            profile,
            segments,
            color,
        } => mesh_lathe(out, profile, *segments, *color, offset),
        Node::LatheSegment {
            profile,
            segments,
            segment_index,
            color,
        } => mesh_lathe_segment(out, profile, *segments, *segment_index, *color, offset),
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
        } => mesh_into_polygons(out, child, offset + *delta),
        Node::Rotate { axis, angle, child } => {
            let mut local = Vec::new();
            mesh_into_polygons(&mut local, child, Vec3::ZERO)?;
            let n = axis.normalize_or(Vec3::Y);
            for poly in &local {
                if let Some(transformed) =
                    transform_polygon(poly, |v| v.rotate_axis_angle(n, *angle) + offset)?
                {
                    out.push(transformed);
                }
            }
            Ok(())
        }
        Node::Scale { factor, child } => {
            let mut local = Vec::new();
            mesh_into_polygons(&mut local, child, Vec3::ZERO)?;
            for poly in &local {
                if let Some(transformed) = transform_polygon(poly, |v| {
                    Vec3::new(
                        v.x * factor.x + offset.x,
                        v.y * factor.y + offset.y,
                        v.z * factor.z + offset.z,
                    )
                })? {
                    out.push(transformed);
                }
            }
            Ok(())
        }
        Node::Mirror { axis, child } => {
            let mut local = Vec::new();
            mesh_into_polygons(&mut local, child, Vec3::ZERO)?;
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
            mesh_into_polygons(&mut local, child, Vec3::ZERO)?;
            for i in 0..*count {
                let delta = offset + *spacing * (i as f32);
                for poly in &local {
                    if let Some(translated) = transform_polygon(poly, |v| v + delta)? {
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
                // Concatenation only equals union when cutters are
                // disjoint; for overlapping cutters the polygons inside
                // the overlap region survive into the BSP as interior
                // walls (issue #341). Run the cutters through union_raw
                // pairwise so the combined cutter is a true CSG union
                // before the single base-difference pass.
                let mut iter = subtract.iter();
                let mut combined = Vec::new();
                mesh_into_polygons(&mut combined, iter.next().unwrap(), offset)?;
                for s in iter {
                    let mut s_polys = Vec::new();
                    mesh_into_polygons(&mut s_polys, s, offset)?;
                    combined = csg::ops::union_raw(combined, s_polys)?;
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
pub(crate) fn is_csg_leaf(node: &Node) -> bool {
    match node {
        Node::Box { .. }
        | Node::Cylinder { .. }
        | Node::Cone { .. }
        | Node::Wedge { .. }
        | Node::Sphere { .. }
        | Node::Lathe { .. }
        | Node::LatheSegment { .. }
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

fn point_from_f32(v: Vec3) -> Result<Point3, MeshError> {
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
    verts: &[Vec3],
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
    F: Fn(Vec3) -> Vec3,
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
    offset: Vec3,
) -> Result<Option<CsgPolygon>, MeshError> {
    let mut new_verts = Vec::with_capacity(poly.vertices.len());
    for v in &poly.vertices {
        let f = v.to_f32();
        let m = match axis {
            Axis::X => Vec3::new(-f.x, f.y, f.z),
            Axis::Y => Vec3::new(f.x, -f.y, f.z),
            Axis::Z => Vec3::new(f.x, f.y, -f.z),
        };
        new_verts.push(point_from_f32(m + offset)?);
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
    offset: Vec3,
) -> Result<(), MeshError> {
    let hx = x * 0.5;
    let hy = y * 0.5;
    let hz = z * 0.5;
    let Vec3 {
        x: ox,
        y: oy,
        z: oz,
    } = offset;

    // Eight corners, named by sign per axis.
    let nnn = Vec3::new(ox - hx, oy - hy, oz - hz);
    let pnn = Vec3::new(ox + hx, oy - hy, oz - hz);
    let npn = Vec3::new(ox - hx, oy + hy, oz - hz);
    let ppn = Vec3::new(ox + hx, oy + hy, oz - hz);
    let nnp = Vec3::new(ox - hx, oy - hy, oz + hz);
    let pnp = Vec3::new(ox + hx, oy - hy, oz + hz);
    let npp = Vec3::new(ox - hx, oy + hy, oz + hz);
    let ppp = Vec3::new(ox + hx, oy + hy, oz + hz);

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
    offset: Vec3,
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
    let revolve = |radius: f32, height: f32, i: usize| -> Vec3 {
        let (cos, sin) = cos_sin[i % segments];
        Vec3::new(
            offset.x + radius * cos,
            offset.y + height,
            offset.z + radius * sin,
        )
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

/// Mesh one angular slice of a lathe as a closed solid (per
/// [`Node::LatheSegment`]). The slice has `(profile.len() - 1)` outer
/// quads (matching the existing lathe convention) plus two radial
/// walls — one at `θ_start`, one at `θ_end` — that close the wedge
/// off against the lathe axis.
///
/// Caller's contract: `profile.first().x == 0` AND `profile.last().x ==
/// 0` (axis-closed). The simplify rewrite that creates `LatheSegment`
/// nodes enforces this; constructing one directly from a non-closed
/// profile silently produces a non-watertight mesh because the radial
/// walls won't close at the axis.
fn mesh_lathe_segment(
    out: &mut Vec<CsgPolygon>,
    profile: &[[f32; 2]],
    segments: u32,
    segment_index: u32,
    color: u32,
    offset: Vec3,
) -> Result<(), MeshError> {
    if profile.len() < 2 || segments < 3 || segment_index >= segments {
        return Ok(());
    }
    let two_pi = std::f32::consts::TAU;
    let theta_start = two_pi * (segment_index as f32) / (segments as f32);
    let theta_end = two_pi * ((segment_index + 1) as f32) / (segments as f32);
    let cos_s = theta_start.cos();
    let sin_s = theta_start.sin();
    let cos_e = theta_end.cos();
    let sin_e = theta_end.sin();

    let revolve = |r: f32, y: f32, cos_t: f32, sin_t: f32| -> Vec3 {
        Vec3::new(offset.x + r * cos_t, offset.y + y, offset.z + r * sin_t)
    };

    // Outer surface: one quad per profile edge, restricted to the
    // angular slice [θ_start, θ_end]. Same (a, b, d, c) winding as
    // mesh_lathe so the radial-outward normal points away from the axis.
    for k in 0..profile.len() - 1 {
        let r0 = profile[k][0];
        let y0 = profile[k][1];
        let r1 = profile[k + 1][0];
        let y1 = profile[k + 1][1];
        let a = revolve(r0, y0, cos_s, sin_s);
        let b = revolve(r1, y1, cos_s, sin_s);
        let c = revolve(r0, y0, cos_e, sin_e);
        let d = revolve(r1, y1, cos_e, sin_e);
        push_polygon_from_f32(out, &[a, b, d, c], color)?;
    }

    // Radial wall at θ_start: the profile rotated to that plane,
    // walked in REVERSED order so the polygon's normal points in the
    // -θ direction (away from the wedge volume). For axis-closed
    // profiles, the first and last vertices coincide at the axis;
    // push_polygon_from_f32 dedupes consecutive identical vertices and
    // drops a trailing duplicate, so the polygon naturally closes.
    let wall_start: Vec<Vec3> = profile
        .iter()
        .rev()
        .map(|p| revolve(p[0], p[1], cos_s, sin_s))
        .collect();
    push_polygon_from_f32(out, &wall_start, color)?;

    // Radial wall at θ_end: forward profile order, normal points in the
    // +θ direction.
    let wall_end: Vec<Vec3> = profile
        .iter()
        .map(|p| revolve(p[0], p[1], cos_e, sin_e))
        .collect();
    push_polygon_from_f32(out, &wall_end, color)?;
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
    offset: Vec3,
) -> Result<(), MeshError> {
    if major_segments < 3 || minor_segments < 3 {
        return Ok(());
    }
    let m = major_segments as usize;
    let n = minor_segments as usize;
    let two_pi = std::f32::consts::TAU;
    // P(i, j) = vertex at major angle α_i, minor angle β_j.
    let position = |i: usize, j: usize| -> Vec3 {
        let alpha = two_pi * (i as f32) / (m as f32);
        let beta = two_pi * (j as f32) / (n as f32);
        let cos_a = alpha.cos();
        let sin_a = alpha.sin();
        let cos_b = beta.cos();
        let sin_b = beta.sin();
        let r = major_radius + minor_radius * cos_b;
        Vec3::new(
            offset.x + r * cos_a,
            offset.y + minor_radius * sin_b,
            offset.z + r * sin_a,
        )
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
    path: &[Vec3],
    scales: Option<&[f32]>,
    color: u32,
    offset: Vec3,
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
    let mut tangents: Vec<Vec3> = Vec::with_capacity(path.len());
    for k in 0..path.len() {
        let prev = if k == 0 { path[k] } else { path[k - 1] };
        let next = if k == path.len() - 1 {
            path[k]
        } else {
            path[k + 1]
        };
        tangents.push((next - prev).normalize_or(Vec3::Z));
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
    let mut rings: Vec<Vec<Vec3>> = Vec::with_capacity(path.len());
    let t0 = tangents[0];
    let up_ref = if t0.y.abs() > 0.95 { Vec3::X } else { Vec3::Y };
    let mut r = up_ref.cross(t0).normalize_or(Vec3::X);
    let mut u = t0.cross(r);
    for (k, p) in path.iter().enumerate() {
        let t = tangents[k];
        if k > 0 {
            let prev_t = tangents[k - 1];
            let axis = prev_t.cross(t);
            if axis.length_squared() > 1e-12 {
                let angle = prev_t.dot(t).clamp(-1.0, 1.0).acos();
                let axis_n = axis.normalize_or(Vec3::Y);
                r = r.rotate_axis_angle(axis_n, angle);
                u = u.rotate_axis_angle(axis_n, angle);
            }
        }
        let scale = scales.map(|s| s[k]).unwrap_or(1.0);
        let p_world = offset + *p;
        let mut ring = Vec::with_capacity(n);
        for pt in profile {
            ring.push(p_world + r * (pt[0] * scale) + u * (pt[1] * scale));
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

/// Cylinder of `radius` and total `height`, centered on the Y axis at
/// `offset`. Implemented as a lathe of a 4-point profile so the side
/// + cap winding matches the rest of the lathed primitives.
fn mesh_cylinder(
    out: &mut Vec<CsgPolygon>,
    radius: f32,
    height: f32,
    segments: u32,
    color: u32,
    offset: Vec3,
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
    offset: Vec3,
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
    offset: Vec3,
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
    offset: Vec3,
) -> Result<(), MeshError> {
    let hx = x * 0.5;
    let hy = y * 0.5;
    let hz = z * 0.5;
    let Vec3 {
        x: ox,
        y: oy,
        z: oz,
    } = offset;
    let a = Vec3::new(ox - hx, oy - hy, oz - hz); // back-bottom-left
    let b = Vec3::new(ox + hx, oy - hy, oz - hz); // back-bottom-right
    let c = Vec3::new(ox - hx, oy - hy, oz + hz); // front-bottom-left
    let d = Vec3::new(ox + hx, oy - hy, oz + hz); // front-bottom-right
    let e = Vec3::new(ox - hx, oy + hy, oz - hz); // back-top-left
    let f = Vec3::new(ox + hx, oy + hy, oz - hz); // back-top-right

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
    offset: Vec3,
) -> Result<(), MeshError> {
    if profile.len() < 3 || depth <= 0.0 {
        return Ok(());
    }
    let n = profile.len();
    let Vec3 {
        x: ox,
        y: oy,
        z: oz,
    } = offset;
    let base = |i: usize| -> Vec3 { Vec3::new(ox + profile[i][0], oy + profile[i][1], oz) };
    let top = |i: usize| -> Vec3 { Vec3::new(ox + profile[i][0], oy + profile[i][1], oz + depth) };

    // Side walls. For edge p_i → p_{i+1}, the quad corners CCW from
    // outside are (base i, base i+1, top i+1, top i).
    for i in 0..n {
        let j = (i + 1) % n;
        push_polygon_from_f32(out, &[base(i), base(j), top(j), top(i)], color)?;
    }

    // Back cap (z = depth, normal +Z): the profile in original CCW
    // winding becomes one n-gon. Front cap (z = 0, normal -Z) is the
    // same loop reversed.
    let back_cap: Vec<Vec3> = (0..n).map(top).collect();
    push_polygon_from_f32(out, &back_cap, color)?;
    let front_cap: Vec<Vec3> = (0..n).rev().map(base).collect();
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
            offset: Vec3::new(1.0, 0.0, 0.0),
            child: std::boxed::Box::new(box_node(1.0, 0)),
        };
        assert!(is_csg_leaf(&translated));
        let composed = Node::Composition(vec![box_node(1.0, 0), box_node(2.0, 1)]);
        assert!(is_csg_leaf(&composed));
    }

    #[test]
    fn is_csg_leaf_rejects_boolean_ops_at_any_depth() {
        let nested_union = Node::Translate {
            offset: Vec3::new(0.0, 0.0, 0.0),
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

    /// Regression for issue #341: the N-ary fast path must true-union
    /// overlapping cutters, not concatenate them. Two perpendicular
    /// cylindrical bores share volume at the cube center; under the
    /// concatenating bug the overlap-region polygons survived as
    /// interior walls and `(difference box cyl1 cyl2)` produced wrong
    /// geometry vs the explicit `(difference box (union cyl1 cyl2))`
    /// form. With the fix the two forms must mesh identically.
    #[test]
    fn n_ary_difference_with_overlapping_cutters_matches_explicit_union() {
        use crate::parse;
        let n_ary = parse(
            "(difference \
             (box 2 2 2 :color 5) \
             (cylinder 0.4 3 12 :color 3) \
             (rotate (1 0 0) 1.5707963 (cylinder 0.4 3 12 :color 3)))",
        )
        .unwrap();
        let explicit_union = parse(
            "(difference \
             (box 2 2 2 :color 5) \
             (union \
              (cylinder 0.4 3 12 :color 3) \
              (rotate (1 0 0) 1.5707963 (cylinder 0.4 3 12 :color 3))))",
        )
        .unwrap();
        let mut from_n_ary = mesh(&n_ary).expect("n-ary difference must mesh");
        let mut from_union = mesh(&explicit_union).expect("explicit-union difference must mesh");
        let key = |t: &Triangle| {
            let mut buf: Vec<u8> = Vec::with_capacity(40);
            for v in &t.vertices {
                for c in [v.x, v.y, v.z] {
                    buf.extend_from_slice(&c.to_le_bytes());
                }
            }
            buf.extend_from_slice(&t.color.to_le_bytes());
            buf
        };
        from_n_ary.sort_by_key(key);
        from_union.sort_by_key(key);
        assert_eq!(from_n_ary, from_union);
    }

    /// End-to-end equivalence for the disjoint-union rewrite: a union
    /// of three spatially separate boxes meshes to *exactly* the same
    /// triangle set as a hand-authored composition of the same three
    /// boxes. Verifies that the rewrite doesn't drop, duplicate, or
    /// reorder geometry on the disjoint path.
    #[test]
    fn disjoint_three_box_union_matches_composition() {
        use crate::parse;
        let union_ast = parse(
            "(union \
             (box 1 1 1 :color 0) \
             (translate (10 0 0) (box 1 1 1 :color 1)) \
             (translate (20 0 0) (box 1 1 1 :color 2)))",
        )
        .unwrap();
        let comp_ast = parse(
            "(composition \
             (box 1 1 1 :color 0) \
             (translate (10 0 0) (box 1 1 1 :color 1)) \
             (translate (20 0 0) (box 1 1 1 :color 2)))",
        )
        .unwrap();
        let mut from_union = mesh(&union_ast).unwrap();
        let mut from_comp = mesh(&comp_ast).unwrap();
        // Order may differ between paths; sort for comparison. The
        // triangle bytes are bit-identical otherwise — both flow the
        // same polygon list through the same root cleanup pass.
        let key = |t: &Triangle| {
            let mut buf: Vec<u8> = Vec::with_capacity(40);
            for v in &t.vertices {
                for c in [v.x, v.y, v.z] {
                    buf.extend_from_slice(&c.to_le_bytes());
                }
            }
            buf.extend_from_slice(&t.color.to_le_bytes());
            buf
        };
        from_union.sort_by_key(key);
        from_comp.sort_by_key(key);
        assert_eq!(from_union, from_comp);
    }

    /// End-to-end equivalence for the difference-distribution rewrite:
    /// a `(difference (union A B) Y)` where Y only touches A produces
    /// the same triangle set as the hand-distributed
    /// `(composition (difference A Y) B)`. Verifies the rewrite chain
    /// (distribute → AABB-prune-arms → disjoint-union-to-composition)
    /// preserves geometry.
    #[test]
    fn difference_over_disjoint_arm_union_matches_hand_distribution() {
        use crate::parse;
        let auto = parse(
            "(difference \
             (union (box 4 4 4 :color 0) (translate (20 0 0) (box 4 4 4 :color 1))) \
             (box 1 1 1 :color 9))",
        )
        .unwrap();
        let manual = parse(
            "(composition \
             (difference (box 4 4 4 :color 0) (box 1 1 1 :color 9)) \
             (translate (20 0 0) (box 4 4 4 :color 1)))",
        )
        .unwrap();
        let mut from_auto = mesh(&auto).unwrap();
        let mut from_manual = mesh(&manual).unwrap();
        let key = |t: &Triangle| {
            let mut buf: Vec<u8> = Vec::with_capacity(40);
            for v in &t.vertices {
                for c in [v.x, v.y, v.z] {
                    buf.extend_from_slice(&c.to_le_bytes());
                }
            }
            buf.extend_from_slice(&t.color.to_le_bytes());
            buf
        };
        from_auto.sort_by_key(key);
        from_manual.sort_by_key(key);
        assert_eq!(from_auto, from_manual);
    }

    /// Pin: an *overlapping* union still goes through the BSP path.
    /// Two boxes that share volume must mesh to a smaller boundary
    /// (the merged surface) than the same boxes composed flatly,
    /// which would emit both their full surfaces with internal walls.
    #[test]
    fn overlapping_two_box_union_still_runs_csg() {
        use crate::parse;
        let union_ast = parse(
            "(union \
             (box 2 2 2 :color 0) \
             (translate (1 0 0) (box 2 2 2 :color 1)))",
        )
        .unwrap();
        let comp_ast = parse(
            "(composition \
             (box 2 2 2 :color 0) \
             (translate (1 0 0) (box 2 2 2 :color 1)))",
        )
        .unwrap();
        let from_union = mesh(&union_ast).unwrap();
        let from_comp = mesh(&comp_ast).unwrap();
        // CSG-merged surface is strictly smaller — the shared volume's
        // internal walls don't survive. If this assertion ever fires,
        // the disjoint-rewrite has misclassified an overlap as
        // disjoint and degraded the output.
        assert!(
            from_union.len() < from_comp.len(),
            "union ({}) should produce fewer triangles than composition ({}) when inputs overlap",
            from_union.len(),
            from_comp.len()
        );
    }

    /// Pin: a single LatheSegment meshes to a non-empty closed solid
    /// when the profile is axis-closed. Per-segment basic correctness
    /// — the wedge has outer quads + two radial walls.
    #[test]
    fn lathe_segment_meshes_to_closed_solid() {
        let n = Node::LatheSegment {
            profile: vec![[0.0, -0.5], [0.5, -0.5], [0.5, 0.5], [0.0, 0.5]],
            segments: 16,
            segment_index: 3,
            color: 7,
        };
        let tris = mesh(&n).expect("lathe segment must mesh");
        assert!(!tris.is_empty(), "wedge produced no triangles");
        // Color preserved through the whole chain.
        assert!(tris.iter().all(|t| t.color == 7));
    }

    /// End-to-end equivalence: a wedge-decomposed lathe must produce
    /// the same logical surface (watertight, identical color) as the
    /// non-decomposed lathe. We compare polygon counts loosely (the
    /// BSP-union of segments may produce a slightly different
    /// triangulation than the direct lathe), but the geometric
    /// validators in `regression.rs` are the load-bearing equivalence
    /// guarantee — pinning here just catches regressions where the
    /// decomposed lathe collapses to nothing or doubles up.
    #[test]
    fn decomposed_lathe_meshes_to_non_empty_solid() {
        use crate::parse;
        let ast = parse("(lathe ((0 -0.5) (0.5 -0.5) (0.5 0.5) (0 0.5)) 16 :color 3)").unwrap();
        let tris = mesh(&ast).expect("lathe must mesh");
        assert!(!tris.is_empty(), "lathe produced no triangles");
        assert!(tris.iter().all(|t| t.color == 3));
    }

    /// Pin: a non-axis-closed lathe profile (open ends) is NOT
    /// decomposed — the wedge rewrite requires both endpoints at
    /// `r == 0` to close the radial walls without an explicit cap.
    /// The rewrite skips this case; the lathe still meshes via the
    /// original whole-lathe path.
    #[test]
    fn open_profile_lathe_skips_decomposition_and_meshes() {
        use crate::parse;
        // Profile starts at (0.5, ...) — not axis-closed.
        let ast = parse("(lathe ((0.5 -0.5) (0.5 0.5)) 16 :color 5)").unwrap();
        let tris = mesh(&ast).expect("open-profile lathe must mesh");
        // Open profile produces an open surface (not watertight) but
        // should still emit triangles for the cylindrical band.
        assert!(!tris.is_empty(), "open lathe produced no triangles");
    }
}
