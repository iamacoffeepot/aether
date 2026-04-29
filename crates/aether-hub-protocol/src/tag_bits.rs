//! ADR-0064 bit-layout constants for tagged opaque ids.
//!
//! Lives here rather than in `aether-mail` because
//! `aether-hub-protocol::canonical::schema::kind_id_from_parts`
//! needs to OR the kind tag in at runtime, and the dep direction is
//! `aether-mail → aether-hub-protocol`. `aether-mail::tagged_id`
//! re-exports these and layers the `Tag` enum + base32 string
//! encoding on top.
//!
//! `[tag: 4 bits | hash: 60 bits]`. The tag identifies the id space
//! (mailbox / kind / handle); the hash is the low 60 bits of an
//! FNV-1a output (mailbox, kind) or a counter (handle). `0x0` is
//! reserved as an invalid sentinel — a zero-initialised `u64` is
//! never a valid tagged id.

/// Bit-shift placing the 4-bit tag in the high nibble of a `u64`.
/// `id = (tag as u64 << TAG_SHIFT) | (hash & HASH_MASK)`.
pub const TAG_SHIFT: u32 = 60;

/// Mask isolating the 60-bit hash body inside a tagged id. Drops
/// the natural high 4 bits of the hash output before the tag bits
/// OR in.
pub const HASH_MASK: u64 = 0x0FFF_FFFF_FFFF_FFFF;

/// Tag value for mailbox ids (ADR-0029).
pub const TAG_MAILBOX: u8 = 0x1;

/// Tag value for kind ids (ADR-0030).
pub const TAG_KIND: u8 = 0x2;

/// Tag value for reply-handle ids (ADR-0045).
pub const TAG_HANDLE: u8 = 0x3;
