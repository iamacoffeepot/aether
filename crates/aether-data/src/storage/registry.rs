//! Native-only collection of storage kinds, so a storage kind's schema is
//! found by its `KindId` at run time: the storage mirror of the mail-kind
//! `DescriptorEntry` inventory (issue #243).
//!
//! `#[derive(Storage)]` on a `#[kind(name = …)]` type submits one
//! [`StorageKindEntry`] per kind; a hand-written storage kind submits its row
//! by hand. The id is the kind's own `Kind::ID`,
//! [`storage_kind_id_from_name`] of its name, so the table holds only the name
//! and the schema. Wasm guests have no inventory, so none of this compiles
//! there.

use crate::hash::storage_kind_id_from_name;
use crate::ids::KindId;
use crate::schema::SchemaType;

/// One storage kind linked into this binary: its `Kind::NAME` and the schema
/// its records decode by. Every field is `'static`, so the row is
/// const-constructible inside `inventory::submit!`.
#[derive(Debug)]
pub struct StorageKindEntry {
    /// The kind's `Kind::NAME`.
    pub name: &'static str,
    /// The kind's `Schema::SCHEMA`.
    pub schema: &'static SchemaType,
}

inventory::collect!(StorageKindEntry);

/// The storage kind whose `Kind::ID` is `id`, if one is linked into this
/// binary.
#[must_use]
pub fn storage_kind(id: KindId) -> Option<&'static StorageKindEntry> {
    inventory::iter::<StorageKindEntry>().find(|entry| storage_kind_id_from_name(entry.name) == id)
}
