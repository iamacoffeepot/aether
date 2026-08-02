//! The compact `aether.kit.world.load` binary format: the current writer,
//! the version-tolerant reader, and the bounds-checked byte cursor they
//! share.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use super::chunk::Chunk;
use super::coords::ChunkPos;
use super::layout::{
    CELLS_PER_CHUNK_AREA, HEIGHT_POINTS_PER_CHUNK, LEGACY_MASK_SUBCELLS_PER_CELL_EDGE, OVERLAY_MASK_WIRE_BYTES,
    SUBCELLS_PER_CELL, SUBCELLS_PER_CELL_EDGE, UNDERLAY_POINTS_PER_CHUNK,
};
use super::material::{Material, cliff_material_from_u8};
use super::table::{Region, SmoothingProfile, WaterPlane};
use super::world::World;

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
    /// A table count exceeded the format's addressable or operational cap.
    LimitExceeded,
}

/// The current write version. Version 7 expands the per-cell packed
/// overlay mask words into one scalar coverage byte per subcell; older
/// packed bits decode as `0` / `255`. Version 6 appends the per-chunk
/// height-delta plane ([`HEIGHT_POINTS_PER_CHUNK`] `i16` octimeter deltas)
/// to the end of each chunk record, after the underlay-point plane.
/// Version 5 appends the per-chunk underlay-point plane
/// ([`UNDERLAY_POINTS_PER_CHUNK`] bytes) to the end of each chunk record.
/// Version 4 adds the water-plane table (after the smoothing-profile table)
/// and the per-chunk water-plane plane (after the height plane); version 3
/// adds a cliff-material byte to each region record; version 2 adds the
/// smoothing-profile table (after the region table) and the per-chunk
/// smoothing plane (after the region plane). Older buffers still decode: a
/// pre-7 buffer expands packed overlay bits to binary coverage, a pre-6
/// buffer reads an all-zero height-delta plane, a pre-5 buffer reads an
/// all-inherit underlay-point plane, a pre-4 buffer reads an empty
/// water-plane table and an all-zero water plane, a pre-3 region reads
/// Stone cliffs, a version-1 buffer reads an empty profile table and an
/// all-zero smoothing plane.
const WORLD_FORMAT_VERSION: u8 = 7;

/// The oldest version [`World::from_bytes`] still decodes.
const WORLD_FORMAT_VERSION_MIN: u8 = 1;

const MAX_DECODED_REGIONS: usize = u16::MAX as usize;
const MAX_DECODED_SMOOTHING_PROFILES: usize = u8::MAX as usize;
const MAX_DECODED_WATER_PLANES: usize = u16::MAX as usize;
const MAX_DECODED_CHUNKS: usize = 65_536;

fn chunk_record_bytes(version: u8) -> usize {
    let overlay_mask_bytes = if version >= 7 {
        OVERLAY_MASK_WIRE_BYTES
    } else {
        2 * CELLS_PER_CHUNK_AREA
    };
    8 + 2 * CELLS_PER_CHUNK_AREA
        + overlay_mask_bytes
        + 4 * CELLS_PER_CHUNK_AREA
        + if version >= 4 {
            2 * CELLS_PER_CHUNK_AREA
        } else {
            0
        }
        + 2 * CELLS_PER_CHUNK_AREA
        + if version >= 2 {
            CELLS_PER_CHUNK_AREA
        } else {
            0
        }
        + if version >= 5 {
            UNDERLAY_POINTS_PER_CHUNK
        } else {
            0
        }
        + if version >= 6 {
            2 * HEIGHT_POINTS_PER_CHUNK
        } else {
            0
        }
}

