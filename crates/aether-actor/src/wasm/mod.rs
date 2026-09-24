//! Wasm guest binding layer — the SDK a wasm32 actor compiles against
//! to speak the `_p32`-suffixed host-fn import surface the substrate's
//! wasm runtime provides.
//!
//! Surface:
//!
//!   - `raw` — `extern "C"` host-fn imports + host-target panic
//!     stubs (the only place the `_p32` symbols are named). These are
//!     the literal FFI boundary: ABI names (`init`, `receive_p32`,
//!     `_p32` suffix, `aether.kinds.inputs`, `aether.namespace`) are
//!     an on-the-wire contract the substrate's wasm runtime expects and
//!     are deliberately unchanged. The module is private to `wasm`, so a
//!     guest reaches the host only through `bridge` and the ctx verbs,
//!     never through a bare import.
//!   - [`bridge`] — per-concern free-function modules (`bridge::log`,
//!     `bridge::mail`, `bridge::persist`). Each module owns one FFI op
//!     family and forwards calls to the matching `raw::*` host fn.
//!   - [`WasmInitCtx`] / [`WasmCtx`] / [`WasmDropCtx`] — concrete per-stage
//!     ctx structs, each impling the relevant subset of the per-stage
//!     capability traits in [`crate::model::ctx`].
//!   - [`WasmActorMailbox<R>`] — actor-typed sender returned by
//!     `ctx.actor::<R>()`, plus
//!     [`WasmActorMailboxWithContext`] for a typed request context bound to
//!     subsequent sends.
//!   - [`WasmActor`] trait — entry point with the `init` constructor and
//!     the `wire` / `unwire` / `on_dehydrate` / `on_rehydrate` lifecycle
//!     hooks (ADR-0101). `init` returns `Result<Self, ActorInitError>` so a
//!     guest can surface its own error message instead of the panic-hook
//!     path's generic "guest trapped during init" text.
//!   - [`crate::export!`] — `#[no_mangle]` `init` / `receive` /
//!     lifecycle shims plus the `aether.kinds.inputs` /
//!     `aether.namespace` custom-section pins.
//!
//! No FFI imports are pulled in unconditionally — the host-fn externs
//! in `raw` live behind a `#[cfg(target_family = "wasm")]` block and
//! the native-target stubs panic if invoked, so the crate compiles
//! for `cargo test --workspace` on the host without dragging the FFI
//! surface into the linker.
//!
//! ADR coverage: ADR-0012 (typed sinks), ADR-0013 (reply-to-sender),
//! ADR-0014 (Component trait + Mail), ADR-0015 (lifecycle hooks),
//! ADR-0016 (state-across-replace), ADR-0024 (`_p32` FFI),
//! ADR-0030 (compile-time kind ids), ADR-0033 (`#[actor]`), ADR-0040
//! (kind-typed state), ADR-0041 (file I/O), ADR-0043 (HTTP egress),
//! ADR-0045 (typed handles), ADR-0058 (`aether.sink.*` namespace),
//! ADR-0060 (tracing→mail bridge), ADR-0074 (unified actor model).

use alloc::borrow::Cow;
use alloc::string::String;

use core::fmt;

pub mod bridge;
pub mod ctx;
pub mod inline;
pub mod mailbox;
mod raw;

// Re-exports of `Wasm*` types — the `Wasm` prefix is deliberate (native/wasm split);
// allows mirror the def-site allows on each type.
#[allow(clippy::module_name_repetitions)]
pub use ctx::{
    ActorTypeTag, InlineChild, NO_INBOUND_SOURCE, RelativeMailbox, Sends, SpawnError, WasmCtx, WasmDropCtx,
    WasmInitCtx, WireCtx,
};
#[allow(clippy::module_name_repetitions)]
pub use mailbox::{WasmActorMailbox, WasmActorMailboxWithContext};

/// Error returned by [`Lifecycle::init`](crate::Lifecycle::init) when the actor cannot start
/// (config parse failure, required handle missing, malformed env var).
/// The message rides the `init_failed_p32` host fn into the substrate,
/// which surfaces it in `LoadResult::Err { error }` instead of the
/// panic-hook path's generic "guest trapped during init" text.
///
/// Wraps a `Cow<'static, str>` so static-string callers don't allocate
/// (`ActorInitError::from("config missing")`) while owned strings still flow
/// through (`ActorInitError::from(format!("..."))`).
#[derive(Debug, Clone)]
pub struct ActorInitError {
    message: Cow<'static, str>,
}

impl ActorInitError {
    /// Construct a `ActorInitError` from anything convertible to a
    /// `Cow<'static, str>` — `&'static str` for compile-time messages,
    /// `String` for `format!`-built diagnostics.
    pub fn new<S: Into<Cow<'static, str>>>(message: S) -> Self {
        Self { message: message.into() }
    }

    /// Borrow the error text. Used by the [`crate::export!`] shim to
    /// copy bytes into the substrate via `init_failed_p32`.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ActorInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<&'static str> for ActorInitError {
    fn from(s: &'static str) -> Self {
        Self::new(s)
    }
}

impl From<String> for ActorInitError {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

/// User-implemented FFI actor — typically a wasm component. ADR-0014
/// commits to `Self`-is-state: cached kind ids, cached sinks, and any
/// domain fields live on the implementor. `init` runs once before any
/// `receive`; receive is driven by the synthesised `__aether_dispatch`
/// from `#[actor]`.
///
/// The boot lifecycle (`init` / `wire` / `unwire`, plus `type Config`)
/// lives on the shared [`crate::Lifecycle`] capability; `WasmActor`
/// composes it alongside the identity [`crate::Addressable`] supertrait and
/// adds only the FFI-specific hot-swap surface (`type State`,
/// `on_dehydrate`, `on_rehydrate`, ADR-0101). `InitError` is pinned to
/// [`ActorInitError`] so a guest surfaces its own message in
/// `LoadResult::Err { error }`; `Self::Config` and the ADR-0156
/// `Self::Params` are both tightened to [`Kind`](aether_data::Kind) +
/// [`Default`] — FFI config and params cross the wasm boundary as encoded
/// bytes, and an empty slice resolves to the guest's compiled default.
///
/// The `#[no_mangle]` `init` / `receive` exports that actually cross
/// the FFI boundary are generated by `export!(public = [MyComponent])`;
/// implementors do not write `extern "C"` by hand.
/// Per-kind dispatch over a runtime state `S` — the wasm counterpart of the
/// native `Dispatch<S>` (iamacoffeepot/aether#2311). The `#[actor]` macro
/// implements it on the addressing identity, forwarding to the inherent
/// `__aether_dispatch` demux table; for an un-split component `S = Self`, so
/// `&mut S == &mut self`.
// The `Wasm` prefix is the deliberate native-vs-wasm disambiguator; public SDK trait.
#[allow(clippy::module_name_repetitions)]
pub trait WasmDispatch<S> {
    /// Route one inbound mail to the matching `#[handler]` over the state.
    /// Returns the dispatch result code the `receive` FFI shim relays.
    /// ADR-0112: the seam carries the most-permissive [`Manual`](crate::Manual)
    /// view; the synthesized dispatcher downgrades per handler class.
    fn dispatch(state: &mut S, ctx: &mut WasmCtx<'_, crate::Erased, crate::Manual>, mail: crate::Mail<'_>) -> u32;
}

// Bare `Actor` collides with `model::Actor`; the `Wasm` prefix is the deliberate native-vs-wasm disambiguator.
#[allow(clippy::module_name_repetitions)]
pub trait WasmActor:
    crate::Addressable
    + for<'a> crate::Lifecycle<
        Self::State,
        InitError = ActorInitError,
        Config: aether_data::Kind + Default,
        Params: aether_data::Kind + Default,
        InitCtx<'a> = WasmInitCtx<'a>,
        Ctx<'a> = WasmCtx<'a, Self>,
    > + WasmDispatch<Self::State>
{
    /// The runtime state this identity boots into (iamacoffeepot/aether#2311)
    /// — **plain data**, bounded only by `Send + 'static`, implementing no
    /// behaviour trait. For an un-split component `State = Self` (the identity
    /// IS its own runtime), synthesized by the `#[actor]` macro; only a
    /// deliberately-split component points it at a dedicated plain `struct`.
    type State: Send + 'static;

    /// ADR-0113 kind-typed persistent state: the durable shape the
    /// actor carries across a `replace_component` swap. Declaring it
    /// (beside a `dehydrate` / `rehydrate` accessor pair) lets the
    /// `#[actor]` macro generate the [`Self::on_dehydrate`] /
    /// [`Self::on_rehydrate`] hooks instead of the author hand-writing
    /// them — the save side snapshots `Persist` and frames it via
    /// `save_state_kind`, the restore side decodes it via
    /// [`PriorState::decode_kind`][crate::PriorState::decode_kind] and boots
    /// fresh (with a `tracing::warn!`) when a reshaped `Persist` kind no
    /// longer decodes.
    ///
    /// Named `Persist` (not `State`) since iamacoffeepot/aether#2311 took the
    /// `State` name for the runtime state above; the authoring keyword stays
    /// `type State = …` (the macro routes it to this slot). Mirrors
    /// [`Lifecycle::Config`](crate::Lifecycle::Config): the `#[actor]` macro
    /// synthesizes `type Persist = ();` when the author omits it, so a
    /// no-persistence actor is unchanged and pays nothing (stable Rust has no
    /// associated-type defaults — rust-lang/rust#29661 — so the synthesis
    /// stands in). Distinct from `Self` because the durable fields are a
    /// subset of the actor's state: the handle ids `init` rebuilds are
    /// intentionally excluded (ADR-0113).
    type Persist: aether_data::Kind;

    /// Save-side hot-swap hook (ADR-0040 / ADR-0101). Runs once on the
    /// old instance immediately before a `replace_component` swap, after
    /// [`Lifecycle::unwire`](crate::Lifecycle::unwire). Default no-op; override to serialize state the
    /// replacement instance recovers through [`Self::on_rehydrate`].
    /// Prefer
    /// [`WasmDropCtx::save_state_kind`][crate::model::ctx::Persistence::save_state_kind]
    /// to let the kind system carry schema identity; reach for the raw
    /// [`WasmDropCtx::save_state`][crate::model::ctx::Persistence::save_state]
    /// only when persisting a non-kind blob or driving an explicit
    /// migration off the leading id.
    ///
    /// Concrete `&mut WasmDropCtx<'_>` — the ctx that carries
    /// `Persistence::save_state` and outbound mail, with the reply /
    /// resolve surfaces intentionally absent.
    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) {
        let _ = ctx;
    }

    /// Restore-side hot-swap hook (ADR-0040 / ADR-0101). Runs after
    /// [`Lifecycle::init`](crate::Lifecycle::init) on a freshly-instantiated replacement, if and only
    /// if the predecessor produced a state bundle via
    /// [`Self::on_dehydrate`] (the substrate skips the call when no
    /// bundle was saved — ADR-0016 §3). Default ignores the prior state;
    /// override to rehydrate from `prior` (typically
    /// [`PriorState::decode_kind`][crate::PriorState::decode_kind]).
    ///
    /// Concrete `&mut WasmCtx<'_, Self>` — the post-init send surface, typed
    /// by the actor like a handler's ctx (ADR-0231 §7), so an override can
    /// both restore fields and emit mail to its declared dependencies. Inside
    /// `#[actor]` an override may write `WasmCtx<'_>`, which the macro types
    /// by the actor, or `WasmCtx<'_, Erased>` to receive the erased view.
    fn on_rehydrate(&mut self, ctx: &mut WasmCtx<'_, Self>, prior: crate::PriorState<'_>) {
        let _ = ctx;
        let _ = prior;
    }
}

/// Placement permission for a reusable instanced Wasm actor that may appear
/// beneath any [`WasmActor`] parent exported from the same resident module
/// (ADR-0166).
///
/// This is a logical module-local composition permission. It does not permit
/// native placement or cross-module loading, and it carries no ownership,
/// supervision, liveness, or isolation semantics.
pub trait ModuleChild: WasmActor + crate::Instanced {}

impl<P, C> crate::ChildOf<P> for C
where
    P: WasmActor,
    C: ModuleChild,
{
}

