//! Per-handler and per-init contexts native actors receive when their
//! dispatcher trampoline fires. The trait + dispatch surface live in
//! the parent module (`super`); the cross-flavour `MonitorHandle` lives
//! in `crate::actor::monitor`.
//!
//! Issue 663 phase B added per-stage capability-trait impls
//! ([`MailSender`](aether_actor::MailSender), [`OutboundReply`](aether_actor::OutboundReply)) on [`NativeCtx`] /
//! [`NativeInitCtx`] alongside the existing inherent methods, so
//! user-facing handler bodies are now spelled in the same
//! cross-transport vocabulary FFI guests use. Substrate-internal
//! accessors (`mailer`, `publish_handle`, `transport_arc`, `self_id`,
//! plus the `spawn_child` builder) stay inherent — they expose
//! types that don't belong on a cross-transport trait
//! (`Arc<Mailer>`, `Arc<Spawner>`, the chassis [`ExportedHandles`] map,
//! the substrate-only `HandlerSpawnBuilder<'_, A>` whose
//! `A: NativeActor + NativeDispatch` bound can't sit on a trait
//! method declared in `aether-actor`). The inherent + trait surface
//! coexist; cap authors reach for whichever is in scope.
//!
//! ## Shape
//!
//! The ctx *type* lives here — its fields, the constructors each birth
//! path reaches for, the downgrade-only mode/actor coercions, and the
//! handler-end `Drop` flush. Everything a handler can *do* with one sits
//! in a sibling named for the question it answers: who this ctx is and
//! who it can address (`address`), what it is dispatching
//! (`inbound`), how it moves work off its own thread (`offload`),
//! how it stages a registry batch (`registry`) or a child birth
//! (`spawn`), how it retires itself and watches peers (`lifecycle`),
//! and how raw + typed mail leaves it (`send`). `init` holds the
//! boot-time [`NativeInitCtx`], and `handles` the chassis-owned
//! [`ExportedHandles`] map it publishes into.

use std::sync::Arc;

use aether_actor::{Manual, Multi, ReplyMode, Single};
use aether_data::{Kind, MailId};
use core::marker::PhantomData;
use core::ptr;

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::envelope::Envelope;
use crate::mail::Source;
use crate::mail::mailer::Mailer;
use crate::runtime::effect_chain::EffectChain;

mod address;
mod handles;
mod inbound;
mod init;
mod lifecycle;
mod offload;
mod registry;
mod send;
mod spawn;

#[cfg(test)]
mod tests;

pub use handles::ExportedHandles;
pub use init::NativeInitCtx;

