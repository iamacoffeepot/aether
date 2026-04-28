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
use crate::mesh::MeshError;
use aether_math::Vec3;
use std::collections::HashMap;

/// N-gon polygonal face — the canonical mesh form (ADR-0057).
///
/// `vertices` is the outer boundary, wound CCW around `plane_normal`.
/// `holes` lists inner boundaries, each wound CW.
#[derive(Debug, Clone, PartialEq)]
pub struct Polygon {
    pub vertices: Vec<Vec3>,
    pub holes: Vec<Vec<Vec3>>,
    pub plane_normal: Vec3,
    pub color: u32,
}

/// Mesh `node` and return the result as n-gon polygons.
///
/// Goes directly through [`crate::mesh::mesh_polygons_internal`] —
/// the polygon-domain mesh evaluator that operates polygon-in /
/// polygon-out throughout (no triangle round-trip). This is the fix
/// for the protruding_sphere SingularEdges: the previous path went
/// `mesh → Vec<Triangle> → from_triangle (re-derives plane via cross
/// product) → polygons`, and `from_triangle` flips `n_z` sign on
/// CDT-output sliver triangles. Skipping the triangle hop avoids the
/// re-derivation entirely — n-gon loops travel from CSG cleanup
/// straight into [`group_loops`].
pub fn mesh_polygons(node: &crate::ast::Node) -> Result<Vec<Polygon>, MeshError> {
    let loops = crate::mesh::mesh_polygons_internal(node)?;
    Ok(group_loops(loops))
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
        let mut classified: Vec<(i128, Vec<Vec3>)> = group
            .into_iter()
            .map(|poly| {
                let area = projected_signed_area(&poly.vertices, &plane);
                let f32_verts: Vec<Vec3> = poly.vertices.iter().map(|v| v.to_f32()).collect();
                (area, f32_verts)
            })
            .filter(|(area, _)| *area != 0)
            .collect();
        classified.sort_by_key(|(area, _)| *area);

        let mut outers: Vec<Vec<Vec3>> = Vec::new();
        let mut holes: Vec<Vec<Vec3>> = Vec::new();
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

fn unit_normal(plane: &csg::plane::Plane3) -> Vec3 {
    let nx = plane.n_x as f64;
    let ny = plane.n_y as f64;
    let nz = plane.n_z as f64;
    let len = (nx * nx + ny * ny + nz * nz).sqrt();
    if len == 0.0 {
        return Vec3::Z;
    }
    Vec3::new((nx / len) as f32, (ny / len) as f32, (nz / len) as f32)
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
pub fn tessellate_polygon(polygon: &Polygon) -> Vec<[Vec3; 3]> {
    if polygon.vertices.len() < 3 {
        return Vec::new();
    }

    // Fast path: convex outer with no holes can be fan-triangulated
    // without touching the integer CDT machinery. Most cleaned-up CSG
    // faces are convex (cleanup's coplanar merging produces convex
    // results when the inputs are convex), and skipping CDT keeps the
    // editor's per-frame cost low.
    //
    // Pass the polygon's stored plane_normal — the cleanup-emitted loops
    // routinely have collinear consecutive vertices (T-junction repair
    // adds them on shared edges), and re-deriving the normal from the
    // first three vertices via cross product collapses to ~zero in that
    // case, which sends `is_convex` into the wrong axis projection. The
    // CDT slow path then panics on the collinear constraint vertices.
    if polygon.holes.is_empty() && is_convex(&polygon.vertices, &polygon.plane_normal) {
        return fan_triangulate(&polygon.vertices);
    }

    // Slow path: anything else (concave outer, or any holes) goes
    // through the integer CDT module. Convert to fixed-point, run CDT,
    // convert back.
    if let Some(tris) = cdt_tessellate(polygon) {
        return tris;
    }

    // CDT failed (issue 335). Fan-triangulate the outer plus each hole
    // independently — matches the behaviour of
    // `csg::tessellate::triangulate_indexed`'s same-condition fallback,
    // so geometry isn't dropped silently. Hole triangles cover the hole
    // (visually wrong) but the surrounding face stays visible. The warn
    // surfaces the failing input so the underlying CDT bug can be
    // reproduced.
    tracing::warn!(
        outer_len = polygon.vertices.len(),
        holes = polygon.holes.len(),
        normal = ?polygon.plane_normal,
        color = polygon.color,
        "CDT failed; fan-triangulating outer + holes (issue 335)"
    );
    let mut out = fan_triangulate(&polygon.vertices);
    for hole in &polygon.holes {
        if hole.len() >= 3 {
            out.extend(fan_triangulate(hole));
        }
    }
    out
}

fn is_convex(vertices: &[Vec3], normal: &Vec3) -> bool {
    // Convex iff all cross products around the loop have the same sign
    // when projected to 2D. We project to whichever pair of axes the
    // polygon's plane normal is most perpendicular to.
    let n = vertices.len();
    if n < 3 {
        return true;
    }
    let (a_idx, b_idx) = dominant_axes(*normal);
    let pick = |v: Vec3, axis: usize| -> f32 {
        match axis {
            0 => v.x,
            1 => v.y,
            _ => v.z,
        }
    };
    let mut sign: i32 = 0;
    for i in 0..n {
        let j = (i + 1) % n;
        let k = (i + 2) % n;
        let ax = pick(vertices[j], a_idx) - pick(vertices[i], a_idx);
        let ay = pick(vertices[j], b_idx) - pick(vertices[i], b_idx);
        let bx = pick(vertices[k], a_idx) - pick(vertices[j], a_idx);
        let by = pick(vertices[k], b_idx) - pick(vertices[j], b_idx);
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

fn dominant_axes(normal: Vec3) -> (usize, usize) {
    let ax = normal.x.abs();
    let ay = normal.y.abs();
    let az = normal.z.abs();
    if ax >= ay && ax >= az {
        (1, 2)
    } else if ay >= az {
        (0, 2)
    } else {
        (0, 1)
    }
}

fn fan_triangulate(vertices: &[Vec3]) -> Vec<[Vec3; 3]> {
    let mut out = Vec::with_capacity(vertices.len().saturating_sub(2));
    for i in 1..vertices.len() - 1 {
        out.push([vertices[0], vertices[i], vertices[i + 1]]);
    }
    out
}

fn cdt_tessellate(polygon: &Polygon) -> Option<Vec<[Vec3; 3]>> {
    csg::tessellate::tessellate_polygon_f32(&polygon.vertices, &polygon.holes)
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
                Vec3::new(0.0, 0.0, 0.0),
                Vec3::new(1.0, 0.0, 0.0),
                Vec3::new(1.0, 1.0, 0.0),
                Vec3::new(0.0, 1.0, 0.0),
            ],
            holes: vec![],
            plane_normal: Vec3::new(0.0, 0.0, 1.0),
            color: 0,
        };
        let tris = tessellate_polygon(&p);
        assert_eq!(tris.len(), 2);
    }

    #[test]
    fn tessellate_triangle_yields_one_triangle() {
        let p = Polygon {
            vertices: vec![
                Vec3::new(0.0, 0.0, 0.0),
                Vec3::new(1.0, 0.0, 0.0),
                Vec3::new(0.0, 1.0, 0.0),
            ],
            holes: vec![],
            plane_normal: Vec3::new(0.0, 0.0, 1.0),
            color: 0,
        };
        let tris = tessellate_polygon(&p);
        assert_eq!(tris.len(), 1);
    }

    #[test]
    fn empty_or_degenerate_polygon_yields_no_triangles() {
        let empty = Polygon {
            vertices: vec![],
            holes: vec![],
            plane_normal: Vec3::new(0.0, 0.0, 1.0),
            color: 0,
        };
        assert!(tessellate_polygon(&empty).is_empty());
        let two_vert = Polygon {
            vertices: vec![Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 0.0, 0.0)],
            holes: vec![],
            plane_normal: Vec3::new(0.0, 0.0, 1.0),
            color: 0,
        };
        assert!(tessellate_polygon(&two_vert).is_empty());
    }

    #[test]
    fn fan_triangulate_n_gon_yields_n_minus_2_triangles() {
        for n in 3..=8 {
            let vertices: Vec<Vec3> = (0..n)
                .map(|i| {
                    let theta = 2.0 * std::f32::consts::PI * (i as f32) / (n as f32);
                    Vec3::new(theta.cos(), theta.sin(), 0.0)
                })
                .collect();
            let tris = fan_triangulate(&vertices);
            assert_eq!(
                tris.len(),
                n - 2,
                "fan-triangulating an {n}-gon should yield {} triangles",
                n - 2
            );
        }
    }

    #[test]
    fn is_convex_accepts_convex_quad() {
        let quad = vec![
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(1.0, 1.0, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
        ];
        assert!(is_convex(&quad, &Vec3::new(0.0, 0.0, 1.0)));
    }

    #[test]
    fn is_convex_rejects_concave_l_shape() {
        // L-shaped polygon — concave at the inner corner.
        let l_shape = vec![
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(2.0, 0.0, 0.0),
            Vec3::new(2.0, 1.0, 0.0),
            Vec3::new(1.0, 1.0, 0.0),
            Vec3::new(1.0, 2.0, 0.0),
            Vec3::new(0.0, 2.0, 0.0),
        ];
        assert!(!is_convex(&l_shape, &Vec3::new(0.0, 0.0, 1.0)));
    }

    /// Pin the bug fix: a 6-vertex chord-rectangle (T-junction repair
    /// added 2 collinear vertices on the top + bottom of a CSG-clipped
    /// cylinder side facet) used to misroute through CDT because the
    /// re-derived normal from the first three (collinear) vertices
    /// degenerated to ~zero, causing `dominant_axes` to pick the
    /// projection that collapses the polygon to a line. Passing the
    /// stored `plane_normal` from the polygon avoids the re-derivation.
    #[test]
    fn is_convex_handles_collinear_first_three_vertices() {
        // First three vertices collinear along the cube top face; the
        // polygon's true normal is in the XZ plane (cylinder side
        // facet).
        let chord_rect = vec![
            Vec3::new(1.17717, 0.5, -0.114792),
            Vec3::new(1.120239, 0.5, -0.199997),
            Vec3::new(1.112137, 0.5, -0.212128),
            Vec3::new(1.112137, -0.5, -0.212128),
            Vec3::new(1.120239, -0.5, -0.199997),
            Vec3::new(1.17717, -0.5, -0.114792),
        ];
        let normal = Vec3::new(-0.831, 0.0, 0.556);
        assert!(is_convex(&chord_rect, &normal));
    }

    #[test]
    fn unit_normal_for_axis_aligned_planes() {
        // Build planes from points and verify unit_normal direction.
        let xy = csg::plane::Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 100, // any positive value
            d: 0,
        };
        let n = unit_normal(&xy);
        assert!((n.x).abs() < 1e-6);
        assert!((n.y).abs() < 1e-6);
        assert!((n.z - 1.0).abs() < 1e-6, "expected +z, got {n:?}");
    }

    #[test]
    fn unit_normal_for_degenerate_plane_returns_default() {
        let degen = csg::plane::Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 0,
            d: 0,
        };
        assert_eq!(unit_normal(&degen), Vec3::new(0.0, 0.0, 1.0));
    }

    /// Issue 335 regression: when CDT returns None on a polygon-
    /// with-holes, the display-time path used to silently return an
    /// empty Vec via `unwrap_or_default()`, dropping whole faces from
    /// the rendered mesh. The fan fallback now keeps the outer (and
    /// each hole) visible. Trigger a deterministic CDT-None by passing
    /// coordinates outside the fixed-point cap (ADR-0054, ±256 units)
    /// — `Point3::from_f32` rejects them and `tessellate_polygon_f32`
    /// returns None before CDT even runs. The hole bypasses the convex
    /// fast path so the failing slow path is exercised.
    #[test]
    fn tessellate_polygon_falls_back_to_fan_when_cdt_returns_none() {
        let p = Polygon {
            vertices: vec![
                Vec3::new(0.0, 0.0, 0.0),
                Vec3::new(1000.0, 0.0, 0.0), // out of fixed-point range
                Vec3::new(1000.0, 1000.0, 0.0),
                Vec3::new(0.0, 1000.0, 0.0),
            ],
            holes: vec![vec![
                Vec3::new(100.0, 100.0, 0.0),
                Vec3::new(100.0, 900.0, 0.0),
                Vec3::new(900.0, 900.0, 0.0),
                Vec3::new(900.0, 100.0, 0.0),
            ]],
            plane_normal: Vec3::new(0.0, 0.0, 1.0),
            color: 0,
        };
        let tris = tessellate_polygon(&p);
        // Outer quad fan-triangulates to 2 triangles; each 4-vert hole
        // fan-triangulates to 2 more. The exact count is the contract:
        // anything > 0 proves the face wasn't dropped.
        assert_eq!(
            tris.len(),
            4,
            "outer (2) + one hole (2) = 4 fan triangles; got {}",
            tris.len()
        );
    }

    #[test]
    fn tessellate_polygon_with_hole_uses_cdt() {
        // Outer 2x2 quad (CCW) with a 1x1 hole (CW).
        let p = Polygon {
            vertices: vec![
                Vec3::new(0.0, 0.0, 0.0),
                Vec3::new(2.0, 0.0, 0.0),
                Vec3::new(2.0, 2.0, 0.0),
                Vec3::new(0.0, 2.0, 0.0),
            ],
            holes: vec![vec![
                Vec3::new(0.5, 0.5, 0.0),
                Vec3::new(0.5, 1.5, 0.0),
                Vec3::new(1.5, 1.5, 0.0),
                Vec3::new(1.5, 0.5, 0.0),
            ]],
            plane_normal: Vec3::new(0.0, 0.0, 1.0),
            color: 0,
        };
        let tris = tessellate_polygon(&p);
        // Annular triangle count: V + 2H - 2 = 8 for 4 outer + 4 hole verts.
        assert_eq!(tris.len(), 8, "expected annular topological minimum");
    }
}
