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
//! markers — but `aether-actor` already depended on `aether-kinds` for its
//! shared kind vocabulary, so a forward dep would cycle.
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
    /// the selected caller scope and the strategy-specific `args`.
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
        MailboxId(with_tag(Tag::Mailbox, fold_lineage(caller_carry, ActorId::instanced(namespace, subname))))
    }
}

/// The reserved scope under which every embedded actor — an FFI/wasm
/// component hosted by the component-host trampoline — resolves (ADR-0099
/// §5/§6, ADR-0119). The sole owner of the `"aether.embedded"` literal;
/// concrete hosts (the trampoline, the substrate `TRAMPOLINE_NAMESPACE`)
/// forward-feed this const rather than re-declaring it.
pub const EMBEDDED_SCOPE: &str = "aether.embedded";

/// Keyless embedded resolution (ADR-0119): folds
/// `instanced(EMBEDDED_SCOPE, NAMESPACE)` onto the selected parent carry — the
/// component's own name as an instance under the reserved embed scope.
/// The runtime retains the calling actor's logical parent mailbox and
/// [`CallerScoped`] selects it as the routing seed. This makes the same actor
/// type resolve beneath whichever host actually embedded it, without naming a
/// concrete host or looking one up. Keyless (`Args<'a> = ()`), so an embedded
/// actor is a [`Singleton`]; bare-type addressing selects its default
/// [`Addressable::NAMESPACE`], while a named peer supplies its runtime load
/// namespace through the same resolver.
pub struct Embedded;

impl Resolve for Embedded {
    type Args<'a> = ();
    fn resolve(caller_carry: u64, namespace: &str, _args: ()) -> MailboxId {
        MailboxId(with_tag(Tag::Mailbox, fold_lineage(caller_carry, ActorId::instanced(EMBEDDED_SCOPE, namespace))))
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
        MailboxId(with_tag(Tag::Mailbox, fold_lineage(caller_carry, ActorId::instanced(EMBEDDED_SCOPE, subname))))
    }
}

/// Which caller-relative lineage seed a resolver consumes.
///
/// Each scope can use the relevant actor's routable [`MailboxId`]; it does not
/// require a separately retained untagged FNV state. [`with_tag`] changes only
/// the high four bits, while each [`fold_lineage`] step's low 60 output bits
/// depend only on the seed's low 60 bits. A mailbox id therefore preserves
/// every bit that can affect any later tagged descendant route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerScope {
    /// The resolver is root-pinned and does not consume caller lineage.
    Root,
    /// The calling actor's own mailbox lineage.
    Current,
    /// The calling actor's logical parent's mailbox lineage.
    Parent,
}

impl CallerScope {
    /// Select the routable lineage seed this scope names.
    ///
    /// Root-pinned resolvers receive [`MailboxId::NONE`] because they do not
    /// consume caller lineage. Current and parent scopes receive the logical
    /// actor mailboxes retained by the runtime; no separate untagged hash
    /// state is required.
    #[must_use]
    pub const fn select(self, current: MailboxId, parent: MailboxId) -> MailboxId {
        match self {
            Self::Root => MailboxId::NONE,
            Self::Current => current,
            Self::Parent => parent,
        }
    }
}

/// A [`Resolve`] strategy that declares which caller-relative scope its fold
/// consumes. Every ctx send selects [`Self::SCOPE`] rather than assuming that
/// all resolvers consume the calling actor's own lineage.
///
/// Implemented for the four built-in strategies:
///
/// - [`One`] declares [`CallerScope::Root`] and ignores caller lineage.
/// - [`Many`] declares [`CallerScope::Current`] for a keyed child of the
///   caller.
/// - [`Embedded`] declares [`CallerScope::Parent`] for a co-hosted embedded
///   singleton beneath the caller's runtime parent.
/// - [`EmbeddedMany`] declares [`CallerScope::Current`] for a spawned sibling
///   whose lineage extends the spawner's (ADR-0099 §Negative "sibling spawn
///   nests").
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a caller-scoped resolution strategy",
    label = "does not declare a caller-relative scope for bare-type resolution",
    note = "a resolver used by a typed ctx must select `Root`, `Current`, or `Parent`; use an \
            explicit by-name or by-id route when no caller-relative scope describes the target"
)]
pub trait CallerScoped: Resolve {
    /// The caller-relative scope this resolver consumes.
    const SCOPE: CallerScope;
}

impl CallerScoped for One {
    const SCOPE: CallerScope = CallerScope::Root;
}
impl CallerScoped for Many {
    const SCOPE: CallerScope = CallerScope::Current;
}
impl CallerScoped for Embedded {
    const SCOPE: CallerScope = CallerScope::Parent;
}
impl CallerScoped for EmbeddedMany {
    const SCOPE: CallerScope = CallerScope::Current;
}

