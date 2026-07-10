// Plane bytes narrow u32 ids down to u16 at the wire boundary; the truncation
// is the documented format contract, not a bug.
#![allow(clippy::cast_possible_truncation)]
// The kinds pull the world module's shared data vocabulary (the `Chunk` /
// `Material` / `Region` data layer plus its geometry consts, many referenced
// only from doc links) from the parent module.
#![allow(clippy::wildcard_imports)]

//! Wire kinds for the world plane stack — the `aether.kit.world.*` mail a
//! peer sends [`WorldView`] to write chunks, paint cells or compact vector
//! stamps, register regions, or load a serialized world.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::data::cliff_material_from_u8;
use super::*;

/// `aether.kit.world.set_chunk` — write one chunk's planes. The ground
/// planes ride as raw `Bytes` (256 material/shape bytes each);
/// `overlay_mask` and `underlay_points` ride as up to one byte per subcell,
/// and `height_points` as up to [`HEIGHT_POINTS_PER_CHUNK`] `i16` deltas;
/// `height` / `region` / `water_plane` ride as length-256 vectors. Shorter
/// vectors pad with `Void` / `0` (and a short `underlay_points` /
/// `height_points` leaves the tail inheriting); longer ones truncate.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.set_chunk")]
pub struct SetChunk {
    pub chunk_x: i32,
    pub chunk_z: i32,
    /// Underlay plane — 256 raw material bytes (`Material as u8`).
    pub underlay: Vec<u8>,
    /// Underlay material-point plane — up to [`UNDERLAY_POINTS_PER_CHUNK`]
    /// bytes (`256 * SUB²`; 65536 at `SUB = 16`), one point per subcell in
    /// row-major cell order (`z*SUB + x` within a cell). Each byte is a
    /// [`Material`] or the [`UNDERLAY_POINT_INHERIT`] sentinel; a short
    /// vector leaves the remaining points inheriting, so an empty vector is
    /// the all-inherit default.
    pub underlay_points: Vec<u8>,
    /// Height-delta plane — up to [`HEIGHT_POINTS_PER_CHUNK`] `i16` octimeter
    /// deltas, one per subcell in the same layout as `underlay_points`. Each
    /// offsets its subcell off the cell's `height` ([`World::point_height`]);
    /// a short vector leaves the remaining points at [`HEIGHT_POINT_INHERIT`]
    /// (`0`, no relief), so an empty vector is the flat default.
    pub height_points: Vec<i16>,
    /// Overlay plane — 256 raw material bytes. `0` = no overlay.
    pub overlay: Vec<u8>,
    /// Overlay subcell coverage plane — [`OVERLAY_MASK_WIRE_BYTES`] bytes
    /// (`256 * SUB²`; 65536 at `SUB = 16`), one coverage byte per subcell in
    /// row-major cell order (`z*SUB + x` within a cell). A short vector
    /// leaves the remaining samples uncovered, so an empty vector is the
    /// no-coverage default.
    pub overlay_mask: Vec<u8>,
    /// Elevation plane — 256 octimeter values.
    pub height: Vec<i32>,
    /// Region-id plane — 256 values. `0` = no region.
    pub region: Vec<u32>,
    /// Water-plane-id plane — 256 values, narrowing to `u16` like `region`.
    /// `0` = the datum-0 level; meaningful only under a water cell.
    pub water_plane: Vec<u32>,
    /// Smoothing-profile-id plane — 256 raw bytes. `0` = no override.
    pub smoothing: Vec<u8>,
}

impl SetChunk {
    /// The chunk address this write targets.
    #[must_use]
    pub fn chunk_pos(&self) -> ChunkPos {
        ChunkPos {
            x: self.chunk_x,
            z: self.chunk_z,
        }
    }

