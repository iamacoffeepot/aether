//! Issue 552 stage 1: native chassis-cap actor surface.
//!
//! The native counterpart of `aether_actor::FfiActor`. Stage 1
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
//! is retired. [`NativeDispatch`] takes `&mut self`; `#[handler]`
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
//! and overrides [`NativeDispatch::__aether_dispatch_fallback`] to
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
/// [`NativeDispatch::__aether_capabilities`] override through
/// `::aether_substrate::` paths — the same crate the rest of the
/// native dispatch impl already resolves against, so native `#[actor]`
/// consumers don't need `aether-kinds` in their own dep list
/// (iamacoffeepot/aether#1037).
pub use aether_kinds::{ComponentCapabilities, FallbackCapability, HandlerCapability};

/// Native chassis-cap actor trait. Per-cap shape: one struct, one
/// `#[actor] impl NativeActor for X` block. The boot lifecycle
/// (`init` / `wire` / `unwire`, plus `type Config`) lives on the shared
/// [`Lifecycle`] capability; `NativeActor`
/// composes it alongside the identity [`Addressable`] supertrait and pins
/// `InitError` to the chassis [`BootError`]. Native config stays a live
/// Rust value (e.g. `AudioConfig`), so unlike the FFI side it carries no
/// `Kind` bound. The `#[actor]` macro synthesizes the per-target ctx GATs
/// (`NativeInitCtx` / `NativeCtx`) into the generated `impl Lifecycle`.
///
/// Issue 629 / Phase A: the `Addressable` supertrait gives `Send + 'static`.
/// The dispatcher thread owns the cap as `Box<Self>` for its lifetime —
/// no cross-thread `Arc` share, no `Sync` bound. Cap state can be plain
/// fields without interior-mutability gymnastics once Phase B sweeps each
/// cap. Per-kind dispatch wiring lives on the sibling [`NativeDispatch`]
/// trait, also macro-emitted.
pub trait NativeActor:
    Addressable
    + for<'a> Lifecycle<
        InitError = BootError,
        InitCtx<'a> = NativeInitCtx<'a>,
        Ctx<'a> = NativeCtx<'a>,
    >
{
}

/// Sum dispatch entry-point — emitted once per `#[actor] impl
/// NativeActor for X` block. Takes the inbound mail's `(kind, payload)`
/// pair, routes by kind id to the right `#[handler]` method, and
/// returns `Some(())` on match + decode success or `None` on unknown
/// kind / decode failure.
///
/// Issue 629 / Phase A: `&mut self` since the dispatcher thread owns
/// the cap as `Box<Self>` and is the sole entry-point. `: Send +
/// 'static` (no `Sync`) — the cap is not shared across threads.
///
/// Per-handler-kind compile checks come from
/// [`aether_actor::HandlesKind`] (one impl per handler the macro
/// emits); a future per-K `NativeDispatch<K>` may layer on top if a
/// caller wants a typed `dispatch_kind::<K>` entry, but Phase A
/// doesn't need it.
pub trait NativeDispatch: Send + 'static {
    // ADR-0112: the dispatch seam carries the most-permissive `Manual`
    // ctx so a `#[handler::manual]` arm reaches the reply surface; the
    // macro downgrades to `Single` per single-class handler. Every impl
    // (macro-emitted and hand-written test fixtures) spells `Manual` here.
    fn __aether_dispatch_envelope(
        &mut self,
        ctx: &mut NativeCtx<'_, crate::Manual>,
        kind: KindId,
        payload: &[u8],
    ) -> Option<()>;

    /// Catch-all fallback for envelopes whose kind doesn't match any
    /// `#[handler]` (issue 576). Default returns `false` — the
    /// chassis trampoline warn-logs the unknown-kind miss as today.
    /// The `#[actor]` macro overrides this when the impl carries a
    /// `#[fallback]` method, returning `true` after the user's
    /// fallback runs so the trampoline knows to suppress the warn
    /// log.
    fn __aether_dispatch_fallback(
        &mut self,
        _ctx: &mut NativeCtx<'_, crate::Manual>,
        _envelope: &Envelope,
    ) -> bool {
        false
    }

    /// The native cap's ADR-0033 receive-side capability surface —
    /// every `#[handler]` kind plus `#[fallback]` presence
    /// (iamacoffeepot/aether#1037). The `#[actor] impl NativeActor`
    /// macro overrides this to enumerate the cap's handlers + fallback,
    /// the always-on native counterpart of a wasm component's
    /// `aether.kinds.inputs` manifest. The native-cap-boot path reads
    /// it to populate the [`CapabilityRegistry`](crate::mail::CapabilityRegistry),
    /// so a native cap (e.g.
    /// `aether.fs`) is queryable for dispatchability just like a loaded
    /// wasm component. Default is an empty surface — only the
    /// (`name` / `doc`-dropping) handler ids + fallback flag are
    /// load-bearing here; reply kinds are deliberately absent (handlers
    /// promise nothing about replies).
    #[must_use]
    fn __aether_capabilities() -> ComponentCapabilities
    where
        Self: Sized,
    {
        ComponentCapabilities::default()
    }
}
