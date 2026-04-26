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
//!   compositions.
//!
//! The top-level entry points [`union_triangles`], [`intersection_triangles`],
//! and [`difference_triangles`] take f32 [`Triangle`] lists and return
//! f32 [`Triangle`] lists — they handle the snap-in / snap-out so the
//! mesher can stay agnostic of integer arithmetic.

pub mod bsp;
pub mod fixed;
pub mod ops;
pub mod plane;
pub mod point;
pub mod polygon;

use crate::csg::fixed::FixedError;
use crate::csg::point::Point3;
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

pub fn union_triangles(a: &[Triangle], b: &[Triangle]) -> Result<Vec<Triangle>, CsgError> {
    let pa = triangles_to_polygons(a)?;
    let pb = triangles_to_polygons(b)?;
    Ok(polygons_to_triangles(&ops::union(pa, pb)?))
}

pub fn intersection_triangles(a: &[Triangle], b: &[Triangle]) -> Result<Vec<Triangle>, CsgError> {
    let pa = triangles_to_polygons(a)?;
    let pb = triangles_to_polygons(b)?;
    Ok(polygons_to_triangles(&ops::intersection(pa, pb)?))
}

pub fn difference_triangles(a: &[Triangle], b: &[Triangle]) -> Result<Vec<Triangle>, CsgError> {
    let pa = triangles_to_polygons(a)?;
    let pb = triangles_to_polygons(b)?;
    Ok(polygons_to_triangles(&ops::difference(pa, pb)?))
}

fn triangles_to_polygons(triangles: &[Triangle]) -> Result<Vec<Polygon>, CsgError> {
    let mut polys = Vec::with_capacity(triangles.len());
    for tri in triangles {
        let v0 = Point3::from_f32(tri.vertices[0])?;
        let v1 = Point3::from_f32(tri.vertices[1])?;
        let v2 = Point3::from_f32(tri.vertices[2])?;
        // Drop degenerate (zero-area) triangles silently — they carry
        // no surface and would produce a zero-normal plane that breaks
        // classification.
        if let Some(p) = Polygon::from_triangle(v0, v1, v2, tri.color) {
            polys.push(p);
        }
    }
    Ok(polys)
}

fn polygons_to_triangles(polys: &[Polygon]) -> Vec<Triangle> {
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