/// Per-mail context for a [`NativeActor`](super::NativeActor) handler. Borrows the
/// actor's [`NativeBinding`] for outbound mail and carries the
/// inbound's reply target so the `OutboundReply::reply::<K>(&payload)` API
/// can route back to the originator without rethreading the handle.
///
/// Stage 1 ships the wiring; the actual reply routing through
/// [`NativeBinding::send_reply_for_handler`] / `Mailer::send_reply` is
/// the stage-2 migration's responsibility (today's caps reply via
/// `mailer.send_reply(...)` directly; stage 2 routes those onto
/// `ctx.reply(...)`).
pub struct NativeCtx<'a, M: ReplyMode = Single, A = Erased> {
    binding: &'a Arc<NativeBinding>,
    source: Source,
    /// ADR-0080 §5: identity of the mail this handler is dispatching.
    /// Outbound `send` paths read this to stamp `parent_mail` on
    /// child mail (so the receiver inherits the right parent in the
    /// causal graph). `MailId::NONE` for ctxs without an inbound
    /// (chassis-root sends, `unwire`, init).
    in_flight_mail_id: MailId,
    /// ADR-0080 §5: root of the causal chain this handler runs in.
    /// Outbound `send` paths read this to stamp `root` on child mail
    /// so descendants share the chain. `MailId::NONE` for ctxs without
    /// an inbound — those sends mint a fresh root from their own
    /// `mail_id` in `NativeBinding::send_mail`.
    in_flight_root: MailId,
    /// ADR-0168 §1: the chain of the work that *caused* this context to
    /// exist, for a context that dispatches no inbound of its own. A
    /// handler-staged birth threads the staging chain here so the newborn's
    /// `wire` hook can attach a birth-completing effect to it; every other
    /// ctx carries [`MailId::NONE`].
    ///
    /// Only [`Self::acquire_settlement_hold`] reads it. The outbound send
    /// lineage ([`Self::outbound_lineage`]) deliberately does not, which is
    /// the whole point: a `wire` ctx serves both the effects that complete
    /// the birth (which the causing chain must cover) and the actor's own
    /// startup sends (which it must not), and one *root* cannot tell those
    /// apart. Attaching the causing chain to the effect rather than to the
    /// context is what keeps the two separable.
    causing_chain: MailId,
    /// #1757 / ADR-0094: the single dispatched [`Envelope`], owned here
    /// for the duration of the handler. The dispatcher moves it in at
    /// construction ([`Self::with_inbound`]) and takes it back at its
    /// settlement tail ([`Self::take_raw_inbound`]) to record `Finished` +
    /// `discharge` exactly once — unless a handler first retained it via
    /// [`Self::take_inbound`], moving the obligation into an
    /// [`InboundMail`](crate::chassis::inbox::InboundMail) guard for a deferred reply. One envelope in one
    /// place: the detector for a missed settlement is `Option::is_some`,
    /// so a double-settle is structurally unrepresentable. `None` for the
    /// ctxs that dispatch nothing (init / close-hook / chassis-root /
    /// cap-test fixtures built through [`Self::new`]).
    inbound: Option<Envelope>,
    /// ADR-0112: phantom reply-mode marker (a ZST, layout-neutral) that
    /// selects which reply surface this ctx exposes. Defaults to
    /// [`Single`], so the common `NativeCtx<'_>` signature is unchanged.
    _mode: PhantomData<M>,
    /// Phantom marker naming the actor this ctx dispatches *for* — the
    /// parent any [`Self::spawn_child`] call places its child under. The
    /// dispatcher builds the typed form because it knows the actor;
    /// [`Self::erase`] downgrades to [`Erased`] for a handler that never
    /// spawns, which is why the default keeps the common `NativeCtx<'_>`
    /// signature unchanged.
    _actor: PhantomData<fn() -> A>,
}
/// The actor marker of a ctx that names no actor, and so cannot parent a
/// child (issue 4158).
///
/// [`NativeCtx::spawn_child`] lives only on the typed form, so the parent of
/// a staged birth is read off the ctx rather than declared beside it — a
/// caller has no way to name a parent the runtime will then contradict.
/// Every ctx built where no actor is in scope — `wire` / `unwire`, the
/// chassis root, a cap-side test fixture — is this form, and loses only a
/// call it could not have made correctly.
///
/// A type-position marker like [`Single`] / [`Manual`], never a value: it is
/// only ever the `A` of a `NativeCtx`, so it carries no impls of its own.
pub struct Erased;
impl<'a> NativeCtx<'a, Single> {
    /// Internal constructor — the chassis dispatcher trampoline (in
    /// `chassis::builder`) builds these for `wire` / `unwire` / close
    /// hooks. Cap-side test fixtures in the per-cap crates also reach
    /// for it directly so they can drive a handler method without
    /// spinning up a full chassis; that's why it's `pub` rather than
    /// `pub(crate)`.
    ///
    /// ADR-0112: stays `<Single>` so those ~hundred fixtures that call
    /// handler methods directly keep their single-mode ctx unchanged.
    /// Build a `<Manual>` ctx for driving the macro dispatch trampoline
    /// with [`Self::new_dispatching`].
    ///
    /// It also stays [`Erased`], for the same reason: a `wire` hook and the
    /// fixtures that call a handler directly name no actor, so nothing here
    /// could parent a child. [`Self::new_for_actor`] is the one that does.
    pub fn new(
        binding: &'a Arc<NativeBinding>,
        sender: Source,
        in_flight_mail_id: MailId,
        in_flight_root: MailId,
    ) -> Self {
        Self {
            binding,
            source: sender,
            in_flight_mail_id,
            in_flight_root,
            causing_chain: MailId::NONE,
            inbound: None,
            _mode: PhantomData,
            _actor: PhantomData,
        }
    }

