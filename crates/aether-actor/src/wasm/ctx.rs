// Wire-encode: `usize → u32` narrowings forward `(ptr, len)` pairs
// to the wasm32 host-fn ABI (`_p32` convention, ADR-0024).
#![allow(clippy::cast_possible_truncation)]

//! Concrete wasm ctx structs — [`WasmInitCtx`] / [`WasmCtx`] / [`WasmDropCtx`].
//!
//! The ctx interface is spelled by the per-stage capability traits in
//! [`crate::model::ctx`]; these structs are concrete impls that route
//! outbound calls through the per-concern bridge functions in
//! `crate::wasm::bridge::mail` and `crate::wasm::bridge::persist`.
//! Ctxs hold per-mail state only (mailbox id at init; reply target at
//! receive), and dispatch goes through the bridge functions directly.

use core::cell::OnceCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::ptr;

use aether_data::{Kind, MailboxId, RequestId, Source, mailbox_id_from_name};

use crate::asset::{AssetCatalog, AssetInfo, AssetWindow};
use crate::wasm::bridge::asset;

use crate::mail::ReplyHandle;
use crate::mail::mailbox::{KindId, Mailbox, resolve, resolve_mailbox};
use crate::model::ctx::emit::Emit;
use crate::model::ctx::mail_sender::MailSender;
use crate::model::ctx::outbound_reply::OutboundReply;
use crate::model::ctx::persistence::Persistence;
use crate::model::ctx::reply_mode::{Manual, Multi, ReplyMode, Single};
use crate::model::{
    Addressable, HandlesKind, Instanced, NamespaceError, Singleton, Subname, validate_namespace_segment,
};
use crate::wasm::bridge::{mail, persist};
use crate::wasm::inline::{ChainMode, Registry, RouteDecision};
use crate::wasm::mailbox::WasmActorMailbox;
use crate::wasm::{ActorInitError, ErasedWasmActor, WasmActor};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// Init-only capability handle for FFI guests. Resolved during
/// `WasmActor::init`; not available at runtime (the type split fences
/// "when can I resolve?" against "when can I send?" at compile time).
// The `Wasm` prefix carries the native/wasm split signal; bare `InitCtx` loses that.
#[allow(clippy::module_name_repetitions)]
pub struct WasmInitCtx<'a> {
    mailbox: u64,
    /// ADR-0163 §3 asset catalog, fetched lazily on the first
    /// [`AssetCatalog::assets`] call and cached for the ctx's life —
    /// `init` is inside the load window, so asset access is live here.
    catalog: OnceCell<Vec<AssetInfo>>,
    _borrow: PhantomData<&'a ()>,
}

impl WasmInitCtx<'_> {
    /// Not part of the public API; called only by [`crate::export!`].
    #[doc(hidden)]
    #[must_use]
    pub fn __new(mailbox: u64) -> Self {
        Self { mailbox, catalog: OnceCell::new(), _borrow: PhantomData }
    }

    /// The component's own mailbox id — the value the substrate uses to
    /// address `receive` calls to this instance.
    #[must_use]
    pub fn mailbox_id(&self) -> MailboxId {
        MailboxId(self.mailbox)
    }

    /// Resolve a kind by its `const ID`. Pure compile-time construction
    /// under ADR-0030 Phase 2 — no host-fn round trip, never fails.
    #[must_use]
    pub const fn resolve<K: Kind>(&self) -> KindId<K> {
        resolve::<K>()
    }

    /// Resolve a mailbox by name and bind it to kind `K`, producing a
    /// typed [`Mailbox<K>`]. Pure compile-time construction; the returned
    /// token is pure addressing.
    #[must_use]
    pub const fn resolve_mailbox<K: Kind>(&self, name: &str) -> Mailbox<K> {
        resolve_mailbox::<K>(name)
    }

    // Issue 1987: the init ctx exposes no `actor()` / `resolve_actor()`
    // sender shortcut. A `WasmActorMailbox` is now a ctx-bound sender that
    // routes through the per-component inline registry, which the init
    // stage does not hold — and init is mail-forbidden anyway (the ctx
    // carries no send surface by design). Addressing + sending begin at
    // `wire`, where `WasmCtx` carries the registry.
}

impl AssetCatalog for WasmInitCtx<'_> {
    fn assets(&self) -> &[AssetInfo] {
        self.catalog.get_or_init(asset::fetch_catalog).as_slice()
    }
}

impl AssetWindow for WasmInitCtx<'_> {
    fn asset(&mut self, name: &str) -> Option<Vec<u8>> {
        asset::fetch_asset(name)
    }
}

/// The window-bearing context `wire` receives (ADR-0163 §3). A thin
/// borrow-wrapper around the post-init [`WasmCtx`] that `Deref`s to it, so
/// every send / subscribe / resolve verb a `wire` body already uses keeps
/// working unchanged through the deref. Its reason to exist is the asset
/// load window: `WireCtx` is the ctx type through which an actor reads the
/// bytes it ships in `aether.asset.<path>` custom sections
/// ([`crate::AssetWindow`]), and taking it — rather than a bare
/// [`WasmCtx`] — is what makes "fetch an asset after the window closed" a
/// compile error (a handler is handed a [`WasmCtx`], which carries no
/// asset surface).
///
/// This slice lands the type and its `wire`-signature sweep; the guest
/// transport that fills the window across the FFI (delivering the catalog
/// and serving `asset(name)` inside a wasm guest) is the named follow-up.
/// Until it lands the wrapper adds no methods of its own — the breaking
/// signature change is taken now, while no bundle actor yet depends on the
/// payload path, so a later slice fills the window without re-breaking
/// every `wire`.
///
/// Two lifetimes: `'ctx` is the borrow of the underlying ctx the FFI
/// membrane owns for the call, `'a` is that ctx's own lifetime. The
/// `#[actor]` macro constructs this around the `WasmCtx` it already builds
/// for `wire`, so authors only ever name it as `&mut WireCtx<'_, '_>`.
// The `Wire` prefix carries the load-window signal; a bare `Ctx` would lose it.
#[allow(clippy::module_name_repetitions)]
pub struct WireCtx<'ctx, 'a> {
    inner: &'ctx mut WasmCtx<'a>,
    /// ADR-0163 §3 asset catalog, fetched lazily on the first
    /// [`AssetCatalog::assets`] call and cached for the ctx's life. A
    /// `wire` body that never enumerates assets pays no hostcall; one that
    /// only pulls by name (`asset(name)`) never touches this cell.
    catalog: OnceCell<Vec<AssetInfo>>,
}

impl<'ctx, 'a> WireCtx<'ctx, 'a> {
    /// Not part of the public API; called only by the `#[actor]` macro's
    /// `wire` forwarder, which wraps the [`WasmCtx`] it builds for the
    /// lifecycle call.
    #[doc(hidden)]
    #[must_use]
    pub fn __new(inner: &'ctx mut WasmCtx<'a>) -> Self {
        Self { inner, catalog: OnceCell::new() }
    }
}

impl<'a> Deref for WireCtx<'_, 'a> {
    type Target = WasmCtx<'a>;
    fn deref(&self) -> &WasmCtx<'a> {
        self.inner
    }
}

impl<'a> DerefMut for WireCtx<'_, 'a> {
    fn deref_mut(&mut self) -> &mut WasmCtx<'a> {
        self.inner
    }
}

impl AssetCatalog for WireCtx<'_, '_> {
    fn assets(&self) -> &[AssetInfo] {
        self.catalog.get_or_init(asset::fetch_catalog).as_slice()
    }
}

impl AssetWindow for WireCtx<'_, '_> {
    fn asset(&mut self, name: &str) -> Option<Vec<u8>> {
        asset::fetch_asset(name)
    }
}

/// A type-erased sendable handle to a cluster relative — the parent,
/// a sibling, or a child of the addressing actor (ADR-0114 addressing
/// amendment). Returned by [`WasmCtx::parent`] / [`WasmCtx::sibling`] /
/// [`WasmCtx::child`], it wraps the relative's resolved [`MailboxId`] (looked
/// up in the per-component inline registry, never folded) plus the registry
/// the send routes through.
///
/// Unlike [`WasmActorMailbox`] this carries no receiver type and no
/// `R: HandlesKind<K>` bound — relative addressing is positional, so the
/// target's handler set is not known at the call site (the by-id counterpart
/// of the runtime-name `send_to_named` escape hatch). The send routes through
/// the inline registry's cluster router: a cluster-member recipient (which a
/// resolved relative always is) dispatches in place via the queue + drain,
/// never the scheduler.
pub struct RelativeMailbox<'a> {
    id: MailboxId,
    /// The addressing actor's own folded [`MailboxId`] raw value — the "from"
    /// half stamped on the in-place send so the relative recipient's
    /// `ctx.source_mailbox()` resolves who sent it. Set by
    /// [`WasmCtx::parent`] / [`WasmCtx::child`] / [`WasmCtx::sibling`] to the
    /// resolving ctx's `mailbox`.
    sender: u64,
    inline: &'a Registry,
}

impl RelativeMailbox<'_> {
    /// The relative's resolved [`MailboxId`].
    #[must_use]
    pub fn mailbox_id(&self) -> MailboxId {
        self.id
    }

    /// Resolve a sendable handle to this relative's inline child whose
    /// subname is `name`, preserving the original addresser for any send
    /// through the returned handle. This is the multi-hop continuation of
    /// [`WasmCtx::child`].
    #[must_use]
    pub fn child(&self, name: &str) -> Option<Self> {
        let id = self.inline.child_of(self.id, name)?;
        Some(RelativeMailbox { id, sender: self.sender, inline: self.inline })
    }

    /// Send `payload` to this relative, routed in place through the cluster
    /// membrane (queue + drain) — no scheduler hop. Inherits the handler's
    /// in-flight causal chain (the default, ADR-0080 §7); the local path
    /// carries no host trace ids, so the flag is moot for an in-cluster
    /// send.
    pub fn send<K: Kind>(&self, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(self.id.0, K::ID.0, &bytes, 1, ChainMode::Inherit, self.sender);
    }

    /// Forward pre-encoded `bytes` of kind `kind` to this relative — the
    /// type-erased counterpart of [`Self::send`], for a mail-forwarding
    /// interposer (the ADR-0137 behavior host, issue 2687) that reroutes an
    /// arbitrary inbound kind it holds no Rust type for. Routes the same way
    /// [`Self::send`] does — in place through the cluster membrane, inheriting
    /// the handler's causal chain — so the interposer stays transparent to
    /// settlement. `count` is fixed at 1: a forward carries one inbound mail.
    pub fn send_bytes(&self, kind: aether_data::KindId, bytes: &[u8]) {
        self.inline.route_or_enqueue(self.id.0, kind.0, bytes, 1, ChainMode::Inherit, self.sender);
    }

    /// Fire-and-forget send to this relative (ADR-0080 §7 detach signal).
    /// In-cluster the recipient dispatches in place regardless; the detach
    /// flag rides through only on the cross-cluster fallback path, which a
    /// resolved relative never takes.
    pub fn send_detached<K: Kind>(&self, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(self.id.0, K::ID.0, &bytes, 1, ChainMode::Detached, self.sender);
    }
}

