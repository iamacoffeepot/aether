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
//!
//! ## Coordinate type
//!
//! Vertices are stored as [`Point3`] (16:16 fixed-point integers) end-
//! to-end through the mesh pipeline — same type the BSP CSG core and
//! cleanup passes already use. The conversion to `f32` happens at the
//! GPU upload boundary inside `aether-mesh-editor-component`, not
//! here. Keeping the polygon-domain integer-typed eliminates the f32
//! noise that previously caused `is_convex` and CDT to disagree on
//! near-collinear vertices (issue 335). The only `f32` field is
//! [`Polygon::plane_normal`], used as a unit-vector hint for axis
//! selection and face-normal lighting; small noise there doesn't
//! affect topology.

use crate::csg;
use crate::csg::point::Point3;
use crate::mesh::MeshError;
use aether_math::Vec3;
use std::collections::HashMap;

/// N-gon polygonal face — the canonical mesh form (ADR-0057).
///
/// `vertices` is the outer boundary, wound CCW around `plane_normal`.
/// `holes` lists inner boundaries, each wound CW. Both carry
/// fixed-point integer coordinates ([`Point3`]) — convert via
/// `Point3::to_f32()` at the GPU upload site.
#[derive(Debug, Clone, PartialEq)]
pub struct Polygon {
    pub vertices: Vec<Point3>,
    pub holes: Vec<Vec<Point3>>,
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
        let mut classified: Vec<(i128, Vec<Point3>)> = group
            .into_iter()
            .map(|poly| {
                let area = projected_signed_area(&poly.vertices, &plane);
                (area, poly.vertices)
            })
            .filter(|(area, _)| *area != 0)
            .collect();
        classified.sort_by_key(|(area, _)| *area);

        // Partition by sign and remember each outer's signed area for
        // the smallest-containing-outer tie-break below.
        let mut outers: Vec<(i128, Vec<Point3>)> = Vec::new();
        let mut holes: Vec<Vec<Point3>> = Vec::new();
        for (area, verts) in classified {
            if area > 0 {
                outers.push((area, verts));
            } else {
                holes.push(verts);
            }
        }

        if outers.is_empty() {
            // No outer in this group (degenerate or hole-only). Skip.
            continue;
        }

        // Attach each hole to the smallest-area outer whose 2D
        // projection contains the hole's first vertex. With one outer
        // in the group this collapses to the original behaviour; with
        // multiple disjoint coplanar outers it routes the hole to the
        // component it actually lies inside (issue 353).
        let containment_axes = containment_axes(&plane);
        let outer_projections: Vec<Vec<(i64, i64)>> = outers
            .iter()
            .map(|(_, verts)| {
                verts
                    .iter()
                    .map(|p| project_2d(*p, containment_axes))
                    .collect()
            })
            .collect();
        let mut hole_assignments: Vec<Vec<Vec<Point3>>> = vec![Vec::new(); outers.len()];
        for hole in holes {
            let probe = match hole.first() {
                Some(p) => project_2d(*p, containment_axes),
                None => continue,
            };
            let mut best: Option<(i128, Point3, usize)> = None;
            for (idx, ((area, outer_verts), projected)) in
                outers.iter().zip(outer_projections.iter()).enumerate()
            {
                if !point_in_polygon_2d(probe, projected) {
                    continue;
                }
                let first_vert = outer_verts[0];
                let candidate = (*area, first_vert, idx);
                let take = match best {
                    None => true,
                    Some((b_area, b_first, _)) => {
                        // Smaller area wins; lex-smallest first vertex
                        // breaks ties, matching the rest of cleanup's
                        // determinism convention.
                        candidate.0 < b_area || (candidate.0 == b_area && candidate.1 < b_first)
                    }
                };
                if take {
                    best = Some(candidate);
                }
            }
            match best {
                Some((_, _, idx)) => hole_assignments[idx].push(hole),
                None => {
                    tracing::warn!(
                        plane_normal = ?plane_normal,
                        color = color,
                        "polygon hole has no containing outer; skipping (issue 353)"
                    );
                }
            }
        }

