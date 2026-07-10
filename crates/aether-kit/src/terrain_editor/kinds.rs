//! Wire vocabulary for terrain-editor selection and semantic commands.

use alloc::{string::String, vec::Vec};

use aether_data::MailboxId;
use serde::{Deserialize, Serialize};

use crate::mark::{MarkGeometry, MarkId, MarkMutationError, MarkRef};

/// Mailbox configuration for the standalone terrain-mark store.
#[derive(
    aether_data::Kind,
    aether_data::Schema,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
)]
#[kind(name = "aether.kit.terrain_editor.config")]
pub struct TerrainEditorConfig {
    pub mark_book_mailbox: MailboxId,
}

impl Default for TerrainEditorConfig {
    fn default() -> Self {
        Self {
            mark_book_mailbox: MailboxId::NONE,
        }
    }
}

/// Replace the editor's ordered selection after validating every reference.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.terrain_editor.set_selection")]
pub struct SetTerrainSelection {
    pub references: Vec<MarkRef>,
}

/// Toggle one validated mark reference in the ordered selection.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.kit.terrain_editor.toggle_selection")]
pub struct ToggleTerrainSelection {
    pub reference: MarkRef,
}

/// Clear the local selection without consulting the mark store.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, Default,
)]
#[kind(name = "aether.kit.terrain_editor.clear_selection")]
pub struct ClearTerrainSelection;

/// Create one mark and select the returned reference.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.terrain_editor.create_mark")]
pub struct CreateTerrainMark {
    pub geometry: MarkGeometry,
    pub label: String,
}

/// Named world-space translation in octimeters.
#[derive(
    aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default,
)]
pub struct WorldDelta {
    pub x_octimeters: i32,
    pub z_octimeters: i32,
}

/// Translate every selected mark after a complete read-only preflight.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.kit.terrain_editor.move_selection")]
pub struct MoveTerrainSelection {
    pub delta: WorldDelta,
}

/// Replace the label of every selected mark that does not already match it.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.terrain_editor.relabel_selection")]
pub struct RelabelTerrainSelection {
    pub label: String,
}

/// Delete every selected mark after a complete read-only preflight.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, Default,
)]
#[kind(name = "aether.kit.terrain_editor.delete_selection")]
pub struct DeleteTerrainSelection;

/// Read the cached selection and one-flight busy flag.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, Default,
)]
#[kind(name = "aether.kit.terrain_editor.query")]
pub struct TerrainEditorQuery;

/// Immediate cached editor state.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "aether.kit.terrain_editor.query_result")]
pub struct TerrainEditorQueryResult {
    pub selection: Vec<MarkRef>,
    pub busy: bool,
}

/// Structured failure for selection validation and semantic mark mutations.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum TerrainEditorError {
    Busy,
    EmptySelection,
    DuplicateSelection {
        id: MarkId,
    },
    MarkBookNotConfigured,
    MarkMissing {
        requested: MarkRef,
    },
    StaleReference {
        requested: MarkRef,
        current: MarkRef,
    },
    CoordinateOverflow {
        reference: MarkRef,
    },
    NoChange,
    MarkMutationRejected {
        requested: Option<MarkRef>,
        error: MarkMutationError,
    },
    RevisionRace {
        expected: MarkRef,
        observed: MarkRef,
    },
    MarkProtocol {
        reason: String,
    },
}

/// Common reply for all terrain-editor commands.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "aether.kit.terrain_editor.command_result")]
pub enum TerrainCommandResult {
    Applied {
        selection: Vec<MarkRef>,
        changed: Vec<MarkRef>,
        deleted: Vec<MarkRef>,
    },
    Rejected {
        selection: Vec<MarkRef>,
        error: TerrainEditorError,
    },
    PartiallyApplied {
        selection: Vec<MarkRef>,
        changed: Vec<MarkRef>,
        deleted: Vec<MarkRef>,
        error: TerrainEditorError,
    },
}