/// An actor that can be addressed by bare type from a peer's ctx, because its
/// [`Resolver`](Addressable::Resolver) is [`CallerScoped`] (ADR-0119
/// amendment). Bounds every carry-passing send surface —
/// [`MailSender::send`](crate::MailSender::send), `ctx.actor::<R>()`, and their
/// batched / detached siblings — beside the cardinality marker.
///
/// Auto-implemented from the resolver with the constraint in **supertrait
/// position** so it elaborates to call sites, the same mechanism
/// [`Singleton`] / [`Instanced`] use. Nobody writes `impl CallerAddressable`.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be addressed by bare type from this context",
    label = "its resolver does not declare a caller-relative scope",
    note = "use an explicit by-name or by-id route when the target cannot select `Root`, \
            `Current`, or `Parent` from the caller's runtime context"
)]
pub trait CallerAddressable: Addressable<Resolver: CallerScoped> {}
impl<T: Addressable<Resolver: CallerScoped>> CallerAddressable for T {}

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
    /// wasm component declaring `NAMESPACE = "aether.kit.camera"` and loaded at its
    /// default name registers at `aether.embedded:aether.camera`
    /// under its component-host, not at the bare `"aether.kit.camera"`.
    const NAMESPACE: &'static str;

    /// The resolution strategy this actor selects (ADR-0119). Cardinality
    /// is derived from it: a keyless resolver ([`One`] / [`Embedded`],
    /// `Args<'a> = ()`) makes the actor a [`Singleton`]; a keyed resolver
    /// ([`Many`] / [`EmbeddedMany`], `Args<'a> = &'a str`) makes it
    /// [`Instanced`]. The `#[actor]` macro emits this; a hand-written
    /// actor sets it directly.
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

/// Placement permission for an actor identity that may appear without an
/// actor parent (ADR-0166).
///
/// This is independent of cardinality and runtime liveness: a root actor may
/// be singleton or instanced, and implementing this trait does not mean that
/// an instance currently exists.
pub trait Root: Addressable {}

/// Placement permission for an actor identity that may appear directly
/// beneath logical parent `P` (ADR-0166).
///
/// This describes a legal edge, not ownership, supervision, or liveness. A
/// child may implement `ChildOf` for several distinct parents and may also
/// implement [`Root`] when both placements are meaningful.
pub trait ChildOf<P: Addressable>: Addressable {}

/// The boot/teardown capability an actor composes onto its identity
/// (iamacoffeepot/aether#2048). The lifecycle was declared twice —
/// once on [`crate::WasmActor`] (wasm/guest) and once on
/// `aether_substrate::NativeActor` (native cap) — with near-identical
/// signatures the two crates kept in sync by hand. Hoisting it onto one
/// standalone trait both transports compose makes a divergent edit a
/// compile error instead of silent drift.
///
/// `Lifecycle<S>` is **generic over the runtime state `S`** the identity
/// boots into (iamacoffeepot/aether#2311): `init` returns `S`, and
/// `wire`/`unwire` operate over `&mut S`. The identity type that implements
/// it is the *addressing* identity; the state `S` is plain data the dispatch
/// surface runs over. For an un-split actor `S = Self`, so `&mut S == &mut
/// self` and the author's `init`/`wire` bodies are unchanged. Both transports
/// (`WasmActor`/`NativeActor`) share this one trait so a divergent edit is a
/// compile error instead of silent drift.
///
/// `Lifecycle` carries **no** supertrait. The methods never read
/// `NAMESPACE`: a send inside a hook is gated on the *target's* cardinality
/// marker, not on the identity. The "a thing that boots must have a
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
/// (`WasmActor: Lifecycle<_, InitError = ActorInitError>`), so existing generic
/// call sites keep seeing a concrete error type.
pub trait Lifecycle<S> {
    /// ADR-0090 boot configuration the chassis threads into [`Self::init`].
    /// `Send + 'static` only here; [`crate::WasmActor`] tightens it to
    /// `Kind` (FFI config crosses the wasm boundary as bytes) while native
    /// config stays a live Rust value.
    type Config: Send + 'static;

