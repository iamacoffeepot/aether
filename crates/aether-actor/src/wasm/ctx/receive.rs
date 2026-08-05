//! The receive-stage ctx — [`WasmCtx`], the per-mail handle every handler
//! and every post-init lifecycle hook is handed: its fields, its
//! construction and reply-mode coercions, and the inbound accessors that
//! read them. Its outbound mail surface lives in `super::send`, its
//! cluster-relative addressing in `super::relative`, and its child-spawning
//! verbs in `super::spawn`.

use core::marker::PhantomData;
use core::ptr;

use aether_data::{Kind, MailboxId, RequestId, Source, mailbox_id_from_name};

use crate::mail::ReplyHandle;
use crate::model::ctx::reply_mode::{Manual, Multi, ReplyMode, Single};
use crate::model::{Addressable, CallerAddressable, CallerScope, CallerScoped, Resolve, Singleton};
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
pub struct WasmCtx<'a, M: ReplyMode = Single> {
    pub(super) mailbox: u64,
    pub(super) sender: Option<ReplyHandle>,
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
    pub(super) source: u64,
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
    /// [`Emit<K>`](crate::Emit) surface. The `#[actor]` macro hands a `#[handler::multi]`
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
    pub(super) fn scope_mailbox(&self, scope: CallerScope) -> u64 {
        self.inline.scope_mailbox(MailboxId(self.mailbox), scope).0
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

    /// The inbound source — the folded [`MailboxId`] of whoever sent the
    /// mail currently being dispatched, or `None` for a sourceless dispatch
    /// (session / remote-engine / broadcast mail, or a lifecycle hook with
    /// no inbound). Reads the same `source` field as
    /// [`OutboundReply::source_mailbox`](crate::OutboundReply::source_mailbox), but on the generic ctx so a
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
    pub fn actor<R: Singleton + CallerAddressable>(&self) -> WasmActorMailbox<'_, R> {
        self.__actor_with_namespace::<R>(R::NAMESPACE)
    }

    /// Namespace-aware typed actor construction for cap-owned facades whose
    /// recipient namespace is a runtime fact. Uses the same resolver-selected
    /// root/current/parent routing scope as [`Self::actor`]; unlike
    /// [`Self::resolve_actor`], this does not flatten `namespace` into a root
    /// mailbox name.
    #[doc(hidden)]
    #[must_use]
    pub fn __actor_with_namespace<R: Singleton + CallerAddressable>(&self, namespace: &str) -> WasmActorMailbox<'_, R> {
        WasmActorMailbox::__new(
            <<R as Addressable>::Resolver as Resolve>::resolve(
                self.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE),
                namespace,
                (),
            )
            .0,
            self.mailbox,
            self.inline,
        )
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
}
