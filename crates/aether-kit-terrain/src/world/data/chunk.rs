//! One `16 × 16` block of the world as a struct-of-arrays — the property
//! planes themselves, and the empty chunk every sparse read falls back to.

use alloc::boxed::Box;
use core::ptr;

use super::layout::{
    CELLS_PER_CHUNK_AREA, HEIGHT_POINTS_PER_CHUNK, OVERLAY_MASK_WIRE_BYTES, UNDERLAY_POINT_INHERIT,
    UNDERLAY_POINTS_PER_CHUNK,
};
use super::material::Material;

/// One `16 × 16` block of the world, as a struct-of-arrays: property
/// planes, each row-major (`z * 16 + x`).
#[allow(clippy::large_stack_frames)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// Ground fabric — cascade-resolved by [`World::underlay`](crate::world::World::underlay).
    pub underlay: [Material; CELLS_PER_CHUNK_AREA],
    /// Per-subcell underlay material points — `SUB × SUB` points per cell
    /// (row-major cell order, `z*SUB + x` within a cell). Each byte is a
    /// [`Material`] or the [`UNDERLAY_POINT_INHERIT`] sentinel (inherit the
    /// cell's cascade). All-inherit — the empty default — meshes exactly as
    /// the per-cell [`Chunk::underlay`]; an explicit point shapes the ground
    /// below cell scale ([`World::underlay_point`](crate::world::World::underlay_point)).
    pub underlay_points: [u8; UNDERLAY_POINTS_PER_CHUNK],
    /// Per-subcell height deltas in octimeters — `SUB × SUB` points per cell
    /// (same layout as [`Chunk::underlay_points`]). Each `i16` offsets its
    /// subcell off the cell's [`Chunk::height`] ([`World::point_height`](crate::world::World::point_height));
    /// [`HEIGHT_POINT_INHERIT`](crate::world::HEIGHT_POINT_INHERIT) (`0`) is no relief. An all-zero plane — the
    /// empty default — resolves exactly at cell stride, so a flat or legacy
    /// world's surface and mesh are byte-identical to the per-cell height;
    /// an authored delta shapes standable relief below cell scale (a fused
    /// column, a terrace, a ledge).
    pub height_points: [i16; HEIGHT_POINTS_PER_CHUNK],
    /// Placed surface — raw, never cascade-resolved. `Void` = none.
    pub overlay: [Material; CELLS_PER_CHUNK_AREA],
    /// Overlay subcell coverage bytes — `SUB × SUB` samples per cell
    /// (row-major cell order, `z*SUB + x` within a cell). `255` is full
    /// coverage; `0` is none. Meaningless where `overlay` is `Void`.
    pub overlay_mask: [u8; OVERLAY_MASK_WIRE_BYTES],
    /// Elevation in octimeters (`0` = flat).
    pub height: [i32; CELLS_PER_CHUNK_AREA],
    /// Region id per cell (`0` = no region).
    pub region: [u16; CELLS_PER_CHUNK_AREA],
    /// Water-plane id per cell (`0` = none — the datum-0 level). Meaningful
    /// only where the cascade-resolved underlay is [`Material::Water`];
    /// selects the row of [`World`](crate::world::World)'s water-plane table whose level the
    /// cell's water surface lies at.
    pub water_plane: [u16; CELLS_PER_CHUNK_AREA],
    /// Smoothing-profile id per cell (`0` = no override — the material's
    /// own smoothing applies).
    pub smoothing: [u8; CELLS_PER_CHUNK_AREA],
}

impl Chunk {
    /// An empty chunk — all planes `Void` / zero.
    #[must_use]
    #[allow(clippy::large_stack_frames)]
    pub fn empty() -> Self {
        *Self::empty_boxed()
    }

