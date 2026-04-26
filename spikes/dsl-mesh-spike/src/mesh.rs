//! Mesh a typed AST into a triangle list.
//!
//! Implemented: `box`, `lathe`, `torus`, `sweep` (with optional
//! per-waypoint `:scales` and parallel-transport framing),
//! `composition`, `translate`, `rotate`, `scale`. Unsupported nodes
//! return `MeshError::NotYetImplemented` so the DSL fails loudly
//! rather than silently producing wrong geometry. `cylinder`, `cone`,
//! `wedge`, `sphere`, `extrude`, `mirror`, `array` are still pending.
//!
//! Convention: every primitive winds CCW from outside (normal =
//! `(b - a) × (c - a)` points outward). Verified by the
//! face-normal-direction test for each primitive.

use crate::ast::Node;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Triangle {
    pub vertices: [[f32; 3]; 3],
    pub color: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    #[error("node kind not yet supported by mesher iteration 1: {0}")]
    NotYetImplemented(&'static str),
}

pub fn mesh(node: &Node) -> Result<Vec<Triangle>, MeshError> {
    let mut tris = Vec::new();
    mesh_into(&mut tris, node, [0.0, 0.0, 0.0])?;
    Ok(tris)
}

fn mesh_into(out: &mut Vec<Triangle>, node: &Node, offset: [f32; 3]) -> Result<(), MeshError> {
    match node {
        Node::Box { x, y, z, color } => {
            mesh_box(out, *x, *y, *z, *color, offset);
            Ok(())
        }
        Node::Lathe {
            profile,
            segments,
            color,
        } => {
            mesh_lathe(out, profile, *segments, *color, offset);
            Ok(())
        }
        Node::Torus {
            major_radius,
            minor_radius,
            major_segments,
            minor_segments,
            color,
        } => {
            mesh_torus(
                out,
                *major_radius,
                *minor_radius,
                *major_segments,
                *minor_segments,
                *color,
                offset,
            );
            Ok(())
        }
        Node::Sweep {
            profile,
            path,
            scales,
            color,
        } => {
            mesh_sweep(out, profile, path, scales.as_deref(), *color, offset);
            Ok(())
        }
        Node::Composition(children) => {
            for child in children {
                mesh_into(out, child, offset)?;
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
            mesh_into(out, child, combined)
        }
        Node::Rotate { axis, angle, child } => {
            // Mesh the child at origin, rotate every vertex around the
            // axis (Rodrigues), then apply the inherited translation.
            // This composes correctly for nested transforms because each
            // recursive call freshly accumulates its own state.
            let mut local = Vec::new();
            mesh_into(&mut local, child, [0.0, 0.0, 0.0])?;
            let n = normalize_or_default(*axis, [0.0, 1.0, 0.0]);
            for mut tri in local {
                for v in tri.vertices.iter_mut() {
                    let r = rotate_axis_angle(*v, n, *angle);
                    *v = [r[0] + offset[0], r[1] + offset[1], r[2] + offset[2]];
                }
                out.push(tri);
            }
            Ok(())
        }
        Node::Scale { factor, child } => {
            // Mesh the child at origin, scale per-axis, then translate.
            // Combine with translate-to-pivot/translate-back for
            // "scale around a pivot point" composition.
            let mut local = Vec::new();
            mesh_into(&mut local, child, [0.0, 0.0, 0.0])?;
            for mut tri in local {
                for v in tri.vertices.iter_mut() {
                    *v = [
                        v[0] * factor[0] + offset[0],
                        v[1] * factor[1] + offset[1],
                        v[2] * factor[2] + offset[2],
                    ];
                }
                out.push(tri);
            }
            Ok(())
        }
        Node::Cylinder { .. } => Err(MeshError::NotYetImplemented("cylinder")),
        Node::Cone { .. } => Err(MeshError::NotYetImplemented("cone")),
        Node::Wedge { .. } => Err(MeshError::NotYetImplemented("wedge")),
        Node::Sphere { .. } => Err(MeshError::NotYetImplemented("sphere")),
        Node::Extrude { .. } => Err(MeshError::NotYetImplemented("extrude")),
        Node::Mirror { .. } => Err(MeshError::NotYetImplemented("mirror")),
        Node::Array { .. } => Err(MeshError::NotYetImplemented("array")),
    }
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
    // If `scales` was supplied with the wrong length, treat as
    // unspecified rather than panic — the spike's job is to be lenient
    // while the syntax matures. Real ADR enforcement comes at promotion.
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
