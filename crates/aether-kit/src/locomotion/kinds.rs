//! Wire kinds for the [`Locomotion`](super::Locomotion) actor — the mail a
//! peer sends to drive the tile-grid mover.

use serde::{Deserialize, Serialize};

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
}