/// A runtime selector for one of a module's `export!`ed actor types — the
/// `hash(NAMESPACE)` folded id [`WasmCtx::spawn_inline_child_by_tag`]
/// resolves against the module's exported set (issue 2692), and the same
/// tag the ADR-0114 §5 reconstruct arm matches a persisted inline child on.
///
/// A newtype rather than a bare `u64` on purpose: it centralizes the single
/// allowed [`mailbox_id_from_name`] call (every other call site is
/// clippy-disallowed) in [`Self::of`], so a consumer selects a type with
/// `ActorTypeTag::of::<SomeActor>()` and never hand-hashes a namespace. It
/// also reads as an actor-type selector, distinct from a [`MailboxId`] even
/// though the underlying hash coincides with the type's depth-1 folded id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActorTypeTag(pub u64);

impl ActorTypeTag {
    /// The actor-type tag for `A` — `hash(A::NAMESPACE)`, folded at compile
    /// time (`Addressable::NAMESPACE` is a `const`). The one sanctioned
    /// [`mailbox_id_from_name`] call outside the id/routing core: it is the
    /// id definition for an actor *type*, so the disallowed-method allow
    /// mirrors [`WasmCtx::spawn_child`] / [`WasmCtx::spawn_inline_child`].
    #[must_use]
    // This is the id definition for an actor type — the single centralized
    // `mailbox_id_from_name` call the by-tag spawn API is built to funnel, so
    // consumers never hand-hash a namespace (all other call sites are
    // clippy-disallowed).
    #[allow(clippy::disallowed_methods)]
    pub const fn of<A: Addressable>() -> Self {
        Self(mailbox_id_from_name(A::NAMESPACE).0)
    }
}

/// Why a synchronous spawn verb failed.
///
/// For the detached [`WasmCtx::spawn_child`] (ADR-0097), only subname
/// validation can fail here — a spawn-time failure (a retired / in-use
/// subname, or the sibling's `init` returning `Err`) surfaces
/// asynchronously on the trampoline, not through this `Result`. For the
/// inline [`WasmCtx::spawn_inline_child`] (ADR-0114) the child's `init`
/// runs in-process, synchronously, so its failure is reported here as
/// [`SpawnError::InitFailed`].
#[derive(Debug, Clone)]
pub enum SpawnError {
    /// A [`Subname::Named`] discriminator failed
    /// [`validate_namespace_segment`].
    SubnameInvalid(NamespaceError),
    /// ADR-0114: an inline child's synchronous `init` returned `Err`. The
    /// wrapped [`ActorInitError`] carries the actor's own failure message.
    /// Unlike the detached `spawn_child` — whose `init` runs later on the
    /// trampoline and logs asynchronously — an inline child's `init` runs
    /// in-guest during [`WasmCtx::spawn_inline_child`], so the boot failure
    /// comes back through this `Result`.
    InitFailed(ActorInitError),
    /// Issue 2692: [`WasmCtx::spawn_inline_child_by_tag`] was handed an
    /// [`ActorTypeTag`] that matched none of the module's `export!`ed actor
    /// types (a stale spec, a script, a tag for a type dropped from the
    /// module). The tag is runtime data, so an unresolvable one is a runtime
    /// error the spawner recovers from rather than a panic — and no host
    /// alias is allocated for it (the export-set fall-through precedes
    /// allocation).
    UnknownActorTag(ActorTypeTag),
}

/// Per-receive (and post-init `wire` / pre-shutdown `unwire`)
/// capability handle for FFI guests. Exposes send, reply, and the
/// inherent [`mailbox_id`](WasmCtx::mailbox_id) for cases that need to
/// address this component explicitly.
// The `Wasm` prefix carries the native/wasm split signal; bare `Ctx` loses that.
#[allow(clippy::module_name_repetitions)]
pub struct WasmCtx<'a, M: ReplyMode = Single> {
    mailbox: u64,
    sender: Option<ReplyHandle>,
    /// The inbound source — the folded [`MailboxId`] raw value of whoever
    /// sent the mail currently being dispatched, threaded onto the ctx at
    /// construction (issues 1987 + 2001). For an in-place (intra-cluster)
    /// dispatch off the drain this is the enqueuing member's id (the in-place
    /// reply table is empty, so the ctx is the only carrier); for a top-level
    /// dispatch the host resolves the source from the inbound's `SourceAddr`
    /// and threads it as the trailing `receive_p32` ABI slot. So
    /// [`Self::source_mailbox`] is a single read of this field on both paths.
    /// [`MailboxId::NONE`] (`0`) means no peer-component origin — a session,
    /// remote-engine, or broadcast mail, or a lifecycle hook with no inbound.
    source: u64,
    /// Whether this ctx came from a top-level host dispatch. Cluster-drained
    /// in-place dispatches carry no host correlation, so `in_reply_to` must
    /// not read the outer dispatch's ambient host scalar.
    host_dispatch: bool,
    /// ADR-0114: the per-component inline-child registry the
    /// [`Self::spawn_inline_child`] / [`Self::despawn_inline_child`] verbs
    /// drive. The `export!` membrane threads in the component's emitted
    /// `static __AETHER_INLINE` (a `&'static` that coerces to `&'a`); a
    /// host unit test threads in a local registry. Held by reference
    /// rather than reached as a global — the same discipline the parent
    /// slot (`__AETHER_COMPONENT`) already follows.
    inline: &'a Registry,
    _borrow: PhantomData<&'a ()>,
    /// ADR-0112: phantom reply-mode marker (a ZST, layout-neutral) that
    /// selects which reply surface this ctx exposes. Defaults to
    /// [`Single`], so the common `WasmCtx<'_>` signature is unchanged.
    _mode: PhantomData<M>,
}

/// The `source` argument to [`WasmCtx::__new`] for a dispatch that carries no
/// inbound source — a lifecycle hook (`wire` / `unwire` / `on_rehydrate`),
/// where [`WasmCtx::source_mailbox`] returns `None`. Equals [`MailboxId::NONE`].
/// (A top-level mail dispatch threads the host-resolved source over the
/// `receive_p32` ABI; the drained-member path threads the enqueuing member's
/// own id.) Named so the `__new` call sites read intent, not a bare `0`.
#[doc(hidden)]
pub const NO_INBOUND_SOURCE: u64 = MailboxId::NONE.0;

impl<'a> WasmCtx<'a, Manual> {
    /// Not part of the public API; called only by [`crate::export!`] and
    /// the inline membrane / drain. The runtime builds the most-permissive
    /// [`Manual`] view; the `#[actor]` dispatcher / lifecycle shims
    /// downgrade it per handler class with [`Self::as_single`].
    ///
    /// `source` is the inbound source (issues 1987 + 2001): the enqueuing
    /// member's id for an in-place drained dispatch, the host-resolved source
    /// for a top-level mail dispatch (threaded over the `receive_p32` ABI), or
    /// [`MailboxId::NONE`] (`0`) for a lifecycle hook with no inbound mail.
    #[doc(hidden)]
    #[must_use]
    pub fn __new(mailbox: u64, inline: &'a Registry, source: u64) -> Self {
        Self { mailbox, sender: None, source, host_dispatch: true, inline, _borrow: PhantomData, _mode: PhantomData }
    }

    /// Not part of the public API; inline-cluster drains build ctxs through
    /// here so `in_reply_to()` does not read the outer host dispatch's stale
    /// reply-correlation scalar.
    #[doc(hidden)]
    #[must_use]
    pub fn __new_local_dispatch(mailbox: u64, inline: &'a Registry, source: u64) -> Self {
        Self { mailbox, sender: None, source, host_dispatch: false, inline, _borrow: PhantomData, _mode: PhantomData }
    }

    /// ADR-0112 downgrade-only coercion: view this [`Manual`] ctx as a
    /// [`Single`] ctx, dropping the `OutboundReply` surface. The
    /// `#[actor]` macro hands a single-class handler this view, so a
    /// handler whose marker disagrees with its class fails to unify.
    /// There is deliberately no `as_manual` — the runtime only ever
    /// downgrades.
    #[doc(hidden)]
    #[must_use]
    pub fn as_single(&mut self) -> &mut WasmCtx<'a, Single> {
        // SAFETY: `M` is `PhantomData`-only, so `WasmCtx<'a, Manual>` and
        // `WasmCtx<'a, Single>` are layout-identical (the marker field is a
        // ZST for every `M` — see `reply_mode_types_are_zsts` and
        // `ffi_ctx_layout_identical_across_modes`). The reborrow swaps the
        // marker without touching any real field and only removes
        // capability, never adds it.
        unsafe { &mut *ptr::from_mut(self).cast::<WasmCtx<'a, Single>>() }
    }

    /// ADR-0134 downgrade-only coercion: view this [`Manual`] ctx as a
    /// [`Multi<K>`] ctx, swapping the `OutboundReply` surface for the
    /// [`Emit<K>`] surface. The `#[actor]` macro hands a `#[handler::multi]`
    /// handler this view (with `K` read off its `Multi<K>` signature), so a
    /// handler whose marker disagrees with its class fails to unify.
    #[doc(hidden)]
    #[must_use]
    pub fn as_multi<K: Kind>(&mut self) -> &mut WasmCtx<'a, Multi<K>> {
        // SAFETY: `M` is `PhantomData`-only and `Multi<K>` is a ZST for every
        // `K`, so `WasmCtx<'a, Manual>` and `WasmCtx<'a, Multi<K>>` are
        // layout-identical (see `reply_mode_types_are_zsts` and
        // `ffi_ctx_layout_identical_across_modes`). The reborrow swaps the
        // marker without touching any real field.
        unsafe { &mut *ptr::from_mut(self).cast::<WasmCtx<'a, Multi<K>>>() }
    }
}