    /// The `wire`-hook context, built by every birth path that runs
    /// `A::wire` (ADR-0079 amended). It dispatches no inbound, so it carries
    /// no in-flight lineage of its own.
    ///
    /// `chain` is the birth path's ADR-0168 §3 declaration of what orders the
    /// effects this hook stages. [`EffectChain::Held`] carries the chain of
    /// the work that caused the birth, so a birth-completing effect holds it
    /// and the staging caller's `Settled` covers it (ADR-0168 §1); the
    /// chainless arms name why no such chain exists. Making it an argument is
    /// what puts the question in front of the author of the next birth path
    /// rather than leaving it to be re-derived.
    ///
    /// The actor's own `wire`-time sends still mint their own roots — see the
    /// [`NativeCtx::causing_chain`] field docs for why the two must not share
    /// one root.
    pub(crate) fn for_wire(binding: &'a Arc<NativeBinding>, chain: EffectChain) -> Self {
        Self {
            binding,
            source: Source::NONE,
            in_flight_mail_id: MailId::NONE,
            in_flight_root: MailId::NONE,
            causing_chain: chain.held_root(),
            inbound: None,
            _mode: PhantomData,
            _actor: PhantomData,
        }
    }
}

impl<'a, M: ReplyMode, A> NativeCtx<'a, M, A> {
    /// The actor-naming counterpart of [`Self::new`] / [`Self::new_dispatching`]
    /// (issue 4158): the same inbound-less ctx, typed by the actor it
    /// dispatches for, so the handler it drives reaches [`Self::spawn_child`].
    /// The reply mode comes from the use site rather than from a second
    /// constructor.
    ///
    /// `binding` must be that actor's own binding — the birth lands under
    /// whatever identity the binding carries, and `A` is what the child's
    /// `ChildOf<A>` permission is checked against. The pumped host turn
    /// ([`PumpedSlot::host_turn`](super::slot::pumped::PumpedSlot::host_turn))
    /// is the production caller and derives both from the same slot; a
    /// cap-side fixture driving a spawning handler names its own actor here.
    pub fn new_for_actor(
        binding: &'a Arc<NativeBinding>,
        sender: Source,
        in_flight_mail_id: MailId,
        in_flight_root: MailId,
    ) -> Self {
        Self {
            binding,
            source: sender,
            in_flight_mail_id,
            in_flight_root,
            causing_chain: MailId::NONE,
            inbound: None,
            _mode: PhantomData,
            _actor: PhantomData,
        }
    }

    /// Issue 4158 downgrade-only coercion: view this ctx as one that names
    /// no actor, dropping [`Self::spawn_child`]. The `#[actor]` macro hands
    /// this view to every handler whose signature declares the plain
    /// `NativeCtx<'_, …>` form, so only a handler that asks for the typed
    /// ctx can parent a child. Like [`Self::as_single`] the coercion only
    /// removes capability — there is deliberately no way back up, because
    /// re-naming an actor is exactly the misstatement this replaced.
    #[doc(hidden)]
    #[must_use]
    pub fn erase(&mut self) -> &mut NativeCtx<'a, M, Erased> {
        // SAFETY: `A` appears only in `PhantomData`, so `NativeCtx<'a, M, A>`
        // and `NativeCtx<'a, M, Erased>` are layout-identical for every `A`
        // (see `native_ctx_layout_identical_across_modes`). The reborrow swaps
        // the marker without touching any real field.
        unsafe { &mut *ptr::from_mut(self).cast::<NativeCtx<'a, M, Erased>>() }
    }
}

impl<'a> NativeCtx<'a, Manual> {
    /// ADR-0112: an inbound-less `<Manual>` ctx for driving the
    /// macro-emitted `NativeDispatch::__aether_dispatch_envelope` (which
    /// carries the most-permissive view) from a cross-crate trampoline
    /// test. The `<Single>` [`Self::new`] backs fixtures that call handler
    /// methods directly; this one backs fixtures that route through the
    /// dispatch seam.
    pub fn new_dispatching(
        binding: &'a Arc<NativeBinding>,
        sender: Source,
        in_flight_mail_id: MailId,
        in_flight_root: MailId,
    ) -> Self {
        Self {
            binding,
            source: sender,
            in_flight_mail_id,
            in_flight_root,
            causing_chain: MailId::NONE,
            inbound: None,
            _mode: PhantomData,
            _actor: PhantomData,
        }
    }
}

