//! Public n-gon polygon API (ADR-0057).
//!
//! `Polygon` is the canonical mesh form for the engine — one `Polygon`
//! per logical face, with optional hole loops for pierced regions.
//! Triangulation moves to display time via [`tessellate_polygon`], so
//! consumers (notably `aether-mesh-editor-component`) hand polygons to
//! the GPU upload step rather than triangles.
//!
//! [`mesh_polygons`] is the polygon-domain analogue of [`crate::mesh`]:
//! parses the same AST, runs the same CSG, but returns n-gon polygons
//! instead of triangles. Internally it goes through [`crate::mesh`]
//! then [`crate::csg::cleanup::run_to_loops`] to recover the boundary
//! loops the cleanup pipeline produces; a future PR will skip the
//! triangle round-trip by taking the polygon-domain path through
//! `csg::ops::*` directly.
//!
//! Hole representation: `Polygon::holes` is the explicit
//! polygon-with-holes form chosen for v1. The cleanup pipeline emits
//! one boundary loop per face component; this module groups loops by
//! plane + color, identifies outer (positive signed area in the plane
//! projection) vs hole (negative signed area), and assembles a
//! `Polygon` per outer with its enclosed holes attached.

use crate::csg;
use crate::csg::point::Point3;
use crate::mesh::{MeshError, Triangle};
use std::collections::HashMap;

/// N-gon polygonal face — the canonical mesh form (ADR-0057).
///
/// `vertices` is the outer boundary, wound CCW around `plane_normal`.
/// `holes` lists inner boundaries, each wound CW.
#[derive(Debug, Clone, PartialEq)]
pub struct Polygon {
    pub vertices: Vec<[f32; 3]>,
    pub holes: Vec<Vec<[f32; 3]>>,
    pub plane_normal: [f32; 3],
    pub color: u32,
}

/// Mesh `node` and return the result as n-gon polygons.
///
/// Internally calls [`crate::mesh::mesh`] to get triangles, then runs
/// the cleanup pipeline's loop-extraction pass (skipping triangulation)
/// to recover the boundary loops, then groups by plane + color into
/// outer + holes via signed-area orientation. The triangle round-trip
/// is wasteful but correct; ADR-0057 PR 3 will replace it with a
/// polygon-domain CSG path.
pub fn mesh_polygons(node: &crate::ast::Node) -> Result<Vec<Polygon>, MeshError> {
    let triangles = crate::mesh::mesh(node)?;
    Ok(triangles_to_polygons(&triangles))
}

fn triangles_to_polygons(triangles: &[Triangle]) -> Vec<Polygon> {
    let csg_polys: Vec<csg::polygon::Polygon> = triangles
        .iter()
        .filter_map(|t| {
            let v0 = Point3::from_f32(t.vertices[0]).ok()?;
            let v1 = Point3::from_f32(t.vertices[1]).ok()?;
            let v2 = Point3::from_f32(t.vertices[2]).ok()?;
            csg::polygon::Polygon::from_triangle(v0, v1, v2, t.color)
        })
        .collect();

    let loops = csg::cleanup::run_to_loops(csg_polys);
    group_loops(loops)
}

type GroupKey = (i64, i64, i64, i128, u32);

fn group_loops(loops: Vec<csg::polygon::Polygon>) -> Vec<Polygon> {
    let mut groups: HashMap<GroupKey, Vec<csg::polygon::Polygon>> = HashMap::new();
    for poly in loops {
        // Use the GCD-canonical plane key so re-derived planes from
        // CDT-output triangles match the original face plane.
        let (kx, ky, kz, kd) = poly.plane.canonical_key();
        let key = (kx, ky, kz, kd, poly.color);
        groups.entry(key).or_default().push(poly);
    }

    // Sort keys so output order is deterministic across runs / platforms.
    let mut sorted_keys: Vec<GroupKey> = groups.keys().copied().collect();
    sorted_keys.sort();

    let mut out: Vec<Polygon> = Vec::with_capacity(sorted_keys.len());
    for key in sorted_keys {
        let group = groups.remove(&key).expect("key from this map");
        let plane = group[0].plane;
        let color = group[0].color;
        let plane_normal = unit_normal(&plane);

        // Sort loops within the group by signed area (deterministic) and
        // partition into outer (positive area) vs hole (negative area).
        // For a typical CSG-cut face there's one outer and zero+ holes.
        let mut classified: Vec<(i128, Vec<[f32; 3]>)> = group
            .into_iter()
            .map(|poly| {
                let area = projected_signed_area(&poly.vertices, &plane);
                let f32_verts: Vec<[f32; 3]> = poly.vertices.iter().map(|v| v.to_f32()).collect();
                (area, f32_verts)
            })
            .filter(|(area, _)| *area != 0)
            .collect();
        classified.sort_by_key(|(area, _)| *area);

        let mut outers: Vec<Vec<[f32; 3]>> = Vec::new();
        let mut holes: Vec<Vec<[f32; 3]>> = Vec::new();
        for (area, verts) in classified {
            if area > 0 {
                outers.push(verts);
            } else {
                holes.push(verts);
            }
        }

        if outers.is_empty() {
            // No outer in this group (degenerate or hole-only). Skip.
            continue;
        }

        // Emit one Polygon per outer. With multiple outers in one group
        // (rare — typically only one face per (plane, color) group), the
        // first outer carries all holes. PR 5 will preserve component
        // identity so each outer's holes are correctly attached.
        let last_outer_idx = outers.len() - 1;
        for (i, outer) in outers.into_iter().enumerate() {
            let polygon_holes = if i == last_outer_idx {
                std::mem::take(&mut holes)
            } else {
                Vec::new()
            };
            out.push(Polygon {
                vertices: outer,
                holes: polygon_holes,
                plane_normal,
                color,
            });
        }
    }
    out
}

