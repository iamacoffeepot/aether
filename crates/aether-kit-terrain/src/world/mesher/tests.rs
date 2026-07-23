use core::ops::Range;

use aether_render::{DrawTriangle, Vertex};

use super::atlas_support::{
    assert_height_break_walls_close, quantized_vertex_octimeters, total_wall_top_edge_length, xz_area_doubled, y_span,
};
use super::cliffs::WindowCenter;
use super::constants::{COVERAGE_LIFT, EDGE, OCTIMETERS_PER_METER, SUB};
use super::coverage::{overlay_materials, subcell_coverage};
use super::style::{StyleTable, flat_color};
use super::*;
use crate::world::{
    CELLS_PER_CHUNK_AREA, CellPos, Chunk, ChunkPos, Material, Region, STEP_MAX_OCTIMETERS, SUBCELLS_PER_CELL, SetChunk,
    SmoothingProfile, UNDERLAY_POINT_INHERIT, World,
};

fn sub4(n: i32) -> i32 {
    n * SUB / 4
}

/// A world of one chunk whose underlay is filled per a closure.
fn world_with_underlay(pos: ChunkPos, fill: impl Fn(i32, i32) -> Material) -> World {
    let mut chunk = Chunk::empty();
    for lz in 0..EDGE {
        for lx in 0..EDGE {
            chunk.underlay[(lz * EDGE + lx) as usize] = fill(lx, lz);
        }
    }
    let mut world = World::new();
    world.insert_chunk(pos, chunk);
    world
}

#[test]
fn operator_remesh_preflight_pins_the_apron_safe_chunk_boundaries() {
    assert!(chunk_remesh_extent_is_coordinate_safe(ChunkPos { x: -524_286, z: 524_285 }));
    assert!(!chunk_remesh_extent_is_coordinate_safe(ChunkPos { x: -524_287, z: 0 }));
    assert!(!chunk_remesh_extent_is_coordinate_safe(ChunkPos { x: 524_286, z: 0 }));
}

/// Two chunks of grass-default region with an explicit sand band over
/// cells `10..22 × 4..8` (crossing the chunk border at `x = 16`).
/// `profile` paints every cell's smoothing plane with the given
/// profile-1 settings; `None` leaves the material defaults governing.
fn sand_band_world(profile: Option<SmoothingProfile>) -> World {
    let mut world = World::new();
    world.insert_region(
        1,
        Region { name: "meadow".into(), default_material: Material::Grass, cliff_material: Material::Stone },
    );
    if let Some(p) = profile {
        world.insert_smoothing_profile(1, p);
    }
    for cx in 0..2 {
        let mut chunk = Chunk::empty();
        chunk.region = [1; CELLS_PER_CHUNK_AREA];
        if profile.is_some() {
            chunk.smoothing = [1; CELLS_PER_CHUNK_AREA];
        }
        for lz in 4..8 {
            for lx in 0..EDGE {
                if (10..22).contains(&(cx * EDGE + lx)) {
                    chunk.underlay[(lz * EDGE + lx) as usize] = Material::Sand;
                }
            }
        }
        world.insert_chunk(ChunkPos { x: cx, z: 0 }, chunk);
    }
    world
}

/// Does `t` (projected to the ground plane) cover point `(px, pz)`?
fn covers(t: &DrawTriangle, px: f32, pz: f32) -> bool {
    let sign = |ax: f32, az: f32, bx: f32, bz: f32| (ax - bx).mul_add(-(pz - bz), (px - bx) * (az - bz));
    let d1 = sign(t.verts[0].x, t.verts[0].z, t.verts[1].x, t.verts[1].z);
    let d2 = sign(t.verts[1].x, t.verts[1].z, t.verts[2].x, t.verts[2].z);
    let d3 = sign(t.verts[2].x, t.verts[2].z, t.verts[0].x, t.verts[0].z);
    let has_neg = d1 < -1e-6 || d2 < -1e-6 || d3 < -1e-6;
    let has_pos = d1 > 1e-6 || d2 > 1e-6 || d3 > 1e-6;
    !(has_neg && has_pos)
}

fn ground_triangles(mesh: &[DrawTriangle]) -> impl Iterator<Item = &DrawTriangle> {
    mesh.iter().filter(|t| t.verts.iter().all(|v| v.y == 0.0))
}

fn ground_covers(mesh: &[DrawTriangle], px: f32, pz: f32) -> bool {
    ground_triangles(mesh).any(|t| covers(t, px, pz))
}

fn insert_material_chunks(world: &mut World, material: Material, smoothing: Option<u8>) {
    for dz in -1..=1 {
        for dx in -1..=1 {
            let mut chunk = Chunk::empty();
            chunk.underlay = [material; CELLS_PER_CHUNK_AREA];
            if let Some(profile) = smoothing {
                chunk.smoothing = [profile; CELLS_PER_CHUNK_AREA];
            }
            world.insert_chunk(ChunkPos { x: dx, z: dz }, chunk);
        }
    }
}

fn global_subcell(lx: i32, lz: i32, sx: usize, sz: usize) -> (i32, i32) {
    (lx * SUB + sx as i32, lz * SUB + sz as i32)
}

fn global_square_predicate(lx: i32, lz: i32, lo: i32, hi: i32) -> impl Fn(usize, usize) -> bool {
    move |sx, sz| {
        let (gx, gz) = global_subcell(lx, lz, sx, sz);
        (lo..hi).contains(&gx) && (lo..hi).contains(&gz)
    }
}

fn fill_subcell_square_where<T: Copy>(
    values: &mut [T; SUBCELLS_PER_CELL],
    value: T,
    in_square: impl Fn(usize, usize) -> bool,
) {
    let sub = SUB as usize;
    for sz in 0..sub {
        for sx in 0..sub {
            if in_square(sx, sz) {
                values[sz * sub + sx] = value;
            }
        }
    }
}

fn subcell_square_where(value: u8, in_square: impl Fn(usize, usize) -> bool) -> [u8; SUBCELLS_PER_CELL] {
    let mut points = [UNDERLAY_POINT_INHERIT; SUBCELLS_PER_CELL];
    fill_subcell_square_where(&mut points, value, in_square);
    points
}

fn subcell_square(value: u8, range: Range<usize>) -> [u8; SUBCELLS_PER_CELL] {
    subcell_square_where(value, |sx, sz| range.contains(&sx) && range.contains(&sz))
}

mod underlay {
    use super::*;

