//! The receive-stage ctx — [`WasmCtx`], the per-mail handle every handler
//! and every post-init lifecycle hook is handed: its fields, its
//! construction and reply-mode coercions, and the inbound accessors that
//! read them. Its outbound mail surface lives in `super::send`, its
//! cluster-relative addressing in `super::relative`, and its child-spawning
//! verbs in `super::spawn`.

use core::marker::PhantomData;
use core::num::NonZeroU64;
use core::ptr;

use aether_data::{Kind, MailboxId, RequestId, Source};

use crate::mail::ReplyHandle;
use crate::model::ctx::Erased;
use crate::model::ctx::reply_mode::{Manual, ReplyMode, Single};
use crate::model::{
    Addressable, CallerAddressable, CallerScope, CallerScoped, DependencyResolver, DependsOn, Reaches, Singleton,
};
use crate::reference::{ActorRef, ErasedActorRef};
use crate::wasm::bridge::mail;
use crate::wasm::inline::Registry;
use crate::wasm::mailbox::WasmActorMailbox;
use alloc::string::String;

/// Per-receive (and post-init `wire` / pre-shutdown `unwire`)
/// capability handle for FFI guests. Exposes send, reply, and the
/// inherent [`mailbox_id`](WasmCtx::mailbox_id) for cases that need to
/// address this component explicitly.
// The `Wasm` prefix carries the native/wasm split signal; bare `Ctx` loses that.
#[allow(clippy::module_name_repetitions)]
pub struct WasmCtx<'a, A = Erased, M: ReplyMode = Single> {
    pub(super) mailbox: u64,
    pub(super) sender: Option<ReplyHandle>,
    /// The inbound source — a proof of whoever sent the mail currently being
    /// dispatched, decoded once from the raw id threaded onto the ctx at
    /// construction (issues 1987 + 2001). For an in-place (intra-cluster)
    /// dispatch off the drain this is the enqueuing member's id (the in-place
    /// reply table is empty, so the ctx is the only carrier); for a top-level
    /// dispatch the host resolves the source from the inbound's `SourceAddr`
    /// and threads it as the trailing `receive_p32` ABI slot. So
    /// [`Self::sender`] is a single read of this field on both paths.
    /// `None` (the raw [`NO_INBOUND_SOURCE`] encoding) means no peer-component
    /// origin — a session, remote-engine, or broadcast mail, or a lifecycle
    /// hook with no inbound.
    pub(super) source: Option<ErasedActorRef>,
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
    pub(super) inline: &'a Registry,
    _borrow: PhantomData<&'a ()>,
    /// ADR-0112: phantom reply-mode marker (a ZST, layout-neutral) that
    /// selects which reply surface this ctx exposes. Defaults to
    /// [`Single`], so the common `WasmCtx<'_>` signature is unchanged.
    _mode: PhantomData<M>,
    /// Phantom marker naming the actor this ctx dispatches *for*. The
    /// `#[actor]` macro supplies it — a handler that spells its actor
    /// receives the typed form, every other arm the [`Erased`] view — so
    /// the default keeps the common `WasmCtx<'_>` signature unchanged.
    _actor: PhantomData<fn() -> A>,
}

/// The `source` argument to [`WasmCtx::__new`] for a dispatch that carries no
/// inbound source — a lifecycle hook (`wire` / `unwire` / `on_rehydrate`),
/// where [`WasmCtx::sender`] returns `None`. It is also the `receive_p32`
/// source slot's encoding of "no source" (ADR-0033): a top-level mail dispatch
/// threads the host-resolved source over that ABI slot, and `0` there means the
/// mail has no peer-component origin. The drained-member path threads the
/// enqueuing member's own id. Named so the `__new` call sites read intent, not
/// a bare `0`.
#[doc(hidden)]
pub const NO_INBOUND_SOURCE: u64 = 0;

/// Decode a raw inbound source into the proof [`WasmCtx::sender`] hands out,
/// reading [`NO_INBOUND_SOURCE`] as no source.
fn decode_source(source: u64) -> Option<ErasedActorRef> {
    NonZeroU64::new(source).map(|raw| ErasedActorRef::new(MailboxId(raw.get())))
}

