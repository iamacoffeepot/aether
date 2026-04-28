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
//! Boolean composition was retired by ADR-0062; the prior implementation
//! lives on `archive/csg-bsp`.
//!
//! Convention: every primitive winds CCW from outside (normal =
//! `(b - a) × (c - a)` points outward). Verified by per-primitive
//! face-normal-direction tests.

use crate::ast::{Axis, Node};
use crate::fixed::FixedError;
use crate::loop_polygon::Polygon as LoopPolygon;
use crate::plane::Plane3;
use crate::point::Point3;
use aether_math::Vec3;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Triangle {
    pub vertices: [Vec3; 3],
    pub color: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    #[error("node kind not yet supported by mesher iteration 1: {0}")]
    NotYetImplemented(&'static str),
    #[error("mesh coordinate out of fixed-point range: {0}")]
    OutOfRange(#[from] FixedError),
}

/// Wire entry: evaluate `node` polygon-domain, run the cleanup +
/// CDT-tessellation pipeline, then fan back to wire `Triangle`s.
///
/// Runs [`crate::simplify::simplify`] as a pre-pass so identity
/// transforms collapse before they reach the mesher.
pub fn mesh(node: &Node) -> Result<Vec<Triangle>, MeshError> {
    let simplified = crate::simplify::simplify(node);
    let mut polys = Vec::new();
    mesh_into_polygons(&mut polys, &simplified, Vec3::ZERO)?;
    Ok(polygons_to_triangles(&crate::tessellate::run(polys)))
}

/// Polygon-domain entry: same composition as [`mesh`] but stops at the
/// n-gon boundary loops cleanup produces (no triangulation). The public
/// polygon API in `crate::polygon` is the consumer.
pub fn mesh_polygons_internal(node: &Node) -> Result<Vec<LoopPolygon>, MeshError> {
    let simplified = crate::simplify::simplify(node);
    let mut polys = Vec::new();
    mesh_into_polygons(&mut polys, &simplified, Vec3::ZERO)?;
    Ok(crate::cleanup::run_to_loops(polys))
}

/// Fan-triangulate a convex polygon list to wire `Triangle`s. Each
/// primitive's mesher emits convex polygons; cleanup-pass output (n-gon
/// loops or CDT triangles) also satisfies convexity per loop.
fn polygons_to_triangles(polys: &[LoopPolygon]) -> Vec<Triangle> {
    let mut tris = Vec::new();
    for poly in polys {
        if poly.vertices.len() < 3 {
            continue;
        }
        let v0 = poly.vertices[0].to_f32();
        for i in 1..poly.vertices.len() - 1 {
            let v1 = poly.vertices[i].to_f32();
            let v2 = poly.vertices[i + 1].to_f32();
            tris.push(Triangle {
                vertices: [v0, v1, v2],
                color: poly.color,
            });
        }
    }
    tris
}

/// Recursive AST evaluator in polygon domain. Primitives emit n-gon
/// polygons directly; structural ops walk children.
fn mesh_into_polygons(
    out: &mut Vec<LoopPolygon>,
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
            open,
            color,
        } => mesh_sweep(out, profile, path, scales.as_deref(), *open, *color, offset),
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
    }
}

fn point_from_f32(v: Vec3) -> Result<Point3, MeshError> {
    Point3::from_f32(v).map_err(MeshError::OutOfRange)
}

/// Build a [`LoopPolygon`] from an n-gon vertex list (n ≥ 3) and push it
/// to `out`. Consecutive duplicate vertices are deduped (axis-collapse
/// from primitives like lathe / sphere-pole rings collapses naturally).
/// Plane is re-derived via the robust non-collinear-triple search;
/// degenerate polygons are silently skipped. Out-of-range vertices
/// surface as [`MeshError::OutOfRange`] — loud failure at the ±256 unit
/// boundary.
fn push_polygon_from_f32(
    out: &mut Vec<LoopPolygon>,
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
    out.push(LoopPolygon {
        vertices: points,
        plane,
        color,
    });
    Ok(())
}

/// Apply `xform` to every vertex of `poly`, re-derive the plane from
/// three non-collinear transformed vertices.
fn transform_polygon<F>(poly: &LoopPolygon, xform: F) -> Result<Option<LoopPolygon>, MeshError>
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
    Ok(Some(LoopPolygon {
        vertices: new_verts,
        plane,
        color: poly.color,
    }))
}

/// Find three non-collinear vertices in `verts` and return the plane
/// they define, or `None` if the polygon is fully degenerate.
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
    poly: &LoopPolygon,
    axis: Axis,
    offset: Vec3,
) -> Result<Option<LoopPolygon>, MeshError> {
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
    Ok(Some(LoopPolygon {
        vertices: new_verts,
        plane,
        color: poly.color,
    }))
}

