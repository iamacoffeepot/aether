//! Typed initialization surface for the player presentation client.

use alloc::string::String;

use aether_capabilities::game::{CellPosition, GridBounds};
use serde::{Deserialize, Serialize};

/// `aether.kit.client.config` — outbound server and toy-world presentation
/// configuration for [`PlayerClient`](crate::client::PlayerClient).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.kit.client.config")]
#[serde(default)]
pub struct PlayerClientConfig {
    /// Socket address passed directly to `aether.tcp.connect`.
    pub server_addr: String,
    /// Self-identification included in the initial player `Hello`.
    pub client_name: String,
    /// Requested spawn cell. The server replaces the accompanying entity id
    /// with the identity assigned to this connection.
    pub spawn_cell: CellPosition,
    /// Inclusive lattice rendered behind authoritative entity markers.
    pub grid_bounds: GridBounds,
}

impl Default for PlayerClientConfig {
    fn default() -> Self {
        Self {
            server_addr: String::from("127.0.0.1:7777"),
            client_name: String::from("player"),
            spawn_cell: CellPosition { cell_x: 0, cell_z: 0 },
            grid_bounds: GridBounds::default(),
        }
    }
}
