//! Issue 552 stage 1: native chassis-cap actor surface.
//!
//! The native counterpart of `aether_actor::WasmActor`. Stage 1
//! introduces the type-level vocabulary; Stage 2 migrated the
//! existing capabilities (Log, Handle, Io, Net, Audio, Render) onto
//! it. Stage 1's deliverable was the trait + ctx + dispatch
//! infrastructure plus a working boot path through
//! [`crate::chassis::builder::Builder::with_actor`]. The legacy
//! `with(cap)` / `Actor + Dispatch` facade path retired in issue 688
//! once every cap migrated to `with_actor`.
//!
//! ## Shape
//!
//! ```text
//! #[capability]
//! #[derive(Singleton)]
//! pub struct ExampleCap { /* plain fields — single-threaded ownership */ }
//!
//! #[actor]
//! impl NativeActor for ExampleCap {
//!     type Config = ();
//!     const NAMESPACE: &'static str = "aether.example";
//!
//!     fn init(_: (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> { … }
//!
//!     #[handler] fn on_hello(&self, ctx: &mut NativeCtx<'_>, mail: Hello) { … }
//! }
//! ```
//!
//! Issue 629 / Phase A: actors are owned by their dispatcher thread
//! as `Box<A>` — the cross-thread `Arc<dyn Any + Send + Sync>` storage
//! is retired. [`Dispatch`] takes `state: &mut S`; `#[handler]`
//! methods can take either `&self` or `&mut self` (Phase B sweeps caps
//! to `&mut self` cap by cap as state migrates off interior mutability).
//!
//! Cross-thread access from drivers / embedders flows through
//! cap-exported sub-handles published in `init` via
//! [`NativeInitCtx::publish_handle`] and retrieved via
//! [`crate::DriverCtx::handle`]. The actor itself never escapes its
//! dispatcher thread.
//!
//! ## What does NOT live here
//!
//! - `actor::<A>()` lookups on per-handler ctx. Once dispatchers are
//!   running, caps and components communicate via mail — peering at
//!   sibling state recreates the shared-state coupling the actor
//!   model is designed to eliminate. The chassis-level
//!   `chassis.actor::<X>() -> Arc<X>` retired with issue 629 / Phase A;
//!   external runtimes (drivers, `TestBench`, MCP) reach for
//!   cap-exported handles instead.
//!
//! ## Catch-all caps (issue 576)
//!
//! Caps that fan-out every kind they're addressed at — broadcast
//! today, hub-as-actor in the future — author with a `#[fallback]`
//! method instead of `#[handler]`s. The macro emits a blanket
//! `impl<K: Kind> HandlesKind<K> for X {}` so typed sends like
//! `ctx.actor::<BroadcastCapability>().send(&payload)` compile for every K,
//! and overrides [`Dispatch::dispatch_fallback`] to
//! route every envelope through the user's fallback method. Hybrid
//! shape (typed handlers + fallback as a runtime safety net) is
//! rejected by the macro: strict receivers shouldn't silently swallow
//! unknown kinds.

pub mod binding;
pub(crate) mod blob_lifecycle;
pub(crate) mod blob_work;
pub mod ctx;
pub(crate) mod dispatch;
pub mod dispatch_blocking;
pub(crate) mod dispatcher_slot;
pub mod envelope;
pub mod local;
pub mod mailbox;
pub mod spawn;
pub mod spawn_thread;

pub use binding::NativeBinding;
pub use ctx::{ExportedHandles, NativeCtx, NativeInitCtx};
pub use dispatch_blocking::{DispatchId, Pending, TaskCompletionWake, TaskDone};
// iamacoffeepot/aether#1272: driver-as-actor capabilities that own
// their inbox drain inline (today only the desktop window driver)
// reach for the `NativeCtx`-free variants of the framework dispatch
// arms so `actor_logs aether.window` reaches the log/trace/cost rings
// the same way every standard-dispatcher-slot actor does.
pub use dispatch::{
    dispatch_cost_tail_if_matching_free, dispatch_log_tail_if_matching_free,
    dispatch_trace_tail_if_matching_free,
};
pub use envelope::Envelope;
pub use mailbox::NativeActorMailbox;
pub use spawn::{SpawnBuilder, SpawnError, Spawner, Subname};
pub use spawn_thread::{InheritCtx, RootCtx};

use aether_actor::{Addressable, Lifecycle};

use crate::chassis::error::BootError;
use crate::mail::KindId;

