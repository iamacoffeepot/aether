//! `aether-kit` — the gameplay-systems layer.
//!
//! Reusable game-building actors that run on the substrate. This crate
//! hosts both the trunk types (the mail shapes peers send a system) at
//! the root and the runtime actors in [`runtime`]. The systems are
//! [`runtime::Locomotion`] (tile-grid movement on a fixed-point ground
//! plane — the module entry) and [`runtime::camera::CameraComponent`]
//! (the multi-camera driver, selected by the `aether_kit@aether.camera`
//! export, ADR-0096). The camera's `aether.camera.*` driver kinds live
//! in [`camera`].
//!
//! # Units
//!
//! Positions are fixed-point integers, so the simulation is bit-exact
//! across machines — the precondition for server authority and
//! deterministic replay. The ground plane is the world XZ plane (Y up);
//! one tile is one real-world meter, subdivided into 256 **octimeters**
//! (the minimum movement quantum, ≈ 3.9 mm).
//!
//! - [`OCTIMETERS_PER_TILE`] = 256 — `1 tile = 1 m = 256 octimeters`.
//! - The **coarse tile** an octimeter position sits on is `pos >>`
//!   [`TILE_BITS`] — a shift, never a divide, because the subdivision is
//!   a power of two. The coarse tile is the unit for occupancy and
//!   blocking; octimeters are the unit for smooth movement.

extern crate alloc;

use serde::{Deserialize, Serialize};

pub mod camera;

#[cfg(feature = "runtime")]
pub mod runtime;

#[cfg(feature = "runtime")]
mod arena;

/// Octimeters per tile: `1 tile = 1 meter = 256 octimeters`.
pub const OCTIMETERS_PER_TILE: i32 = 256;

/// Right-shift an octimeter coordinate by this to derive its coarse
/// tile (`2^8 = 256` octimeters per tile).
pub const TILE_BITS: u32 = 8;

/// `aether.kit.locomotion.teleport` — place the controlled mover at the
/// center of the named tile. Ignored (warn-log) if the tile is outside
/// the map.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.locomotion.teleport")]
pub struct Teleport {
    pub tile_x: i32,
    pub tile_z: i32,
}

/// `aether.kit.locomotion.set_walkable` — toggle whether a tile blocks
/// movement. Out-of-map tiles are ignored (warn-log).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.locomotion.set_walkable")]
pub struct SetWalkable {
    pub tile_x: i32,
    pub tile_z: i32,
    pub walkable: bool,
}

/// `aether.kit.locomotion.set_granularity` — set the movement-cell size
/// in octimeters: the grid the mover snaps to. `256` (a full tile) is
/// classic tile-to-tile movement; smaller values let it stop on sub-tiles;
/// `8` is effectively continuous. Clamped to `8..=256`. The `Tab` key
/// cycles preset sizes live.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.locomotion.set_granularity")]
pub struct SetGranularity {
    pub cell_octimeters: i32,
}

/// `aether.kit.locomotion.preview` — a design aid, not part of play. Freezes
/// the live hazard game and paints a top-down contact-sheet of one shape's
/// parameter variations (a 3×3 matrix: thickness down the rows, the shape's
/// spatial parameter across the columns) so the look of each parameter can be
/// compared at a glance. `shape` selects which: `0` resumes the game, `1` ring,
/// `2` wall, `3` wave.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.locomotion.preview")]
pub struct Preview {
    pub shape: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::Kind;

    #[test]
    fn kind_names_are_stable() {
        assert_eq!(Teleport::NAME, "aether.kit.locomotion.teleport");
        assert_eq!(SetWalkable::NAME, "aether.kit.locomotion.set_walkable");
        assert_eq!(
            SetGranularity::NAME,
            "aether.kit.locomotion.set_granularity"
        );
    }

    #[test]
    fn coarse_tile_is_a_shift() {
        // A position 1.5 tiles along sits on coarse tile 1.
        let pos = OCTIMETERS_PER_TILE + OCTIMETERS_PER_TILE / 2;
        assert_eq!(pos >> TILE_BITS, 1);
    }
}
