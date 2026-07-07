// Serialization casts here are bounded by the fixed plane geometry: a
// chunk has 256 cells, region ids and plane lengths are small by
// construction, and `region: Vec<u32>` narrows to the `[u16; 256]`
// region plane by design (ids past u16 are not addressable). The
// truncation these lints warn about cannot occur in this domain.
#![allow(clippy::cast_possible_truncation)]
// `chunk_index` casts a `rem_euclid` result (always `0..16`) to `usize`;
// the sign-loss the lint warns about cannot occur.
#![allow(clippy::cast_sign_loss)]

//! The chunked world plane stack.
//!
//! The world is a **cell lattice**: cells are addresses, not objects.
//! What a cell *is* — its ground fabric, elevation, region membership —
//! lives in a stack of property planes over that lattice, chunked
//! `16 × 16` cells (a [`Chunk`]). A [`World`] holds a sparse set of
//! chunks keyed by [`ChunkPos`] plus a region table.
//!
//! # Units
//!
//! `1 cell = 1 m = 256 octimeters` (the fixed-point movement quantum).
//! The chunk a cell sits in is the cell right-shifted by [`CHUNK_BITS`]
//! — an arithmetic shift, so a negative cell floors toward `-∞` (cell
//! `-1` is in chunk
//! `-1`, not `0`), which is the intended lattice tiling. A cell's index
//! within its chunk is `z.rem_euclid(16) * 16 + x.rem_euclid(16)`
//! (row-major), so negative cells index their chunk correctly.
//!
//! # Ground fabric — underlay / overlay
//!
//! Two material planes:
//!
//! - [`Chunk::underlay`] — the ground fabric. This is what the region
//!   cascade resolves ([`World::underlay`]): the cell's own underlay if
//!   non-[`Material::Void`], else the cell's region's `default_material`,
//!   else `Void`.
//! - [`Chunk::overlay`] — an optional crisp placed surface (path, water,
//!   floor). `Void` means no overlay. Never cascade-resolved
//!   ([`World::overlay`] is a raw plane read).
//! - [`Chunk::overlay_mask`] — a subcell coverage bitmask per cell. Each
//!   cell holds a [`SUBCELLS_PER_CELL_EDGE`]² bit grid (bit `z*SUB + x`,
//!   `1` = the overlay covers that subcell); `0xFFFF` at `SUB = 4` is
//!   full coverage. The subcell is the finest semantic resolution —
//!   paint must not out-resolve movement / blocking, which resolve at the
//!   subcell. Masks OR-compose (bitwise) within the overlay layer;
//!   painter's order applies across layers. Reserved here — the wire
//!   layout is final, but mask meshing is a follow-up; nothing in this
//!   module interprets the bits.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

/// Cells along one edge of a chunk. Chunks are `16 × 16` cells.
pub const CELLS_PER_CHUNK: i32 = 16;

/// Right-shift a cell coordinate by this to derive its chunk
/// (`2^4 = 16` cells per chunk edge). Arithmetic shift — floors on
/// negatives.
pub const CHUNK_BITS: u32 = 4;

/// Cells in one chunk: `16 × 16`. The length of every per-chunk plane.
pub const CELLS_PER_CHUNK_AREA: usize = 256;

/// Subcells along one edge of a cell — the overlay coverage resolution.
/// A cell's [`Chunk::overlay_mask`] is a `SUB × SUB` bit grid. Raising
/// this is a single-constant change; the wire mask-plane length is
/// derived from it ([`OVERLAY_MASK_WIRE_BYTES`]), never hard-coded, so
/// no wire migration is needed. The in-memory mask is a `u16`, which
/// holds `SUB = 4` (16 bits).
pub const SUBCELLS_PER_CELL_EDGE: u32 = 4;

/// Bits in one cell's overlay coverage mask: `SUB²`.
pub const SUBCELLS_PER_CELL: usize = (SUBCELLS_PER_CELL_EDGE * SUBCELLS_PER_CELL_EDGE) as usize;

/// Wire length in bytes of a chunk's overlay-mask plane:
/// `CELLS_PER_CHUNK_AREA * SUB² / 8` (= 512 at `SUB = 4`). The plane
/// travels as little-endian mask words per cell, row-major cell order.
pub const OVERLAY_MASK_WIRE_BYTES: usize = CELLS_PER_CHUNK_AREA * SUBCELLS_PER_CELL / 8;

