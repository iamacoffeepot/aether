// Surface nets is domain math: bounded grid-index integer counts cast to
// f32 for centroid placement, and `a`/`b`/`q`/`t0`/`t1` are the canonical
// edge / quad / triangle vocabulary — the same convention `mesh.rs` and
// `tessellate.rs` carry.
#![allow(clippy::cast_precision_loss, clippy::many_single_char_names)]

//! Naive surface-nets meshing of a dense scalar volume (issue 1868).
//!
//! The dual (one-vertex-per-cell) surface extractor: given a dense
//! `u32` sample grid and an iso threshold, it meshes only the interface
//! between the *inside* region (`value >= iso_threshold`) and the
//! *outside* region, so cost is O(boundary area) rather than O(volume
//! cells). An all-outside volume emits zero triangles; an all-inside
//! volume emits only its outer shell (clamped against a virtual outside
//! beyond the grid), not its interior.
//!
//! Library-only, per ADR-0053: this produces triangles, it does not
//! render. It sits alongside the DSL mesher (`crate::mesh`) as a second
//! triangle producer, reusing the crate's [`Triangle`] and
//! `aether_math::Vec3`. The reachability solver's [`ScalarField`]
//! (issue 1857) is the canonical input — its dense row-major
//! `values[t * H * W + y * W + x]` layout *is* the stacked volume, so a
//! caller meshes a field by passing `(x, y, t)` as `(x, y, z)` with
//! `depth = ticks`.
//!
//! The sweep is iterative and dense (no recursion over geometry, per
//! the load-bearing-code rule in `CLAUDE.md`): a single linear pass over
//! the cells places one centroid vertex per straddling cube, and a
//! second linear pass over the interior axis-aligned grid edges emits a
//! sign-wound quad (two [`Triangle`]s) per sign-change edge.
//!
//! [`ScalarField`]: https://docs.rs/aether-kinds

use crate::mesh::Triangle;
use aether_math::Vec3;

/// Flat color index handed to every emitted triangle. The viewer maps
/// this through its palette; a single index keeps the surface one solid
/// color (the boundary surface has no per-face material).
const SURFACE_COLOR: u32 = 0;

/// A sentinel for "no vertex placed in this cube". Cubes that do not
/// straddle the surface get no vertex; the quad pass skips any edge
/// whose four surrounding cubes are not all populated.
const NO_VERTEX: u32 = u32::MAX;