impl<'a, A> NativeCtx<'a, Manual, A> {
    /// ADR-0112 downgrade-only coercion: view this [`Manual`] ctx as a
    /// [`Single`] ctx, dropping the `OutboundReply` surface. The
    /// `#[actor]` macro hands a single-class handler this view, so a
    /// handler whose marker disagrees with its class fails to unify.
    /// There is deliberately no `as_manual` — the runtime only ever
    /// downgrades.
    #[doc(hidden)]
    #[must_use]
    pub fn as_single(&mut self) -> &mut NativeCtx<'a, Single, A> {
        // SAFETY: `M` is `PhantomData`-only, so `NativeCtx<'a, Manual, A>` and
        // `NativeCtx<'a, Single, A>` are layout-identical (the marker field is
        // a ZST for every `M` — see `native_ctx_layout_identical_across_modes`).
        // The reborrow swaps the marker without touching any real field and
        // only removes capability, never adds it.
        unsafe { &mut *ptr::from_mut(self).cast::<NativeCtx<'a, Single, A>>() }
    }

    /// ADR-0134 downgrade-only coercion: view this [`Manual`] ctx as a
    /// [`Multi<K>`] ctx, swapping the `OutboundReply` surface for the
    /// [`Emit<K>`](aether_actor::Emit) surface. The `#[actor]` macro hands a `#[handler::multi]`
    /// handler this view (with `K` read off its `Multi<K>` signature), so a
    /// handler whose marker disagrees with its class fails to unify.
    #[doc(hidden)]
    #[must_use]
    pub fn as_multi<K: Kind>(&mut self) -> &mut NativeCtx<'a, Multi<K>, A> {
        // SAFETY: `M` is `PhantomData`-only and `Multi<K>` is a ZST for every
        // `K`, so `NativeCtx<'a, Manual, A>` and `NativeCtx<'a, Multi<K>, A>`
        // are layout-identical (see `native_ctx_layout_identical_across_modes`).
        // The reborrow swaps the marker without touching any real field.
        unsafe { &mut *ptr::from_mut(self).cast::<NativeCtx<'a, Multi<K>, A>>() }
    }

    /// #1757: the per-dispatch constructor — moves the single dispatched
    /// [`Envelope`] into the ctx so a handler can retain it via
    /// [`Self::take_inbound`] (and so the dispatcher's settlement tail
    /// settles exactly one owner). Only the native dispatcher
    /// ([`DispatcherSlot::dispatch_one`](crate::actor::native::slot::dispatcher))
    /// builds these; the inbound-less [`Self::new`] backs the
    /// close-hook / chassis-root / cap-test ctxs that dispatch nothing.
    pub(crate) fn with_inbound(
        binding: &'a Arc<NativeBinding>,
        sender: Source,
        in_flight_mail_id: MailId,
        in_flight_root: MailId,
        inbound: Envelope,
    ) -> Self {
        Self {
            binding,
            source: sender,
            in_flight_mail_id,
            in_flight_root,
            causing_chain: MailId::NONE,
            inbound: Some(inbound),
            _mode: PhantomData,
            _actor: PhantomData,
        }
    }
}

impl<M: ReplyMode, A> NativeCtx<'_, M, A> {
    /// Borrow the wired `Mailer`. Issue 953: surfaced so cap handlers
    /// (`TraceDispatchCapability` is the motivating consumer) can
    /// reach the per-chassis trace handle for `now_nanos` without
    /// going through `binding()`. Mirrors the `NativeInitCtx::mailer`
    /// accessor but returns a borrow rather than a clone — handler
    /// paths usually just need a `&Mailer` for one call.
    #[must_use]
    pub fn mailer(&self) -> &Arc<Mailer> {
        self.binding.mailer()
    }

    /// Clone this actor's transport. Runtime adapters that rebuild an
    /// embedded execution context during a handler (the Wasm trampoline's
    /// replacement path) use the same binding as the actor they remain behind.
    #[doc(hidden)]
    #[must_use]
    pub fn transport_arc(&self) -> Arc<NativeBinding> {
        Arc::clone(self.binding)
    }
}

impl<M: ReplyMode, A> Drop for NativeCtx<'_, M, A> {
    /// ADR-0087 / 2b (iamacoffeepot/aether#1105): handler-end flush. One
    /// `NativeCtx` is built per dispatched envelope (and one for
    /// `unwire`), so its scope *is* the handler's lifetime — dropping it
    /// is the universal "handler finished" hook. Flushing the binding's
    /// outbound buffer here forms the handler's buffered sends into one
    /// ring blob and routes them, covering the main dispatch loop, the
    /// shutdown-drain loop, and `unwire` with a single hook (no
    /// per-call-site flush to forget and silently drop mail).
    /// Idempotent — an empty buffer no-ops.
    fn drop(&mut self) {
        self.binding.flush_outbound();
    }
}