/// Octimeters per cell: `1 cell = 1 m = 256 octimeters`.
const OCTIMETERS_PER_CELL: i32 = 256;

/// Right-shift an octimeter coordinate by this to derive its cell.
const OCTIMETER_BITS: u32 = 8;

/// A cell address on the world lattice. Cells are addresses; their
/// properties live in the plane stack.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct CellPos {
    pub x: i32,
    pub z: i32,
}

/// A chunk address — a cell address right-shifted by [`CHUNK_BITS`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct ChunkPos {
    pub x: i32,
    pub z: i32,
}

impl CellPos {
    /// The chunk this cell belongs to. Arithmetic right shift, so
    /// negative cells floor toward `-∞`.
    #[must_use]
    pub fn chunk(self) -> ChunkPos {
        ChunkPos {
            x: self.x >> CHUNK_BITS,
            z: self.z >> CHUNK_BITS,
        }
    }

    /// The cell's center in octimeters — cell-center-anchored, so a
    /// mover placed here sits in the middle of the cell, not on its
    /// corner. `(x << 8) + 128`.
    #[must_use]
    pub fn center_octimeters(self) -> (i32, i32) {
        (
            (self.x << OCTIMETER_BITS) + OCTIMETERS_PER_CELL / 2,
            (self.z << OCTIMETER_BITS) + OCTIMETERS_PER_CELL / 2,
        )
    }

    /// The cell an octimeter position sits in. Arithmetic right shift —
    /// negative positions floor.
    #[must_use]
    pub fn from_octimeters(x: i32, z: i32) -> Self {
        Self {
            x: x >> OCTIMETER_BITS,
            z: z >> OCTIMETER_BITS,
        }
    }

    /// Index of this cell within its chunk's row-major planes.
    /// `rem_euclid` so negative cells map into `0..256` correctly.
    fn chunk_index(self) -> usize {
        (self.z.rem_euclid(CELLS_PER_CHUNK) * CELLS_PER_CHUNK + self.x.rem_euclid(CELLS_PER_CHUNK))
            as usize
    }
}

/// The ground-material vocabulary. Rides the wire as a raw `u8` inside a
/// `Bytes` plane, never as a schema enum, so a plane has one canonical
/// byte-array form. `Void` (`0`) is "nothing here".
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Material {
    #[default]
    Void = 0,
    Grass = 1,
    Dirt = 2,
    Stone = 3,
    Sand = 4,
    Water = 5,
}

impl Material {
    /// The raw wire byte.
    #[must_use]
    pub fn to_u8(self) -> u8 {
        self as u8
    }

    /// Decode a wire byte, degrading an unknown value to [`Material::Void`]
    /// rather than erroring — a malformed plane byte becomes empty space,
    /// not a panic.
    #[must_use]
    pub fn from_u8_or_void(byte: u8) -> Self {
        Self::try_from(byte).unwrap_or(Self::Void)
    }
}

impl TryFrom<u8> for Material {
    type Error = u8;

    fn try_from(byte: u8) -> Result<Self, u8> {
        match byte {
            0 => Ok(Self::Void),
            1 => Ok(Self::Grass),
            2 => Ok(Self::Dirt),
            3 => Ok(Self::Stone),
            4 => Ok(Self::Sand),
            5 => Ok(Self::Water),
            other => Err(other),
        }
    }
}

/// A semantic group of cells with a default ground material. Regions are
/// referenced by 1-based id from the per-cell region plane (`0` = no
/// region); the region table is positional, so a region's id is its
/// index in [`World`]'s table plus one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    pub name: String,
    pub default_material: Material,
}

/// Ceiling on a smoothing profile's iteration count. The contour pass
/// reads `2 × iterations` subcells outward, so this cap keeps every
/// field-driven read inside the mesher's fixed two-cell apron — the
/// invariant the `R = 1` neighbor remesh relies on.
pub const MAX_SMOOTHING_ITERATIONS: u32 = 4;

/// A contour-smoothing profile — the number and the degrees a cell's
/// smoothing plane points at. Referenced by 1-based id from the per-cell
/// smoothing plane (`0` = no override, the material default applies); the
/// table is positional like the region table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmoothingProfile {
    /// Corner-minimization iteration count (`0` = raw blocky contours).
    /// Clamped to [`MAX_SMOOTHING_ITERATIONS`] at registration.
    pub iterations: u32,
    /// Corner angle in degrees the cellular passes flatten down to (`90`
    /// rounds only true right angles; smaller rounds gentler junctions).
    /// Clamped to `[45, 90]` at registration — the windowed rule's
    /// threshold derivation assumes at least 45.
    pub degrees: u32,
}

