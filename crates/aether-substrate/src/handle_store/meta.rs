//! On-disk sidecar metadata for the persistent handle store (ADR-0049
//! §2). Each `<hash>.bin` byte blob is paired with a `<hash>.meta`
//! postcard-encoded [`HandleMeta`] describing its identity, provenance,
//! and durability flags.
//!
//! The meta is the index entry: the boot scan (issue #985) reads only
//! the meta sidecars to populate the sparse disk index without touching
//! the (potentially gigabyte-scale) `.bin` payloads. `bytes_len` is the
//! cheap consistency check against the sibling `.bin`; restore asserts
//! the lengths agree.

use serde::{Deserialize, Serialize};

/// On-disk `HandleMeta` schema version. Bumping this invalidates all
/// prior entries (the boot scan treats a version mismatch as
/// evict-on-restore) — that's the cross-substrate-version migration
/// mechanism per ADR-0049 §6.
pub const SCHEMA_VERSION: u8 = 1;

/// Postcard-encoded sidecar describing one persistent handle. Written
/// atomically next to the handle's `<hash>.bin` payload; the boot scan
/// reads it to rebuild the sparse disk index without loading the bytes.
///
/// Ids are stored as raw `u64` rather than the typed [`aether_data::HandleId`]
/// / [`aether_data::KindId`] newtypes so the on-disk format is a flat
/// postcard struct independent of the tagged-id serde branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandleMeta {
    /// On-disk format version. Set to [`SCHEMA_VERSION`] on write; a
    /// mismatch on read means a substrate-version skew and the entry is
    /// dropped.
    pub schema_version: u8,
    /// The handle id this entry was written under. Used for collision
    /// detection on restore (the path hash and the recorded id must
    /// agree).
    pub handle_id: u64,
    /// Schema-hash of the kind type at write time (ADR-0030). The
    /// schema-evolution check (issue #988) compares this against the
    /// current registry's id for the same kind name.
    pub kind_id: u64,
    /// Provenance: the transform that produced this handle, or `None`
    /// for pinned source handles (ADR-0049 §1).
    pub transform_origin: Option<TransformOrigin>,
    /// Length of the sibling `.bin` payload. Cheap consistency check on
    /// restore.
    pub bytes_len: u32,
    /// Millis since the unix epoch at write time. The eviction tick
    /// (issue #986) sorts candidates by this ascending (created_at-LRU).
    pub created_at: u64,
    /// Whether the user pinned this handle (ADR-0045 §9). Pinned entries
    /// skip byte-pressure eviction.
    pub pinned: bool,
}

/// Provenance record: which transform produced a handle and which
/// inputs went in. The substrate's local copy of the same lineage
/// ADR-0046 §7 surfaces at the application layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransformOrigin {
    /// The component mailbox that computed this handle.
    pub component_mailbox: u64,
    /// Which transform on that component.
    pub transform_index: u32,
    /// The input handle ids that produced this output, slot-ordered.
    pub input_handle_ids: Vec<u64>,
}