impl World {
    /// Serialize to the compact `aether.kit.world.load` binary format: a
    /// version byte, the region table, the smoothing-profile table, the
    /// water-plane table, then per-chunk plane records — all little-endian.
    /// Region, profile, and water-plane ids are positional (index + 1), so
    /// the table order is the id order.
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
            out.push(region.cliff_material.to_u8());
        }
        out.extend_from_slice(&(self.smoothing_profiles.len() as u32).to_le_bytes());
        for profile in &self.smoothing_profiles {
            out.push(profile.iterations as u8);
            out.extend_from_slice(&(profile.degrees as u16).to_le_bytes());
        }
        out.extend_from_slice(&(self.water_planes.len() as u32).to_le_bytes());
        for plane in &self.water_planes {
            out.extend_from_slice(&plane.level_octimeters.to_le_bytes());
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
            out.extend_from_slice(&chunk.overlay_mask);
            for h in &chunk.height {
                out.extend_from_slice(&h.to_le_bytes());
            }
            for w in &chunk.water_plane {
                out.extend_from_slice(&w.to_le_bytes());
            }
            for r in &chunk.region {
                out.extend_from_slice(&r.to_le_bytes());
            }
            out.extend_from_slice(&chunk.smoothing);
            out.extend_from_slice(&chunk.underlay_points);
            for delta in &chunk.height_points {
                out.extend_from_slice(&delta.to_le_bytes());
            }
        }
        out
    }

    /// Decode the [`World::to_bytes`] format, current or older (a pre-6
    /// buffer carries no height-delta plane — it reads all-zero relief; a
    /// pre-5 buffer carries no underlay-point plane — it reads all-inherit; a
    /// pre-4 buffer carries no water-plane table or plane — both read empty
    /// / zero; a pre-3 region reads Stone cliffs; a version-1 buffer carries
    /// no smoothing table or plane). A truncated buffer or unknown version
    /// returns `Err` rather than panicking; the caller keeps its prior
    /// world on any error.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, WorldDecodeError> {
        let mut reader = Reader::new(bytes);
        let version = reader.u8()?;
        if !(WORLD_FORMAT_VERSION_MIN..=WORLD_FORMAT_VERSION).contains(&version) {
            return Err(WorldDecodeError::BadVersion(version));
        }
        let region_count_raw = reader.u32()?;
        let region_count =
            reader.checked_count(region_count_raw, 2 + 1 + usize::from(version >= 3), MAX_DECODED_REGIONS)?;
        let mut regions = Vec::with_capacity(region_count);
        for _ in 0..region_count {
            let name_len = reader.u16()? as usize;
            let name_bytes = reader.take(name_len)?;
            let name = String::from_utf8(name_bytes.to_vec()).map_err(|_| WorldDecodeError::BadName)?;
            let default_material = Material::from_u8_or_void(reader.u8()?);
            let cliff_material = if version >= 3 {
                cliff_material_from_u8(reader.u8()?)
            } else {
                Material::Stone
            };
            regions.push(Region { name, default_material, cliff_material });
        }
        let mut smoothing_profiles = Vec::new();
        if version >= 2 {
            let profile_count_raw = reader.u32()?;
            let profile_count = reader.checked_count(profile_count_raw, 3, MAX_DECODED_SMOOTHING_PROFILES)?;
            smoothing_profiles.reserve(profile_count);
            for _ in 0..profile_count {
                let iterations = u32::from(reader.u8()?);
                let degrees = u32::from(reader.u16()?);
                smoothing_profiles.push(SmoothingProfile { iterations, degrees });
            }
        }
        let mut water_planes = Vec::new();
        if version >= 4 {
            let plane_count_raw = reader.u32()?;
            let plane_count = reader.checked_count(plane_count_raw, 4, MAX_DECODED_WATER_PLANES)?;
            water_planes.reserve(plane_count);
            for _ in 0..plane_count {
                water_planes.push(WaterPlane { level_octimeters: reader.i32()? });
            }
        }
        let chunk_count_raw = reader.u32()?;
        let chunk_count = reader.checked_count(chunk_count_raw, chunk_record_bytes(version), MAX_DECODED_CHUNKS)?;
        let mut chunks = BTreeMap::new();
        for _ in 0..chunk_count {
            let x = reader.i32()?;
            let z = reader.i32()?;
            let mut chunk = Chunk::empty_boxed();
            for slot in &mut chunk.underlay {
                *slot = Material::from_u8_or_void(reader.u8()?);
            }
            for slot in &mut chunk.overlay {
                *slot = Material::from_u8_or_void(reader.u8()?);
            }
            read_overlay_mask(&mut reader, version, &mut chunk)?;
            for slot in &mut chunk.height {
                *slot = reader.i32()?;
            }
            if version >= 4 {
                for slot in &mut chunk.water_plane {
                    *slot = reader.u16()?;
                }
            }
            for slot in &mut chunk.region {
                *slot = reader.u16()?;
            }
            if version >= 2 {
                for slot in &mut chunk.smoothing {
                    *slot = reader.u8()?;
                }
            }
            if version >= 5 {
                for slot in &mut chunk.underlay_points {
                    *slot = reader.u8()?;
                }
            }
            if version >= 6 {
                for slot in &mut chunk.height_points {
                    *slot = reader.i16()?;
                }
            }
            chunks.insert(ChunkPos { x, z }, chunk);
        }
        Ok(Self { chunks, regions, smoothing_profiles, water_planes })
    }
}