    /// Decode the wire planes into a [`Chunk`]. Material bytes degrade to
    /// `Void` on an unknown value; every plane pads to 256 / truncates
    /// past it; `region` casts `u32` down to `u16`.
    #[must_use]
    pub fn into_chunk(self) -> Box<Chunk> {
        let mut chunk = Chunk::empty_boxed();
        for (dst, byte) in chunk.underlay.iter_mut().zip(&self.underlay) {
            *dst = Material::from_u8_or_void(*byte);
        }
        // Underlay-point plane: a short vector leaves the tail inheriting
        // (Chunk::empty seeds every point to the inherit sentinel).
        for (dst, byte) in chunk.underlay_points.iter_mut().zip(&self.underlay_points) {
            *dst = *byte;
        }
        // Height-delta plane: a short vector leaves the tail at zero relief
        // (Chunk::empty seeds every delta to the inherit sentinel).
        for (dst, delta) in chunk.height_points.iter_mut().zip(&self.height_points) {
            *dst = *delta;
        }
        for (dst, byte) in chunk.overlay.iter_mut().zip(&self.overlay) {
            *dst = Material::from_u8_or_void(*byte);
        }
        // Coverage plane: a short vector leaves the tail uncovered
        // (Chunk::empty seeds every sample to 0); trailing bytes truncate.
        for (dst, byte) in chunk.overlay_mask.iter_mut().zip(&self.overlay_mask) {
            *dst = *byte;
        }
        for (dst, value) in chunk.height.iter_mut().zip(&self.height) {
            *dst = *value;
        }
        for (dst, value) in chunk.region.iter_mut().zip(&self.region) {
            *dst = *value as u16;
        }
        for (dst, value) in chunk.water_plane.iter_mut().zip(&self.water_plane) {
            *dst = *value as u16;
        }
        for (dst, byte) in chunk.smoothing.iter_mut().zip(&self.smoothing) {
            *dst = *byte;
        }
        chunk
    }
}

/// `aether.kit.world.set_cell_points` — stamp one cell's `SUB × SUB`
/// underlay material points, the single-cell live-paint counterpart to
/// `set_chunk`'s whole-plane write. `points` carries up to
/// [`SUBCELLS_PER_CELL`] bytes in `z*SUB + x` subcell order; a short vector
/// leaves the cell's remaining points inheriting (an empty vector clears
/// the cell back to all-inherit). Each byte is a [`Material`] or the
/// [`UNDERLAY_POINT_INHERIT`] sentinel — including `0` = authored `Void`,
/// which cuts a hole.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.set_cell_points")]
pub struct SetCellPoints {
    pub x: i32,
    pub z: i32,
    pub points: Vec<u8>,
}

impl SetCellPoints {
    /// The cell this stamp targets.
    #[must_use]
    pub fn cell(&self) -> CellPos {
        CellPos {
            x: self.x,
            z: self.z,
        }
    }
}

/// A point on the world's XZ plane, expressed in octimeters.
///
/// Keeping the axes and unit named in the wire vocabulary prevents callers
/// from having to infer whether a positional pair is `[x, z]`, `[z, x]`, or
/// measured in cells rather than octimeters.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorldPoint {
    pub x_octimeters: i32,
    pub z_octimeters: i32,
}

impl WorldPoint {
    #[must_use]
    pub const fn new(x_octimeters: i32, z_octimeters: i32) -> Self {
        Self {
            x_octimeters,
            z_octimeters,
        }
    }
}

/// Maximum number of vertices accepted by one polygon stamp.
pub const MAX_STAMP_VERTICES: usize = 1024;
/// Maximum raster extent of one stamp along either axis, in subcells.
pub const MAX_STAMP_EDGE_SUBCELLS: usize = 4096;
/// Maximum total raster area allocated for one stamp, in subcells.
pub const MAX_STAMP_SUBCELLS: usize = 1_048_576;
/// Maximum estimated scanline work accepted by one stamp. The estimate
/// includes edge tests, intersection sorting, and interval-to-subcell visits.
pub const MAX_STAMP_RASTER_WORK: usize = 33_554_432;