impl<'a> WasmCtx<'a, Erased, Manual> {
    /// Not part of the public API; called only by [`crate::export!`] and
    /// the inline membrane / drain. The runtime builds the most-permissive
    /// [`Manual`] view, with the actor [`Erased`] — the entry points run
    /// where no actor type is in scope, so the `#[actor]` macro upgrades to
    /// the typed form per handler with [`Self::__for_actor`] and downgrades
    /// per handler class with [`Self::as_single`].
    ///
    /// `source` is the inbound source (issues 1987 + 2001): the enqueuing
    /// member's id for an in-place drained dispatch, the host-resolved source
    /// for a top-level mail dispatch (threaded over the `receive_p32` ABI), or
    /// [`NO_INBOUND_SOURCE`] (`0`) for a lifecycle hook with no inbound mail.
    #[doc(hidden)]
    #[must_use]
    pub fn __new(mailbox: u64, inline: &'a Registry, source: u64) -> Self {
        Self {
            mailbox,
            sender: None,
            source: decode_source(source),
            host_dispatch: true,
            inline,
            _borrow: PhantomData,
            _mode: PhantomData,
            _actor: PhantomData,
        }
    }

    /// Not part of the public API; inline-cluster drains build ctxs through
    /// here so `in_reply_to()` does not read the outer host dispatch's stale
    /// reply-correlation scalar.
    #[doc(hidden)]
    #[must_use]
    pub fn __new_local_dispatch(mailbox: u64, inline: &'a Registry, source: u64) -> Self {
        Self {
            mailbox,
            sender: None,
            source: decode_source(source),
            host_dispatch: false,
            inline,
            _borrow: PhantomData,
            _mode: PhantomData,
            _actor: PhantomData,
        }
    }
}

impl<'a, A> WasmCtx<'a, A, Manual> {
    /// ADR-0112 downgrade-only coercion: view this [`Manual`] ctx as a
    /// [`Single`] ctx, dropping the `OutboundReply` surface. The
    /// `#[actor]` macro hands a single-class handler this view, so a
    /// handler whose marker disagrees with its class fails to unify.
    /// There is deliberately no `as_manual` — the runtime only ever
    /// downgrades. Preserves the actor marker `A`.
    #[doc(hidden)]
    #[must_use]
    pub fn as_single(&mut self) -> &mut WasmCtx<'a, A, Single> {
        // SAFETY: `M` is `PhantomData`-only, so `WasmCtx<'a, A, Manual>` and
        // `WasmCtx<'a, A, Single>` are layout-identical (the marker field is a
        // ZST for every `M` — see `reply_mode_types_are_zsts` and
        // `ffi_ctx_layout_identical_across_modes`). The reborrow swaps the
        // marker without touching any real field and only removes
        // capability, never adds it.
        unsafe { &mut *ptr::from_mut(self).cast::<WasmCtx<'a, A, Single>>() }
    }
}

impl<'a, M: ReplyMode> WasmCtx<'a, Erased, M> {
    /// Upgrade this erased ctx to the actor being dispatched (issue 6279).
    /// The `#[actor]` macro calls it with `Self` for a handler, `#[fallback]`,
    /// `wire`, or `unwire` hook whose signature names its actor, ahead of the
    /// per-class [`Self::as_single`] downgrade; every
    /// other arm receives the erased ctx as today. Defined on the erased form
    /// only, so the upgrade always starts from the dispatcher's erased ctx.
    ///
    /// Not part of the public API; the macro is the only intended caller.
    #[doc(hidden)]
    #[must_use]
    pub fn __for_actor<A>(&mut self) -> &mut WasmCtx<'a, A, M> {
        // SAFETY: `A` appears only in `PhantomData`, so `WasmCtx<'a, Erased, M>`
        // and `WasmCtx<'a, A, M>` are layout-identical for every `A` (see
        // `ffi_ctx_layout_identical_across_modes`). The reborrow swaps the
        // marker without touching any real field.
        unsafe { &mut *ptr::from_mut(self).cast::<WasmCtx<'a, A, M>>() }
    }
}

