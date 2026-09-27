//! `aether.lifecycle` capability: the frame lifecycle the chassis drives one
//! step at a time (ADR-0082).
//!
//! The chassis owns cadence and sends
//! [`LifecycleAdvance`](aether_kinds::LifecycleAdvance) once per frame. This
//! capability owns everything else: the lifecycle graph
//! ([`LifecycleGraphData`] and its typestate builder, a graph of
//! `{ stage_kind, next, optional quit }` edges), the subscriber table keyed by
//! stage kind and its fan-out, the [`LifecycleConfig`] init config, and the
//! settlement gating. It is a singleton, so its namespace is reachable from
//! wasm: a component subscribes to a stage with
//! `ctx.subscribe::<LifecycleCapability, Render>()`.
//!
//! On each advance the capability:
//!
//! 1. Broadcasts the current state's signal to every subscriber registered for
//!    that stage kind. Stage kinds are empty ZSTs, so the broadcast carries no
//!    payload and is itself the signal; data a subscriber needs rides its own
//!    mail (the camera publishes `view_proj` to `aether.render`, for example).
//! 2. Subscribes the settlement registry on the broadcast's chain root and
//!    defers the state-pointer move until that chain settles, so cadence
//!    tracks real subscriber drain time. With no settlement registry wired (a
//!    registry-less test harness) it falls back to fire-and-advance.
//! 3. On settle, follows the resolved edge (`quit` when `quit_pending` is set
//!    and the state declares a quit edge, consuming the flag, otherwise
//!    `next`) and replies
//!    [`LifecycleAdvanceComplete`](aether_kinds::LifecycleAdvanceComplete) to
//!    the chassis loop that issued the advance.
//!
//! The stage kinds and the advance stay in `aether-kinds`: they are substrate
//! protocol vocabulary many actors address. The subscription request kinds
//! live in [`kinds`], beside the capability that answers them, because an
//! explicit subscriber is a `ProtocolPath` that `aether-kinds` cannot name.

#![forbid(unsafe_code)]
// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

use aether_actor::actor;

/// The stage kinds this capability publishes, each beside the field its
/// typed subscriber set is stored in: the one list the `Publishes` impls,
/// the [`LifecycleSubscription`] variants, and the runtime's typed sets and
/// stage dispatch are all written from, so a new stage is added here once.
///
/// `$emit` is a macro taking the list as `$($stage:ident $field:ident),+`.
/// The list is ADR-0082's stage vocabulary, which a chassis lifecycle graph
/// can declare as a state; `Quit` and `LifecycleAdvance` are absent because
/// they travel into the cap as signals, never out of it as a broadcast.
macro_rules! published_stages {
    ($emit:ident) => {
        $emit! {
            Tick tick,
            InitCaps init_caps,
            InitComponents init_components,
            Render render,
            Present present,
            Shutdown shutdown,
        }
    };
}

pub mod kinds;
pub use kinds::*;

mod graph;
// `LifecycleStateData` is named only by `mod settlement`'s `resolve_edge`,
// which rides the `runtime` gate, so the re-export does too.
#[cfg(feature = "runtime")]
use graph::LifecycleStateData;
pub use graph::{BuildError, LifecycleGraphBuilder, LifecycleGraphData, NoOpen, OpenNoNext, OpenWithNext};

mod subscribers;

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
/// `ctx.subscribe::<LifecycleCapability, K>()` without pulling the
/// substrate runtime. The state-bearing runtime (`LifecycleCapabilityState` in
/// `mod runtime`, which owns the data graph, subscriber table, fan-out,
/// and settlement gating) lives behind the one `feature = "runtime"`
/// gate; the chassis only feeds the cap [`LifecycleAdvance`](aether_kinds::LifecycleAdvance) cadence.
#[actor(singleton, root)]
pub struct LifecycleCapability;

// The runtime half — the whole `aether_substrate`-typed surface (imports,
// `LifecycleCapabilityState`, the settlement + fan-out names, the
// runtime-gated inspect impl, and the test fixtures) — lives in
// `runtime.rs`, gated once here.
#[cfg(feature = "runtime")]
mod runtime;
