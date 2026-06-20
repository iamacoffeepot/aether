//! Actor SDK primitive: the marker trait surface (here in `mod.rs`)
//! plus per-mail / per-init / per-drop ctx machinery
//! ([`ctx`]) and the `Slot` single-instance
//! backing store ([`slot`]). Marker traits are
//! pure compile-time markers — no transport machinery, no lifecycle
//! methods, just identity (`Addressable`), singleton-ness (`Singleton`),
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
pub mod slot;

use aether_data::{ActorId, Kind, MailboxId, Tag, fold_lineage, with_tag};

/// A resolution strategy (ADR-0119): given a caller's lineage carry, the
/// actor's own `NAMESPACE`, and whatever args the strategy needs, produce
/// the `MailboxId`. An actor selects one of these as its
/// [`Addressable::Resolver`]; cardinality is *derived* from the resolver's
/// [`Args`](Resolve::Args) shape rather than declared.
///
/// `Args` is a generic associated type because a keyed resolver borrows its
/// key (`&'a str`): keyless strategies set `Args<'a> = ()`, keyed ones set
/// `Args<'a> = &'a str`.
pub trait Resolve {
    /// What addressing this strategy requires: `()` keyless, a borrowed key
    /// for a keyed (instanced) target.
    type Args<'a>;

    /// Produce the mailbox for `namespace` as this strategy sees it, given
    /// the caller's lineage carry and the strategy-specific `args`.
    #[must_use]
    fn resolve(caller_carry: u64, namespace: &str, args: Self::Args<'_>) -> MailboxId;
}

/// Root-pinned keyless resolution (ADR-0119): the depth-1 fixed point
/// (ADR-0099 §3), this actor's own [`ActorId`] tagged as a mailbox,
/// **ignoring the caller's carry** because a root cap sits at the root. It
/// equals `mailbox_id_from_name(NAMESPACE)` because [`with_tag`] is
/// idempotent on an already-`Mailbox`-tagged value, so every chassis cap
/// keeps the exact id it has today. Makes its actor a [`Singleton`].
pub struct One;

impl Resolve for One {
    type Args<'a> = ();
    fn resolve(_caller_carry: u64, namespace: &str, _args: ()) -> MailboxId {
        MailboxId(with_tag(Tag::Mailbox, ActorId::singleton(namespace).0))
    }
}

/// Keyed resolution (ADR-0119): folds `ActorId::instanced(NAMESPACE, subname)`
/// onto the caller's carry, so the same type resolves to a different mailbox
/// under each parent and for each subname. Makes its actor an [`Instanced`].
pub struct Many;

impl Resolve for Many {
    type Args<'a> = &'a str;
    fn resolve(caller_carry: u64, namespace: &str, subname: &str) -> MailboxId {
        MailboxId(with_tag(
            Tag::Mailbox,
            fold_lineage(caller_carry, ActorId::instanced(namespace, subname)),
        ))
    }
}

/// The reserved scope under which every embedded actor — an FFI/wasm
/// component hosted by the component-host trampoline — resolves (ADR-0099
/// §5/§6, ADR-0119). The sole owner of the `"aether.embedded"` literal;
/// concrete hosts (the trampoline, the substrate `TRAMPOLINE_NAMESPACE`)
/// forward-feed this const rather than re-declaring it.
pub const EMBEDDED_SCOPE: &str = "aether.embedded";

/// Keyless embedded resolution (ADR-0119): folds
/// `instanced(EMBEDDED_SCOPE, NAMESPACE)` onto the caller's carry — the
/// component's own name as an instance under the reserved embed scope.
/// Resolution is relative (ADR-0099 §5): in the embedding context the caller
/// is the component host, whose carry is `aether.component`, so the result is
/// the component's registered mailbox. The carry is supplied by the caller
/// (`aether-capabilities`'s `resolve_embedded` for by-name lookups), not
/// re-derived here. Keyless (`Args<'a> = ()`), so an embedded actor is a
/// [`Singleton`].
pub struct Embedded;

impl Resolve for Embedded {
    type Args<'a> = ();
    fn resolve(caller_carry: u64, namespace: &str, _args: ()) -> MailboxId {
        MailboxId(with_tag(
            Tag::Mailbox,
            fold_lineage(caller_carry, ActorId::instanced(EMBEDDED_SCOPE, namespace)),
        ))
    }
}

/// Keyed embedded resolution (ADR-0119, ADR-0097): a spawned sibling under
/// the embed scope, keyed by a runtime `subname` rather than the actor's own
/// `NAMESPACE`. Folds `instanced(EMBEDDED_SCOPE, subname)` onto the caller's
/// carry. Keyed (`Args<'a> = &'a str`), so it is an [`Instanced`].
pub struct EmbeddedMany;

impl Resolve for EmbeddedMany {
    type Args<'a> = &'a str;
    fn resolve(caller_carry: u64, _namespace: &str, subname: &str) -> MailboxId {
        MailboxId(with_tag(
            Tag::Mailbox,
            fold_lineage(caller_carry, ActorId::instanced(EMBEDDED_SCOPE, subname)),
        ))
    }
}

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
pub trait Addressable: Sized + Send + 'static {
    /// The recipient name this actor claims **within its scope**
    /// (ADR-0098). For a root-scoped actor — every chassis capability —
    /// it is the full mailbox name (`aether.<name>`). For an actor
    /// hosted inside a parent the full mailbox name is the path
    /// `"{scope}:{NAMESPACE}"`, so `NAMESPACE` is just the segment: a
    /// wasm component declaring `NAMESPACE = "aether.camera"` and loaded at its
    /// default name registers at `aether.embedded:aether.camera`
    /// under its component-host, not at the bare `"aether.camera"`.
    const NAMESPACE: &'static str;

