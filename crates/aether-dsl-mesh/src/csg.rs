//! BSP-CSG mesher for the `union` / `intersection` / `difference`
//! operators added by ADR-0054.
//!
//! Per ADR-0054, the algorithm is single-threaded BSP-CSG (Thibault &
//! Naylor 1980) with **internal fixed-point coordinates** and **exact
//! integer-determinant predicates** — the wire `Vertex` format stays
//! `f32`, but classification ("which side of plane Q is point P on?"),
//! edge intersection, and topology decisions all run as `i64`/`i128`
//! arithmetic against snapped 16:16 fixed-point coordinates.
//!
//! Coordinates entering the CSG core must satisfy `|coord| ≤ 256`; the
//! [`fixed::f32_to_fixed`] conversion returns `Err` outside that range
//! so out-of-range geometry is a loud failure rather than silent
//! precision degradation.
//!
//! ### Layering
//!
//! - [`fixed`]: f32 ↔ 16:16 conversion + range/finiteness validation.
//! - [`point`]: integer-grid 3D point.
//! - [`plane`]: integer-coefficient plane + signed-side predicate.
//! - [`polygon`]: convex polygon over integer points + split-vs-plane.
//! - [`bsp`]: BSP tree (build / invert / clip).
//! - [`ops`]: union / intersection / difference as tree-clipping
//!   compositions; output is n-gon boundary loops.
//! - [`cleanup`]: post-CSG mesh repair (welding, coplanar merge,
//!   T-junction repair, sliver removal).
//! - [`tessellate`]: CDT triangulation for the wire `Vec<Triangle>`
//!   path. Skipped by the polygon-domain entry points.

pub mod bsp;
pub mod cleanup;
pub mod fixed;
pub mod ops;
pub mod plane;
pub mod point;
pub mod polygon;
pub mod tessellate;

use crate::csg::fixed::FixedError;
use crate::csg::polygon::Polygon;
use crate::mesh::Triangle;

#[derive(Debug, thiserror::Error)]
pub enum CsgError {
    #[error("CSG fixed-point conversion: {0}")]
    Fixed(#[from] FixedError),
    /// BSP recursion exceeded the safety depth limit. Indicates either
    /// pathological input or a residual snap-drift cascade that the
    /// per-plane tolerance in `Plane3::coplanar_threshold` didn't catch.
    /// Loud failure — the caller gets an error rather than a stack
    /// overflow.
    #[error(
        "CSG BSP recursion exceeded depth limit ({limit}); likely a snap-drift cascade not caught by the side-test tolerance"
    )]
    RecursionLimit { limit: usize },
}

/// Fan-triangulate a polygon list back to wire `Triangle`s. Polygons
/// emerging from BSP clipping are convex (each input is a triangle, and
/// convex × half-space remains convex), so fan triangulation is
/// well-defined. Cleanup-pass output (n-gon loops or CDT triangles)
/// also satisfies convexity per loop.
pub(crate) fn polygons_to_triangles(polys: &[Polygon]) -> Vec<Triangle> {
    let mut tris = Vec::new();
    for poly in polys {
        if poly.vertices.len() < 3 {
            continue;
        }
        // Fan-triangulate. Polygons emerging from BSP clipping are
        // convex (each input is a triangle, and convex × half-space
        // remains convex), so the simple fan is well-defined.
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
