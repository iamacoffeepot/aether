//! Actor SDK primitive: the marker trait surface (here in `mod.rs`)
//! plus per-mail / per-init / per-drop ctx machinery
//! ([`ctx`]), the `Sender` / `MailCtx` traits
//! ([`sender`]), and the `Slot` single-instance
//! backing store ([`slot`]). Marker traits are
//! pure compile-time markers — no transport machinery, no lifecycle
//! methods, just identity (`Actor`), singleton-ness (`Singleton`),
//! and per-handler-kind gating (`HandlesKind`).
//!
//! Pre-PR-C of issue 533 these lived here. Issue 533's facade pattern
//! (ADR-0075) put chassis cap structs in `aether-kinds`, which meant
//! both `aether-kinds` and `aether-actor` needed to reference the
//! markers — but `aether-actor` already depended on `aether-kinds` (for
//! `aether.input.subscribe`), so a forward dep would cycle.
//! PR C broke the cycle by moving the markers down to `aether-data`
//! (the universal data layer both crates depend on); marked stopgap.
//!
//! PR E1 of issue 545 collapsed the facade pattern back out of
//! `aether-kinds` — caps now live entirely in `aether-substrate`. The
//! cycle that forced the down-move evaporated, and PR E4 (this PR)
//! restores the markers to their natural home alongside the rest of
//! the actor SDK.

pub mod ctx;
pub mod sender;
pub mod slot;

use aether_data::{ActorId, Kind, MailboxId, Tag, fold_lineage, with_tag};

/// The symmetric trait every actor implements: the recipient name it
/// claims. Lifecycle methods (`boot` for native chassis caps, `init`
/// for wasm components) live on per-transport subtraits; this trait
/// stays ctx-free so the same shape applies to both sides.
///
/// Dispatch invariant: every actor drains cooperatively on the chassis
/// worker pool. A handler must never block the dispatcher — offload
/// blocking work (sync disk I/O, a runloop on a non-mail external
/// source like TCP `accept` or a file-watch source) to a `ctx.spawn`'d
/// thread that blocks off-pool and feeds its results back as mail. A
/// request/reply-shaped need is served by an FSM that carries state
/// across handler invocations (send → return → handle the reply) rather
/// than by parking a pool worker in-handler.
pub trait Actor: Sized + Send + 'static {
    /// The recipient name this actor claims **within its scope**
    /// (ADR-0098). For a root-scoped actor — every chassis capability —
    /// it is the full mailbox name (`aether.<name>`). For an actor
    /// hosted inside a parent the full mailbox name is the path
    /// `"{scope}:{NAMESPACE}"`, so `NAMESPACE` is just the segment: a
    /// wasm component declaring `NAMESPACE = "camera"` and loaded at its
    /// default name registers at `aether.embedded:camera`
    /// under its component-host, not at the bare `"camera"`.
    const NAMESPACE: &'static str;
}

/// Cardinality marker: exactly one instance of this actor is live **per
/// scope** (ADR-0098). A scope is either the substrate root or a parent
/// instance, and `R::NAMESPACE` is this actor's segment within it — so
/// the full mailbox name is `R::NAMESPACE` at the root, or the path
/// `"{scope}:{R::NAMESPACE}"` when hosted inside a parent. The substrate
/// enforces "at most one live mailbox per full name" at registration
/// (ADR-0079); because the scope is part of the name, that is exactly
/// "one of this actor per scope".
///
/// Root-scoped singletons — every chassis cap, including catch-alls like
/// `BroadcastCapability` — have full name `== NAMESPACE`, so a sender
/// type-addresses them with `ctx.actor::<R>()`. A singleton hosted
/// inside a parent (a wasm component under its component-host, a
/// per-session actor under a listener) is reached **scope-relative**,
/// not through the bare `NAMESPACE`: by its resolved name via
/// `ctx.resolve_actor::<R>(name)`, or — for a loaded component — the
/// component-host's `loaded::<R>(name)` helper, which composes the
/// component-host scope onto the name (`LoadResult.name` is the full
/// address). Multi-instance loads use the same name-keyed path.
///
/// Mutually exclusive with [`Instanced`] at the type level: an actor is
/// either one-of-a-kind within a scope (singleton) or N-instances under
/// a shared prefix (instanced, name-keyed). ADR-0079.
pub trait Singleton: Actor {
    /// This actor's [`MailboxId`] as seen by a caller whose lineage carry
    /// is `caller_carry` (ADR-0099 §5). `ctx.actor::<R>()` calls
    /// `R::resolve(self_carry)` and addresses the result.
    ///
    /// The default is the static, root-pinned resolution: the depth-1
    /// fixed point (§3), this actor's own [`ActorId`] tagged as a mailbox,
    /// **ignoring the caller's carry** because a root cap sits at the root,
    /// not below the caller. It equals today's `mailbox_id_from_name(NAMESPACE)`
    /// because [`with_tag`] is idempotent on an already-`Mailbox`-tagged
    /// value — so every chassis cap keeps the exact id it has today.
    ///
    /// A non-root actor overrides this. A direct child folds the caller's
    /// carry — `with_tag(Mailbox, fold_lineage(caller_carry, ActorId::singleton(NAMESPACE)))`
    /// — and an embeddable component delegates to its embedding-host class
    /// ([`EmbeddedHost`], iamacoffeepot/aether#1432). The override is the only
    /// carrier of the §2 position fact: a root-pinned actor keeps the default,
    /// and nothing else about position is held at the type level.
    #[must_use]
    fn resolve(_caller_carry: u64) -> MailboxId {
        MailboxId(with_tag(
            Tag::Mailbox,
            ActorId::singleton(Self::NAMESPACE).0,
        ))
    }
}