    /// The resolution strategy this actor selects (ADR-0119). Cardinality
    /// is derived from it: a keyless resolver ([`One`] / [`Embedded`],
    /// `Args<'a> = ()`) makes the actor a [`Singleton`]; a keyed resolver
    /// ([`Many`] / [`EmbeddedMany`], `Args<'a> = &'a str`) makes it
    /// [`Instanced`]. The `#[bridge]` / `#[actor]` macros emit this; a
    /// hand-written actor sets it directly.
    type Resolver: Resolve;

    /// This actor's [`MailboxId`] as seen by a caller whose lineage carry is
    /// `caller_carry` (ADR-0099 §5), produced by delegating to the selected
    /// [`Resolver`](Self::Resolver). Declared once here, never overridden —
    /// variation lives in the chosen resolver, not in this method (ADR-0119).
    /// `ctx.actor::<R>()` calls this with `()`; `ctx.resolve_actor::<R>(key)`
    /// calls it with the borrowed key.
    #[must_use]
    fn resolve(caller_carry: u64, args: <Self::Resolver as Resolve>::Args<'_>) -> MailboxId {
        <Self::Resolver as Resolve>::resolve(caller_carry, Self::NAMESPACE, args)
    }
}

/// The boot/teardown capability an actor composes onto its identity
/// (iamacoffeepot/aether#2048). The lifecycle was declared twice —
/// once on [`crate::WasmActor`] (wasm/guest) and once on
/// `aether_substrate::NativeActor` (native cap) — with near-identical
/// signatures the two crates kept in sync by hand. Hoisting it onto one
/// standalone trait both transports compose makes a divergent edit a
/// compile error instead of silent drift.
///
/// `Lifecycle` carries **no** supertrait. The methods never read
/// `NAMESPACE`: `init` returns `Self`, `wire`/`unwire` operate on the
/// ctx, and a send inside a hook is gated on the *target's* cardinality
/// marker, not on `Self`'s identity. The "a thing that boots must have a
/// mailbox to boot into" constraint is asserted where it bites — on the
/// transport subtraits (`WasmActor`/`NativeActor`), which add `Addressable` as a
/// co-supertrait — rather than welded into the capability.
///
/// The per-target contexts are generic associated types each concrete
/// impl pins: the `#[actor]` macro knows the target and emits
/// `type InitCtx<'a> = WasmInitCtx<'a>; type Ctx<'a> = WasmCtx<'a>;` (or the
/// native pair), so a `wire`/`unwire` body reaches the concrete ctx's
/// inherent methods (`ctx.actor::<R>().send(&p)`) with no generic bound at
/// the call site. `InitError` is pinned per transport subtrait
/// (`WasmActor: Lifecycle<InitError = BootError>`), so existing generic call
/// sites keep seeing a concrete error type.
pub trait Lifecycle {
    /// ADR-0090 boot configuration the chassis threads into [`Self::init`].
    /// `Send + 'static` only here; [`crate::WasmActor`] tightens it to
    /// `Kind` (FFI config crosses the wasm boundary as bytes) while native
    /// config stays a live Rust value.
    type Config: Send + 'static;

