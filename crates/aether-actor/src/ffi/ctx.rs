// Wire-encode: `usize → u32` narrowings forward `(ptr, len)` pairs
// to the wasm32 host-fn ABI (`_p32` convention, ADR-0024).
#![allow(clippy::cast_possible_truncation)]

//! Concrete FFI ctx structs — [`FfiInitCtx`] / [`FfiCtx`] / [`FfiDropCtx`].
//!
//! Replaces the pre-issue-663 parametric `Ctx<'a, T>` / `InitCtx<'a, T>` /
//! `DropCtx<'a, T>` aliases. The ctx interface is now spelled by the
//! per-stage capability traits in [`crate::actor::ctx`]; these structs
//! are concrete impls that route outbound calls through the
//! per-concern bridge ZSTs in [`crate::ffi::bridge`] ([`MAIL_BRIDGE`] /
//! [`PERSIST_BRIDGE`]).
//!
//! Issue 665 retired the `transport: &'a FfiTransport` field along
//! with the `FfiTransport` ZST and `MailTransport` trait — ctxs hold
//! per-mail state only (mailbox id at init; reply target at receive),
//! and dispatch goes through the bridge statics directly.

use core::marker::PhantomData;
use core::ptr;

use aether_data::{Kind, MailboxId, mailbox_id_from_name};

use crate::actor::ctx::mail_sender::MailSender;
use crate::actor::ctx::outbound_reply::OutboundReply;
use crate::actor::ctx::persistence::Persistence;
use crate::actor::ctx::reply_mode::{Manual, ReplyMode, Single, Stream};
use crate::actor::ctx::resolver::Resolver;
use crate::actor::{
    Actor, HandlesKind, Instanced, NamespaceError, Singleton, Subname, validate_namespace_segment,
};
use crate::ffi::bridge::{MAIL_BRIDGE, PERSIST_BRIDGE};
use crate::ffi::inline::InlineRegistry;
use crate::ffi::mailbox::FfiActorMailbox;
use crate::ffi::{BootError, ErasedFfiActor, FfiActor};
use crate::mail::ReplyHandle;
use crate::mail::mailbox::{KindId, Mailbox, resolve, resolve_mailbox};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// Init-only capability handle for FFI guests. Resolved during
/// `FfiActor::init`; not available at runtime (the type split fences
/// "when can I resolve?" against "when can I send?" at compile time).
pub struct FfiInitCtx<'a> {
    mailbox: u64,
    _borrow: PhantomData<&'a ()>,
}

impl FfiInitCtx<'_> {
    /// Not part of the public API; called only by [`crate::export!`].
    #[doc(hidden)]
    #[must_use]
    pub fn __new(mailbox: u64) -> Self {
        Self {
            mailbox,
            _borrow: PhantomData,
        }
    }

    /// The component's own mailbox id. Mirrors [`Resolver::mailbox_id`].
    #[must_use]
    pub fn mailbox_id(&self) -> u64 {
        self.mailbox
    }

    /// Resolve a kind by its `const ID`. Mirrors [`Resolver::resolve`].
    #[must_use]
    pub const fn resolve<K: Kind>(&self) -> KindId<K> {
        resolve::<K>()
    }

    /// Bind a mailbox name to kind `K`. Mirrors
    /// [`Resolver::resolve_mailbox`].
    #[must_use]
    pub const fn resolve_mailbox<K: Kind>(&self, name: &str) -> Mailbox<K> {
        resolve_mailbox::<K>(name)
    }

    /// Singleton sender shortcut. Returns a typed [`FfiActorMailbox`]
    /// addressing the unique instance of receiver actor `R`.
    #[must_use]
    pub fn actor<R: Singleton>(&self) -> FfiActorMailbox<R> {
        FfiActorMailbox::__new(R::resolve(self.mailbox).0)
    }

    /// Multi-instance sender. Resolve a typed [`FfiActorMailbox`]
    /// from a runtime instance name.
    // Runtime-name escape hatch: the instance name is only known at runtime,
    // so there is no `R::resolve` lineage carry to route through.
    #[must_use]
    #[allow(clippy::disallowed_methods)]
    pub fn resolve_actor<R: Actor>(&self, name: &str) -> FfiActorMailbox<R> {
        FfiActorMailbox::__new(mailbox_id_from_name(name).0)
    }
}

