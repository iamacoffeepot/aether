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

mod contract;
pub mod ctx;
mod declared;
mod protocol;
mod publish;
mod sendable;
pub mod slot;

use aether_data::{ActorId, Kind, MailboxId, Tag, fold_lineage, with_tag};

pub(crate) use self::contract::WatchedRef;
pub use self::contract::{
    Contract, Contracts, ReplyShape, Silent, SilentRow, Undeclared, WatchTarget, Watchable, Watches,
};
pub use self::declared::{
    AllHandle, Declared, DependencyLink, DependencyList, Gap, Here, ListIndex, RowIndex, There, declared_dependencies,
};
#[doc(hidden)]
pub use self::declared::{dependency_records_len, write_dependency_records};
#[doc(hidden)]
pub use self::protocol::ProtocolCast;
pub use self::protocol::{Anyone, At, CastTarget, CoveredBy, CoversRows, Protocol, Row, RowAt, RowReply, RowSet};
pub use self::publish::{Publisher, Publishes, Subscriber};
pub use self::sendable::{SendableTo, SentBy};

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

    /// Fold an optional key to a resolution candidate (ADR-0230): the same
    /// derivation as `resolve`, entered through an `Option<&str>` key.
    /// Each strategy delegates to its own `resolve` and returns `None` for
    /// the key shape it cannot fold — never a fallback id.
    #[must_use]
    fn candidate(caller_carry: u64, namespace: &str, key: Option<&str>) -> Option<MailboxId>;
}

/// Root-pinned keyless resolution (ADR-0119): the depth-1 fixed point
/// (ADR-0099 §3), this actor's own [`ActorId`] tagged as a mailbox,
/// **ignoring the caller's carry** because a root cap sits at the root. It
/// equals `ActorId::singleton(NAMESPACE)` because [`with_tag`] is
/// idempotent on an already-`Mailbox`-tagged value, so every chassis cap
/// keeps the exact id it has today. Makes its actor a [`Singleton`].
pub struct One;