    /// An empty chunk allocated at its final address. The dense subcell
    /// planes are large at `SUB = 16`, so decode and sparse insertion paths
    /// use this form instead of building the chunk by value on a guest stack.
    #[must_use]
    pub fn empty_boxed() -> Box<Self> {
        let mut chunk = Box::<Self>::new_uninit();
        let ptr = chunk.as_mut_ptr();
        // SAFETY: every field is initialized exactly once before
        // `assume_init`, and no read happens until the fully initialized box
        // is returned.
        unsafe {
            ptr::addr_of_mut!((*ptr).underlay).write([Material::Void; CELLS_PER_CHUNK_AREA]);
            ptr::addr_of_mut!((*ptr).underlay_points)
                .cast::<u8>()
                .write_bytes(UNDERLAY_POINT_INHERIT, UNDERLAY_POINTS_PER_CHUNK);
            ptr::addr_of_mut!((*ptr).height_points).cast::<i16>().write_bytes(0, HEIGHT_POINTS_PER_CHUNK);
            ptr::addr_of_mut!((*ptr).overlay).write([Material::Void; CELLS_PER_CHUNK_AREA]);
            ptr::addr_of_mut!((*ptr).overlay_mask).cast::<u8>().write_bytes(0, OVERLAY_MASK_WIRE_BYTES);
            ptr::addr_of_mut!((*ptr).height).write([0; CELLS_PER_CHUNK_AREA]);
            ptr::addr_of_mut!((*ptr).region).write([0; CELLS_PER_CHUNK_AREA]);
            ptr::addr_of_mut!((*ptr).water_plane).write([0; CELLS_PER_CHUNK_AREA]);
            ptr::addr_of_mut!((*ptr).smoothing).write([0; CELLS_PER_CHUNK_AREA]);
            chunk.assume_init()
        }
    }
}

impl Default for Chunk {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::super::coords::ChunkPos;
    use super::super::layout::{HEIGHT_POINT_INHERIT, SUBCELLS_PER_CELL};
    use super::*;
    use crate::world::SetChunk;

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
            underlay_points: Vec::new(),
            height_points: Vec::new(),
            overlay: vec![0u8; CELLS_PER_CHUNK_AREA],
            overlay_mask: vec![0u8; OVERLAY_MASK_WIRE_BYTES],
            height: vec![0i32; CELLS_PER_CHUNK_AREA],
            region,
            water_plane: vec![0u32; CELLS_PER_CHUNK_AREA],
            smoothing: vec![0u8; CELLS_PER_CHUNK_AREA],
        };
        assert_eq!(set.chunk_pos(), ChunkPos { x: 2, z: -1 });
        let chunk = set.into_chunk();
        assert_eq!(chunk.underlay[3 * 16 + 2], Material::Water);
        assert_eq!(chunk.underlay[0], Material::Void, "unknown byte clamps to Void");
        assert_eq!(chunk.region[1], 7);
    }

    #[test]
    fn set_chunk_copies_overlay_coverage_bytes() {
        // Dense coverage plane; cell 1's first two subcells get direct
        // scalar coverage bytes.
        let mut overlay_mask = vec![0u8; OVERLAY_MASK_WIRE_BYTES];
        overlay_mask[SUBCELLS_PER_CELL] = 17;
        overlay_mask[SUBCELLS_PER_CELL + 1] = 239;
        let set = SetChunk {
            chunk_x: 0,
            chunk_z: 0,
            underlay: Vec::new(),
            underlay_points: Vec::new(),
            height_points: Vec::new(),
            overlay: Vec::new(),
            overlay_mask,
            height: Vec::new(),
            region: Vec::new(),
            water_plane: Vec::new(),
            smoothing: Vec::new(),
        };
        let chunk = set.into_chunk();
        assert_eq!(chunk.overlay_mask[0], 0);
        assert_eq!(chunk.overlay_mask[SUBCELLS_PER_CELL], 17);
        assert_eq!(chunk.overlay_mask[SUBCELLS_PER_CELL + 1], 239);
        assert_eq!(OVERLAY_MASK_WIRE_BYTES, 65_536, "SUB=16 -> 256*256 bytes");
    }

    #[test]
    fn set_chunk_pads_short_planes_and_truncates_long() {
        let set = SetChunk {
            chunk_x: 0,
            chunk_z: 0,
            underlay: vec![Material::Grass.to_u8(); 2], // short → rest Void
            underlay_points: vec![Material::Stone.to_u8(); 2], // short → rest inherit
            height_points: vec![10i16; 2],              // short → rest zero relief
            overlay: vec![0u8; CELLS_PER_CHUNK_AREA + 10], // long → truncated
            overlay_mask: Vec::new(),
            height: vec![5i32; 1],
            region: Vec::new(),
            water_plane: Vec::new(),
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
        // The two written points hold; the tail keeps the inherit sentinel.
        assert_eq!(chunk.underlay_points[0], Material::Stone.to_u8());
        assert_eq!(chunk.underlay_points[1], Material::Stone.to_u8());
        assert_eq!(chunk.underlay_points[2], UNDERLAY_POINT_INHERIT);
        // The two written height deltas hold; the tail keeps zero relief.
        assert_eq!(chunk.height_points[0], 10);
        assert_eq!(chunk.height_points[1], 10);
        assert_eq!(chunk.height_points[2], HEIGHT_POINT_INHERIT);
    }
}
