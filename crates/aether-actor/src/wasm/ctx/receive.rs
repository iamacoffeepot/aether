//! The receive-stage ctx — [`WasmCtx`], the per-mail handle every handler
//! and every post-init lifecycle hook is handed: its fields, its
//! construction and reply-mode coercions, and the inbound accessors that
//! read them. Its outbound mail surface lives in `super::send`, its typed
//! parent door in `super::parent`, and its child-spawning
//! verbs in `super::spawn`.

use core::marker::PhantomData;
use core::num::NonZeroU64;
use core::ptr;

use aether_data::{Kind, MailboxId, RequestId, Source};

use crate::mail::ReplyHandle;
use crate::model::ctx::Erased;
use crate::model::ctx::reply_mode::{ReplyMode, Single, Unchecked};
use crate::model::{
    Addressable, Anyone, CallerAddressable, CallerScope, CallerScoped, DependencyResolver, DependsOn,
    SenderRequirement, Singleton,
};
use crate::reference::{ActorRef, ErasedActorRef};
use crate::wasm::bridge::mail;
use crate::wasm::inline::Registry;
use alloc::string::String;

/// Per-receive (and post-init `wire` / pre-shutdown `unwire`)
/// capability handle for FFI guests. Exposes send, reply, and
/// cluster-relative addressing; it never reveals the actor's own mailbox
/// position (ADR-0230).
// The `Wasm` prefix carries the native/wasm split signal; bare `Ctx` loses that.
#[allow(clippy::module_name_repetitions)]
pub struct WasmCtx<'a, A = Erased, S = Anyone, M: ReplyMode = Single> {
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
    /// not read the outer dispatch's ambient host scalar. A departure notice
    /// is host mail too, so the watch it ends is asked for only here.
    pub(super) host_dispatch: bool,
    /// ADR-0114: the per-component inline-child registry the
    /// [`Self::spawn_inline_child`] / [`Self::despawn_inline_child`] verbs
    /// drive. The `export!` membrane threads in the component's emitted
    /// `static __AETHER_INLINE` (a `&'static` that coerces to `&'a`); a
    /// host unit test threads in a local registry. Held by reference
    /// rather than reached as a global — the same discipline the parent
    /// slot (`__AETHER_COMPONENT`) already follows.
    pub(super) inline: &'a Registry,
    /// ADR-0243 §7: whether this dispatch already armed a held reply with
    /// [`Self::hold`]. A second hold would owe two replies to one request.
    /// A plain `bool`, the same in every reply mode, so the mode and actor
    /// reborrows stay layout-identical.
    pub(super) held_armed: bool,
    _borrow: PhantomData<&'a ()>,
    /// ADR-0112: phantom reply-mode marker (a ZST, layout-neutral) that
    /// selects which reply surface this ctx exposes. Defaults to
    /// [`Single`], so the common `WasmCtx<'_>` signature is unchanged.
    _mode: PhantomData<M>,
    /// Phantom marker naming the actor this ctx dispatches *for*. The
    /// `#[actor]` macro supplies it: a method whose ctx omits its actor is
    /// typed by it (ADR-0231 §7), and only one that spells [`Erased`]
    /// receives the erased view. The type default is [`Erased`], for a ctx
    /// built where no actor is in scope.
    _actor: PhantomData<fn() -> A>,
    /// Phantom marker naming what this ctx's handler requires of its sender
    /// (ADR-0231 §11), and so what [`Self::sender`] hands out. The `#[actor]`
    /// arm of a handler that names a protocol here retypes the ctx to it once
    /// the sender cast passes; every other ctx carries [`Anyone`], the type
    /// default.
    _sender: PhantomData<fn() -> S>,
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

impl<'a> WasmCtx<'a, Erased, Anyone, Unchecked> {
    /// Not part of the public API; called only by [`crate::export!`] and
    /// the inline membrane / drain. The runtime builds the most-permissive
    /// [`Unchecked`] view, with the actor [`Erased`] — the entry points run
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
            held_armed: false,
            _borrow: PhantomData,
            _mode: PhantomData,
            _actor: PhantomData,
            _sender: PhantomData,
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
            held_armed: false,
            _borrow: PhantomData,
            _mode: PhantomData,
            _actor: PhantomData,
            _sender: PhantomData,
        }
    }
}