        for ((_, outer), polygon_holes) in outers.into_iter().zip(hole_assignments.into_iter()) {
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

/// Pick the two world axes to project onto for 2D containment tests:
/// drop the axis with the largest absolute normal component. Same
/// convention as `csg::cleanup::merge::drop_axis` — exact for axis-
/// aligned planes, shears tilted ones (collinearity preserved).
fn containment_axes(plane: &csg::plane::Plane3) -> (usize, usize) {
    let ax = plane.n_x.unsigned_abs();
    let ay = plane.n_y.unsigned_abs();
    let az = plane.n_z.unsigned_abs();
    if ax >= ay && ax >= az {
        (1, 2)
    } else if ay >= az {
        (0, 2)
    } else {
        (0, 1)
    }
}

fn project_2d(p: Point3, axes: (usize, usize)) -> (i64, i64) {
    let coords = [p.x as i64, p.y as i64, p.z as i64];
    (coords[axes.0], coords[axes.1])
}

/// Standard ray-casting point-in-polygon test in 2D fixed-point
/// integers. Half-open horizontal-edge convention: an edge is counted
/// if its lower endpoint is strictly below the ray (`y <= probe_y`)
/// and its upper endpoint is at or above (`y > probe_y`). This
/// avoids double-counting a vertex shared by two edges and matches
/// the convention used in `csg::tessellate::cdt::triangulate`.
fn point_in_polygon_2d(probe: (i64, i64), poly: &[(i64, i64)]) -> bool {
    let (px, py) = (probe.0 as i128, probe.1 as i128);
    let mut inside = false;
    let n = poly.len();
    if n < 3 {
        return false;
    }
    for i in 0..n {
        let j = (i + n - 1) % n;
        let (ix, iy) = (poly[i].0 as i128, poly[i].1 as i128);
        let (jx, jy) = (poly[j].0 as i128, poly[j].1 as i128);
        // Half-open straddle: one endpoint strictly above py, the
        // other at-or-below. Equivalent to `(iy > py) != (jy > py)`.
        if (iy > py) == (jy > py) {
            continue;
        }
        // Intersection x at horizontal line y = py:
        //   x_at = jx + (ix - jx) * (py - jy) / (iy - jy)
        // Want px < x_at; cross-multiply by (iy - jy) and track sign.
        let denom = iy - jy;
        let lhs = (px - jx) * denom;
        let rhs = (ix - jx) * (py - jy);
        let crosses = if denom > 0 { lhs < rhs } else { lhs > rhs };
        if crosses {
            inside = !inside;
        }
    }
    inside
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
/// Output triangles carry [`Point3`] vertices; convert via
/// `Point3::to_f32()` at the GPU upload boundary.
///
/// Returns an empty Vec for degenerate input (fewer than 3 outer
/// vertices).
pub fn tessellate_polygon(polygon: &Polygon) -> Vec<[Point3; 3]> {
    if polygon.vertices.len() < 3 {
        return Vec::new();
    }

    // Fast path: convex outer with no holes can be fan-triangulated
    // without touching the CDT machinery. Most cleaned-up CSG faces
    // are convex (cleanup's coplanar merging produces convex results
    // when the inputs are convex), and skipping CDT keeps the editor's
    // per-frame cost low. The convex check is exact integer cross
    // products, so collinear T-junction-inserted vertices never
    // misclassify the polygon as concave (issue 335).
    if polygon.holes.is_empty() && is_convex(&polygon.vertices, &polygon.plane_normal) {
        return fan_triangulate(&polygon.vertices);
    }

    // Slow path: anything else (concave outer, or any holes) goes
    // through the integer CDT module.
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

/// Convex check using integer cross products. The polygon's stored
/// `plane_normal` selects which two axes to project onto; cross
/// products are i128 (safe — vertex coordinates fit in i32 with room
/// to spare, so differences and products of differences stay well
/// inside i128).
///
/// Tolerance: `csg::cleanup::tjunctions` accepts collinearity up to
/// `COLLINEAR_TOLERANCE_FIXED_UNITS = 4` perpendicular fixed units —
/// vertices inserted by T-junction repair can sit up to 4 units off
/// their hosting edge after accumulated CSG snap drift (issue #299).
/// We match that tolerance here: any cross of magnitude ≤
/// `4 * max_edge_length` corresponds to a deviation ≤ 4 units, and is
/// treated as collinear. Real convex corners produce crosses on the
/// order of `max_edge_length²` — orders of magnitude above the
/// snap-drift floor — so the threshold doesn't risk false positives.
/// The comparison stays in integer arithmetic via squared form
/// (`cross² ≤ 16 · max_edge_sq`).
fn is_convex(vertices: &[Point3], normal: &Vec3) -> bool {
    let n = vertices.len();
    if n < 3 {
        return true;
    }
    let (a_idx, b_idx) = dominant_axes(*normal);
    let pick = |p: Point3, axis: usize| -> i128 {
        match axis {
            0 => p.x as i128,
            1 => p.y as i128,
            _ => p.z as i128,
        }
    };

    // First pass: max edge length squared (in fixed-point units²) for
    // the snap-drift tolerance.
    let mut max_edge_sq: i128 = 0;
    for i in 0..n {
        let j = (i + 1) % n;
        let dx = pick(vertices[j], a_idx) - pick(vertices[i], a_idx);
        let dy = pick(vertices[j], b_idx) - pick(vertices[i], b_idx);
        let len_sq = dx * dx + dy * dy;
        if len_sq > max_edge_sq {
            max_edge_sq = len_sq;
        }
    }
    // (4 * max_edge_length)² = 16 * max_edge_sq.
    let tol_sq = max_edge_sq.saturating_mul(16);

    let mut sign: i32 = 0;
    for i in 0..n {
        let j = (i + 1) % n;
        let k = (i + 2) % n;
        let ax = pick(vertices[j], a_idx) - pick(vertices[i], a_idx);
        let ay = pick(vertices[j], b_idx) - pick(vertices[i], b_idx);
        let bx = pick(vertices[k], a_idx) - pick(vertices[j], a_idx);
        let by = pick(vertices[k], b_idx) - pick(vertices[j], b_idx);
        let cross = ax * by - ay * bx;
        if cross.saturating_mul(cross) <= tol_sq {
            continue;
        }
        let s = cross.signum() as i32;
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

fn fan_triangulate(vertices: &[Point3]) -> Vec<[Point3; 3]> {
    let mut out = Vec::with_capacity(vertices.len().saturating_sub(2));
    for i in 1..vertices.len() - 1 {
        out.push([vertices[0], vertices[i], vertices[i + 1]]);
    }
    out
}

fn cdt_tessellate(polygon: &Polygon) -> Option<Vec<[Point3; 3]>> {
    csg::tessellate::tessellate_polygon_integer(
        &polygon.vertices,
        &polygon.holes,
        polygon.plane_normal,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Node;

    fn p(x: f32, y: f32, z: f32) -> Point3 {
        Point3::from_f32(Vec3::new(x, y, z)).expect("in range")
    }

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
        for poly in &polys {
            assert_eq!(poly.vertices.len(), 4, "box face should be a quad");
            assert!(poly.holes.is_empty(), "box face has no holes");
        }
    }

    #[test]
    fn fan_tessellate_quad_yields_two_triangles() {
        let polygon = Polygon {
            vertices: vec![
                p(0.0, 0.0, 0.0),
                p(1.0, 0.0, 0.0),
                p(1.0, 1.0, 0.0),
                p(0.0, 1.0, 0.0),
            ],
            holes: vec![],
            plane_normal: Vec3::new(0.0, 0.0, 1.0),
            color: 0,
        };
        let tris = tessellate_polygon(&polygon);
        assert_eq!(tris.len(), 2);
    }

    #[test]
    fn tessellate_triangle_yields_one_triangle() {
        let polygon = Polygon {
            vertices: vec![p(0.0, 0.0, 0.0), p(1.0, 0.0, 0.0), p(0.0, 1.0, 0.0)],
            holes: vec![],
            plane_normal: Vec3::new(0.0, 0.0, 1.0),
            color: 0,
        };
        let tris = tessellate_polygon(&polygon);
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
            vertices: vec![p(0.0, 0.0, 0.0), p(1.0, 0.0, 0.0)],
            holes: vec![],
            plane_normal: Vec3::new(0.0, 0.0, 1.0),
            color: 0,
        };
        assert!(tessellate_polygon(&two_vert).is_empty());
    }

    #[test]
    fn fan_triangulate_n_gon_yields_n_minus_2_triangles() {
        for n in 3..=8 {
            let vertices: Vec<Point3> = (0..n)
                .map(|i| {
                    let theta = 2.0 * std::f32::consts::PI * (i as f32) / (n as f32);
                    p(theta.cos(), theta.sin(), 0.0)
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
            p(0.0, 0.0, 0.0),
            p(1.0, 0.0, 0.0),
            p(1.0, 1.0, 0.0),
            p(0.0, 1.0, 0.0),
        ];
        assert!(is_convex(&quad, &Vec3::new(0.0, 0.0, 1.0)));
    }

    #[test]
    fn is_convex_rejects_concave_l_shape() {
        let l_shape = vec![
            p(0.0, 0.0, 0.0),
            p(2.0, 0.0, 0.0),
            p(2.0, 1.0, 0.0),
            p(1.0, 1.0, 0.0),
            p(1.0, 2.0, 0.0),
            p(0.0, 2.0, 0.0),
        ];
        assert!(!is_convex(&l_shape, &Vec3::new(0.0, 0.0, 1.0)));
    }

    /// Pin the bug fix from prior PR: a 6-vertex chord-rectangle
    /// (T-junction repair added 2 collinear vertices on the top + bottom
    /// of a CSG-clipped cylinder side facet) used to misroute through
    /// CDT because the re-derived normal from the first three (collinear)
    /// vertices degenerated to ~zero. Passing the stored `plane_normal`
    /// avoids the re-derivation.
    #[test]
    fn is_convex_handles_collinear_first_three_vertices() {
        let chord_rect = vec![
            p(1.17717, 0.5, -0.114792),
            p(1.120239, 0.5, -0.199997),
            p(1.112137, 0.5, -0.212128),
            p(1.112137, -0.5, -0.212128),
            p(1.120239, -0.5, -0.199997),
            p(1.17717, -0.5, -0.114792),
        ];
        let normal = Vec3::new(-0.831, 0.0, 0.556);
        assert!(is_convex(&chord_rect, &normal));
    }

    /// Issue 335: a cylinder side facet with a snap-rounded T-junction
    /// vertex offset by ~1 fixed-point unit used to misclassify as
    /// concave under f32 cross products (a 4.5e-6 sign flip). With
    /// integer cross products, collinear-modulo-snap vertices give
    /// exactly zero — the polygon classifies as convex and fast-paths.
    #[test]
    fn is_convex_handles_snap_rounded_near_collinear_vertices() {
        // Polygon [9] from the cross-bored block diagnostic: a rectangle
        // on the cylinder side facet with an off-by-one-snap vertex at
        // index 7. Plane normal (-1/√2, 0, -1/√2).
        let verts = vec![
            p(0.34640503, 1.0, 0.19999695),
            p(0.34640503, 0.4928131, 0.19999695),
            p(0.34640503, 0.30717468, 0.19999695),
            p(0.34640503, 0.19999695, 0.19999695),
            p(0.34640503, -0.19999695, 0.19999695),
            p(0.34640503, -0.30717468, 0.19999695),
            p(0.34640503, -1.0, 0.19999695),
            p(0.19998169, -1.0, 0.3464203), // snap-offset by 1 fixed-point unit
            p(0.19999695, -0.7463989, 0.34640503),
            p(0.19999695, -0.45358276, 0.34640503),
            p(0.19999695, -0.34640503, 0.34640503),
            p(0.19999695, 0.34640503, 0.34640503),
            p(0.19999695, 0.45358276, 0.34640503),
            p(0.19999695, 1.0, 0.34640503),
        ];
        let normal = Vec3::new(-0.70710677, 0.0, -0.70710677);
        assert!(
            is_convex(&verts, &normal),
            "snap-rounded near-collinear vertex should not flip convex classification"
        );
    }

    #[test]
    fn unit_normal_for_axis_aligned_planes() {
        let xy = csg::plane::Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 100,
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

    /// Issue 335 regression: when CDT returns None, the display-time
    /// path used to silently return an empty Vec via `unwrap_or_default()`,
    /// dropping whole faces. The fan fallback now keeps the outer (and
    /// each hole) visible. Trigger CDT-None with a bad-winding hole loop
    /// (CCW like the outer instead of CW) — CDT's constraint enforcement
    /// rejects it, and our fallback fan-triangulates instead.
    #[test]
    fn tessellate_polygon_falls_back_to_fan_when_cdt_returns_none() {
        let polygon = Polygon {
            vertices: vec![
                p(0.0, 0.0, 0.0),
                p(2.0, 0.0, 0.0),
                p(2.0, 2.0, 0.0),
                p(0.0, 2.0, 0.0),
            ],
            // Hole wound the same direction as the outer (CCW) — CDT
            // would expect CW. Some inputs CDT can recover; for those
            // that can't, the fan fallback fires.
            holes: vec![vec![
                p(0.5, 0.5, 0.0),
                p(1.5, 0.5, 0.0),
                p(1.5, 1.5, 0.0),
                p(0.5, 1.5, 0.0),
            ]],
            plane_normal: Vec3::new(0.0, 0.0, 1.0),
            color: 0,
        };
        let tris = tessellate_polygon(&polygon);
        // Whether CDT recovers or the fallback fires, output must be
        // non-empty so the face stays visible. Outer + hole each have 4
        // vertices; CDT's annular topology gives 8 triangles, fan
        // fallback gives outer fan (2) + hole fan (2) = 4. Either is
        // acceptable; the contract is "geometry is not dropped".
        assert!(
            !tris.is_empty(),
            "fallback must produce some triangles even if CDT fails"
        );
    }

    #[test]
    fn tessellate_polygon_with_hole_uses_cdt() {
        // Outer 2x2 quad (CCW) with a 1x1 hole (CW).
        let polygon = Polygon {
            vertices: vec![
                p(0.0, 0.0, 0.0),
                p(2.0, 0.0, 0.0),
                p(2.0, 2.0, 0.0),
                p(0.0, 2.0, 0.0),
            ],
            holes: vec![vec![
                p(0.5, 0.5, 0.0),
                p(0.5, 1.5, 0.0),
                p(1.5, 1.5, 0.0),
                p(1.5, 0.5, 0.0),
            ]],
            plane_normal: Vec3::new(0.0, 0.0, 1.0),
            color: 0,
        };
        let tris = tessellate_polygon(&polygon);
        // Annular triangle count: V + 2H - 2 = 8 for 4 outer + 4 hole verts.
        assert_eq!(tris.len(), 8, "expected annular topological minimum");
    }

    /// Build a `csg::polygon::Polygon` directly from an explicit loop
    /// for `group_loops` containment tests. The plane is supplied by the
    /// caller so outer (CCW) and hole (CW) loops can share an identical
    /// cached plane — that's how the upstream CSG cleanup pipeline emits
    /// them, and `group_loops` groups by `canonical_key`, which keeps
    /// opposite-facing planes distinct.
    fn loop_poly(
        verts: Vec<Point3>,
        plane: csg::plane::Plane3,
        color: u32,
    ) -> csg::polygon::Polygon {
        csg::polygon::Polygon {
            vertices: verts,
            plane,
            color,
        }
    }

    fn xy_plane_pos_z() -> csg::plane::Plane3 {
        csg::plane::Plane3 {
            n_x: 0,
            n_y: 0,
            n_z: 1,
            d: 0,
        }
    }

    /// Issue 353: two disjoint coplanar outers share a plane and color,
    /// only one carries a hole. The hole must attach to its containing
    /// outer, not the other one.
    #[test]
    fn group_loops_attaches_hole_to_containing_outer_only() {
        // Outer A (CCW): the unit square at the origin.
        let outer_a = vec![
            p(0.0, 0.0, 0.0),
            p(1.0, 0.0, 0.0),
            p(1.0, 1.0, 0.0),
            p(0.0, 1.0, 0.0),
        ];
        // Outer B (CCW): another unit square offset by +5 on x —
        // disjoint from A on the same z=0 plane.
        let outer_b = vec![
            p(5.0, 0.0, 0.0),
            p(6.0, 0.0, 0.0),
            p(6.0, 1.0, 0.0),
            p(5.0, 1.0, 0.0),
        ];
        // Hole (CW) sitting inside outer A.
        let hole_a = vec![
            p(0.25, 0.25, 0.0),
            p(0.25, 0.75, 0.0),
            p(0.75, 0.75, 0.0),
            p(0.75, 0.25, 0.0),
        ];

        let plane = xy_plane_pos_z();
        let loops = vec![
            loop_poly(outer_a.clone(), plane, 7),
            loop_poly(outer_b.clone(), plane, 7),
            loop_poly(hole_a.clone(), plane, 7),
        ];
        let polys = group_loops(loops);
        assert_eq!(polys.len(), 2, "two outers => two output polygons");

        // The polygon with the hole must be the one whose first vertex
        // sits at the origin (outer A); the offset outer must come back
        // hole-free.
        let with_hole = polys
            .iter()
            .find(|p| !p.holes.is_empty())
            .expect("exactly one polygon should carry a hole");
        let without_hole = polys
            .iter()
            .find(|p| p.holes.is_empty())
            .expect("the disjoint outer should be hole-free");
        assert_eq!(with_hole.holes.len(), 1);
        assert_eq!(with_hole.vertices[0], p(0.0, 0.0, 0.0));
        assert_eq!(without_hole.vertices[0], p(5.0, 0.0, 0.0));
    }

    /// Issue 353: two outers each with their own hole on the same plane
    /// and color. Each polygon must receive exactly its hole.
    #[test]
    fn group_loops_routes_per_outer_holes_correctly() {
        let outer_a = vec![
            p(0.0, 0.0, 0.0),
            p(2.0, 0.0, 0.0),
            p(2.0, 2.0, 0.0),
            p(0.0, 2.0, 0.0),
        ];
        let outer_b = vec![
            p(5.0, 0.0, 0.0),
            p(7.0, 0.0, 0.0),
            p(7.0, 2.0, 0.0),
            p(5.0, 2.0, 0.0),
        ];
        let hole_a = vec![
            p(0.5, 0.5, 0.0),
            p(0.5, 1.5, 0.0),
            p(1.5, 1.5, 0.0),
            p(1.5, 0.5, 0.0),
        ];
        let hole_b = vec![
            p(5.5, 0.5, 0.0),
            p(5.5, 1.5, 0.0),
            p(6.5, 1.5, 0.0),
            p(6.5, 0.5, 0.0),
        ];

        let plane = xy_plane_pos_z();
        let loops = vec![
            loop_poly(outer_a, plane, 3),
            loop_poly(outer_b, plane, 3),
            loop_poly(hole_a.clone(), plane, 3),
            loop_poly(hole_b.clone(), plane, 3),
        ];
        let polys = group_loops(loops);
        assert_eq!(polys.len(), 2);
        for poly in &polys {
            assert_eq!(poly.holes.len(), 1, "each outer should own one hole");
            // The hole's first vertex must lie inside the outer's 2D
            // projection — sanity-check via x range.
            let hx = poly.holes[0][0].x;
            let outer_xs: Vec<i32> = poly.vertices.iter().map(|v| v.x).collect();
            let min_x = *outer_xs.iter().min().unwrap();
            let max_x = *outer_xs.iter().max().unwrap();
            assert!(
                hx > min_x && hx < max_x,
                "hole's first vert must be in its outer's x range"
            );
        }
    }

    /// Single-outer-with-hole case (the previous behaviour) still works:
    /// the smallest-containing-outer logic collapses to "the only outer
    /// gets the hole".
    #[test]
    fn group_loops_single_outer_with_hole_still_works() {
        let outer = vec![
            p(0.0, 0.0, 0.0),
            p(2.0, 0.0, 0.0),
            p(2.0, 2.0, 0.0),
            p(0.0, 2.0, 0.0),
        ];
        let hole = vec![
            p(0.5, 0.5, 0.0),
            p(0.5, 1.5, 0.0),
            p(1.5, 1.5, 0.0),
            p(1.5, 0.5, 0.0),
        ];
        let plane = xy_plane_pos_z();
        let loops = vec![loop_poly(outer, plane, 0), loop_poly(hole, plane, 0)];
        let polys = group_loops(loops);
        assert_eq!(polys.len(), 1);
        assert_eq!(polys[0].holes.len(), 1);
    }

    /// Hole with no containing outer (e.g. a stray inverted loop the
    /// upstream pipeline shouldn't have produced). We must not attach it
    /// to an arbitrary outer — drop it and let the warn surface.
    #[test]
    fn group_loops_drops_orphan_hole_with_no_containing_outer() {
        let outer = vec![
            p(0.0, 0.0, 0.0),
            p(1.0, 0.0, 0.0),
            p(1.0, 1.0, 0.0),
            p(0.0, 1.0, 0.0),
        ];
        // Hole (CW) sitting far away from the only outer — not contained.
        let stray_hole = vec![
            p(10.0, 10.0, 0.0),
            p(10.0, 11.0, 0.0),
            p(11.0, 11.0, 0.0),
            p(11.0, 10.0, 0.0),
        ];
        let plane = xy_plane_pos_z();
        let loops = vec![loop_poly(outer, plane, 1), loop_poly(stray_hole, plane, 1)];
        let polys = group_loops(loops);
        assert_eq!(polys.len(), 1);
        assert!(
            polys[0].holes.is_empty(),
            "orphan hole must not attach to a non-containing outer"
        );
    }

    /// The grouped result with two outer-and-hole pairs must produce
    /// valid CDT input — each polygon tessellates without dropping
    /// geometry.
    #[test]
    fn group_loops_output_tessellates_validly() {
        let outer_a = vec![
            p(0.0, 0.0, 0.0),
            p(2.0, 0.0, 0.0),
            p(2.0, 2.0, 0.0),
            p(0.0, 2.0, 0.0),
        ];
        let outer_b = vec![
            p(5.0, 0.0, 0.0),
            p(7.0, 0.0, 0.0),
            p(7.0, 2.0, 0.0),
            p(5.0, 2.0, 0.0),
        ];
        let hole_a = vec![
            p(0.5, 0.5, 0.0),
            p(0.5, 1.5, 0.0),
            p(1.5, 1.5, 0.0),
            p(1.5, 0.5, 0.0),
        ];
        let hole_b = vec![
            p(5.5, 0.5, 0.0),
            p(5.5, 1.5, 0.0),
            p(6.5, 1.5, 0.0),
            p(6.5, 0.5, 0.0),
        ];

        let plane = xy_plane_pos_z();
        let loops = vec![
            loop_poly(outer_a, plane, 4),
            loop_poly(outer_b, plane, 4),
            loop_poly(hole_a, plane, 4),
            loop_poly(hole_b, plane, 4),
        ];
        let polys = group_loops(loops);
        for poly in &polys {
            let tris = tessellate_polygon(poly);
            // Annular topology with 4 outer + 4 hole verts: 8 triangles.
            assert_eq!(
                tris.len(),
                8,
                "outer-with-hole should tessellate cleanly to 8 tris"
            );
        }
    }

    #[test]
    fn point_in_polygon_2d_basic_cases() {
        let square = vec![(0, 0), (10, 0), (10, 10), (0, 10)];
        assert!(point_in_polygon_2d((5, 5), &square));
        assert!(!point_in_polygon_2d((15, 5), &square));
        assert!(!point_in_polygon_2d((-1, -1), &square));
        // Degenerate polygon (< 3 verts) is empty.
        assert!(!point_in_polygon_2d((0, 0), &[(0, 0), (1, 1)]));
    }
}
