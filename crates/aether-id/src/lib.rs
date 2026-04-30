//! Typed-id leaf crate: ADR-0064 / ADR-0065 newtypes + ADR-0029
//! mailbox-name hashing + tag-bit layout. Sits below both
//! `aether-mail` and `aether-hub-protocol` so the wire-protocol
//! crate can type its mailbox/kind/handle fields without the
//! `aether-mail → aether-hub-protocol` dep cycle. `aether-mail`
//! still owns the `Schema` and `CastEligible` trait impls for these
//! types (orphan rules — those traits live in `aether-mail`).
//!
//! The newtypes are `#[repr(transparent)]` over `u64`, so the
//! postcard binary wire is byte-identical to a raw `u64` field.
//! `Serialize` / `Deserialize` branch on `is_human_readable`: JSON
//! gets the ADR-0064 tagged-string form (`mbx-XXXX-XXXX-XXXX`),
//! binary gets the raw varint.

#![no_std]

extern crate alloc;

pub mod hash;
pub mod ids;
pub mod tag_bits;
pub mod tagged_id;

pub use hash::{
    KIND_DOMAIN, MAILBOX_DOMAIN, TYPE_DOMAIN, fnv1a_64_bytes, fnv1a_64_prefixed,
    mailbox_id_from_name,
};
pub use ids::{HandleId, KindId, MailboxId, tag_for_type_id, type_name_for_type_id};
pub use tagged_id::{Tag, with_tag};