fn unit_normal(plane: &csg::plane::Plane3) -> [f32; 3] {
    let nx = plane.n_x as f64;
    let ny = plane.n_y as f64;
    let nz = plane.n_z as f64;
    let len = (nx * nx + ny * ny + nz * nz).sqrt();
    if len == 0.0 {
        return [0.0, 0.0, 1.0];
    }
    [(nx / len) as f32, (ny / len) as f32, (nz / len) as f32]
}

/// Signed doubled area of `vertices` projected onto the plane's
/// dominant axes. Positive = CCW around `plane.normal`, negative = CW.
fn projected_signed_area(vertices: &[Point3], plane: &csg::plane::Plane3) -> i128 {
    let (axis_a, axis_b) = projection_axes(plane);
    let mut sum: i128 = 0;
    let n = vertices.len();
    for i in 0..n {
        let j = (i + 1) % n;
        let ai = pick(vertices[i], axis_a) as i128;
        let bi = pick(vertices[i], axis_b) as i128;
        let aj = pick(vertices[j], axis_a) as i128;
        let bj = pick(vertices[j], axis_b) as i128;
        sum += ai * bj - aj * bi;
    }
    sum
}

#[derive(Debug, Clone, Copy)]
enum Axis {
    X,
    Y,
    Z,
}

fn pick(p: Point3, a: Axis) -> i32 {
    match a {
        Axis::X => p.x,
        Axis::Y => p.y,
        Axis::Z => p.z,
    }
}

fn projection_axes(plane: &csg::plane::Plane3) -> (Axis, Axis) {
    let ax = plane.n_x.unsigned_abs();
    let ay = plane.n_y.unsigned_abs();
    let az = plane.n_z.unsigned_abs();
    if ax >= ay && ax >= az {
        if plane.n_x >= 0 {
            (Axis::Y, Axis::Z)
        } else {
            (Axis::Z, Axis::Y)
        }
    } else if ay >= az {
        if plane.n_y >= 0 {
            (Axis::Z, Axis::X)
        } else {
            (Axis::X, Axis::Z)
        }
    } else if plane.n_z >= 0 {
        (Axis::X, Axis::Y)
    } else {
        (Axis::Y, Axis::X)
    }
}

/// Tessellate a polygon (with optional holes) into triangles for GPU
/// upload. Output triangles are wound CCW around `plane_normal`. This
/// is the display-time tessellation step ADR-0057 moves out of the
/// cleanup pipeline — consumers like `aether-mesh-editor-component`
/// call it once per polygon when assembling render-ready geometry.
///
/// Returns an empty Vec for degenerate input (fewer than 3 outer
/// vertices, or polygon outside the integer fixed-point coordinate
/// budget).
pub fn tessellate_polygon(polygon: &Polygon) -> Vec<[[f32; 3]; 3]> {
    if polygon.vertices.len() < 3 {
        return Vec::new();
    }

    // Fast path: convex outer with no holes can be fan-triangulated
    // without touching the integer CDT machinery. Most cleaned-up CSG
    // faces are convex (cleanup's coplanar merging produces convex
    // results when the inputs are convex), and skipping CDT keeps the
    // editor's per-frame cost low.
    if polygon.holes.is_empty() && is_convex(&polygon.vertices) {
        return fan_triangulate(&polygon.vertices);
    }

    // Slow path: anything else (concave outer, or any holes) goes
    // through the integer CDT module. Convert to fixed-point, run CDT,
    // convert back.
    cdt_tessellate(polygon).unwrap_or_default()
}