/// A type the module `M`'s `export!` lists, exported or under
/// `private = [..]`, so a `replace_component` swap can rebuild it as an
/// inline child (ADR-0114 §5).
///
/// `export!` declares a hidden module type, `__AetherModule`, and implements
/// `Rebuildable<__AetherModule>` for every type it lists. The parameter is
/// local to the listing crate, so the impl is legal for a type from another
/// crate as well (a re-exported actor such as ADR-0137's behavior host). The
/// listed types are the set the rehydrate shim rebuilds.
///
/// Every `export!` also proves it lists each inline child a listed type
/// declares in `#[actor(spawns(..))]` ([`Spawns`]): it calls each listed
/// type's hidden `__aether_listed_children::<__AetherModule>()`, whose where
/// clause requires `Rebuildable<__AetherModule>` for every declared child. So
/// the types a module can spawn inline and the types a replace rebuilds are
/// one set by construction.
///
/// # Safety
///
/// Implement it only through `export!`. A hand-written impl lets the
/// coverage check pass for a child the module's rebuild arm does not list, so
/// the next replace drops it while its alias survives and the parent answers
/// its mail.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is spawned inline by an actor this `export!` lists, but this `export!` does not list it",
    label = "not listed by this module's `export!`",
    note = "list it under `private = [{Self}]` in this `export!` so a replace can rebuild it, or export it"
)]
pub unsafe trait Rebuildable<M> {}

/// The spawner `Self` declares `C` as an inline child in
/// `#[actor(spawns(C, …))]` (ADR-0114 §5).
///
/// `#[actor]` emits it for each declared child. The typed inline spawn verbs
/// [`WasmCtx::spawn_inline_child`] and [`WasmCtx::spawn_inline`] require it
/// of the spawning actor, and every `export!` that lists the spawner checks
/// that it also lists each declared child ([`Rebuildable`]). A generic helper
/// that forwards to either verb repeats the bound.
///
/// # Safety
///
/// Implement it only through `#[actor]`, which emits it together with the
/// matching bound the `export!` coverage check reads. A hand-written impl lets
/// a spawn skip that check, so a replace drops a child no `export!` lists.
#[diagnostic::on_unimplemented(
    message = "`{Self}` spawns `{C}` inline but does not declare it",
    label = "`{C}` is not in `{Self}`'s `spawns(..)`",
    note = "add `{C}` to `spawns(..)` in `{Self}`'s `#[actor(..)]`"
)]
pub unsafe trait Spawns<C> {}

/// Macro-generated placement facts for one exported Wasm actor.
///
/// This descriptor is an immutable companion to the actor-lineage custom
/// section. Later module-local spawn validation can consult it directly
/// without reflecting over marker traits or decoding custom-section bytes.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::module_name_repetitions)]
pub struct WasmPlacementFacts {
    /// Whether the generated resolver gives the actor instanced cardinality.
    pub is_instanced: bool,
    /// Whether the actor carries the module-local [`ModuleChild`] permission.
    pub module_child: bool,
    /// Exact declared parent actor tags from `child_of(...)`.
    pub exact_parent_tags: &'static [ActorTypeTag],
}

/// Object-safe erasure over a guest [`WasmActor`]'s post-construction
/// surface (ADR-0096). A multi-actor module — `export!(public = [A, B, …])` —
/// holds whichever exported type a given instance became in one
/// `Slot<Box<dyn ErasedWasmActor>>`, and the FFI shims route mail and
/// lifecycle calls through this trait. `#[actor]` emits the impl per
/// type, forwarding to the inherent `__aether_dispatch` and the
/// `WasmActor` lifecycle hooks.
///
/// `init` is deliberately not erased: it is generic over the ctx and
/// returns `Self`, so it cannot be a trait-object method. The
/// `export!` multi-actor arm matches the inbound actor-type tag against
/// each exported type and calls the concrete `T::init` before boxing
/// the result as a `dyn ErasedWasmActor`.
///
/// The hot-swap hooks erase the same way (ADR-0101), so a boxed
/// multi-actor instance preserves state across `replace_component`
/// with no multi-actor-specific machinery.
pub trait ErasedWasmActor {
    /// The actor type's [`crate::Addressable::NAMESPACE`], so the `receive`
    /// shim can derive the instance's own mailbox id for self-addressing.
    fn erased_namespace(&self) -> &'static str;

    /// Forwards to the `#[actor]`-synthesized `__aether_dispatch`.
    /// ADR-0112: the object-safe seam carries the most-permissive
    /// [`Manual`](crate::Manual) view; the synthesized dispatcher
    /// downgrades per handler class.
    fn erased_dispatch(&mut self, ctx: &mut WasmCtx<'_, crate::Erased, crate::Manual>, mail: crate::Mail<'_>) -> u32;