/// Cardinality marker: many instances of this actor type can be live
/// per substrate, each under its own subname. `R::NAMESPACE` is a
/// **prefix** — full mailbox names take the form
/// `"{NAMESPACE}:{subname}"` (e.g. `aether.net.session:42`). The `:`
/// separator is structural; subnames may not contain it.
///
/// Forcing function is socket actors (ADR-0079): a singleton listener
/// (e.g. `NetCapability`) accepts connections and spawns one
/// `SessionActor` per accepted socket via `ctx.spawn_child`. Senders
/// address an instance by name through `ctx.resolve_actor::<R>(subname)`.
///
/// Mutually exclusive with [`Singleton`] at the type level. ADR-0079.
pub trait Instanced: Actor {
    /// This actor's [`MailboxId`] for the instance named `subname`, as seen
    /// by a caller whose lineage carry is `caller_carry` (ADR-0099 §5).
    /// Symmetric to [`Singleton::resolve`]: an instanced node folds its
    /// `ActorId` — `hash(NAMESPACE:subname)` — onto the caller's carry, so
    /// the same type resolves to a different mailbox under each parent and
    /// for each subname.
    ///
    /// The embedding-host class [`EmbeddedHost`] is the instanced type
    /// behind every embeddable actor's resolution: a component folds its
    /// own `NAMESPACE` as an instance under `aether.embedded` onto the
    /// component-host cap's carry (the composition that supplies that carry
    /// lives with the host cap — `aether-capabilities`'s `resolve_embedded`).
    #[must_use]
    fn resolve(caller_carry: u64, subname: &str) -> MailboxId {
        MailboxId(with_tag(
            Tag::Mailbox,
            fold_lineage(caller_carry, ActorId::instanced(Self::NAMESPACE, subname)),
        ))
    }
}

/// The embedding-host class (ADR-0099 §5/§6): the reserved namespace
/// `aether.embedded` under which every **embeddable** actor — an FFI/wasm
/// component — resolves, independent of the concrete host that embeds it
/// (`WasmTrampoline` today, a dynamic-library host tomorrow). This type is
/// the sole owner of the `"aether.embedded"` literal; concrete hosts
/// forward-feed `EmbeddedHost::NAMESPACE` rather than re-declaring it, so an
/// embeddable actor's id depends on what the code is, not how it is hosted,
/// and the class namespace is read only here (ADR-0099 §5's read-from-owner
/// rule).
///
/// `EmbeddedHost` is [`Instanced`] — one node per embedded component, keyed
/// by the component's own `NAMESPACE`. An embeddable actor resolves by
/// folding its name as an instance under this class onto the component-host
/// cap's carry; the carry is supplied by the composition co-located with the
/// host cap (`aether-capabilities`'s `resolve_embedded`), which names neither
/// the `aether.component` host nor `aether.embedded` itself — each is read
/// only from its owner.
pub struct EmbeddedHost;

impl Actor for EmbeddedHost {
    const NAMESPACE: &'static str = "aether.embedded";
}

impl Instanced for EmbeddedHost {}

/// How a spawned child's mailbox subname is derived (ADR-0079). The
/// full mailbox name is `"{A::NAMESPACE}:{subname}"`; the substrate
/// hashes that string deterministically (ADR-0029) to the returned
/// `MailboxId`. Shared spawn-addressing vocabulary: native
/// `spawn_child` and the FFI guest's `FfiCtx::spawn_child` (ADR-0097)
/// both name children through it, so the two transports name children
/// the same way.
#[derive(Debug, Clone, Copy)]
pub enum Subname<'a> {
    /// Spawner-allocated monotonic discriminator — "spawn me one of
    /// these, I'll track the returned `MailboxId`." The fit for
    /// per-connection / per-entity churn where no human-readable name
    /// is needed.
    Counter,
    /// Caller-supplied subname. Must pass [`validate_namespace_segment`]
    /// and be unique within the owning prefix (no `:` separator); names
    /// retire on drop (ADR-0079).
    Named(&'a str),
}