impl Resolver for FfiInitCtx<'_> {
    fn mailbox_id(&self) -> u64 {
        self.mailbox
    }

    fn resolve<K: Kind>(&self) -> KindId<K> {
        resolve::<K>()
    }

    fn resolve_mailbox<K: Kind>(&self, name: &str) -> Mailbox<K> {
        resolve_mailbox::<K>(name)
    }
}

/// Why a synchronous spawn verb failed.
///
/// For the detached [`FfiCtx::spawn_child`] (ADR-0097), only subname
/// validation can fail here — a spawn-time failure (a retired / in-use
/// subname, or the sibling's `init` returning `Err`) surfaces
/// asynchronously on the trampoline, not through this `Result`. For the
/// inline [`FfiCtx::spawn_inline_child`] (ADR-0114) the child's `init`
/// runs in-process, synchronously, so its failure is reported here as
/// [`SpawnError::InitFailed`].
#[derive(Debug, Clone)]
pub enum SpawnError {
    /// A [`Subname::Named`] discriminator failed
    /// [`validate_namespace_segment`].
    SubnameInvalid(NamespaceError),
    /// ADR-0114: an inline child's synchronous `init` returned `Err`. The
    /// wrapped [`BootError`] carries the actor's own failure message.
    /// Unlike the detached `spawn_child` — whose `init` runs later on the
    /// trampoline and logs asynchronously — an inline child's `init` runs
    /// in-guest during [`FfiCtx::spawn_inline_child`], so the boot failure
    /// comes back through this `Result`.
    InitFailed(BootError),
}

/// Per-receive (and post-init `wire` / pre-shutdown `unwire`)
/// capability handle for FFI guests. Exposes send, reply, and
/// resolution primitives. Issue 703 added [`Resolver`] + a
/// `mailbox_id` field so `wire`-stage explicit subscribes
/// (sending [`SubscribeInput`](aether_kinds::SubscribeInput) to the
/// `InputCapability`) can self-address.
pub struct FfiCtx<'a, M: ReplyMode = Single> {
    mailbox: u64,
    sender: Option<u32>,
    /// ADR-0114: the per-component inline-child registry the
    /// [`Self::spawn_inline_child`] / [`Self::despawn_inline_child`] verbs
    /// drive. The `export!` membrane threads in the component's emitted
    /// `static __AETHER_INLINE` (a `&'static` that coerces to `&'a`); a
    /// host unit test threads in a local registry. Held by reference
    /// rather than reached as a global — the same discipline the parent
    /// slot (`__AETHER_COMPONENT`) already follows.
    inline: &'a InlineRegistry,
    _borrow: PhantomData<&'a ()>,
    /// ADR-0112: phantom reply-mode marker (a ZST, layout-neutral) that
    /// selects which reply surface this ctx exposes. Defaults to
    /// [`Single`], so the common `FfiCtx<'_>` signature is unchanged.
    _mode: PhantomData<M>,
}

impl<'a> FfiCtx<'a, Manual> {
    /// Not part of the public API; called only by [`crate::export!`].
    /// The runtime builds the most-permissive [`Manual`] view; the
    /// `#[actor]` dispatcher / lifecycle shims downgrade it per handler
    /// class with [`Self::as_single`].
    #[doc(hidden)]
    #[must_use]
    pub fn __new(mailbox: u64, inline: &'a InlineRegistry) -> Self {
        Self {
            mailbox,
            sender: None,
            inline,
            _borrow: PhantomData,
            _mode: PhantomData,
        }
    }

    /// ADR-0112 downgrade-only coercion: view this [`Manual`] ctx as a
    /// [`Single`] ctx, dropping the `OutboundReply` surface. The
    /// `#[actor]` macro hands a single-class handler this view, so a
    /// handler whose marker disagrees with its class fails to unify.
    /// There is deliberately no `as_manual` — the runtime only ever
    /// downgrades.
    #[doc(hidden)]
    #[must_use]
    pub fn as_single(&mut self) -> &mut FfiCtx<'a, Single> {
        // SAFETY: `M` is `PhantomData`-only, so `FfiCtx<'a, Manual>` and
        // `FfiCtx<'a, Single>` are layout-identical (the marker field is a
        // ZST for every `M` — see `reply_mode_types_are_zsts` and
        // `ffi_ctx_layout_identical_across_modes`). The reborrow swaps the
        // marker without touching any real field and only removes
        // capability, never adds it.
        unsafe { &mut *ptr::from_mut(self).cast::<FfiCtx<'a, Single>>() }
    }