impl<M: ReplyMode> WasmCtx<'_, M> {
    /// Not part of the public API; called only by the `#[actor]`
    /// dispatcher. Accepts `None` or `Some(ReplyHandle)` — the dispatcher
    /// passes `mail.reply_handle()` verbatim so component-origin and
    /// broadcast mail (which have no reply target) land as `None`.
    #[doc(hidden)]
    pub fn __set_reply_to(&mut self, sender: Option<ReplyHandle>) {
        self.sender = sender;
    }

    /// Reply target for the mail currently being dispatched. Mirrors
    /// [`OutboundReply::reply_target`].
    #[must_use]
    pub fn reply_target(&self) -> Option<ReplyHandle> {
        self.sender
    }

    /// The inbound source — the folded [`MailboxId`] of whoever sent the
    /// mail currently being dispatched, or `None` for a sourceless dispatch
    /// (session / remote-engine / broadcast mail, or a lifecycle hook with
    /// no inbound). Reads the same `source` field as
    /// [`OutboundReply::source_mailbox`], but on the generic ctx so a
    /// `#[fallback]` (which runs on the downgraded [`Single`] view, issue
    /// 2687) can resolve a cluster-membrane interposer's lane direction —
    /// compare the source against a stored child id: equal ⇒ up-lane, else
    /// ⇒ down-lane.
    #[must_use]
    pub fn source_mailbox(&self) -> Option<MailboxId> {
        (self.source != MailboxId::NONE.0).then_some(MailboxId(self.source))
    }

    /// Correlation id of the request this inbound reply answers.
    ///
    /// Returns `None` for ordinary request mail, uncorrelated replies, and
    /// inline-cluster drained dispatches.
    #[must_use]
    pub fn in_reply_to(&self) -> Option<RequestId> {
        if !self.host_dispatch {
            return None;
        }
        let correlation = mail::reply_correlation();
        (correlation != Source::NO_CORRELATION).then_some(RequestId(correlation))
    }

    /// Recover and remove the typed context for the request this inbound reply
    /// answers. Returns `None` for ordinary mail, unmatched replies, wrong
    /// context kind, or decode failure.
    pub fn take_context<C: Kind>(&mut self) -> Option<C> {
        let request = self.in_reply_to()?;
        // SAFETY: the macro-emitted registry is accessed only under the
        // serialized wasm guest entrypoint.
        unsafe { self.inline.request_contexts_mut().take(request) }
    }

    /// The component's own mailbox id — the value the substrate uses to
    /// address `receive` calls to this instance. Typed subscription facades
    /// self-address through their context-bound actor mailbox.
    #[must_use]
    pub fn mailbox_id(&self) -> MailboxId {
        MailboxId(self.mailbox)
    }

    /// Singleton sender shortcut. Returns a ctx-bound [`WasmActorMailbox`]
    /// addressing the unique instance of receiver actor `R`, carrying this
    /// actor's own id as the send's `from` (issue 1987) and a borrow of
    /// the inline registry the send routes through.
    #[must_use]
    pub fn actor<R: Singleton>(&self) -> WasmActorMailbox<'_, R> {
        WasmActorMailbox::__new(R::resolve(self.mailbox, ()).0, self.mailbox, self.inline)
    }

    /// Multi-instance sender. Resolve a ctx-bound [`WasmActorMailbox`]
    /// from a runtime instance name, carrying this actor's own id as the
    /// send's `from` and the inline registry the send routes through.
    // Runtime-name escape hatch: the instance name is only known at runtime,
    // so there is no `R::resolve` lineage carry to route through.
    #[must_use]
    #[allow(clippy::disallowed_methods)]
    pub fn resolve_actor<R: Addressable>(&self, name: &str) -> WasmActorMailbox<'_, R> {
        WasmActorMailbox::__new(mailbox_id_from_name(name).0, self.mailbox, self.inline)
    }

    /// ADR-0063 fail-fast: bring the substrate down with `reason`.
    /// Diverging — does not return. The body `panic!`s; the substrate's
    /// wasm runtime catches the trap and ADR-0063 escalates the
    /// substrate-side `fatal_abort` path. Symmetric to
    /// `aether_substrate::actor::native::NativeCtx::fatal_abort` so
    /// trap-escalation reads the same on both sides.
    ///
    /// # Panics
    /// Always panics — that's the point. The trap propagates to the
    /// substrate's ADR-0063 fail-fast escalation path.
    // Mirrors `aether_substrate::actor::native::NativeCtx::fatal_abort`
    // — `reason` is owned because callers `format!(...)` inline and the
    // diverging body means no further use.
    #[allow(clippy::needless_pass_by_value)]
    pub fn fatal_abort(&self, reason: String) -> ! {
        panic!("aether-actor: fatal_abort: {reason}")
    }

    /// ADR-0097: spawn a sibling actor type from the same resident
    /// module — the wasm analogue of native `ctx.spawn_child::<A>`. `A`
    /// is one of this module's exported `Instanced` types; the SDK
    /// resolves its actor-type tag (`mailbox_id_from_name(A::NAMESPACE)`)
    /// and encodes `A::Config`, both at compile time. Returns the new
    /// instance's [`MailboxId`] synchronously — it is `hash(name)`
    /// (ADR-0029) — and the instance becomes addressable at
    /// `aether.embedded:<name>`.
    ///
    /// Only synchronous subname validation can `Err` here; a spawn-time
    /// failure (a retired / in-use subname, or the sibling's `init`
    /// returning `Err`) is logged on the trampoline and does not come
    /// back through this `Result` (ADR-0097 §4). The spawned sibling's
    /// `Source` is this actor's mailbox, so its replies route here.
    pub fn spawn_child<A>(&self, subname: Subname<'_>, config: &A::Config) -> Result<MailboxId, SpawnError>
    where
        A: Instanced + WasmActor,
    {
        // Compile-time actor-type tag for the spawned sibling (hash(NAMESPACE),
        // ADR-0029) — this is the id definition for the new instance, computed
        // before any lineage carry exists.
        #[allow(clippy::disallowed_methods)]
        let type_tag = mailbox_id_from_name(<A as Addressable>::NAMESPACE).0;
        let (is_counter, full_subname) = resolve_subname(subname)?;
        let config_bytes = config.encode_into_bytes();
        let id = mail::spawn_sibling(type_tag, is_counter, &full_subname, &config_bytes);
        Ok(MailboxId(id))
    }

    /// ADR-0114: spawn an **inline child** — a co-located child actor that
    /// shares this component's WASM instance, slot, and run-token, while
    /// being addressed and mailed like any actor. The signature mirrors
    /// [`Self::spawn_child`] (a `Subname`-discriminated `Instanced` type);
    /// the only difference is co-residency.
    ///
    /// The host folds the child's alias [`MailboxId`]
    /// (`{parent}/aether.embedded:<subname>`) and registers a route to
    /// this trampoline's own slot; the SDK then runs `A::init`
    /// **synchronously** (unlike the detached `spawn_child`, whose `init`
    /// runs later on a fresh trampoline) and inserts the boxed child into
    /// this ctx's per-component [`Registry`] keyed by the alias. Mail
    /// addressed to the alias lands in this slot and the `export!`
    /// membrane demuxes it to the child; the child's own sends stamp the
    /// child's address as origin and its replies route back.
    ///
    /// A [`Subname::Named`] that fails validation returns
    /// [`SpawnError::SubnameInvalid`]; a synchronous `init` `Err` returns
    /// [`SpawnError::InitFailed`].
    ///
    /// The alias is folded on the instance carry (flat), so a child's
    /// subname must be unique within the whole cluster — two children that
    /// resolve to the same `aether.embedded:<subname>` collide on one alias.
    /// The spawning actor's real id is recorded as the child's logical
    /// parent so relative addressing (`ctx.parent()` / `ctx.sibling(name)` /
    /// `ctx.child(name)`) resolves over the registry. Per-parent subname
    /// scoping (the nested-alias fold, ADR-0117) is a follow-up needing a
    /// substrate change.
    pub fn spawn_inline_child<A>(&self, subname: Subname<'_>, config: &A::Config) -> Result<MailboxId, SpawnError>
    where
        // `ErasedWasmActor` is the boxing seam every `#[actor]` type emits
        // (ADR-0096) — the registry stores the child as `dyn
        // ErasedWasmActor`, so the bound is the mechanical realisation of
        // "reuse the existing erasure" (no new child-dispatch trait).
        A: Instanced + WasmActor + ErasedWasmActor,
        // iamacoffeepot/aether#2311: `A::init` returns the runtime state, boxed
        // as the erased child (`State = Self` for an un-split component).
        <A as WasmActor>::State: ErasedWasmActor,
    {
        let (is_counter, full_subname) = resolve_subname(subname)?;
        let alias = MailboxId(mail::spawn_inline_child(is_counter, &full_subname));
        // Re-decode an owned `A::Config` for the in-guest `init` from the
        // same bytes the detached path would have shipped — symmetric with
        // `spawn_child`'s encode-in-guest / decode-in-host round-trip, and
        // it sidesteps a `Clone` bound the detached verb also lacks.
        let bytes = config.encode_into_bytes();
        let Some(owned) = <A::Config as Kind>::decode_from_bytes(&bytes) else {
            return Err(SpawnError::InitFailed(ActorInitError::new("spawn_inline_child: Config round-trip failed")));
        };
        // The actor-type tag the rehydrate reconstruct matches against the
        // module's exported types (ADR-0114 §5) — the same `hash(NAMESPACE)`
        // tag `init_typed_p32` selects on. This is the id definition for the
        // child type, so the disallowed-method allow mirrors `spawn_child`.
        #[allow(clippy::disallowed_methods)]
        let type_tag = mailbox_id_from_name(<A as Addressable>::NAMESPACE).0;
        // The spawner's real folded id is recorded as the child's logical
        // parent so relative addressing (`ctx.parent()` / `ctx.sibling()`)
        // resolves over the registry. The alias fold itself stays flat on
        // the instance carry (the substrate's current `spawn_inline_child`),
        // so subnames are cluster-unique; per-parent subname scoping (the
        // nested-alias fold) is a follow-up needing a substrate change.
        install_inline_child::<A>(self.inline, alias, type_tag, full_subname, is_counter, self.mailbox, bytes, owned)
    }

    /// ADR-0114 / issue 2692: spawn an **inline child** whose type is
    /// selected at runtime by an [`ActorTypeTag`] resolved against the
    /// module's `export!`ed actor set, rather than named at compile time
    /// like [`Self::spawn_inline_child`]. The tag-dispatched sibling of the
    /// typed verb: same subname resolution, same first-class alias, same
    /// in-guest `init` and registry insert — the one difference is that the
    /// type is looked up by tag (through the same export-set table the
    /// reconstruct arm walks, ADR-0114 §5) instead of monomorphized. So a
    /// spawner can hold specs carrying tags and stay non-generic over its
    /// children, which is what lets the behavior host and the panel drop
    /// their per-child-type generic / hand-written dispatch.
    ///
    /// `config_bytes` are the selected type's `Config` encoded to its wire
    /// shape (empty for a `Config = ()` type); the resolver decodes them for
    /// the child's `init`, the runtime-data mirror of the typed verb's
    /// in-guest `encode` / `decode` round-trip.
    ///
    /// A [`Subname::Named`] that fails validation returns
    /// [`SpawnError::SubnameInvalid`] before any type lookup; a tag matching
    /// no exported type returns [`SpawnError::UnknownActorTag`] with **no**
    /// host alias allocated (the export-set fall-through precedes
    /// allocation); a synchronous `init` `Err` or a `Config` decode miss
    /// returns [`SpawnError::InitFailed`].
    pub fn spawn_inline_child_by_tag(
        &self,
        tag: ActorTypeTag,
        subname: Subname<'_>,
        config_bytes: &[u8],
    ) -> Result<MailboxId, SpawnError> {
        let (is_counter, full_subname) = resolve_subname(subname)?;
        // The resolver is installed on the module's registry by every
        // `export!` init shim — it enumerates the exported type set the
        // lookup needs, which is knowable only inside the macro expansion,
        // so it cannot be a stored SDK-side generic. A registry with no
        // resolver is a raw host-unit registry never wired by `export!`
        // (the seam the host unit tests drive with a synthetic resolver); a
        // real module always installs one at init, before any handler or
        // `wire` runs.
        let Some(resolver) = self.inline.spawn_resolver() else {
            return Err(SpawnError::UnknownActorTag(tag));
        };
        resolver(self.inline, self.mailbox, tag, is_counter, &full_subname, config_bytes)
    }

    /// ADR-0114: tear down an **inline child** spawned by
    /// [`Self::spawn_inline_child`]. Drops the child from this ctx's
    /// per-component [`Registry`] (running the child's `Drop`), so it
    /// stops handling mail. `child` is the alias [`MailboxId`] that
    /// `spawn_inline_child` returned (the registry key, the natural
    /// handle). Returns `true` if a resident child was removed, `false` if
    /// the alias named no inline child — idempotent, so despawning an
    /// absent or already-gone alias is a clean `false`, not an error.
    ///
    /// **The substrate alias route is kept** — teardown is guest-only, with
    /// no substrate change and no alias deregistration. The alias stays a
    /// route to this component's slot, so any in-flight or later mail to the
    /// torn-down alias — fresh mail or an orphaned downstream reply — lands
    /// in this inbox, the `export!` membrane finds no resident child and
    /// falls through to the parent's standard dispatch tail, and the chain
    /// settles (ADR-0080 / ADR-0094) rather than leaking. Discarding the
    /// alias would short-circuit-drop that orphan mail; routing it to the
    /// parent is the deliberate teardown discipline.
    ///
    /// Callable from any depth: a parent on a child, a sibling on a
    /// sibling, or a child on itself. A self-despawn mid-dispatch drops
    /// correctly — the child is taken out of its slot while it runs, so
    /// `remove` clears the empty slot and the matching `reinsert` on the
    /// inline registry finds nothing and no-ops, dropping the live box at
    /// end of dispatch.
    ///
    /// The teardown mirror of the spawn-time `wire` (issue 2746): a resident
    /// child runs its `unwire` before it is dropped. A self-despawn
    /// mid-dispatch has already taken the box onto the stack, so `take`
    /// finds an empty slot and no `unwire` runs here — the box drops at end
    /// of dispatch via the `reinsert` no-op, and a child unwiring itself
    /// synchronously mid-handler would be the wrong semantic anyway.
    /// Whole-component teardown does not yet cascade `unwire` to resident
    /// inline children (the entry `unwire` FFI runs only the top-level
    /// instance); that is separate future work.
    // Despawn is a command; its `bool` ("was a resident child removed")
    // is informational and may be ignored, the same contract as
    // `BTreeMap::remove` / `HashSet::remove` (neither is `#[must_use]`).
    // The pedantic candidate lint only fires now that the body reads a
    // borrowed registry rather than mutating a crate-global static.
    #[allow(clippy::must_use_candidate)]
    pub fn despawn_inline_child(&self, child: MailboxId) -> bool {
        // Take the resident box onto the stack, run its `unwire` through a
        // ctx addressed to its alias, then drop it; `remove` clears the
        // now-empty slot. A self-despawn (box already taken by dispatch)
        // takes `None`, so `unwire` is skipped and the slot removal stays a
        // clean no-op-then-`false`/`true` per the existing contract.
        if let Some(mut taken) = self.inline.take(child) {
            let mut unwire_ctx: WasmCtx<'_, Manual> = WasmCtx::__new(child.0, self.inline, NO_INBOUND_SOURCE);
            taken.erased_unwire(&mut unwire_ctx);
        }
        self.inline.remove(child)
    }
}

impl<'a, M: ReplyMode> WasmCtx<'a, M> {
    /// ADR-0114 addressing amendment: a sendable handle to this actor's
    /// **parent** in the cluster, or `None` if this actor is the cluster
    /// root (the instance itself — its parent is cross-cluster, addressed
    /// through a chassis cap or the runtime-name escape hatch, not here).
    ///
    /// Resolves by registry lookup over the per-component inline registry,
    /// never by folding (a [`MailboxId`] is a one-way hash chain, so the
    /// guest cannot reproduce the parent id; it looks the recorded parent
    /// up). A send through the returned handle routes in place through the
    /// cluster membrane.
    #[must_use]
    pub fn parent(&self) -> Option<RelativeMailbox<'a>> {
        let id = self.inline.parent_of(MailboxId(self.mailbox))?;
        Some(RelativeMailbox { id, sender: self.mailbox, inline: self.inline })
    }

    /// ADR-0114 addressing amendment: a sendable handle to this actor's
    /// inline **child** whose subname is `name`, or `None` if no such child
    /// is resident in the cluster. Pure registry lookup, never a fold.
    #[must_use]
    pub fn child(&self, name: &str) -> Option<RelativeMailbox<'a>> {
        let id = self.inline.child_of(MailboxId(self.mailbox), name)?;
        Some(RelativeMailbox { id, sender: self.mailbox, inline: self.inline })
    }

    /// ADR-0114 addressing amendment: a sendable handle to this actor's
    /// **sibling** whose subname is `name` — the child of this actor's
    /// parent named `name` — or `None` if this actor has no recorded parent
    /// or no such sibling resides. Pure registry lookup, never a fold.
    #[must_use]
    pub fn sibling(&self, name: &str) -> Option<RelativeMailbox<'a>> {
        let id = self.inline.sibling_of(MailboxId(self.mailbox), name)?;
        Some(RelativeMailbox { id, sender: self.mailbox, inline: self.inline })
    }

    /// Issue 1987: send `payload` through a stored [`Mailbox<K>`] addressing
    /// token, threading this actor's own id as the send's `from` so the
    /// recipient's `ctx.source_mailbox()` resolves the sender and the host
    /// stamps the correct origin. A `Mailbox<K>` is a pure address (it
    /// carries no origin), so the ctx supplies the "from" half — the
    /// by-token counterpart of `ctx.actor::<R>().send(&k)`. Routes through
    /// the inline registry like every ctx send: a cluster-member recipient
    /// dispatches in place, any other hands off to the host. Inherits the
    /// handler's in-flight causal chain (ADR-0080 §7).
    pub fn send<K: Kind>(&mut self, mailbox: Mailbox<K>, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(mailbox.mailbox(), K::ID.0, &bytes, 1, ChainMode::Inherit, self.mailbox);
    }

    /// Send through a stored mailbox token and store a typed context for the
    /// reply correlation id.
    #[must_use]
    pub fn send_with_context<K: Kind, C: Kind>(&mut self, mailbox: Mailbox<K>, payload: &K, context: &C) -> RequestId {
        match self.inline.route_decision(mailbox.mailbox()) {
            RouteDecision::Local => {
                tracing::warn!(
                    kind = K::NAME,
                    recipient = mailbox.mailbox(),
                    "send_with_context on an inline-cluster local route has no host correlation",
                );
                self.send(mailbox, payload);
                RequestId(Source::NO_CORRELATION)
            }
            RouteDecision::Remote => {
                self.send(mailbox, payload);
                let request = RequestId(mail::prev_correlation());
                // SAFETY: the macro-emitted registry is accessed only under the
                // serialized wasm guest entrypoint.
                unsafe {
                    self.inline.request_contexts_mut().insert(request, context);
                }
                request
            }
        }
    }

    /// Issue 1987: send `payload` to a raw [`MailboxId`], threading this
    /// actor's own id as the send's `from`. The by-id escape hatch for a
    /// recipient address known only at runtime (the typed-token counterpart
    /// is [`Self::send`]; the by-name counterpart is
    /// [`MailSender::send_to_named`]). Routes through the
    /// inline registry and inherits the handler's causal chain like every
    /// ctx send.
    pub fn send_to<K: Kind>(&mut self, id: MailboxId, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(id.0, K::ID.0, &bytes, 1, ChainMode::Inherit, self.mailbox);
    }

    /// Send to a raw mailbox id and store a typed context for the reply
    /// correlation id.
    #[must_use]
    pub fn send_to_with_context<K: Kind, C: Kind>(&mut self, id: MailboxId, payload: &K, context: &C) -> RequestId {
        match self.inline.route_decision(id.0) {
            RouteDecision::Local => {
                tracing::warn!(
                    kind = K::NAME,
                    recipient = id.0,
                    "send_to_with_context on an inline-cluster local route has no host correlation",
                );
                self.send_to(id, payload);
                RequestId(Source::NO_CORRELATION)
            }
            RouteDecision::Remote => {
                self.send_to(id, payload);
                let request = RequestId(mail::prev_correlation());
                // SAFETY: the macro-emitted registry is accessed only under the
                // serialized wasm guest entrypoint.
                unsafe {
                    self.inline.request_contexts_mut().insert(request, context);
                }
                request
            }
        }
    }
}