/// Validation outcome for namespace segments — both the `NAMESPACE`
/// const on an [`Instanced`] type (the listener prefix) and the
/// runtime subname passed to `spawn_child`. Same rules apply at both
/// sites: stay printable-ASCII-ish, don't collide with the structural
/// `:` separator, and stay under [`NAMESPACE_SEGMENT_MAX_LEN`] bytes.
///
/// `TooLong` carries the limit so error messages can render it
/// without re-fetching the const, and so future relaxation can vary
/// the limit per call site if needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceError {
    Empty,
    ContainsSeparator,
    ContainsControlOrWhitespace,
    TooLong { limit: usize },
}

/// Byte-length cap for namespace segments. Generous — full names like
/// `aether.net.session:<uuid>` (~50 bytes) clear it by a wide margin —
/// but bounded so the registry's `HashMap<MailboxId, _>` keys stay in
/// a predictable size class and a runaway caller can't grow the
/// tombstone set with megabyte names.
pub const NAMESPACE_SEGMENT_MAX_LEN: usize = 256;

/// Validate a namespace segment. Used at registration time on the
/// `NAMESPACE` const of [`Singleton`] / [`Instanced`] types, and at
/// runtime on the `subname` passed to `spawn_child`. ADR-0079.
///
/// Rejects:
/// - empty segments
/// - segments containing the `:` separator
/// - segments containing ASCII control bytes or any whitespace (incl. space)
/// - segments longer than [`NAMESPACE_SEGMENT_MAX_LEN`] bytes
///
/// Multi-byte UTF-8 (CJK, emoji, ...) is allowed — the rule is "no
/// ASCII control / whitespace / separator," not "ASCII-only." `MailboxId`
/// hashing is byte-level so any valid UTF-8 hashes deterministically.
pub fn validate_namespace_segment(s: &str) -> Result<(), NamespaceError> {
    if s.is_empty() {
        return Err(NamespaceError::Empty);
    }
    if s.len() > NAMESPACE_SEGMENT_MAX_LEN {
        return Err(NamespaceError::TooLong {
            limit: NAMESPACE_SEGMENT_MAX_LEN,
        });
    }
    for c in s.chars() {
        if c == ':' {
            return Err(NamespaceError::ContainsSeparator);
        }
        if c.is_control() || c.is_whitespace() {
            return Err(NamespaceError::ContainsControlOrWhitespace);
        }
    }
    Ok(())
}