    #[test]
    fn partition_ground_has_no_gaps() {
        // The partition must tile the painted ground exactly — every probe
        // point strictly inside the chunk is covered by at least one
        // ground-plane triangle. A saddle-rule or window-skip bug shows up
        // as a hole here.
        let world = sand_band_world(None);
        let mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, &StyleTable::default());
        for j in 0..32 {
            for i in 0..32 {
                let px = (i as f32 + 0.37) * 0.5;
                let pz = (j as f32 + 0.53) * 0.5;
                assert!(ground_covers(&mesh, px, pz), "ground hole at ({px}, {pz})");
            }
        }
    }

    #[test]
    fn partition_chunks_tile_the_seam_together() {
        // Every boundary window is owned by exactly one chunk (the one
        // holding its center cell), so neither mesh alone covers the whole
        // seam strip — but their union must, with no gap. An ownership or
        // classification disagreement between the two chunks shows up as a
        // hole here.
        let world = sand_band_world(None);
        let mesh0 = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, &StyleTable::default());
        let mesh1 = mesh_chunk(&world, ChunkPos { x: 1, z: 0 }, &StyleTable::default());
        for j in 0..64 {
            let pz = (j as f32).mul_add(0.25, 0.13);
            for i in 0..16 {
                let px = (i as f32).mul_add(0.125, 15.06);
                assert!(ground_covers(&mesh0, px, pz) || ground_covers(&mesh1, px, pz), "seam hole at ({px}, {pz})");
            }
        }
    }

    /// The sand-band world over a gentle eastward ramp — heights step 8
    /// octimeters per cell, capped at the step ceiling so nothing cliffs
    /// anywhere (a 64-octimeter drop against the unloaded zero-height
    /// surroundings is a legal step, not a cliff).
    fn ramp_world() -> World {
        let mut world = sand_band_world(None);
        for cx in 0..2 {
            let at = ChunkPos { x: cx, z: 0 };
            let mut chunk = world.chunk(at).expect("chunk").clone();
            for lz in 0..EDGE {
                for lx in 0..EDGE {
                    let global_x = cx * EDGE + lx;
                    chunk.height[(lz * EDGE + lx) as usize] = (8 * global_x).min(STEP_MAX_OCTIMETERS);
                }
            }
            world.insert_chunk(at, chunk);
        }
        world
    }

    #[test]
    fn drawn_ground_equals_stood_on_height() {
        // The drawn-equals-stood-on tripwire: on a cliff-free ramp every
        // vertex sits exactly on `World::surface_height` — the same patch
        // a mover will stand on. Any lift path that misses a vertex
        // (nine-slice, strip, window poly) diverges here.
        let world = ramp_world();
        let mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, &StyleTable::default());
        assert!(!mesh.is_empty());
        for v in mesh.iter().flat_map(|t| t.verts.iter()) {
            let surface = world.surface_height(v.x, v.z);
            assert!((v.y - surface).abs() < 1e-4, "vertex ({}, {}) drawn at {} but stood on {surface}", v.x, v.z, v.y);
        }
    }

    #[test]
    fn ramp_ground_still_has_no_gaps() {
        // The no-gap probe on sloped ground: the strip fallback and the
        // per-window bilinear quads must tile exactly like the flat path.
        let world = ramp_world();
        let mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, &StyleTable::default());
        for j in 0..32 {
            for i in 0..32 {
                let px = (i as f32 + 0.37) * 0.5;
                let pz = (j as f32 + 0.53) * 0.5;
                assert!(mesh.iter().any(|t| covers(t, px, pz)), "ground hole at ({px}, {pz})");
            }
        }
    }

    #[test]
    fn a_same_material_cliff_stands_a_wall_on_the_cell_line() {
        // A grass plateau chunk one meter up beside a grass ground chunk:
        // same material, so the material partition leaves no marched
        // boundary and the wall stands on the shared cell-edge line — where
        // the old skirt stood. The face belongs to the high cell, so the
        // high chunk lofts it exactly once and the low chunk never does;
        // double emission would z-fight, omission would leave a window
        // through the terrain.
        let mut world = World::new();
        let mut low = Chunk::empty();
        low.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, low);
        let mut high = Chunk::empty();
        high.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        high.height = [256; CELLS_PER_CHUNK_AREA];
        world.insert_chunk(ChunkPos { x: 1, z: 0 }, high);

        let is_border_wall = |t: &DrawTriangle| {
            t.verts.iter().all(|v| (v.x - 16.0).abs() < 1e-6)
                && t.verts.iter().any(|v| v.y > 0.9)
                && t.verts.iter().any(|v| v.y < 0.1)
        };
        let low_mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, &StyleTable::default());
        let high_mesh = mesh_chunk(&world, ChunkPos { x: 1, z: 0 }, &StyleTable::default());
        assert!(high_mesh.iter().any(is_border_wall), "the high chunk stands the wall on the cell line");
        assert!(!low_mesh.iter().any(is_border_wall), "the low chunk does not double-draw the shared face");
    }

    #[test]
    fn a_material_boundary_at_a_cliff_does_not_drape() {
        // Sand plateau (1 m up) against grass ground, material boundary on
        // the cliff line: every colored (non-wall) triangle must stay on
        // its own side of the break — the cliff face belongs to the gray
        // wall. Position-pure lifting would stretch the boundary windows
        // into meter-tall sand and grass caps down the face.
        let mut world = World::new();
        let mut low = Chunk::empty();
        low.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        let mut high = Chunk::empty();
        high.underlay = [Material::Sand; CELLS_PER_CHUNK_AREA];
        high.height = [256; CELLS_PER_CHUNK_AREA];
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, low);
        world.insert_chunk(ChunkPos { x: 1, z: 0 }, high);

        for at in [ChunkPos { x: 0, z: 0 }, ChunkPos { x: 1, z: 0 }] {
            let mesh = mesh_chunk(&world, at, &StyleTable::default());
            for t in &mesh {
                let gray = t
                    .verts
                    .iter()
                    .all(|v| (v.color.r - v.color.b).abs() < 0.05 && (v.color.g - v.color.b).abs() < 0.05);
                if gray {
                    continue; // walls own the vertical face
                }
                let max_y = t.verts.iter().map(|v| v.y).fold(f32::MIN, f32::max);
                let min_y = t.verts.iter().map(|v| v.y).fold(f32::MAX, f32::min);
                assert!(
                    max_y - min_y < 0.5,
                    "chunk {at:?}: a colored triangle drapes the cliff (span {} at x {})",
                    max_y - min_y,
                    t.verts[0].x,
                );
            }
        }
    }

    #[test]
    fn void_chunk_meshes_to_nothing() {
        let mut world = World::new();
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, Chunk::empty());
        let tris = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, &StyleTable::default());
        assert!(tris.is_empty(), "an all-Void chunk emits no geometry");
    }
}

mod coverage {
    use super::*;

    #[test]
    fn overlay_helpers_collect_materials_and_gate_coverage() {
        let at = ChunkPos { x: 0, z: 0 };
        let mut chunk = Chunk::empty();
        let stone_cell = (8 * EDGE + 8) as usize;
        let grass_cell = (2 * EDGE + 2) as usize;
        chunk.overlay[stone_cell] = Material::Stone;
        chunk.overlay[grass_cell] = Material::Grass;
        chunk.overlay_mask[stone_cell * SUBCELLS_PER_CELL + 3] = 77;
        let mut world = World::new();
        world.insert_chunk(at, chunk);

        assert_eq!(
            overlay_materials(&world, at),
            vec![Material::Grass, Material::Stone],
            "overlay materials are distinct and stable by material id",
        );
        assert_eq!(
            subcell_coverage(&world, at, 8 * SUB + 3, 8 * SUB, Material::Stone),
            77,
            "matching overlay material exposes its scalar mask sample",
        );
        assert_eq!(
            subcell_coverage(&world, at, 8 * SUB + 3, 8 * SUB, Material::Grass),
            0,
            "off-material coverage is treated as uncovered",
        );
    }

    #[test]
    fn scalar_overlay_mask_marches_at_authored_fraction() {
        // One authored overlay cell carries a scalar vertical edge. The
        // contour pass first reconstructs scalar zones bilinearly, then
        // marches the 127.5 crossing through that reconstructed grid. A
        // nearest/blocky path would land at the half-cell edge instead.
        let at = ChunkPos { x: 0, z: 0 };
        let cell = CellPos { x: 8, z: 8 };
        let cell_idx = (cell.z * EDGE + cell.x) as usize;
        let sub = SUB as usize;
        let mut overlay = vec![Material::Void.to_u8(); CELLS_PER_CHUNK_AREA];
        overlay[cell_idx] = Material::Stone.to_u8();
        let mut overlay_mask = vec![0u8; CELLS_PER_CHUNK_AREA * SUBCELLS_PER_CELL];
        for sz in 0..sub {
            for sx in 0..sub {
                overlay_mask[cell_idx * SUBCELLS_PER_CELL + sz * sub + sx] = if sx < sub / 2 {
                    96
                } else {
                    192
                };
            }
        }
        let mut world = World::new();
        world.insert_chunk(
            at,
            SetChunk {
                chunk_x: 0,
                chunk_z: 0,
                underlay: vec![Material::Grass.to_u8(); CELLS_PER_CHUNK_AREA],
                underlay_points: Vec::new(),
                height_points: Vec::new(),
                overlay,
                overlay_mask,
                height: Vec::new(),
                region: Vec::new(),
                water_plane: Vec::new(),
                smoothing: Vec::new(),
            }
            .into_chunk(),
        );

        let mesh = mesh_chunk(&world, at, &StyleTable::default());
        let coverage_verts: Vec<&Vertex> = mesh
            .iter()
            .filter(|t| t.verts.iter().all(|v| (v.y - COVERAGE_LIFT).abs() < 1e-6))
            .flat_map(|t| t.verts.iter())
            .collect();
        assert!(!coverage_verts.is_empty(), "the overlay coverage pass emits lifted contour geometry");

        let low_sample_x = cell.x as f32 + (sub / 2 - 2) as f32 / SUB as f32 + 0.5 / SUB as f32;
        let high_sample_x = cell.x as f32 + (sub / 2 - 1) as f32 / SUB as f32 + 0.5 / SUB as f32;
        let low_reconstructed = 96.0;
        let high_reconstructed = (96.0 + 192.0) * 0.5;
        let crossing_t = (127.5 - low_reconstructed) / (high_reconstructed - low_reconstructed);
        let expected_x = low_sample_x + (high_sample_x - low_sample_x) * crossing_t;
        let min_x = coverage_verts.iter().map(|v| v.x).fold(f32::MAX, f32::min);
        assert!((min_x - expected_x).abs() < 1e-4, "scalar contour crosses at {expected_x}, got {min_x}");
        assert!(
            (min_x - (cell.x as f32 + 0.5)).abs() > 1e-3,
            "the contour did not collapse to the blocky half-cell edge",
        );
    }
}