impl<'a, A, S> WasmCtx<'a, A, S, Unchecked> {
    /// ADR-0112 downgrade-only coercion: view this [`Unchecked`] ctx as a
    /// [`Single`] ctx, dropping the `OutboundReply` surface. The
    /// `#[actor]` macro hands a single-class handler this view, so a
    /// handler whose marker disagrees with its class fails to unify.
    /// There is deliberately no `as_unchecked` — the runtime only ever
    /// downgrades. Preserves the actor marker `A` and the sender marker `S`.
    #[doc(hidden)]
    #[must_use]
    pub fn as_single(&mut self) -> &mut WasmCtx<'a, A, S, Single> {
        // SAFETY: `M` is `PhantomData`-only, so `WasmCtx<'a, A, S, Unchecked>` and
        // `WasmCtx<'a, A, S, Single>` are layout-identical (the marker field is a
        // ZST for every `M` — see `reply_mode_types_are_zsts` and
        // `ffi_ctx_layout_identical_across_modes`). The reborrow swaps the
        // marker without touching any real field and only removes
        // capability, never adds it.
        unsafe { &mut *ptr::from_mut(self).cast::<WasmCtx<'a, A, S, Single>>() }
    }

    /// Accept a returned [`Pending<R>`](super::Pending) receipt. The
    /// `#[actor]` macro calls this on the value a `-> Pending<R>` handler
    /// returns, once its `as_single` reborrow has ended; a single handler
    /// never holds this `<Unchecked>` view, so it cannot disarm its own receipt
    /// and declare a false `Silent` row.
    #[doc(hidden)]
    pub fn __accept_pending<R>(&mut self, pending: super::Pending<R>) {
        pending.disarm();
    }

    /// Reply target for the mail currently being dispatched. Mirrors
    /// [`OutboundReply::reply_target`](crate::OutboundReply::reply_target).
    ///
    /// The handle belongs to the unchecked reply surface: an unchecked handler may
    /// keep it and answer later with `reply_to`. A single handler cannot read
    /// it, because the substrate frees a single-class dispatch's handle when
    /// the handler returns (ADR-0112, #6412).
    #[must_use]
    pub fn reply_target(&self) -> Option<ReplyHandle> {
        self.sender
    }
}

impl<'a, S, M: ReplyMode> WasmCtx<'a, Erased, S, M> {
    /// Upgrade this erased ctx to the actor being dispatched (issue 6279).
    /// The `#[actor]` macro calls it with `Self` for a handler or `#[fallback]`
    /// whose signature names its actor — every one that does not spell
    /// [`Erased`] (ADR-0231 §7) — ahead of the per-class [`Self::as_single`]
    /// downgrade, and once at an adopted handler set's delegation, whose
    /// dispatch method takes the ctx typed by its adopter. Defined on the
    /// erased form only, so the upgrade always starts from the dispatcher's
    /// erased ctx.
    ///
    /// The lifecycle ctx is typed by its actor, so the macro's
    /// `ErasedWasmActor::erased_wire` / `erased_unwire` /
    /// `erased_on_rehydrate` and `export!`'s single-actor `wire` / `unwire` /
    /// `on_rehydrate` shims are its other callers: each upgrades once, where
    /// the erased ctx is born at the FFI boundary.
    ///
    /// Not part of the public API; the macro and `export!` are the only
    /// intended callers.
    #[doc(hidden)]
    #[must_use]
    pub fn __for_actor<A>(&mut self) -> &mut WasmCtx<'a, A, S, M> {
        // SAFETY: `A` appears only in `PhantomData`, so `WasmCtx<'a, Erased, S, M>`
        // and `WasmCtx<'a, A, S, M>` are layout-identical for every `A` (see
        // `ffi_ctx_layout_identical_across_modes`). The reborrow swaps the
        // marker without touching any real field.
        unsafe { &mut *ptr::from_mut(self).cast::<WasmCtx<'a, A, S, M>>() }
    }
}