    /// Forwards to [`Lifecycle::wire`](crate::Lifecycle::wire). The synthesized
    /// impl upgrades the carried erased ctx to the actor, whose lifecycle ctx
    /// is typed by it, and downgrades the [`Manual`](crate::Manual) view to
    /// `Single`.
    fn erased_wire(&mut self, ctx: &mut WasmCtx<'_, crate::Erased, crate::Manual>);

    /// Forwards to [`Lifecycle::unwire`](crate::Lifecycle::unwire), upgrading
    /// the ctx the same way as [`Self::erased_wire`].
    fn erased_unwire(&mut self, ctx: &mut WasmCtx<'_, crate::Erased, crate::Manual>);

    /// Forwards to [`WasmActor::on_dehydrate`].
    fn erased_on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>);

    /// Forwards to [`WasmActor::on_rehydrate`], upgrading the ctx the same way
    /// as [`Self::erased_wire`].
    fn erased_on_rehydrate(
        &mut self,
        ctx: &mut WasmCtx<'_, crate::Erased, crate::Manual>,
        prior: crate::PriorState<'_>,
    );
}

/// Stage a guest init-failure message into the substrate via
/// `init_failed_p32` (ADR-0096). Shared by the single- and multi-actor
/// `export!` init shims so the byte-staging boilerplate isn't repeated at
/// each construction site, and so no expansion names the private `raw`
/// module. wasm32-only — the host build carries no FFI surface.
#[cfg(target_family = "wasm")]
#[doc(hidden)]
pub fn stage_init_failure(message: &str) {
    let bytes = message.as_bytes();
    // SAFETY: `init_failed` copies `len` bytes from `ptr` into the
    // substrate synchronously; the borrowed slice outlives the call.
    unsafe {
        raw::init_failed(bytes.as_ptr().addr() as u32, bytes.len() as u32);
    }
}

/// Guest-runtime log install (wasm32). Wires the FFI sink
/// (`bridge::log::emit_log_event`) into the target-blind
/// [`crate::log`] forwarding seam, then sets the forwarding subscriber
/// as `tracing`'s global default. Called from the `export!` prologue
/// before the guest's `init` runs. This is the only wasm-specific log
/// glue — `bridge::log` is `pub(crate)`, so the sink can only be wired
/// from inside the crate; the subscriber itself stays target-blind in
/// [`crate::log`]. Shared by the `export!` shims so the wiring isn't
/// repeated per init shim.
#[cfg(target_family = "wasm")]
#[doc(hidden)]
pub fn install_guest_logging() {
    crate::log::install_log_sink(bridge::log::emit_log_event);
    crate::log::install_forwarding_subscriber();
}

/// Validate an export-selected inline actor before its alias is allocated.
/// Membership is established by the generated resolver branch that calls
/// this helper; cardinality and exact-or-module-child placement come from the
/// selected actor's generated [`WasmPlacementFacts`].
#[doc(hidden)]
pub fn __validate_inline_child_placement(
    registry: &inline::Registry,
    parent: u64,
    child: ActorTypeTag,
    facts: WasmPlacementFacts,
) -> Result<(), SpawnError> {
    let parent = registry
        .actor_type_tag(aether_data::MailboxId(parent))
        .ok_or(SpawnError::ParentIdentityUnavailable(aether_data::MailboxId(parent)))?;
    if !facts.is_instanced {
        return Err(SpawnError::ActorNotInstanced(child));
    }
    if !facts.module_child && !facts.exact_parent_tags.contains(&parent) {
        return Err(SpawnError::PlacementDenied { parent, child });
    }
    Ok(())
}

/// Validate the raw alias a host allocated for an inline child. A zero alias
/// is the host's failure sentinel, not an address that can enter the inline
/// registry, so validation occurs before configuration decode or child init.
#[cfg(any(target_family = "wasm", test))]
pub(crate) fn __validate_inline_child_alias(alias: u64) -> Result<aether_data::MailboxId, SpawnError> {
    use core::num::NonZeroU64;

    NonZeroU64::new(alias).map(|alias| aether_data::MailboxId(alias.get())).ok_or(SpawnError::AliasAllocationFailed)
}

#[cfg(target_family = "wasm")]
#[doc(hidden)]
pub fn __alloc_inline_child_alias(
    parent: u64,
    is_counter: bool,
    subname: &str,
) -> Result<aether_data::MailboxId, SpawnError> {
    __validate_inline_child_alias(bridge::mail::spawn_inline_child_scoped(parent, is_counter, subname))
}

pub mod guest_alloc;

/// Bind a `WasmActor` implementor to the guest's `#[no_mangle]`
/// `init` / `receive` exports. Expands to:
///
/// - A `static` [`crate::Slot<T>`] that backs the actor instance.
/// - `extern "C" fn init(mailbox_id: u64) -> u32` — builds an
///   [`WasmInitCtx`], calls `T::init`, stashes the result in the slot.
/// - `extern "C" fn receive(kind, ptr, byte_len, count, sender, recipient)
///   -> u32` — builds [`WasmCtx`] and [`crate::Mail`], calls the
///   `#[actor]`-synthesized `__aether_dispatch` on the stashed
///   instance.
/// - `#[link_section = "aether.kinds.inputs"]` static that pins the
///   actor's handler manifest into the cdylib's wasm custom section
///   the substrate reads at `load_component`.
/// - `#[link_section = "aether.namespace"]` static that pins the
///   actor's `Addressable::NAMESPACE` bytes (issue 525 Phase 1B).
///
/// Every entry is keyed, keys come in any order, and each key appears at most
/// once:
///
/// | Key | Takes | Meaning |
/// |---|---|---|
/// | `public = [A, B, …]` | one or more actor types | exported: loadable by an export selector and spawnable by runtime tag (ADR-0096) |
/// | `default = A` | one actor type | exported, and the type a load with no selector instantiates (ADR-0138 / ADR-0147) |
/// | `boot = B` | one actor type | exported, and instantiated once on every load of the module, whatever selector the caller names; never selectable (ADR-0147) |
/// | `private = [C, …]` | one or more actor types | inline children the module rebuilds on a replace but does not export (ADR-0114 §5) |
/// | `generators = [g, …]` | one or more generator macro paths | export generators, such as `aether_bloomery_bundle::bundle` (ADR-0224) |
///
/// A `default` or `boot` type is not listed again under `public`. The
/// exported set is `boot`, then `default`, then `public`, whatever the source
/// order. A module that exports exactly one actor loads without a selector:
///
/// ```ignore
/// pub struct Hello { /* fields */ }
/// impl aether_actor::WasmActor for Hello { /* init + receive */ }
/// aether_actor::export!(public = [Hello]);
/// ```
///
/// A module that exports several actors routes through
/// `__export_multi_internal!`. Without `default` it designates no default,
/// so a load with no selector is refused with an error naming the exports:
///
/// ```ignore
/// aether_actor::export!(public = [Camera, CameraController, MeshViewer]);
/// aether_actor::export!(default = Camera, public = [CameraController]);
/// aether_actor::export!(boot = Registry, public = [WidgetA, WidgetB]);
/// aether_actor::export!(boot = Registry, default = Camera, public = [CameraController]);
/// ```
///
/// A call that is not keyed is a compile error that shows the keyed spelling,
/// and so is a repeated key, an empty list, or an unknown key.
///
/// # Generators
///
/// `generators = [aether_bloomery_bundle::bundle]` names export-generator
/// macros (paths, not trait objects). This crate collects a framework-owned
/// descriptor envelope per exported type — actor namespace plus optional
/// namespaced extensions — by invoking each type's same-name companion macro
/// (`use` / `pub use` / `as` aliases carry it). Generators receive `actors`
/// (envelopes, intact for the pipeline) and `exports` (the types the final
/// emitter will bind). A generator may rewrite `exports` and append envelopes
/// without moving another actor's extensions. FFI emission stays in this
/// macro: the pipeline ends in one keyed `export!` call.
///
/// `type Alias = T` is not followed; generators need a path that names the
/// `#[actor]` / `#[program]` / `#[reactor]` type (or an import alias of it):
///
/// ```ignore
/// aether_actor::export!(
///     default = Probe,
///     public = [ProbeWithConfig, SourcePublisher, SourceWitness, ReactorOutputSink],
///     generators = [aether_bloomery_bundle::bundle],
/// );
/// ```
///
/// # Private inline children
///
/// `private = [T, …]` names inline-child types the module spawns and
/// rebuilds on a replace but does not export (ADR-0114 §5). A private type
/// reaches the rehydrate shim's rebuild arm and the
/// `aether.kinds.inputs.private` section, which only the host's module-load
/// dependency check reads (ADR-0230 §3), so a load or replace of the module
/// is refused while a private child's declared dependency is not live. It is
/// not loadable by an export selector, not in `aether.kinds.inputs` or any
/// other manifest section the host reads, and not spawnable by runtime tag.
///
/// ```ignore
/// aether_actor::export!(default = Parent, public = [Sibling], private = [Child]);
/// ```
///
/// Each `export!` declares a hidden module type and implements [`Rebuildable`]
/// for every type it lists, exported or private, from any crate. A spawner
/// declares the inline children it spawns through the typed verbs in
/// `#[actor(spawns(..))]` ([`Spawns`]), and the verbs require that
/// declaration. Every `export!` then checks, at compile time, that it lists
/// each child that any type it lists declares, so a declared child the
/// `export!` does not list is a compile error at the `export!`. By induction,
/// the `export!` lists every actor that can run in its module, including
/// another crate's actors (a re-exported behavior host is an ordinary
/// `public` entry), and its rebuild arm covers the whole cluster.
///
/// Hot-swap state continuity (ADR-0040 / ADR-0101) needs no flag: the
/// `on_dehydrate` / `on_rehydrate` exports always forward to the
/// `WasmActor` hooks, which default to no-ops unless the actor overrides
/// them.
///
/// # The `library` feature: one `export!` per emitted wasm module
///
/// The entries this macro binds are fixed-name symbols (`init`,
/// `receive_p32`, the manifest custom sections), so a wasm module can
/// carry exactly one live `export!` expansion — a cdylib that links an
/// rlib whose own `export!` is live gets duplicate entry symbols at link
/// time and duplicate manifest records the substrate's reader rejects.
/// The gate is built in: every emitted item is behind
/// `cfg(not(feature = "library"))`, resolved against the *invoking*
/// crate's features. A crate that is consumed as an actor library by
/// another module declares `library = []` (non-default) in its
/// `[features]`; the embedding cdylib depends on it with
/// `features = ["library"]`, which strips the dependency's entry surface
/// while keeping the actor impls linkable (`spawn_inline_child` and
/// plain type reuse are unaffected). The crate's own wasm build never
/// enables the feature, so its standalone module keeps its entries. Call
/// sites stay the same keyed `export!(…)` either way; a crate nobody
/// embeds needs no feature declaration at all.
#[macro_export]
macro_rules! export {
    // One entry arm per key, each handing the whole input to the keyed
    // muncher, which accepts the keys in any order.
    (boot = $($tt:tt)*) => {
        $crate::__export_parse!(@start boot = $($tt)*);
    };
    (default = $($tt:tt)*) => {
        $crate::__export_parse!(@start default = $($tt)*);
    };
    (public = $($tt:tt)*) => {
        $crate::__export_parse!(@start public = $($tt)*);
    };
    (private = $($tt:tt)*) => {
        $crate::__export_parse!(@start private = $($tt)*);
    };
    (generators = $($tt:tt)*) => {
        $crate::__export_parse!(@start generators = $($tt)*);
    };
    // Every call that does not start with a key: a bare type, a bare type
    // list, a bare list followed by keys, and an empty call.
    ($($tt:tt)*) => {
        ::core::compile_error!(
            "export! takes keyed entries: `export!(public = [A, B])`, or \
             `export!(default = A, public = [B], private = [C])`"
        );
    };
}

// The keyed `export!` muncher. Its state is `{ boot, default, public, private,
// generators }`: `boot` / `default` are `none` or `{ T }`, `public` /
// `private` are `[]` until their key fills them with `{ T }` entries, and
// `generators` is `none` until read. Each key comes once, in any order. The
// exported set is `boot`, then `default`, then `public`. With `generators`,
// the finished state enters the generator pipeline; without, `@finish`
// completes one of the fixed forms and dispatches straight to the internal
// emitters.
#[doc(hidden)]
#[macro_export]
macro_rules! __export_parse {
    (@start $($tt:tt)*) => {
        $crate::__export_parse!(
            @parse
            { boot: none, default: none, public: [], private: [], generators: none }
            $($tt)*
        );
    };

    (@parse { boot: none, default: $default:tt, public: $public:tt, private: $private:tt, generators: $generators:tt }
     boot = $boot:ty $(, $($rest:tt)*)?) => {
        $crate::__export_parse!(
            @parse
            { boot: { $boot }, default: $default, public: $public, private: $private, generators: $generators }
            $($($rest)*)?
        );
    };

    (@parse { boot: $boot:tt, default: none, public: $public:tt, private: $private:tt, generators: $generators:tt }
     default = $default:ty $(, $($rest:tt)*)?) => {
        $crate::__export_parse!(
            @parse
            { boot: $boot, default: { $default }, public: $public, private: $private, generators: $generators }
            $($($rest)*)?
        );
    };

    (@parse { boot: $boot:tt, default: $default:tt, public: [], private: $private:tt, generators: $generators:tt }
     public = [$($p:ty),+ $(,)?] $(, $($rest:tt)*)?) => {
        $crate::__export_parse!(
            @parse
            { boot: $boot, default: $default, public: [$({ $p })+], private: $private, generators: $generators }
            $($($rest)*)?
        );
    };

    (@parse { boot: $boot:tt, default: $default:tt, public: [], private: $private:tt, generators: $generators:tt }
     public = [] $($rest:tt)*) => {
        ::core::compile_error!("export! `public` list must not be empty");
    };

    (@parse { boot: $boot:tt, default: $default:tt, public: $public:tt, private: [], generators: $generators:tt }
     private = [$($p:ty),+ $(,)?] $(, $($rest:tt)*)?) => {
        $crate::__export_parse!(
            @parse
            { boot: $boot, default: $default, public: $public, private: [$({ $p })+], generators: $generators }
            $($($rest)*)?
        );
    };

    (@parse { boot: $boot:tt, default: $default:tt, public: $public:tt, private: [], generators: $generators:tt }
     private = [] $($rest:tt)*) => {
        ::core::compile_error!("export! `private` list must not be empty");
    };

    (@parse { boot: $boot:tt, default: $default:tt, public: $public:tt, private: $private:tt, generators: none }
     generators = [$($g:path),+ $(,)?] $(, $($rest:tt)*)?) => {
        $crate::__export_parse!(
            @parse
            { boot: $boot, default: $default, public: $public, private: $private, generators: [$($g),+] }
            $($($rest)*)?
        );
    };

    (@parse { boot: $boot:tt, default: $default:tt, public: $public:tt, private: $private:tt, generators: none }
     generators = [] $($rest:tt)*) => {
        ::core::compile_error!("export! `generators` list must not be empty");
    };

    // A key whose slot is already filled.
    (@parse { boot: { $($x:tt)* }, $($state:tt)* } boot = $($rest:tt)*) => {
        ::core::compile_error!("export! key `boot` is given twice");
    };
    (@parse { boot: $boot:tt, default: { $($x:tt)* }, $($state:tt)* } default = $($rest:tt)*) => {
        ::core::compile_error!("export! key `default` is given twice");
    };
    (@parse { boot: $boot:tt, default: $default:tt, public: [$($x:tt)+], $($state:tt)* } public = $($rest:tt)*) => {
        ::core::compile_error!("export! key `public` is given twice");
    };
    (@parse { boot: $boot:tt, default: $default:tt, public: $public:tt, private: [$($x:tt)+], $($state:tt)* } private = $($rest:tt)*) => {
        ::core::compile_error!("export! key `private` is given twice");
    };
    (@parse { boot: $boot:tt, default: $default:tt, public: $public:tt, private: $private:tt, generators: [$($x:tt)+] } generators = $($rest:tt)*) => {
        ::core::compile_error!("export! key `generators` is given twice");
    };

    // A known key whose value did not match its arm above, or an unknown key.
    (@parse { $($state:tt)* } $key:ident = $($rest:tt)*) => {
        $crate::__export_parse!(@malformed $key);
    };

    (@parse { boot: $boot:tt, default: $default:tt, public: $public:tt, private: $private:tt, generators: $generators:tt }) => {
        $crate::__export_parse!(@order { boot: $boot, default: $default, public: $public, private: $private, generators: $generators });
    };

    (@parse { $($state:tt)* } $bad:tt $($rest:tt)*) => {
        ::core::compile_error!(concat!(
            "export! takes only keyed entries (boot, default, public, private, generators); `",
            stringify!($bad),
            "` is not one; list exported actors under `public = [..]`"
        ));
    };

    (@malformed boot) => {
        ::core::compile_error!("export! `boot = B` takes one actor type");
    };
    (@malformed default) => {
        ::core::compile_error!("export! `default = A` takes one actor type");
    };
    (@malformed public) => {
        ::core::compile_error!("export! `public = [A, B]` takes a bracketed list of actor types");
    };
    (@malformed private) => {
        ::core::compile_error!("export! `private = [C]` takes a bracketed list of actor types");
    };
    (@malformed generators) => {
        ::core::compile_error!("export! `generators = [g]` takes a bracketed list of generator macro paths");
    };
    (@malformed $bad:ident) => {
        ::core::compile_error!(concat!(
            "export! takes only keyed entries (boot, default, public, private, generators); `",
            stringify!($bad),
            "` is not one; list exported actors under `public = [..]`"
        ));
    };

    // Every key read: the exported set is `boot`, then `default`, then
    // `public`, whatever the source order.
    (@order { boot: none, default: none, public: [$($public:tt)*], private: $private:tt, generators: $generators:tt }) => {
        $crate::__export_parse!(@route { boot: none, default: none, all: [$($public)*], private: $private, generators: $generators });
    };
    (@order { boot: { $boot:ty }, default: none, public: [$($public:tt)*], private: $private:tt, generators: $generators:tt }) => {
        $crate::__export_parse!(@route { boot: { $boot }, default: none, all: [{ $boot } $($public)*], private: $private, generators: $generators });
    };
    (@order { boot: none, default: { $default:ty }, public: [$($public:tt)*], private: $private:tt, generators: $generators:tt }) => {
        $crate::__export_parse!(@route { boot: none, default: { $default }, all: [{ $default } $($public)*], private: $private, generators: $generators });
    };
    (@order { boot: { $boot:ty }, default: { $default:ty }, public: [$($public:tt)*], private: $private:tt, generators: $generators:tt }) => {
        $crate::__export_parse!(@route { boot: { $boot }, default: { $default }, all: [{ $boot } { $default } $($public)*], private: $private, generators: $generators });
    };

    // With generators: hand the state to the pipeline, whose final `export!`
    // emits the marker and the coverage check.
    (@route { boot: $boot:tt, default: $default:tt, all: [$($all:tt)*], private: [$($private:tt)*], generators: [$($g:path),+] }) => {
        $crate::__export_with_generators!(
            boot: $boot,
            default: $default,
            types: [$($all)*],
            private: [$($private)*],
            generators: [$($g),+]
        );
    };

    // No generators: the marked set is the exported set then the private one.
    (@route { boot: $boot:tt, default: $default:tt, all: [$($all:tt)*], private: [$($private:tt)*], generators: none }) => {
        $crate::__export_parse!(
            @finish
            { boot: $boot, default: $default, all: [$($all)*], marked: [$($all)* $($private)*], private: [$($private)*] }
        );
    };

    (@finish { boot: none, default: none, all: [], marked: [$($marked:tt)*], private: [$($private:tt)*] }) => {
        ::core::compile_error!("export! needs at least one exported type");
    };

    (@finish
        { boot: none, default: none, all: [{ $component:ty }], marked: [$({ $m:ty })*], private: [$({ $p:ty })*] }
    ) => {
        $crate::__export_internal!(@listed $($m),*);
        $crate::__export_internal!($component ; private [$($p),*]);
    };

    (@finish
        { boot: none, default: none, all: [$({ $component:ty })+], marked: [$({ $m:ty })*], private: [$({ $p:ty })*] }
    ) => {
        $crate::__export_internal!(@listed $($m),*);
        $crate::__export_multi_internal!(@no_boot ; @no_default ; @all $($component),+ ; @private [$($p),*]);
    };

    (@finish
        { boot: none, default: { $default:ty }, all: [$({ $component:ty })+], marked: [$({ $m:ty })*], private: [$({ $p:ty })*] }
    ) => {
        $crate::__export_internal!(@listed $($m),*);
        $crate::__export_multi_internal!(@no_boot ; @default $default ; @all $($component),+ ; @private [$($p),*]);
    };

    (@finish
        { boot: { $boot:ty }, default: { $default:ty }, all: [$({ $component:ty })+], marked: [$({ $m:ty })*], private: [$({ $p:ty })*] }
    ) => {
        $crate::__export_internal!(@listed $($m),*);
        $crate::__export_multi_internal!(@boot $boot ; @default $default ; @all $($component),+ ; @private [$($p),*]);
    };

    (@finish { boot: { $boot:ty }, default: none, all: [{ $only:ty }], marked: [$($marked:tt)*], private: [$($private:tt)*] }) => {
        ::core::compile_error!("export! boot-only modules need at least one non-boot export");
    };

    (@finish
        { boot: { $boot:ty }, default: none, all: [$({ $component:ty })+], marked: [$({ $m:ty })*], private: [$({ $p:ty })*] }
    ) => {
        $crate::__export_internal!(@listed $($m),*);
        $crate::__export_multi_internal!(@boot $boot ; @no_default ; @all $($component),+ ; @private [$($p),*]);
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __export_with_generators {
    (
        boot: $boot:tt,
        default: $default:tt,
        types: [$($types:tt)*],
        private: [$($private:tt)*],
        generators: [$($g:path),+]
    ) => {
        $crate::__export_collect!(
            @start
            { boot: $boot, default: $default, types: [$($types)*], private: [$($private)*], generators: [$($g),+] }
        );
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __export_collect {
    (@start { boot: $boot:tt, default: $default:tt, types: [], private: $private:tt, generators: [$($g:path),+] }) => {
        ::core::compile_error!("export! generators require at least one type");
    };
    (@start
        {
            boot: $boot:tt,
            default: $default:tt,
            types: [ { $first:path } $($rest:tt)* ],
            private: $private:tt,
            generators: [$($g:path),+]
        }
    ) => {
        $first! {
            @aether_export_desc
            $crate::__export_collect
            {
                current_ty: { $first }
                pending: [ $($rest)* ]
                actors: []
                exports: [ { $first } $($rest)* ]
                private: $private
                boot: $boot
                default: $default
                remaining_generators: [$($g),+]
            }
        }
    };
    (@start
        { boot: $boot:tt, default: $default:tt, types: [ { $first:ty } $($rest:tt)* ], private: $private:tt, generators: [$($g:path),+] }
    ) => {
        ::core::compile_error!(concat!(
            "export! generators require a simple type path with #[actor] or #[reactor] companion metadata; `type` aliases are not followed: ",
            stringify!($first)
        ));
    };
    (@aether_export_got
        { namespace: $ns:tt, extensions: [ $($ext:tt)* ] }
        {
            current_ty: { $ty:path }
            pending: [ { $next:path } $($pending:tt)* ]
            actors: [$($actors:tt)*]
            exports: [$($exports:tt)*]
            private: $private:tt
            boot: $boot:tt
            default: $default:tt
            remaining_generators: [$($g:path),+]
        }
    ) => {
        $next! {
            @aether_export_desc
            $crate::__export_collect
            {
                current_ty: { $next }
                pending: [ $($pending)* ]
                actors: [
                    $($actors)*
                    { ty: { $ty } namespace: $ns extensions: [ $($ext)* ] }
                ]
                exports: [$($exports)*]
                private: $private
                boot: $boot
                default: $default
                remaining_generators: [$($g),+]
            }
        }
    };
    (@aether_export_got
        { namespace: $ns:tt, extensions: [ $($ext:tt)* ] }
        {
            current_ty: { $ty:path }
            pending: []
            actors: [$($actors:tt)*]
            exports: [$($exports:tt)*]
            private: $private:tt
            boot: $boot:tt
            default: $default:tt
            remaining_generators: [$gen:path $(, $rest:path)*]
        }
    ) => {
        $gen! {
            @aether_export_generate
            { remaining_generators: [$($rest),*] }
            {
                boot: $boot,
                default: $default,
                actors: [
                    $($actors)*
                    { ty: { $ty } namespace: $ns extensions: [ $($ext)* ] }
                ],
                exports: [$($exports)*],
                private: $private
            }
        }
    };
    (@aether_export_got { $($meta:tt)* } { $($state:tt)* }) => {
        ::core::compile_error!("export! descriptor envelope was malformed");
    };
    ($($tt:tt)*) => {
        ::core::compile_error!("export! descriptor collection failed");
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __export_desc_discard {
    (@aether_export_got { $($__aether_meta:tt)* } { $($__aether_state:tt)* }) => {};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __export_continue {
    (
        remaining_generators: []
        boot: $boot:tt
        default: $default:tt
        actors: [$($actors:tt)*]
        exports: [$($exports:tt)*]
        private: [$($private:tt)*]
    ) => {
        $crate::__export_emit_classified! {
            boot: $boot
            default: $default
            actors: [$($actors)*]
            exports: [$($exports)*]
            private: [$($private)*]
        }
    };
    (
        remaining_generators: [$next:path $(, $rest:path)*]
        boot: $boot:tt
        default: $default:tt
        actors: [$($actors:tt)*]
        exports: [$($exports:tt)*]
        private: [$($private:tt)*]
    ) => {
        $next! {
            @aether_export_generate
            { remaining_generators: [$($rest),*] }
            {
                boot: $boot,
                default: $default,
                actors: [$($actors)*],
                exports: [$($exports)*],
                private: [$($private)*]
            }
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __export_internal {
    ($component:ty ; private [$($private:ty),*]) => {
        static __AETHER_COMPONENT: $crate::Slot<$component> = $crate::Slot::new();

        // ADR-0114: the component's own inline-child registry — one per
        // `export!`, mirroring `__AETHER_COMPONENT`. The `receive`
        // membrane and the dehydrate / rehydrate shims thread
        // `&__AETHER_INLINE` to the inline-child consumers instead of
        // reaching for a crate-global static.
        static __AETHER_INLINE: $crate::wasm::inline::Registry =
            $crate::wasm::inline::Registry::new();

        // ADR-0033 / issue 442: pin the actor's `aether.kinds.inputs`
        // bytes into the cdylib's wasm custom section. The const data
        // (`__AETHER_INPUTS_MANIFEST_LEN` / `__AETHER_INPUTS_MANIFEST`)
        // is emitted by `#[actor]` on the type's inherent impl;
        // section emission lives here so it only fires in the cdylib
        // root crate (where `export!()` is invoked) and never in
        // transitive rlib pulls of a `#[actor]`-using crate, which
        // would otherwise stack duplicate Component records and fail
        // the substrate's manifest reader.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(link_section = "aether.kinds.inputs")]
        static __AETHER_INPUTS_SECTION: [u8; <$component>::__AETHER_INPUTS_MANIFEST_LEN] =
            <$component>::__AETHER_INPUTS_MANIFEST;

        // Issue 6590: the private inline children's groups, in their own
        // section beside the boundary-free single-actor inputs.
        $crate::__export_internal!(@private_inputs $($private),*);

        // ADR-0166: pin only this exported actor's anonymous placement facts.
        // The actor macro emits associated const data; retention stays at the
        // cdylib-root `export!` call so transitive rlibs contribute no custom
        // section of their own.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(link_section = "aether.actor.lineage")]
        static __AETHER_LINEAGE_SECTION: [u8; <$component>::__AETHER_LINEAGE_MANIFEST_LEN] =
            <$component>::__AETHER_LINEAGE_MANIFEST;

        // Issue 525 Phase 1B: pin the actor's `Addressable::NAMESPACE` bytes
        // into a sibling `aether.namespace` custom section. The
        // substrate reads this at load time as the default mailbox
        // name when the load payload omits an explicit `name`.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(link_section = "aether.namespace")]
        static __AETHER_NAMESPACE_SECTION: [u8; <$component as $crate::Addressable>::NAMESPACE.len()] = {
            let bytes = <$component as $crate::Addressable>::NAMESPACE.as_bytes();
            let mut out = [0u8; <$component as $crate::Addressable>::NAMESPACE.len()];
            let mut i = 0;
            while i < bytes.len() {
                out[i] = bytes[i];
                i += 1;
            }
            out
        };

        /// # Safety
        /// Called exactly once by the substrate before any `receive`.
        /// Receives the actor's own mailbox id and logical parent mailbox so
        /// runtime ctxs can select Root, Current, or Parent lineage.
        ///
        /// ADR-0090 (issue 1256): the substrate writes `config_len`
        /// bytes at `config_ptr` (`CONFIG_OFFSET` in the substrate's
        /// scratch layout) before calling. `config_len == 0` passes
        /// through as `&[]` and resolves to the actor's compiled
        /// `Config::default()`. Non-empty bytes are decoded as the
        /// actor's `Config`; a decode failure stages the message via
        /// `init_failed_p32` and returns 1.
        ///
        /// Returns `0` on success and non-zero when the actor's `init`
        /// returned `Err(ActorInitError)` or non-empty config bytes failed
        /// to decode.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(export_name = "init_with_parent_p32")]
        pub unsafe extern "C" fn init_with_parent(
            mailbox_id: u64,
            parent_mailbox_id: u64,
            config_ptr: u32,
            config_len: u32,
        ) -> u32 {
            $crate::wasm::install_guest_logging();
            // Build the config slice. Empty-len short-circuits to `&[]`
            // so a null/zero `config_ptr` is not dereferenced — mirrors
            // `PriorState::bytes`.
            let config_bytes: &[u8] = if config_len == 0 {
                &[]
            } else {
                // SAFETY: substrate wrote `config_len` bytes at
                // `config_ptr` (ADR-0090); slice lifetime is bounded
                // by this call, which finishes before the substrate
                // reuses the scratch region.
                unsafe {
                    ::core::slice::from_raw_parts(
                        config_ptr as usize as *const u8,
                        config_len as usize,
                    )
                }
            };
            let config = if config_len == 0 {
                <<$component as $crate::Lifecycle<$component>>::Config as ::core::default::Default>::default()
            } else {
                let Some(config) = <<$component as $crate::Lifecycle<$component>>::Config as $crate::__macro_internals::Kind>::decode_from_bytes(
                    config_bytes,
                ) else {
                    let msg = ::core::concat!(
                        "guest init: ",
                        ::core::stringify!($component),
                        " could not decode Config from bytes",
                    );
                    $crate::wasm::stage_init_failure(msg);
                    return 1;
                };
                config
            };
            // ADR-0114 addressing amendment: capture the real folded
            // mailbox id the substrate hands the guest as this cluster's
            // self-identity, so the instance is addressable at any lineage
            // depth (a loaded component is depth >= 2). The receive membrane
            // and `WasmCtx` self read this rather than recomputing
            // `hash(NAMESPACE)`, the ADR-0099 depth-1 fixed point.
            __AETHER_INLINE.set_self_id(mailbox_id);
            __AETHER_INLINE.set_parent_id(parent_mailbox_id);
            // Issue 2692: install this module's by-tag inline-spawn resolver
            // (the tag-match over its exported set) so guest handler / `wire`
            // code can `ctx.spawn_inline_child_by_tag(...)`. Set once here at
            // init, before any `wire` or `receive` runs.
            __AETHER_INLINE.set_spawn_resolver(
                $crate::__export_internal!(@spawn_inline_child_by_tag $component),
            );
            // ADR-0156 §2: the component host passes empty params bytes for
            // now, so the guest resolves `Params` to its compiled default —
            // the same empty-slice-to-default path config takes above. Every
            // current component ships `Params = ()`; a later slice threads
            // real params bytes over the FFI.
            let params =
                <<$component as $crate::Lifecycle<$component>>::Params as ::core::default::Default>::default();
            let mut ctx: $crate::WasmInitCtx<'_> = $crate::WasmInitCtx::__new(mailbox_id);
            match <$component as $crate::Lifecycle<$component>>::init(config, params, &mut ctx) {
                Ok(instance) => {
                    __AETHER_INLINE.set_entry_actor_tag($crate::ActorTypeTag::of::<$component>());
                    unsafe {
                        __AETHER_COMPONENT.set(instance);
                    }
                    0
                }
                Err(err) => {
                    $crate::wasm::stage_init_failure(err.message());
                    1
                }
            }
        }

        /// # Safety
        /// Existing config-bearing init ABI. Older substrates do not supply a
        /// logical parent, so this forwards with mailbox id `0`.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(export_name = "init_with_config_p32")]
        pub unsafe extern "C" fn init_with_config(mailbox_id: u64, config_ptr: u32, config_len: u32) -> u32 {
            unsafe { init_with_parent(mailbox_id, 0, config_ptr, config_len) }
        }

        /// # Safety
        /// ADR-0090: legacy zero-config `init` shim. Called by older
        /// substrate builds that don't know about `init_with_config_p32`. Reaches
        /// into `init_with_config` with empty config bytes, resolving typed
        /// config actors through their compiled `Config::default()`.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn init(mailbox_id: u64) -> u32 {
            // SAFETY: forwarding to `init_with_config` with `config_len = 0`
            // makes `config_ptr` unread (the function's empty-len
            // branch returns `&[]`), so the dummy `0` pointer is
            // never dereferenced.
            unsafe { init_with_config(mailbox_id, 0, 0) }
        }

        /// # Safety
        /// Called by the substrate exactly once after `init` returns
        /// Ok and the component's mailbox is published, before the
        /// first `receive` (issue 584 Phase 2b, ADR-0079 amended).
        /// Mail-allowed — peer mailboxes are addressable. Receives the
        /// component's own mailbox id so the SDK ctx can self-address.
        ///
        /// Uses `WasmCtx`, the send-capable runtime ctx, so typed capability
        /// facades can self-address; `WasmInitCtx` carries no send surface.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn wire(mailbox_id: u64) -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            // ADR-0114 addressing amendment: also capture the real folded id
            // here, so the cluster self-identity is set even if a future
            // host calls `wire` without a prior `init_with_config` on this
            // instance (idempotent — same value).
            __AETHER_INLINE.set_self_id(mailbox_id);
            // ADR-0112: the runtime builds the erased `Manual` view. The
            // lifecycle ctx is `WasmCtx<'_, $component>` (= Single), so upgrade
            // it to the actor once, here where it is born, and downgrade.
            let mut ctx = $crate::WasmCtx::__new(mailbox_id, &__AETHER_INLINE, $crate::wasm::NO_INBOUND_SOURCE);
            <$component as $crate::Lifecycle<$component>>::wire(instance, ctx.__for_actor::<$component>().as_single());
            0
        }

        /// # Safety
        /// Called by the substrate exactly once before `on_dehydrate`
        /// (on a replace) or the instance drop, on the dying instance
        /// (issue 584 Phase 2b, ADR-0079 amended). Mail-allowed — live
        /// peers are still addressable; sends to a dead peer warn-drop.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn unwire(mailbox_id: u64) -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            let mut ctx = $crate::WasmCtx::__new(mailbox_id, &__AETHER_INLINE, $crate::wasm::NO_INBOUND_SOURCE);
            <$component as $crate::Lifecycle<$component>>::unwire(instance, ctx.__for_actor::<$component>().as_single());
            0
        }

        /// # Safety
        /// Called by the substrate with `(kind, ptr, byte_len, count,
        /// sender, recipient)` matching the FFI contract. Exported under
        /// the `_p32` suffix per ADR-0024 Phase 1; the trailing
        /// `recipient: u64` (ADR-0114 decision #1) widens like the other
        /// frame slots on the wasm path.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(export_name = "receive_p32")]
        pub unsafe extern "C" fn receive(
            kind: u64,
            ptr: u32,
            byte_len: u32,
            count: u32,
            sender: u32,
            recipient: u64,
            source: u64,
        ) -> u32 {
            // ADR-0114 addressing amendment: the cluster self-identity is the
            // real folded id captured at `init` / `wire`, so the membrane and
            // `WasmCtx` self are correct at any lineage depth. Fall back to
            // `hash(NAMESPACE)` (the ADR-0099 depth-1 fixed point) only if no
            // shim has run yet — a receive before `wire`, which should not
            // happen but must not regress a depth-1 actor.
            let mailbox_id = {
                let captured = __AETHER_INLINE.self_id();
                if captured != 0 {
                    captured
                } else {
                    $crate::__macro_internals::mailbox_id_from_name(
                        <$component as $crate::Addressable>::NAMESPACE,
                    )
                    .0
                }
            };
            let mail =
                unsafe { $crate::Mail::__from_raw(kind, ptr, byte_len, count, sender, recipient) };
            // ADR-0114: the receive membrane demuxes on the routed
            // recipient — own id dispatches the parent's handlers, an
            // inline-child alias dispatches the co-located child. For a
            // normally-addressed actor the recipient equals `mailbox_id`,
            // so the closure runs verbatim. ADR-0112: dispatch receives
            // the full `Manual` ctx; `__aether_dispatch` downgrades per
            // handler class.
            //
            // The top-level dispatch's `&mut instance` borrow is scoped so it
            // is released before the drain loop, which re-acquires the
            // instance fresh per item — no two `&mut` instance borrows ever
            // overlap (the borrow-aliasing the #1945 bounce proved).
            let rc = {
                let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                    return 1;
                };
                // Top-level dispatch: the host threads the resolved inbound
                // source on the `receive_p32` ABI (issue 2001), so the ctx
                // carries it directly and `ctx.sender()` is a single
                // field read — the same path the in-place drain takes. The
                // membrane gets the same `source` so a mail routed straight to
                // an inline-child alias (ADR-0114) hands the child its source
                // too, not just the cluster root.
                $crate::wasm::inline::membrane_dispatch(mailbox_id, mail, &__AETHER_INLINE, source, move |__aether_mail| {
                    let mut ctx = $crate::WasmCtx::__new(mailbox_id, &__AETHER_INLINE, source);
                    instance.__aether_dispatch(&mut ctx, __aether_mail)
                })
            };
            // ADR-0114 addressing amendment: drain every intra-cluster send
            // the dispatch (and any cascade it triggers) buffered, in place
            // under this one run-token. Each item re-acquires the instance
            // inside the per-item `dispatch_own` factory, which the drain
            // hands the item's inbound source (`__aether_source`, issue 1987)
            // so the own-path ctx reads the same source the child path threads.
            $crate::wasm::inline::drain_cluster_queue(&__AETHER_INLINE, |__aether_source| {
                move |__aether_mail| {
                    // SAFETY: re-acquired fresh per drained item; the prior
                    // iteration's borrow has already dropped (the membrane
                    // returned). A missing instance during drain is impossible
                    // — the slot was present for the top-level dispatch above.
                    let instance = unsafe { __AETHER_COMPONENT.get_mut() }
                        .expect("instance present for the cluster-queue drain");
                    let mut ctx = $crate::WasmCtx::__new_local_dispatch(mailbox_id, &__AETHER_INLINE, __aether_source);
                    instance.__aether_dispatch(&mut ctx, __aether_mail)
                }
            });
            rc
        }

        /// ADR-0095: the generic guest allocator the substrate delivers every
        /// inbound payload through. `cabi_realloc`-shaped — allocate
        /// (`old_ptr == 0`), grow possibly-relocating (`new_size > old_size`),
        /// free (`new_size == 0`). The substrate allocates a small region once
        /// at instantiate and a large region on demand, writes the payload, and
        /// calls the entry point (`receive` / `init_with_config` /
        /// `on_rehydrate`). Backed by [`$crate::wasm::guest_alloc`].
        ///
        /// # Safety
        /// Called by the substrate per the layout contract; see
        /// [`$crate::wasm::guest_alloc::realloc_bytes`].
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(export_name = "realloc_p32")]
        pub unsafe extern "C" fn realloc_p32(
            old_ptr: u32,
            old_size: u32,
            align: u32,
            new_size: u32,
        ) -> u32 {
            // SAFETY: see `guest_alloc::realloc_bytes`. wasm32 pointers are
            // 32-bit; a null result (free, or allocation failure) maps to 0.
            unsafe {
                $crate::wasm::guest_alloc::realloc_bytes(
                    old_ptr as *mut u8,
                    old_size as usize,
                    align as usize,
                    new_size as usize,
                )
                .addr() as u32
            }
        }

        /// # Safety
        /// Called by the substrate exactly once, on the old instance,
        /// immediately before a `replace_component` swap. Forwards to
        /// [`$crate::WasmActor::on_dehydrate`], a no-op unless the actor
        /// overrides it.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn on_dehydrate() -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            // ADR-0114 addressing amendment: the cluster self-identity is the
            // real folded id captured at `init` / `wire` — the same id
            // `receive` derives for `WasmCtx`, so a `send::<R>` from the save
            // hook resolves correctly at any lineage depth. Fall back to
            // `hash(NAMESPACE)` only before any shim has run.
            let mailbox_id = {
                let captured = __AETHER_INLINE.self_id();
                if captured != 0 {
                    captured
                } else {
                    $crate::__macro_internals::mailbox_id_from_name(
                        <$component as $crate::Addressable>::NAMESPACE,
                    )
                    .0
                }
            };
            // ADR-0114 §5: run the parent's `on_dehydrate` and every
            // resident inline child's into a single composite, then call
            // the host `save_state` once. With no inline children the
            // composite is byte-identical to the parent's own blob, so a
            // childless component dehydrates exactly as before; a parent
            // that saves nothing and has no children skips the host save.
            let __aether_user_state = $crate::wasm::inline::compose::dehydrate(
                mailbox_id,
                &__AETHER_INLINE,
                |ctx| <$component as $crate::WasmActor>::on_dehydrate(instance, ctx),
            );
            let __aether_state = __AETHER_INLINE.compose_request_context_state(__aether_user_state);
            if let Some((version, bytes)) = __aether_state {
                let mut ctx: $crate::WasmDropCtx<'_> =
                    $crate::WasmDropCtx::__new(mailbox_id, __AETHER_INLINE.parent_id_for(mailbox_id));
                ctx.save_state(version, &bytes);
            }
            0
        }

        /// # Safety
        /// Called by the substrate after `init` on a freshly
        /// instantiated replacement, with `(version, ptr, len)`
        /// describing the prior-state bundle the old instance produced.
        /// Exported under the `_p32` suffix per ADR-0024 Phase 1.
        /// Forwards to [`$crate::WasmActor::on_rehydrate`], a no-op unless
        /// the actor overrides it.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(export_name = "on_rehydrate_p32")]
        pub unsafe extern "C" fn on_rehydrate(version: u32, ptr: u32, len: u32) -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            // ADR-0114 addressing amendment: self_id-first, name-hash
            // fallback — the same derivation as `receive` / `on_dehydrate`.
            let mailbox_id = {
                let captured = __AETHER_INLINE.self_id();
                if captured != 0 {
                    captured
                } else {
                    $crate::__macro_internals::mailbox_id_from_name(
                        <$component as $crate::Addressable>::NAMESPACE,
                    )
                    .0
                }
            };
            // ADR-0114 §5: decompose the migration bundle, restore the
            // parent, then reconstruct each inline child by type. For a
            // childless component the bundle decomposes to the raw parent
            // blob, so the parent sees the identical `PriorState` it would
            // have before. A single-actor module rebuilds `$component` and
            // its private children; a child of any other type is logged and
            // skipped.
            let prior_bytes: &[u8] = if len == 0 {
                &[]
            } else {
                // SAFETY: substrate wrote `len` bytes at `ptr` (the rehydrate
                // ABI); the slice is bounded by this call.
                unsafe { ::core::slice::from_raw_parts(ptr as usize as *const u8, len as usize) }
            };
            let (__aether_contexts, __aether_user_version, __aether_user_bytes) =
                $crate::split_state_envelope(version, prior_bytes);
            __AETHER_INLINE.restore_request_contexts(__aether_contexts);
            $crate::wasm::inline::compose::reconstruct_inline_children(
                __aether_user_version,
                &__aether_user_bytes,
                &__AETHER_INLINE,
                |parent_version, parent_bytes| {
                    let mut ctx = $crate::WasmCtx::__new(mailbox_id, &__AETHER_INLINE, $crate::wasm::NO_INBOUND_SOURCE);
                    // SAFETY: `parent_bytes` lives for this closure call;
                    // `PriorState::__from_ptr` bounds the slice to it.
                    let parent_prior = unsafe {
                        $crate::PriorState::__from_ptr(
                            parent_version,
                            parent_bytes.as_ptr() as usize,
                            parent_bytes.len(),
                        )
                    };
                    // #6533: `on_rehydrate` takes the lifecycle ctx typed by
                    // the actor, so upgrade the erased ctx once, here where
                    // it is born, as the `wire` / `unwire` shims do.
                    <$component as $crate::WasmActor>::on_rehydrate(
                        instance,
                        ctx.__for_actor::<$component>().as_single(),
                        parent_prior,
                    );
                },
                |registry, parent, child| {
                    $crate::__export_internal!(@reconstruct_child registry, parent, child ; [$component] ; [$($private),*])
                },
            );
            0
        }
    };

    // The module type, the `Rebuildable` marker for every type an `export!`
    // lists, exported and private, and the check that the `export!` lists
    // every inline child a listed type declares in `spawns(..)` (ADR-0114 §5).
    // Every finished `export!` form emits them through this one arm, once.
    // Not gated on the wasm target or `library`: a crate's spawn declarations
    // are checked on the host build and in a `library` embed as well.
    (@listed $($listed:ty),*) => {
        #[doc(hidden)]
        pub struct __AetherModule;

        $(
            // SAFETY: emitted by `export!` for a type its rebuild arm lists.
            unsafe impl $crate::Rebuildable<__AetherModule> for $listed {}
        )*

        // Never called: a closure body is type-checked, so each call's where
        // clause requires every child the listed type declares in
        // `spawns(..)` to be listed here as well, and an unused const needs
        // no suppression.
        const _: fn() = || {
            $( <$listed>::__aether_listed_children::<__AetherModule>(); )*
        };
    };

    // Issue 6590: pin the private inline children's inputs into the sibling
    // `aether.kinds.inputs.private` section, so the host's module-load
    // dependency check (ADR-0230 §3) reads their `#[actor(depends(..))]`
    // declarations. Nothing else reads the section, so a private type stays
    // out of export selection and component description. Every `export!`
    // form calls this once; an empty private list emits no section.
    (@private_inputs) => {};
    (@private_inputs $($private:ty),+) => {
        $crate::__export_internal!(
            @grouped_inputs __AETHER_PRIVATE_INPUTS_LEN, __AETHER_PRIVATE_INPUTS_SECTION,
            "aether.kinds.inputs.private" ; $($private),+
        );
    };

    // ADR-0096: one inputs section of `ActorBoundary`-led groups. Each
    // listed type's records are preceded by an `ActorBoundary { namespace }`
    // record (version-tagged like every other record) so the host's reader
    // regroups the flat record stream into one capability set per type, in
    // list order. The multi-actor `aether.kinds.inputs` static and the
    // private-children static are both this arm, so the two sections share
    // one record grammar byte for byte.
    (@grouped_inputs $len:ident, $section_static:ident, $section:literal ; $($listed:ty),+) => {
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        const $len: usize = 0usize $(
            + 1
            + $crate::__macro_internals::canonical::inputs_actor_boundary_len(
                <$listed as $crate::Addressable>::NAMESPACE,
            )
            + <$listed>::__AETHER_INPUTS_MANIFEST_LEN
        )+;

        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(link_section = $section)]
        static $section_static: [u8; $len] = {
            let mut out = [0u8; $len];
            let mut pos = 0usize;
            $(
                {
                    // The per-type `ActorBoundary` record, then that
                    // type's own `aether.kinds.inputs` manifest bytes.
                    const BOUNDARY_LEN: usize =
                        $crate::__macro_internals::canonical::inputs_actor_boundary_len(
                            <$listed as $crate::Addressable>::NAMESPACE,
                        );
                    const BOUNDARY_BYTES: [u8; BOUNDARY_LEN] =
                        $crate::__macro_internals::canonical::write_inputs_actor_boundary::<BOUNDARY_LEN>(
                            <$listed as $crate::Addressable>::NAMESPACE,
                        );
                    // Per-record section version byte — a token reference
                    // to `INPUTS_SECTION_VERSION` (bumped by ADR-0118 /
                    // issue 1984; the boundary record is a per-record frame
                    // and tracks the same version as the records emitted by
                    // the derive macro).
                    out[pos] = $crate::__macro_internals::INPUTS_SECTION_VERSION;
                    pos += 1;
                    let mut i = 0;
                    while i < BOUNDARY_LEN {
                        out[pos] = BOUNDARY_BYTES[i];
                        pos += 1;
                        i += 1;
                    }
                    const MANIFEST_LEN: usize = <$listed>::__AETHER_INPUTS_MANIFEST_LEN;
                    const MANIFEST_BYTES: [u8; MANIFEST_LEN] =
                        <$listed>::__AETHER_INPUTS_MANIFEST;
                    let mut j = 0;
                    while j < MANIFEST_LEN {
                        out[pos] = MANIFEST_BYTES[j];
                        pos += 1;
                        j += 1;
                    }
                }
            )+
            let _ = pos;
            out
        };
    };

    // Reconstruct one inline child by matching its persisted type tag
    // against the module's exported types, then, only when none matches,
    // against its private types (ADR-0114 §5). A matching candidate
    // validates the replacement module's current placement facts against the
    // effective parent, then re-`init`s the child and restores its state
    // through the parent-aware compose helper. An unmatched or rejected child
    // returns `false` so the caller logs + skips it; a matched tag that
    // placement rejects does not fall through to the private types.
    (@reconstruct_child $registry:ident, $parent:ident, $child:ident ; [$($candidate:ty),+] ; [$($private:ty),*]) => {{
        $(
            if $child.type_tag == $crate::ActorTypeTag::of::<$candidate>().0 {
                $crate::__export_internal!(@reconstruct_one $registry, $parent, $child ; $candidate)
            } else
        )+
        $(
            if $child.type_tag == $crate::ActorTypeTag::of::<$private>().0 {
                $crate::__export_internal!(@reconstruct_one $registry, $parent, $child ; $private)
            } else
        )*
        {
            false
        }
    }};

    (@reconstruct_one $registry:ident, $parent:ident, $child:ident ; $candidate:ty) => {{
        $crate::wasm::__validate_inline_child_placement(
            $registry,
            $parent.0,
            $crate::ActorTypeTag($child.type_tag),
            <$candidate>::__AETHER_PLACEMENT,
        )
        .is_ok()
            && $crate::wasm::inline::compose::reconstruct_one_child_at_parent::<$candidate>($registry, $parent, $child)
    }};

    // Resolve one inline child to *spawn* by matching a runtime actor-type
    // tag against the module's exported type set (issue 2692) — the spawn
    // sibling of `@reconstruct_child`, a second consumer of the same
    // `$($candidate)` list, not a second copy of the table. Emits a
    // non-capturing closure that coerces to `wasm::inline::SpawnByTagFn`; the
    // module's init shims install it on `__AETHER_INLINE`. The matched branch
    // allocates the child's alias via the host `spawn_inline_child` host fn
    // THEN runs the shared decode + init core; an unmatched tag returns
    // `UnknownActorTag` *before* any allocation, so no host alias is orphaned.
    (@spawn_inline_child_by_tag $($candidate:ty),+) => {
        |__aether_registry: &$crate::wasm::inline::Registry,
         __aether_parent: u64,
         __aether_tag: $crate::ActorTypeTag,
         __aether_is_counter: bool,
         __aether_subname: &str,
         __aether_config: &[u8]|
         -> ::core::result::Result<$crate::MailboxId, $crate::SpawnError> {
            $(
                if __aether_tag == $crate::ActorTypeTag::of::<$candidate>() {
                    $crate::wasm::__validate_inline_child_placement(
                        __aether_registry,
                        __aether_parent,
                        __aether_tag,
                        <$candidate>::__AETHER_PLACEMENT,
                    )?;
                    let __aether_alias = $crate::wasm::__alloc_inline_child_alias(
                        __aether_parent,
                        __aether_is_counter,
                        __aether_subname,
                    )?;
                    return $crate::wasm::inline::compose::spawn_one_child::<$candidate>(
                        __aether_registry,
                        __aether_parent,
                        __aether_alias,
                        __aether_tag.0,
                        $crate::__macro_internals::String::from(__aether_subname),
                        __aether_is_counter,
                        __aether_config,
                    );
                }
            )+
            ::core::result::Result::Err(
                $crate::SpawnError::UnknownActorTag(__aether_tag),
            )
        }
    };
}

/// ADR-0096 / ADR-0138: FFI shims for a multi-actor module —
/// `export!(default = A, public = [B, …])` or `export!(public = [A, B, …])`.
///
/// One module-level `Slot<Box<dyn ErasedWasmActor>>` holds whichever
/// exported type the instance became. Two construction entry points:
///
/// - `init_with_parent_p32` constructs the **default** type and records the
///   logical parent mailbox. `init_with_config_p32` remains as an additive
///   compatibility wrapper that supplies parent `0`. A defaultless module
///   stages the same "module has no default" failure through both exports.
/// - `init_typed_with_parent_p32` carries both logical parent and actor-type
///   tag. `init_typed_p32` remains as its parent-`0` compatibility wrapper.
///   Both match the tag against each exported type's
///   `mailbox_id_from_name(NAMESPACE)` and construct the selected one.
///
/// `receive` / `wire` / `unwire` / `on_dehydrate` / `on_rehydrate` all
/// route through the boxed `ErasedWasmActor`, so a multi-actor instance
/// preserves state across `replace_component` exactly as a single-actor
/// one does (ADR-0101). The `aether.kinds.inputs` section carries every
/// exported type's records, each preceded by an `ActorBoundary`
/// (ADR-0096), so the host can regroup per type and resolve an export
/// selector to a tag.
///
/// ADR-0138: the two forms share one `@shared_body` rule (slot, inline
/// registry, inputs section, `init` / `init_typed`, receive, dehydrate /
/// rehydrate). They differ in exactly three items, emitted by the `@default`
/// / `@no_default` wrapper rules: the default form emits the `aether.namespace`
/// custom section naming the default type and an `init_with_config_p32` that
/// constructs it; the no-default form omits `aether.namespace`, emits the
/// `aether.no_default` marker section instead, and stages a failure from
/// `init_with_config_p32`.
#[doc(hidden)]
#[macro_export]
macro_rules! __export_multi_internal {
    // ADR-0147: `@boot` wrapper — emit the `aether.boot` custom section naming
    // `$boot`'s namespace, then re-dispatch to the bootless `@default` /
    // `@no_default` arm for the `aether.namespace` / `aether.no_default`
    // section, the init shim, and the shared body. Boot is otherwise an
    // ordinary member of `@all`: its type tag, `aether.kinds.inputs`
    // `ActorBoundary`, and spawn-by-tag entry all come from the shared body, so
    // the host constructs the singleton through the same `init_typed_p32` path
    // as any named export. The two marker dimensions (boot present/absent,
    // default present/absent) compose rather than exploding the shared body.
    (@boot $boot:ty ; @default $default:ty ; @all $($component:ty),+ ; @private [$($private:ty),*]) => {
        $crate::__export_multi_internal!(@boot_section $boot);
        $crate::__export_multi_internal!(@default $default ; @all $($component),+ ; @private [$($private),*]);
    };
    (@boot $boot:ty ; @no_default ; @all $($component:ty),+ ; @private [$($private:ty),*]) => {
        $crate::__export_multi_internal!(@boot_section $boot);
        $crate::__export_multi_internal!(@no_default ; @all $($component),+ ; @private [$($private),*]);
    };
    // `@no_boot` wrapper — no boot section, a straight re-dispatch. Its
    // presence makes the four boot × default combinations explicit at the
    // `export!` call site instead of leaving the bootless forms to invoke
    // `@default` / `@no_default` directly.
    (@no_boot ; @default $default:ty ; @all $($component:ty),+ ; @private [$($private:ty),*]) => {
        $crate::__export_multi_internal!(@default $default ; @all $($component),+ ; @private [$($private),*]);
    };
    (@no_boot ; @no_default ; @all $($component:ty),+ ; @private [$($private:ty),*]) => {
        $crate::__export_multi_internal!(@no_default ; @all $($component),+ ; @private [$($private),*]);
    };
    // The `aether.boot` custom section (ADR-0147) — a wasm-target-gated static
    // pinning `$boot`'s `Addressable::NAMESPACE` bytes, byte-for-byte the twin
    // of the `aether.namespace` section the `@default` arm emits below. Its
    // presence is what the host's `read_boot_namespace_from_bytes` reads to
    // find the module's unconditional boot type.
    (@boot_section $boot:ty) => {
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(link_section = "aether.boot")]
        static __AETHER_BOOT_SECTION: [u8; <$boot as $crate::Addressable>::NAMESPACE.len()] = {
            let bytes = <$boot as $crate::Addressable>::NAMESPACE.as_bytes();
            let mut out = [0u8; <$boot as $crate::Addressable>::NAMESPACE.len()];
            let mut i = 0;
            while i < bytes.len() {
                out[i] = bytes[i];
                i += 1;
            }
            out
        };
    };
    // ADR-0138: multi-actor module WITH a default. Emits the
    // `aether.namespace` section (naming `$default`) and parent-aware plus
    // compatibility init exports that construct `$default`, then the shared body.
    (@default $default:ty ; @all $($component:ty),+ ; @private [$($private:ty),*]) => {
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(link_section = "aether.namespace")]
        static __AETHER_NAMESPACE_SECTION: [u8; <$default as $crate::Addressable>::NAMESPACE.len()] = {
            let bytes = <$default as $crate::Addressable>::NAMESPACE.as_bytes();
            let mut out = [0u8; <$default as $crate::Addressable>::NAMESPACE.len()];
            let mut i = 0;
            while i < bytes.len() {
                out[i] = bytes[i];
                i += 1;
            }
            out
        };

        /// # Safety
        /// Parent-aware init ABI; constructs the default (opted-in) export.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(export_name = "init_with_parent_p32")]
        pub unsafe extern "C" fn init_with_parent(
            mailbox_id: u64,
            parent_mailbox_id: u64,
            config_ptr: u32,
            config_len: u32,
        ) -> u32 {
            $crate::wasm::install_guest_logging();
            let config_bytes: &[u8] = if config_len == 0 {
                &[]
            } else {
                // SAFETY: substrate wrote `config_len` bytes at `config_ptr` (ADR-0090).
                unsafe {
                    ::core::slice::from_raw_parts(config_ptr as usize as *const u8, config_len as usize)
                }
            };
            // ADR-0114 addressing amendment: capture the real folded id as the
            // cluster self-identity (correct at any lineage depth).
            __AETHER_INLINE.set_self_id(mailbox_id);
            __AETHER_INLINE.set_parent_id(parent_mailbox_id);
            // Issue 2692: install the by-tag inline-spawn resolver over the
            // module's full exported set (default + rest), so any exported actor
            // can `ctx.spawn_inline_child_by_tag(...)`.
            __AETHER_INLINE.set_spawn_resolver(
                $crate::__export_internal!(@spawn_inline_child_by_tag $($component),+),
            );
            $crate::__export_multi_internal!(@construct $default, mailbox_id, config_bytes)
        }

        /// # Safety
        /// Existing config-bearing init ABI. Older substrates supply no
        /// logical parent, so this forwards with mailbox id `0`.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(export_name = "init_with_config_p32")]
        pub unsafe extern "C" fn init_with_config(mailbox_id: u64, config_ptr: u32, config_len: u32) -> u32 {
            unsafe { init_with_parent(mailbox_id, 0, config_ptr, config_len) }
        }

        $crate::__export_multi_internal!(@shared_body $($component),+ ; @private [$($private),*]);
    };

    // ADR-0138: multi-actor module WITHOUT a default. Omits the
    // `aether.namespace` section, emits the `aether.no_default` marker
    // section, and stages a failure from both default-init exports
    // (the guest-side backstop — the host rejects a bare, defaultless load
    // before it ever reaches this shim). A named load still resolves
    // through `init_typed_p32` in the shared body.
    (@no_default ; @all $($component:ty),+ ; @private [$($private:ty),*]) => {
        // The section-level no-default marker (ADR-0138): a single version
        // byte in `aether.no_default`, wasm-target-gated exactly like the
        // `aether.namespace` section the default form emits. Its presence is
        // what lets the host distinguish a defaultless multi-actor module
        // from a legacy single-actor module.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(link_section = "aether.no_default")]
        static __AETHER_NO_DEFAULT_SECTION: [u8; 1] = [1u8];

        /// # Safety
        /// Parent-aware init ABI on a defaultless module: there is no default to
        /// construct, so stage a failure and return non-zero. This is a
        /// backstop — the host rejects a bare, defaultless load first.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(export_name = "init_with_parent_p32")]
        pub unsafe extern "C" fn init_with_parent(
            _mailbox_id: u64,
            _parent_mailbox_id: u64,
            _config_ptr: u32,
            _config_len: u32,
        ) -> u32 {
            $crate::wasm::install_guest_logging();
            $crate::wasm::stage_init_failure(
                "guest init: module has no default entry (ADR-0138) — load a named export",
            );
            1
        }

        /// # Safety
        /// Existing config-bearing init ABI. It preserves the same staged
        /// no-default failure as the parent-aware export.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(export_name = "init_with_config_p32")]
        pub unsafe extern "C" fn init_with_config(mailbox_id: u64, config_ptr: u32, config_len: u32) -> u32 {
            unsafe { init_with_parent(mailbox_id, 0, config_ptr, config_len) }
        }

        $crate::__export_multi_internal!(@shared_body $($component),+ ; @private [$($private),*]);
    };

    // ADR-0138: the body shared by `@default` and `@no_default` — everything
    // except the `aether.namespace` / `aether.no_default` sections and the
    // default-init shims, which the wrapper rules emit.
    (@shared_body $($component:ty),+ ; @private [$($private:ty),*]) => {
        static __AETHER_MULTI: $crate::Slot<
            $crate::__macro_internals::Box<dyn $crate::ErasedWasmActor>
        > = $crate::Slot::new();

        // ADR-0114: the module's own inline-child registry — one per
        // `export!`, mirroring `__AETHER_MULTI`. The `receive` membrane
        // and the dehydrate / rehydrate shims thread `&__AETHER_INLINE` to
        // the inline-child consumers instead of reaching for a crate-global
        // static.
        static __AETHER_INLINE: $crate::wasm::inline::Registry =
            $crate::wasm::inline::Registry::new();

        // ADR-0096: per-actor `aether.kinds.inputs` section, one
        // `ActorBoundary`-led group per exported type, the default type
        // first, so the host's `read_actor_inputs_from_bytes` regroups the
        // flat record stream into one capability set per type. A
        // single-actor `export!` never reaches this arm, so the
        // boundary-free single-actor layout stays byte-identical.
        $crate::__export_internal!(
            @grouped_inputs __AETHER_MULTI_INPUTS_LEN, __AETHER_INPUTS_SECTION,
            "aether.kinds.inputs" ; $($component),+
        );

        // Issue 6590: the private inline children's groups, in their own
        // section so no reader of `aether.kinds.inputs` sees them.
        $crate::__export_internal!(@private_inputs $($private),*);

        // ADR-0166: concatenate the associated lineage bytes for exactly the
        // types selected by this `export!` invocation. Unlike inputs, lineage
        // records carry both actor tags and namespaces, so no boundary record
        // is needed.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        const __AETHER_MULTI_LINEAGE_LEN: usize =
            0usize $(+ <$component>::__AETHER_LINEAGE_MANIFEST_LEN)+;

        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(link_section = "aether.actor.lineage")]
        static __AETHER_LINEAGE_SECTION: [u8; __AETHER_MULTI_LINEAGE_LEN] = {
            let mut out = [0u8; __AETHER_MULTI_LINEAGE_LEN];
            let mut pos = 0usize;
            $(
                {
                    const MANIFEST_LEN: usize = <$component>::__AETHER_LINEAGE_MANIFEST_LEN;
                    const MANIFEST_BYTES: [u8; MANIFEST_LEN] =
                        <$component>::__AETHER_LINEAGE_MANIFEST;
                    let mut index = 0;
                    while index < MANIFEST_LEN {
                        out[pos] = MANIFEST_BYTES[index];
                        pos += 1;
                        index += 1;
                    }
                }
            )+
            let _ = pos;
            out
        };

        /// # Safety
        /// ADR-0090 legacy zero-config init; forwards to the 3-arg
        /// `init_with_config` the wrapper rule (`@default` / `@no_default`)
        /// emits — construct the default, or stage the no-default failure.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn init(mailbox_id: u64) -> u32 {
            unsafe { init_with_config(mailbox_id, 0, 0) }
        }