impl Resolve for One {
    type Args<'a> = ();
    fn resolve(_caller_carry: u64, namespace: &str, _args: ()) -> MailboxId {
        MailboxId(with_tag(Tag::Mailbox, ActorId::singleton(namespace).0))
    }
    fn candidate(caller_carry: u64, namespace: &str, key: Option<&str>) -> Option<MailboxId> {
        key.is_none().then(|| Self::resolve(caller_carry, namespace, ()))
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
    fn candidate(caller_carry: u64, namespace: &str, key: Option<&str>) -> Option<MailboxId> {
        key.map(|key| Self::resolve(caller_carry, namespace, key))
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
}

/// The caller carry handed to a resolver when the scope names no lineage.
///
/// A root-pinned resolver ignores its carry.
const EMPTY_CARRY: u64 = 0;

impl CallerScope {
    /// Select the caller carry this scope names.
    ///
    /// Root-pinned resolvers receive [`EMPTY_CARRY`] because they do not
    /// consume caller lineage; the current scope receives the calling actor's
    /// mailbox, so no separate untagged hash state is required.
    #[must_use]
    pub(crate) const fn select(self, current: MailboxId) -> u64 {
        match self {
            Self::Root => EMPTY_CARRY,
            Self::Current => current.0,
        }
    }
}

/// A [`Resolve`] strategy that declares which caller-relative scope its fold
/// consumes. Every ctx send selects [`Self::SCOPE`] rather than assuming that
/// all resolvers consume the calling actor's own lineage.
///
/// Implemented for the two built-in strategies:
///
/// - [`One`] declares [`CallerScope::Root`] and ignores caller lineage.
/// - [`Many`] declares [`CallerScope::Current`] for a keyed child of the
///   caller, native or spawned inline, whose lineage extends the caller's.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a caller-scoped resolution strategy",
    label = "does not declare a caller-relative scope for bare-type resolution",
    note = "a resolver used by a typed ctx must select `Root` or `Current`; use an explicit \
            by-name or by-id route when no caller-relative scope describes the target"
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

mod sealed {
    /// Private supertrait sealing [`super::DependencyResolver`] — only the
    /// keyless strategies in this module can implement it, so the set of
    /// declarable strategies is closed.
    pub trait Sealed {}
}

/// A [`Resolve`] strategy that may back a declared `#[actor(depends(R))]`
/// dependency (ADR-0230): the keyless strategies, whose candidate position
/// the host folds without run-time data. Sealed: the only implementor is
/// [`One`], a root singleton (ADR-0241 §5). A keyed strategy names an instance through
/// run-time data, so its actor is reached through the reference its spawn
/// returned rather than a declaration.
pub trait DependencyResolver: Resolve + sealed::Sealed {
    /// The wire tag `export!` writes, from the actor's
    /// [`Declared::Depends`] list, into the `InputsRecord::Dependency` record
    /// the host reader matches on, and the tag the native birth check folds
    /// by.
    /// Tags are stable: a new declarable strategy takes the next unused tag,
    /// never a retired one (tag 1 was the retired embedded resolver).
    const TAG: u8;
}

impl sealed::Sealed for One {}

impl DependencyResolver for One {
    const TAG: u8 = 0;
}

/// An actor that can be addressed by bare type from a peer's ctx, because its
/// [`Resolver`](Addressable::Resolver) is [`CallerScoped`] (ADR-0119
/// amendment). Bounds every flat typed send verb (`ctx.send::<R>` and its
/// batched / tracked / detached siblings) and `ctx.actor_ref::<R>()` beside
/// the cardinality marker.
///
/// Auto-implemented from the resolver with the constraint in **supertrait
/// position** so it elaborates to call sites, the same mechanism
/// [`Singleton`] / [`Instanced`] use. Nobody writes `impl CallerAddressable`.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be addressed by bare type from this context",
    label = "its resolver does not declare a caller-relative scope",
    note = "use an explicit by-name or by-id route when the target cannot select `Root` or \
            `Current` from the caller's runtime context"
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
    /// (ADR-0098). For a root singleton — every chassis capability, and a
    /// wasm component declaring `NAMESPACE = "aether.kit.camera"` — it is the
    /// full mailbox name. An instanced actor takes a key, `NAMESPACE:key`,
    /// and a child is `parent/NAMESPACE:key` (ADR-0241 §5): a guest is named
    /// by its own namespace, never by its host's.
    const NAMESPACE: &'static str;

    /// The resolution strategy this actor selects (ADR-0119). Cardinality
    /// is derived from it: a keyless resolver ([`One`], `Args<'a> = ()`)
    /// makes the actor a [`Singleton`]; a keyed resolver ([`Many`],
    /// `Args<'a> = &'a str`) makes it
    /// [`Instanced`]. The `#[actor]` macro emits this; a hand-written
    /// actor sets it directly.
    type Resolver: Resolve;

    /// This actor's [`MailboxId`] as seen by a caller whose lineage carry is
    /// `caller_carry` (ADR-0099 §5), produced by delegating to the selected
    /// [`Resolver`](Self::Resolver). Declared once here, never overridden —
    /// variation lives in the chosen resolver, not in this method (ADR-0119).
    /// `ctx.actor_ref::<R>()` calls this with `()`; a spawn places a keyed
    /// instance by calling it with the borrowed subname.
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

/// The mailbox of a root-pinned singleton, resolved with no caller in hand.
///
/// Boot and driver code addresses a chassis capability before any actor ctx
/// exists, so there is no flat verb to reach the target through. It gets
/// the same address anyway: [`One`] pins to the root and ignores the caller's
/// carry (ADR-0099 §3), so the seed is the empty carry and the answer is
/// the depth-1 fixed point — `resolve` stays the single derivation rather
/// than the caller re-deriving `hash(NAMESPACE)` beside it.
///
/// The bound is the point. `C: Root` says the identity may sit without an
/// actor parent, and `Resolver = One` says its address does not depend on
/// one. A [`Many`] actor — whose mailbox is knowable only
/// under some caller's lineage — is a compile error here rather than a
/// silently wrong depth-1 hash.
#[must_use]
pub fn root_mailbox<C: Root + Addressable<Resolver = One>>() -> MailboxId {
    C::resolve(EMPTY_CARRY, ())
}

/// Placement permission for an actor identity that may appear directly
/// beneath logical parent `P` (ADR-0166).
///
/// This describes a legal edge, not ownership, supervision, or liveness. A
/// child lists every parent it may sit beneath in one `child_of(A, B, ..)`
/// list on its `#[actor(..)]`, and may also be [`Root`] when both placements
/// are meaningful.
///
/// A hand-written impl does not compile (ADR-0231 §10). The actor's one
/// [`Declared`] impl lists its parents as [`Declared::Parents`], and each impl
/// names `P`'s position in that list as [`Index`](ChildOf::Index), so an impl
/// for an undeclared parent either repeats an emitted impl (`E0119`) or names
/// a position that holds another parent or none (`E0277`). The list is the
/// actor's whole placement set.
#[diagnostic::on_unimplemented(
    message = "`{Self}` does not declare `{P}` as a parent",
    label = "`{P}` is not in `{Self}`'s `child_of(..)`",
    note = "add `{P}` to the `child_of(..)` list on `{Self}`'s `#[actor(..)]`"
)]
pub trait ChildOf<P: Addressable>: Addressable + Declared {
    /// `P`'s position in [`Declared::Parents`], written by the expansion
    /// that declared it.
    #[doc(hidden)]
    type Index: ListIndex<<Self as Declared>::Parents, P>;
}