    /// ADR-0112 forward-only coercion to the reserved [`Stream`] view.
    /// `#[handler::stream]` is rejected by the macro today, so this has
    /// no in-tree caller yet; it exists so the stream class has its
    /// downgrade path the day the emit surface lands.
    #[doc(hidden)]
    #[must_use]
    pub fn as_stream(&mut self) -> &mut FfiCtx<'a, Stream> {
        // SAFETY: same as `as_single` — `M` is `PhantomData`-only, so the
        // marker swap is a layout-identity reborrow.
        unsafe { &mut *ptr::from_mut(self).cast::<FfiCtx<'a, Stream>>() }
    }
}

impl<M: ReplyMode> FfiCtx<'_, M> {
    /// Not part of the public API; called only by the `#[actor]`
    /// dispatcher. Accepts `None` or `Some(ReplyHandle)` — the dispatcher
    /// passes `mail.reply_handle()` verbatim so component-origin and
    /// broadcast mail (which have no reply target) land as `None`.
    #[doc(hidden)]
    pub fn __set_reply_to(&mut self, sender: Option<ReplyHandle>) {
        self.sender = sender.map(ReplyHandle::raw);
    }

    /// Reply with an explicit `sender` + cached `KindId<K>`.
    ///
    /// Prefer the trait surface: [`OutboundReply::reply`] replies to the
    /// dispatcher-stamped sender (a no-op when there's none), and
    /// [`OutboundReply::reply_to`] takes an explicit [`ReplyHandle`]. Both
    /// derive the kind from `K::ID`, so the cached `KindId<K>` argument
    /// here is redundant.
    #[deprecated(
        note = "use OutboundReply::reply / reply_to; ADR-0100 dropped the Serialize bound"
    )]
    pub fn reply_kind<K: Kind>(&self, sender: ReplyHandle, kind: KindId<K>, payload: &K) {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.reply_mail(sender.raw(), kind.raw(), &bytes, 1);
    }

    /// Reply target for the mail currently being dispatched. Mirrors
    /// [`OutboundReply::reply_target`].
    pub fn reply_target(&self) -> Option<ReplyHandle> {
        self.sender.map(ReplyHandle::__from_raw)
    }

    /// Singleton sender shortcut. Returns a typed [`FfiActorMailbox`]
    /// addressing the unique instance of receiver actor `R`.
    #[must_use]
    pub fn actor<R: Singleton>(&self) -> FfiActorMailbox<R> {
        FfiActorMailbox::__new(R::resolve(self.mailbox).0)
    }

    /// Multi-instance sender. Resolve a typed [`FfiActorMailbox`]
    /// from a runtime instance name.
    // Runtime-name escape hatch: the instance name is only known at runtime,
    // so there is no `R::resolve` lineage carry to route through.
    #[must_use]
    #[allow(clippy::disallowed_methods)]
    pub fn resolve_actor<R: Actor>(&self, name: &str) -> FfiActorMailbox<R> {
        FfiActorMailbox::__new(mailbox_id_from_name(name).0)
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
    pub fn spawn_child<A>(
        &self,
        subname: Subname<'_>,
        config: &A::Config,
    ) -> Result<MailboxId, SpawnError>
    where
        A: Instanced + FfiActor,
    {
        // Compile-time actor-type tag for the spawned sibling (hash(NAMESPACE),
        // ADR-0029) — this is the id definition for the new instance, computed
        // before any lineage carry exists.
        #[allow(clippy::disallowed_methods)]
        let tag = mailbox_id_from_name(<A as Actor>::NAMESPACE).0;
        let (is_counter, full_subname) = resolve_subname(subname)?;
        let config_bytes = config.encode_into_bytes();
        let id = MAIL_BRIDGE.spawn_sibling(tag, is_counter, &full_subname, &config_bytes);
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
    /// this ctx's per-component [`InlineRegistry`] keyed by the alias. Mail
    /// addressed to the alias lands in this slot and the `export!`
    /// membrane demuxes it to the child; the child's own sends stamp the
    /// child's address as origin and its replies route back.
    ///
    /// A [`Subname::Named`] that fails validation returns
    /// [`SpawnError::SubnameInvalid`]; a synchronous `init` `Err` returns
    /// [`SpawnError::InitFailed`].
    pub fn spawn_inline_child<A>(
        &self,
        subname: Subname<'_>,
        config: &A::Config,
    ) -> Result<MailboxId, SpawnError>
    where
        // `ErasedFfiActor` is the boxing seam every `#[actor]` type emits
        // (ADR-0096) — the registry stores the child as `dyn
        // ErasedFfiActor`, so the bound is the mechanical realisation of
        // "reuse the existing erasure" (no new child-dispatch trait).
        A: Instanced + FfiActor + ErasedFfiActor,
    {
        let (is_counter, full_subname) = resolve_subname(subname)?;
        let alias = MailboxId(MAIL_BRIDGE.spawn_inline_child(is_counter, &full_subname));
        // Re-decode an owned `A::Config` for the in-guest `init` from the
        // same bytes the detached path would have shipped — symmetric with
        // `spawn_child`'s encode-in-guest / decode-in-host round-trip, and
        // it sidesteps a `Clone` bound the detached verb also lacks.
        let bytes = config.encode_into_bytes();
        let Some(owned) = <A::Config as Kind>::decode_from_bytes(&bytes) else {
            return Err(SpawnError::InitFailed(BootError::new(
                "spawn_inline_child: Config round-trip failed",
            )));
        };
        // The actor-type tag the rehydrate reconstruct matches against the
        // module's exported types (ADR-0114 §5) — the same `hash(NAMESPACE)`
        // tag `init_typed_p32` selects on. This is the id definition for the
        // child type, so the disallowed-method allow mirrors `spawn_child`.
        #[allow(clippy::disallowed_methods)]
        let type_tag = mailbox_id_from_name(<A as Actor>::NAMESPACE).0;
        install_inline_child::<A>(
            self.inline,
            alias,
            type_tag,
            full_subname,
            is_counter,
            owned,
        )
    }

    /// ADR-0114: tear down an **inline child** spawned by
    /// [`Self::spawn_inline_child`]. Drops the child from this ctx's
    /// per-component [`InlineRegistry`] (running the child's `Drop`), so it
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
    /// No `unwire` runs on teardown in v1: inline children get only `init`
    /// today, so an `unwire` here would be asymmetric. The inline-child
    /// `wire` / `unwire` / subscription lifecycle lands separately, and
    /// teardown grows its `unwire` call then.
    // Despawn is a command; its `bool` ("was a resident child removed")
    // is informational and may be ignored, the same contract as
    // `BTreeMap::remove` / `HashSet::remove` (neither is `#[must_use]`).
    // The pedantic candidate lint only fires now that the body reads a
    // borrowed registry rather than mutating a crate-global static.
    #[allow(clippy::must_use_candidate)]
    pub fn despawn_inline_child(&self, child: MailboxId) -> bool {
        self.inline.remove(child)
    }
}

/// Resolve a [`Subname`] into the `(is_counter, discriminator)` pair the
/// spawn host fns take, shared by [`FfiCtx::spawn_child`] and
/// [`FfiCtx::spawn_inline_child`]. `Counter` passes an empty discriminator
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
/// `registry` (ADR-0114). Split out of [`FfiCtx::spawn_inline_child`] so
/// the in-guest `init` + registry insert is exercisable on the host build
/// (where the `spawn_inline_child` host fn is a panicking stub): the unit
/// test calls this with a local registry, a synthetic alias, and an owned
/// config.
///
/// ADR-0114 §5: `type_tag` / `full_subname` / `is_counter` are recorded in
/// the slot so a `replace_component` swap can reconstruct the child by
/// type and re-fold its metadata.
fn install_inline_child<A>(
    registry: &InlineRegistry,
    alias: MailboxId,
    type_tag: u64,
    full_subname: String,
    is_counter: bool,
    config: A::Config,
) -> Result<MailboxId, SpawnError>
where
    A: FfiActor + ErasedFfiActor,
{
    let mut ctx = FfiInitCtx::__new(alias.0);
    match A::init(config, &mut ctx) {
        Ok(child) => {
            registry.insert_child(alias, type_tag, full_subname, is_counter, Box::new(child));
            Ok(alias)
        }
        Err(err) => Err(SpawnError::InitFailed(err)),
    }
}

impl<M: ReplyMode> Resolver for FfiCtx<'_, M> {
    fn mailbox_id(&self) -> u64 {
        self.mailbox
    }

    fn resolve<K: Kind>(&self) -> KindId<K> {
        resolve::<K>()
    }

    fn resolve_mailbox<K: Kind>(&self, name: &str) -> Mailbox<K> {
        resolve_mailbox::<K>(name)
    }
}

