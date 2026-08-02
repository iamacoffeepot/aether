//! The world container: the sparse chunk map, the positional table
//! registrations, and the narrow seams the sibling modules mutate chunks
//! through.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use super::chunk::Chunk;
use super::coords::ChunkPos;
use super::material::Material;
use super::table::{MAX_SMOOTHING_ITERATIONS, Region, SmoothingProfile, WaterPlane};

/// The world: a sparse set of chunks plus a region table. Cells with no
/// chunk read as `Void` / `0`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct World {
    pub(super) chunks: BTreeMap<ChunkPos, Box<Chunk>>,
    pub(super) regions: Vec<Region>,
    pub(super) smoothing_profiles: Vec<SmoothingProfile>,
    pub(super) water_planes: Vec<WaterPlane>,
}

impl World {
    /// An empty world.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The chunk at `at`, if present.
    #[must_use]
    pub fn chunk(&self, at: ChunkPos) -> Option<&Chunk> {
        self.chunks.get(&at).map(Box::as_ref)
    }

    /// Insert (or replace) the chunk at `at`.
    pub fn insert_chunk(&mut self, at: ChunkPos, chunk: impl Into<Box<Chunk>>) {
        self.chunks.insert(at, chunk.into());
    }

    /// The mutable chunk at `at`, creating an empty one when absent. Shape
    /// stamps use this narrow sibling-module seam to write the overlay
    /// material and scalar coverage planes without exposing the world's
    /// chunk map as public API.
    pub(in crate::world) fn chunk_mut_or_insert(&mut self, at: ChunkPos) -> &mut Chunk {
        self.chunks.entry(at).or_insert_with(Chunk::empty_boxed).as_mut()
    }

    /// Replace a chunk entry and return its prior box, preserving absence as
    /// `None`. Proposal preview uses this narrow seam to install and restore
    /// staged boxes without exposing the sparse map.
    pub(in crate::world) fn replace_chunk(
        &mut self,
        at: ChunkPos,
        replacement: Option<Box<Chunk>>,
    ) -> Option<Box<Chunk>> {
        match replacement {
            Some(chunk) => self.chunks.insert(at, chunk),
            None => self.chunks.remove(&at),
        }
    }

    /// Implement the private mutation target through the same sparse chunk
    /// seams used by the public immediate mutations.
    pub(in crate::world) fn mutation_chunk(&self, at: ChunkPos) -> Option<&Chunk> {
        self.chunk(at)
    }

    /// Clone one present chunk directly as a box for proposal copy-on-write.
    pub(in crate::world) fn clone_chunk_box(&self, at: ChunkPos) -> Option<Box<Chunk>> {
        self.chunks.get(&at).cloned()
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
                Region { name: String::new(), default_material: Material::Void, cliff_material: Material::Stone },
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
            self.smoothing_profiles.resize(index + 1, SmoothingProfile { iterations: 0, degrees: 90 });
        }
        self.smoothing_profiles[index] = clamped;
    }

    /// Register a water plane under a 1-based `id`. The table is positional
    /// like the region table, so this grows it (padding intervening slots
    /// with the datum-0 level) and writes `plane` at index `id - 1`.
    /// `id == 0` is ignored (`0` is the "no plane" sentinel — the datum-0
    /// level).
    pub fn insert_water_plane(&mut self, id: u32, plane: WaterPlane) {
        if id == 0 {
            return;
        }
        let index = id as usize - 1;
        if index >= self.water_planes.len() {
            self.water_planes.resize(index + 1, WaterPlane { level_octimeters: 0 });
        }
        self.water_planes[index] = plane;
    }

    /// Iterate the chunk set in `ChunkPos` order (deterministic — the
    /// `BTreeMap` key order).
    pub fn chunks(&self) -> impl Iterator<Item = (ChunkPos, &Chunk)> {
        self.chunks.iter().map(|(pos, chunk)| (*pos, chunk.as_ref()))
    }
}

impl super::super::proposal::MutationTarget for World {
    fn chunk(&self, at: ChunkPos) -> Option<&Chunk> {
        self.mutation_chunk(at)
    }

    fn chunk_mut_or_insert(&mut self, at: ChunkPos) -> &mut Chunk {
        Self::chunk_mut_or_insert(self, at)
    }

    fn replace_chunk(&mut self, at: ChunkPos, chunk: Box<Chunk>) {
        Self::replace_chunk(self, at, Some(chunk));
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixture::cell;
    use super::*;

    #[test]
    fn insert_region_ignores_zero_and_grows_table() {
        let mut world = World::new();
        world.insert_region(
            0,
            Region { name: "ignored".into(), default_material: Material::Grass, cliff_material: Material::Stone },
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
            Region { name: "third".into(), default_material: Material::Stone, cliff_material: Material::Stone },
        );
        let mut chunk3 = Chunk::empty();
        chunk3.region[0] = 3;
        world.insert_chunk(ChunkPos { x: 1, z: 0 }, chunk3);
        assert_eq!(world.underlay(cell(16, 0)), Material::Stone);
    }
}