/// A declared dependency (ADR-0230): `Self: DependsOn<R>` means the actor
/// could not have been created before `R` was `Live`. It is checked where
/// the actor stands up (ADR-0241 §4): the host refuses the load, the module
/// boot actor, the republish that rebuilds a live instance, or the inline
/// spawn ([`SpawnError::DependencyNotLive`](crate::SpawnError::DependencyNotLive))
/// while `R` has no `Live` route, before `init`. Declared with
/// `#[actor(depends(R))]`.
///
/// Only keyless actors are declarable: `R: Singleton + CallerAddressable`
/// with a [`DependencyResolver`] strategy. A keyed actor cannot be a
/// declared dependency — which instance is meant is run-time data, and
/// that case is reached through the reference its spawn returned:
///
/// ```compile_fail,E0277
/// use aether_actor::{ActorInitError, Addressable, Mail, Many, WasmActor, WasmCtx, WasmInitCtx, actor};
///
/// struct Keyed;
///
/// impl Addressable for Keyed {
///     const NAMESPACE: &'static str = "example.keyed";
///     type Resolver = Many;
/// }
///
/// struct Dependent;
///
/// #[actor(depends(Keyed))]
/// impl WasmActor for Dependent {
///     const NAMESPACE: &'static str = "example.dependent";
///
///     fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
///         Ok(Self)
///     }
///
///     #[fallback]
///     fn fallback(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
/// }
/// ```
///
/// The sealed bound carries its own weight: a custom keyless strategy
/// satisfies `Singleton + CallerAddressable` and still fails, on
/// `DependencyResolver` alone:
///
/// ```compile_fail,E0277
/// use aether_actor::{
///     ActorInitError, Addressable, CallerScope, CallerScoped, Mail, MailboxId, Resolve, WasmActor, WasmCtx,
///     WasmInitCtx, actor,
/// };
///
/// struct CustomKeyless;
///
/// impl Resolve for CustomKeyless {
///     type Args<'a> = ();
///
///     fn candidate(_carry: u64, _namespace: &str, _key: Option<&str>) -> Option<MailboxId> {
///         None
///     }
///
///     fn resolve(_carry: u64, _namespace: &str, _key: ()) -> MailboxId {
///         unreachable!()
///     }
/// }
///
/// impl CallerScoped for CustomKeyless {
///     const SCOPE: CallerScope = CallerScope::Root;
/// }
///
/// struct Custom;
///
/// impl Addressable for Custom {
///     const NAMESPACE: &'static str = "example.custom";
///     type Resolver = CustomKeyless;
/// }
///
/// struct Dependent;
///
/// #[actor(depends(Custom))]
/// impl WasmActor for Dependent {
///     const NAMESPACE: &'static str = "example.dependent";
///
///     fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
///         Ok(Self)
///     }
///
///     #[fallback]
///     fn fallback(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
/// }
/// ```
///
/// A hand-written impl does not compile (ADR-0231 §10). The actor's one
/// [`Declared`] impl lists its dependencies as [`Declared::Depends`], and the
/// pre-`init` check reads that list: the native birth check walks it, and on
/// wasm `export!` writes an `InputsRecord::Dependency` from each entry into the
/// inputs section the host reads. Each impl names `R`'s position in that list
/// as [`Index`](DependsOn::Index), so an impl for an undeclared `R` either
/// repeats an emitted impl (`E0119`) or names a position that holds another
/// dependency or none (`E0277`), and no [`ActorRef<R>`](crate::ActorRef) proof
/// is minted for an actor whose birth never checked that `R` was `Live`.
#[diagnostic::on_unimplemented(
    message = "`{Self}` does not declare a dependency on `{R}`",
    note = "add `{R}` to the `depends(...)` list on the actor's `#[actor(...)]` attribute"
)]
pub trait DependsOn<R: Singleton + CallerAddressable>: Addressable + Declared
where
    R::Resolver: DependencyResolver,
{
    /// `R`'s position in [`Declared::Depends`], written by the expansion
    /// that declared it.
    #[doc(hidden)]
    type Index: ListIndex<<Self as Declared>::Depends, R>;
}

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
/// `type InitCtx<'a> = WasmInitCtx<'a>; type Ctx<'a> = WasmCtx<'a, Self>;` (or
/// the native pair, `NativeCtx<'a, Self>`), so a `wire`/`unwire` hook receives
/// a ctx typed by its actor and reaches the concrete ctx's inherent methods
/// (`ctx.send::<R>(&p)`) with no generic bound at the call site. `InitError` is pinned per transport subtrait
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

    /// The error either birth hook, [`Self::init`] or [`Self::wire`], returns
    /// when the actor cannot start. Pinned to the concrete boot error on each
    /// transport subtrait. Whoever asked for the birth is told it.
    type InitError;

    /// The per-target init ctx (`WasmInitCtx<'a>` / `NativeInitCtx<'a>`),
    /// synthesized per impl by `#[actor]`.
    type InitCtx<'a>;

    /// The per-target post-init ctx, typed by the actor (`WasmCtx<'a, Self>` /
    /// `NativeCtx<'a, Self>`), synthesized per impl by `#[actor]`.
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
    /// Defaults to `Ok(())`; override to register subscriptions or announce.
    ///
    /// An `Err` fails the birth as an `Err` from [`Self::init`] does
    /// (ADR-0247 rule 3): whoever asked for the actor is answered with the
    /// error and the actor never goes live. The hook was entered, so
    /// [`Self::unwire`] still runs before the actor drops, and no mail the
    /// hook sent leaves a birth that holds its outbound mail.
    ///
    /// # Errors
    /// Whatever the actor could not set up, as its transport's
    /// [`InitError`](Self::InitError).
    fn wire(state: &mut S, ctx: &mut Self::Ctx<'_>) -> Result<(), Self::InitError> {
        let _ = (state, ctx);
        Ok(())
    }

    /// Pre-shutdown, mail-allowed hook (ADR-0079). Runs after the inbox
    /// drain, before the actor value drops. Default no-op. It returns
    /// nothing: a close has no asker to tell and runs to its end.
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
/// declares the root cap as a dependency and mails it by type with
/// `ctx.send::<R>(&k)`. A loaded singleton component is a root singleton too
/// (ADR-0241 §5): `ctx.actor_ref::<R>()` folds its published name
/// (`R::NAMESPACE`) from the root, as it does for a chassis cap.
///
/// The rendered address itself — `LoadResult.path`, e.g. `aether.kit.camera`
/// — is a boundary string: the host's
/// `resolve_address` parser at the MCP, RPC, and harness boundary is the one
/// place text becomes a position (ADR-0230). A load requester needs no
/// resolution at all: the load reply arrives from the loaded actor, so the
/// requester keeps the reply's stamped sender as its reference.
///
/// Mutually exclusive with [`Instanced`] at the type level: an actor is
/// either one-of-a-kind within a scope (singleton) or N-instances under
/// a shared prefix (instanced, name-keyed). ADR-0079.
/// Derived from the resolver (ADR-0119): a keyless [`Resolver`](Addressable::Resolver)
/// (`Args<'a> = ()` — [`One`], for root caps and components alike) makes the
/// actor a `Singleton`. The blanket impl supplies it; nobody writes
/// `impl Singleton`. Typed addressing also asks for [`CallerAddressable`],
/// which [`One`] satisfies by selecting [`CallerScope::Root`].
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
/// `SessionActor` per accepted socket via `ctx.spawn_child`. Senders reach
/// an instance through the reference its spawn returned, or through a
/// `child_as` lookup; text never names one (ADR-0230).
///
/// Mutually exclusive with [`Singleton`] at the type level. ADR-0079.
/// Derived from the resolver (ADR-0119): a keyed [`Resolver`](Addressable::Resolver)
/// (`Args<'a> = &'a str` — [`Many`], native or guest) makes the actor an
/// `Instanced`, reached through the
/// reference its spawn returned. The blanket impl supplies it; nobody writes
/// `impl Instanced`.
pub trait Instanced: Addressable<Resolver: for<'a> Resolve<Args<'a> = &'a str>> {}
impl<T: Addressable<Resolver: for<'a> Resolve<Args<'a> = &'a str>>> Instanced for T {}

