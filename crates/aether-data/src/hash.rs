//! FNV-1a 64-bit hashing + namespace prefixes used to construct
//! deterministic ids from names / canonical schema bytes.
//!
//! The byte-domain prefixes (`MAILBOX_DOMAIN`, `KIND_DOMAIN`,
//! `TYPE_DOMAIN`) keep id spaces statistically disjoint so a misrouted
//! `MailboxId`-into-`KindId` slot (or vice versa) hashes to a
//! different value rather than colliding silently.

use crate::ids::{ActorId, HandleId, MailboxId, ThreadId, TransformId};
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

/// ADR-0088 §7: domain prefix for thread-name hashes. Hashed input is
/// `THREAD_DOMAIN ++ thread_name.as_bytes()` (e.g.
/// `"thread:aether-worker-0"`). See `MAILBOX_DOMAIN` for the
/// disjointness rationale; a `ThreadId` carries `Tag::Thread` bits and
/// hashes under this prefix so it can't alias the mailbox / kind / type
/// spaces. The reverse-lookup registry recovers the origin name from a
/// `ThreadId` so the dispatch hot path can store a `Copy` u64 instead
/// of allocating the thread name string per hop.
pub const THREAD_DOMAIN: &[u8] = b"thread:";

/// ADR-0048 §1: 16-byte domain prefix for native-transform id hashes.
/// Hashed input is `TRANSFORM_DOMAIN ++ "{crate}::{module}::{fn}"`. A
/// transform's identity is name-based (not position-based), so
/// inserting / reordering transforms in a file leaves every id stable;
/// renaming or moving a transform fn changes its id. Disjoint from the
/// mailbox / kind / handle domains so a `TransformId` can't alias any
/// of those spaces. The `#[transform]` macro prepends this before the
/// canonical name bytes.
pub const TRANSFORM_DOMAIN: [u8; 16] = *b"aether/xform/v1\0";

/// ADR-0048 §4: 16-byte domain prefix for content-addressed handle-id
/// derivation. Disjoint from `KIND_DOMAIN`, `MAILBOX_DOMAIN`,
/// `TRANSFORM_DOMAIN`, and `TYPE_DOMAIN` so a 64-bit collision within
/// the handle space can't cross-pollinate the other registries.
pub const HANDLE_DOMAIN: [u8; 16] = *b"aether/handle/v1";

/// ADR-0048 §4: derive the content-addressed [`HandleId`] for a native
/// transform applied to a set of input handles.
///
/// `inputs` are the input handle ids in **slot-index order** (the
/// caller resolves `Edge.slot` ascending to build the list). The id
/// keys on the global `transform_id` — a native transform is global to
/// the substrate build, not owned by a component instance — so
/// identical compute dedups engine-wide and across restarts.
///
/// Derivation (ADR-0048 §4):
///
/// ```text
/// HANDLE_DOMAIN
///   ++ transform_id.0.to_le_bytes()
///   ++ [inputs.len() as u8]
///   ++ for (slot, handle) in inputs: [slot as u8] ++ handle.0.to_le_bytes()
/// ```
///
/// The explicit `slot` byte before each input handle id protects the
/// `compose(a, b)` vs `compose(b, a)` case — swapping the two slots
/// changes the hash, so semantically different transforms get different
/// cache entries. The `inputs.len()` byte distinguishes `foo(a)` from
/// `foo(a, a)`. The result carries `Tag::Handle` bits so it lives in the
/// same id space as ephemeral handles (the executor's cache check keys
/// on the full tagged id).
///
/// `inputs.len()` is truncated to a `u8`; the ADR-0048 §1 input cap is
/// 8, so a transform never reaches the 256-input wraparound.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn content_addressed_handle_id(transform_id: TransformId, inputs: &[HandleId]) -> HandleId {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    feed(&HANDLE_DOMAIN);
    feed(&transform_id.0.to_le_bytes());
    feed(&[inputs.len() as u8]);
    for (slot, handle) in inputs.iter().enumerate() {
        feed(&[slot as u8]);
        feed(&handle.0.to_le_bytes());
    }
    HandleId(with_tag(Tag::Handle, hash))
}

/// FNV-1a 64 over a byte slice. Retained for the few call sites that
/// hash neither a mailbox name nor a kind schema. New callers should
/// prefer `fnv1a_64_prefixed` with an explicit domain so the output
/// id space doesn't collide with an existing domain by accident.
#[must_use]
pub const fn fnv1a_64_bytes(bytes: &[u8]) -> u64 {
    fnv1a_64_prefixed(&[], bytes)
}

