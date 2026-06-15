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
use crate::ffi::FfiActor;
use crate::ffi::bridge::{MAIL_BRIDGE, PERSIST_BRIDGE};
use crate::ffi::mailbox::FfiActorMailbox;
use crate::mail::ReplyHandle;
use crate::mail::mailbox::{KindId, Mailbox, resolve, resolve_mailbox};
use alloc::string::String;

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

/// Why a synchronous [`FfiCtx::spawn_child`] call failed before the host
/// staged the request (ADR-0097). Spawn-time failures — a retired or
/// in-use subname, or the sibling's `init` returning `Err` — surface
/// asynchronously on the trampoline, not through this `Result`.
#[derive(Debug, Clone)]
pub enum SpawnError {
    /// A [`Subname::Named`] discriminator failed
    /// [`validate_namespace_segment`].
    SubnameInvalid(NamespaceError),
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
    pub fn __new(mailbox: u64) -> Self {
        Self {
            mailbox,
            sender: None,
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
        let (is_counter, full_subname) = match subname {
            // `Counter`: pass the type-namespace prefix; the host appends
            // its monotonic discriminator so the name is globally unique.
            Subname::Counter => (true, String::from(<A as Actor>::NAMESPACE)),
            // `Named`: form the full prefixed subname here; validate the
            // caller-supplied segment before it crosses the FFI.
            Subname::Named(name) => {
                validate_namespace_segment(name).map_err(SpawnError::SubnameInvalid)?;
                (
                    false,
                    alloc::format!("{}.{}", <A as Actor>::NAMESPACE, name),
                )
            }
        };
        let config_bytes = config.encode_into_bytes();
        let id = MAIL_BRIDGE.spawn_sibling(tag, is_counter, &full_subname, &config_bytes);
        Ok(MailboxId(id))
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
        None
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

/// Narrowed capability handle for the `on_dehydrate` save hook.
/// Outbound mail still works through [`MailSender`]; the reply / resolve
/// surfaces are intentionally absent.
pub struct FfiDropCtx<'a> {
    /// The actor's own mailbox id (its lineage carry), so a buffered
    /// `send` resolves the receiver through `R::resolve(self.mailbox)`
    /// like every other ctx (ADR-0099 §5).
    mailbox: u64,
    _borrow: PhantomData<&'a ()>,
}

impl FfiDropCtx<'_> {
    /// Not part of the public API; called only by [`crate::export!`].
    #[doc(hidden)]
    #[must_use]
    pub fn __new(mailbox: u64) -> Self {
        Self {
            mailbox,
            _borrow: PhantomData,
        }
    }

    /// Deposit a migration bundle. Mirrors [`Persistence::save_state`].
    ///
    /// # Panics
    /// Panics if the host `save_state` import returns non-zero — fail-fast
    /// per ADR-0063: the persistence bridge is part of the substrate
    /// contract and a failure here means the runtime is in an
    /// unrecoverable state.
    pub fn save_state(&mut self, version: u32, bytes: &[u8]) {
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
        let status = PERSIST_BRIDGE.save_state(version, bytes);
        assert_eq!(
            status, 0,
            "aether-actor: save_state failed (status {status})"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{FfiCtx, Manual, Single};
    use crate::actor::ctx::OutboundReply;
    use core::mem::{align_of, size_of};

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