/// Resolve a [`Subname`] into the `(is_counter, discriminator)` pair the
/// spawn host fns take, shared by [`WasmCtx::spawn_child`] and
/// [`WasmCtx::spawn_inline_child`]. `Counter` passes an empty discriminator
/// the host ignores (it assigns a bare monotonic counter and produces just
/// `n.to_string()`); `Named` validates the caller-supplied segment (no `:`,
/// no control/whitespace, not empty) then passes it bare as the flat
/// discriminator — convention: no `.` in a discriminator.
fn resolve_subname(subname: Subname<'_>) -> Result<(bool, String), SpawnError> {
    match subname {
        Subname::Counter => Ok((true, String::new())),
        Subname::Named(name) => {
            validate_namespace_segment(name).map_err(SpawnError::SubnameInvalid)?;
            Ok((false, String::from(name)))
        }
    }
}

/// Build an inline child's actor value and register it under its alias in
/// `registry` (ADR-0114). Split out of [`WasmCtx::spawn_inline_child`] so
/// the in-guest `init` + registry insert is exercisable on the host build
/// (where the `spawn_inline_child` host fn is a panicking stub): the unit
/// test calls this with a local registry, a synthetic alias, and an owned
/// config.
///
/// ADR-0114 §5: `type_tag` / `full_subname` / `is_counter` are recorded in
/// the slot so a `replace_component` swap can reconstruct the child by
/// type and re-fold its metadata. `config_bytes` (issue 2690) is the
/// child's encoded `Config` — the same bytes `config` was decoded from —
/// retained in the slot so a subsequent dehydrate/reconstruct cycle can
/// re-init the child from its real config instead of empty bytes.
///
/// `pub(crate)` so the by-tag spawn core
/// ([`crate::wasm::inline::compose::spawn_one_child`], issue 2692) shares
/// this exact `init` + insert step with the typed verb rather than
/// copying it.
///
/// After the insert, the fresh child's `wire` runs (issue 2746): the child
/// is taken back out of its slot onto the stack, `erased_wire` is driven
/// through a [`WasmCtx`] addressed to its alias, and it is reinserted — the
/// same take/reinsert discipline `membrane_dispatch` uses, so a `wire` that
/// spawns a nested inline child re-enters the registry without aliasing its
/// interior-mutable map. Only the two fresh-spawn paths funnel here; the
/// `replace_component` reconstruct path (`reconstruct_one_child`) has its
/// own insert and runs `init` + `on_rehydrate`, not `wire`, so a reload
/// never fires `wire`.
// The parameters are the slot's reconstruct record (ADR-0114 §5) plus the
// decoded config — a fixed set with no meaningful grouping short of a
// one-use struct.
#[allow(clippy::too_many_arguments)]
pub(crate) fn install_inline_child<A>(
    registry: &Registry,
    alias: MailboxId,
    type_tag: u64,
    full_subname: String,
    is_counter: bool,
    parent: u64,
    config_bytes: Vec<u8>,
    config: A::Config,
) -> Result<MailboxId, SpawnError>
where
    A: WasmActor + ErasedWasmActor,
    // iamacoffeepot/aether#2311: `A::init` returns the runtime state, boxed as
    // the erased child. For an un-split component `State = Self`.
    <A as WasmActor>::State: ErasedWasmActor,
{
    let mut ctx = WasmInitCtx::__new(alias.0);
    // ADR-0156 §2: inline children resolve `Params` to the compiled default
    // (empty params for now), mirroring the `()`-config round-trip.
    let params = <A::Params as Default>::default();
    let child = A::init(config, params, &mut ctx).map_err(SpawnError::InitFailed)?;
    registry.insert_child(alias, type_tag, full_subname, is_counter, parent, config_bytes, Box::new(child));
    // Run the fresh child's `wire` (issue 2746). Take it back onto the stack
    // so its slot is empty for the duration — a `wire` that spawns a nested
    // inline child then re-enters the registry (a different slot) with no
    // aliasing, and a `wire` that re-addresses its own alias finds the empty
    // slot, exactly as `membrane_dispatch` handles a resident child. `take`
    // yields `Some` here (the box was just inserted); the `if let` is a
    // defensive no-op rather than an `expect`.
    if let Some(mut fresh) = registry.take(alias) {
        let mut wire_ctx: WasmCtx<'_, Manual> = WasmCtx::__new(alias.0, registry, NO_INBOUND_SOURCE);
        fresh.erased_wire(&mut wire_ctx);
        registry.reinsert(alias, fresh);
    }
    Ok(alias)
}

