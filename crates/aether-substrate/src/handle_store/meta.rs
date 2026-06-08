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

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// On-disk `HandleMeta` schema version. Bumping this invalidates all
/// prior entries (the boot scan treats a version mismatch as
/// evict-on-restore) — that's the cross-substrate-version migration
/// mechanism per ADR-0049 §6.
///
/// v2 (issue #988) added `kind_name` so the schema-evolution check can
/// look the kind up in the current registry by name; v1 entries (which
/// lack the field) are un-rescuable and evict on mismatch.
pub const SCHEMA_VERSION: u8 = 2;

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
    /// The kind's name at write time. Lets the schema-evolution check
    /// distinguish "kind retired" (no registry entry for the name) from
    /// "kind schema changed" (registry id differs from `kind_id`) —
    /// added in `schema_version` 2 (issue #988).
    pub kind_name: String,
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

/// On-disk format version for the `index.bin` boot fast-path snapshot
/// (ADR-0049 §3). A mismatch on read makes the boot fall through to the
/// directory scan rather than trusting a stale-format snapshot. Tracked
/// independently of [`SCHEMA_VERSION`] (the per-entry `.meta` version):
/// `index.bin` is a derived summary, so its layout can evolve without
/// touching the authoritative sidecars.
pub const INDEX_FORMAT_VERSION: u8 = 1;

/// One entry in the [`IndexSnapshot`] boot fast-path summary — a flat
/// mirror of the in-memory `DiskEntry`, keyed by raw `u64` handle id in
/// the snapshot map. Ids are raw `u64` (not the typed
/// [`aether_data::KindId`] newtype) so the on-disk format is a flat
/// postcard struct independent of the tagged-id serde branch, matching
/// [`HandleMeta`]'s convention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// Schema-hash of the kind type (ADR-0030), raw `u64`. Mirrors the
    /// `.meta` sidecar's `kind_id`.
    pub kind_id: u64,
    /// Length of the sibling `.bin` payload.
    pub bytes_len: u32,
    /// Whether the handle was pinned at snapshot time (ADR-0045 §9).
    pub pinned: bool,
    /// Millis since the unix epoch at the handle's write time.
    pub created_at: u64,
}

/// The whole `index.bin` file: a leading `schema_version` byte followed
/// by the disk-index summary (ADR-0049 §3). Written atomically on
/// graceful shutdown by snapshotting the live disk index, and loaded at
/// boot to populate the index in one read + decode instead of one
/// `open()` per `.meta` sidecar.
///
/// This is a plain postcard struct — deliberately not an aether `Kind`
/// (no derive, no registry entry, no schema-driven codec): persistence
/// is substrate-internal (ADR-0049) and the snapshot never crosses the
/// wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexSnapshot {
    /// Set to [`INDEX_FORMAT_VERSION`] on write; a mismatch on read
    /// drops the fast path back to the directory scan.
    pub schema_version: u8,
    /// The disk-index summary, keyed by raw `u64` handle id.
    pub entries: HashMap<u64, IndexEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_snapshot_postcard_round_trips() {
        let mut entries = HashMap::new();
        entries.insert(
            0x1111_2222_3333_4444,
            IndexEntry {
                kind_id: 0xAAAA,
                bytes_len: 1024,
                pinned: true,
                created_at: 42,
            },
        );
        entries.insert(
            0x5555_6666_7777_8888,
            IndexEntry {
                kind_id: 0xBBBB,
                bytes_len: 7,
                pinned: false,
                created_at: 99,
            },
        );
        let snapshot = IndexSnapshot {
            schema_version: INDEX_FORMAT_VERSION,
            entries,
        };
        let bytes = postcard::to_allocvec(&snapshot).expect("snapshot encodes");
        // The leading byte is the version header.
        assert_eq!(bytes[0], INDEX_FORMAT_VERSION);
        let decoded: IndexSnapshot = postcard::from_bytes(&bytes).expect("snapshot decodes");
        assert_eq!(decoded, snapshot);
    }
}
