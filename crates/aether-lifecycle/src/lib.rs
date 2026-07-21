//! `aether.lifecycle` cap (ADR-0082). The non-generic capability the
//! chassis drives one frame at a time.
//!
//! The chassis owns cadence: it sends [`LifecycleAdvance`] once per
//! frame. The cap owns everything else — the lifecycle graph (a data
//! graph of `{ stage_kind, next, optional quit }` edges, in
//! `mod graph`), the subscriber table keyed by stage kind and
//! the fan-out (the sender side + `broadcast_to_subscribers` in
//! `mod subscribers`), and the settlement gating (the
//! advance state machine in `mod settlement`). Because it
//! is `#[actor(singleton)]`d like
//! `InputCapability` and `RenderCapability`, its
//! `NAMESPACE` is wasm-reachable: a component subscribes a stage via
//! `ctx.actor::<LifecycleCapability>().subscribe::<Render>()`.
//!
//! On each [`LifecycleAdvance`] the cap:
//!
//! 1. Broadcasts the current state's signal to every subscriber
//!    registered for that stage kind. Stage kinds are empty ZSTs, so
//!    the payload is empty — the broadcast *is* the signal; any data a
//!    subscriber needs rides its own mail (e.g. the camera publishes
//!    `view_proj` to `aether.render`).
//! 2. Subscribes the settlement registry on the broadcast's chain
//!    root and defers the state-pointer mutation to [`Settled`]
//!    (ADR-0082 §6) — so cadence couples to actual subscriber drain
//!    time. When no settlement registry is wired (a registry-less test
//!    harness) it falls back to fire-and-advance.
//! 3. On settle, advances the resolved edge — `quit` if `quit_pending`
//!    is set and the state declares a quit edge (consuming the flag),
//!    otherwise `next` — and replies
//!    [`LifecycleAdvanceComplete`](aether_kinds::LifecycleAdvanceComplete)
//!    to the chassis loop that issued the advance.
//!
//! Extracted by the arc that dissolved the capabilities monolith
//! (iamacoffeepot/aether#3749) as a leaf per-cap crate. Owns the lifecycle graph ([`LifecycleGraphData`] + its
//! typestate builder), the [`LifecycleConfig`] init config, the
//! [`LifecycleCapability`] identity + its subscriber-table / settlement
//! runtime (`runtime`), and the send-side [`LifecycleMailboxExt`] facade.
//! It is a pure leaf — no other capability depends on it, so capabilities
//! keeps no `aether-lifecycle` dependency (no facade).
//!
//! The `aether.lifecycle.*` mail kinds ([`LifecycleAdvance`], the
//! subscribe family, the stage-signal ZSTs) stay in `aether-kinds`: they
//! are substrate protocol vocabulary many actors address rather than a
//! cap-internal detail, so this crate only references them.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

use aether_kinds::trace::Settled;
// `MonitorNotice` rides the handled-kind list like the subscribe family:
// the `#[actor]` macro emits its always-on `HandlesKind` marker for the
// runtime half's ADR-0079 vacate/close purge handler.
use aether_kinds::{
    LifecycleAdvance, LifecycleSubscribe, LifecycleSubscribeSelf, LifecycleUnsubscribe, LifecycleUnsubscribeAll,
    LifecycleUnsubscribeSelf, MonitorNotice, Quit,
};
// `LifecycleSubscribeResult` rides the native gate (not `runtime`): the
// `#[actor]` macro's ADR-0109 `HandlerEntry` inventory submission —
// emitted on every native build, runtime or not — names the subscribe
// handlers' reply kind `::ID`, so a transport-only build must see it.
// `LifecycleAdvanceComplete` is the reply of the two `#[handler::manual]`
// arms, which declare no manifest reply kind, so it is named only by the
// runtime handler bodies and lives in `mod runtime` behind the `runtime`
// gate.
#[cfg(not(target_family = "wasm"))]
use aether_kinds::LifecycleSubscribeResult;

use aether_actor::actor;

mod graph;
// `LifecycleStateData` is named only by `mod settlement`'s `resolve_edge`,
// which rides the `runtime` gate, so the re-export does too.
#[cfg(feature = "runtime")]
use graph::LifecycleStateData;
pub use graph::{BuildError, LifecycleGraphBuilder, LifecycleGraphData, NoOpen, OpenNoNext, OpenWithNext};

mod subscribers;
pub use subscribers::LifecycleMailboxExt;

// The settlement state machine and the boot-config both name the
// runtime-only `LifecycleCapabilityState`, so both live under the `runtime`
// directory beside the rest of the runtime half, covered by the one
// `mod runtime;` gate. `LifecycleConfig` configures that runtime state, so its
// re-export sources through `runtime` rather than a per-import gate here.
#[cfg(feature = "runtime")]
pub use runtime::{LifecycleConfig, LifecycleConfigLayer, LifecycleOverlay, LifecycleParams, frame_lifecycle_params};

/// The `aether.lifecycle` cap **identity** (ADR-0122 identity/runtime
/// split, ADR-0082). A ZST carrying only the addressing — the
/// `Addressable` / `HandlesKind` markers and the name-inventory entry,
/// all emitted always-on by `#[actor]` — so a wasm guest names it via
/// `ctx.actor::<LifecycleCapability>()` without pulling the substrate
/// runtime. The state-bearing runtime (`LifecycleCapabilityState` in
/// `mod runtime`, which owns the data graph, subscriber table, fan-out,
/// and settlement gating) lives behind the one `feature = "runtime"`
/// gate; the chassis only feeds the cap [`LifecycleAdvance`] cadence.
#[actor(singleton)]
pub struct LifecycleCapability;

// The runtime half — the whole `aether_substrate`-typed surface (imports,
// `LifecycleCapabilityState`, the settlement + fan-out names, the
// runtime-gated inspect impl, and the test fixtures) — lives in
// `runtime.rs`, gated once here.
#[cfg(feature = "runtime")]
mod runtime;