/// Emit 6 quad faces for an axis-aligned box of size `(x, y, z)`
/// centered at `(0, 0, 0)` then translated by `offset`. Faces wound CCW
/// from outside.
fn mesh_box(
    out: &mut Vec<LoopPolygon>,
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
/// `x == 0` collapse to a single axis point — quads degenerate to
/// triangles, filling caps and apex points without a separate pass.
fn mesh_lathe(
    out: &mut Vec<LoopPolygon>,
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

/// Donut around the Y axis. Generates `major_segments × minor_segments`
/// quads on the surface.
fn mesh_torus(
    out: &mut Vec<LoopPolygon>,
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
            let a = position(i, j);
            let b = position(i_next, j);
            let c = position(i, j_next);
            let d = position(i_next, j_next);
            push_polygon_from_f32(out, &[a, c, d, b], color)?;
        }
    }
    Ok(())
}

/// Sweep a 2D `profile` polygon along a 3D `path` using a parallel-
/// transport frame. Caps are emitted by default (`open = false`).
fn mesh_sweep(
    out: &mut Vec<LoopPolygon>,
    profile: &[[f32; 2]],
    path: &[Vec3],
    scales: Option<&[f32]>,
    open: bool,
    color: u32,
    offset: Vec3,
) -> Result<(), MeshError> {
    if profile.len() < 3 || path.len() < 2 {
        return Ok(());
    }
    let scales = match scales {
        Some(s) if s.len() == path.len() => Some(s),
        _ => None,
    };
    let n = profile.len();

    let profile_signed_area_2x: f64 = (0..n)
        .map(|i| {
            let j = (i + 1) % n;
            (profile[i][0] as f64) * (profile[j][1] as f64)
                - (profile[j][0] as f64) * (profile[i][1] as f64)
        })
        .sum();
    let profile_ccw = profile_signed_area_2x > 0.0;

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

    for k in 0..rings.len() - 1 {
        let r0 = &rings[k];
        let r1 = &rings[k + 1];
        for i in 0..n {
            let j = (i + 1) % n;
            let a = r0[i];
            let b = r1[i];
            let c = r0[j];
            let d = r1[j];
            let quad: [Vec3; 4] = if profile_ccw {
                [a, c, d, b]
            } else {
                [a, b, d, c]
            };
            push_polygon_from_f32(out, &quad, color)?;
        }
    }

    if !open {
        let last = rings.len() - 1;
        let start_cap: Vec<Vec3> = if profile_ccw {
            rings[0].iter().rev().copied().collect()
        } else {
            rings[0].clone()
        };
        push_polygon_from_f32(out, &start_cap, color)?;
        let end_cap: Vec<Vec3> = if profile_ccw {
            rings[last].clone()
        } else {
            rings[last].iter().rev().copied().collect()
        };
        push_polygon_from_f32(out, &end_cap, color)?;
    }
    Ok(())
}

/// Cylinder of `radius` and total `height`, centered on the Y axis at
/// `offset`. Implemented as a lathe of a 4-point profile.
fn mesh_cylinder(
    out: &mut Vec<LoopPolygon>,
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
    out: &mut Vec<LoopPolygon>,
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
/// both the number of latitude rings and the number of longitude
/// segments. Implemented as a lathe of a half-circle profile.
fn mesh_sphere(
    out: &mut Vec<LoopPolygon>,
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
/// `offset`. Faces wound CCW from outside.
fn mesh_wedge(
    out: &mut Vec<LoopPolygon>,
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
    push_polygon_from_f32(out, &[c, d, f, e], color)?; // Hypotenuse quad
    Ok(())
}

/// Extrude a 2D `profile` polygon along Z by `depth`. Generates side-
/// wall quads + two cap polygons.
///
/// **Caller's contract**: `profile` must be convex for the cap to be
/// correctly meshed as a single n-gon. Concave profiles need ear-
/// clipping (deferred).
fn mesh_extrude(
    out: &mut Vec<LoopPolygon>,
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

    for i in 0..n {
        let j = (i + 1) % n;
        push_polygon_from_f32(out, &[base(i), base(j), top(j), top(i)], color)?;
    }

    let back_cap: Vec<Vec3> = (0..n).map(top).collect();
    push_polygon_from_f32(out, &back_cap, color)?;
    let front_cap: Vec<Vec3> = (0..n).rev().map(base).collect();
    push_polygon_from_f32(out, &front_cap, color)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lathe_meshes_to_non_empty_solid() {
        use crate::parse;
        let ast = parse("(lathe ((0 -0.5) (0.5 -0.5) (0.5 0.5) (0 0.5)) 16 :color 3)").unwrap();
        let tris = mesh(&ast).expect("lathe must mesh");
        assert!(!tris.is_empty(), "lathe produced no triangles");
        assert!(tris.iter().all(|t| t.color == 3));
    }

    #[test]
    fn open_profile_lathe_meshes() {
        use crate::parse;
        let ast = parse("(lathe ((0.5 -0.5) (0.5 0.5)) 16 :color 5)").unwrap();
        let tris = mesh(&ast).expect("open-profile lathe must mesh");
        assert!(!tris.is_empty(), "open lathe produced no triangles");
    }
}