impl<'a, A, M: ReplyMode> WasmCtx<'a, A, M> {
    /// Downgrade-only coercion: view this ctx as one that names no actor. A
    /// handler that spells its actor reaches an erased-only helper through
    /// this. Like [`Self::as_single`] the coercion only removes capability —
    /// the way back up is the macro's [`Self::__for_actor`].
    #[must_use]
    pub fn erase(&mut self) -> &mut WasmCtx<'a, Erased, M> {
        // SAFETY: `A` appears only in `PhantomData`, so `WasmCtx<'a, A, M>` and
        // `WasmCtx<'a, Erased, M>` are layout-identical for every `A` (see
        // `ffi_ctx_layout_identical_across_modes`). The reborrow swaps the
        // marker without touching any real field.
        unsafe { &mut *ptr::from_mut(self).cast::<WasmCtx<'a, Erased, M>>() }
    }
}

impl<A, M: ReplyMode> WasmCtx<'_, A, M> {
    pub(super) fn scope_mailbox(&self, scope: CallerScope) -> u64 {
        self.inline.scope_mailbox(MailboxId(self.mailbox), scope)
    }

    /// Not part of the public API; called only by the `#[actor]`
    /// dispatcher. Accepts `None` or `Some(ReplyHandle)` — the dispatcher
    /// passes `mail.reply_handle()` verbatim so component-origin and
    /// broadcast mail (which have no reply target) land as `None`.
    #[doc(hidden)]
    pub fn __set_reply_to(&mut self, sender: Option<ReplyHandle>) {
        self.sender = sender;
    }

    /// Reply target for the mail currently being dispatched. Mirrors
    /// [`OutboundReply::reply_target`](crate::OutboundReply::reply_target).
    #[must_use]
    pub fn reply_target(&self) -> Option<ReplyHandle> {
        self.sender
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
    /// answers. Returns `None` for ordinary mail, unmatched replies, a wrong
    /// context kind, or a decode failure.
    ///
    /// A wrong-kind take leaves the context stored, so a handler that serves
    /// several context kinds tries each type in turn. A decode failure
    /// consumes it.
    pub fn take_context<C: Kind>(&mut self) -> Option<C> {
        let request = self.in_reply_to()?;
        self.inline.take_request_context(request)
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
    pub fn actor<R: Singleton + CallerAddressable>(&self) -> WasmActorMailbox<'_, R>
    where
        A: Reaches<R>,
    {
        self.singleton_handle::<R>()
    }

    /// Proven reference to a declared dependency (ADR-0230): mints an
    /// [`ActorRef`] for the position [`Self::actor`] folds for `R`, with no
    /// host call — the load was refused unless `R` was `Live`, so the answer
    /// is already known. Bounded `A: DependsOn<R>` directly, so it does not
    /// exist on the erased ctx.
    #[must_use]
    pub fn actor_ref<R: Singleton + CallerAddressable>(&self) -> ActorRef<R>
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        ActorRef::new(self.singleton_handle::<R>().mailbox_id())
    }

    /// The envelope sender as a proven [`ErasedActorRef`]: mints the dispatch
    /// source the host stamped, with no lookup. `None` for a sourceless
    /// dispatch (session / remote-engine / broadcast mail, or a lifecycle
    /// hook with no inbound). Needs no actor type, so it exists on the
    /// erased ctx too: a `#[fallback]` runs on the downgraded [`Single`]
    /// view (issue 2687), and a cluster-membrane interposer reads its lane
    /// direction there by comparing the sender against a stored child.
    #[must_use]
    pub fn sender(&self) -> Option<ErasedActorRef> {
        self.source
    }

    /// Typed singleton handle construction: the shared body behind
    /// [`Self::actor`] and [`Self::actor_ref`]. Folds `R::NAMESPACE` under the
    /// resolver-selected root/current/parent routing scope.
    #[must_use]
    pub(crate) fn singleton_handle<R: Singleton + CallerAddressable>(&self) -> WasmActorMailbox<'_, R> {
        WasmActorMailbox::new(
            R::resolve(self.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE), ()).0,
            self.mailbox,
            self.inline,
        )
    }

    /// Send through a proven [`ActorRef`]: returns a ctx-bound [`WasmActorMailbox`]
    /// addressing the reference's id, carrying this actor's own id as the send's
    /// `from` and a borrow of the inline registry the send routes through.
    #[must_use]
    pub fn to<R: Addressable>(&self, target: &ActorRef<R>) -> WasmActorMailbox<'_, R> {
        WasmActorMailbox::new(target.id().0, self.mailbox, self.inline)
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
}
