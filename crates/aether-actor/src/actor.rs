//! ADR-0074 §Decision: actors are the unified primitive that
//! components and capabilities collapse onto. Issue 525 Phase 3 lifts
//! the symmetric bits — `NAMESPACE` and `FRAME_BARRIER` — onto a
//! transport-agnostic super-trait here so the substrate-side
//! `NativeActor` and the wasm-side `WasmActor` (Phase 4) can both
//! extend it.
//!
//! What's NOT here: lifecycle (`init` / `boot` / `Drop`). Lifecycle
//! signatures need transport-specific ctx types
//! (`ChassisCtx<'_>` vs `InitCtx<'_, WasmTransport>`) and one of them
//! is fallible while the other (currently) is infallible — keeping
//! them on the lifecycle subtraits keeps `Actor` itself ctx-free.

/// The symmetric trait that every actor implements: name + scheduling
/// class. Lifecycle methods (`boot` for native, `init` for wasm) live
/// on the per-transport subtraits (`NativeActor`, `WasmActor`).
///
/// Pre-issue-525-Phase-3 these consts lived directly on
/// `aether_substrate::Capability` (post-issue-525-Phase-3
/// `NativeActor`); the lift makes them accessible to the wasm-side
/// `Component` trait too without each side re-declaring the same
/// surface. `aether-component::Component` still declares its own
/// `NAMESPACE` independently today (Phase 1B); Phase 4 folds
/// `Component` onto `WasmActor: Actor` so the const is sourced from
/// the same trait both sides.
pub trait Actor: Sized + Send + 'static {
    /// The recipient name this actor claims. For native capabilities
    /// it's the chassis-owned mailbox name (`aether.<name>`); for wasm
    /// components it's the default name `load_component` registers
    /// under when the load payload omits an explicit override.
    const NAMESPACE: &'static str;

    /// ADR-0074 §Decision 5 scheduling class. `true` means this actor
    /// participates in the per-frame drain barrier — the chassis
    /// frame loop waits for the dispatcher's inbox to quiesce before
    /// submitting the next render frame, so any mail a peer sent
    /// this frame is integrated before submit. Defaults to `false`
    /// (free-running). Today only `RenderCapability` overrides;
    /// future drawing-side capabilities and any wasm component that
    /// wants per-frame coupling will too.
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

/// Per-handler-kind marker: `R: HandlesKind<K>` means actor `R` has
/// a `#[handler]` method accepting kind `K`. Auto-emitted by the
/// `#[actor]` proc-macro alongside the dispatch table — one impl
/// per handler kind. Authors never write these by hand.
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
pub trait HandlesKind<K: aether_data::Kind>: Actor {}