/// Re-export of the ADR-0033 capability vocabulary so the
/// `#[actor] impl NativeActor` macro can construct the
/// [`Dispatch::capabilities`] override through
/// `::aether_substrate::` paths — the same crate the rest of the
/// native dispatch impl already resolves against, so native `#[actor]`
/// consumers don't need `aether-kinds` in their own dep list
/// (iamacoffeepot/aether#1037).
pub use aether_kinds::{ComponentCapabilities, FallbackCapability, HandlerCapability};

/// Per-kind dispatch over a runtime state `S` (iamacoffeepot/aether#2311 —
/// the reshaped native dispatch trait, now generic over the state rather than
/// taking `&mut self`). The `#[actor]` macro implements it on the addressing
/// identity, `impl Dispatch<State> for Identity`, emitting the sum dispatch
/// table; for an un-split cap `S = Self`, so `&mut S == &mut self`. Native-only
/// (the wasm counterpart is [`aether_actor::WasmDispatch`]).
pub trait Dispatch<S> {
    // ADR-0112: the dispatch seam carries the most-permissive `Manual` ctx so a
    // `#[handler::manual]` arm reaches the reply surface; the macro downgrades
    // to `Single` per single-class handler.
    /// Route one inbound envelope to the matching `#[handler]` over the state.
    /// `Some(())` on a handled kind + decode success, `None` otherwise.
    fn dispatch(
        state: &mut S,
        ctx: &mut NativeCtx<'_, crate::Manual>,
        kind: KindId,
        payload: &[u8],
    ) -> Option<()>;

    /// Catch-all for envelopes no `#[handler]` matched (issue 576). Default
    /// returns `false` so the trampoline warn-logs the miss; the macro
    /// overrides it when a `#[fallback]` is present.
    fn dispatch_fallback(
        _state: &mut S,
        _ctx: &mut NativeCtx<'_, crate::Manual>,
        _envelope: &Envelope,
    ) -> bool {
        false
    }

    /// The native cap's ADR-0033 receive-side capability surface — every
    /// `#[handler]` kind plus `#[fallback]` presence (iamacoffeepot/aether#1037).
    /// Static — independent of any state instance. The `#[actor]` macro
    /// overrides this to enumerate the cap's handlers + fallback, the always-on
    /// native counterpart of a wasm component's `aether.kinds.inputs` manifest.
    /// The native-cap-boot path reads it to populate the
    /// [`CapabilityRegistry`](crate::mail::CapabilityRegistry), so a native cap
    /// (e.g. `aether.fs`) is queryable for dispatchability just like a loaded
    /// wasm component. Default is an empty surface.
    #[must_use]
    fn capabilities() -> ComponentCapabilities
    where
        Self: Sized,
    {
        ComponentCapabilities::default()
    }
}

/// Native chassis-cap actor trait (iamacoffeepot/aether#2311 — identity/runtime
/// split, composed shape). One **identity** type owns the addressing
/// ([`Addressable`]) and composes the boot lifecycle and per-kind dispatch
/// parameterised by its runtime [`State`](NativeActor::State): the shared
/// [`Lifecycle<Self::State>`](Lifecycle) (`InitError` pinned to
/// the chassis [`BootError`], the ctx GATs to `NativeInitCtx` / `NativeCtx`) and
/// the native [`Dispatch<Self::State>`](Dispatch). The state is **plain data**
/// — bounded only by `Send + 'static`, it implements no behaviour trait.
///
/// `State` defaults to `Self` for every un-split cap (the identity IS its own
/// runtime, so `&mut Self::State == &mut self`); the default is supplied by the
/// `#[actor]` macro (`type State = Self;`), since associated-type defaults are
/// unstable on the 2024 edition. A cap that separates addressing from runtime
/// points `State` at a dedicated plain `struct` in a `feature = "runtime"`-gated
/// module. Native config stays a live Rust value (e.g. `AudioConfig`), so unlike
/// the FFI side it carries no `Kind` bound.
///
/// The dispatcher owns the actor as `Box<Self::State>` and drives it through the
/// composed traits: `<A as Lifecycle<_>>::init` / `<A as Dispatch<_>>::dispatch`.
pub trait NativeActor:
    Addressable
    + for<'a> Lifecycle<
        Self::State,
        InitError = BootError,
        InitCtx<'a> = NativeInitCtx<'a>,
        Ctx<'a> = NativeCtx<'a>,
    > + Dispatch<Self::State>
{
    /// The runtime state this identity boots into — **plain data**, bounded
    /// only by `Send + 'static`.
    type State: Send + 'static;
}