/// How a spawned child's mailbox subname is derived (ADR-0079). The
/// full mailbox name is `"{A::NAMESPACE}:{subname}"`; the substrate
/// hashes that string deterministically (ADR-0029) to the returned
/// `MailboxId`. Shared spawn-addressing vocabulary: native
/// `spawn_child` and the FFI guest's inline spawn verbs
/// (`WasmCtx::spawn_inline_child`, ADR-0114) both name children through
/// it, so the two transports name children the same way.
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
/// handler kind. An actor's author never writes one by hand; a hand-written
/// impl is test or harness scaffolding for a target with no `#[actor]` block,
/// and it names [`Anyone`] as its [`Sender`](HandlesKind::Sender).
///
/// Gates [`SendableTo<R>`] on the flat typed send verbs (`ctx.send::<R>`)
/// and [`Target<K>`](crate::Target) on [`ActorRef<R>`](crate::ActorRef)
/// (`ctx.send_to(&reference, &k)`), so the compiler rejects sends to a kind
/// the receiver doesn't handle.
/// The single source of truth is the handler list on the actor's
/// `impl` block; adding a `#[handler]` updates senders' compile-time
/// checks automatically. ADR-0075 §Decision 1.
///
/// Blanket impls (e.g. `impl<T: Into<DrawTriangle>> HandlesKind<T> for
/// RenderCapability`) are an opt-in extension if a real conversion case
/// wants them; the default macro emission is strict so wire bytes stay
/// obvious.
#[diagnostic::on_unimplemented(
    message = "`{Self}` has no handler for `{K}`",
    label = "`{Self}` does not handle `{K}`",
    note = "a `#[fallback]` does not count as handling a kind"
)]
pub trait HandlesKind<K: Kind>: Addressable {
    /// What the handler requires of the actor that sends it `K`
    /// (ADR-0231 §11): the protocol the sender must cover, or [`Anyone`] when
    /// it asks nothing.
    ///
    /// `#[actor]` reads it from the handler's signature. A `#[handler::tell]`
    /// or `#[handler::request]` that takes a fourth parameter
    /// `sender: ProtocolRef<P>` requires `P`, and every other handler requires
    /// [`Anyone`]. That one parameter drives two checks, so they cannot
    /// drift. Every typed send verb bounds the sending actor against this
    /// type ([`SentBy`]), so an actor that lacks one of `P`'s handlers cannot
    /// build the send. And the handler's dispatch arm casts the mail's sender
    /// to `P` before the handler runs and hands it the proven reference, so
    /// mail from a route the build cannot see, such as a relayed call or mail
    /// with no sender, is refused and never reaches the handler.
    type Sender: Protocol;
}