/// Extract the boundary surface of a dense scalar volume as a triangle
/// list, via naive (centroid-placed) surface nets.
///
/// `values` is a dense row-major `width * height * depth` grid of
/// samples, indexed `values[z * height * width + y * width + x]` — the
/// same layout the reachability solver's `ScalarField` uses with
/// `(x, y, tick)` mapped to `(x, y, z)`. A sample is *inside* iff
/// `value >= iso_threshold`; everything else is *outside*. The grid is
/// padded by one virtual-outside layer on every side, so an inside
/// region touching the volume edge caps cleanly against an outer shell.
///
/// Cubes sit between 8 corner samples. A cube *straddles* the surface
/// iff its 8 corners are not all the same side; only straddling cubes
/// contribute, which is the O(boundary area) property — interior-full
/// and interior-empty cubes are skipped. Each straddling cube gets one
/// vertex at its centroid, mapped to world as `origin + cell * c` where
/// `c` is the cube center in sample coordinates (the boundary shell's
/// cubes sit a half-cell outside the `[0, dim-1]` sample extent). For
/// each axis-aligned grid edge whose two endpoints differ in side, the
/// four cubes sharing that edge are wound into a quad (two triangles)
/// oriented so the front face points toward the outside region.
///
/// Returns an empty list when the volume has no cubes (any dimension
/// `< 2`), when `values.len()` does not match `width * height * depth`,
/// or when no cube straddles the surface.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn surface_net(
    width: usize,
    height: usize,
    depth: usize,
    values: &[u32],
    iso_threshold: u32,
    cell: Vec3,
    origin: Vec3,
) -> Vec<Triangle> {
    // Fewer than two samples along any axis means no cube exists, and a
    // wrong-length `values` is a malformed field — both mesh to nothing
    // rather than panicking (defensive against a bad decoded field).
    if width < 2 || height < 2 || depth < 2 {
        return Vec::new();
    }
    if values.len() != width * height * depth {
        return Vec::new();
    }

    // The grid is padded by one virtual-outside layer on every side, so
    // an inside region touching the volume edge caps cleanly against a
    // shell: a padded sample `(px, py, pz)` maps to the real sample
    // `(px - 1, py - 1, pz - 1)` when that lies in range, and is treated
    // as outside (below the threshold) otherwise. The padded sample grid
    // is `(width + 2, height + 2, depth + 2)`; its cubes are
    // `(width + 1) * (height + 1) * (depth + 1)`. The outermost cubes
    // straddle the original boundary, producing the outer shell that a
    // bare-grid sweep would miss.
    let pw = width + 2;
    let ph = height + 2;
    let pd = depth + 2;

    let inside = |px: usize, py: usize, pz: usize| -> bool {
        if px == 0 || py == 0 || pz == 0 || px > width || py > height || pz > depth {
            return false; // virtual outside
        }
        let (x, y, z) = (px - 1, py - 1, pz - 1);
        values[z * height * width + y * width + x] >= iso_threshold
    };

    let cells_x = pw - 1;
    let cells_y = ph - 1;
    let cells_z = pd - 1;

    // Pass 1: place one centroid vertex per straddling cube. `vertex`
    // holds the index into `positions` for each cube, or `NO_VERTEX`.
    let cell_count = cells_x * cells_y * cells_z;
    let mut vertex = vec![NO_VERTEX; cell_count];
    let mut positions: Vec<Vec3> = Vec::new();

    let cube_index = |i: usize, j: usize, k: usize| -> usize { (k * cells_y + j) * cells_x + i };

    for k in 0..cells_z {
        for j in 0..cells_y {
            for i in 0..cells_x {
                // The cube's 8 corners span padded samples
                // (i..=i+1, j..=j+1, k..=k+1). Count how many are inside;
                // a cube straddles iff the count is neither 0 nor 8.
                let mut inside_count = 0u8;
                for dk in 0..2 {
                    for dj in 0..2 {
                        for di in 0..2 {
                            if inside(i + di, j + dj, k + dk) {
                                inside_count += 1;
                            }
                        }
                    }
                }
                if inside_count == 0 || inside_count == 8 {
                    continue;
                }
                // The padded cube `(i, j, k)` centers between padded
                // samples `i` and `i+1`, i.e. real coordinate
                // `(i - 1) + 0.5 = i - 0.5`. World placement maps a real
                // coordinate `c` to `origin + cell * c`, so the boundary
                // shell's cubes sit a half-cell outside the sample extent.
                let centroid = origin
                    + Vec3::new(
                        cell.x * (i as f32 - 0.5),
                        cell.y * (j as f32 - 0.5),
                        cell.z * (k as f32 - 0.5),
                    );
                // The vertex count is bounded by the boundary cell count;
                // a field with more than `u32::MAX - 1` straddling cubes
                // (4 billion) is not representable, so this never
                // truncates and never collides with `NO_VERTEX`.
                #[allow(clippy::cast_possible_truncation)]
                let idx = positions.len() as u32;
                positions.push(centroid);
                vertex[cube_index(i, j, k)] = idx;
            }
        }
    }

    if positions.is_empty() {
        return Vec::new();
    }

    // Pass 2: for each interior padded-grid axis-aligned edge whose two
    // endpoints differ in side, the four cubes sharing that edge form a
    // quad. "Interior" is in the *padded* grid — the padding guarantees
    // every original boundary edge now has four surrounding cubes, so the
    // outer shell is emitted. The four cubes are addressed by stepping
    // back along the two axes orthogonal to the edge.
    let mut triangles: Vec<Triangle> = Vec::new();

    // X-axis edges: endpoints (x, y, z)-(x+1, y, z). The four cubes
    // sharing this edge vary in (j, k) around (y, z).
    for z in 1..cells_z {
        for y in 1..cells_y {
            for x in 0..cells_x {
                let a = inside(x, y, z);
                let b = inside(x + 1, y, z);
                if a == b {
                    continue;
                }
                let q = [
                    vertex[cube_index(x, y - 1, z - 1)],
                    vertex[cube_index(x, y, z - 1)],
                    vertex[cube_index(x, y, z)],
                    vertex[cube_index(x, y - 1, z)],
                ];
                emit_quad(&mut triangles, &positions, q, a);
            }
        }
    }

    // Y-axis edges: endpoints (x, y, z)-(x, y+1, z). Four cubes vary in
    // (i, k). The (i, k) winding has the opposite handedness to the
    // (j, k) edge above for the same sign direction, so flip the
    // sign-driven order to keep normals outward.
    for z in 1..cells_z {
        for y in 0..cells_y {
            for x in 1..cells_x {
                let a = inside(x, y, z);
                let b = inside(x, y + 1, z);
                if a == b {
                    continue;
                }
                let q = [
                    vertex[cube_index(x - 1, y, z - 1)],
                    vertex[cube_index(x, y, z - 1)],
                    vertex[cube_index(x, y, z)],
                    vertex[cube_index(x - 1, y, z)],
                ];
                emit_quad(&mut triangles, &positions, q, !a);
            }
        }
    }

    // Z-axis edges: endpoints (x, y, z)-(x, y, z+1). Four cubes vary in
    // (i, j).
    for z in 0..cells_z {
        for y in 1..cells_y {
            for x in 1..cells_x {
                let a = inside(x, y, z);
                let b = inside(x, y, z + 1);
                if a == b {
                    continue;
                }
                let q = [
                    vertex[cube_index(x - 1, y - 1, z)],
                    vertex[cube_index(x, y - 1, z)],
                    vertex[cube_index(x, y, z)],
                    vertex[cube_index(x - 1, y, z)],
                ];
                emit_quad(&mut triangles, &positions, q, a);
            }
        }
    }

    triangles
}