/// Fold `bytes` into an in-progress FNV-1a 64 hash. `const`-safe so the
/// id helpers compose several byte runs (a domain prefix, scope
/// segments, the separators between them) into one hash without
/// allocating a joined buffer.
#[must_use]
const fn fnv1a_64_fold(mut hash: u64, bytes: &[u8]) -> u64 {
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
        i += 1;
    }
    hash
}

/// FNV-1a 64 over `prefix ++ payload` without allocating. Equivalent
/// to `fnv1a_64_bytes(&[prefix, payload].concat())` but `const`-safe.
/// Used by `mailbox_id_from_name` (prefix `MAILBOX_DOMAIN`) and by
/// `#[derive(Kind)]` through the macro (prefix `KIND_DOMAIN`).
#[must_use]
pub const fn fnv1a_64_prefixed(prefix: &[u8], payload: &[u8]) -> u64 {
    fnv1a_64_fold(fnv1a_64_fold(0xcbf2_9ce4_8422_2325, prefix), payload)
}

/// Compute the deterministic `MailboxId` for a mailbox name. ADR-0029
/// FNV-1a with the `MAILBOX_DOMAIN` prefix, ADR-0064 tag bits stamped
/// into the high nibble. Substrate and guest SDK compute this
/// identically so ids round-trip verbatim across the FFI without a
/// host-fn resolve.
#[must_use]
pub const fn mailbox_id_from_name(name: &str) -> MailboxId {
    MailboxId(with_tag(
        Tag::Mailbox,
        fnv1a_64_prefixed(MAILBOX_DOMAIN, name.as_bytes()),
    ))
}

/// The `:` that joins a scope/prefix to a segment in a mailbox name.
/// ADR-0079 instanced subnames (`{NAMESPACE}:{subname}`) and ADR-0098
/// scoped singletons both compose on it. Structural — forbidden inside
/// a segment — so the join reverse-parses unambiguously.
const SCOPE_SEPARATOR: u8 = b':';

/// ADR-0098: the [`MailboxId`] for a scoped mailbox name composed of
/// `prefix`, the scope separator, and `segment`. Identical to
/// `mailbox_id_from_name("{prefix}:{segment}")` but `const`-safe and
/// without allocating the joined string, so scope-relative resolution
/// stays a no-round-trip const hash. Feeds `MAILBOX_DOMAIN ++ prefix ++
/// ":" ++ segment` through the same domain-prefixed FNV-1a as
/// [`mailbox_id_from_name`] and stamps the same `Tag::Mailbox` bits, so
/// the ADR-0029 hash identity is preserved.
#[must_use]
pub const fn mailbox_id_from_name_pair(prefix: &str, segment: &str) -> MailboxId {
    let hash = fnv1a_64_prefixed(MAILBOX_DOMAIN, prefix.as_bytes());
    let hash = fnv1a_64_fold(hash, &[SCOPE_SEPARATOR]);
    let hash = fnv1a_64_fold(hash, segment.as_bytes());
    MailboxId(with_tag(Tag::Mailbox, hash))
}

/// One lineage step (ADR-0099 §3): fold a child node's [`ActorId`] onto
/// the parent's rolling `carry`. The carry is the running FNV-1a state
/// over the lineage of `ActorId`s, root to leaf; a node's [`MailboxId`] is
/// `with_tag(Mailbox, carry)`. A spawn extends the lineage in O(1) by
/// one fold step, so an actor carries its whole lineage as a single
/// `u64`. Folding a child onto its ancestors' running hash — a hash
/// chain — keeps each node recoverable, unlike a flat hash of a joined
/// path string.
///
/// The depth-1 case is the identity: a root node's carry is its own
/// `ActorId.0`, and because that value is already `Tag::Mailbox`-tagged,
/// `with_tag(Mailbox, carry) == ActorId`. So every chassis cap keeps the
/// exact id it has today; only depth-≥2 actors fold. Harness-specific
/// composition (a loaded component's `[host, aether.embedded:name]`
/// lineage) lives where the host / embedding-host class `NAMESPACE` consts
/// do, not here — this primitive is name-agnostic.
#[must_use]
pub const fn fold_lineage(parent_carry: u64, child: ActorId) -> u64 {
    fnv1a_64_fold(parent_carry, &child.0.to_le_bytes())
}

/// The [`ActorId`] of one rendered path segment (ADR-0099 §4): a bare
/// `atom` is a singleton node `hash(atom)`; an `atom:discriminator` is
/// an instanced node `hash(atom:discriminator)`. The inverse of the
/// per-segment render.
#[must_use]
fn segment_actor_id(segment: &str) -> ActorId {
    match segment.split_once(':') {
        Some((namespace, discriminator)) => ActorId::instanced(namespace, discriminator),
        None => ActorId::singleton(segment),
    }
}

