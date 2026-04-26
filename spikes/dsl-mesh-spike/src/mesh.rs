//! Mesh a typed AST into a triangle list.
//!
//! Iteration 1 supports `box`, `composition`, and `translate` only — enough
//! to validate the meshing concept and produce a viewable artifact for a
//! cube-with-translates scene. Other primitives and transforms return
//! `MeshError::NotYetImplemented` so unsupported scenes fail loudly rather
//! than silently producing wrong geometry.

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
        Node::Cylinder { .. } => Err(MeshError::NotYetImplemented("cylinder")),
        Node::Cone { .. } => Err(MeshError::NotYetImplemented("cone")),
        Node::Wedge { .. } => Err(MeshError::NotYetImplemented("wedge")),
        Node::Sphere { .. } => Err(MeshError::NotYetImplemented("sphere")),
        Node::Extrude { .. } => Err(MeshError::NotYetImplemented("extrude")),
        Node::Rotate { .. } => Err(MeshError::NotYetImplemented("rotate")),
        Node::Scale { .. } => Err(MeshError::NotYetImplemented("scale")),
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