// ADR-0114 addressing amendment: every `WasmCtx` send resolves the recipient
// id then routes through the inline registry's `route_or_enqueue`, so a send
// to a cluster member (own id or a resident inline-child alias) dispatches in
// place through the membrane (queue + drain) and only a cross-cluster
// recipient hits the host. For a childless component with no captured
// `self_id` match the recipient is always `Remote`, so the path is identical
// to a bare `mail::send_mail`.
impl<M: ReplyMode> MailSender for WasmCtx<'_, M> {
    //noinspection DuplicatedCode
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Singleton + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(
            R::resolve(self.mailbox, ()).0,
            K::ID.0,
            &bytes,
            1,
            ChainMode::Inherit,
            self.mailbox,
        );
    }

    //noinspection DuplicatedCode
    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Singleton + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        self.inline.route_or_enqueue(
            R::resolve(self.mailbox, ()).0,
            K::ID.0,
            bytes,
            payloads.len() as u32,
            ChainMode::Inherit,
            self.mailbox,
        );
    }

    //noinspection DuplicatedCode
    // Runtime-name send escape hatch (the `MailSender::send_to_named` contract):
    // the recipient name is supplied at runtime, no compile-time `R` to resolve.
    #[allow(clippy::disallowed_methods)]
    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(
            mailbox_id_from_name(name).0,
            K::ID.0,
            &bytes,
            1,
            ChainMode::Inherit,
            self.mailbox,
        );
    }

    fn prev_correlation(&self) -> u64 {
        mail::prev_correlation()
    }

    //noinspection DuplicatedCode
    fn send_detached<R, K>(&mut self, payload: &K)
    where
        R: Singleton + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(
            R::resolve(self.mailbox, ()).0,
            K::ID.0,
            &bytes,
            1,
            ChainMode::Detached,
            self.mailbox,
        );
    }

    //noinspection DuplicatedCode
    // Runtime-name detached escape hatch — the `send_to_named` counterpart.
    #[allow(clippy::disallowed_methods)]
    fn send_detached_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(
            mailbox_id_from_name(name).0,
            K::ID.0,
            &bytes,
            1,
            ChainMode::Detached,
            self.mailbox,
        );
    }

    //noinspection DuplicatedCode
    // By-id detached send: the inherent `send_to` with `ChainMode::Detached`.
    fn send_detached_to<K: Kind>(&mut self, id: MailboxId, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(id.0, K::ID.0, &bytes, 1, ChainMode::Detached, self.mailbox);
    }
}

// ADR-0112: the reply surface is per-mode. `Manual` carries it (a
// manual-class handler issues its own replies); `Single` deliberately
// does not, so a `-> ()` single handler is provably silent and a stray
// single-ctx `ctx.reply` is a compile error rather than a manifest lie.
impl OutboundReply for WasmCtx<'_, Manual> {
    type ReplyHandle = ReplyHandle;

    fn reply_target(&self) -> Option<ReplyHandle> {
        self.sender
    }

    fn source_mailbox(&self) -> Option<MailboxId> {
        // Issue 2687: delegate to the inherent generic accessor (the single
        // source of truth), which the `Single` `#[fallback]` ctx also reads.
        // The fully-qualified path resolves the inherent method, not this
        // trait method, so there is no recursion.
        WasmCtx::source_mailbox(self)
    }

    fn reply<K: Kind>(&mut self, payload: &K) {
        if let Some(handle) = self.sender {
            let bytes = payload.encode_into_bytes();
            mail::reply_mail(handle.raw(), K::ID.0, &bytes, 1, self.mailbox);
        }
    }

    fn reply_to<K: Kind>(&mut self, sender: ReplyHandle, payload: &K) {
        let bytes = payload.encode_into_bytes();
        mail::reply_mail(sender.raw(), K::ID.0, &bytes, 1, self.mailbox);
    }
}

// ADR-0134: the emit surface is the multi class's, implemented only for
// the `Multi<K>` mode. Each `emit` is a detached chain root addressed at
// the dispatch source (`self.source`, the `send_detached_to` body with the
// source as recipient), so an emission starts a fresh chain rather than
// holding the request chain open. A sourceless dispatch (session /
// broadcast / substrate-origin mail, `MailboxId::NONE`) has no routable
// target, so the emission warn-drops.
impl<K: Kind> Emit<K> for WasmCtx<'_, Multi<K>> {
    fn emit(&mut self, payload: &K) {
        if self.source == MailboxId::NONE.0 {
            tracing::warn!(
                kind = <K as Kind>::NAME,
                "multi handler emit dropped: the dispatch carries no routable \
                 source (session / broadcast / substrate-origin mail)",
            );
            return;
        }
        let bytes = payload.encode_into_bytes();
        self.inline.route_or_enqueue(self.source, K::ID.0, &bytes, 1, ChainMode::Detached, self.mailbox);
    }
}

/// A `save_state` deposit captured in memory instead of forwarded to the
/// host `save_state` import (ADR-0114 §5). The dehydrate compose hands the
/// parent and each inline child a [`WasmDropCtx`] bound to one of these so
/// it can collect every saved blob and pack them into a single composite,
/// then call the real host `save_state` once.
#[derive(Default)]
pub(crate) struct CapturedState {
    /// The most recent `(version, bytes)` the hook saved. `None` until the
    /// hook calls `save_state`; the last call wins (mirroring the host's
    /// single-`Option<StateBundle>` overwrite contract).
    saved: Option<(u32, Vec<u8>)>,
}

impl CapturedState {
    /// Take the captured `(version, bytes)`, leaving the slot empty.
    #[must_use]
    pub fn take(&mut self) -> Option<(u32, Vec<u8>)> {
        self.saved.take()
    }
}

/// Narrowed capability handle for the `on_dehydrate` save hook.
/// Outbound mail still works through [`MailSender`]; the reply / resolve
/// surfaces are intentionally absent.
// The `Wasm` prefix carries the native/wasm split signal; bare `DropCtx` loses that.
#[allow(clippy::module_name_repetitions)]
pub struct WasmDropCtx<'a> {
    /// The actor's own mailbox id (its lineage carry), so a buffered
    /// `send` resolves the receiver through `R::resolve(self.mailbox)`
    /// like every other ctx (ADR-0099 §5).
    mailbox: u64,
    /// ADR-0114 §5: when `Some`, `save_state` records into this buffer
    /// instead of the host import, so the dehydrate compose can collect
    /// the parent's and each child's bundle and pack one composite. `None`
    /// is the ordinary path — `save_state` forwards to the host.
    capture: Option<&'a mut CapturedState>,
    _borrow: PhantomData<&'a ()>,
}

impl<'a> WasmDropCtx<'a> {
    /// Not part of the public API; called only by [`crate::export!`].
    /// Forwards `save_state` to the host import.
    #[doc(hidden)]
    #[must_use]
    pub fn __new(mailbox: u64) -> Self {
        Self { mailbox, capture: None, _borrow: PhantomData }
    }

    /// Not part of the public API; called only by the dehydrate compose
    /// (`crate::wasm::inline::compose`). `save_state` records into `capture`
    /// rather than the host import, so the composite can be assembled
    /// before a single real host `save_state`.
    #[doc(hidden)]
    #[must_use]
    pub(crate) fn __new_capturing(mailbox: u64, capture: &'a mut CapturedState) -> Self {
        Self { mailbox, capture: Some(capture), _borrow: PhantomData }
    }

    /// Deposit a migration bundle. Mirrors [`Persistence::save_state`].
    /// When this ctx was built capturing (ADR-0114 §5), the deposit is
    /// recorded in the capture buffer; otherwise it forwards to the host.
    ///
    /// # Panics
    /// Panics if the host `save_state` import returns non-zero — fail-fast
    /// per ADR-0063: the persistence bridge is part of the substrate
    /// contract and a failure here means the runtime is in an
    /// unrecoverable state. (The capturing path cannot fail.)
    pub fn save_state(&mut self, version: u32, bytes: &[u8]) {
        if let Some(capture) = self.capture.as_mut() {
            capture.saved = Some((version, bytes.to_vec()));
            return;
        }
        let status = persist::save_state(version, bytes);
        assert_eq!(status, 0, "aether-actor: save_state failed (status {status})");
    }

    /// Persist a typed kind value. Mirrors
    /// [`Persistence::save_state_kind`].
    pub fn save_state_kind<K>(&mut self, version: u32, value: &K)
    where
        K: Kind + aether_data::Schema + serde::Serialize,
    {
        <Self as Persistence>::save_state_kind::<K>(self, version, value);
    }
}