fn read_overlay_mask(reader: &mut Reader<'_>, version: u8, chunk: &mut Chunk) -> Result<(), WorldDecodeError> {
    if version >= 7 {
        for slot in &mut chunk.overlay_mask {
            *slot = reader.u8()?;
        }
        return Ok(());
    }
    for cell in 0..CELLS_PER_CHUNK_AREA {
        let mask = reader.u16()?;
        let base = cell * SUBCELLS_PER_CELL;
        let scale = SUBCELLS_PER_CELL_EDGE as usize / LEGACY_MASK_SUBCELLS_PER_CELL_EDGE;
        for legacy_z in 0..LEGACY_MASK_SUBCELLS_PER_CELL_EDGE {
            for legacy_x in 0..LEGACY_MASK_SUBCELLS_PER_CELL_EDGE {
                let bit = legacy_z * LEGACY_MASK_SUBCELLS_PER_CELL_EDGE + legacy_x;
                let coverage = if (mask >> bit) & 1 == 1 {
                    255
                } else {
                    0
                };
                for sz in legacy_z * scale..(legacy_z + 1) * scale {
                    for sx in legacy_x * scale..(legacy_x + 1) * scale {
                        chunk.overlay_mask[base + sz * SUBCELLS_PER_CELL_EDGE as usize + sx] = coverage;
                    }
                }
            }
        }
    }
    Ok(())
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
        let slice = self.bytes.get(self.pos..end).ok_or(WorldDecodeError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn checked_count(
        &self,
        count: u32,
        minimum_record_bytes: usize,
        maximum_count: usize,
    ) -> Result<usize, WorldDecodeError> {
        let count = usize::try_from(count).map_err(|_| WorldDecodeError::LimitExceeded)?;
        if count > maximum_count {
            return Err(WorldDecodeError::LimitExceeded);
        }
        let required = count.checked_mul(minimum_record_bytes).ok_or(WorldDecodeError::LimitExceeded)?;
        if required > self.remaining() {
            return Err(WorldDecodeError::Truncated);
        }
        Ok(count)
    }

    fn u8(&mut self) -> Result<u8, WorldDecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, WorldDecodeError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn i16(&mut self) -> Result<i16, WorldDecodeError> {
        let b = self.take(2)?;
        Ok(i16::from_le_bytes([b[0], b[1]]))
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
    use alloc::vec;

    use super::super::coords::CellPos;
    use super::super::fixture::cell;
    use super::super::layout::{HEIGHT_POINT_INHERIT, UNDERLAY_POINT_INHERIT};
    use super::*;

    #[test]
    fn world_bytes_roundtrip() {
        let mut world = World::new();
        world.insert_region(
            1,
            Region { name: "meadow".into(), default_material: Material::Grass, cliff_material: Material::Stone },
        );
        world.insert_region(
            2,
            Region { name: "shore".into(), default_material: Material::Sand, cliff_material: Material::Dirt },
        );
        world.insert_smoothing_profile(1, SmoothingProfile { iterations: 3, degrees: 60 });
        world.insert_water_plane(1, WaterPlane { level_octimeters: -17 });
        world.insert_water_plane(2, WaterPlane { level_octimeters: 320 });
        let mut a = Chunk::empty();
        a.underlay[0] = Material::Stone;
        a.overlay[5] = Material::Water;
        let overlay_base = 5 * SUBCELLS_PER_CELL;
        a.overlay_mask[overlay_base] = 9;
        a.overlay_mask[overlay_base + 1] = 255;
        a.height[10] = -42;
        a.region[20] = 2;
        a.water_plane[40] = 2;
        a.smoothing[30] = 1;
        // An authored underlay-point pattern (a pinned point and an explicit
        // Void hole) rides the v5 chunk record and must survive the trip.
        a.underlay_points[100] = Material::Sand.to_u8();
        a.underlay_points[101] = Material::Void.to_u8();
        world.insert_chunk(ChunkPos { x: 1, z: -3 }, a);
        let mut b = Chunk::empty();
        b.underlay[255] = Material::Dirt;
        world.insert_chunk(ChunkPos { x: -7, z: 4 }, b);

        let bytes = world.to_bytes();
        let decoded = World::from_bytes(&bytes).expect("roundtrip decodes");

        // Structural equality across the whole world.
        assert_eq!(decoded.regions, world.regions);
        assert_eq!(decoded.smoothing_profiles, world.smoothing_profiles);
        assert_eq!(decoded.water_planes, world.water_planes);
        assert_eq!(decoded.chunk(ChunkPos { x: 1, z: -3 }), world.chunk(ChunkPos { x: 1, z: -3 }));
        assert_eq!(decoded.chunk(ChunkPos { x: -7, z: 4 }), world.chunk(ChunkPos { x: -7, z: 4 }));
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
    fn from_bytes_rejects_truncated_and_bad_version() {
        assert_eq!(World::from_bytes(&[]), Err(WorldDecodeError::Truncated));
        assert_eq!(World::from_bytes(&[9]), Err(WorldDecodeError::BadVersion(9)));
        // Version + a region count claiming one region, but no region bytes.
        let mut buf = vec![WORLD_FORMAT_VERSION];
        buf.extend_from_slice(&1u32.to_le_bytes());
        assert_eq!(World::from_bytes(&buf), Err(WorldDecodeError::Truncated));

        let mut oversized_regions = vec![WORLD_FORMAT_VERSION];
        oversized_regions
            .extend_from_slice(&u32::try_from(MAX_DECODED_REGIONS + 1).expect("region cap fits u32").to_le_bytes());
        assert_eq!(World::from_bytes(&oversized_regions), Err(WorldDecodeError::LimitExceeded),);

        let mut oversized_profiles = vec![WORLD_FORMAT_VERSION];
        oversized_profiles.extend_from_slice(&0u32.to_le_bytes());
        oversized_profiles.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(World::from_bytes(&oversized_profiles), Err(WorldDecodeError::LimitExceeded),);

        let mut oversized_water = vec![WORLD_FORMAT_VERSION];
        oversized_water.extend_from_slice(&0u32.to_le_bytes());
        oversized_water.extend_from_slice(&0u32.to_le_bytes());
        oversized_water.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(World::from_bytes(&oversized_water), Err(WorldDecodeError::LimitExceeded),);

        let mut oversized_chunks = vec![WORLD_FORMAT_VERSION];
        oversized_chunks.extend_from_slice(&0u32.to_le_bytes());
        oversized_chunks.extend_from_slice(&0u32.to_le_bytes());
        oversized_chunks.extend_from_slice(&0u32.to_le_bytes());
        oversized_chunks.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(World::from_bytes(&oversized_chunks), Err(WorldDecodeError::LimitExceeded),);
    }

    #[test]
    fn pre_v3_region_decodes_stone_cliffs() {
        // A version-2 buffer's region record has no cliff byte; it must
        // decode with the Stone default. Hand-built like the v1 tripwire:
        // one region, empty profile table, no chunks.
        let mut buf = vec![2u8];
        buf.extend_from_slice(&1u32.to_le_bytes()); // one region
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(b"r");
        buf.push(Material::Grass.to_u8());
        buf.extend_from_slice(&0u32.to_le_bytes()); // no profiles
        buf.extend_from_slice(&0u32.to_le_bytes()); // no chunks
        let world = World::from_bytes(&buf).expect("a v2 buffer still decodes");
        assert_eq!(world.regions[0].cliff_material, Material::Stone);
        assert_eq!(world.regions[0].default_material, Material::Grass);
    }

    #[test]
    fn pre_v4_buffer_decodes_empty_water_table_and_zero_plane() {
        // Tripwire: a version-3 buffer carries no water-plane table and no
        // per-chunk water plane; both must read empty / zero, and a water
        // cell in it resolves at the datum-0 level. Hand-built: no regions
        // or profiles, one chunk with a water cell and the exact v3 plane
        // bytes (no water plane between height and region).
        let mut buf = vec![3u8];
        buf.extend_from_slice(&0u32.to_le_bytes()); // no regions
        buf.extend_from_slice(&0u32.to_le_bytes()); // no profiles
        buf.extend_from_slice(&1u32.to_le_bytes()); // one chunk
        buf.extend_from_slice(&0i32.to_le_bytes()); // chunk x
        buf.extend_from_slice(&0i32.to_le_bytes()); // chunk z
        let mut underlay = [0u8; CELLS_PER_CHUNK_AREA];
        underlay[0] = Material::Water.to_u8();
        buf.extend_from_slice(&underlay);
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // overlay
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // masks
        buf.extend_from_slice(&[0u8; 4 * CELLS_PER_CHUNK_AREA]); // heights
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // regions
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // smoothing

        let world = World::from_bytes(&buf).expect("a v3 buffer still decodes");
        assert!(world.water_planes.is_empty());
        let chunk = world.chunk(ChunkPos { x: 0, z: 0 }).expect("chunk");
        assert_eq!(chunk.water_plane, [0u16; CELLS_PER_CHUNK_AREA]);
        assert_eq!(world.water_level(cell(0, 0)), Some(0), "datum-0 level");
    }

    #[test]
    fn pre_v5_buffer_decodes_all_inherit_underlay_points() {
        // Tripwire: a version-4 buffer carries no per-chunk underlay-point
        // plane; it must read all-inherit, so every point resolves the cell's
        // cascade material exactly as a per-cell underlay did. Hand-built with
        // the exact v4 chunk-record layout (no underlay-point plane at the
        // tail): no regions / profiles / water planes, one chunk with a Stone
        // cell.
        let mut buf = vec![4u8];
        buf.extend_from_slice(&0u32.to_le_bytes()); // no regions
        buf.extend_from_slice(&0u32.to_le_bytes()); // no profiles
        buf.extend_from_slice(&0u32.to_le_bytes()); // no water planes
        buf.extend_from_slice(&1u32.to_le_bytes()); // one chunk
        buf.extend_from_slice(&0i32.to_le_bytes()); // chunk x
        buf.extend_from_slice(&0i32.to_le_bytes()); // chunk z
        let mut underlay = [0u8; CELLS_PER_CHUNK_AREA];
        underlay[0] = Material::Stone.to_u8();
        buf.extend_from_slice(&underlay);
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // overlay
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // masks
        buf.extend_from_slice(&[0u8; 4 * CELLS_PER_CHUNK_AREA]); // heights
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // water planes
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // regions
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // smoothing

        let world = World::from_bytes(&buf).expect("a v4 buffer still decodes");
        let chunk = world.chunk(ChunkPos { x: 0, z: 0 }).expect("chunk");
        assert!(
            chunk.underlay_points.iter().all(|point| *point == UNDERLAY_POINT_INHERIT),
            "a pre-5 buffer reads an all-inherit underlay-point plane",
        );
        assert_eq!(
            world.underlay_point(cell(0, 0), 2, 1),
            Material::Stone,
            "an inherit point resolves the cell's cascade material",
        );
    }

    #[test]
    fn pre_v6_buffer_decodes_all_zero_height_points() {
        // Tripwire: a version-5 buffer carries no per-chunk height-delta
        // plane; it must read all-zero relief, so every point resolves the
        // cell's own height exactly as a per-cell height did. Hand-built with
        // the exact v5 chunk-record layout (underlay-point plane at the tail,
        // no height plane after it): no tables, one chunk with a raised cell.
        let mut buf = vec![5u8];
        buf.extend_from_slice(&0u32.to_le_bytes()); // no regions
        buf.extend_from_slice(&0u32.to_le_bytes()); // no profiles
        buf.extend_from_slice(&0u32.to_le_bytes()); // no water planes
        buf.extend_from_slice(&1u32.to_le_bytes()); // one chunk
        buf.extend_from_slice(&0i32.to_le_bytes()); // chunk x
        buf.extend_from_slice(&0i32.to_le_bytes()); // chunk z
        let mut underlay = [0u8; CELLS_PER_CHUNK_AREA];
        underlay[0] = Material::Stone.to_u8();
        buf.extend_from_slice(&underlay);
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // overlay
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // masks
        let mut heights = [0u8; 4 * CELLS_PER_CHUNK_AREA];
        heights[0..4].copy_from_slice(&128i32.to_le_bytes()); // cell 0 height
        buf.extend_from_slice(&heights);
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // water planes
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // regions
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // smoothing
        buf.resize(buf.len() + UNDERLAY_POINTS_PER_CHUNK, UNDERLAY_POINT_INHERIT); // points

        let world = World::from_bytes(&buf).expect("a v5 buffer still decodes");
        let chunk = world.chunk(ChunkPos { x: 0, z: 0 }).expect("chunk");
        assert!(
            chunk.height_points.iter().all(|point| *point == HEIGHT_POINT_INHERIT),
            "a pre-6 buffer reads an all-zero height-delta plane",
        );
        assert_eq!(world.point_height(cell(0, 0), 2, 1), 128, "a zero-relief point resolves the cell's own height");
    }

    #[test]
    fn pre_v7_buffer_expands_overlay_bits_to_coverage_bytes() {
        // Tripwire: a version-6 buffer stores two packed mask bytes per
        // cell. Decoding expands each bit to the v7 scalar plane: set bits
        // become 255, clear bits become 0, preserving binary midpoint
        // crossings under the scalar mesher.
        let mut buf = vec![6u8];
        buf.extend_from_slice(&0u32.to_le_bytes()); // no regions
        buf.extend_from_slice(&0u32.to_le_bytes()); // no profiles
        buf.extend_from_slice(&0u32.to_le_bytes()); // no water planes
        buf.extend_from_slice(&1u32.to_le_bytes()); // one chunk
        buf.extend_from_slice(&0i32.to_le_bytes()); // chunk x
        buf.extend_from_slice(&0i32.to_le_bytes()); // chunk z
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // underlay
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // overlay
        let mut masks = [0u8; 2 * CELLS_PER_CHUNK_AREA];
        masks[0..2].copy_from_slice(&0b0000_0000_0000_0101u16.to_le_bytes());
        buf.extend_from_slice(&masks);
        buf.extend_from_slice(&[0u8; 4 * CELLS_PER_CHUNK_AREA]); // heights
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // water planes
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // regions
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // smoothing
        buf.resize(buf.len() + UNDERLAY_POINTS_PER_CHUNK, UNDERLAY_POINT_INHERIT); // points
        buf.resize(buf.len() + 2 * HEIGHT_POINTS_PER_CHUNK, 0); // height deltas

        let world = World::from_bytes(&buf).expect("a v6 buffer still decodes");
        let chunk = world.chunk(ChunkPos { x: 0, z: 0 }).expect("chunk");
        assert_eq!(chunk.overlay_mask[0], 255);
        assert_eq!(chunk.overlay_mask[3], 255, "legacy bit 0 expands across its SUB=16 block");
        assert_eq!(chunk.overlay_mask[4], 0);
        assert_eq!(chunk.overlay_mask[8], 255, "legacy bit 2 expands across its SUB=16 block");
    }

    #[test]
    fn world_bytes_roundtrip_preserves_height_deltas() {
        // An authored height-delta pattern rides the v6 chunk record and must
        // survive the trip byte-for-byte alongside the other planes.
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.height[10] = 64;
        world.insert_chunk(ChunkPos { x: 2, z: -1 }, chunk);
        world.set_cell_heights(
            CellPos { x: 32, z: -16 }, // cell (0,0) of chunk (2,-1)
            &[300, -300, 0, 127, -128],
        );

        let bytes = world.to_bytes();
        let decoded = World::from_bytes(&bytes).expect("roundtrip decodes");
        assert_eq!(
            decoded.chunk(ChunkPos { x: 2, z: -1 }),
            world.chunk(ChunkPos { x: 2, z: -1 }),
            "the height-delta plane survives the round trip",
        );
    }
}