/// One `16 × 16` block of the world, as a struct-of-arrays: five
/// property planes, each row-major (`z * 16 + x`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// Ground fabric — cascade-resolved by [`World::underlay`].
    pub underlay: [Material; CELLS_PER_CHUNK_AREA],
    /// Placed surface — raw, never cascade-resolved. `Void` = none.
    pub overlay: [Material; CELLS_PER_CHUNK_AREA],
    /// Overlay subcell coverage bitmask per cell — a `SUB × SUB` bit grid
    /// (bit `z*SUB + x`). `0xFFFF` at `SUB = 4` is full coverage; `0` is
    /// none. Meaningless where `overlay` is `Void`. Reserved; not
    /// interpreted here.
    pub overlay_mask: [u16; CELLS_PER_CHUNK_AREA],
    /// Elevation in octimeters (`0` = flat).
    pub height: [i32; CELLS_PER_CHUNK_AREA],
    /// Region id per cell (`0` = no region).
    pub region: [u16; CELLS_PER_CHUNK_AREA],
    /// Smoothing-profile id per cell (`0` = no override — the material's
    /// own smoothing applies).
    pub smoothing: [u8; CELLS_PER_CHUNK_AREA],
}

impl Chunk {
    /// An empty chunk — all planes `Void` / zero.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            underlay: [Material::Void; CELLS_PER_CHUNK_AREA],
            overlay: [Material::Void; CELLS_PER_CHUNK_AREA],
            overlay_mask: [0; CELLS_PER_CHUNK_AREA],
            height: [0; CELLS_PER_CHUNK_AREA],
            region: [0; CELLS_PER_CHUNK_AREA],
            smoothing: [0; CELLS_PER_CHUNK_AREA],
        }
    }
}

impl Default for Chunk {
    fn default() -> Self {
        Self::empty()
    }
}

/// The world: a sparse set of chunks plus a region table. Cells with no
/// chunk read as `Void` / `0`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct World {
    chunks: BTreeMap<ChunkPos, Chunk>,
    regions: Vec<Region>,
    smoothing_profiles: Vec<SmoothingProfile>,
}

impl World {
    /// An empty world.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The cascade-resolved ground material at `cell`: the cell's own
    /// underlay if non-`Void`, else the cell's region's `default_material`,
    /// else `Void`.
    #[must_use]
    pub fn underlay(&self, cell: CellPos) -> Material {
        let Some(chunk) = self.chunks.get(&cell.chunk()) else {
            return Material::Void;
        };
        let idx = cell.chunk_index();
        let own = chunk.underlay[idx];
        if own != Material::Void {
            return own;
        }
        let region_id = chunk.region[idx];
        if region_id != 0
            && let Some(region) = self.regions.get(region_id as usize - 1)
        {
            return region.default_material;
        }
        Material::Void
    }

    /// The raw overlay material at `cell` — never cascade-resolved.
    #[must_use]
    pub fn overlay(&self, cell: CellPos) -> Material {
        self.chunks
            .get(&cell.chunk())
            .map_or(Material::Void, |chunk| chunk.overlay[cell.chunk_index()])
    }

    /// The raw overlay subcell coverage mask at `cell` — never
    /// cascade-resolved. A missing chunk reads `0` (no coverage), which is
    /// the apron read the mesher relies on: a chunk-border window can
    /// sample one subcell into an absent neighbor and see empty space
    /// rather than panicking. The bits are meaningless where
    /// [`World::overlay`] is `Void`.
    #[must_use]
    pub fn overlay_mask(&self, cell: CellPos) -> u16 {
        self.chunks
            .get(&cell.chunk())
            .map_or(0, |chunk| chunk.overlay_mask[cell.chunk_index()])
    }

    /// Elevation at `cell` in octimeters. Unset cells read `0`.
    #[must_use]
    pub fn height(&self, cell: CellPos) -> i32 {
        self.chunks
            .get(&cell.chunk())
            .map_or(0, |chunk| chunk.height[cell.chunk_index()])
    }

    /// The chunk at `at`, if present.
    #[must_use]
    pub fn chunk(&self, at: ChunkPos) -> Option<&Chunk> {
        self.chunks.get(&at)
    }