impl MailSender for WasmDropCtx<'_> {
    //noinspection DuplicatedCode
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Singleton + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        mail::send_mail(R::resolve(self.mailbox, ()).0, K::ID.0, &bytes, 1, false, self.mailbox);
    }

    //noinspection DuplicatedCode
    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Singleton + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        mail::send_mail(R::resolve(self.mailbox, ()).0, K::ID.0, bytes, payloads.len() as u32, false, self.mailbox);
    }

    //noinspection DuplicatedCode
    // Runtime-name send escape hatch (the `MailSender::send_to_named` contract):
    // the recipient name is supplied at runtime, no compile-time `R` to resolve.
    #[allow(clippy::disallowed_methods)]
    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        mail::send_mail(mailbox_id_from_name(name).0, K::ID.0, &bytes, 1, false, self.mailbox);
    }

    fn prev_correlation(&self) -> u64 {
        mail::prev_correlation()
    }

    //noinspection DuplicatedCode
    fn send_detached<R, K>(&mut self, payload: &K)
    where
        R: Singleton + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        mail::send_mail(R::resolve(self.mailbox, ()).0, K::ID.0, &bytes, 1, true, self.mailbox);
    }

    //noinspection DuplicatedCode
    // Runtime-name detached escape hatch — the `send_to_named` counterpart.
    #[allow(clippy::disallowed_methods)]
    fn send_detached_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        mail::send_mail(mailbox_id_from_name(name).0, K::ID.0, &bytes, 1, true, self.mailbox);
    }

    //noinspection DuplicatedCode
    // By-id detached send — the by-name body with the caller's id.
    fn send_detached_to<K: Kind>(&mut self, id: MailboxId, payload: &K) {
        let bytes = payload.encode_into_bytes();
        mail::send_mail(id.0, K::ID.0, &bytes, 1, true, self.mailbox);
    }
}