/// Resolve a rendered `/`-path to its [`MailboxId`] by the ADR-0099 §4
/// parse → fold (the inverse of the display render): split on `/` into
/// per-node segments, map each to its [`ActorId`] (`segment_actor_id`),
/// and chain-fold root → leaf. A `MailboxId` is **never** the hash of a
/// joined path string — it is this fold over the path's nodes — so
/// string-addressing callers (the registry's name lookup, the MCP
/// `recipient_name` surface, the test bench) resolve a hosted / nested
/// actor through here rather than hashing the whole name. The cold path:
/// type addressing stays a const fold, and only written paths pay this
/// parse. A single-segment path (every root cap) folds to that segment's
/// `ActorId`, identical to [`mailbox_id_from_name`].
#[must_use]
pub fn mailbox_id_from_path(path: &str) -> MailboxId {
    let mut segments = path.split('/');
    // A non-empty `split` always yields at least one item; default the
    // empty-string edge case to the empty-segment ActorId.
    let mut carry = segment_actor_id(segments.next().unwrap_or("")).0;
    for segment in segments {
        carry = fold_lineage(carry, segment_actor_id(segment));
    }
    MailboxId(with_tag(Tag::Mailbox, carry))
}

/// ADR-0098: maximum number of segments in a composed mailbox path
/// (`root:a:b` is depth 3). Mailbox names are registry keys, so an
/// unbounded scope chain would let a runaway caller bloat the key
/// space; composition past this depth is rejected, not hashed.
pub const MAX_SCOPE_PATH_DEPTH: usize = 8;

/// ADR-0098: maximum total byte length of a composed mailbox path
/// (segments plus the separators between them). Generous headroom over
/// [`MAX_SCOPE_PATH_DEPTH`] full-length segments; bounds the registry
/// key size class alongside the depth cap.
pub const MAX_SCOPE_PATH_BYTES: usize = 4096;

/// Why [`validate_scope_path`] rejected a path. Each variant carries the
/// breached limit so a message can render it without re-fetching the
/// const.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopePathError {
    /// More than [`MAX_SCOPE_PATH_DEPTH`] segments.
    TooDeep { limit: usize },
    /// Composed length over [`MAX_SCOPE_PATH_BYTES`] bytes.
    TooLong { limit: usize },
}

/// ADR-0098: check that a scope path — ordered segments joined by the
/// scope separator — stays within the depth and length caps before it
/// is composed into a mailbox name. Per-segment rules (printable, no
/// separator) stay the caller's concern (ADR-0079
/// `validate_namespace_segment`); this guards the aggregate that
/// becomes a registry key.
pub const fn validate_scope_path(segments: &[&str]) -> Result<(), ScopePathError> {
    if segments.len() > MAX_SCOPE_PATH_DEPTH {
        return Err(ScopePathError::TooDeep {
            limit: MAX_SCOPE_PATH_DEPTH,
        });
    }
    let mut total: usize = 0;
    let mut i = 0;
    while i < segments.len() {
        total += segments[i].len();
        if i + 1 < segments.len() {
            total += 1;
        }
        i += 1;
    }
    if total > MAX_SCOPE_PATH_BYTES {
        return Err(ScopePathError::TooLong {
            limit: MAX_SCOPE_PATH_BYTES,
        });
    }
    Ok(())
}

