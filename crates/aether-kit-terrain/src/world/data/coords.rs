//! Cell and chunk addresses, and the shifts between them and octimeter
//! space. Cells are addresses; what a cell *is* lives in the plane stack.

use serde::{Deserialize, Serialize};

use super::layout::{CELLS_PER_CHUNK, CHUNK_BITS, OCTIMETER_BITS, OCTIMETERS_PER_CELL};

/// A cell address on the world lattice. Cells are addresses; their
/// properties live in the plane stack.
#[derive(aether_data::Schema, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
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
        ChunkPos { x: self.x >> CHUNK_BITS, z: self.z >> CHUNK_BITS }
    }

    /// The cell's center in octimeters — cell-center-anchored, so a
    /// mover placed here sits in the middle of the cell, not on its
    /// corner. `(x << 8) + 128`.
    #[must_use]
    pub fn center_octimeters(self) -> (i32, i32) {
        ((self.x << OCTIMETER_BITS) + OCTIMETERS_PER_CELL / 2, (self.z << OCTIMETER_BITS) + OCTIMETERS_PER_CELL / 2)
    }

    /// The cell an octimeter position sits in. Arithmetic right shift —
    /// negative positions floor.
    #[must_use]
    pub fn from_octimeters(x: i32, z: i32) -> Self {
        Self { x: x >> OCTIMETER_BITS, z: z >> OCTIMETER_BITS }
    }

    /// Index of this cell within its chunk's row-major planes.
    /// `rem_euclid` so negative cells map into `0..256` correctly.
    pub(in crate::world) fn chunk_index(self) -> usize {
        (self.z.rem_euclid(CELLS_PER_CHUNK) * CELLS_PER_CHUNK + self.x.rem_euclid(CELLS_PER_CHUNK)) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixture::cell;
    use super::*;

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
}