impl<M: ReplyMode> MailSender for FfiCtx<'_, M> {
    //noinspection DuplicatedCode
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Singleton + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.send_mail(R::resolve(self.mailbox).0, K::ID.0, &bytes, 1, false);
    }

    //noinspection DuplicatedCode
    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Singleton + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        MAIL_BRIDGE.send_mail(
            R::resolve(self.mailbox).0,
            K::ID.0,
            bytes,
            payloads.len() as u32,
            false,
        );
    }

    //noinspection DuplicatedCode
    // Runtime-name send escape hatch (the `Resolver::send_to_named` contract):
    // the recipient name is supplied at runtime, no compile-time `R` to resolve.
    #[allow(clippy::disallowed_methods)]
    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.send_mail(mailbox_id_from_name(name).0, K::ID.0, &bytes, 1, false);
    }

    fn prev_correlation(&self) -> u64 {
        MAIL_BRIDGE.prev_correlation()
    }

    //noinspection DuplicatedCode
    fn send_detached<R, K>(&mut self, payload: &K)
    where
        R: Singleton + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.send_mail(R::resolve(self.mailbox).0, K::ID.0, &bytes, 1, true);
    }

    //noinspection DuplicatedCode
    // Runtime-name detached escape hatch — the `send_to_named` counterpart.
    #[allow(clippy::disallowed_methods)]
    fn send_detached_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.send_mail(mailbox_id_from_name(name).0, K::ID.0, &bytes, 1, true);
    }
}