impl Persistence for WasmDropCtx<'_> {
    fn save_state(&mut self, version: u32, bytes: &[u8]) {
        // Route through the inherent `save_state` so the ADR-0114 §5
        // capture path applies — the generated `on_dehydrate` hooks reach
        // the bundle through `Persistence::save_state_kind`, which calls
        // this trait method, so a capturing ctx must intercept here too.
        WasmDropCtx::save_state(self, version, bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ActorTypeTag, Emit, Manual, Multi, NO_INBOUND_SOURCE, Registry, Single, SpawnError, WasmCtx,
        install_inline_child,
    };
    use crate::mail::{Mail, PriorState};
    use crate::model::Subname;
    use crate::wasm::inline::RouteDecision;
    use crate::wasm::inline::compose::{InlineChildToReconstruct, reconstruct_one_child, spawn_one_child};
    use crate::wasm::{ActorInitError, ErasedWasmActor, WasmActor, WasmDropCtx, WasmInitCtx, WasmPlacementFacts};
    use crate::{Addressable, ChildOf, HandlesKind, ModuleChild, WasmActorMailbox};
    use aether_data::{Kind, MailboxId, Source};
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::cell::Cell;
    use core::mem::{align_of, size_of};

    /// Test inline child whose `init` always fails — drives the
    /// [`SpawnError::InitFailed`] path. The `ErasedWasmActor` dispatch
    /// hooks are unreachable: a failed `init` never registers or
    /// dispatches the child.
    struct FailingChild;

    impl Addressable for FailingChild {
        const NAMESPACE: &'static str = "test.inline.failing_child";
        type Resolver = crate::Many;
    }

    impl crate::Lifecycle<Self> for FailingChild {
        type Config = ();
        type Params = ();
        type InitError = ActorInitError;
        type InitCtx<'a> = WasmInitCtx<'a>;
        type Ctx<'a> = WasmCtx<'a>;

        fn init(_config: (), _params: (), _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
            Err(ActorInitError::new("inline child init deliberately fails"))
        }
    }

    impl WasmActor for FailingChild {
        type State = Self;
        type Persist = ();
    }

    impl crate::WasmDispatch<Self> for FailingChild {
        fn dispatch(_state: &mut Self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
            unreachable!("a failed-init child is never dispatched")
        }
    }

    impl ErasedWasmActor for FailingChild {
        fn erased_namespace(&self) -> &'static str {
            Self::NAMESPACE
        }
        fn erased_dispatch(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
            unreachable!("a failed-init child is never dispatched")
        }
        fn erased_wire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {
            unreachable!()
        }
        fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {
            unreachable!()
        }
        fn erased_on_dehydrate(&mut self, _ctx: &mut WasmDropCtx<'_>) {
            unreachable!()
        }
        fn erased_on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _prior: PriorState<'_>) {
            unreachable!()
        }
    }

    /// Step 3: a synchronous `init` `Err` surfaces as
    /// [`SpawnError::InitFailed`] (the inline child runs `init` in-process,
    /// unlike the detached `spawn_child` whose init failure logs async).
    /// Exercises [`install_inline_child`] directly so the host build runs
    /// it without the panicking `spawn_inline_child` host-fn stub.
    #[test]
    fn install_inline_child_reports_init_failure() {
        let registry = Registry::new();
        let result = install_inline_child::<FailingChild>(
            &registry,
            MailboxId(0x5555),
            0,
            String::from("child"),
            false,
            0,
            Vec::new(),
            (),
        );
        assert!(
            matches!(result, Err(SpawnError::InitFailed(_))),
            "a failing init must return SpawnError::InitFailed, got {result:?}",
        );
    }

    /// Step 3: subname validation parity with `spawn_child` — a
    /// separator-bearing `Named` subname is rejected up front with
    /// [`SpawnError::SubnameInvalid`], before any host round-trip (so the
    /// host build's panicking host-fn stub is never reached).
    #[test]
    fn spawn_inline_child_rejects_invalid_subname() {
        let registry = Registry::new();
        let ctx = WasmCtx::__new(0, &registry, NO_INBOUND_SOURCE);
        let result = ctx.spawn_inline_child::<FailingChild>(Subname::Named("bad:name"), &());
        assert!(
            matches!(result, Err(SpawnError::SubnameInvalid(_))),
            "a separator-bearing subname must return SubnameInvalid, got {result:?}",
        );
    }

    /// Issue 2001: `source_mailbox()` is a single read of the ctx's
    /// `source` field on the top-level path — the host threads the resolved
    /// inbound source over the `receive_p32` ABI and the `export!` membrane
    /// hands it to `__new` (the same field the in-place drain threads). A
    /// non-`NONE` source yields `Some(id)`; `NONE` (the no-peer-origin
    /// sentinel) yields `None`. No host round-trip is involved.
    #[test]
    fn source_mailbox_reads_the_threaded_source_field() {
        let registry = Registry::new();

        let source = MailboxId(0x9999_0000_1234_5678);
        let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(0x10, &registry, source.0);
        assert_eq!(ctx.source_mailbox(), Some(source), "a non-NONE threaded source must surface verbatim");

        let none_ctx: WasmCtx<'_, Manual> = WasmCtx::__new(0x10, &registry, NO_INBOUND_SOURCE);
        assert_eq!(none_ctx.source_mailbox(), None, "MailboxId::NONE means no peer-component origin");
    }

    #[test]
    fn local_dispatch_ctx_never_reads_host_reply_correlation() {
        let registry = Registry::new();
        let ctx: WasmCtx<'_, Manual> = WasmCtx::__new_local_dispatch(0x10, &registry, NO_INBOUND_SOURCE);
        assert_eq!(ctx.in_reply_to(), None, "cluster-drained dispatches carry no host correlation");
    }

    /// ADR-0134: `emit` on a `Multi<K>` ctx routes a detached mail at the
    /// threaded dispatch source, and a sourceless dispatch drops the
    /// emission. The source is set to a cluster member (the self id) so the
    /// detached route resolves in place and enqueues locally — no host call
    /// (the host stub panics on the host build, so reaching the assert
    /// without a panic proves the local branch). A `()` payload encodes to
    /// empty bytes.
    #[test]
    fn emit_routes_at_the_threaded_source_and_drops_when_sourceless() {
        let registry = Registry::new();
        let source = 0x7200_u64;
        registry.set_self_id(source);

        // A dispatch whose source is a cluster member: emit routes a
        // detached mail there and enqueues locally.
        let mut ctx: WasmCtx<'_, Manual> = WasmCtx::__new(source, &registry, source);
        Emit::<()>::emit(ctx.as_multi::<()>(), &());
        assert_eq!(registry.queued_len(), 1, "emit routes a detached mail at the threaded source");

        // A sourceless dispatch (NONE) has no routable target — the emit
        // drops rather than enqueuing.
        let mut none_ctx: WasmCtx<'_, Manual> = WasmCtx::__new(source, &registry, NO_INBOUND_SOURCE);
        Emit::<()>::emit(none_ctx.as_multi::<()>(), &());
        assert_eq!(registry.queued_len(), 1, "a sourceless emit drops — no additional mail enqueued");
    }

    /// ADR-0134: the multi mode marker is layout-neutral — a `Multi<K>`
    /// view has the same size + alignment as the `Single` / `Manual` views.
    /// This is the invariant the `as_multi` pointer reborrow rests on.
    #[test]
    fn ffi_ctx_layout_identical_for_multi_mode() {
        assert_eq!(size_of::<WasmCtx<'static, Single>>(), size_of::<WasmCtx<'static, Multi<u32>>>(),);
        assert_eq!(align_of::<WasmCtx<'static, Single>>(), align_of::<WasmCtx<'static, Multi<u32>>>(),);
    }

    /// Test inline child whose `init` succeeds, so `install_inline_child`
    /// registers it in the test-local registry for the despawn test. Its
    /// dispatch hooks are unreachable here — the test only installs then
    /// despawns.
    struct SucceedingChild;

    impl Addressable for SucceedingChild {
        const NAMESPACE: &'static str = "test.inline.succeeding_child";
        type Resolver = crate::Many;
    }

    impl crate::Lifecycle<Self> for SucceedingChild {
        type Config = ();
        type Params = ();
        type InitError = ActorInitError;
        type InitCtx<'a> = WasmInitCtx<'a>;
        type Ctx<'a> = WasmCtx<'a>;

        fn init(_config: (), _params: (), _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
            Ok(Self)
        }
    }

    impl WasmActor for SucceedingChild {
        type State = Self;
        type Persist = ();
    }

    impl ModuleChild for SucceedingChild {}

    impl SucceedingChild {
        const __AETHER_PLACEMENT: WasmPlacementFacts =
            WasmPlacementFacts { is_instanced: true, module_child: true, exact_parent_tags: &[] };
    }

    impl crate::WasmDispatch<Self> for SucceedingChild {
        fn dispatch(_state: &mut Self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
            unreachable!("the despawn test never dispatches this child")
        }
    }

    impl HandlesKind<()> for SucceedingChild {}

    impl ErasedWasmActor for SucceedingChild {
        fn erased_namespace(&self) -> &'static str {
            Self::NAMESPACE
        }
        fn erased_dispatch(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
            unreachable!("the despawn test never dispatches this child")
        }
        fn erased_wire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {}
        fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {}
        fn erased_on_dehydrate(&mut self, _ctx: &mut WasmDropCtx<'_>) {}
        fn erased_on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _prior: PriorState<'_>) {}
    }

    /// ADR-0112: the mode marker is layout-neutral — the `Single` and
    /// `Manual` views have identical size + alignment. This is the
    /// invariant the `as_single` pointer reborrow rests on.
    #[test]
    fn ffi_ctx_layout_identical_across_modes() {
        assert_eq!(size_of::<WasmCtx<'static, Single>>(), size_of::<WasmCtx<'static, Manual>>(),);
        assert_eq!(align_of::<WasmCtx<'static, Single>>(), align_of::<WasmCtx<'static, Manual>>(),);
    }

    /// ADR-0114 addressing amendment: a ctx self-identified as the cluster
    /// root resolves `child(name)` to the resident inline child, returns
    /// `None` for a missing name, and a send through the resolved relative
    /// routes in place (enqueues locally — no host call, which would panic
    /// on the host build). `parent()` of the root is `None` (cross-cluster).
    #[test]
    fn ctx_relative_verbs_resolve_and_route_in_place() {
        let registry = Registry::new();
        let root = 0x7100_u64;
        registry.set_self_id(root);
        // Install a child of the root keyed by a synthetic alias, then a
        // grandchild under it. Record each parent the way `spawn_inline_child`
        // would.
        let widget = MailboxId(0x7101);
        let label = MailboxId(0x7102);
        install_inline_child::<SucceedingChild>(
            &registry,
            widget,
            0,
            String::from("widget"),
            false,
            root,
            Vec::new(),
            (),
        )
        .expect("a succeeding init installs the inline child");
        install_inline_child::<SucceedingChild>(
            &registry,
            label,
            0,
            String::from("label"),
            false,
            widget.0,
            Vec::new(),
            (),
        )
        .expect("a succeeding init installs the inline grandchild");

        let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(root, &registry, NO_INBOUND_SOURCE);

        // The root has no registry parent entry — its parent is cross-cluster.
        assert!(ctx.parent().is_none(), "the cluster root resolves no in-cluster parent");

        // child(name) resolves the resident widget; a missing name is None.
        let child = ctx.child("widget").expect("the widget resolves by subname");
        assert_eq!(child.mailbox_id(), widget, "child resolves to the alias id");
        assert!(ctx.child("missing").is_none(), "a missing subname resolves to None");
        let grandchild = child.child("label").expect("the grandchild resolves relative to the child handle");
        assert_eq!(grandchild.mailbox_id(), label, "handle-relative child walk reaches the grandchild");
        assert!(child.child("missing").is_none(), "a missing grandchild segment resolves to None");

        // The resolved relative is a cluster member, so a send routes in
        // place; the local path enqueues and makes no host call (the host
        // stub panics on the host build, so reaching this line without a
        // panic proves the send took the local branch). A `()` payload
        // encodes to empty bytes.
        assert_eq!(
            registry.route_decision(child.mailbox_id().0),
            RouteDecision::Local,
            "the resolved relative is classified as an in-cluster recipient",
        );
        child.send(&());
        assert_eq!(registry.queued_len(), 1, "a send to a resolved relative enqueues locally — no scheduler hop");
    }

    #[test]
    fn send_tracked_local_route_returns_no_correlation_sentinel() {
        let registry = Registry::new();
        let root = 0x7100_u64;
        registry.set_self_id(root);
        let child = MailboxId(0x7101);
        install_inline_child::<SucceedingChild>(
            &registry,
            child,
            0,
            String::from("widget"),
            false,
            root,
            Vec::new(),
            (),
        )
        .expect("install inline child");

        let mailbox = WasmActorMailbox::<SucceedingChild>::__new(child.0, root, &registry);
        let request = mailbox.send_tracked(&());
        assert_eq!(request.0, Source::NO_CORRELATION, "local inline sends have no host-minted request id");
    }

    // Issue 2692: the by-tag spawn host-unit fixtures. `thread_local` (not
    // a `static`) keeps parallel-threaded test runs from racing on the
    // observed-config cell.
    extern crate std;

    std::thread_local! {
        /// The `value` field [`StubChild::init`] last decoded from its
        /// config bytes, so the by-tag spawn test can assert the passed
        /// bytes were threaded through decode → init.
        static STUB_INIT_CONFIG: Cell<Option<u32>> = const { Cell::new(None) };
    }

    /// Config for [`StubChild`] carrying an observable `value`, so a by-tag
    /// spawn test proves `config_bytes` were decoded and handed to `init`
    /// (rather than dropped or replaced with an empty default).
    #[derive(::aether_data::Kind, ::aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Default)]
    #[kind(name = "test.inline.stub_config")]
    struct StubConfig {
        value: u32,
    }

    /// Inline child whose `init` records its decoded config `value` into the
    /// thread-local, so the by-tag host-unit test reads back what was
    /// threaded. Its dispatch / lifecycle hooks are unreachable — the tests
    /// only spawn it, never mail it.
    struct StubChild;

    impl Addressable for StubChild {
        const NAMESPACE: &'static str = "test.inline.stub_child";
        type Resolver = crate::Many;
    }

    impl crate::Lifecycle<Self> for StubChild {
        type Config = StubConfig;
        type Params = ();
        type InitError = ActorInitError;
        type InitCtx<'a> = WasmInitCtx<'a>;
        type Ctx<'a> = WasmCtx<'a>;

        fn init(config: StubConfig, _params: (), _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
            STUB_INIT_CONFIG.set(Some(config.value));
            Ok(Self)
        }
    }

    impl WasmActor for StubChild {
        type State = Self;
        type Persist = ();
    }

    impl StubChild {
        const __AETHER_PLACEMENT: WasmPlacementFacts = WasmPlacementFacts {
            is_instanced: true,
            module_child: false,
            exact_parent_tags: &[ActorTypeTag::of::<NestingParent>()],
        };
    }

    impl crate::WasmDispatch<Self> for StubChild {
        fn dispatch(_state: &mut Self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
            unreachable!("the by-tag spawn tests never dispatch the stub child")
        }
    }

    impl ErasedWasmActor for StubChild {
        fn erased_namespace(&self) -> &'static str {
            Self::NAMESPACE
        }
        fn erased_dispatch(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
            unreachable!("the by-tag spawn tests never dispatch the stub child")
        }
        fn erased_wire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {}
        fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {}
        fn erased_on_dehydrate(&mut self, _ctx: &mut WasmDropCtx<'_>) {}
        fn erased_on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _prior: PriorState<'_>) {}
    }

    /// Synthetic stand-in for the `export!`-generated resolver: matches the
    /// [`StubChild`] tag against the (one-type) exported set and, on a
    /// match, fabricates the alias the real macro resolver would have
    /// allocated via the host `spawn_inline_child` host fn (which panics on
    /// the host build) before running the shared `spawn_one_child` core. Any
    /// other tag falls through to [`SpawnError::UnknownActorTag`], exactly
    /// as the generated resolver's tag-match fall-through does.
    fn stub_resolver(
        registry: &Registry,
        parent: u64,
        tag: ActorTypeTag,
        is_counter: bool,
        full_subname: &str,
        config_bytes: &[u8],
    ) -> Result<MailboxId, SpawnError> {
        if tag == ActorTypeTag::of::<StubChild>() {
            let alias = MailboxId(0xABCD_0001);
            spawn_one_child::<StubChild>(
                registry,
                parent,
                alias,
                tag.0,
                String::from(full_subname),
                is_counter,
                config_bytes,
            )
        } else {
            Err(SpawnError::UnknownActorTag(tag))
        }
    }

    /// A resolver that panics if reached — the subname-validation-first
    /// tests install it to prove the guard runs before any resolver call.
    fn panicking_resolver(
        _registry: &Registry,
        _parent: u64,
        _tag: ActorTypeTag,
        _is_counter: bool,
        _full_subname: &str,
        _config_bytes: &[u8],
    ) -> Result<MailboxId, SpawnError> {
        panic!("the resolver must not run when subname validation fails")
    }

    /// Step 5(a): a known tag resolves to its exported type, and the passed
    /// `config_bytes` are decoded and threaded into that type's `init`. Owned
    /// logic: the tag → type selection and the config-decode-into-init path,
    /// neither a derive nor another crate's machinery.
    #[test]
    fn spawn_inline_child_by_tag_spawns_matched_type_and_threads_config() {
        let registry = Registry::new();
        registry.set_spawn_resolver(stub_resolver);
        STUB_INIT_CONFIG.set(None);

        let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(0x10, &registry, NO_INBOUND_SOURCE);
        let config_bytes = StubConfig { value: 0x1234_5678 }.encode_into_bytes();
        let alias = ctx
            .spawn_inline_child_by_tag(ActorTypeTag::of::<StubChild>(), Subname::Named("tagged"), &config_bytes)
            .expect("a known tag spawns its exported type");

        assert!(registry.take(alias).is_some(), "the tagged child is resident under the resolver's alias");
        assert_eq!(
            STUB_INIT_CONFIG.get(),
            Some(0x1234_5678),
            "the config bytes were decoded and threaded into the child's init",
        );
    }

    /// Issue 2789: a by-tag inline spawn records the **spawner** as the
    /// child's parent, not the cluster root — so a nested by-tag spawn (an
    /// inline child spawning its own child, e.g. the behavior host wrapping
    /// a widget) is reachable through the spawner's `ctx.child` /
    /// `ctx.parent`. The spawner's own id (`0x5AFE`) is set distinct from
    /// the cluster root (`0x1111`) so the assertion fails against the old
    /// `registry.self_id()` behavior. Owned logic: the by-tag spawn's
    /// parent recording, mirroring the typed `spawn_inline_child` path.
    #[test]
    fn spawn_inline_child_by_tag_parents_to_the_spawner_not_the_root() {
        let registry = Registry::new();
        registry.set_self_id(0x1111);
        registry.set_spawn_resolver(stub_resolver);
        STUB_INIT_CONFIG.set(None);

        let spawner = 0x5AFE_u64;
        let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(spawner, &registry, NO_INBOUND_SOURCE);
        let alias = ctx
            .spawn_inline_child_by_tag(
                ActorTypeTag::of::<StubChild>(),
                Subname::Named("nested"),
                &StubConfig { value: 1 }.encode_into_bytes(),
            )
            .expect("a known tag spawns its exported type");

        assert_eq!(
            registry.parent_of(alias),
            Some(MailboxId(spawner)),
            "the by-tag child's recorded parent is the spawner, not the cluster root",
        );
    }

    /// Step 5(b): a tag matching no exported type returns
    /// [`SpawnError::UnknownActorTag`] and inserts no child — the untrusted
    /// runtime-tag path the spawner recovers from.
    #[test]
    fn spawn_inline_child_by_tag_unknown_tag_errors_and_inserts_nothing() {
        let registry = Registry::new();
        registry.set_spawn_resolver(stub_resolver);

        let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(0x10, &registry, NO_INBOUND_SOURCE);
        let unknown = ActorTypeTag(0xFFFF_FFFF_FFFF_FFFF);
        let result = ctx.spawn_inline_child_by_tag(unknown, Subname::Named("tagged"), &[]);
        assert!(
            matches!(result, Err(SpawnError::UnknownActorTag(t)) if t == unknown),
            "an unresolvable tag returns UnknownActorTag(tag), got {result:?}",
        );
        assert!(registry.child_metas().is_empty(), "an unknown tag inserts no child");
    }

    /// Step 5(c): subname validation runs before the resolver — a
    /// separator-bearing `Named` is rejected with
    /// [`SpawnError::SubnameInvalid`] and the (panicking) resolver never
    /// runs.
    #[test]
    fn spawn_inline_child_by_tag_rejects_bad_subname_before_resolver() {
        let registry = Registry::new();
        registry.set_spawn_resolver(panicking_resolver);

        let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(0x10, &registry, NO_INBOUND_SOURCE);
        let result = ctx.spawn_inline_child_by_tag(ActorTypeTag::of::<StubChild>(), Subname::Named("bad:name"), &[]);
        assert!(
            matches!(result, Err(SpawnError::SubnameInvalid(_))),
            "a separator-bearing subname is rejected before the resolver runs, got {result:?}",
        );
    }

    std::thread_local! {
        /// How many times a [`LifecycleProbe`] has run its `wire`, so the
        /// spawn-runs-`wire` and reconstruct-does-not-`wire` tripwires can
        /// observe the lifecycle call (issue 2746).
        static PROBE_WIRE_COUNT: Cell<u32> = const { Cell::new(0) };
        /// How many times a [`LifecycleProbe`] has run its `unwire`, so the
        /// despawn-runs-`unwire` tripwire can observe the teardown call.
        static PROBE_UNWIRE_COUNT: Cell<u32> = const { Cell::new(0) };
    }

    /// Inline child whose `wire` / `unwire` bump thread-local counters, so
    /// the composition path's new lifecycle calls (issue 2746) are
    /// observable. Its dispatch hook is unreachable — the tests only spawn /
    /// despawn / reconstruct it.
    struct LifecycleProbe;

    impl Addressable for LifecycleProbe {
        const NAMESPACE: &'static str = "test.inline.lifecycle_probe";
        type Resolver = crate::Many;
    }

    impl crate::Lifecycle<Self> for LifecycleProbe {
        type Config = ();
        type Params = ();
        type InitError = ActorInitError;
        type InitCtx<'a> = WasmInitCtx<'a>;
        type Ctx<'a> = WasmCtx<'a>;

        fn init(_config: (), _params: (), _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
            Ok(Self)
        }
    }

    impl WasmActor for LifecycleProbe {
        type State = Self;
        type Persist = ();
    }

    impl crate::WasmDispatch<Self> for LifecycleProbe {
        fn dispatch(_state: &mut Self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
            unreachable!("the lifecycle-probe tests never dispatch this child")
        }
    }

    impl ErasedWasmActor for LifecycleProbe {
        fn erased_namespace(&self) -> &'static str {
            Self::NAMESPACE
        }
        fn erased_dispatch(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
            unreachable!("the lifecycle-probe tests never dispatch this child")
        }
        fn erased_wire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {
            PROBE_WIRE_COUNT.set(PROBE_WIRE_COUNT.get() + 1);
        }
        fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {
            PROBE_UNWIRE_COUNT.set(PROBE_UNWIRE_COUNT.get() + 1);
        }
        fn erased_on_dehydrate(&mut self, _ctx: &mut WasmDropCtx<'_>) {}
        fn erased_on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _prior: PriorState<'_>) {}
    }

    /// Inline child whose `wire` spawns a nested inline child by tag — the
    /// reentrant shape the take/reinsert composition must support (a `wire`
    /// that re-enters the registry to install a grandchild). `BehaviorHost`'s
    /// `wire` spawns its wrapped widget exactly this way in the live engine
    /// (issue 2746). The nested child is a [`StubChild`], resolved through
    /// [`stub_resolver`], so its `init` records the threaded config.
    struct NestingParent;

    impl Addressable for NestingParent {
        const NAMESPACE: &'static str = "test.inline.nesting_parent";
        type Resolver = crate::Many;
    }

    impl crate::Lifecycle<Self> for NestingParent {
        type Config = ();
        type Params = ();
        type InitError = ActorInitError;
        type InitCtx<'a> = WasmInitCtx<'a>;
        type Ctx<'a> = WasmCtx<'a>;

        fn init(_config: (), _params: (), _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
            Ok(Self)
        }
    }

    impl WasmActor for NestingParent {
        type State = Self;
        type Persist = ();
    }

    impl ChildOf<NestingParent> for FailingChild {}
    impl ChildOf<NestingParent> for StubChild {}

    #[test]
    fn placement_fixtures_cover_exact_and_composable_lineage() {
        const EXACT_PARENT: ActorTypeTag = ActorTypeTag::of::<NestingParent>();
        const MISMATCH_PARENT: ActorTypeTag = ActorTypeTag::of::<LifecycleProbe>();

        fn assert_child_of<P: Addressable, C: ChildOf<P>>() {}
        fn assert_module_child<C: ModuleChild>() {}

        assert_child_of::<NestingParent, FailingChild>();
        assert_child_of::<NestingParent, StubChild>();
        assert_module_child::<SucceedingChild>();
        assert_child_of::<NestingParent, SucceedingChild>();
        assert_child_of::<LifecycleProbe, SucceedingChild>();

        assert_ne!(EXACT_PARENT, MISMATCH_PARENT, "the rejection candidate must have a distinct parent tag");
        assert_eq!(
            StubChild::__AETHER_PLACEMENT,
            WasmPlacementFacts { is_instanced: true, module_child: false, exact_parent_tags: &[EXACT_PARENT] },
            "the exact candidate must name only its declared parent",
        );
        assert!(
            !StubChild::__AETHER_PLACEMENT.exact_parent_tags.contains(&MISMATCH_PARENT),
            "the exact candidate must reject a different parent tag",
        );
        assert_eq!(
            SucceedingChild::__AETHER_PLACEMENT,
            WasmPlacementFacts { is_instanced: true, module_child: true, exact_parent_tags: &[] },
            "the composable candidate carries module permission without exact parents",
        );
        assert!(
            SucceedingChild::__AETHER_PLACEMENT.exact_parent_tags.is_empty(),
            "a composable candidate must not also carry exact parents",
        );
    }

    impl crate::WasmDispatch<Self> for NestingParent {
        fn dispatch(_state: &mut Self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
            unreachable!("the nesting-parent test never dispatches this child")
        }
    }

    impl ErasedWasmActor for NestingParent {
        fn erased_namespace(&self) -> &'static str {
            Self::NAMESPACE
        }
        fn erased_dispatch(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
            unreachable!("the nesting-parent test never dispatches this child")
        }
        fn erased_wire(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
            let config_bytes = StubConfig { value: 0x0BAD_CAFE }.encode_into_bytes();
            ctx.spawn_inline_child_by_tag(ActorTypeTag::of::<StubChild>(), Subname::Named("nested"), &config_bytes)
                .expect("the nested by-tag spawn during wire succeeds");
        }
        fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {}
        fn erased_on_dehydrate(&mut self, _ctx: &mut WasmDropCtx<'_>) {}
        fn erased_on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _prior: PriorState<'_>) {}
    }

    /// Issue 2746: a fresh inline spawn runs the child's `wire` after `init`,
    /// and a `wire` that spawns a nested inline child works — the reentrant
    /// take/reinsert path that would be silent UB under a borrow held across
    /// the call. Owned logic: the composition path's lifecycle call and its
    /// reentrancy, not a derive or another crate's machinery.
    #[test]
    fn install_inline_child_runs_wire_and_supports_nested_spawn() {
        let registry = Registry::new();
        registry.set_self_id(0x9000);
        registry.set_spawn_resolver(stub_resolver);
        STUB_INIT_CONFIG.set(None);

        let parent = MailboxId(0x9001);
        install_inline_child::<NestingParent>(
            &registry,
            parent,
            0,
            String::from("nesting"),
            false,
            0x9000,
            Vec::new(),
            (),
        )
        .expect("the nesting parent installs");

        // The parent's `wire` ran and it was reinserted into its slot.
        assert!(registry.take(parent).is_some(), "the parent's wire ran and it was reinserted");
        // The `wire` spawned a nested inline child mid-wire (the reentrant
        // install path) — resolved to the stub resolver's fixed alias.
        assert!(
            registry.take(MailboxId(0xABCD_0001)).is_some(),
            "wire installed a nested inline child (reentrant registry access)",
        );
        assert_eq!(
            STUB_INIT_CONFIG.get(),
            Some(0x0BAD_CAFE),
            "the nested child ran init with the config threaded through wire's spawn",
        );
    }

    /// Issue 2746: `despawn_inline_child` runs a resident child's `unwire`
    /// before dropping it (and spawn ran its `wire`). Owned logic: the
    /// teardown mirror the composition path now makes.
    #[test]
    fn despawn_inline_child_runs_unwire() {
        let registry = Registry::new();
        registry.set_self_id(0x9200);
        PROBE_WIRE_COUNT.set(0);
        PROBE_UNWIRE_COUNT.set(0);

        let probe = MailboxId(0x9201);
        install_inline_child::<LifecycleProbe>(
            &registry,
            probe,
            0,
            String::from("probe"),
            false,
            0x9200,
            Vec::new(),
            (),
        )
        .expect("the probe installs");
        assert_eq!(PROBE_WIRE_COUNT.get(), 1, "a fresh inline spawn runs the child's wire exactly once");

        let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(0x9200, &registry, NO_INBOUND_SOURCE);
        let removed = ctx.despawn_inline_child(probe);
        assert!(removed, "despawning a resident child returns true");
        assert_eq!(PROBE_UNWIRE_COUNT.get(), 1, "despawn runs the child's unwire exactly once");
        assert!(registry.take(probe).is_none(), "the despawned child's slot is gone");
    }

    /// Issue 2746: a `replace_component` reconstruct runs `init` +
    /// `on_rehydrate`, never `wire` — the fresh-spawn-vs-reload distinction
    /// that keeps `wire` a genuine-first-attach signal. Guards against a
    /// future move of the `wire` call into the shared `insert_child`, which
    /// would wrongly fire it on every reload.
    #[test]
    fn reconstruct_does_not_run_wire() {
        let registry = Registry::new();
        PROBE_WIRE_COUNT.set(0);

        let alias = MailboxId(0x9301);
        let to_reconstruct = InlineChildToReconstruct {
            alias,
            type_tag: 0,
            is_counter: false,
            full_subname: "probe",
            state_version: 0,
            state_bytes: &[],
            config_bytes: &[],
        };
        let ok = reconstruct_one_child::<LifecycleProbe>(&registry, &to_reconstruct);
        assert!(ok, "a ()-config probe reconstructs from empty bytes");
        assert_eq!(PROBE_WIRE_COUNT.get(), 0, "a reconstruct runs init + on_rehydrate, never wire");
        assert!(registry.take(alias).is_some(), "the reconstructed child is resident under its alias");
    }
}