    /// ADR-0156 §1/§2 composer-supplied construction input the composer
    /// threads into [`Self::init`] beside [`Self::Config`]. Where `Config`
    /// is the ADR-0090 argv/env/default-resolved settings, `Params` is the
    /// second, orthogonal channel: values the composer computes and hands in
    /// at boot. `Send + 'static` only here; [`crate::WasmActor`] tightens it
    /// to `Kind + Default` (FFI params cross the wasm boundary as bytes, an
    /// empty slice resolving to the default), while a native cap shares the
    /// composer's address space and keeps `Params` a live Rust value.
    ///
    /// The `#[actor]` macro synthesizes `type Params = ();` when the author
    /// omits it (stable Rust has no associated-type defaults), mirroring the
    /// [`crate::WasmActor::Persist`] stand-in, so an actor that needs no
    /// composer input pays nothing.
    type Params: Send + 'static;

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
    /// returns the initial runtime state `S`. Receives the ADR-0090
    /// [`Config`](Self::Config) and the ADR-0156 composer-supplied
    /// [`Params`](Self::Params) as the two construction channels. ADR-0079:
    /// the init ctx carries no send surface — use [`Self::wire`] for
    /// mail-driven setup.
    fn init(config: Self::Config, params: Self::Params, ctx: &mut Self::InitCtx<'_>) -> Result<S, Self::InitError>;

    /// Post-init, mail-allowed hook (ADR-0079). Runs after `init` returned
    /// `Ok` and the mailbox is published, before the first envelope.
    /// Default no-op; override to register subscriptions or announce.
    fn wire(state: &mut S, ctx: &mut Self::Ctx<'_>) {
        let _ = (state, ctx);
    }

    /// Pre-shutdown, mail-allowed hook (ADR-0079). Runs after the inbox
    /// drain, before the actor value drops. Default no-op.
    fn unwire(state: &mut S, ctx: &mut Self::Ctx<'_>) {
        let _ = (state, ctx);
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
/// type-addresses them with `ctx.actor::<R>()`. A singleton hosted inside a
/// parent resolves from the runtime-retained parent mailbox. For a loaded
/// component, `ctx.actor::<R>()` selects the default load name
/// (`R::NAMESPACE`) and a component's `peer_named::<R>(name)` facade supplies
/// an explicit runtime load name. The component-host's `loaded::<R>(name)`
/// facade remains the explicit root-host route for callers that already hold
/// that host mailbox. Replicas remain explicitly named (`base-0`, `base-1`,
/// …), since no default-named instance exists.
///
/// The rendered address itself — `LoadResult.name`, e.g.
/// `aether.component/aether.embedded:NAME` — is a path of nodes, so it
/// belongs only to the string surfaces that parse one
/// (`mailbox_id_from_path`: the registry's name lookup, the MCP
/// `recipient_name` surface). Handing it to `ctx.send_to_named(name, …)`
/// misses, because that flat escape hatch hashes its argument as one root
/// name (`mailbox_id_from_name`): a `/` trips that hasher's debug assertion,
/// and a release build resolves an id nothing registered, so the mail routes
/// cleanly and drops. `ctx.resolve_actor::<R>(key)` is different: it is a
/// typed keyed route available only to [`Instanced`] actors, and delegates
/// the key plus the resolver-selected current / root / parent scope to
/// `R::resolve`. The id beside a rendered name (`LoadResult.mailbox_id`) has
/// no such ambiguity: a caller already holding one sends to it directly,
/// through the guest's `ctx.send_to(id, &mail)` or the native
/// `ctx.actor_at::<R>(id)`.
///
/// Mutually exclusive with [`Instanced`] at the type level: an actor is
/// either one-of-a-kind within a scope (singleton) or N-instances under
/// a shared prefix (instanced, name-keyed). ADR-0079.
/// Derived from the resolver (ADR-0119): a keyless [`Resolver`](Addressable::Resolver)
/// (`Args<'a> = ()` — [`One`] for root caps, [`Embedded`] for components)
/// makes the actor a `Singleton`. The blanket impl supplies it; nobody writes
/// `impl Singleton`. Typed addressing also asks for [`CallerAddressable`];
/// [`Embedded`] satisfies it by selecting [`CallerScope::Parent`].
pub trait Singleton: Addressable<Resolver: for<'a> Resolve<Args<'a> = ()>> {}
impl<T: Addressable<Resolver: for<'a> Resolve<Args<'a> = ()>>> Singleton for T {}

/// Cardinality marker: many instances of this actor type can be live under a
/// resolver-selected scope, each under its own subname. `R::NAMESPACE` is a
/// **prefix** — the node's [`ActorId`] takes the form
/// `hash("{NAMESPACE}:{subname}")` (e.g. `aether.net.session:42`) before its
/// resolver places it in the mailbox lineage. The `:` separator is structural;
/// subnames may not contain it.
///
/// Forcing function is socket actors (ADR-0079): a singleton listener
/// (e.g. `NetCapability`) accepts connections and spawns one
/// `SessionActor` per accepted socket via `ctx.spawn_child`. Senders address
/// an instance by key through `ctx.resolve_actor::<R>(subname)`. That typed
/// route requires [`CallerAddressable`], selects
/// [`CallerScoped::SCOPE`] from the runtime context, then calls
/// `R::resolve(selected_mailbox.0, subname)`; it is not the flat string
/// addressing performed by `MailSender::send_to_named`.
///
/// Mutually exclusive with [`Singleton`] at the type level. ADR-0079.
/// Derived from the resolver (ADR-0119): a keyed [`Resolver`](Addressable::Resolver)
/// (`Args<'a> = &'a str` — [`Many`], or [`EmbeddedMany`] for spawned
/// siblings) makes the actor an `Instanced`, reached by
/// `ctx.resolve_actor::<R>(subname)`. The blanket impl supplies it; nobody
/// writes `impl Instanced`.
///
/// A singleton cannot use the keyed construction surface:
///
/// ```compile_fail
/// use aether_actor::{Addressable, One, WasmCtx};
///
/// struct RootCap;
/// impl Addressable for RootCap {
///     const NAMESPACE: &'static str = "example.root";
///     type Resolver = One;
/// }
///
/// fn keyed_singleton(ctx: &WasmCtx<'_>) {
///     let _ = ctx.resolve_actor::<RootCap>("instance");
/// }
/// ```
///
/// A keyed custom resolver must also declare a caller scope before a ctx can
/// select its routing seed:
///
/// ```compile_fail
/// use aether_actor::{Addressable, MailboxId, Resolve, WasmCtx};
///
/// struct UnscopedKeyed;
/// impl Resolve for UnscopedKeyed {
///     type Args<'a> = &'a str;
///
///     fn resolve(carry: u64, _namespace: &str, _key: &str) -> MailboxId {
///         MailboxId(carry)
///     }
/// }
///
/// struct Peer;
/// impl Addressable for Peer {
///     const NAMESPACE: &'static str = "example.peer";
///     type Resolver = UnscopedKeyed;
/// }
///
/// fn unscoped_keyed(ctx: &WasmCtx<'_>) {
///     let _ = ctx.resolve_actor::<Peer>("instance");
/// }
/// ```
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
    /// and be unique within the owning prefix (no structural `:` or `/`
    /// separator); names retire on drop (ADR-0079).
    Named(&'a str),
}

/// Validation outcome for namespace segments — both the `NAMESPACE`
/// const on an [`Instanced`] type (the listener prefix) and the
/// runtime subname passed to `spawn_child`. Same rules apply at both
/// sites: stay printable-ASCII-ish, don't collide with the structural
/// `:` / `/` separators, and stay under [`NAMESPACE_SEGMENT_MAX_LEN`] bytes.
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
/// - segments containing the structural `:` or `/` separators
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
        return Err(NamespaceError::TooLong { limit: NAMESPACE_SEGMENT_MAX_LEN });
    }
    for c in s.chars() {
        if matches!(c, ':' | '/') {
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
/// `ctx.actor::<R>()` / `ctx.resolve_actor::<R>(key)`) so the compiler
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
/// carries a boot lifecycle ([`Lifecycle<S>`](Lifecycle)) over a runtime
/// state `S`. The blanket impl supplies it for any type that is both, so
/// `WasmActor` / `NativeActor` implementors are `Actor<Self::State>`
/// automatically. Code that wants a fully-formed actor bounds `Actor<S>`;
/// code that wants only identity bounds `Addressable`.
pub trait Actor<S>: Addressable + Lifecycle<S> {}
impl<S, T: Addressable + Lifecycle<S>> Actor<S> for T {}

#[cfg(test)]
mod tests {
    // These tests assert the resolve/lineage machinery against the depth-1
    // name hash — the primitive is the reference value under test, not a
    // sibling-cap address.
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use aether_data::{fold_lineage, mailbox_id_from_name};

    #[test]
    fn namespace_segments_reject_structural_separators() {
        assert_eq!(validate_namespace_segment("bad:name"), Err(NamespaceError::ContainsSeparator),);
        assert_eq!(validate_namespace_segment("bad/name"), Err(NamespaceError::ContainsSeparator),);
        assert_eq!(validate_namespace_segment("aether.kit.camera"), Ok(()));
    }

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

    /// ADR-0119 amendments: every built-in strategy admitted to the typed send
    /// surface declares its caller-relative scope. Embedded singletons select
    /// the runtime parent; spawned embedded siblings still select the current
    /// spawner whose lineage they extend (ADR-0099 §Negative).
    #[test]
    fn caller_scoped_declares_each_admitted_strategys_scope() {
        fn requires_caller_addressable<T: CallerAddressable>() {}

        struct RootCap;
        impl Addressable for RootCap {
            const NAMESPACE: &'static str = "test.caller_scoped.root";
            type Resolver = One;
        }

        struct KeyedChild;
        impl Addressable for KeyedChild {
            const NAMESPACE: &'static str = "test.caller_scoped.child";
            type Resolver = Many;
        }

        struct SpawnedSibling;
        impl Addressable for SpawnedSibling {
            const NAMESPACE: &'static str = "test.caller_scoped.sibling";
            type Resolver = EmbeddedMany;
        }

        struct EmbeddedPeer;
        impl Addressable for EmbeddedPeer {
            const NAMESPACE: &'static str = "test.caller_scoped.embedded";
            type Resolver = Embedded;
        }

        requires_caller_addressable::<RootCap>();
        requires_caller_addressable::<KeyedChild>();
        requires_caller_addressable::<EmbeddedPeer>();
        requires_caller_addressable::<SpawnedSibling>();

        assert_eq!(<One as CallerScoped>::SCOPE, CallerScope::Root);
        assert_eq!(<Many as CallerScoped>::SCOPE, CallerScope::Current);
        assert_eq!(<Embedded as CallerScoped>::SCOPE, CallerScope::Parent);
        assert_eq!(<EmbeddedMany as CallerScoped>::SCOPE, CallerScope::Current);
    }

    #[test]
    fn caller_scope_selects_root_current_and_parent_mailboxes() {
        let current = MailboxId(0x4010);
        let parent = MailboxId(0x4020);

        assert_eq!(CallerScope::Root.select(current, parent), MailboxId::NONE);
        assert_eq!(CallerScope::Current.select(current, parent), current);
        assert_eq!(CallerScope::Parent.select(current, parent), parent);
    }

    /// `with_tag` replaces only the high nibble, while FNV-1a folding modulo
    /// 2^60 depends only on the seed modulo 2^60. Every possible raw high
    /// nibble must therefore produce the same child route as the tagged seed,
    /// and replacing the child's raw fold with its mailbox id must remain safe
    /// for the grandchild fold too.
    #[test]
    fn mailbox_ids_are_routing_equivalent_lineage_seeds() {
        let body = 0x0123_4567_89ab_cdef;
        let tagged_parent = with_tag(Tag::Mailbox, body);
        let child = ActorId::instanced("test.routing_seed.child", "one");
        let grandchild = ActorId::instanced("test.routing_seed.grandchild", "two");

        for high_nibble in 0_u64..16 {
            let raw_parent = (high_nibble << 60) | body;
            let raw_child = fold_lineage(raw_parent, child);
            let child_mailbox = with_tag(Tag::Mailbox, raw_child);

            assert_eq!(
                child_mailbox,
                with_tag(Tag::Mailbox, fold_lineage(tagged_parent, child)),
                "the parent carry's high nibble cannot affect the child route",
            );
            assert_eq!(
                with_tag(Tag::Mailbox, fold_lineage(raw_child, grandchild)),
                with_tag(Tag::Mailbox, fold_lineage(child_mailbox, grandchild)),
                "the tagged child mailbox must remain a valid grandchild seed",
            );
        }
    }

    #[test]
    fn placement_markers_are_independent_permissions() {
        struct Manager;
        impl Addressable for Manager {
            const NAMESPACE: &'static str = "test.placement.manager";
            type Resolver = One;
        }

        struct Worker;
        impl Addressable for Worker {
            const NAMESPACE: &'static str = "test.placement.worker";
            type Resolver = Many;
        }
        impl Root for Worker {}
        impl ChildOf<Manager> for Worker {}

        fn requires_root<T: Root>() {}
        fn requires_child<T: ChildOf<Manager>>() {}
        requires_root::<Worker>();
        requires_child::<Worker>();
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
        assert_eq!(<RootCap as Addressable>::resolve(0, ()), frozen, "One is the depth-1 id");
        assert_eq!(<RootCap as Addressable>::resolve(0xDEAD_BEEF, ()), frozen, "One ignores the caller's carry");
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
        let expected =
            MailboxId(with_tag(Tag::Mailbox, fold_lineage(carry, ActorId::instanced("test.resolve.per_thing", "42"))));
        assert_eq!(<PerThing as Addressable>::resolve(carry, "42"), expected, "folds carry+subname");
        assert_ne!(
            <PerThing as Addressable>::resolve(carry, "42"),
            <PerThing as Addressable>::resolve(carry, "43"),
            "different subnames resolve to different mailboxes"
        );
    }
}