// ADR-0112: the reply surface is per-mode. `Manual` carries it (a
// manual-class handler issues its own replies); `Single` deliberately
// does not, so a `-> ()` single handler is provably silent and a stray
// single-ctx `ctx.reply` is a compile error rather than a manifest lie.
impl OutboundReply for FfiCtx<'_, Manual> {
    type ReplyHandle = ReplyHandle;

    fn reply_target(&self) -> Option<ReplyHandle> {
        self.sender.map(ReplyHandle::__from_raw)
    }

    fn source_mailbox(&self) -> Option<MailboxId> {
        let handle = self.sender?;
        let id = MAIL_BRIDGE.source_of(handle);
        (id != MailboxId::NONE.0).then_some(MailboxId(id))
    }

    fn reply<K: Kind>(&mut self, payload: &K) {
        if let Some(raw) = self.sender {
            let bytes = payload.encode_into_bytes();
            MAIL_BRIDGE.reply_mail(raw, K::ID.0, &bytes, 1);
        }
    }

    fn reply_to<K: Kind>(&mut self, sender: ReplyHandle, payload: &K) {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.reply_mail(sender.raw(), K::ID.0, &bytes, 1);
    }
}

/// A `save_state` deposit captured in memory instead of forwarded to the
/// host `save_state` import (ADR-0114 §5). The dehydrate compose hands the
/// parent and each inline child a [`FfiDropCtx`] bound to one of these so
/// it can collect every saved blob and pack them into a single composite,
/// then call the real host `save_state` once.
#[derive(Default)]
pub struct CapturedState {
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
pub struct FfiDropCtx<'a> {
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

impl<'a> FfiDropCtx<'a> {
    /// Not part of the public API; called only by [`crate::export!`].
    /// Forwards `save_state` to the host import.
    #[doc(hidden)]
    #[must_use]
    pub fn __new(mailbox: u64) -> Self {
        Self {
            mailbox,
            capture: None,
            _borrow: PhantomData,
        }
    }