mod partition_inputs {
    use super::*;

    #[test]
    fn all_inherit_meshes_identically_to_cell_expansion() {
        // Tripwire: the underlay-point inherit sentinel must fold back to the
        // per-cell material exactly, so an unshaped world meshes byte-for-byte
        // as it did before points existed. A varied world meshed with
        // all-inherit points must equal the same world with every point
        // explicitly pinned to its cell's cascade material — the literal
        // cell-expansion `partition_inputs` performed before. Any drift in
        // `underlay_point`'s indexing or sentinel handling breaks this.
        let at = ChunkPos { x: 0, z: 0 };
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                let i = (lz * EDGE + lx) as usize;
                chunk.underlay[i] = match (lx + lz) % 4 {
                    0 => Material::Void, // falls through to the region default
                    1 => Material::Grass,
                    2 => Material::Sand,
                    _ => Material::Stone,
                };
                chunk.height[i] = 10 * lx;
                chunk.region[i] = 1;
            }
        }
        world.insert_chunk(at, chunk);
        world.insert_region(
            1,
            Region { name: "meadow".into(), default_material: Material::Dirt, cliff_material: Material::Stone },
        );

        let inherit_mesh = mesh_chunk(&world, at, &StyleTable::default());
        assert!(!inherit_mesh.is_empty(), "the fixture must mesh something");

        // Pin every cell's points to its own cascade material — the explicit
        // cell-expansion. Apron cells stay off-chunk and read Void either way.
        let mut expanded = world.clone();
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                let cell = CellPos { x: at.x * EDGE + lx, z: at.z * EDGE + lz };
                let material = expanded.underlay(cell).to_u8();
                expanded.set_cell_points(cell, &[material; SUBCELLS_PER_CELL]);
            }
        }
        let expanded_mesh = mesh_chunk(&expanded, at, &StyleTable::default());

        assert_eq!(inherit_mesh, expanded_mesh, "all-inherit points fold to the explicit cell-expansion");
    }

    #[test]
    fn a_point_shaped_cell_marches_inside_its_cell_span() {
        // A cell whose only authored material is the middle half-cell of
        // interior subcells (the rest inherit Void) marches a silhouette that
        // departs the cell-edge lines: the Grass/Void contour lands between
        // subcell centers, strictly inside the cell span, where a whole-cell
        // material would only bound at the integer cell edges.
        let at = ChunkPos { x: 0, z: 0 };
        let cell = CellPos { x: 8, z: 8 };
        let sub = SUB as usize;
        let points = subcell_square(Material::Grass.to_u8(), sub / 4..3 * sub / 4);
        let mut world = World::new();
        world.insert_chunk(at, Chunk::empty()); // all Void underlay
        world.set_cell_points(cell, &points);

        let mesh = mesh_chunk(&world, at, &StyleTable::default());
        assert!(!mesh.is_empty(), "the authored blob must mesh");

        let verts = mesh.iter().flat_map(|t| t.verts.iter());
        let inside_x = verts.clone().any(|v| v.x > cell.x as f32 + 0.1 && v.x < cell.x as f32 + 0.9);
        let inside_z = verts.clone().any(|v| v.z > cell.z as f32 + 0.1 && v.z < cell.z as f32 + 0.9);
        assert!(inside_x && inside_z, "the marched contour must depart the cell-edge lines");
        // And it stays bounded inside the cell — no vertex escapes the span.
        assert!(
            verts.clone().all(|v| {
                v.x >= cell.x as f32 - 1e-4
                    && v.x <= cell.x as f32 + 1.0 + 1e-4
                    && v.z >= cell.z as f32 - 1e-4
                    && v.z <= cell.z as f32 + 1.0 + 1e-4
            }),
            "the shaped silhouette stays inside the cell",
        );
    }
}

mod walls {
    use super::*;

    fn cell_cliff_world(break_x: i32) -> World {
        let mut world = World::new();
        for chunk_z in -1..=1 {
            for chunk_x in -1..=2 {
                let mut chunk = Chunk::empty();
                chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
                for local_z in 0..EDGE {
                    for local_x in 0..EDGE {
                        let global_x = chunk_x * EDGE + local_x;
                        chunk.height[(local_z * EDGE + local_x) as usize] = if global_x >= break_x {
                            256
                        } else {
                            0
                        };
                    }
                }
                world.insert_chunk(ChunkPos { x: chunk_x, z: chunk_z }, chunk);
            }
        }
        world
    }

    fn cap_cover_count(meshes: &[&[DrawTriangle]], x: f32, z: f32) -> usize {
        meshes
            .iter()
            .flat_map(|mesh| mesh.iter())
            .filter(|triangle| xz_area_doubled(triangle) > 1e-6 && covers(triangle, x, z))
            .count()
    }

    #[test]
    fn cliff_windows_disable_every_overlapped_cell_and_emit_one_cap_layer() {
        let internal = cell_cliff_world(8);
        let internal_plan = CliffPlan::build(&internal, ChunkPos { x: 0, z: 0 });
        assert!(internal_plan.cell_has_cliff(CellPos { x: 7, z: 8 }));
        assert!(internal_plan.cell_has_cliff(CellPos { x: 8, z: 8 }));
        let internal_mesh = mesh_chunk(&internal, ChunkPos { x: 0, z: 0 }, &StyleTable::default());
        assert_eq!(
            cap_cover_count(&[&internal_mesh], 7.981, 8.011),
            1,
            "the low half of an internal cliff window has one cap layer",
        );

        let seam = cell_cliff_world(16);
        let west_plan = CliffPlan::build(&seam, ChunkPos { x: 0, z: 0 });
        let east_plan = CliffPlan::build(&seam, ChunkPos { x: 1, z: 0 });
        assert!(west_plan.cell_has_cliff(CellPos { x: 15, z: 8 }));
        assert!(east_plan.cell_has_cliff(CellPos { x: 16, z: 8 }));
        assert!(!west_plan.has_window_at(WindowCenter { x_octimeters: 16 * 256, z_octimeters: 8 * 256 }));
        let west = mesh_chunk(&seam, ChunkPos { x: 0, z: 0 }, &StyleTable::default());
        let east = mesh_chunk(&seam, ChunkPos { x: 1, z: 0 }, &StyleTable::default());
        assert_eq!(
            cap_cover_count(&[&west, &east], 15.981, 8.011),
            1,
            "the neighbor-owned local-256 window has one fleet-wide cap layer",
        );
    }