/// Fan a quad of four cube-vertices into two triangles, wound so the
/// front face points toward the outside region.
///
/// `q` is the four surrounding cube vertex indices in CCW order around
/// the edge as seen from the positive edge direction; `endpoint0_inside`
/// is whether the edge's first endpoint is inside. When it is, the
/// inside region sits "behind" the CCW order, so the quad is wound in
/// the listed order; otherwise the order is reversed so the front face
/// flips to face the (now opposite) outside. Any quad with an
/// unpopulated corner (an edge on the volume's outer face) is dropped —
/// the four cubes must all straddle for a closed quad.
fn emit_quad(out: &mut Vec<Triangle>, positions: &[Vec3], q: [u32; 4], endpoint0_inside: bool) {
    if q.contains(&NO_VERTEX) {
        return;
    }
    let p = [
        positions[q[0] as usize],
        positions[q[1] as usize],
        positions[q[2] as usize],
        positions[q[3] as usize],
    ];
    // CCW fan (0,1,2)+(0,2,3) faces one way; reverse for the other sign.
    let (t0, t1) = if endpoint0_inside {
        ([p[0], p[1], p[2]], [p[0], p[2], p[3]])
    } else {
        ([p[0], p[2], p[1]], [p[0], p[3], p[2]])
    };
    out.push(Triangle {
        vertices: t0,
        color: SURFACE_COLOR,
    });
    out.push(Triangle {
        vertices: t1,
        color: SURFACE_COLOR,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_math::Aabb;

    /// Build a dense `w*h*d` volume from a closure classifying each
    /// grid point as inside (`true` → 1) or outside (`false` → 0).
    fn volume(w: usize, h: usize, d: usize, f: impl Fn(usize, usize, usize) -> bool) -> Vec<u32> {
        let mut v = vec![0u32; w * h * d];
        for z in 0..d {
            for y in 0..h {
                for x in 0..w {
                    if f(x, y, z) {
                        v[z * h * w + y * w + x] = 1;
                    }
                }
            }
        }
        v
    }

    fn unit() -> (Vec3, Vec3) {
        (Vec3::splat(1.0), Vec3::ZERO)
    }

    /// A single inside sample at the center of a 3x3x3 volume meshes to
    /// a closed surface of 12 triangles — exactly the box=12 precedent.
    /// The lone inside sample is shared by 8 straddling cubes, giving 8
    /// vertices and the 6 interior sign-change edges incident on it,
    /// each emitting one quad (two triangles) → 12 triangles.
    #[test]
    fn single_inside_sample_meshes_closed_cube() {
        let (cell, origin) = unit();
        let v = volume(3, 3, 3, |x, y, z| (x, y, z) == (1, 1, 1));
        let tris = surface_net(3, 3, 3, &v, 1, cell, origin);
        assert_eq!(tris.len(), 12, "single voxel → 12-triangle closed cube");
    }

    /// An all-outside volume emits nothing — no cube straddles.
    #[test]
    fn all_outside_emits_no_triangles() {
        let (cell, origin) = unit();
        let v = volume(4, 4, 4, |_, _, _| false);
        let tris = surface_net(4, 4, 4, &v, 1, cell, origin);
        assert!(tris.is_empty(), "all-outside → 0 triangles");
    }

    /// An all-inside volume emits only its outer shell against the
    /// virtual-outside padding, and the count tracks boundary area, not
    /// interior volume (the O(area) guarantee). Growing only the z-extent
    /// of a fully-inside box adds side-wall area but leaves every interior
    /// cube fully inside (non-straddling, zero vertices), so the triangle
    /// count grows by the added wall area alone.
    #[test]
    fn all_inside_emits_only_shell_independent_of_depth() {
        let (cell, origin) = unit();
        let shallow = volume(3, 3, 3, |_, _, _| true);
        let deep = volume(3, 3, 5, |_, _, _| true);
        let tris_shallow = surface_net(3, 3, 3, &shallow, 1, cell, origin);
        let tris_deep = surface_net(3, 3, 5, &deep, 1, cell, origin);
        assert!(!tris_shallow.is_empty(), "full cube has an outer shell");
        // The deeper box has taller side walls (more boundary area), so a
        // strictly larger shell — but nowhere near a volume-scaling pass:
        // every fully-interior cube stays non-straddling and contributes
        // nothing.
        assert!(
            tris_deep.len() > tris_shallow.len(),
            "taller box has more side-wall area: shallow={} deep={}",
            tris_shallow.len(),
            tris_deep.len(),
        );
        // The all-inside shell is a closed box. Each of the 3x3x3 volume's
        // 6 faces has 3x3 = 9 boundary samples, each emitting one outward
        // sign-change edge → one quad → 9 quads per face. 6 * 9 = 54 quads
        // → 108 triangles. The interior is never meshed (the O(area)
        // guarantee, not O(volume)).
        assert_eq!(
            tris_shallow.len(),
            6 * 3 * 3 * 2,
            "3x3x3 full box shell = 6 faces * 9 quads * 2 tris",
        );
    }

    /// A volume with an interior empty pocket emits more triangles than
    /// the bare outer shell — the pocket is a tunnel/cavity boundary —
    /// and the meshed vertices' bounding box matches the volume extent.
    #[test]
    fn interior_pocket_adds_surface_and_bbox_matches_extent() {
        let (cell, origin) = unit();
        let w = 5;
        let h = 5;
        let d = 5;
        let full = volume(w, h, d, |_, _, _| true);
        // Carve a single empty sample at the center.
        let pocket = volume(w, h, d, |x, y, z| (x, y, z) != (2, 2, 2));
        let tris_full = surface_net(w, h, d, &full, 1, cell, origin);
        let tris_pocket = surface_net(w, h, d, &pocket, 1, cell, origin);
        assert!(
            tris_pocket.len() > tris_full.len(),
            "an interior empty pocket adds boundary surface (a cavity)",
        );
        // The vertex bounding box spans the outer shell's cube centroids,
        // which sit a half-cell outside the sample extent: real coordinate
        // -0.5 at the low side, (dim-1)+0.5 at the high side along each
        // axis.
        let pts: Vec<Vec3> = tris_pocket
            .iter()
            .flat_map(|t| t.vertices.iter().copied())
            .collect();
        let bbox = Aabb::from_points(&pts);
        let lo = -0.5_f32;
        let hi_x = (w - 1) as f32 + 0.5;
        let hi_y = (h - 1) as f32 + 0.5;
        let hi_z = (d - 1) as f32 + 0.5;
        let eps = 1e-4;
        assert!((bbox.min.x - lo).abs() < eps, "bbox min.x = {}", bbox.min.x);
        assert!((bbox.min.y - lo).abs() < eps, "bbox min.y = {}", bbox.min.y);
        assert!((bbox.min.z - lo).abs() < eps, "bbox min.z = {}", bbox.min.z);
        assert!(
            (bbox.max.x - hi_x).abs() < eps,
            "bbox max.x = {}",
            bbox.max.x
        );
        assert!(
            (bbox.max.y - hi_y).abs() < eps,
            "bbox max.y = {}",
            bbox.max.y
        );
        assert!(
            (bbox.max.z - hi_z).abs() < eps,
            "bbox max.z = {}",
            bbox.max.z
        );
    }

    /// The single-voxel cube's faces wind outward: every triangle's
    /// geometric normal points away from the voxel center. The voxel
    /// sample sits at world `(1, 1, 1)`; each surrounding centroid sits at
    /// a corner offset of `±0.5`, so the closed surface is the cube
    /// `[0.5, 1.5]` cubed centered at `(1, 1, 1)`, and an outward normal
    /// has a positive dot with `face_centroid - cube_center`.
    #[test]
    fn single_voxel_winds_outward() {
        let (cell, origin) = unit();
        let v = volume(3, 3, 3, |x, y, z| (x, y, z) == (1, 1, 1));
        let tris = surface_net(3, 3, 3, &v, 1, cell, origin);
        let center = Vec3::new(1.0, 1.0, 1.0);
        for t in &tris {
            let [a, b, c] = t.vertices;
            let normal = (b - a).cross(c - a);
            let face_center = (a + b + c) * (1.0 / 3.0);
            let outward = face_center - center;
            assert!(
                normal.dot(outward) > 0.0,
                "triangle normal must face outward: n·out = {}",
                normal.dot(outward),
            );
        }
    }

    /// `iso_threshold = 1` classifies cost-0 cells outside and every
    /// positive value — the `u32::MAX` unreachable sentinel included —
    /// inside, with no special case and no panic. A volume with a single
    /// `u32::MAX` sample meshes the same closed cube as a single `1`.
    #[test]
    fn sentinel_classified_inside_no_panic() {
        let (cell, origin) = unit();
        let mut v = volume(3, 3, 3, |_, _, _| false);
        // Center sample at (x, y, z) = (1, 1, 1) in a 3x3x3 grid:
        // index z*H*W + y*W + x = 1*9 + 1*3 + 1 = 13.
        v[13] = u32::MAX;
        let tris = surface_net(3, 3, 3, &v, 1, cell, origin);
        assert_eq!(
            tris.len(),
            12,
            "a u32::MAX sample is inside under iso_threshold = 1",
        );
    }

    /// Degenerate dimensions (a single slice along any axis) have no
    /// cubes and mesh to nothing — boundary clamping never indexes out
    /// of range.
    #[test]
    fn degenerate_dimensions_emit_nothing() {
        let (cell, origin) = unit();
        for (w, h, d) in [(1, 3, 3), (3, 1, 3), (3, 3, 1)] {
            let v = volume(w, h, d, |_, _, _| true);
            let tris = surface_net(w, h, d, &v, 1, cell, origin);
            assert!(
                tris.is_empty(),
                "a {w}x{h}x{d} volume has no cells → 0 triangles",
            );
        }
    }

    /// A mismatched `values` length returns empty rather than panicking
    /// — defensive against a malformed decoded field.
    #[test]
    fn mismatched_length_returns_empty() {
        let (cell, origin) = unit();
        let v = vec![1u32; 10]; // not 3*3*3 = 27
        let tris = surface_net(3, 3, 3, &v, 1, cell, origin);
        assert!(tris.is_empty(), "wrong-length values → 0 triangles");
    }

    /// Non-unit `cell` and a non-zero `origin` place vertices in world
    /// space at `origin + cell * (i+0.5)`.
    #[test]
    fn cell_and_origin_place_vertices_in_world() {
        let cell = Vec3::new(2.0, 3.0, 4.0);
        let origin = Vec3::new(10.0, 20.0, 30.0);
        let v = volume(3, 3, 3, |x, y, z| (x, y, z) == (1, 1, 1));
        let tris = surface_net(3, 3, 3, &v, 1, cell, origin);
        let pts: Vec<Vec3> = tris
            .iter()
            .flat_map(|t| t.vertices.iter().copied())
            .collect();
        let bbox = Aabb::from_points(&pts);
        // Centroids at i in {0, 1}: x = 10 + 2*(0.5) = 11 and
        // 10 + 2*(1.5) = 13; y = 20 + 3*0.5 = 21.5 .. 24.5;
        // z = 30 + 4*0.5 = 32 .. 36.
        let eps = 1e-4;
        assert!((bbox.min.x - 11.0).abs() < eps);
        assert!((bbox.max.x - 13.0).abs() < eps);
        assert!((bbox.min.y - 21.5).abs() < eps);
        assert!((bbox.max.y - 24.5).abs() < eps);
        assert!((bbox.min.z - 32.0).abs() < eps);
        assert!((bbox.max.z - 36.0).abs() < eps);
    }
}