    /// Not part of the public API; called only by the dehydrate compose
    /// (`crate::ffi::inline::compose`). `save_state` records into `capture`
    /// rather than the host import, so the composite can be assembled
    /// before a single real host `save_state`.
    #[doc(hidden)]
    #[must_use]
    pub fn __new_capturing(mailbox: u64, capture: &'a mut CapturedState) -> Self {
        Self {
            mailbox,
            capture: Some(capture),
            _borrow: PhantomData,
        }
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
        let status = PERSIST_BRIDGE.save_state(version, bytes);
        assert_eq!(
            status, 0,
            "aether-actor: save_state failed (status {status})"
        );
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

impl MailSender for FfiDropCtx<'_> {
    //noinspection DuplicatedCode
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Singleton + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.send_mail(R::resolve(self.mailbox).0, K::ID.0, &bytes, 1, false);
    }

    //noinspection DuplicatedCode
    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Singleton + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        MAIL_BRIDGE.send_mail(
            R::resolve(self.mailbox).0,
            K::ID.0,
            bytes,
            payloads.len() as u32,
            false,
        );
    }

    //noinspection DuplicatedCode
    // Runtime-name send escape hatch (the `Resolver::send_to_named` contract):
    // the recipient name is supplied at runtime, no compile-time `R` to resolve.
    #[allow(clippy::disallowed_methods)]
    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.send_mail(mailbox_id_from_name(name).0, K::ID.0, &bytes, 1, false);
    }

    fn prev_correlation(&self) -> u64 {
        MAIL_BRIDGE.prev_correlation()
    }

    //noinspection DuplicatedCode
    fn send_detached<R, K>(&mut self, payload: &K)
    where
        R: Singleton + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.send_mail(R::resolve(self.mailbox).0, K::ID.0, &bytes, 1, true);
    }

    //noinspection DuplicatedCode
    // Runtime-name detached escape hatch — the `send_to_named` counterpart.
    #[allow(clippy::disallowed_methods)]
    fn send_detached_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.send_mail(mailbox_id_from_name(name).0, K::ID.0, &bytes, 1, true);
    }
}

