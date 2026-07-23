//! Wire vocabulary for terra selection and semantic commands.

use alloc::{string::String, vec::Vec};

use aether_data::MailboxId;
use serde::{Deserialize, Serialize};

use crate::mark::{MarkGeometry, MarkId, MarkMutationError, MarkRef};

/// Mailbox configuration for the standalone terrain-mark store.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.kit.terra.config")]
pub struct TerraConfig {
    pub mark_book_mailbox: MailboxId,
}

impl Default for TerraConfig {
    fn default() -> Self {
        Self { mark_book_mailbox: MailboxId::NONE }
    }
}

/// Replace terra's ordered selection after validating every reference.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.terra.set_selection")]
pub struct SetTerraSelection {
    pub references: Vec<MarkRef>,
}

/// Toggle one validated mark reference in the ordered selection.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.kit.terra.toggle_selection")]
pub struct ToggleTerraSelection {
    pub reference: MarkRef,
}

/// Clear the local selection without consulting the mark store.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, Default)]
#[kind(name = "aether.kit.terra.clear_selection")]
pub struct ClearTerraSelection;

/// Create one mark and select the returned reference.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.terra.create_mark")]
pub struct CreateTerraMark {
    pub geometry: MarkGeometry,
    pub label: String,
}

/// Named world-space translation in octimeters.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorldDelta {
    pub x_octimeters: i32,
    pub z_octimeters: i32,
}

/// Translate every selected mark after a complete read-only preflight.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.kit.terra.move_selection")]
pub struct MoveTerraSelection {
    pub delta: WorldDelta,
}

/// Replace the label of every selected mark that does not already match it.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.terra.relabel_selection")]
pub struct RelabelTerraSelection {
    pub label: String,
}

/// Delete every selected mark after a complete read-only preflight.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, Default)]
#[kind(name = "aether.kit.terra.delete_selection")]
pub struct DeleteTerraSelection;

/// Read the cached selection and one-flight busy flag.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, Default)]
#[kind(name = "aether.kit.terra.query")]
pub struct TerraQuery;

/// Immediate cached terra state.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.kit.terra.query_result")]
pub struct TerraQueryResult {
    pub selection: Vec<MarkRef>,
    pub busy: bool,
}

/// Structured failure for selection validation and semantic mark mutations.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum TerraError {
    Busy,
    EmptySelection,
    DuplicateSelection { id: MarkId },
    MarkBookNotConfigured,
    MarkMissing { requested: MarkRef },
    StaleReference { requested: MarkRef, current: MarkRef },
    CoordinateOverflow { reference: MarkRef },
    NoChange,
    MarkMutationRejected { requested: Option<MarkRef>, error: MarkMutationError },
    RevisionRace { expected: MarkRef, observed: MarkRef },
    MarkProtocol { reason: String },
}

/// Common reply for all terra commands.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.kit.terra.command_result")]
pub enum TerraCommandResult {
    Applied { selection: Vec<MarkRef>, changed: Vec<MarkRef>, deleted: Vec<MarkRef> },
    Rejected { selection: Vec<MarkRef>, error: TerraError },
    PartiallyApplied { selection: Vec<MarkRef>, changed: Vec<MarkRef>, deleted: Vec<MarkRef>, error: TerraError },
}