        /// # Safety
        /// ADR-0096 typed init: `type_tag` selects which exported type
        /// to construct (its `mailbox_id_from_name(NAMESPACE)`).
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(export_name = "init_typed_with_parent_p32")]
        pub unsafe extern "C" fn init_typed_with_parent(
            mailbox_id: u64,
            parent_mailbox_id: u64,
            type_tag: u64,
            config_ptr: u32,
            config_len: u32,
        ) -> u32 {
            $crate::wasm::install_guest_logging();
            let config_bytes: &[u8] = if config_len == 0 {
                &[]
            } else {
                // SAFETY: substrate wrote `config_len` bytes at `config_ptr` (ADR-0090).
                unsafe {
                    ::core::slice::from_raw_parts(config_ptr as usize as *const u8, config_len as usize)
                }
            };
            // ADR-0114 addressing amendment: capture the real folded id as the
            // cluster self-identity (correct at any lineage depth).
            __AETHER_INLINE.set_self_id(mailbox_id);
            __AETHER_INLINE.set_parent_id(parent_mailbox_id);
            // Issue 2692: install the by-tag inline-spawn resolver over the
            // module's full exported set — the same set this shim selects the
            // constructed type from — so the constructed actor can
            // `ctx.spawn_inline_child_by_tag(...)`.
            __AETHER_INLINE.set_spawn_resolver(
                $crate::__export_internal!(@spawn_inline_child_by_tag $($component),+),
            );
            $(
                if type_tag
                    == $crate::__macro_internals::mailbox_id_from_name(
                        <$component as $crate::Addressable>::NAMESPACE,
                    )
                    .0
                {
                    return $crate::__export_multi_internal!(@construct $component, mailbox_id, config_bytes);
                }
            )+
            $crate::wasm::stage_init_failure(
                "guest init: unknown actor-type tag for multi-actor module",
            );
            1
        }

        /// # Safety
        /// Existing typed-init ABI. Older substrates supply no logical parent,
        /// so this forwards with mailbox id `0`.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(export_name = "init_typed_p32")]
        pub unsafe extern "C" fn init_typed(
            mailbox_id: u64,
            type_tag: u64,
            config_ptr: u32,
            config_len: u32,
        ) -> u32 {
            unsafe { init_typed_with_parent(mailbox_id, 0, type_tag, config_ptr, config_len) }
        }

        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn wire(mailbox_id: u64) -> u32 {
            let Some(instance) = (unsafe { __AETHER_MULTI.get_mut() }) else {
                return 1;
            };
            // ADR-0114 addressing amendment: capture the real folded id (also
            // here, idempotently, so the cluster self-identity is set even if
            // a future host calls `wire` without a prior init on this slot).
            __AETHER_INLINE.set_self_id(mailbox_id);
            // ADR-0112: the boxed `ErasedWasmActor` seam carries the `Manual`
            // view; the synthesized impl downgrades to `Single` per hook.
            let mut ctx = $crate::WasmCtx::__new(mailbox_id, &__AETHER_INLINE, $crate::wasm::NO_INBOUND_SOURCE);
            instance.erased_wire(&mut ctx);
            0
        }

        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn unwire(mailbox_id: u64) -> u32 {
            let Some(instance) = (unsafe { __AETHER_MULTI.get_mut() }) else {
                return 1;
            };
            // ADR-0112: the boxed `ErasedWasmActor` seam carries the `Manual`
            // view; the synthesized impl downgrades to `Single` per hook.
            let mut ctx = $crate::WasmCtx::__new(mailbox_id, &__AETHER_INLINE, $crate::wasm::NO_INBOUND_SOURCE);
            instance.erased_unwire(&mut ctx);
            0
        }

        /// # Safety
        /// FFI receive contract (ADR-0024); routes through the boxed
        /// `ErasedWasmActor`. Self-mailbox id derived from the live
        /// instance's namespace. The trailing `recipient: u64` (ADR-0114
        /// decision #1) carries the routed mailbox through to `Mail`.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(export_name = "receive_p32")]
        pub unsafe extern "C" fn receive(
            kind: u64,
            ptr: u32,
            byte_len: u32,
            count: u32,
            sender: u32,
            recipient: u64,
            source: u64,
        ) -> u32 {
            // ADR-0114 addressing amendment: the cluster self-identity is the
            // real folded id captured at init / wire (correct at any depth),
            // falling back to `hash(NAMESPACE)` only before any shim has run.
            let mailbox_id = {
                let captured = __AETHER_INLINE.self_id();
                if captured != 0 {
                    captured
                } else {
                    let Some(instance) = (unsafe { __AETHER_MULTI.get_mut() }) else {
                        return 1;
                    };
                    $crate::__macro_internals::mailbox_id_from_name(
                        instance.erased_namespace(),
                    )
                    .0
                }
            };
            let mail =
                unsafe { $crate::Mail::__from_raw(kind, ptr, byte_len, count, sender, recipient) };
            // ADR-0114: same receive membrane as the single-actor arm —
            // own id dispatches the default/boxed type, an inline-child
            // alias dispatches the co-located child. ADR-0112: the boxed
            // `ErasedWasmActor` seam carries the `Manual` view; the
            // synthesized impl downgrades to `Single` per hook. The
            // top-level dispatch's borrow is scoped so it is released before
            // the cluster-queue drain, which re-acquires the instance fresh
            // per item.
            let rc = {
                let Some(instance) = (unsafe { __AETHER_MULTI.get_mut() }) else {
                    return 1;
                };
                // Top-level dispatch: the host threads the resolved inbound
                // source on the `receive_p32` ABI (issue 2001), so the ctx
                // carries it directly and `ctx.sender()` is a single
                // field read — the same path the in-place drain takes. The
                // membrane gets the same `source` so a mail routed straight to
                // an inline-child alias (ADR-0114) hands the child its source
                // too, not just the cluster root.
                $crate::wasm::inline::membrane_dispatch(mailbox_id, mail, &__AETHER_INLINE, source, move |__aether_mail| {
                    let mut ctx = $crate::WasmCtx::__new(mailbox_id, &__AETHER_INLINE, source);
                    instance.erased_dispatch(&mut ctx, __aether_mail)
                })
            };
            // ADR-0114 addressing amendment: drain every intra-cluster send
            // in place under this one run-token; each item re-acquires the
            // boxed instance inside the per-item `dispatch_own` factory, which
            // the drain hands the item's inbound source (`__aether_source`,
            // issue 1987) so the own-path ctx matches the child path.
            $crate::wasm::inline::drain_cluster_queue(&__AETHER_INLINE, |__aether_source| {
                move |__aether_mail| {
                    // SAFETY: re-acquired fresh per drained item; the prior
                    // iteration's borrow has dropped (the membrane returned).
                    let instance = unsafe { __AETHER_MULTI.get_mut() }
                        .expect("instance present for the cluster-queue drain");
                    let mut ctx = $crate::WasmCtx::__new_local_dispatch(mailbox_id, &__AETHER_INLINE, __aether_source);
                    instance.erased_dispatch(&mut ctx, __aether_mail)
                }
            });
            rc
        }

        /// ADR-0095 guest allocator — identical to the single-actor arm.
        ///
        /// # Safety
        /// Called by the substrate per the layout contract.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(export_name = "realloc_p32")]
        pub unsafe extern "C" fn realloc_p32(
            old_ptr: u32,
            old_size: u32,
            align: u32,
            new_size: u32,
        ) -> u32 {
            unsafe {
                $crate::wasm::guest_alloc::realloc_bytes(
                    old_ptr as *mut u8,
                    old_size as usize,
                    align as usize,
                    new_size as usize,
                )
                .addr() as u32
            }
        }

        /// # Safety
        /// Called by the substrate exactly once, on the old instance,
        /// immediately before a `replace_component` swap. Routes through
        /// the boxed `ErasedWasmActor` to the live type's
        /// [`$crate::WasmActor::on_dehydrate`] (ADR-0101).
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn on_dehydrate() -> u32 {
            let Some(instance) = (unsafe { __AETHER_MULTI.get_mut() }) else {
                return 1;
            };
            // ADR-0114 addressing amendment: the cluster self-identity is the
            // real folded id captured at `init` / `wire` — the same id
            // `receive` derives for `WasmCtx`, so a `send::<R>` from the save
            // hook resolves correctly at any lineage depth. Fall back to
            // `hash(namespace)` only before any shim has run.
            let mailbox_id = {
                let captured = __AETHER_INLINE.self_id();
                if captured != 0 {
                    captured
                } else {
                    $crate::__macro_internals::mailbox_id_from_name(
                        instance.erased_namespace(),
                    )
                    .0
                }
            };
            // ADR-0114 §5: compose the parent + every inline child into one
            // composite, then `save_state` once (the boxed instance's
            // dehydrate routes through `erased_on_dehydrate`). Childless ⇒
            // byte-identical to the boxed parent's own blob.
            let __aether_user_state = $crate::wasm::inline::compose::dehydrate(
                mailbox_id,
                &__AETHER_INLINE,
                |ctx| instance.erased_on_dehydrate(ctx),
            );
            let __aether_state = __AETHER_INLINE.compose_request_context_state(__aether_user_state);
            if let Some((version, bytes)) = __aether_state {
                let mut ctx: $crate::WasmDropCtx<'_> =
                    $crate::WasmDropCtx::__new(mailbox_id, __AETHER_INLINE.parent_id_for(mailbox_id));
                ctx.save_state(version, &bytes);
            }
            0
        }

        /// # Safety
        /// Called by the substrate after `init` on a freshly
        /// instantiated replacement, with `(version, ptr, len)`
        /// describing the prior-state bundle the old instance produced.
        /// Routes through the boxed `ErasedWasmActor` to the live type's
        /// [`$crate::WasmActor::on_rehydrate`] (ADR-0101). Self-mailbox id
        /// is the captured folded id, with the live instance's namespace
        /// hash as the pre-shim fallback.
        #[cfg(all(target_family = "wasm", not(feature = "library")))]
        #[unsafe(export_name = "on_rehydrate_p32")]
        pub unsafe extern "C" fn on_rehydrate(version: u32, ptr: u32, len: u32) -> u32 {
            let Some(instance) = (unsafe { __AETHER_MULTI.get_mut() }) else {
                return 1;
            };
            // ADR-0114 addressing amendment: self_id-first, name-hash
            // fallback — the same derivation as `receive` / `on_dehydrate`.
            let mailbox_id = {
                let captured = __AETHER_INLINE.self_id();
                if captured != 0 {
                    captured
                } else {
                    $crate::__macro_internals::mailbox_id_from_name(
                        instance.erased_namespace(),
                    )
                    .0
                }
            };
            // ADR-0114 §5: decompose, restore the boxed parent, then
            // reconstruct each inline child by matching its type tag against
            // every exported type, then every private type. Childless ⇒ the
            // boxed parent sees the identical `PriorState`.
            let prior_bytes: &[u8] = if len == 0 {
                &[]
            } else {
                // SAFETY: substrate wrote `len` bytes at `ptr` (the rehydrate
                // ABI); the slice is bounded by this call.
                unsafe { ::core::slice::from_raw_parts(ptr as usize as *const u8, len as usize) }
            };
            let (__aether_contexts, __aether_user_version, __aether_user_bytes) =
                $crate::split_state_envelope(version, prior_bytes);
            __AETHER_INLINE.restore_request_contexts(__aether_contexts);
            $crate::wasm::inline::compose::reconstruct_inline_children(
                __aether_user_version,
                &__aether_user_bytes,
                &__AETHER_INLINE,
                |parent_version, parent_bytes| {
                    // ADR-0112: the boxed `ErasedWasmActor` seam carries the
                    // `Manual` view; the synthesized impl downgrades per hook.
                    let mut ctx = $crate::WasmCtx::__new(mailbox_id, &__AETHER_INLINE, $crate::wasm::NO_INBOUND_SOURCE);
                    // SAFETY: `parent_bytes` lives for this closure call.
                    let parent_prior = unsafe {
                        $crate::PriorState::__from_ptr(
                            parent_version,
                            parent_bytes.as_ptr() as usize,
                            parent_bytes.len(),
                        )
                    };
                    instance.erased_on_rehydrate(&mut ctx, parent_prior);
                },
                |registry, parent, child| {
                    $crate::__export_internal!(@reconstruct_child registry, parent, child ; [$($component),+] ; [$($private),*])
                },
            );
            0
        }
    };

    // Resolve `$ty`'s Config from `$config_bytes`, run its `init`, and box
    // the result into `__AETHER_MULTI`. Empty bytes use `Config::default()`;
    // a non-empty decode/init failure stages the message and `return 1`.
    (@construct $ty:ty, $mailbox_id:ident, $config_bytes:ident) => {{
        let config = if $config_bytes.is_empty() {
            <<$ty as $crate::Lifecycle<$ty>>::Config as ::core::default::Default>::default()
        } else {
            let Some(config) = <
                <$ty as $crate::Lifecycle<$ty>>::Config as $crate::__macro_internals::Kind
            >::decode_from_bytes($config_bytes) else {
                $crate::wasm::stage_init_failure(::core::concat!(
                    "guest init: ",
                    ::core::stringify!($ty),
                    " could not decode Config from bytes",
                ));
                return 1;
            };
            config
        };
        // ADR-0156 §2: empty params for now — resolve `Params` to its
        // compiled default, mirroring the empty-config path above.
        let params = <<$ty as $crate::Lifecycle<$ty>>::Params as ::core::default::Default>::default();
        let mut ctx: $crate::WasmInitCtx<'_> = $crate::WasmInitCtx::__new($mailbox_id);
        match <$ty as $crate::Lifecycle<$ty>>::init(config, params, &mut ctx) {
            ::core::result::Result::Ok(instance) => {
                __AETHER_INLINE.set_entry_actor_tag($crate::ActorTypeTag::of::<$ty>());
                unsafe {
                    __AETHER_MULTI.set(
                        $crate::__macro_internals::Box::new(instance)
                            as $crate::__macro_internals::Box<dyn $crate::ErasedWasmActor>,
                    );
                }
                0
            }
            ::core::result::Result::Err(err) => {
                $crate::wasm::stage_init_failure(err.message());
                1
            }
        }
    }};
}
