//! Marker traits for the actor model. Pure compile-time markers — no
//! transport machinery, no lifecycle methods, just identity (`Actor`),
//! singleton-ness (`Singleton`), per-handler-kind gating
//! (`HandlesKind`), and the native dispatch entry-point (`Dispatch`).
//!
//! Pre-PR-C of issue 533 these lived here. Issue 533's facade pattern
//! (ADR-0075) put chassis cap structs in `aether-kinds`, which meant
//! both `aether-kinds` and `aether-actor` needed to reference the
//! markers — but `aether-actor` already depended on `aether-kinds` (for
//! `aether.control.subscribe_input`), so a forward dep would cycle.
//! PR C broke the cycle by moving the markers down to `aether-data`
//! (the universal data layer both crates depend on); marked stopgap.
//!
//! PR E1 of issue 545 collapsed the facade pattern back out of
//! `aether-kinds` — caps now live entirely in `aether-substrate`. The
//! cycle that forced the down-move evaporated, and PR E4 (this PR)
//! restores the markers to their natural home alongside the rest of
//! the actor SDK.

use aether_data::{Kind, ReplyTo};

/// The symmetric trait every actor implements: name + scheduling class.
/// Lifecycle methods (`boot` for native chassis caps, `init` for wasm
/// components) live on per-transport subtraits; this trait stays
/// ctx-free so the same shape applies to both sides.
pub trait Actor: Sized + Send + 'static {
    /// The recipient name this actor claims. For native capabilities
    /// it's the chassis-owned mailbox name (`aether.<name>`); for wasm
    /// components it's the default name `load_component` registers
    /// under when the load payload omits an explicit override.
    const NAMESPACE: &'static str;

    /// ADR-0074 §Decision 5 scheduling class. `true` means this actor
    /// participates in the per-frame drain barrier — the chassis frame
    /// loop waits for the dispatcher's inbox to quiesce before
    /// submitting the next render frame, so any mail a peer sent this
    /// frame is integrated before submit. Defaults to `false`
    /// (free-running). Today only `RenderCapability` overrides; future
    /// drawing-side capabilities and any wasm component that wants
    /// per-frame coupling will too.
    const FRAME_BARRIER: bool = false;
}

/// Marker: only one instance of this actor can be live per substrate.
/// Required by `Ctx::actor::<R>()` so the type → mailbox lookup is
/// unambiguous — the substrate enforces "at most one Singleton actor
/// per `R::NAMESPACE`" at registration time, and senders address by
/// type rather than by name.
///
/// Chassis caps (including catch-all caps like `BroadcastCapability`) are always
/// singletons. User components are singletons when their cdylib loads
/// at the default name (`R::NAMESPACE` from the wasm custom section);
/// multi-instance loads use `ctx.resolve_actor::<R>(name)` instead and
/// don't go through the singleton path. ADR-0075 §Decision 1.
pub trait Singleton: Actor {}

/// Per-handler-kind marker: `R: HandlesKind<K>` means actor `R` has a
/// `#[handler]` method accepting kind `K`. Auto-emitted by the
/// `#[actor]` proc-macro alongside the dispatch table — one impl per
/// handler kind. Authors never write these by hand.
///
/// Gates `ActorMailbox<'_, R, T>::send::<K>` (constructed via
/// `ctx.actor::<R>()` / `ctx.resolve_actor::<R>(name)`) so the compiler
/// rejects sends to a kind the receiver doesn't handle.
/// The single source of truth is the handler list on the actor's
/// `impl` block; adding a `#[handler]` updates senders' compile-time
/// checks automatically. ADR-0075 §Decision 1.
///
/// Blanket impls (e.g. `impl<T: Into<DrawTriangle>> HandlesKind<T> for
/// RenderCapability`) are an opt-in extension if a real conversion case
/// wants them; the default macro emission is strict so wire bytes stay
/// obvious.
pub trait HandlesKind<K: Kind>: Actor {}

/// Runtime dispatch entry-point. Auto-emitted by `#[actor]` on
/// native chassis-cap inherent impls. Routes a single mail (matched
/// by kind id) to the corresponding `#[handler]` method.
///
/// `sender` carries the envelope's reply target and correlation id
/// (issue 533 PR D1). `#[handler]` methods that need to reply declare
/// a 3-arg signature `(&mut self, sender: ReplyTo, mail: K)`; the
/// macro routes `sender` through to those handlers and ignores it
/// for 2-arg `(&mut self, mail: K)` handlers (fire-and-forget caps
/// like Log).
///
/// Returns `Some(())` on match + decode success, `None` on unknown
/// kind or decode failure. The chassis-side dispatcher logs misses
/// separately (kind-id + cap-namespace) so the strict-receiver
/// surface stays observable.
///
/// `sender` is `aether_data::ReplyTo` — the substrate-side dispatch
/// type. It's distinct from `aether_actor::ReplyTo`, the wasm-guest-
/// side opaque u32 handle in `mail.rs`. Both live next to where they
/// route mail, share a name because callers always reach for the one
/// matching their transport, and never appear in the same scope.
pub trait Dispatch {
    fn __dispatch(&mut self, sender: ReplyTo, kind: u64, payload: &[u8]) -> Option<()>;
}
