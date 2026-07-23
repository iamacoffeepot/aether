//! Wire kinds for the [`crate::mover::WorldMover`] body — the mail a peer
//! sends to place the controllable marker on the painted world. Core driving
//! reuses the substrate input kinds (`Key` / `MouseButton` / …), so this is
//! the mover's whole bespoke wire surface.

use serde::{Deserialize, Serialize};

const fn owns_input_by_default() -> bool {
    true
}

/// `aether.kit.mover.config` — standalone input ownership for the world mover.
/// Empty component config resolves to this type's default (`owns_input = true`).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.kit.mover.config")]
pub struct MoverConfig {
    #[serde(default = "owns_input_by_default")]
    pub owns_input: bool,
}

impl Default for MoverConfig {
    fn default() -> Self {
        Self { owns_input: true }
    }
}

/// `aether.kit.mover.teleport` — place the controlled body at the center of
/// the named cell on the world lattice. The cell address is unbounded; a
/// negative cell is valid (the world plane extends in every direction).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.mover.teleport")]
pub struct MoverTeleport {
    pub cell_x: i32,
    pub cell_z: i32,
}