impl Persistence for FfiDropCtx<'_> {
    fn save_state(&mut self, version: u32, bytes: &[u8]) {
        // Route through the inherent `save_state` so the ADR-0114 §5
        // capture path applies — the generated `on_dehydrate` hooks reach
        // the bundle through `Persistence::save_state_kind`, which calls
        // this trait method, so a capturing ctx must intercept here too.
        FfiDropCtx::save_state(self, version, bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::{FfiCtx, InlineRegistry, Manual, Single, SpawnError, install_inline_child};
    use crate::Actor;
    use crate::actor::ctx::{OutboundReply, Resolver};
    use crate::actor::{Instanced, Subname};
    use crate::ffi::{BootError, ErasedFfiActor, FfiActor, FfiDropCtx};
    use crate::mail::{Mail, PriorState};
    use aether_data::MailboxId;
    use alloc::string::String;
    use core::mem::{align_of, size_of};

    /// Test inline child whose `init` always fails — drives the
    /// [`SpawnError::InitFailed`] path. The `ErasedFfiActor` dispatch
    /// hooks are unreachable: a failed `init` never registers or
    /// dispatches the child.
    struct FailingChild;

    impl Actor for FailingChild {
        const NAMESPACE: &'static str = "test.inline.failing_child";
    }

    impl Instanced for FailingChild {}

    impl FfiActor for FailingChild {
        type Config = ();
        type State = ();

        fn init<C>(_config: (), _ctx: &mut C) -> Result<Self, BootError>
        where
            C: Resolver,
        {
            Err(BootError::new("inline child init deliberately fails"))
        }
    }

    impl ErasedFfiActor for FailingChild {
        fn erased_namespace(&self) -> &'static str {
            Self::NAMESPACE
        }
        fn erased_dispatch(&mut self, _ctx: &mut FfiCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
            unreachable!("a failed-init child is never dispatched")
        }
        fn erased_wire(&mut self, _ctx: &mut FfiCtx<'_, Manual>) {
            unreachable!()
        }
        fn erased_unwire(&mut self, _ctx: &mut FfiCtx<'_, Manual>) {
            unreachable!()
        }
        fn erased_on_dehydrate(&mut self, _ctx: &mut FfiDropCtx<'_>) {
            unreachable!()
        }
        fn erased_on_rehydrate(&mut self, _ctx: &mut FfiCtx<'_, Manual>, _prior: PriorState<'_>) {
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
        let registry = InlineRegistry::new();
        let result = install_inline_child::<FailingChild>(
            &registry,
            MailboxId(0x5555),
            0,
            String::from("child"),
            false,
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
        let registry = InlineRegistry::new();
        let ctx = FfiCtx::__new(0, &registry);
        let result = ctx.spawn_inline_child::<FailingChild>(Subname::Named("bad:name"), &());
        assert!(
            matches!(result, Err(SpawnError::SubnameInvalid(_))),
            "a separator-bearing subname must return SubnameInvalid, got {result:?}",
        );
    }

    /// Test inline child whose `init` succeeds, so `install_inline_child`
    /// registers it in the test-local registry for the despawn test. Its
    /// dispatch hooks are unreachable here — the test only installs then
    /// despawns.
    struct SucceedingChild;

    impl Actor for SucceedingChild {
        const NAMESPACE: &'static str = "test.inline.succeeding_child";
    }

    impl Instanced for SucceedingChild {}

    impl FfiActor for SucceedingChild {
        type Config = ();
        type State = ();

        fn init<C>(_config: (), _ctx: &mut C) -> Result<Self, BootError>
        where
            C: Resolver,
        {
            Ok(Self)
        }
    }

    impl ErasedFfiActor for SucceedingChild {
        fn erased_namespace(&self) -> &'static str {
            Self::NAMESPACE
        }
        fn erased_dispatch(&mut self, _ctx: &mut FfiCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
            unreachable!("the despawn test never dispatches this child")
        }
        fn erased_wire(&mut self, _ctx: &mut FfiCtx<'_, Manual>) {}
        fn erased_unwire(&mut self, _ctx: &mut FfiCtx<'_, Manual>) {}
        fn erased_on_dehydrate(&mut self, _ctx: &mut FfiDropCtx<'_>) {}
        fn erased_on_rehydrate(&mut self, _ctx: &mut FfiCtx<'_, Manual>, _prior: PriorState<'_>) {}
    }

    /// Step 2: `despawn_inline_child` removes a resident inline child
    /// (returns `true`) and is idempotent — a second despawn of the same
    /// alias returns `false`, not an error. Installs the child directly
    /// (the host build's `spawn_inline_child` host fn is a panicking stub).
    #[test]
    fn despawn_inline_child_removes_then_idempotent() {
        let registry = InlineRegistry::new();
        let alias = MailboxId(0x6161);
        install_inline_child::<SucceedingChild>(
            &registry,
            alias,
            0,
            String::from("widget"),
            false,
            (),
        )
        .expect("a succeeding init installs the inline child");

        let ctx = FfiCtx::__new(alias.0, &registry);
        assert!(
            ctx.despawn_inline_child(alias),
            "despawning a resident child returns true",
        );
        assert!(
            !ctx.despawn_inline_child(alias),
            "a second despawn of the same alias returns false (idempotent)",
        );
    }

    /// ADR-0112: the mode marker is layout-neutral — the `Single` and
    /// `Manual` views have identical size + alignment. This is the
    /// invariant the `as_single` / `as_stream` pointer reborrows rest on.
    #[test]
    fn ffi_ctx_layout_identical_across_modes() {
        assert_eq!(
            size_of::<FfiCtx<'static, Single>>(),
            size_of::<FfiCtx<'static, Manual>>(),
        );
        assert_eq!(
            align_of::<FfiCtx<'static, Single>>(),
            align_of::<FfiCtx<'static, Manual>>(),
        );
    }

    /// ADR-0112: `OutboundReply` is reachable from the `Manual` ctx
    /// only. The single-locked ctx carries no reply surface, so a `-> ()`
    /// single handler is provably silent (a stray single-ctx `ctx.reply`
    /// is a compile error, not a manifest lie).
    #[test]
    fn outbound_reply_present_on_manual() {
        fn assert_impls<C: OutboundReply>() {}
        assert_impls::<FfiCtx<'static, Manual>>();
    }
}
