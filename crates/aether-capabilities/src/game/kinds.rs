//! Tick-native intent and fact vocabulary for an authoritative turn simulation.

use alloc::vec::Vec;

use aether_data::MailboxId;
use serde::{Deserialize, Serialize};

/// A cell on the authoritative simulation lattice.
///
/// Axes and units stay named on the wire so a consumer never has to infer
/// tuple order or whether a coordinate is measured in cells or octimeters.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CellPosition {
    pub cell_x: i32,
    pub cell_z: i32,
}

/// Inclusive bounds of the toy simulation grid.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridBounds {
    pub min_cell_x: i32,
    pub max_cell_x: i32,
    pub min_cell_z: i32,
    pub max_cell_z: i32,
}

impl Default for GridBounds {
    fn default() -> Self {
        Self { min_cell_x: -4, max_cell_x: 4, min_cell_z: -4, max_cell_z: 4 }
    }
}

/// `aether.sim.config` — init-time wiring and bounded-retention policy.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.sim.config")]
#[serde(default)]
pub struct SimConfig {
    /// Optional live fact consumer. Every completed bundle is pushed here.
    pub fact_sink: Option<MailboxId>,
    /// Maximum number of complete tick bundles retained for polling. The
    /// actor clamps this to the supported range `1..=1024`.
    pub ring_depth: u32,
    /// Inclusive authoritative movement and spawn bounds.
    pub grid_bounds: GridBounds,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self { fact_sink: None, ring_depth: 64, grid_bounds: GridBounds::default() }
    }
}

/// `aether.sim.spawn` — request creation of one entity at a named cell.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.sim.spawn")]
pub struct Spawn {
    pub entity_id: u64,
    pub cell_x: i32,
    pub cell_z: i32,
}

/// Cardinal movement on the XZ cell lattice.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveDirection {
    North,
    East,
    South,
    West,
}

/// `aether.sim.move_intent` — request one adjacent-cell move.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.sim.move_intent")]
pub struct MoveIntent {
    pub entity_id: u64,
    pub direction: MoveDirection,
}

/// One entity in an authoritative state summary.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityState {
    pub entity_id: u64,
    pub cell_x: i32,
    pub cell_z: i32,
}

/// The granular state transition represented by a trajectory event.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrajectoryKind {
    Spawned,
    Moved { from: CellPosition, to: CellPosition },
    Removed,
}

/// `aether.sim.trajectory_event` — one entity delta from an atomic turn.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.sim.trajectory_event")]
pub struct TrajectoryEvent {
    pub tick: u64,
    pub entity_id: u64,
    pub kind: TrajectoryKind,
}

/// `aether.sim.state_summary` — authoritative post-turn entity positions.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.sim.state_summary")]
pub struct StateSummary {
    pub tick: u64,
    pub entities: Vec<EntityState>,
}

/// `aether.sim.tick_bundle` — the atomic facts produced by one turn.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.sim.tick_bundle")]
pub struct TickBundle {
    pub tick: u64,
    pub superseded_through: u64,
    pub trajectory: Vec<TrajectoryEvent>,
    pub summary: StateSummary,
}

/// `aether.sim.poll` — request facts strictly newer than `since_tick`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.sim.poll")]
pub struct Poll {
    pub since_tick: u64,
}

/// `aether.sim.poll_result` — retained catch-up facts and the live watermark.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.sim.poll_result")]
pub struct PollResult {
    pub bundles: Vec<TickBundle>,
    pub current_tick: u64,
}