/// Per-handler reply marker: `R: Replies<K, Reply = O>` means actor `R`
/// accepts `K` and its single-reply handler returns kind `O`.
#[diagnostic::on_unimplemented(
    message = "`{Self}` does not reply to `{K}` with a single typed kind",
    label = "this reply bound is not satisfied",
    note = "the handler may be silent, unchecked, or reply with a different kind"
)]
pub trait Replies<K: Kind>: HandlesKind<K> {
    type Reply: Kind;
}

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
    use super::*;
    use aether_data::fold_lineage;

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
    /// surface declares its caller-relative scope: a root singleton selects
    /// no lineage, and a keyed child extends its caller's (ADR-0099 §Negative).
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

        requires_caller_addressable::<RootCap>();
        requires_caller_addressable::<KeyedChild>();

        assert_eq!(<One as CallerScoped>::SCOPE, CallerScope::Root);
        assert_eq!(<Many as CallerScoped>::SCOPE, CallerScope::Current);
    }

    #[test]
    fn caller_scope_selects_the_current_mailbox_or_no_lineage() {
        let current = MailboxId(0x4010);

        assert_eq!(CallerScope::Current.select(current), current.0);
        assert_eq!(CallerScope::Root.select(current), EMPTY_CARRY);
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
        impl Declared for Worker {
            type Depends = ();
            type Spawns = ();
            type Parents = (Manager, ());
        }
        impl ChildOf<Manager> for Worker {
            type Index = Here;
        }

        fn requires_root<T: Root>() {}
        fn requires_child<T: ChildOf<Manager>>() {}
        requires_root::<Worker>();
        requires_child::<Worker>();
    }

    /// ADR-0099 §5 / ADR-0119: the [`One`] resolver ignores the caller's
    /// carry and returns the depth-1 fixed point — the actor's own
    /// `ActorId::singleton(NAMESPACE)`, so the chassis-cap vocabulary stays
    /// frozen (§3).
    #[test]
    fn one_resolver_is_frozen_depth_one() {
        struct RootCap;
        impl Addressable for RootCap {
            const NAMESPACE: &'static str = "test.resolve.rootcap";
            type Resolver = One;
        }

        let frozen = ActorId::singleton("test.resolve.rootcap").0;
        assert_eq!(<RootCap as Addressable>::resolve(0, ()).0, frozen, "One is the depth-1 id");
        assert_eq!(<RootCap as Addressable>::resolve(0xDEAD_BEEF, ()).0, frozen, "One ignores the caller's carry");
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

    /// ADR-0230: `candidate` is the same derivation as `resolve`, entered
    /// through the address key shape. Each strategy agrees with its own
    /// `resolve` for the matching shape.
    #[test]
    fn candidate_matches_resolve_for_matching_key_shape() {
        let carry = 0x0BAD_F00D_u64;

        assert_eq!(
            One::candidate(carry, "test.candidate.root", None),
            Some(One::resolve(carry, "test.candidate.root", ())),
            "a keyless strategy folds a keyless address",
        );
        assert_eq!(
            Many::candidate(carry, "test.candidate.child", Some("one")),
            Some(Many::resolve(carry, "test.candidate.child", "one")),
            "a keyed strategy folds the carried key",
        );
    }

    /// ADR-0230: a key of the wrong shape for the strategy is `None`,
    /// never a fallback id.
    #[test]
    fn candidate_rejects_mismatched_key_shape() {
        let carry = 0x0BAD_F00D_u64;

        assert_eq!(One::candidate(carry, "test.candidate.root", Some("one")), None, "One takes no key");
        assert_eq!(Many::candidate(carry, "test.candidate.child", None), None, "Many requires a key");
    }
}