    /// Insert (or replace) the chunk at `at`.
    pub fn insert_chunk(&mut self, at: ChunkPos, chunk: Chunk) {
        self.chunks.insert(at, chunk);
    }

    /// Register a region under a 1-based `id`. The table is positional,
    /// so this grows it (padding intervening slots with empty regions)
    /// and writes `region` at index `id - 1`. `id == 0` is ignored (`0`
    /// is the "no region" sentinel).
    pub fn insert_region(&mut self, id: u32, region: Region) {
        if id == 0 {
            return;
        }
        let index = id as usize - 1;
        if index >= self.regions.len() {
            self.regions.resize(
                index + 1,
                Region {
                    name: String::new(),
                    default_material: Material::Void,
                },
            );
        }
        self.regions[index] = region;
    }

    /// Register a smoothing profile under a 1-based `id`, clamping
    /// `iterations` to [`MAX_SMOOTHING_ITERATIONS`] and `degrees` to
    /// `[45, 90]`. The table is positional like the region table; `id == 0`
    /// is ignored (`0` is the "no override" sentinel).
    pub fn insert_smoothing_profile(&mut self, id: u32, profile: SmoothingProfile) {
        if id == 0 {
            return;
        }
        let clamped = SmoothingProfile {
            iterations: profile.iterations.min(MAX_SMOOTHING_ITERATIONS),
            degrees: profile.degrees.clamp(45, 90),
        };
        let index = id as usize - 1;
        if index >= self.smoothing_profiles.len() {
            self.smoothing_profiles.resize(
                index + 1,
                SmoothingProfile {
                    iterations: 0,
                    degrees: 90,
                },
            );
        }
        self.smoothing_profiles[index] = clamped;
    }

    /// The smoothing override at `cell`, if the cell's smoothing plane
    /// points at a registered profile. `None` — plane `0`, missing chunk,
    /// or an unregistered id — means the material default applies.
    #[must_use]
    pub fn smoothing_override(&self, cell: CellPos) -> Option<SmoothingProfile> {
        let chunk = self.chunks.get(&cell.chunk())?;
        let id = chunk.smoothing[cell.chunk_index()];
        if id == 0 {
            return None;
        }
        self.smoothing_profiles.get(id as usize - 1).copied()
    }

    /// Iterate the chunk set in `ChunkPos` order (deterministic — the
    /// `BTreeMap` key order).
    pub fn chunks(&self) -> impl Iterator<Item = (ChunkPos, &Chunk)> {
        self.chunks.iter().map(|(pos, chunk)| (*pos, chunk))
    }
}

/// `aether.kit.world.set_chunk` — write one chunk's planes. The three
/// ground planes ride as raw `Bytes` (256 material/shape bytes each);
/// `height` / `region` ride as length-256 vectors. Shorter vectors pad
/// with `Void` / `0`; longer ones truncate.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.set_chunk")]
pub struct SetChunk {
    pub chunk_x: i32,
    pub chunk_z: i32,
    /// Underlay plane — 256 raw material bytes (`Material as u8`).
    pub underlay: Vec<u8>,
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
        for (dst, byte) in chunk.smoothing.iter_mut().zip(&self.smoothing) {
            *dst = *byte;
        }
        chunk
    }
}

/// `aether.kit.world.set_region` — register a region in the table under a
/// 1-based `region_id`, giving the underlay cascade a default material to
/// fall through to. `default_material` is a raw `Material` byte.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.set_region")]
pub struct SetRegion {
    pub region_id: u32,
    pub name: String,
    pub default_material: u8,
}