/// Per-handler-kind marker: `R: HandlesKind<K>` means actor `R` has a
/// `#[handler]` method accepting kind `K`. Auto-emitted by the
/// `#[actor]` proc-macro alongside the dispatch table — one impl per
/// handler kind. Authors never write these by hand.
///
/// Gates `ActorMailbox<'_, R, T>::send::<K>` (constructed via
/// `ctx.actor::<R>()` / `ctx.resolve_actor::<R>(name)`) so the compiler
/// rejects sends to a kind the receiver doesn't handle.
/// The single source of truth is the handler list on the actor's
/// `impl` block; adding a `#[handler]` updates senders' compile-time
/// checks automatically. ADR-0075 §Decision 1.
///
/// Blanket impls (e.g. `impl<T: Into<DrawTriangle>> HandlesKind<T> for
/// RenderCapability`) are an opt-in extension if a real conversion case
/// wants them; the default macro emission is strict so wire bytes stay
/// obvious.
pub trait HandlesKind<K: Kind>: Actor {}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::{fold_lineage, mailbox_id_from_name};

    /// Issue 625 (ADR-0079): `#[derive(Singleton)]` and
    /// `#[derive(Instanced)]` are the explicit author-side surface
    /// for cardinality. Trait mutual exclusion is documentation +
    /// use-site bounds (no sealed-trait enforcement); this smoke
    /// confirms both derives produce reachable marker impls
    /// independently.
    #[test]
    fn singleton_derive_emits_marker_impl() {
        #[derive(crate::Singleton)]
        struct UniqueCap;
        impl Actor for UniqueCap {
            const NAMESPACE: &'static str = "test.cardinality.unique";
        }
        fn requires_singleton<T: Singleton>() {}
        requires_singleton::<UniqueCap>();
    }

    #[test]
    fn instanced_derive_emits_marker_impl() {
        #[derive(crate::Instanced)]
        struct PerThing;
        impl Actor for PerThing {
            const NAMESPACE: &'static str = "test.cardinality.per_thing";
        }
        fn requires_instanced<T: Instanced>() {}
        requires_instanced::<PerThing>();
    }

    /// ADR-0099 §5 static default: a root-pinned singleton's [`Singleton::resolve`]
    /// ignores the caller's carry and returns the depth-1 fixed point —
    /// the id `mailbox_id_from_name(NAMESPACE)` yields today, so the
    /// chassis-cap vocabulary stays frozen (§3).
    #[test]
    fn singleton_resolve_default_is_frozen_depth_one() {
        struct RootCap;
        impl Actor for RootCap {
            const NAMESPACE: &'static str = "test.resolve.rootcap";
        }
        impl Singleton for RootCap {}

        let frozen = mailbox_id_from_name("test.resolve.rootcap");
        assert_eq!(
            RootCap::resolve(0),
            frozen,
            "default resolve is the depth-1 id"
        );
        assert_eq!(
            RootCap::resolve(0xDEAD_BEEF),
            frozen,
            "a root cap ignores the caller's carry"
        );
    }

    /// ADR-0099 §5 own-child path: a non-root singleton overrides
    /// [`Singleton::resolve`] to fold the caller's carry, so the same type
    /// resolves to a different mailbox under each parent.
    #[test]
    fn singleton_resolve_override_folds_caller_carry() {
        struct Child;
        impl Actor for Child {
            const NAMESPACE: &'static str = "test.resolve.child";
        }
        impl Singleton for Child {
            fn resolve(caller_carry: u64) -> MailboxId {
                MailboxId(with_tag(
                    Tag::Mailbox,
                    fold_lineage(caller_carry, ActorId::singleton(<Self as Actor>::NAMESPACE)),
                ))
            }
        }

        let carry = 0x1234_5678_u64;
        let expected = MailboxId(with_tag(
            Tag::Mailbox,
            fold_lineage(carry, ActorId::singleton("test.resolve.child")),
        ));
        assert_eq!(Child::resolve(carry), expected, "override folds the carry");
        assert_ne!(
            Child::resolve(carry),
            mailbox_id_from_name("test.resolve.child"),
            "a folded child id differs from the flat depth-1 hash"
        );
    }

    /// ADR-0099 §5 instanced default: an [`Instanced`] type's provided
    /// `resolve` folds its `ActorId::instanced(NAMESPACE, subname)` onto the
    /// caller's carry — symmetric to the singleton own-child override, but
    /// keyed by `subname` so each instance under a parent gets its own id.
    #[test]
    fn instanced_resolve_default_folds_carry_and_subname() {
        struct PerThing;
        impl Actor for PerThing {
            const NAMESPACE: &'static str = "test.resolve.per_thing";
        }
        impl Instanced for PerThing {}

        let carry = 0x0BAD_F00D_u64;
        let expected = MailboxId(with_tag(
            Tag::Mailbox,
            fold_lineage(carry, ActorId::instanced("test.resolve.per_thing", "42")),
        ));
        assert_eq!(
            PerThing::resolve(carry, "42"),
            expected,
            "folds carry+subname"
        );
        assert_ne!(
            PerThing::resolve(carry, "42"),
            PerThing::resolve(carry, "43"),
            "different subnames resolve to different mailboxes"
        );
    }

    /// ADR-0099 §5/§6: the embedding-host class [`EmbeddedHost`] owns the
    /// reserved `aether.embedded` namespace and resolves an embedded instance
    /// by folding `aether.embedded:<name>` onto the host cap's carry. This is
    /// the fold `aether-capabilities`'s `resolve_embedded` composes (with the
    /// component-host carry supplied there).
    #[test]
    fn embedded_host_folds_under_reserved_namespace() {
        assert_eq!(EmbeddedHost::NAMESPACE, "aether.embedded");

        // Stand in for the `aether.component` host cap's depth-1 carry.
        let host_carry = mailbox_id_from_name("aether.component").0;
        let expected = MailboxId(with_tag(
            Tag::Mailbox,
            fold_lineage(host_carry, ActorId::instanced("aether.embedded", "camera")),
        ));
        assert_eq!(
            EmbeddedHost::resolve(host_carry, "camera"),
            expected,
            "embeddable id is the host-class fold"
        );
        assert_ne!(
            EmbeddedHost::resolve(host_carry, "camera"),
            mailbox_id_from_name("camera"),
            "the fold differs from the bare-NAMESPACE hash (the #1364 miss)"
        );
    }
}