impl<'a, A, S, M: ReplyMode> WasmCtx<'a, A, S, M> {
    /// Downgrade-only coercion: view this ctx as one that names no actor. A
    /// typed handler — every handler that does not spell [`Erased`] — reaches
    /// an erased-only helper through this. Like [`Self::as_single`] the
    /// coercion only removes capability — the way back up is the macro's
    /// [`Self::__for_actor`].
    #[must_use]
    pub fn erase(&mut self) -> &mut WasmCtx<'a, Erased, S, M> {
        // SAFETY: `A` appears only in `PhantomData`, so `WasmCtx<'a, A, S, M>` and
        // `WasmCtx<'a, Erased, S, M>` are layout-identical for every `A` (see
        // `ffi_ctx_layout_identical_across_modes`). The reborrow swaps the
        // marker without touching any real field.
        unsafe { &mut *ptr::from_mut(self).cast::<WasmCtx<'a, Erased, S, M>>() }
    }
}

impl<A, S, M: ReplyMode> WasmCtx<'_, A, S, M> {
    pub(super) fn scope_mailbox(&self, scope: CallerScope) -> u64 {
        scope.select(MailboxId(self.mailbox))
    }

    /// Not part of the public API; called only by the `#[actor]`
    /// dispatcher. Accepts `None` or `Some(ReplyHandle)` — the dispatcher
    /// passes `mail.reply_handle()` verbatim so component-origin and
    /// broadcast mail (which have no reply target) land as `None`.
    #[doc(hidden)]
    pub fn __set_reply_to(&mut self, sender: Option<ReplyHandle>) {
        self.sender = sender;
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
    ///
    /// Each [`Held`](crate::Held) the context carries is claimed back live, to
    /// be answered or parked again (ADR-0243 §4). A reply whose stored context
    /// holds a `Held` must take it before its handler returns: the `export!`
    /// receive shim panics, naming the context kind, when it is left untaken.
    pub fn take_context<C: Kind>(&mut self) -> Option<C> {
        let request = self.in_reply_to()?;
        self.inline.take_request_context(request)
    }

    /// Proven reference to a declared dependency (ADR-0230): mints an
    /// [`ActorRef`] for the position `R`'s resolver folds from the
    /// root / current / parent routing scope it selects, with no host call —
    /// the load was refused unless `R` was `Live`, so the answer is already
    /// known. Bounded `A: DependsOn<R>` directly, so it does not exist on the
    /// erased ctx. Every flat typed send routes to this position.
    #[must_use]
    pub fn actor_ref<R: Singleton + CallerAddressable>(&self) -> ActorRef<R>
    where
        A: DependsOn<R>,
        R::Resolver: DependencyResolver,
    {
        ActorRef::new(R::resolve(self.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE), ()))
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

impl<A, S: SenderRequirement, M: ReplyMode> WasmCtx<'_, A, S, M> {
    /// The envelope sender, as what this ctx's sender requirement `S` hands
    /// out (ADR-0231 §11).
    ///
    /// A reply's sender is the actor that replied, as it is for a native
    /// handler.
    ///
    /// On a ctx that states nothing, which is [`Anyone`], it is a proven
    /// [`ErasedActorRef`]: the dispatch source the host stamped, minted with
    /// no lookup, and `None` for a sourceless dispatch (session /
    /// remote-engine / broadcast mail, or a lifecycle hook with no inbound).
    /// It needs no actor type, so it exists on the erased ctx too: a
    /// `#[fallback]` runs on the downgraded [`Single`] view (issue 2687), and
    /// a cluster-membrane interposer reads its lane direction there by
    /// comparing the sender against a stored child.
    ///
    /// On a ctx whose handler names a protocol `P` as its sender it is the
    /// [`ProtocolRef<P>`](crate::ProtocolRef) the dispatch arm proved before
    /// the handler ran, with no `Option` and no cast. Both read the one
    /// `source` stamp, which is fixed at construction, so the typed reference
    /// names the position the arm's cast proved.
    #[must_use]
    pub fn sender(&self) -> S::Reference {
        S::__reference(self.source.map(ErasedActorRef::id), |_| true)
    }
}