impl SetRegion {
    /// The [`Region`] this registers, decoding `default_material`
    /// (unknown → `Void`).
    #[must_use]
    pub fn into_region(self) -> Region {
        Region {
            name: self.name,
            default_material: Material::from_u8_or_void(self.default_material),
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

/// A [`World::from_bytes`] failure — the buffer was truncated or carried
/// an unknown format version.
#[derive(Debug, PartialEq, Eq)]
pub enum WorldDecodeError {
    /// Ran off the end of the buffer mid-record.
    Truncated,
    /// First byte was not a recognized format version.
    BadVersion(u8),
    /// A region name was not valid UTF-8.
    BadName,
}

/// The current write version. Version 2 adds the smoothing-profile table
/// (after the region table) and the per-chunk smoothing plane (after the
/// region plane); version 1 buffers still decode, reading as an empty
/// table and an all-zero plane.
const WORLD_FORMAT_VERSION: u8 = 2;

/// The oldest version [`World::from_bytes`] still decodes.
const WORLD_FORMAT_VERSION_MIN: u8 = 1;

impl World {
    /// Serialize to the compact `aether.kit.world.load` binary format: a
    /// version byte, the region table, the smoothing-profile table, then
    /// per-chunk plane records — all little-endian. Region and profile ids
    /// are positional (index + 1), so the table order is the id order.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(WORLD_FORMAT_VERSION);
        out.extend_from_slice(&(self.regions.len() as u32).to_le_bytes());
        for region in &self.regions {
            let name = region.name.as_bytes();
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(name);
            out.push(region.default_material.to_u8());
        }
        out.extend_from_slice(&(self.smoothing_profiles.len() as u32).to_le_bytes());
        for profile in &self.smoothing_profiles {
            out.push(profile.iterations as u8);
            out.extend_from_slice(&(profile.degrees as u16).to_le_bytes());
        }
        out.extend_from_slice(&(self.chunks.len() as u32).to_le_bytes());
        for (pos, chunk) in &self.chunks {
            out.extend_from_slice(&pos.x.to_le_bytes());
            out.extend_from_slice(&pos.z.to_le_bytes());
            for m in &chunk.underlay {
                out.push(m.to_u8());
            }
            for m in &chunk.overlay {
                out.push(m.to_u8());
            }
            for mask in &chunk.overlay_mask {
                out.extend_from_slice(&mask.to_le_bytes());
            }
            for h in &chunk.height {
                out.extend_from_slice(&h.to_le_bytes());
            }
            for r in &chunk.region {
                out.extend_from_slice(&r.to_le_bytes());
            }
            out.extend_from_slice(&chunk.smoothing);
        }
        out
    }

    /// Decode the [`World::to_bytes`] format, current or version 1 (which
    /// carries no smoothing table or plane — both read empty / zero). A
    /// truncated buffer or unknown version returns `Err` rather than
    /// panicking; the caller keeps its prior world on any error.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, WorldDecodeError> {
        let mut reader = Reader::new(bytes);
        let version = reader.u8()?;
        if !(WORLD_FORMAT_VERSION_MIN..=WORLD_FORMAT_VERSION).contains(&version) {
            return Err(WorldDecodeError::BadVersion(version));
        }
        let region_count = reader.u32()? as usize;
        let mut regions = Vec::with_capacity(region_count);
        for _ in 0..region_count {
            let name_len = reader.u16()? as usize;
            let name_bytes = reader.take(name_len)?;
            let name =
                String::from_utf8(name_bytes.to_vec()).map_err(|_| WorldDecodeError::BadName)?;
            let default_material = Material::from_u8_or_void(reader.u8()?);
            regions.push(Region {
                name,
                default_material,
            });
        }
        let mut smoothing_profiles = Vec::new();
        if version >= 2 {
            let profile_count = reader.u32()? as usize;
            smoothing_profiles.reserve(profile_count);
            for _ in 0..profile_count {
                let iterations = u32::from(reader.u8()?);
                let degrees = u32::from(reader.u16()?);
                smoothing_profiles.push(SmoothingProfile {
                    iterations,
                    degrees,
                });
            }
        }
        let chunk_count = reader.u32()? as usize;
        let mut chunks = BTreeMap::new();
        for _ in 0..chunk_count {
            let x = reader.i32()?;
            let z = reader.i32()?;
            let mut chunk = Chunk::empty();
            for slot in &mut chunk.underlay {
                *slot = Material::from_u8_or_void(reader.u8()?);
            }
            for slot in &mut chunk.overlay {
                *slot = Material::from_u8_or_void(reader.u8()?);
            }
            for slot in &mut chunk.overlay_mask {
                *slot = reader.u16()?;
            }
            for slot in &mut chunk.height {
                *slot = reader.i32()?;
            }
            for slot in &mut chunk.region {
                *slot = reader.u16()?;
            }
            if version >= 2 {
                for slot in &mut chunk.smoothing {
                    *slot = reader.u8()?;
                }
            }
            chunks.insert(ChunkPos { x, z }, chunk);
        }
        Ok(Self {
            chunks,
            regions,
            smoothing_profiles,
        })
    }
}

/// A bounds-checked little-endian byte cursor for [`World::from_bytes`].
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WorldDecodeError> {
        let end = self.pos.checked_add(n).ok_or(WorldDecodeError::Truncated)?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(WorldDecodeError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, WorldDecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, WorldDecodeError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, WorldDecodeError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn i32(&mut self) -> Result<i32, WorldDecodeError> {
        let b = self.take(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(x: i32, z: i32) -> CellPos {
        CellPos { x, z }
    }

    #[test]
    fn chunk_shift_floors_on_negative_cells() {
        // Cell 0 and 15 are both in chunk 0; cell 16 in chunk 1.
        assert_eq!(cell(0, 0).chunk(), ChunkPos { x: 0, z: 0 });
        assert_eq!(cell(15, 15).chunk(), ChunkPos { x: 0, z: 0 });
        assert_eq!(cell(16, 16).chunk(), ChunkPos { x: 1, z: 1 });
        // Arithmetic shift floors: cell -1 is in chunk -1, not 0.
        assert_eq!(cell(-1, -1).chunk(), ChunkPos { x: -1, z: -1 });
        assert_eq!(cell(-16, -16).chunk(), ChunkPos { x: -1, z: -1 });
        assert_eq!(cell(-17, -17).chunk(), ChunkPos { x: -2, z: -2 });
    }

    #[test]
    fn from_octimeters_floors_on_negative_positions() {
        // 256 octimeters per cell; cell 0 spans [0,256), cell -1 spans [-256,0).
        assert_eq!(CellPos::from_octimeters(0, 0), cell(0, 0));
        assert_eq!(CellPos::from_octimeters(255, 255), cell(0, 0));
        assert_eq!(CellPos::from_octimeters(256, 256), cell(1, 1));
        assert_eq!(CellPos::from_octimeters(-1, -1), cell(-1, -1));
        assert_eq!(CellPos::from_octimeters(-256, -256), cell(-1, -1));
        assert_eq!(CellPos::from_octimeters(-257, -257), cell(-2, -2));
    }

    #[test]
    fn center_octimeters_is_cell_center() {
        assert_eq!(cell(0, 0).center_octimeters(), (128, 128));
        assert_eq!(cell(1, 2).center_octimeters(), (384, 640));
        assert_eq!(cell(-1, -1).center_octimeters(), (-128, -128));
    }

    #[test]
    fn negative_cell_indexes_its_chunk_correctly() {
        // Cell -1 sits at local (15,15) of chunk -1 → index 255.
        assert_eq!(cell(-1, -1).chunk_index(), 15 * 16 + 15);
        // Cell -16 sits at local (0,0) of chunk -1 → index 0.
        assert_eq!(cell(-16, -16).chunk_index(), 0);
    }

    #[test]
    fn underlay_cascade_resolves_cell_then_region_then_void() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        // Cell (2,3): explicit Stone underlay — cell override wins.
        chunk.underlay[3 * 16 + 2] = Material::Stone;
        // Cell (4,5): Void underlay but in region 1 → region default.
        chunk.region[5 * 16 + 4] = 1;
        // Cell (6,7): Void underlay, no region → Void.
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world.insert_region(
            1,
            Region {
                name: "meadow".into(),
                default_material: Material::Grass,
            },
        );

        assert_eq!(world.underlay(cell(2, 3)), Material::Stone, "cell override");
        assert_eq!(
            world.underlay(cell(4, 5)),
            Material::Grass,
            "region default"
        );
        assert_eq!(
            world.underlay(cell(6, 7)),
            Material::Void,
            "no cascade source"
        );
    }

    #[test]
    fn overlay_never_cascades() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        // Cell in region 1 with a region default, but Void overlay.
        chunk.region[0] = 1;
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world.insert_region(
            1,
            Region {
                name: "r".into(),
                default_material: Material::Grass,
            },
        );
        // Underlay cascades to Grass; overlay stays raw Void.
        assert_eq!(world.underlay(cell(0, 0)), Material::Grass);
        assert_eq!(world.overlay(cell(0, 0)), Material::Void);
    }

    #[test]
    fn sparse_world_reads_void_and_zero() {
        let world = World::new();
        assert_eq!(world.underlay(cell(100, -50)), Material::Void);
        assert_eq!(world.overlay(cell(100, -50)), Material::Void);
        assert_eq!(world.overlay_mask(cell(100, -50)), 0);
        assert_eq!(world.height(cell(100, -50)), 0);
        assert!(world.chunk(ChunkPos { x: 3, z: 3 }).is_none());
    }

    #[test]
    fn set_chunk_decodes_planes_and_clamps_unknown_material() {
        let mut underlay = vec![0u8; CELLS_PER_CHUNK_AREA];
        underlay[3 * 16 + 2] = Material::Water.to_u8();
        underlay[0] = 99; // unknown byte → Void
        let mut region = vec![0u32; CELLS_PER_CHUNK_AREA];
        region[1] = 7;
        let set = SetChunk {
            chunk_x: 2,
            chunk_z: -1,
            underlay,
            overlay: vec![0u8; CELLS_PER_CHUNK_AREA],
            overlay_mask: vec![0u8; OVERLAY_MASK_WIRE_BYTES],
            height: vec![0i32; CELLS_PER_CHUNK_AREA],
            region,
            smoothing: vec![0u8; CELLS_PER_CHUNK_AREA],
        };
        assert_eq!(set.chunk_pos(), ChunkPos { x: 2, z: -1 });
        let chunk = set.into_chunk();
        assert_eq!(chunk.underlay[3 * 16 + 2], Material::Water);
        assert_eq!(
            chunk.underlay[0],
            Material::Void,
            "unknown byte clamps to Void"
        );
        assert_eq!(chunk.region[1], 7);
    }

    #[test]
    fn set_chunk_decodes_overlay_mask_as_le_words() {
        // 512-byte mask plane; cell 1's word set to 0xBEEF (LE bytes EF BE).
        let mut overlay_mask = vec![0u8; OVERLAY_MASK_WIRE_BYTES];
        overlay_mask[2] = 0xEF;
        overlay_mask[3] = 0xBE;
        let set = SetChunk {
            chunk_x: 0,
            chunk_z: 0,
            underlay: Vec::new(),
            overlay: Vec::new(),
            overlay_mask,
            height: Vec::new(),
            region: Vec::new(),
            smoothing: Vec::new(),
        };
        let chunk = set.into_chunk();
        assert_eq!(chunk.overlay_mask[0], 0);
        assert_eq!(chunk.overlay_mask[1], 0xBEEF);
        assert_eq!(OVERLAY_MASK_WIRE_BYTES, 512, "SUB=4 → 256*16/8 bytes");
    }

    #[test]
    fn set_chunk_pads_short_planes_and_truncates_long() {
        let set = SetChunk {
            chunk_x: 0,
            chunk_z: 0,
            underlay: vec![Material::Grass.to_u8(); 2], // short → rest Void
            overlay: vec![0u8; CELLS_PER_CHUNK_AREA + 10], // long → truncated
            overlay_mask: Vec::new(),
            height: vec![5i32; 1],
            region: Vec::new(),
            smoothing: vec![3u8; 1], // short → rest no-override
        };
        let chunk = set.into_chunk();
        assert_eq!(chunk.underlay[0], Material::Grass);
        assert_eq!(chunk.underlay[1], Material::Grass);
        assert_eq!(chunk.underlay[2], Material::Void);
        assert_eq!(chunk.height[0], 5);
        assert_eq!(chunk.height[1], 0);
        assert_eq!(chunk.smoothing[0], 3);
        assert_eq!(chunk.smoothing[1], 0);
    }

    #[test]
    fn world_bytes_roundtrip() {
        let mut world = World::new();
        world.insert_region(
            1,
            Region {
                name: "meadow".into(),
                default_material: Material::Grass,
            },
        );
        world.insert_region(
            2,
            Region {
                name: "shore".into(),
                default_material: Material::Sand,
            },
        );
        world.insert_smoothing_profile(
            1,
            SmoothingProfile {
                iterations: 3,
                degrees: 60,
            },
        );
        let mut a = Chunk::empty();
        a.underlay[0] = Material::Stone;
        a.overlay[5] = Material::Water;
        a.overlay_mask[5] = 0x0F0F;
        a.height[10] = -42;
        a.region[20] = 2;
        a.smoothing[30] = 1;
        world.insert_chunk(ChunkPos { x: 1, z: -3 }, a);
        let mut b = Chunk::empty();
        b.underlay[255] = Material::Dirt;
        world.insert_chunk(ChunkPos { x: -7, z: 4 }, b);

        let bytes = world.to_bytes();
        let decoded = World::from_bytes(&bytes).expect("roundtrip decodes");

        // Structural equality across the whole world.
        assert_eq!(decoded.regions, world.regions);
        assert_eq!(decoded.smoothing_profiles, world.smoothing_profiles);
        assert_eq!(
            decoded.chunk(ChunkPos { x: 1, z: -3 }),
            world.chunk(ChunkPos { x: 1, z: -3 })
        );
        assert_eq!(
            decoded.chunk(ChunkPos { x: -7, z: 4 }),
            world.chunk(ChunkPos { x: -7, z: 4 })
        );
    }

    #[test]
    fn version_one_buffer_decodes_with_no_smoothing() {
        // Tripwire: the version-1 layout — no profile table, no per-chunk
        // smoothing plane — is pinned here byte-for-byte and must keep
        // decoding as long as WORLD_FORMAT_VERSION_MIN is 1. Build one v1
        // buffer by hand: one region, one chunk with a Stone cell.
        let mut buf = vec![1u8];
        buf.extend_from_slice(&1u32.to_le_bytes()); // one region
        buf.extend_from_slice(&6u16.to_le_bytes());
        buf.extend_from_slice(b"meadow");
        buf.push(Material::Grass.to_u8());
        buf.extend_from_slice(&1u32.to_le_bytes()); // one chunk
        buf.extend_from_slice(&2i32.to_le_bytes());
        buf.extend_from_slice(&(-1i32).to_le_bytes());
        let mut underlay = [0u8; CELLS_PER_CHUNK_AREA];
        underlay[7] = Material::Stone.to_u8();
        buf.extend_from_slice(&underlay);
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // overlay
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // masks
        buf.extend_from_slice(&[0u8; 4 * CELLS_PER_CHUNK_AREA]); // heights
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // regions

        let world = World::from_bytes(&buf).expect("a v1 buffer still decodes");
        assert_eq!(world.regions.len(), 1);
        assert!(world.smoothing_profiles.is_empty());
        let chunk = world.chunk(ChunkPos { x: 2, z: -1 }).expect("chunk");
        assert_eq!(chunk.underlay[7], Material::Stone);
        assert_eq!(chunk.smoothing, [0u8; CELLS_PER_CHUNK_AREA]);
    }

    #[test]
    fn smoothing_override_resolves_plane_then_table_then_none() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.smoothing[0] = 1; // registered below
        chunk.smoothing[1] = 9; // never registered
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world.insert_smoothing_profile(
            1,
            SmoothingProfile {
                iterations: 7, // past the cap — clamps to 4
                degrees: 30,   // under the floor — clamps to 45
            },
        );

        assert_eq!(
            world.smoothing_override(cell(0, 0)),
            Some(SmoothingProfile {
                iterations: 4,
                degrees: 45,
            }),
            "registration clamps to the apron-safe range",
        );
        assert_eq!(
            world.smoothing_override(cell(1, 0)),
            None,
            "an unregistered id is no override",
        );
        assert_eq!(
            world.smoothing_override(cell(2, 0)),
            None,
            "plane 0 is no override",
        );
        assert_eq!(
            world.smoothing_override(cell(100, 100)),
            None,
            "a missing chunk is no override",
        );
    }

    #[test]
    fn from_bytes_rejects_truncated_and_bad_version() {
        assert_eq!(World::from_bytes(&[]), Err(WorldDecodeError::Truncated));
        assert_eq!(
            World::from_bytes(&[9]),
            Err(WorldDecodeError::BadVersion(9))
        );
        // Version + a region count claiming one region, but no region bytes.
        let mut buf = vec![WORLD_FORMAT_VERSION];
        buf.extend_from_slice(&1u32.to_le_bytes());
        assert_eq!(World::from_bytes(&buf), Err(WorldDecodeError::Truncated));
    }

    #[test]
    fn insert_region_ignores_zero_and_grows_table() {
        let mut world = World::new();
        world.insert_region(
            0,
            Region {
                name: "ignored".into(),
                default_material: Material::Grass,
            },
        );
        // id 0 is the no-region sentinel — table stays empty, so a cell
        // pointing at region 1 finds no default and reads Void.
        let mut chunk = Chunk::empty();
        chunk.region[0] = 1;
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        assert_eq!(world.underlay(cell(0, 0)), Material::Void);

        // Inserting id 3 grows the table to length 3 (ids 1,2 padded
        // empty); a cell pointing at region 3 resolves its default.
        world.insert_region(
            3,
            Region {
                name: "third".into(),
                default_material: Material::Stone,
            },
        );
        let mut chunk3 = Chunk::empty();
        chunk3.region[0] = 3;
        world.insert_chunk(ChunkPos { x: 1, z: 0 }, chunk3);
        assert_eq!(world.underlay(cell(16, 0)), Material::Stone);
    }
}