    #[test]
    fn a_chord_uses_each_crossing_high_side_cliff_material() {
        let mut world = World::new();
        world.insert_region(
            1,
            Region { name: "lower".into(), default_material: Material::Grass, cliff_material: Material::Stone },
        );
        world.insert_region(
            2,
            Region { name: "upper".into(), default_material: Material::Grass, cliff_material: Material::Sand },
        );
        for chunk_z in -1..=1 {
            for chunk_x in -1..=1 {
                let mut chunk = Chunk::empty();
                chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
                for local_z in 0..EDGE {
                    for local_x in 0..EDGE {
                        let global_x = chunk_x * EDGE + local_x;
                        let global_z = chunk_z * EDGE + local_z;
                        let index = (local_z * EDGE + local_x) as usize;
                        chunk.height[index] = if global_x >= 8 {
                            256
                        } else {
                            0
                        };
                        chunk.region[index] = if global_z < 8 {
                            1
                        } else {
                            2
                        };
                    }
                }
                world.insert_chunk(ChunkPos { x: chunk_x, z: chunk_z }, chunk);
            }
        }

        let styles = StyleTable::default();
        let mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, &styles);
        let stone = flat_color(styles.get(Material::Stone));
        let sand = flat_color(styles.get(Material::Sand));
        let wall_half = |upper: bool| {
            mesh.iter().filter(move |triangle| {
                if xz_area_doubled(triangle) > 1e-6
                    || y_span(triangle) < 0.5
                    || !triangle.verts.iter().all(|vertex| (vertex.x - 8.0).abs() < 1e-6)
                {
                    return false;
                }
                let center_z = triangle.verts.iter().map(|vertex| vertex.z).sum::<f32>() / 3.0;
                let in_window = (center_z - 8.0).abs() < 1.0 / 32.0;
                in_window && (center_z > 8.0) == upper
            })
        };
        let lower: Vec<_> = wall_half(false).collect();
        let upper: Vec<_> = wall_half(true).collect();
        assert!(!lower.is_empty() && !upper.is_empty());
        assert!(lower.iter().all(|triangle| { triangle.verts.iter().all(|vertex| vertex.color == stone) }));
        assert!(upper.iter().all(|triangle| { triangle.verts.iter().all(|vertex| vertex.color == sand) }));
    }

    #[test]
    fn marched_wall_tops_land_on_the_cap_contour() {
        // The watertight pin: a material-boundary cliff lofts marched walls
        // whose top vertices are the cap contour's own vertices. Sand plateau
        // (1 m up, east) against grass ground: mesh the high chunk, split its
        // triangles into walls (a tall vertical span) and caps (near-flat),
        // and assert every wall-top vertex coincides exactly with a cap
        // vertex — no gap and no overlap between the cliff face and the
        // surfaces it joins. The wall lofts its top through the same
        // owner-clamped patch the cap drew, so the match is bit-exact.
        let mut world = World::new();
        let mut low = Chunk::empty();
        low.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        let mut high = Chunk::empty();
        high.underlay = [Material::Sand; CELLS_PER_CHUNK_AREA];
        high.height = [256; CELLS_PER_CHUNK_AREA];
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, low);
        world.insert_chunk(ChunkPos { x: 1, z: 0 }, high);

        let mesh = mesh_chunk(&world, ChunkPos { x: 1, z: 0 }, &StyleTable::default());
        let walls: Vec<&DrawTriangle> = mesh.iter().filter(|t| y_span(t) > 0.5).collect();
        assert!(!walls.is_empty(), "the material-boundary cliff lofts marched walls");
        let cap_verts: Vec<&Vertex> = mesh.iter().filter(|t| y_span(t) <= 0.5).flat_map(|t| t.verts.iter()).collect();
        for wall in &walls {
            let top = wall.verts.iter().map(|v| v.y).fold(f32::MIN, f32::max);
            for v in wall.verts.iter().filter(|v| (v.y - top).abs() < 1e-6) {
                assert!(
                    cap_verts
                        .iter()
                        .any(|c| (c.x - v.x).abs() < 1e-6 && (c.y - v.y).abs() < 1e-6 && (c.z - v.z).abs() < 1e-6),
                    "wall-top vertex ({}, {}, {}) has no matching cap vertex",
                    v.x,
                    v.y,
                    v.z,
                );
            }
        }
    }

    #[test]
    fn a_flat_world_emits_no_walls() {
        // Tripwire: a cliff-free world lofts no walls, so the wall pass adds
        // nothing — the full mesh equals the underlay mesh built without it,
        // byte for byte. A wall pass that fired on flat ground (or
        // double-counted a lattice against a marched segment) diverges here.
        let world = world_with_underlay(ChunkPos { x: 0, z: 0 }, |_, _| Material::Grass);
        let at = ChunkPos { x: 0, z: 0 };
        let styles = StyleTable::default();
        let mut expected = Vec::new();
        let cliffs = CliffPlan::build(&world, at);
        mesh_underlay(&world, at, &cliffs, &styles, &mut expected);
        normalize_cap_winding(&mut expected);
        let full = mesh_chunk(&world, at, &styles);
        assert_eq!(full, expected, "the wall pass adds nothing to a flat world");
    }

    #[test]
    fn adjacent_chunks_agree_on_a_wall_across_their_seam() {
        // A grass cliff running in x across the vertical chunk seam at
        // x = 16: cells at z < 8 sit 1 m up, z >= 8 at the datum, same
        // material so the face is a cell-edge lattice wall. Each chunk owns
        // its half of the line; at the shared seam the two halves must meet
        // on identical vertices, or the wall would gap or double where the
        // chunks join.
        let mut world = World::new();
        for cx in 0..2 {
            let mut chunk = Chunk::empty();
            chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
            for lz in 0..EDGE {
                for lx in 0..EDGE {
                    chunk.height[(lz * EDGE + lx) as usize] = if lz < 8 {
                        256
                    } else {
                        0
                    };
                }
            }
            world.insert_chunk(ChunkPos { x: cx, z: 0 }, chunk);
        }
        let wall_verts = |at| {
            let mesh = mesh_chunk(&world, at, &StyleTable::default());
            let mut verts: Vec<_> = mesh
                .iter()
                .filter(|t| y_span(t) > 0.5)
                .flat_map(|t| t.verts.iter())
                .filter(|v| (v.x - 16.0).abs() < 0.2)
                .map(quantized_vertex_octimeters)
                .collect();
            verts.sort_unstable();
            verts.dedup();
            verts
        };
        let west = wall_verts(ChunkPos { x: 0, z: 0 });
        let east = wall_verts(ChunkPos { x: 1, z: 0 });
        let shared: Vec<_> = west.iter().filter(|vertex| east.contains(vertex)).collect();
        assert!(
            shared.iter().any(|vertex| (vertex.x_octimeters - 16 * OCTIMETERS_PER_METER as i64).abs() <= 16),
            "neighbor-owned windows meet on an identical contour endpoint near the chunk seam",
        );
    }

    #[test]
    fn a_raised_point_cell_wears_marched_walls() {
        // A point-authored blob raised to a plateau wears walls that follow
        // its marched silhouette, not the cell edge: a half-cell grass point
        // block in one grass cell (the rest Void points) lifted 2 m stands a
        // Void-drop wall whose top vertices sit strictly inside the cell
        // span, where its Grass/Void contour marches — the cap-detaches-
        // from-the-cell-edge case the cell-edge skirt could not close.
        let at = ChunkPos { x: 0, z: 0 };
        let cell = CellPos { x: 8, z: 8 };
        let sub = SUB as usize;
        // Explicit Void points around a half-cell grass core, so the raised
        // cell's grass silhouette pulls inside its own edges.
        let mut points = [Material::Void.to_u8(); SUBCELLS_PER_CELL];
        for sub_z in sub / 4..3 * sub / 4 {
            for sub_x in sub / 4..3 * sub / 4 {
                points[sub_z * sub + sub_x] = Material::Grass.to_u8();
            }
        }
        // Surround the authored points with lower grass ground. The grass
        // core carries the 512-octimeter point lift while its explicit Void
        // surround stays at the datum, so every silhouette crossing is a
        // physical cliff adjacency (the bounded planner never invents a
        // wall from material labels alone).
        let mut chunk = Chunk::empty();
        chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        let mut world = World::new();
        world.insert_chunk(at, chunk);
        world.set_cell_points(cell, &points);
        let mut deltas = [0i16; SUBCELLS_PER_CELL];
        for sub_z in sub / 4..3 * sub / 4 {
            for sub_x in sub / 4..3 * sub / 4 {
                deltas[sub_z * sub + sub_x] = 512;
            }
        }
        world.set_cell_heights(cell, &deltas);

        let mesh = mesh_chunk(&world, at, &StyleTable::default());
        let walls: Vec<&DrawTriangle> = mesh.iter().filter(|t| y_span(t) > 0.5).collect();
        assert!(!walls.is_empty(), "the raised blob wears walls");
        let inside = walls.iter().flat_map(|t| t.verts.iter()).any(|v| {
            v.x > cell.x as f32 + 0.1
                && v.x < cell.x as f32 + 0.9
                && v.z > cell.z as f32 + 0.1
                && v.z < cell.z as f32 + 0.9
        });
        assert!(inside, "the walls follow the marched silhouette inside the cell");
    }

    #[test]
    fn drawn_relief_equals_stood_on() {
        // drawn≡stood-on over authored subcell relief: a continuous per-point
        // hill (deltas within the step ceiling, returning to zero at the chunk
        // edges) lofts no walls, and every cap vertex sits exactly on
        // `World::surface_height` — the same subcell patch a mover stands on.
        let at = ChunkPos { x: 0, z: 0 };
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        world.insert_chunk(at, chunk);
        let sub = SUB as usize;
        // A pyramid: 12 octimeters per subcell up to the middle and back down
        // on both axes, flat (zero) by every chunk edge so the grass never
        // stands above the Void surround and nothing cliffs anywhere.
        let density_scale = SUB / 4;
        let ramp = |g: i32| (12 * g.min(sub4(60) - g).max(0) / density_scale) as i16;
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                let mut deltas = [0i16; SUBCELLS_PER_CELL];
                for sz in 0..sub {
                    for sx in 0..sub {
                        let dx = ramp(lx * SUB + sx as i32);
                        let dz = ramp(lz * SUB + sz as i32);
                        deltas[sz * sub + sx] = dx.min(dz);
                    }
                }
                world.set_cell_heights(CellPos { x: lx, z: lz }, &deltas);
            }
        }
        let mesh = mesh_chunk(&world, at, &StyleTable::default());
        assert!(!mesh.is_empty(), "the relief cell meshes caps");
        assert!(mesh.iter().all(|t| y_span(t) < 0.5), "a continuous hill lofts no walls");
        let mut raised = false;
        for v in mesh.iter().flat_map(|t| t.verts.iter()) {
            let surface = world.surface_height(v.x, v.z);
            assert!(
                (v.y - surface).abs() < 2e-3,
                "cap vertex ({}, {}) drawn at {} but stood on {surface}",
                v.x,
                v.z,
                v.y,
            );
            raised |= v.y > 1.0; // the ridge peaks near 360/256 ≈ 1.4 m
        }
        assert!(raised, "the authored relief actually lifts the cap");
    }

    /// Author a stone diamond (rotated square) at point resolution centered on
    /// subcell `(34, 34)` with subcell radius `r`, raised `lift` octimeters on
    /// a Void surround; the diamond's edges cross cell interiors diagonally.
    fn diamond_world(r: i32, lift: i16) -> World {
        let at = ChunkPos { x: 0, z: 0 };
        let mut world = World::new();
        world.insert_chunk(at, Chunk::empty()); // all-Void underlay
        let sub = SUB as usize;
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                let mut points = [Material::Void.to_u8(); SUBCELLS_PER_CELL];
                let mut deltas = [0i16; SUBCELLS_PER_CELL];
                for sz in 0..sub {
                    for sx in 0..sub {
                        let gx = lx * SUB + sx as i32;
                        let gz = lz * SUB + sz as i32;
                        if (gx - sub4(34)).abs() + (gz - sub4(34)).abs() <= sub4(r) {
                            points[sz * sub + sx] = Material::Stone.to_u8();
                            deltas[sz * sub + sx] = lift;
                        }
                    }
                }
                let cell = CellPos { x: lx, z: lz };
                world.set_cell_points(cell, &points);
                world.set_cell_heights(cell, &deltas);
            }
        }
        world
    }

    #[test]
    fn an_authored_diamond_wears_walls_on_its_silhouette() {
        // A point-authored stone diamond raised 300 octimeters wears walls on
        // its marched silhouette (which crosses cell interiors diagonally),
        // with no wall on an interior cell edge between two fully-raised cells.
        let at = ChunkPos { x: 0, z: 0 };
        let world = diamond_world(12, 300);
        let mesh = mesh_chunk(&world, at, &StyleTable::default());
        let walls: Vec<&DrawTriangle> = mesh.iter().filter(|t| y_span(t) > 0.5).collect();
        assert!(!walls.is_empty(), "the raised diamond wears walls");

        // The silhouette departs the cell grid: some wall vertex sits strictly
        // inside a cell, off every integer lattice line.
        let departs = walls.iter().flat_map(|t| t.verts.iter()).any(|v| {
            let fx = v.x - v.x.floor();
            let fz = v.z - v.z.floor();
            (0.1..0.9).contains(&fx) && (0.1..0.9).contains(&fz)
        });
        assert!(departs, "walls follow the diagonal silhouette, not cell edges");

        // No wall on the interior cell edge x = 8 between cells (7,8) and
        // (8,8) — both fully inside the diamond (the boundary crosses x = 8
        // only at z = 6 and z = 11), so a fused interior stands no wall.
        let interior_edge =
            walls.iter().flat_map(|t| t.verts.iter()).any(|v| (v.x - 8.0).abs() < 1e-4 && (8.05..8.95).contains(&v.z));
        assert!(!interior_edge, "no wall vertex sits on an interior cell edge inside the diamond");

        // And the cap actually rides the raised plane.
        let peak =
            mesh.iter().filter(|t| y_span(t) < 0.5).flat_map(|t| t.verts.iter()).map(|v| v.y).fold(f32::MIN, f32::max);
        assert!((peak - 300.0 / 256.0).abs() < 1e-3, "the diamond cap stands at the raised level, peak {peak}");
    }

    #[test]
    fn fused_equal_height_points_stand_no_internal_wall() {
        // Two stone cells raised to the same per-point level fuse: the wall
        // pass closes their outer perimeter against the Void surround but
        // stands no wall on their shared edge, where the heights match.
        let at = ChunkPos { x: 0, z: 0 };
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.underlay[8 * EDGE as usize + 8] = Material::Stone; // cell (8,8)
        chunk.underlay[8 * EDGE as usize + 9] = Material::Stone; // cell (9,8)
        world.insert_chunk(at, chunk);
        world.set_cell_heights(CellPos { x: 8, z: 8 }, &[300; SUBCELLS_PER_CELL]);
        world.set_cell_heights(CellPos { x: 9, z: 8 }, &[300; SUBCELLS_PER_CELL]);

        let mesh = mesh_chunk(&world, at, &StyleTable::default());
        let walls: Vec<&DrawTriangle> = mesh.iter().filter(|t| y_span(t) > 0.5).collect();
        assert!(!walls.is_empty(), "the fused pair wears an outer perimeter wall");
        let shared_edge =
            walls.iter().flat_map(|t| t.verts.iter()).any(|v| (v.x - 9.0).abs() < 1e-4 && (8.05..8.95).contains(&v.z));
        assert!(!shared_edge, "no wall stands on the fused pair's equal-height shared edge");
    }

    #[test]
    fn adjacent_chunks_agree_on_an_authored_break() {
        // A stone shelf (raised 300, rows z < 8) meets grass ground (rows
        // z >= 8) along a break running in x across the vertical chunk seam at
        // x = 16. The marched wall pass reads point levels — pure functions of
        // world position — so each chunk lofts its half and the two meet on
        // identical vertices at the shared seam column.
        let mut world = World::new();
        for cx in 0..2 {
            let mut chunk = Chunk::empty();
            for lz in 0..EDGE {
                for lx in 0..EDGE {
                    let i = (lz * EDGE + lx) as usize;
                    chunk.underlay[i] = if lz < 8 {
                        Material::Stone
                    } else {
                        Material::Grass
                    };
                }
            }
            world.insert_chunk(ChunkPos { x: cx, z: 0 }, chunk);
        }
        for cx in 0..2 {
            for lz in 0..8 {
                for lx in 0..EDGE {
                    world.set_cell_heights(CellPos { x: cx * EDGE + lx, z: lz }, &[300; SUBCELLS_PER_CELL]);
                }
            }
        }
        // Marched wall vertices land on subcell sample lines, not the exact
        // chunk edge, so agreement is that the two chunks emit an identical
        // vertex where their seam-straddling windows share an edge — a
        // non-empty intersection of their wall-vertex sets, near the seam.
        // The z band pins the collection to the authored z = 8 break line
        // (the shelf's outer Void-drop walls are a different feature).
        let wall_verts = |at| {
            let mesh = mesh_chunk(&world, at, &StyleTable::default());
            let mut verts: Vec<_> = mesh
                .iter()
                .filter(|t| y_span(t) > 0.5)
                .flat_map(|t| t.verts.iter())
                .filter(|v| (7.5..8.5).contains(&v.z))
                .map(quantized_vertex_octimeters)
                .collect();
            verts.sort_unstable();
            verts.dedup();
            verts
        };
        let west = wall_verts(ChunkPos { x: 0, z: 0 });
        let east = wall_verts(ChunkPos { x: 1, z: 0 });
        assert!(!west.is_empty() && !east.is_empty(), "each chunk lofts the break");
        let shared: Vec<_> = west.iter().filter(|v| east.contains(v)).collect();
        assert!(!shared.is_empty(), "the two chunks meet on identical seam vertices");
        // The shared vertices sit at the seam column and the raised top level.
        assert!(
            shared.iter().any(|vertex| {
                (vertex.x_octimeters - 16 * OCTIMETERS_PER_METER as i64).abs() <= 32 && vertex.y_octimeters == 300
            }),
            "the shared seam wall stands at the authored break level",
        );
    }

    #[test]
    fn a_pure_delta_plateau_stands_walls_on_its_break_lines() {
        // The same-material height-break class: a stone field at uniform cell
        // height wears a plateau authored purely with point deltas — no
        // material boundary anywhere (nothing for the marched pass) and no
        // cell-height cliff (nothing for the cell-edge walk) — so every wall
        // must come from the subcell relief walk, standing exactly on the
        // plateau's break lines. Those lines run through cell interiors
        // (x and z = 7.5 / 9.5 m), where a cell-edge wall cannot stand.
        let at = ChunkPos { x: 0, z: 0 };
        let mut world = World::new();
        insert_material_chunks(&mut world, Material::Stone, None);
        // Raise the v2.5 global subcell span [30, 38) x [30, 38), scaled to
        // the current density, by 300 octimeters.
        let lo_sub = sub4(30);
        let hi_sub = sub4(38);
        let sub = SUB as usize;
        for lz in 6..10 {
            for lx in 6..10 {
                let mut deltas = [0i16; SUBCELLS_PER_CELL];
                for sz in 0..sub {
                    for sx in 0..sub {
                        let (gx, gz) = global_subcell(lx, lz, sx, sz);
                        if (lo_sub..hi_sub).contains(&gx) && (lo_sub..hi_sub).contains(&gz) {
                            deltas[sz * sub + sx] = 300;
                        }
                    }
                }
                world.set_cell_heights(CellPos { x: lx, z: lz }, &deltas);
            }
        }
        let mesh = mesh_chunk(&world, at, &StyleTable::default());
        let walls: Vec<&DrawTriangle> = mesh.iter().filter(|t| y_span(t) > 0.5).collect();
        assert!(!walls.is_empty(), "the delta plateau wears walls");
        // Every wall vertex stays inside one subcell of the authored
        // perimeter. Two-crossing corner windows replace the old square
        // lattice corner with a bounded chord, while straight runs remain on
        // the authored lines.
        let lo = 7.5f32;
        let hi = 9.5f32;
        for v in walls.iter().flat_map(|t| t.verts.iter()) {
            let on_x = ((v.x - lo).abs() <= 1.0 / SUB as f32 || (v.x - hi).abs() <= 1.0 / SUB as f32)
                && (lo - 1e-4..=hi + 1e-4).contains(&v.z);
            let on_z = ((v.z - lo).abs() <= 1.0 / SUB as f32 || (v.z - hi).abs() <= 1.0 / SUB as f32)
                && (lo - 1e-4..=hi + 1e-4).contains(&v.x);
            assert!(on_x || on_z, "wall vertex ({}, {}) escaped the bounded local contour", v.x, v.z);
        }
        // And all four perimeter sides are closed.
        for (side, pick) in [
            ("west", &(|v: &Vertex| (v.x - lo).abs() < 1e-4) as &dyn Fn(&Vertex) -> bool),
            ("east", &|v: &Vertex| (v.x - hi).abs() < 1e-4),
            ("north", &|v: &Vertex| (v.z - lo).abs() < 1e-4),
            ("south", &|v: &Vertex| (v.z - hi).abs() < 1e-4),
        ] {
            assert!(walls.iter().flat_map(|t| t.verts.iter()).any(pick), "the plateau's {side} side is open");
        }
    }

    #[test]
    fn a_relief_cell_on_a_cell_cliff_lofts_its_wall_exactly_once() {
        // A relief cell standing a plain cell-height cliff above a
        // same-material neighbor: the cell-edge walk skips relief cells and
        // the subcell relief walk owns the face, so the shared edge must be
        // covered exactly once — the wall segments' top edges sum to the one
        // cell-edge meter. Double emission (both walks firing) would sum to
        // two; a routing hole would sum to zero.
        let at = ChunkPos { x: 0, z: 0 };
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        chunk.height[(8 * EDGE + 8) as usize] = 256; // cell (8,8) one meter up
        world.insert_chunk(at, chunk);
        // A small legal-step delta inside the raised cell flips it (and its
        // neighbors) onto the point path without adding any further break.
        let mut deltas = [0i16; SUBCELLS_PER_CELL];
        deltas[SUBCELLS_PER_CELL - 1] = 16;
        world.set_cell_heights(CellPos { x: 8, z: 8 }, &deltas);

        let mesh = mesh_chunk(&world, at, &StyleTable::default());
        // Wall triangles on the west edge plane x = 8 over cell (8,8)'s span.
        let west: Vec<&DrawTriangle> = mesh
            .iter()
            .filter(|t| {
                y_span(t) > 0.5
                    && t.verts.iter().all(|v| (v.x - 8.0).abs() < 1e-4 && (8.0 - 1e-4..=9.0 + 1e-4).contains(&v.z))
            })
            .collect();
        assert!(!west.is_empty(), "the cliff edge carries a wall");
        // Each wall quad's top edge appears on exactly one of its two
        // triangles (the one holding both top vertices); summing those top
        // spans measures the edge coverage.
        let top_length: f32 = west
            .iter()
            .map(|t| {
                let tops: Vec<&Vertex> = t.verts.iter().filter(|v| v.y > 0.5).collect();
                if tops.len() == 2 {
                    (tops[0].z - tops[1].z).abs()
                } else {
                    0.0
                }
            })
            .sum();
        assert!(
            (0.9..=1.0).contains(&top_length),
            "the shared edge is covered once with only bounded corner chords, got {top_length} m",
        );
    }

    /// The authored-silhouette fixture from the causeway demo failure: a
    /// stone square raised purely by point deltas over flat grass ground —
    /// materials and heights break on the same subcell lines (global
    /// subcells `[30, 38)²`, i.e. `[7.5, 9.5)` m), every cell height zero.
    /// Smoothing is pinned off through the per-cell override plane so the
    /// marched contour is exactly the authored break line.
    fn delta_column_world() -> World {
        let mut world = World::new();
        world.insert_smoothing_profile(1, SmoothingProfile { iterations: 0, degrees: 90 });
        insert_material_chunks(&mut world, Material::Grass, Some(1));
        let lo_sub = sub4(30);
        let hi_sub = sub4(38);
        for lz in 6..10 {
            for lx in 6..10 {
                let mut deltas = [0i16; SUBCELLS_PER_CELL];
                let points =
                    subcell_square_where(Material::Stone.to_u8(), global_square_predicate(lx, lz, lo_sub, hi_sub));
                fill_subcell_square_where(&mut deltas, 300, global_square_predicate(lx, lz, lo_sub, hi_sub));
                let cell = CellPos { x: lx, z: lz };
                world.set_cell_points(cell, &points);
                world.set_cell_heights(cell, &deltas);
            }
        }
        world
    }

    /// A cell-aligned stone block raised a whole-cell step above flat grass —
    /// a MATERIAL boundary with **no subcell relief**, the relief-free cliff
    /// that [`delta_column_world`]'s subcell deltas never exercise. Whole-cell
    /// `height` only (`height_points` left inheriting), so
    /// `cell_has_height_relief` stays false across the boundary. Smoothing is
    /// pinned off (profile 1, iterations 0) so the marched contour stays
    /// axis-aligned and the block's perimeter walls run straight.
    fn material_block_world() -> World {
        let mut world = World::new();
        world.insert_smoothing_profile(1, SmoothingProfile { iterations: 0, degrees: 90 });
        for dz in -1..=1 {
            for dx in -1..=1 {
                let mut c = Chunk::empty();
                c.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
                c.smoothing = [1; CELLS_PER_CHUNK_AREA];
                if dx == 0 && dz == 0 {
                    // A 4x4 stone block on whole-cell heights — the cliff is a
                    // material + cell-height break with zero subcell relief.
                    for lz in 6..10 {
                        for lx in 6..10 {
                            let idx = (lz * EDGE + lx) as usize;
                            c.underlay[idx] = Material::Stone;
                            c.height[idx] = 512;
                        }
                    }
                }
                world.insert_chunk(ChunkPos { x: dx, z: dz }, c);
            }
        }
        world
    }

    #[test]
    fn a_delta_silhouette_wall_coverage_is_complete() {
        // The picket-fence regression from the causeway demo: gating marched
        // segments on the window center's point level skips every window
        // whose center lands on the low-material side — along a contour the
        // center alternates sides subcell to subcell, halving the wall into
        // isolated wedges. The side-aware per-segment gate must close the
        // full perimeter: the emitted wall top-edge length covers the
        // square's 8 m perimeter — the marched chamfers trade the corner
        // right angles for diagonals and the clip-gap walls close the
        // chamfer slivers' lattice edges, so the total sits just above 8 —
        // far above the alternating half.
        let world = delta_column_world();
        let mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, &StyleTable::default());
        let total = total_wall_top_edge_length(&mesh, 1.0);
        assert!(
            (7.5..8.6).contains(&total),
            "wall top-edge coverage {total} m over an 8 m perimeter — alternating gaps halve it",
        );
    }

    #[test]
    fn a_material_block_wall_coverage_is_complete() {
        // Tripwire: a relief-free material-boundary cliff must close its full
        // perimeter. The bug lifted the marched wall top through the
        // window-center owner rather than the anchored (high) side; on an
        // axis-aligned boundary the owner sits on the low (grass) side for
        // every boundary window, so every wall top collapses onto the grass
        // plate and the whole face vanishes (zero-height, see-through). A 4x4
        // cell stone block on flat grass has a 16 m perimeter; the emitted
        // wall top-edge length must cover it, far above the collapsed
        // near-zero the bug leaves.
        let world = material_block_world();
        let mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, &StyleTable::default());
        let block_top = 512.0 / 256.0;
        let total = total_wall_top_edge_length(&mesh, block_top - 0.5);
        assert!(
            (15.0..17.5).contains(&total),
            "wall top-edge coverage {total} m over a 16 m perimeter — \
             a relief-free material-boundary cliff collapsed its faces",
        );
    }

    #[test]
    fn caps_split_at_a_delta_break_without_draping() {
        // The zigzag regression from the causeway demo: a contour vertex on
        // the break line lifted through whichever subcell it floors into
        // drapes the grass cap up the column flank and sags the stone cap
        // down it with subcell parity. Split caps read their own side: every
        // cap triangle (nonzero footprint — walls project to a line) lies
        // wholly on its side's plate, the grass ground at zero, the stone
        // column at its raised level, nothing in between.
        let world = delta_column_world();
        let mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, &StyleTable::default());
        let high = 300.0 / 256.0;
        let (mut on_ground, mut on_column) = (false, false);
        for t in &mesh {
            if xz_area_doubled(t) < 1e-6 {
                continue; // vertical wall faces own the gap
            }
            // Every cap triangle lies wholly on one plate: a draped triangle
            // would mix the two levels (or hit an interpolated height between
            // them) within one footprint.
            let ground = t.verts.iter().all(|v| v.y.abs() < 1e-3);
            let column = t.verts.iter().all(|v| (v.y - high).abs() < 1e-3);
            assert!(
                ground || column,
                "cap triangle at ({}, {}) spans heights {:?} — a drape across the break",
                t.verts[0].x,
                t.verts[0].z,
                t.verts.map(|v| v.y),
            );
            on_ground |= ground;
            on_column |= column;
        }
        assert!(on_ground && on_column, "both caps are present (ground {on_ground}, column {on_column})");
    }

    #[test]
    fn delta_break_walls_seal_both_caps() {
        // Watertightness across the delta break: every wall top vertex
        // coincides with a high-cap vertex, and every wall bottom vertex
        // coincides with a low-cap vertex at the same position — under
        // closure the base is the low cap's committed edge exactly, so the
        // wall spans precisely the vertical gap the split caps open, no
        // pinholes, no overlap.
        let world = delta_column_world();
        let mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, &StyleTable::default());
        assert_height_break_walls_close(&mesh, "delta break");
    }

    /// Shared assertion for authored-plateau fixtures — the live causeway
    /// probe's invariant, in-repo: every cap triangle (nonzero footprint)
    /// lies level on one plateau (a triangle bridging two plateaus is the
    /// slanted "plane not sitting flat" the demo showed — a flap fan
    /// chording across a break), the authored breaks genuinely split the
    /// caps, and every split is spanned by a wall at exactly its two
    /// plates.
    fn assert_plateaus_flat_and_sealed(world: &World, at: ChunkPos) {
        let mesh = mesh_chunk(world, at, &StyleTable::default());
        for t in &mesh {
            if xz_area_doubled(t) < 1e-6 {
                continue; // vertical wall faces
            }
            let ys = t.verts.map(|v| v.y);
            assert!(
                (ys[0] - ys[1]).abs() < 1e-3 && (ys[0] - ys[2]).abs() < 1e-3,
                "cap triangle at ({}, {}) bridges plateaus: {ys:?}",
                t.verts[0].x,
                t.verts[0].z,
            );
        }
        assert_height_break_walls_close(&mesh, "authored plateau fixture");
    }

    #[test]
    fn a_three_plateau_junction_stays_flat_and_sealed() {
        // Three same-material plateaus (deltas 0 / 128 / 256) meeting at a
        // point inside a stone square on grass, smoothing on — the junction
        // regime the causeway probe caught: mixed windows near the zone
        // lines hold same-label corner samples on different plateaus, and a
        // whole-window flap fan chords across the break (pairs like
        // 0.5 ↔ 1.0 in one footprint triangle). The clipped flaps must keep
        // every cap triangle on one plateau and wall every split.
        let mut world = World::new();
        world.insert_smoothing_profile(1, SmoothingProfile { iterations: 2, degrees: 45 });
        insert_material_chunks(&mut world, Material::Grass, Some(1));
        let sub = SUB as usize;
        let body_lo = sub4(28);
        let body_hi = sub4(44);
        let split = sub4(36);
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                let mut points = [UNDERLAY_POINT_INHERIT; SUBCELLS_PER_CELL];
                let mut deltas = [0i16; SUBCELLS_PER_CELL];
                let mut any = false;
                for sz in 0..sub {
                    for sx in 0..sub {
                        let (gx, gz) = global_subcell(lx, lz, sx, sz);
                        if (body_lo..body_hi).contains(&gx) && (body_lo..body_hi).contains(&gz) {
                            points[sz * sub + sx] = Material::Stone.to_u8();
                            // Zone lines x = 9 m and z = 9 m meet at the
                            // junction and run out through the perimeter.
                            deltas[sz * sub + sx] = if gx < split {
                                0
                            } else if gz < split {
                                128
                            } else {
                                256
                            };
                            any = true;
                        }
                    }
                }
                if any {
                    let cell = CellPos { x: lx, z: lz };
                    world.set_cell_points(cell, &points);
                    world.set_cell_heights(cell, &deltas);
                }
            }
        }
        assert_plateaus_flat_and_sealed(&world, ChunkPos { x: 0, z: 0 });
    }

    #[test]
    fn a_two_height_silhouette_against_grass_stays_flat_and_sealed() {
        // An L-shaped stone region on grass whose two arms stand at
        // different heights (deltas 128 / 256), smoothing on — the internal
        // same-material break line runs out through the stone/grass
        // perimeter, the second junction regime from the causeway probe.
        // Clipped flaps keep every cap triangle level; the walls close both
        // the perimeter and the internal break.
        let mut world = World::new();
        world.insert_smoothing_profile(1, SmoothingProfile { iterations: 2, degrees: 45 });
        insert_material_chunks(&mut world, Material::Grass, Some(1));
        let sub = SUB as usize;
        let arm_x = sub4(30)..sub4(36);
        let arm_z = sub4(26)..sub4(42);
        let foot_x = sub4(36)..sub4(42);
        let foot_z = sub4(34)..sub4(42);
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                let mut points = [UNDERLAY_POINT_INHERIT; SUBCELLS_PER_CELL];
                let mut deltas = [0i16; SUBCELLS_PER_CELL];
                let mut any = false;
                for sz in 0..sub {
                    for sx in 0..sub {
                        let (gx, gz) = global_subcell(lx, lz, sx, sz);
                        // Vertical arm at +128; the foot at +256.
                        let arm = arm_x.contains(&gx) && arm_z.contains(&gz);
                        let foot = foot_x.contains(&gx) && foot_z.contains(&gz);
                        if arm || foot {
                            points[sz * sub + sx] = Material::Stone.to_u8();
                            deltas[sz * sub + sx] = if arm {
                                128
                            } else {
                                256
                            };
                            any = true;
                        }
                    }
                }
                if any {
                    let cell = CellPos { x: lx, z: lz };
                    world.set_cell_points(cell, &points);
                    world.set_cell_heights(cell, &deltas);
                }
            }
        }
        assert_plateaus_flat_and_sealed(&world, ChunkPos { x: 0, z: 0 });
    }
}

