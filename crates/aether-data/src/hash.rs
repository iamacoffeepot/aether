//! FNV-1a 64-bit hashing + namespace prefixes used to construct
//! deterministic ids from names / canonical schema bytes.
//!
//! The byte-domain prefixes (`MAILBOX_DOMAIN`, `KIND_DOMAIN`,
//! `TYPE_DOMAIN`) keep id spaces statistically disjoint so a misrouted
//! `MailboxId`-into-`KindId` slot (or vice versa) hashes to a
//! different value rather than colliding silently.

use crate::ids::MailboxId;
use crate::tagged_id::{Tag, with_tag};

/// Domain tag prefixed to every mailbox-name hash so the `MailboxId`
/// space is disjoint from `Kind::ID`. Both ids are 64-bit FNV-1a
/// outputs; without a prefix the spaces overlap and a future bug that
/// feeds a mailbox id into a kind-id slot (or vice versa) would
/// misattribute silently. Prefixing makes the mis-attribution
/// statistically impossible rather than relying on positional
/// discipline at every call site.
pub const MAILBOX_DOMAIN: &[u8] = b"mailbox:";

/// Domain tag prefixed to every kind-id hash. See `MAILBOX_DOMAIN` for
/// the rationale; the derive macro and `kind_id_from_parts` both
/// prepend this before the canonical schema bytes.
pub const KIND_DOMAIN: &[u8] = b"kind:";

/// ADR-0065: domain prefix for type-id hashes. Hashed input is
/// `TYPE_DOMAIN ++ canonical_type_name.as_bytes()` (e.g.
/// `"type:aether.mailbox_id"`). Disjoint from mailbox / kind domains
/// so a typed-id `TYPE_ID` cannot alias either space.
pub const TYPE_DOMAIN: &[u8] = b"type:";

/// FNV-1a 64 over a byte slice. Retained for the few call sites that
/// hash neither a mailbox name nor a kind schema. New callers should
/// prefer `fnv1a_64_prefixed` with an explicit domain so the output
/// id space doesn't collide with an existing domain by accident.
pub const fn fnv1a_64_bytes(bytes: &[u8]) -> u64 {
    fnv1a_64_prefixed(&[], bytes)
}

/// FNV-1a 64 over `prefix ++ payload` without allocating. Equivalent
/// to `fnv1a_64_bytes(&[prefix, payload].concat())` but `const`-safe.
/// Used by `mailbox_id_from_name` (prefix `MAILBOX_DOMAIN`) and by
/// `#[derive(Kind)]` through the macro (prefix `KIND_DOMAIN`).
pub const fn fnv1a_64_prefixed(prefix: &[u8], payload: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut i = 0;
    while i < prefix.len() {
        hash ^= prefix[i] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        i += 1;
    }
    let mut i = 0;
    while i < payload.len() {
        hash ^= payload[i] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        i += 1;
    }
    hash
}

/// Compute the deterministic `MailboxId` for a mailbox name. ADR-0029
/// FNV-1a with the `MAILBOX_DOMAIN` prefix, ADR-0064 tag bits stamped
/// into the high nibble. Substrate and guest SDK compute this
/// identically so ids round-trip verbatim across the FFI without a
/// host-fn resolve.
pub const fn mailbox_id_from_name(name: &str) -> MailboxId {
    MailboxId(with_tag(
        Tag::Mailbox,
        fnv1a_64_prefixed(MAILBOX_DOMAIN, name.as_bytes()),
    ))
}
