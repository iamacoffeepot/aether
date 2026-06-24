//! The `aether.lifecycle` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;`
//! declaration in the parent carries the gate), so a transport-only build
//! of the `LifecycleCapability` identity never names these types nor pulls
//! `aether_substrate`. The substrate-typed imports are gated once by this
//! module rather than line-by-line; the `#[actor] impl` reaches the state,
//! ctx, settlement, and fan-out names through the single `use runtime::*`
//! glob in the parent.

// Lifecycle-level names the state and handlers reach. Explicit `use
// super::{…}` (never `use super::*` — clippy `wildcard_imports` is denied
// and exempts only `pub use`).
use super::LifecycleGraphData;

pub use super::config::LifecycleConfig;
#[cfg(test)]
pub use super::settlement::ADVANCE_TIMEOUT_MS_DEFAULT;
pub use super::settlement::{PendingAdvance, Step, resolve_edge};
pub use super::subscribers::broadcast_to_subscribers;

pub use aether_actor::Manual;
pub use aether_actor::actor::ctx::OutboundReply;
pub use aether_data::{Kind, KindId, MailboxId as DataMailboxId, mailbox_id_from_name};
pub use aether_kinds::LifecycleAdvanceComplete;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::mail::mailer::Mailer;
pub use std::collections::{BTreeMap, BTreeSet};
pub use std::sync::Arc;
pub use std::time::{Duration, Instant};

/// `aether.lifecycle` runtime state (ADR-0082). Owns the lifecycle data
/// graph, the subscriber table, the state pointer, and the settlement
/// gating; the chassis only feeds the cap [`LifecycleAdvance`](aether_kinds::LifecycleAdvance)
/// cadence. The dispatcher holds this as the cap's state and routes
/// envelopes through the macro-emitted `Dispatch` impl; the addressing
/// identity is the distinct ZST `LifecycleCapability`. Living in this
/// private module keeps it `pub`-enough to satisfy the `NativeActor::State`
/// interface without exposing it as crate-public API.
///
/// Plain-field shape (ADR-0078): every handler runs on the cap's single
/// dispatcher thread, so no `Mutex` / `Arc<Atomic*>` is needed for the
/// subscriber table or state pointer.
///
/// Fields are `pub(crate)` so the settlement state machine
/// (`mod settlement`) can carry its inherent-impl cluster in a sibling
/// file and the parent's handlers can read them.
pub struct LifecycleCapabilityState {
    pub(crate) graph: LifecycleGraphData,
    /// Subscriber table keyed by stage kind id (ADR-0082 §7).
    pub(crate) subscribers: BTreeMap<KindId, BTreeSet<DataMailboxId>>,
    /// Kind id of the state the cap will broadcast on the next
    /// [`LifecycleAdvance`](aether_kinds::LifecycleAdvance). Starts at
    /// `graph.start()`; mutated after each settled advance to the resolved
    /// next/quit edge target.
    pub(crate) current_state: KindId,
    /// True once the lifecycle reached a terminal — further advances
    /// are no-ops.
    pub(crate) terminal_reached: bool,
    /// Quit flag (ADR-0082 §3). Set by inbound [`Quit`](aether_kinds::Quit)
    /// mail; consumed at the next state whose graph declares a `quit` edge.
    pub(crate) quit_pending: bool,
    /// In-flight advance awaiting settlement (ADR-0082 §6).
    pub(crate) pending: Option<PendingAdvance>,
    /// Deadline for a pending advance's `Settled`
    /// (iamacoffeepot/aether#1048). Set from
    /// `AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS`.
    pub(crate) advance_timeout: Duration,
    /// EWMA of observed `Sent`→`Settled` latency (ADR-0082 §6),
    /// updated once per settle. `None` until the first settlement.
    pub(crate) settlement_latency_ewma: Option<Duration>,
    /// Last time a slow-settlement warn fired, for the
    /// `SLOW_SETTLE_WARN_COOLDOWN` rate limit.
    pub(crate) last_slow_warn: Option<Instant>,
    /// `Arc<Mailer>` cached at init for `subscribe_settlement_mail`
    /// calls inside handlers.
    pub(crate) mailer: Arc<Mailer>,
}