mod voids {
    use super::*;

    #[test]
    fn zeroed_deltas_mesh_identically() {
        // Tripwire: a world whose height deltas are all zero meshes byte-for-
        // byte as one that never carried the plane. Author then clear a cell's
        // deltas over a varied world; the net-zero relief must collapse back
        // to the cell-stride caps and walls, adding and moving nothing.
        let at = ChunkPos { x: 0, z: 0 };
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                let i = (lz * EDGE + lx) as usize;
                chunk.underlay[i] = if lx < 8 {
                    Material::Grass
                } else {
                    Material::Sand
                };
                chunk.height[i] = 8 * lx; // gentle ramp, no cliffs
            }
        }
        world.insert_chunk(at, chunk);
        let baseline = mesh_chunk(&world, at, &StyleTable::default());

        let mut authored = world.clone();
        authored.set_cell_heights(CellPos { x: 4, z: 4 }, &[50; SUBCELLS_PER_CELL]);
        authored.set_cell_heights(CellPos { x: 4, z: 4 }, &[]); // clears to zero
        let after = mesh_chunk(&authored, at, &StyleTable::default());

        assert_eq!(baseline, after, "a net-zero delta plane meshes identically to no plane");
    }

    #[test]
    fn a_fillable_void_joint_floors_and_walls() {
        // Tripwire: an enclosed Void well one meter deep in a grass field
        // closes as a real flat-bottomed cut — a floor cap at the void
        // points' stored height and rim walls from the grass down to it, no
        // open bottom. A prediction pass blind to the void depth would leave
        // the groove open (a wall to a fixed drop, or no wall at all); the
        // floor and its walls both read the one total height plane.
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        // Cell (7, 7) is a full Void well, floored one meter down and enclosed
        // by grass on every side.
        let cell = CellPos { x: 7, z: 7 };
        world.set_cell_points(cell, &[Material::Void.to_u8(); SUBCELLS_PER_CELL]);
        world.set_cell_heights(cell, &[-256; SUBCELLS_PER_CELL]);

        let mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, &StyleTable::default());
        let floor = -256.0 / OCTIMETERS_PER_METER;
        let has_floor =
            mesh.iter().any(|t| xz_area_doubled(t) > 1e-6 && t.verts.iter().all(|v| (v.y - floor).abs() < 1e-3));
        assert!(has_floor, "the enclosed void floors over at its stored depth");
        // A rim wall spans the grass edge (y ~ 0) down to the floor (y ~ -1 m)
        // — the groove is closed, not open-bottomed.
        let has_rim_wall = mesh.iter().any(|t| {
            xz_area_doubled(t) < 1e-6
                && t.verts.iter().any(|v| v.y > -0.1)
                && t.verts.iter().any(|v| (v.y - floor).abs() < 1e-3)
        });
        assert!(has_rim_wall, "the void rim walls down to the floor, no open bottom");
    }

    #[test]
    fn a_rounded_void_rim_clones_its_sloped_floor_vertices() {
        // An enclosed full-cell groove slopes along z. A constant bottom
        // ring would flatten every wall base; rebuilding the rounded chord
        // independently would miss its cap pins. The planned Void fragments
        // and ribbons instead select the same named floor anchors.
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        let cell = CellPos { x: 7, z: 7 };
        world.set_cell_points(cell, &[Material::Void.to_u8(); SUBCELLS_PER_CELL]);
        let sub = SUB as usize;
        let mut deltas = [0i16; SUBCELLS_PER_CELL];
        for point_z in 0..sub {
            for point_x in 0..sub {
                deltas[point_z * sub + point_x] = -256 + point_z as i16 * 4;
            }
        }
        world.set_cell_heights(cell, &deltas);

        let mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, &StyleTable::default());
        let floor_vertices: Vec<_> = mesh
            .iter()
            .filter(|triangle| xz_area_doubled(triangle) > 1e-6)
            .flat_map(|triangle| triangle.verts.iter())
            .filter(|vertex| vertex.y < -0.2)
            .collect();
        let wall_bases: Vec<_> = mesh
            .iter()
            .filter(|triangle| xz_area_doubled(triangle) < 1e-6 && triangle.verts.iter().any(|vertex| vertex.y > -0.1))
            .flat_map(|triangle| triangle.verts.iter())
            .filter(|vertex| vertex.y < -0.2)
            .collect();
        assert!(!wall_bases.is_empty(), "the sloped groove has a rounded rim");
        for base in &wall_bases {
            assert!(
                floor_vertices.iter().any(|floor| { floor.x == base.x && floor.y == base.y && floor.z == base.z }),
                "wall base ({}, {}, {}) is not a byte-identical floor-cap vertex",
                base.x,
                base.y,
                base.z,
            );
        }
        let mut levels: Vec<_> =
            wall_bases.iter().map(|vertex| (vertex.y * OCTIMETERS_PER_METER).round() as i32).collect();
        levels.sort_unstable();
        levels.dedup();
        assert!(levels.len() > 2, "the wall bottom follows the sloped low cap instead of a constant ring: {levels:?}");
    }
}
