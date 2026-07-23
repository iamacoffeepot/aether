//! Wire vocabulary for the revisioned terrain-mark store.

use alloc::{string::String, vec::Vec};

use serde::{Deserialize, Serialize};

use crate::world::WorldPoint;

/// Stable identity for a terrain mark.
///
/// The scalar stays wrapped so a mark id cannot be confused with a revision
/// or an unrelated counter at API boundaries.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MarkId(u32);

impl MarkId {
    /// Wrap a scalar mark id.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Return the wrapped scalar id.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A stable mark identity paired with the revision observed by the caller.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MarkRef {
    pub id: MarkId,
    pub revision: u32,
}

/// Geometry attached to a terrain mark, expressed in named world points.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum MarkGeometry {
    Point(WorldPoint),
    Path(Vec<WorldPoint>),
    Area(Vec<WorldPoint>),
}

/// One stored terrain annotation.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub id: MarkId,
    pub revision: u32,
    pub geometry: MarkGeometry,
    pub label: String,
}

impl Mark {
    /// Return the mark's identity and current revision as one named value.
    #[must_use]
    pub const fn reference(&self) -> MarkRef {
        MarkRef { id: self.id, revision: self.revision }
    }
}

/// A rejected mutation leaves the store unchanged.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum MarkMutationError {
    InvalidGeometry { reason: String },
    EmptyUpdate,
    IdExhausted,
    RevisionExhausted,
}

/// `aether.kit.mark.create` — allocate and store one terrain mark.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.mark.create")]
pub struct MarkCreate {
    pub geometry: MarkGeometry,
    pub label: String,
}

/// Reply to [`MarkCreate`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.kit.mark.create_result")]
pub enum MarkCreateResult {
    Created { reference: MarkRef },
    Rejected { error: MarkMutationError },
}

/// `aether.kit.mark.update` — replace either or both mutable mark fields.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.mark.update")]
pub struct MarkUpdate {
    pub id: MarkId,
    pub geometry: Option<MarkGeometry>,
    pub label: Option<String>,
}

/// Reply to [`MarkUpdate`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.kit.mark.update_result")]
pub enum MarkUpdateResult {
    Updated { reference: MarkRef },
    NotFound { id: MarkId },
    Rejected { error: MarkMutationError },
}

/// `aether.kit.mark.delete` — remove one mark without reusing its id.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.mark.delete")]
pub struct MarkDelete {
    pub id: MarkId,
}

/// Reply to [`MarkDelete`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.kit.mark.delete_result")]
pub enum MarkDeleteResult {
    Deleted { reference: MarkRef },
    NotFound { id: MarkId },
}

/// `aether.kit.mark.get` — fetch one mark by stable identity.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.mark.get")]
pub struct MarkGet {
    pub id: MarkId,
}

/// Reply to [`MarkGet`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.kit.mark.get_result")]
pub struct MarkGetResult {
    pub mark: Option<Mark>,
}

/// `aether.kit.mark.list` — fetch every mark in ascending id order.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, Default)]
#[kind(name = "aether.kit.mark.list")]
pub struct MarkList;

/// Reply to [`MarkList`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.kit.mark.list_result")]
pub struct MarkListResult {
    pub marks: Vec<Mark>,
}

/// Hot-swap snapshot for [`super::MarkBook`](super::MarkBook).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.kit.mark.saved_state")]
pub struct SavedMarks {
    pub marks: Vec<Mark>,
    pub next_id: u32,
}