/// `aether.kit.world.stamp_polygon` — rasterize a polygon directly into the
/// scalar overlay coverage plane. `points` is a [`WorldPoint`] vertex ring;
/// concave rings use even-odd fill. `material` is a raw [`Material`] byte.
/// Fewer than three points, more than [`MAX_STAMP_VERTICES`] points, a
/// degenerate or oversized ring, or `Material::Void` paints nothing. Across
/// all stamp kinds, equal materials max-compose coverage; a different
/// material replaces the mask of each cell it reaches in painter order.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.stamp_polygon")]
pub struct StampPolygon {
    pub points: Vec<WorldPoint>,
    pub material: u8,
}

/// `aether.kit.world.stamp_disc` — rasterize a compact disc description into
/// the scalar overlay coverage plane. `center` is a [`WorldPoint`],
/// `radius_octimeters` is center-to-edge distance, and `material` is a raw
/// [`Material`] byte. A zero radius, oversized raster, or `Material::Void`
/// paints nothing.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.stamp_disc")]
pub struct StampDisc {
    pub center: WorldPoint,
    pub radius_octimeters: u32,
    pub material: u8,
}

/// `aether.kit.world.stamp_hexagon` — rasterize a flat-top regular hexagon
/// into the scalar overlay coverage plane. `center` is a [`WorldPoint`],
/// `radius_octimeters` is center-to-vertex distance, and `material` is a raw
/// [`Material`] byte. A zero radius, oversized raster, or `Material::Void`
/// paints nothing.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.stamp_hexagon")]
pub struct StampHexagon {
    pub center: WorldPoint,
    pub radius_octimeters: u32,
    pub material: u8,
}

/// `aether.kit.world.set_cell_heights` — stamp one cell's `SUB × SUB` height
/// deltas, the single-cell live-paint counterpart to `set_chunk`'s
/// whole-plane `height_points` write and the height sibling of
/// `set_cell_points`. `deltas` carries up to [`SUBCELLS_PER_CELL`] `i16`
/// octimeter offsets in `z*SUB + x` subcell order; a short vector leaves the
/// cell's remaining points at [`HEIGHT_POINT_INHERIT`] (an empty vector
/// clears the cell back to no relief). Each delta lifts (or drops) its
/// subcell off the cell's `height` — real, standable relief the height
/// pipeline resolves one stride down.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.set_cell_heights")]
pub struct SetCellHeights {
    pub x: i32,
    pub z: i32,
    pub deltas: Vec<i16>,
}

impl SetCellHeights {
    /// The cell this stamp targets.
    #[must_use]
    pub fn cell(&self) -> CellPos {
        CellPos {
            x: self.x,
            z: self.z,
        }
    }
}

/// `aether.kit.world.set_region` — register a region in the table under a
/// 1-based `region_id`, giving the underlay cascade a default material to
/// fall through to. `default_material` and `cliff_material` are raw
/// `Material` bytes; a `cliff_material` byte that decodes to `Void` or
/// nothing falls back to Stone (a cliff face always wears something).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.set_region")]
pub struct SetRegion {
    pub region_id: u32,
    pub name: String,
    pub default_material: u8,
    pub cliff_material: u8,
}

impl SetRegion {
    /// The [`Region`] this registers, decoding `default_material`
    /// (unknown → `Void`) and `cliff_material` (unknown or `Void` →
    /// Stone).
    #[must_use]
    pub fn into_region(self) -> Region {
        Region {
            name: self.name,
            default_material: Material::from_u8_or_void(self.default_material),
            cliff_material: cliff_material_from_u8(self.cliff_material),
        }
    }
}

/// `aether.kit.world.load` — load a serialized world from the substrate's
/// I/O surface (ADR-0041 namespace + path). The bytes are the
/// [`World::to_bytes`] format; a decode failure keeps the prior world.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.load")]
pub struct WorldLoad {
    pub namespace: String,
    pub path: String,
}