    /// The error [`Self::init`] returns when the actor cannot start. Pinned
    /// to the concrete boot error on each transport subtrait.
    type InitError;

    /// The per-target init ctx (`WasmInitCtx<'a>` / `NativeInitCtx<'a>`),
    /// synthesized per impl by `#[actor]`.
    type InitCtx<'a>;

    /// The per-target post-init ctx (`WasmCtx<'a>` / `NativeCtx<'a>`),
    /// synthesized per impl by `#[actor]`.
    type Ctx<'a>;

    /// Runs once before any mail. Resolves kinds/handles via `ctx` and
    /// returns the initial state. ADR-0079: the init ctx carries no send
    /// surface — use [`Self::wire`] for mail-driven setup.
    fn init(config: Self::Config, ctx: &mut Self::InitCtx<'_>) -> Result<Self, Self::InitError>
    where
        Self: Sized;

    /// Post-init, mail-allowed hook (ADR-0079). Runs after `init` returned
    /// `Ok` and the mailbox is published, before the first envelope.
    /// Default no-op; override to register subscriptions or announce.
    fn wire(&mut self, ctx: &mut Self::Ctx<'_>) {
        let _ = ctx;
    }

    /// Pre-shutdown, mail-allowed hook (ADR-0079). Runs after the inbox
    /// drain, before the actor value drops. Default no-op.
    fn unwire(&mut self, ctx: &mut Self::Ctx<'_>) {
        let _ = ctx;
    }
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
/// Derived from the resolver (ADR-0119): a keyless [`Resolver`](Addressable::Resolver)
/// (`Args<'a> = ()` — [`One`] for root caps, [`Embedded`] for components)
/// makes the actor a `Singleton`, reached by `ctx.actor::<R>()`. The blanket
/// impl supplies it; nobody writes `impl Singleton`.
pub trait Singleton: Addressable<Resolver: for<'a> Resolve<Args<'a> = ()>> {}
impl<T: Addressable<Resolver: for<'a> Resolve<Args<'a> = ()>>> Singleton for T {}

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
/// Derived from the resolver (ADR-0119): a keyed [`Resolver`](Addressable::Resolver)
/// (`Args<'a> = &'a str` — [`Many`], or [`EmbeddedMany`] for spawned
/// siblings) makes the actor an `Instanced`, reached by
/// `ctx.resolve_actor::<R>(subname)`. The blanket impl supplies it; nobody
/// writes `impl Instanced`.
pub trait Instanced: Addressable<Resolver: for<'a> Resolve<Args<'a> = &'a str>> {}
impl<T: Addressable<Resolver: for<'a> Resolve<Args<'a> = &'a str>>> Instanced for T {}

/// How a spawned child's mailbox subname is derived (ADR-0079). The
/// full mailbox name is `"{A::NAMESPACE}:{subname}"`; the substrate
/// hashes that string deterministically (ADR-0029) to the returned
/// `MailboxId`. Shared spawn-addressing vocabulary: native
/// `spawn_child` and the FFI guest's `WasmCtx::spawn_child` (ADR-0097)
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
pub trait HandlesKind<K: Kind>: Addressable {}

/// A complete actor: an addressable identity ([`Addressable`]) that also
/// carries a boot lifecycle ([`Lifecycle`]). The blanket impl supplies it
/// for any type that is both, so `WasmActor` / `NativeActor` implementors
/// are `Actor` automatically. Code that wants a fully-formed actor bounds
/// `Actor`; code that wants only identity bounds `Addressable`.
pub trait Actor: Addressable + Lifecycle {}
impl<T: Addressable + Lifecycle> Actor for T {}

#[cfg(test)]
mod tests {
    // These tests assert the resolve/lineage machinery against the depth-1
    // name hash — the primitive is the reference value under test, not a
    // sibling-cap address.
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use aether_data::{fold_lineage, mailbox_id_from_name};

    /// ADR-0119: cardinality is derived from the resolver. A keyless
    /// [`One`] resolver makes the actor a reachable [`Singleton`] via
    /// the blanket impl — no hand-written `impl Singleton`.
    #[test]
    fn one_resolver_derives_singleton() {
        struct UniqueCap;
        impl Addressable for UniqueCap {
            const NAMESPACE: &'static str = "test.cardinality.unique";
            type Resolver = One;
        }
        fn requires_singleton<T: Singleton>() {}
        requires_singleton::<UniqueCap>();
    }

    /// ADR-0119: a keyed [`Many`] resolver makes the actor a reachable
    /// [`Instanced`] via the blanket impl.
    #[test]
    fn many_resolver_derives_instanced() {
        struct PerThing;
        impl Addressable for PerThing {
            const NAMESPACE: &'static str = "test.cardinality.per_thing";
            type Resolver = Many;
        }
        fn requires_instanced<T: Instanced>() {}
        requires_instanced::<PerThing>();
    }

    /// ADR-0099 §5 / ADR-0119: the [`One`] resolver ignores the caller's
    /// carry and returns the depth-1 fixed point — the id
    /// `mailbox_id_from_name(NAMESPACE)` yields today, so the chassis-cap
    /// vocabulary stays frozen (§3).
    #[test]
    fn one_resolver_is_frozen_depth_one() {
        struct RootCap;
        impl Addressable for RootCap {
            const NAMESPACE: &'static str = "test.resolve.rootcap";
            type Resolver = One;
        }

        let frozen = mailbox_id_from_name("test.resolve.rootcap");
        assert_eq!(
            <RootCap as Addressable>::resolve(0, ()),
            frozen,
            "One is the depth-1 id"
        );
        assert_eq!(
            <RootCap as Addressable>::resolve(0xDEAD_BEEF, ()),
            frozen,
            "One ignores the caller's carry"
        );
    }

    /// ADR-0099 §5 / ADR-0119: the [`Many`] resolver folds
    /// `ActorId::instanced(NAMESPACE, subname)` onto the caller's carry, so
    /// each instance under a parent gets its own id keyed by subname.
    #[test]
    fn many_resolver_folds_carry_and_subname() {
        struct PerThing;
        impl Addressable for PerThing {
            const NAMESPACE: &'static str = "test.resolve.per_thing";
            type Resolver = Many;
        }

        let carry = 0x0BAD_F00D_u64;
        let expected = MailboxId(with_tag(
            Tag::Mailbox,
            fold_lineage(carry, ActorId::instanced("test.resolve.per_thing", "42")),
        ));
        assert_eq!(
            <PerThing as Addressable>::resolve(carry, "42"),
            expected,
            "folds carry+subname"
        );
        assert_ne!(
            <PerThing as Addressable>::resolve(carry, "42"),
            <PerThing as Addressable>::resolve(carry, "43"),
            "different subnames resolve to different mailboxes"
        );
    }
}
