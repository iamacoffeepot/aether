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
//!   external runtimes (drivers, `SubstrateHarness`, MCP) reach for
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

// The vocabulary — what a native actor *is* and what it holds — sits flat
// here; the machinery around it nests one level down, a directory per concept:
// how an actor is born (`spawn`), where it drains (`slot`), how it moves work
// off its own thread (`offload`), and how its outbound mail fans out (`blob`).
pub mod binding;
pub mod ctx;
pub mod envelope;
pub(crate) mod identity;
pub mod local;
pub mod mailbox;

pub(crate) mod blob;
pub mod offload;
pub mod slot;
pub mod spawn;

pub use crate::mail::registry::effect::{RegistryBatch, RegistryBatchError, RegistryBatchResult};
pub use binding::NativeBinding;
pub use ctx::{Erased, ExportedHandles, NativeCtx, NativeInitCtx};
pub use envelope::Envelope;
pub use mailbox::{NativeActorMailbox, NativeActorMailboxWithContext};
pub use offload::blocking::{DeferredReply, DispatchId, IntoDeferredReply, Pending, TaskCompletionWake, TaskDone};
pub use offload::thread::{InheritCtx, RootCtx};
pub use slot::pumped::PumpedSlot;
pub use spawn::{HandlerSpawnBuilder, SpawnBuilder, SpawnError, SpawnOutcome, SpawnReceipt, Spawner, Subname};
// iamacoffeepot/aether#3707: the cap-level rate-limit/queue helper over the
// ADR-0093 `offload::blocking` primitive it wraps — a substrate-tier native
// helper, used by the content-gen provider caps and the rpc test-echo actor.
pub use offload::task_queue::{DEFAULT_MAX_IN_FLIGHT, TaskQueue};

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
    // to `Single` per single-class handler. Issue 4158: it is also typed by
    // `Self`, the actor being dispatched, so a handler that opts into the typed
    // form parents its children under the actor the runtime is actually
    // running; each arm `erase()`s for a handler whose signature names no actor.
    /// Route one inbound envelope to the matching `#[handler]` over the state.
    /// `Some(())` on a handled kind + decode success, `None` otherwise.
    fn dispatch(
        state: &mut S,
        ctx: &mut NativeCtx<'_, crate::Manual, Self>,
        kind: KindId,
        payload: &[u8],
    ) -> Option<()>
    where
        Self: Sized;

    /// Catch-all for envelopes no `#[handler]` matched (issue 576). Default
    /// returns `false` so the trampoline warn-logs the miss; the macro
    /// overrides it when a `#[fallback]` is present.
    fn dispatch_fallback(_state: &mut S, _ctx: &mut NativeCtx<'_, crate::Manual, Self>, _envelope: &Envelope) -> bool
    where
        Self: Sized,
    {
        false
    }

    /// Every kind this actor dispatches through its typed table — the set the
    /// per-handler cost table is seeded from (iamacoffeepot/aether#4266).
    ///
    /// Distinct from [`Self::capabilities`], which is the ADR-0033 *advertised*
    /// receive surface `describe_component` reports. The two coincided until a
    /// `#[handler(task)]` made them differ: an ADR-0093 completion arrives as
    /// [`TaskCompletionWake`], which the typed table dispatches but which is
    /// internal plumbing between an actor and its own offloaded work, not
    /// something a caller can address. Advertising it would be misleading;
    /// leaving it unmeasured means the handler owns no `CostCell`, so its
    /// execution time never folds, `actor_cost` reports no row, and the
    /// ADR-0087 cost-aware recruiter cannot see the work.
    ///
    /// Defaults to the advertised handlers' ids, which is correct for any actor
    /// whose typed arms are all declared. The `#[actor]` macro overrides it to
    /// append `TaskCompletionWake` when the actor has task handlers.
    #[must_use]
    fn measured_kinds() -> Vec<KindId>
    where
        Self: Sized,
    {
        Self::capabilities().handlers.iter().map(|handler| handler.id).collect()
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
    + for<'a> Lifecycle<Self::State, InitError = BootError, InitCtx<'a> = NativeInitCtx<'a>, Ctx<'a> = NativeCtx<'a>>
    + Dispatch<Self::State>
{
    /// The runtime state this identity boots into — **plain data**, bounded
    /// only by `Send + 'static`.
    type State: Send + 'static;
}