fn is_convex(vertices: &[[f32; 3]]) -> bool {
    // Convex iff all cross products around the loop have the same sign
    // when projected to 2D. We project to whichever pair of axes the
    // polygon's plane normal is most perpendicular to (computed from
    // the first three vertices).
    let n = vertices.len();
    if n < 3 {
        return true;
    }
    let normal = compute_normal(vertices);
    let (a_idx, b_idx) = dominant_axes(normal);
    let mut sign: i32 = 0;
    for i in 0..n {
        let j = (i + 1) % n;
        let k = (i + 2) % n;
        let ax = vertices[j][a_idx] - vertices[i][a_idx];
        let ay = vertices[j][b_idx] - vertices[i][b_idx];
        let bx = vertices[k][a_idx] - vertices[j][a_idx];
        let by = vertices[k][b_idx] - vertices[j][b_idx];
        let cross = ax * by - ay * bx;
        let s = if cross > 0.0 {
            1
        } else if cross < 0.0 {
            -1
        } else {
            0
        };
        if s == 0 {
            continue;
        }
        if sign == 0 {
            sign = s;
        } else if s != sign {
            return false;
        }
    }
    true
}

fn compute_normal(vertices: &[[f32; 3]]) -> [f32; 3] {
    let a = vertices[0];
    let b = vertices[1];
    let c = vertices[2];
    let e1 = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let e2 = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    [
        e1[1] * e2[2] - e1[2] * e2[1],
        e1[2] * e2[0] - e1[0] * e2[2],
        e1[0] * e2[1] - e1[1] * e2[0],
    ]
}

fn dominant_axes(normal: [f32; 3]) -> (usize, usize) {
    let ax = normal[0].abs();
    let ay = normal[1].abs();
    let az = normal[2].abs();
    if ax >= ay && ax >= az {
        (1, 2)
    } else if ay >= az {
        (0, 2)
    } else {
        (0, 1)
    }
}

fn fan_triangulate(vertices: &[[f32; 3]]) -> Vec<[[f32; 3]; 3]> {
    let mut out = Vec::with_capacity(vertices.len().saturating_sub(2));
    for i in 1..vertices.len() - 1 {
        out.push([vertices[0], vertices[i], vertices[i + 1]]);
    }
    out
}

fn cdt_tessellate(polygon: &Polygon) -> Option<Vec<[[f32; 3]; 3]>> {
    csg::cleanup::tessellate_polygon_f32(&polygon.vertices, &polygon.holes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Node;

    #[test]
    fn box_emits_six_quad_polygons() {
        let node = Node::Box {
            x: 1.0,
            y: 1.0,
            z: 1.0,
            color: 0,
        };
        let polys = mesh_polygons(&node).unwrap();
        // A unit cube: 6 faces, each a single quad after coplanar merge.
        assert_eq!(polys.len(), 6);
        for p in &polys {
            assert_eq!(p.vertices.len(), 4, "box face should be a quad");
            assert!(p.holes.is_empty(), "box face has no holes");
        }
    }

    #[test]
    fn fan_tessellate_quad_yields_two_triangles() {
        let p = Polygon {
            vertices: vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, 1.0, 0.0],
                [0.0, 1.0, 0.0],
            ],
            holes: vec![],
            plane_normal: [0.0, 0.0, 1.0],
            color: 0,
        };
        let tris = tessellate_polygon(&p);
        assert_eq!(tris.len(), 2);
    }

    #[test]
    fn tessellate_triangle_yields_one_triangle() {
        let p = Polygon {
            vertices: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            holes: vec![],
            plane_normal: [0.0, 0.0, 1.0],
            color: 0,
        };
        let tris = tessellate_polygon(&p);
        assert_eq!(tris.len(), 1);
    }

    #[test]
    fn tessellate_polygon_with_hole_uses_cdt() {
        // Outer 2x2 quad (CCW) with a 1x1 hole (CW).
        let p = Polygon {
            vertices: vec![
                [0.0, 0.0, 0.0],
                [2.0, 0.0, 0.0],
                [2.0, 2.0, 0.0],
                [0.0, 2.0, 0.0],
            ],
            holes: vec![vec![
                [0.5, 0.5, 0.0],
                [0.5, 1.5, 0.0],
                [1.5, 1.5, 0.0],
                [1.5, 0.5, 0.0],
            ]],
            plane_normal: [0.0, 0.0, 1.0],
            color: 0,
        };
        let tris = tessellate_polygon(&p);
        // Annular triangle count: V + 2H - 2 = 8 for 4 outer + 4 hole verts.
        assert_eq!(tris.len(), 8, "expected annular topological minimum");
    }
}