/// ADR-0088 §7: compute the deterministic [`ThreadId`] for an OS thread
/// name. FNV-1a with the `THREAD_DOMAIN` prefix, ADR-0064 tag bits
/// stamped into the high nibble. Uniform with [`mailbox_id_from_name`]
/// so a thread id encodes to the `thr-XXXX-XXXX-XXXX` string form and
/// reverses through the same inventory chain. Computed once per worker
/// thread off the dispatch hot path (the value is `Copy`), so storing
/// it in a trace event costs no per-hop allocation.
#[must_use]
pub const fn thread_id_from_name(name: &str) -> ThreadId {
    ThreadId(with_tag(
        Tag::Thread,
        fnv1a_64_prefixed(THREAD_DOMAIN, name.as_bytes()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    #[test]
    fn pair_matches_joined_name() {
        // The identity scope-relative resolution leans on: composing
        // `prefix` + `:` + `segment` hashes the same as hashing the
        // already-joined name, so the const path never has to allocate.
        assert_eq!(
            mailbox_id_from_name_pair("a", "b"),
            mailbox_id_from_name("a:b"),
        );
        assert_eq!(
            mailbox_id_from_name_pair("aether.embedded", "camera"),
            mailbox_id_from_name("aether.embedded:camera"),
        );
        // Empty prefix / segment still composes consistently with the
        // joined form (the separator is always present).
        assert_eq!(
            mailbox_id_from_name_pair("", "x"),
            mailbox_id_from_name(":x")
        );
        assert_eq!(
            mailbox_id_from_name_pair("x", ""),
            mailbox_id_from_name("x:")
        );
    }

    #[test]
    fn pair_evaluates_in_const() {
        const ID: MailboxId = mailbox_id_from_name_pair("scope", "leaf");
        assert_eq!(ID, mailbox_id_from_name("scope:leaf"));
    }

    #[test]
    fn depth_one_fold_is_the_actor_id() {
        // A root cap's lineage is one node; the fold loop never runs, so
        // its MailboxId is `with_tag(Mailbox, ActorId.0)`, and since the
        // ActorId is already Mailbox-tagged that equals today's
        // name-hash id. Every chassis cap keeps its exact id — zero
        // rehash at depth 1.
        let render = ActorId::singleton("aether.render");
        assert_eq!(
            MailboxId(with_tag(Tag::Mailbox, render.0)),
            mailbox_id_from_name("aether.render"),
        );
    }

    #[test]
    fn depth_two_fold_differs_from_a_flat_name_hash() {
        // A two-node lineage folds the child's ActorId onto the root's
        // carry; the result is the hash chain, not the hash of any
        // joined string. So a hosted / nested actor rehashes off its old
        // flat name — the wire break is real at depth >= 2 (and only
        // there; depth 1 is the fixed point above). Name-agnostic: the
        // fold takes ActorIds, never literal harness namespaces.
        let root = ActorId::singleton("root");
        let child = ActorId::instanced("child", "7");
        let folded = MailboxId(with_tag(Tag::Mailbox, fold_lineage(root.0, child)));
        assert_ne!(folded, mailbox_id_from_name("root:child:7"));
        assert_ne!(folded, mailbox_id_from_name("child:7"));
    }

    #[test]
    fn fold_extends_a_carry_one_node() {
        // Folding is sequential and non-commutative: a/b differs from
        // b/a, so position is encoded without a separate depth field.
        let a = ActorId::singleton("a");
        let b = ActorId::singleton("b");
        let ab = fold_lineage(a.0, b);
        let ba = fold_lineage(b.0, a);
        assert_ne!(ab, ba);
    }

    #[test]
    fn path_resolves_to_the_chain_fold() {
        // A single-segment path is the depth-1 fixed point — identical to
        // the name hash, so every root cap resolves unchanged.
        assert_eq!(mailbox_id_from_path("root"), mailbox_id_from_name("root"));

        // A multi-segment `/`-path folds each node's ActorId root → leaf:
        // a bare atom is a singleton node, `atom:disc` an instanced one.
        // This is the inverse of the render (ADR-0099 §4).
        let s0 = ActorId::singleton("root").0;
        let s1 = fold_lineage(s0, ActorId::instanced("scope", "7"));
        let expected = MailboxId(with_tag(
            Tag::Mailbox,
            fold_lineage(s1, ActorId::singleton("leaf")),
        ));
        assert_eq!(mailbox_id_from_path("root/scope:7/leaf"), expected);

        // And it is NOT the flat hash of the joined string — names don't
        // hash to nested ids.
        assert_ne!(
            mailbox_id_from_path("root/scope:7/leaf"),
            mailbox_id_from_name("root/scope:7/leaf"),
        );
    }

    #[test]
    fn scope_path_within_caps_is_ok() {
        assert_eq!(validate_scope_path(&["a", "b", "c"]), Ok(()));
        assert_eq!(validate_scope_path(&[]), Ok(()));
    }

    #[test]
    fn scope_path_too_deep_is_rejected() {
        let deep = ["x"; MAX_SCOPE_PATH_DEPTH + 1];
        assert_eq!(
            validate_scope_path(&deep),
            Err(ScopePathError::TooDeep {
                limit: MAX_SCOPE_PATH_DEPTH,
            }),
        );
    }

    #[test]
    fn scope_path_too_long_is_rejected() {
        let big: String = "x".repeat(MAX_SCOPE_PATH_BYTES + 1);
        assert_eq!(
            validate_scope_path(&[big.as_str()]),
            Err(ScopePathError::TooLong {
                limit: MAX_SCOPE_PATH_BYTES,
            }),
        );
    }
}
