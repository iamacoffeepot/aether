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
//! [`InputCapability`](crate::input::InputCapability) and
//! `RenderCapability`, its
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
//!    otherwise `next` — and replies [`LifecycleAdvanceComplete`] to
//!    the chassis loop that issued the advance.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

use aether_kinds::trace::Settled;
use aether_kinds::{
    LifecycleAdvance, LifecycleSubscribe, LifecycleSubscribeSelf, LifecycleUnsubscribe,
    LifecycleUnsubscribeAll, LifecycleUnsubscribeSelf, Quit,
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
pub(in crate::lifecycle) use graph::LifecycleStateData;
pub use graph::{
    BuildError, LifecycleGraphBuilder, LifecycleGraphData, NoOpen, OpenNoNext, OpenWithNext,
};

mod subscribers;
pub use subscribers::LifecycleMailboxExt;

// The settlement state machine and the boot-config both name the
// runtime-only `LifecycleCapabilityState`, so both live under the `runtime`
// directory beside the rest of the runtime half, covered by the one
// `mod runtime;` gate. `LifecycleConfig` configures that runtime state, so its
// re-export sources through `runtime` rather than a per-import gate here.
#[cfg(feature = "runtime")]
pub use runtime::LifecycleConfig;

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
// `LifecycleCapabilityState`, the settlement + fan-out names) — lives in
// `runtime.rs`, gated once here. The `#[actor] impl` and the state's
// inherent-method cluster reach it through the `use runtime::*` glob.
#[cfg(feature = "runtime")]
mod runtime;
#[cfg(feature = "runtime")]
#[allow(clippy::wildcard_imports)]
use runtime::*;

/// Read-only inspect surface on the runtime state (ADR-0122 split).
/// Production callers observe lifecycle progress via subscribed stage
/// broadcasts rather than peeking at these.
#[cfg(feature = "runtime")]
impl LifecycleCapabilityState {
    /// Read-only access to the current state's kind id.
    #[must_use]
    pub fn current_state(&self) -> KindId {
        self.current_state
    }

    /// True once the lifecycle has broadcast a terminal state and
    /// further advances are no-ops.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.terminal_reached
    }

    /// True if a [`Quit`] mail has arrived but not yet been consumed.
    #[must_use]
    pub fn quit_pending(&self) -> bool {
        self.quit_pending
    }
}

/// Construction-level state fixture: a Render→Present→Shutdown
/// data graph + a fresh mailer, built directly (no chassis boot),
/// with the supplied advance timeout. Shared with
/// `mod settlement`'s tests via `pub(in crate::lifecycle)`.
#[cfg(all(test, feature = "runtime"))]
pub(in crate::lifecycle) fn test_cap(advance_timeout: Duration) -> LifecycleCapabilityState {
    use aether_kinds::{Present, Render, Shutdown};
    use aether_substrate::mail::registry::Registry;

    let graph = LifecycleGraphData::builder()
        .state::<Render>()
        .next::<Present>()
        .state::<Present>()
        .next::<Shutdown>()
        .quit::<Shutdown>()
        .terminal::<Shutdown>()
        .start::<Render>()
        .build()
        .expect("test setup: graph builds");
    let mailer = Arc::new(Mailer::new(Arc::new(Registry::default())));
    LifecycleCapabilityState {
        current_state: graph.start(),
        graph,
        subscribers: BTreeMap::new(),
        terminal_reached: false,
        quit_pending: false,
        pending: None,
        advance_timeout,
        settlement_latency_ewma: None,
        last_slow_warn: None,
        mailer,
    }
}

/// A `Tick`→`Shutdown` graph fixture (the round-trip test wants
/// `Tick` as a declared stage, which [`test_cap`]'s Render-rooted
/// graph doesn't carry).
#[cfg(all(test, feature = "runtime"))]
pub(in crate::lifecycle) fn tick_start_graph_cap() -> LifecycleCapabilityState {
    use aether_kinds::{Shutdown, Tick};
    use aether_substrate::mail::registry::Registry;

    let graph = LifecycleGraphData::builder()
        .state::<Tick>()
        .next::<Shutdown>()
        .terminal::<Shutdown>()
        .start::<Tick>()
        .build()
        .expect("test setup: tick graph builds");
    let mailer = Arc::new(Mailer::new(Arc::new(Registry::default())));
    LifecycleCapabilityState {
        current_state: graph.start(),
        graph,
        subscribers: BTreeMap::new(),
        terminal_reached: false,
        quit_pending: false,
        pending: None,
        advance_timeout: Duration::from_millis(ADVANCE_TIMEOUT_MS_DEFAULT),
        settlement_latency_ewma: None,
        last_slow_warn: None,
        mailer,
    }
}
