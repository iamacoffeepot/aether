// Plane bytes narrow u32 ids down to u16 at the wire boundary; the truncation
// is the documented format contract, not a bug.
#![allow(clippy::cast_possible_truncation)]
// The kinds pull the world module's shared data vocabulary (the `Chunk` /
// `Material` / `Region` data layer plus its geometry consts, many referenced
// only from doc links) from the parent module.
#![allow(clippy::wildcard_imports)]

//! Wire kinds for the world plane stack — the `aether.kit.world.*` mail a
//! peer sends [`WorldView`] to write chunks, paint cells,
//! register regions / smoothing profiles / water planes, restyle a material,
//! switch the view mode, or load a serialized world.

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::data::cliff_material_from_u8;
use super::*;

/// `aether.kit.world.set_chunk` — write one chunk's planes. The ground
/// planes ride as raw `Bytes` (256 material/shape bytes each);
/// `underlay_points` rides as up to [`UNDERLAY_POINTS_PER_CHUNK`] bytes and
/// `height_points` as up to [`HEIGHT_POINTS_PER_CHUNK`] `i16` deltas;
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
    /// bytes (`256 * SUB²`; 4096 at `SUB = 4`), one point per subcell in
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
    /// Overlay subcell-mask plane — [`OVERLAY_MASK_WIRE_BYTES`] bytes
    /// (`256 * SUB² / 8`; 512 at `SUB = 4`), one little-endian mask word
    /// per cell in row-major cell order.
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
    pub fn into_chunk(self) -> Chunk {
        let mut chunk = Chunk::empty();
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
        // Mask plane: two little-endian bytes per cell (u16 at SUB=4). A
        // short plane pads with 0 (no coverage); trailing bytes truncate.
        for (dst, pair) in chunk
            .overlay_mask
            .iter_mut()
            .zip(self.overlay_mask.chunks_exact(2))
        {
            *dst = u16::from_le_bytes([pair[0], pair[1]]);
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

/// `aether.kit.world.set_smoothing_profile` — register a contour-smoothing
/// profile in the table under a 1-based `profile_id`, giving the per-cell
/// smoothing plane a `(iterations, degrees)` pair to point at.
/// `iterations` clamps to [`MAX_SMOOTHING_ITERATIONS`] and `degrees` to
/// `[45, 90]` at registration.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.set_smoothing_profile")]
pub struct SetSmoothingProfile {
    pub profile_id: u32,
    pub iterations: u32,
    pub degrees: u32,
}

impl SetSmoothingProfile {
    /// The [`SmoothingProfile`] this registers (clamping happens at
    /// [`World::insert_smoothing_profile`]).
    #[must_use]
    pub fn profile(&self) -> SmoothingProfile {
        SmoothingProfile {
            iterations: self.iterations,
            degrees: self.degrees,
        }
    }
}

/// `aether.kit.world.set_water_plane` — register a water plane in the table
/// under a 1-based `plane_id`, giving water cells a flat authored surface
/// level to resolve at. The per-cell water plane (`aether.kit.world.set_chunk`'s
/// `water_plane`) points at the row; retuning a lake's level is this one
/// table write, live like `set_region`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.set_water_plane")]
pub struct SetWaterPlane {
    pub plane_id: u32,
    pub level_octimeters: i32,
}

impl SetWaterPlane {
    /// The [`WaterPlane`] this registers.
    #[must_use]
    pub fn plane(&self) -> WaterPlane {
        WaterPlane {
            level_octimeters: self.level_octimeters,
        }
    }
}

/// `aether.kit.world.set_material_style` — write a material's complete
/// live style row (base HSL, noise shape, smoothing defaults, rim / wash /
/// water tunables, encroachment margin), then remesh every cached chunk. A
/// full-row write: every field of the resolved row is replaced, none
/// carried over from the prior value. `smoothing_iterations` clamps to
/// [`MAX_SMOOTHING_ITERATIONS`] and `smoothing_degrees` to `[45, 90]`, the
/// same rule [`World::insert_smoothing_profile`] applies to the per-cell
/// smoothing table; `encroach_reach_octimeters` clamps to `[0, 256]` and
/// `encroach_raggedness` to `[0, 1]` so a margin never outruns the mesher's
/// apron. `material` is a raw [`Material`] byte; an undecodable byte or
/// `Void` rejects the whole write (see
/// `world::mesher::style::StyleTable::apply`).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.set_material_style")]
pub struct SetMaterialStyle {
    pub material: u8,
    /// Base hue in degrees `[0, 360)`.
    pub base_hue: f32,
    /// Base saturation in percent `[0, 100]`.
    pub base_sat: f32,
    /// Base lightness in percent `[0, 100]`.
    pub base_light: f32,
    /// Peak hue deviation in degrees the noise field adds.
    pub amp_hue: f32,
    /// Peak saturation deviation in percent.
    pub amp_sat: f32,
    /// Peak lightness deviation in percent.
    pub amp_light: f32,
    /// Noise wavelength in cells — the world distance over one lattice
    /// period at the base octave.
    pub wavelength: f32,
    /// Fractal octave count.
    pub octaves: u32,
    /// Per-octave amplitude falloff (lacunarity is fixed at 2).
    pub persistence: f32,
    /// Seed offset folded into every channel so each material keys its own
    /// decorrelated field.
    pub seed_offset: u32,
    /// Wavelength in cells of the stroke flow field the wash grades along.
    pub flow_wavelength: f32,
    /// Corner-smoothing angle in degrees for this material's overlay
    /// contours (`45` chamfers hardest, `90` only true right-angle
    /// corners). Clamped to `[45, 90]` on apply.
    pub smoothing_degrees: u32,
    /// Corner-smoothing iteration count (`0` = raw blocky contours).
    /// Clamped to [`MAX_SMOOTHING_ITERATIONS`] on apply.
    pub smoothing_iterations: u32,
    /// Rim inset in octimeters — the width of a pooled edge strip.
    pub rim_inset_octimeters: i32,
    /// Rim lightness darkening `[0, 1]` where the paint pools.
    pub rim_darken: f32,
    /// Wash lightness gradient depth `[0, 1]` along the stroke direction.
    pub wash_grade: f32,
    /// Water lightness reduction in percent at full depth.
    pub water_depth_darken: f32,
    /// Blob-merge hue-step threshold in degrees: same-material cells whose
    /// resolved hue differs by more than this pool a rim between them.
    pub blob_merge_degrees: f32,
    /// Encroachment dominance in a total order: at a seam the higher-rank
    /// material grows a noise-ragged margin over the lower one. `0` never
    /// encroaches, and equal ranks never encroach on each other.
    pub encroach_rank: u8,
    /// How far in octimeters the margin reaches past the seam into the
    /// lower material before the raggedness noise carves it back. `0`
    /// disables the layer for this material. Clamped to `[0, 256]` on apply.
    pub encroach_reach_octimeters: i32,
    /// The fraction `[0, 1]` of the reach the world-anchored noise eats, so
    /// the margin reads ragged rather than a clean offset band. Clamped to
    /// `[0, 1]` on apply.
    pub encroach_raggedness: f32,
}

/// How the mesher paints a chunk: the finished gouache grammar, or the
/// raw grayscale noise field for calibrating the material table by eye.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ViewMode {
    /// Flat keyed cells, pooled rims, wash, corner-minimized contours,
    /// depth-graded water — the finished look.
    #[default]
    Painted,
    /// Each cell's own hue-noise field as grayscale, so the table's
    /// wavelength / octaves / amplitude read directly off the surface.
    Raw,
}

impl ViewMode {
    /// Decode the wire byte: `1` is the raw field, anything else painted.
    #[must_use]
    pub fn from_u8(byte: u8) -> Self {
        if byte == 1 { Self::Raw } else { Self::Painted }
    }
}

/// `aether.kit.world.set_view_mode` — switch the whole view between the
/// painted gouache grammar and the raw grayscale field. `mode` is a raw
/// [`ViewMode`] byte (`0` painted, `1` raw); an unknown byte reads as
/// painted.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.set_view_mode")]
pub struct SetViewMode {
    pub mode: u8,
}

impl SetViewMode {
    /// The [`ViewMode`] this selects.
    #[must_use]
    pub fn view_mode(&self) -> ViewMode {
        ViewMode::from_u8(self.mode)
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
