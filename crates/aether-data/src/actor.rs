//! Marker traits for the actor model. Pure compile-time markers — no
//! transport machinery, no lifecycle methods, just identity (`Actor`),
//! singleton-ness (`Singleton`), and per-handler-kind gating
//! (`HandlesKind`).
//!
//! These live in `aether-data` (the universal data layer, ADR-0069)
//! rather than in `aether-actor` (the SDK) because both `aether-kinds`
//! (chassis cap facades, ADR-0075 §Decision 3) and `aether-actor`
//! (the transport-aware machinery) need to reference them. Putting
//! them in `aether-actor` would force `aether-kinds` to depend on
//! `aether-actor`, but `aether-actor` already depends on `aether-kinds`
//! for the `aether.control.subscribe_input` kind — that's a cycle.
//!
//! `aether-actor` re-exports these (`pub use aether_data::Actor`) so
//! existing SDK consumers see no API change.

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
/// Required by `Ctx::send_to::<R>` so the type → mailbox lookup is
/// unambiguous — the substrate enforces "at most one Singleton actor
/// per `R::NAMESPACE`" at registration time, and senders address by
/// type rather than by name.
///
/// Chassis caps and synthetic actors like `HubBroadcast` are always
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
/// Gates `Ctx::send_to::<R>(&K)` and `ActorMailbox<R, T>::send::<K>` so
/// the compiler rejects sends to a kind the receiver doesn't handle.
/// The single source of truth is the handler list on the actor's
/// `impl` block; adding a `#[handler]` updates senders' compile-time
/// checks automatically. ADR-0075 §Decision 1.
///
/// Blanket impls (e.g. `impl<T: Into<DrawTriangle>> HandlesKind<T> for
/// RenderCapability`) are an opt-in extension if a real conversion case
/// wants them; the default macro emission is strict so wire bytes stay
/// obvious.
pub trait HandlesKind<K: crate::Kind>: Actor {}

/// Runtime dispatch entry-point. Auto-emitted by `#[actor]` on
/// native chassis-cap inherent impls. Routes a single mail (matched
/// by kind id) to the corresponding `#[handler]` method.
///
/// Lives here (not in `aether-actor`) so chassis-cap facades in
/// `aether-kinds` can implement it without picking up an `aether-actor`
/// dependency — see [`Actor`] for the cycle context.
///
/// Returns `Some(())` on match + decode success, `None` on unknown
/// kind or decode failure. The chassis-side dispatcher logs misses
/// separately (kind-id + cap-namespace) so the strict-receiver
/// surface stays observable.
pub trait Dispatch {
    fn __dispatch(&mut self, kind: u64, payload: &[u8]) -> Option<()>;
}
