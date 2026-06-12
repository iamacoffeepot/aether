//! `aether.lifecycle` cap (ADR-0082). The bridged, non-generic
//! capability the chassis drives one frame at a time.
//!
//! The chassis owns cadence: it sends [`LifecycleAdvance`] once per
//! frame. The cap owns everything else — the lifecycle graph (a data
//! graph of `{ stage_kind, next, optional quit }` edges), the
//! subscriber table keyed by stage kind, the fan-out, and the
//! settlement gating. Because it is `#[bridge(singleton)]`d like
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

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

use aether_actor::FfiActorMailbox;
use aether_data::{Kind, KindId, MailboxId};
use aether_kinds::trace::Settled;
use aether_kinds::{
    LifecycleAdvance, LifecycleSubscribe, LifecycleSubscribeSelf, LifecycleUnsubscribe,
    LifecycleUnsubscribeAll, LifecycleUnsubscribeSelf, Quit,
};
// Reply types ride only the native handler bodies (via `super::`), so they
// elide on wasm where `mod native` is compiled out.
#[cfg(not(target_arch = "wasm32"))]
use aether_kinds::{LifecycleAdvanceComplete, LifecycleSubscribeResult};
#[cfg(not(target_arch = "wasm32"))]
use aether_substrate::actor::native::NativeActorMailbox;

#[cfg(not(target_arch = "wasm32"))]
pub use native::LifecycleConfig;

/// Sender-side facade for callers addressing [`LifecycleCapability`]
/// via `ctx.actor::<LifecycleCapability>()` (ADR-0082 §7, §12).
///
/// Lifts the stage-subscribe operations one indirection above the raw
/// `.send(&LifecycleSubscribe { .. })` so component code stops
/// reconstructing the kind struct (and the `.0` field unwraps) at every
/// call site — same shape and rationale as
/// [`InputMailboxExt`](crate::input::InputMailboxExt).
///
/// Impl'd for both transports `ctx.actor::<LifecycleCapability>()` can
/// return:
///
/// - [`FfiActorMailbox<LifecycleCapability>`] — always-on, for the §12
///   wasm-component stage-subscribe site.
/// - [`NativeActorMailbox<'_, LifecycleCapability>`] — native cap-to-cap
///   sends, gated on `#[cfg(not(target_arch = "wasm32"))]`.
///
/// All methods are fire-and-forget. `subscribe` / `unsubscribe` reply
/// via `aether.lifecycle.subscribe_result`; reply handling stays on the
/// caller. The cap fail-fasts (`Err`) on a stage its chassis graph
/// doesn't declare (ADR-0082 §7).
///
/// The generic escape hatch is unaffected: `mailbox.send(&LifecycleSubscribe { .. })`
/// still works, since `send` is an inherent method on the underlying
/// mailbox type.
pub trait LifecycleMailboxExt {
    /// Mail `aether.lifecycle.subscribe_self { stage }` to the cap —
    /// subscribe the *calling* actor to the lifecycle stage `K` (a
    /// stage kind, e.g. `Tick` / `Render`). The cap resolves the
    /// subscriber from the inbound's host-stamped `Source` (ADR-0083),
    /// so the call site spells out neither the stage id nor its own
    /// mailbox. This is the common form. Idempotent.
    fn subscribe<K: Kind>(&self);

    /// Mail `aether.lifecycle.subscribe { stage, mailbox }` to the cap.
    /// Add an *explicit* `mailbox` to the subscriber set for stage `K`.
    /// The rare cross-mailbox form; [`subscribe`](Self::subscribe)
    /// covers the self case. Idempotent.
    fn subscribe_for<K: Kind>(&self, mailbox: MailboxId);

    /// Mail `aether.lifecycle.unsubscribe_self { stage }` to the cap —
    /// unsubscribe the *calling* actor from stage `K`. Reflexive twin
    /// of [`subscribe`](Self::subscribe). Idempotent on "not currently
    /// subscribed."
    fn unsubscribe<K: Kind>(&self);

    /// Mail `aether.lifecycle.unsubscribe { stage, mailbox }` to the
    /// cap. Remove an *explicit* `mailbox` from the subscriber set for
    /// stage `K`. Idempotent on "not currently subscribed."
    fn unsubscribe_for<K: Kind>(&self, mailbox: MailboxId);
}

impl LifecycleMailboxExt for FfiActorMailbox<LifecycleCapability> {
    fn subscribe<K: Kind>(&self) {
        self.send(&LifecycleSubscribeSelf { stage: K::ID.0 });
    }
    fn subscribe_for<K: Kind>(&self, mailbox: MailboxId) {
        self.send(&LifecycleSubscribe {
            stage: K::ID.0,
            mailbox: mailbox.0,
        });
    }
    fn unsubscribe<K: Kind>(&self) {
        self.send(&LifecycleUnsubscribeSelf { stage: K::ID.0 });
    }
    fn unsubscribe_for<K: Kind>(&self, mailbox: MailboxId) {
        self.send(&LifecycleUnsubscribe {
            stage: K::ID.0,
            mailbox: mailbox.0,
        });
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl LifecycleMailboxExt for NativeActorMailbox<'_, LifecycleCapability> {
    fn subscribe<K: Kind>(&self) {
        self.send(&LifecycleSubscribeSelf { stage: K::ID.0 });
    }
    fn subscribe_for<K: Kind>(&self, mailbox: MailboxId) {
        self.send(&LifecycleSubscribe {
            stage: K::ID.0,
            mailbox: mailbox.0,
        });
    }
    fn unsubscribe<K: Kind>(&self) {
        self.send(&LifecycleUnsubscribeSelf { stage: K::ID.0 });
    }
    fn unsubscribe_for<K: Kind>(&self, mailbox: MailboxId) {
        self.send(&LifecycleUnsubscribe {
            stage: K::ID.0,
            mailbox: mailbox.0,
        });
    }
}

/// A non-terminal state in the lifecycle graph (ADR-0082 §1). A stage
/// kind id, a required `next` edge, and an optional `quit` escape edge.
/// Stage payloads are empty signals, so a state carries no factory — the
/// `<C>` chassis-context closure the original generic driver threaded is
/// gone (the data graph is non-generic, which is what makes the cap
/// bridgeable).
#[derive(Clone)]
pub(crate) struct LifecycleStateData {
    pub(crate) kind: KindId,
    pub(crate) next: KindId,
    pub(crate) quit: Option<KindId>,
}

/// A compiled lifecycle graph as plain data (ADR-0082 §1). Built via
/// [`LifecycleGraphData::builder`]; consumed by [`LifecycleCapability`]
/// at boot through [`LifecycleConfig`]. Freeze-at-construction — once
/// built it isn't mutated.
pub struct LifecycleGraphData {
    states: Vec<LifecycleStateData>,
    terminals: Vec<KindId>,
    start: KindId,
}

impl fmt::Debug for LifecycleGraphData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state_kinds: Vec<KindId> = self.states.iter().map(|s| s.kind).collect();
        f.debug_struct("LifecycleGraphData")
            .field("start", &self.start)
            .field("states", &state_kinds)
            .field("terminals", &self.terminals)
            .finish()
    }
}

impl LifecycleGraphData {
    /// Start building a new lifecycle graph. The returned builder is in
    /// the [`NoOpen`] state — no pending state — and accepts `.state`,
    /// `.terminal`, `.start`, or `.build`.
    #[must_use]
    pub fn builder() -> LifecycleGraphBuilder<NoOpen> {
        LifecycleGraphBuilder {
            inner: GraphInner {
                states: Vec::new(),
                terminals: Vec::new(),
                start: None,
                pending: None,
            },
            _state: PhantomData,
        }
    }

    /// Look up the state registered at `kind`. `None` for an unknown
    /// kind or a terminal.
    pub(crate) fn state(&self, kind: KindId) -> Option<&LifecycleStateData> {
        self.states.iter().find(|s| s.kind == kind)
    }

    /// True if `kind` is a registered terminal.
    pub(crate) fn is_terminal(&self, kind: KindId) -> bool {
        self.terminals.contains(&kind)
    }

    /// The configured start state's kind id.
    pub(crate) fn start(&self) -> KindId {
        self.start
    }
}

/// Builder type-state marker: no pending state. Initial state. Accepts
/// `.state`, `.terminal`, `.start`, or `.build`.
pub struct NoOpen;
/// Builder type-state marker: a state was just registered via `.state`
/// and needs its `next` edge before another state can be added. Accepts
/// `.next` (transitions to [`OpenWithNext`]) or `.quit` (stays here).
pub struct OpenNoNext;
/// Builder type-state marker: the current state has its `next` edge set.
/// `.state` / `.terminal` / `.start` / `.build` commit the pending state
/// and transition back to [`NoOpen`]; `.quit` is also still accepted.
pub struct OpenWithNext;

/// Builder for [`LifecycleGraphData`]. Built via
/// [`LifecycleGraphData::builder`]; finalized by `.build`. Mirrors the
/// original `LifecycleGraph` builder minus the `<C>` parameter and the
/// per-state factory closure — `.state::<K>()` records only `K::ID`,
/// because stage payloads are empty signals.
pub struct LifecycleGraphBuilder<S> {
    inner: GraphInner,
    _state: PhantomData<S>,
}

struct GraphInner {
    states: Vec<LifecycleStateData>,
    terminals: Vec<KindId>,
    start: Option<KindId>,
    pending: Option<PendingState>,
}

struct PendingState {
    kind: KindId,
    next: Option<KindId>,
    quit: Option<KindId>,
}

impl GraphInner {
    fn set_pending_quit(&mut self, quit: KindId) {
        if let Some(pending) = self.pending.as_mut() {
            pending.quit = Some(quit);
        }
    }

    /// Commit the pending state into `states`. The only callers reach
    /// here from `LifecycleGraphBuilder<OpenWithNext>`, which guarantees
    /// `pending.next.is_some()` — the unwrap is unreachable in well-typed
    /// code.
    fn commit_pending(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let next = pending.next.expect(
            "lifecycle builder bug: commit_pending invoked without a next edge set; \
             type-state should prevent this",
        );
        self.states.push(LifecycleStateData {
            kind: pending.kind,
            next,
            quit: pending.quit,
        });
    }
}

impl LifecycleGraphBuilder<NoOpen> {
    /// Register a new state. The stage's broadcast kind id is `K::ID`.
    #[must_use]
    pub fn state<K: Kind>(mut self) -> LifecycleGraphBuilder<OpenNoNext> {
        self.inner.pending = Some(PendingState {
            kind: <K as Kind>::ID,
            next: None,
            quit: None,
        });
        LifecycleGraphBuilder {
            inner: self.inner,
            _state: PhantomData,
        }
    }

    /// Register a terminal state. Terminals have no outgoing edges;
    /// reaching one ends the lifecycle.
    #[must_use]
    pub fn terminal<K: Kind>(mut self) -> Self {
        self.inner.terminals.push(<K as Kind>::ID);
        self
    }

    /// Set the start state. Exactly one `.start::<K>()` is required
    /// before `.build()`.
    #[must_use]
    pub fn start<K: Kind>(mut self) -> Self {
        self.inner.start = Some(<K as Kind>::ID);
        self
    }

    /// Finalize the graph.
    pub fn build(self) -> Result<LifecycleGraphData, BuildError> {
        finalize(self.inner)
    }
}

impl LifecycleGraphBuilder<OpenNoNext> {
    /// Set the pending state's `next` edge. Transitions to
    /// [`OpenWithNext`].
    #[must_use]
    pub fn next<K: Kind>(mut self) -> LifecycleGraphBuilder<OpenWithNext> {
        if let Some(pending) = self.inner.pending.as_mut() {
            pending.next = Some(<K as Kind>::ID);
        }
        LifecycleGraphBuilder {
            inner: self.inner,
            _state: PhantomData,
        }
    }

    /// Set the pending state's optional `quit` escape edge. Stays in
    /// [`OpenNoNext`] — `next` is still required.
    #[must_use]
    pub fn quit<K: Kind>(mut self) -> Self {
        self.inner.set_pending_quit(<K as Kind>::ID);
        self
    }
}

impl LifecycleGraphBuilder<OpenWithNext> {
    /// Set or override the pending state's optional `quit` escape edge.
    #[must_use]
    pub fn quit<K: Kind>(mut self) -> Self {
        self.inner.set_pending_quit(<K as Kind>::ID);
        self
    }

    /// Commit the pending state and start a new one.
    #[must_use]
    pub fn state<K: Kind>(mut self) -> LifecycleGraphBuilder<OpenNoNext> {
        self.inner.commit_pending();
        self.inner.pending = Some(PendingState {
            kind: <K as Kind>::ID,
            next: None,
            quit: None,
        });
        LifecycleGraphBuilder {
            inner: self.inner,
            _state: PhantomData,
        }
    }

    /// Commit the pending state and add a terminal.
    #[must_use]
    pub fn terminal<K: Kind>(mut self) -> LifecycleGraphBuilder<NoOpen> {
        self.inner.commit_pending();
        self.inner.terminals.push(<K as Kind>::ID);
        LifecycleGraphBuilder {
            inner: self.inner,
            _state: PhantomData,
        }
    }

    /// Commit the pending state and set the start.
    #[must_use]
    pub fn start<K: Kind>(mut self) -> LifecycleGraphBuilder<NoOpen> {
        self.inner.commit_pending();
        self.inner.start = Some(<K as Kind>::ID);
        LifecycleGraphBuilder {
            inner: self.inner,
            _state: PhantomData,
        }
    }

    /// Commit the pending state and finalize.
    pub fn build(mut self) -> Result<LifecycleGraphData, BuildError> {
        self.inner.commit_pending();
        finalize(self.inner)
    }
}

/// Errors returned by [`LifecycleGraphBuilder::build`]. Each variant
/// names the structural invariant violated and the kind id involved.
#[derive(Debug)]
pub enum BuildError {
    /// No `.start::<K>()` was called before `.build()`.
    MissingStart,
    /// `.start::<K>()` targeted a kind id that wasn't registered.
    StartNotRegistered { start: KindId },
    /// A state's `next` edge targets a kind id that isn't registered.
    NextNotRegistered { state: KindId, next: KindId },
    /// A state's `quit` edge targets a kind id that isn't registered.
    QuitNotRegistered { state: KindId, quit: KindId },
    /// The graph contains no terminals — no completion path.
    NoTerminals,
    /// A kind id is registered more than once (state, terminal, or
    /// both).
    DuplicateKind { kind: KindId },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingStart => f.write_str("no .start::<K>() was called before .build()"),
            Self::StartNotRegistered { start } => write!(
                f,
                "start kind {start:?} is not registered as a state or terminal"
            ),
            Self::NextNotRegistered { state, next } => write!(
                f,
                "state {state:?}: next target {next:?} is not registered as a state or terminal"
            ),
            Self::QuitNotRegistered { state, quit } => write!(
                f,
                "state {state:?}: quit target {quit:?} is not registered as a state or terminal"
            ),
            Self::NoTerminals => {
                f.write_str("graph has no terminal states; the lifecycle has no completion path")
            }
            Self::DuplicateKind { kind } => write!(
                f,
                "kind {kind:?} is registered more than once (appears as both a state and \
                 terminal, or as two states)"
            ),
        }
    }
}

impl Error for BuildError {}

fn finalize(inner: GraphInner) -> Result<LifecycleGraphData, BuildError> {
    let GraphInner {
        states,
        terminals,
        start,
        pending: _,
    } = inner;

    let start = start.ok_or(BuildError::MissingStart)?;

    // Duplicate-kind check across the union of states + terminals.
    let mut seen: Vec<KindId> = Vec::with_capacity(states.len() + terminals.len());
    let all_kinds = states
        .iter()
        .map(|s| s.kind)
        .chain(terminals.iter().copied());
    for kind in all_kinds {
        if seen.contains(&kind) {
            return Err(BuildError::DuplicateKind { kind });
        }
        seen.push(kind);
    }

    let known = |k: KindId| states.iter().any(|s| s.kind == k) || terminals.contains(&k);
    if !known(start) {
        return Err(BuildError::StartNotRegistered { start });
    }

    for s in &states {
        if !known(s.next) {
            return Err(BuildError::NextNotRegistered {
                state: s.kind,
                next: s.next,
            });
        }
        if let Some(q) = s.quit
            && !known(q)
        {
            return Err(BuildError::QuitNotRegistered {
                state: s.kind,
                quit: q,
            });
        }
    }

    if terminals.is_empty() {
        return Err(BuildError::NoTerminals);
    }

    Ok(LifecycleGraphData {
        states,
        terminals,
        start,
    })
}

#[aether_actor::bridge(singleton)]
mod native {
    use std::collections::{BTreeMap, BTreeSet};
    use std::env;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use aether_actor::actor;
    use aether_actor::actor::ctx::OutboundReply;
    use aether_data::{Kind, KindId, MailboxId as DataMailboxId, mailbox_id_from_name};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::{MailId, MailboxId, Source};

    use super::{
        LifecycleAdvance, LifecycleAdvanceComplete, LifecycleGraphData, LifecycleStateData,
        LifecycleSubscribe, LifecycleSubscribeResult, LifecycleSubscribeSelf, LifecycleUnsubscribe,
        LifecycleUnsubscribeAll, LifecycleUnsubscribeSelf, Quit, Settled,
    };

    /// Internal state-advance decision produced by `on_advance` before
    /// the cap mutates its own fields. Declared at module scope to keep
    /// the handler body statement-only (`clippy::items_after_statements`).
    enum Step {
        StateAdvance { broadcast: KindId, next: KindId },
        Terminal { broadcast: KindId },
        Unknown,
    }

    /// Default deadline for a pending advance's `Settled` to arrive
    /// before [`LifecycleCapability::on_advance`] force-completes it
    /// (iamacoffeepot/aether#1048). Override via
    /// `AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS`. Generous relative to the
    /// ~16 ms frame tick: normal settlement is sub-tick, so this only
    /// fires when the settlement pipeline has actually stalled —
    /// degrading a permanent wedge into a visible stutter rather than
    /// tripping on ordinary jitter.
    const ADVANCE_TIMEOUT_MS_DEFAULT: u64 = 1_000;

    /// Early-warning threshold for slow settlement
    /// (iamacoffeepot/aether#1052, the prevention follow-up to #1048). A
    /// `Sent`→`Settled` latency past `advance_timeout / SLOW_SETTLE_DIVISOR`
    /// (≈100ms at the 1s default, ~6 frames at 60Hz) is well above the
    /// sub-tick norm but a full 10× short of the force-complete deadline,
    /// so the warn surfaces a degrading settlement pipeline *before* it
    /// wedges, with headroom to act.
    const SLOW_SETTLE_DIVISOR: u32 = 10;

    /// EWMA smoothing factor for the rolling settlement-latency stat, as
    /// a permille (200 ‰ = 0.2). A single spike moves the average ~20%.
    const SETTLE_EWMA_ALPHA_PERMILLE: u32 = 200;

    /// Minimum spacing between slow-settlement warns. A saturating
    /// pipeline settles slowly on *every* advance, so an unguarded warn
    /// would itself spam the rings; one line per episode is enough.
    const SLOW_SETTLE_WARN_COOLDOWN: Duration = Duration::from_secs(5);

    /// Construction-time configuration for [`LifecycleCapability`].
    /// Carries the compiled data graph + the initial subscriber wiring.
    /// Built per-chassis at builder time and consumed by `init`.
    pub struct LifecycleConfig {
        /// The compiled lifecycle graph. Built via
        /// [`LifecycleGraphData::builder`](super::LifecycleGraphData::builder)
        /// on the chassis side.
        pub graph: LifecycleGraphData,
        /// Initial `(stage_kind, mailbox)` pairs to populate the
        /// subscriber table at boot — a chassis builder can pre-subscribe
        /// a mailbox to a stage this way without round-tripping a
        /// `LifecycleSubscribe` mail. Each pair must
        /// reference a stage kind declared by `graph` — the boot path
        /// verifies this and returns `BootError` otherwise, so
        /// misconfiguration fails fast at chassis-build.
        pub initial_subscribers: Vec<(KindId, DataMailboxId)>,
    }

    /// The `aether.lifecycle` capability (ADR-0082). Non-generic and
    /// bridged, so a wasm guest names it via
    /// `ctx.actor::<LifecycleCapability>()`. Owns the data graph, the
    /// subscriber table, the fan-out, and the settlement gating; the
    /// chassis only feeds it [`LifecycleAdvance`] cadence.
    ///
    /// Plain-field shape (ADR-0078): every handler runs on the cap's
    /// single dispatcher thread, so no `Mutex` / `Arc<Atomic*>` is needed
    /// for the subscriber table or state pointer.
    pub struct LifecycleCapability {
        graph: LifecycleGraphData,
        /// Subscriber table keyed by stage kind id (ADR-0082 §7).
        subscribers: BTreeMap<KindId, BTreeSet<DataMailboxId>>,
        /// Kind id of the state the cap will broadcast on the next
        /// [`LifecycleAdvance`]. Starts at `graph.start()`; mutated after
        /// each settled advance to the resolved next/quit edge target.
        current_state: KindId,
        /// True once the lifecycle reached a terminal — further advances
        /// are no-ops.
        terminal_reached: bool,
        /// Quit flag (ADR-0082 §3). Set by inbound [`Quit`] mail;
        /// consumed at the next state whose graph declares a `quit` edge.
        quit_pending: bool,
        /// In-flight advance awaiting settlement (ADR-0082 §6).
        pending: Option<PendingAdvance>,
        /// Deadline for a pending advance's `Settled`
        /// (iamacoffeepot/aether#1048). Set from
        /// `AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS`.
        advance_timeout: Duration,
        /// EWMA of observed `Sent`→`Settled` latency (ADR-0082 §6),
        /// updated once per settle. `None` until the first settlement.
        settlement_latency_ewma: Option<Duration>,
        /// Last time a slow-settlement warn fired, for the
        /// [`SLOW_SETTLE_WARN_COOLDOWN`] rate limit.
        last_slow_warn: Option<Instant>,
        /// `Arc<Mailer>` cached at init for `subscribe_settlement_mail`
        /// calls inside handlers.
        mailer: Arc<Mailer>,
    }

    /// Per-advance state tracked across `on_advance` → `on_settled`.
    struct PendingAdvance {
        /// Causal-chain root of the in-flight broadcast (ADR-0080 §6).
        root: MailId,
        /// Kind id of the state just broadcast — echoed in `completed`.
        completed_kind: KindId,
        /// Kind id of the state to broadcast next — echoed in `next`.
        /// `KindId(0)` when the settling broadcast was a terminal.
        next_kind: KindId,
        /// True if the settling broadcast is a terminal state.
        is_terminal: bool,
        /// Original chassis sender of the [`LifecycleAdvance`] mail.
        reply_to: Source,
        /// When this advance was issued. Drives the `advance_timeout`
        /// force-complete fallback (iamacoffeepot/aether#1048).
        started: Instant,
    }

    #[actor]
    impl NativeActor for LifecycleCapability {
        type Config = LifecycleConfig;
        const NAMESPACE: &'static str = "aether.lifecycle";

        fn init(config: LifecycleConfig, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let LifecycleConfig {
                graph,
                initial_subscribers,
            } = config;
            let current_state = graph.start();
            let advance_timeout_millis = env::var("AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(ADVANCE_TIMEOUT_MS_DEFAULT);
            let mailer = ctx.mailer();
            let mut subscribers: BTreeMap<KindId, BTreeSet<DataMailboxId>> = BTreeMap::new();
            for (stage, mailbox) in initial_subscribers {
                // Reject unknown-stage subscriptions at boot rather than
                // silently dropping mail at runtime — ADR-0082 §7's
                // fail-fast contract applies to compile-site config too.
                if graph.state(stage).is_none() && !graph.is_terminal(stage) {
                    return Err(BootError::Other(
                        format!(
                            "aether.lifecycle: initial subscriber references stage {stage:?} not \
                             declared by graph"
                        )
                        .into(),
                    ));
                }
                subscribers.entry(stage).or_default().insert(mailbox);
            }
            Ok(Self {
                graph,
                subscribers,
                current_state,
                terminal_reached: false,
                quit_pending: false,
                pending: None,
                advance_timeout: Duration::from_millis(advance_timeout_millis),
                settlement_latency_ewma: None,
                last_slow_warn: None,
                mailer,
            })
        }

        /// Subscribe a mailbox to a lifecycle stage broadcast (ADR-0082
        /// §7). Replies with [`LifecycleSubscribeResult`] —
        /// `Err { stage, error }` when the stage isn't declared in this
        /// chassis's graph (fail-fast at wire time).
        ///
        /// # Agent
        /// `LifecycleSubscribe { stage, mailbox }`. Stage must be a kind
        /// id registered as a state or terminal in the lifecycle graph.
        #[handler]
        fn on_subscribe(&mut self, ctx: &mut NativeCtx<'_>, payload: LifecycleSubscribe) {
            let stage_kind = KindId(payload.stage);
            let mailbox = DataMailboxId(payload.mailbox);
            let known =
                self.graph.state(stage_kind).is_some() || self.graph.is_terminal(stage_kind);
            let result = if known {
                self.subscribers
                    .entry(stage_kind)
                    .or_default()
                    .insert(mailbox);
                LifecycleSubscribeResult::Ok
            } else {
                LifecycleSubscribeResult::Err {
                    stage: payload.stage,
                    error: format!(
                        "stage {stage_kind:?} is not declared by this chassis's lifecycle graph"
                    ),
                }
            };
            ctx.reply(&result);
        }

        /// Subscribe the *sending* actor to a lifecycle stage broadcast
        /// (ADR-0082 §7, ADR-0083). Resolves the subscriber from the
        /// inbound envelope's host-stamped `Source` via
        /// [`source_mailbox`](NativeCtx::source_mailbox) rather than a
        /// caller-supplied mailbox, so the subscriber cannot be forged.
        /// `None` means the sender has no local mailbox (an external
        /// session or another engine) — reply `Err` and subscribe
        /// nothing, which gates the reflexive form to in-process actors
        /// by construction. Reuses [`Self::on_subscribe`]'s insert path
        /// once the mailbox is resolved.
        ///
        /// # Agent
        /// `LifecycleSubscribeSelf { stage }`. Stage must be a kind id
        /// registered as a state or terminal in the lifecycle graph.
        #[handler]
        fn on_subscribe_self(&mut self, ctx: &mut NativeCtx<'_>, payload: LifecycleSubscribeSelf) {
            let stage_kind = KindId(payload.stage);
            let result = match ctx.source_mailbox() {
                None => LifecycleSubscribeResult::Err {
                    stage: payload.stage,
                    error: "aether.lifecycle.subscribe_self requires a local component sender; \
                            an external session or remote engine must use \
                            aether.lifecycle.subscribe with an explicit mailbox"
                        .to_string(),
                },
                Some(sender) => {
                    let known = self.graph.state(stage_kind).is_some()
                        || self.graph.is_terminal(stage_kind);
                    if known {
                        self.subscribers
                            .entry(stage_kind)
                            .or_default()
                            .insert(DataMailboxId(sender.0));
                        LifecycleSubscribeResult::Ok
                    } else {
                        LifecycleSubscribeResult::Err {
                            stage: payload.stage,
                            error: format!(
                                "stage {stage_kind:?} is not declared by this chassis's \
                                 lifecycle graph"
                            ),
                        }
                    }
                }
            };
            ctx.reply(&result);
        }

        /// Unsubscribe a mailbox from a lifecycle stage broadcast.
        /// Idempotent on "not currently subscribed."
        ///
        /// # Agent
        /// `LifecycleUnsubscribe { stage, mailbox }`.
        #[handler]
        fn on_unsubscribe(&mut self, ctx: &mut NativeCtx<'_>, payload: LifecycleUnsubscribe) {
            let stage_kind = KindId(payload.stage);
            let mailbox = DataMailboxId(payload.mailbox);
            let known =
                self.graph.state(stage_kind).is_some() || self.graph.is_terminal(stage_kind);
            let result = if known {
                if let Some(set) = self.subscribers.get_mut(&stage_kind) {
                    set.remove(&mailbox);
                }
                LifecycleSubscribeResult::Ok
            } else {
                LifecycleSubscribeResult::Err {
                    stage: payload.stage,
                    error: format!(
                        "stage {stage_kind:?} is not declared by this chassis's lifecycle graph"
                    ),
                }
            };
            ctx.reply(&result);
        }

        /// Unsubscribe the *sending* actor from a lifecycle stage
        /// broadcast (ADR-0082 §7, ADR-0083). Resolves the subscriber
        /// from the inbound envelope's host-stamped `Source` via
        /// [`source_mailbox`](NativeCtx::source_mailbox), mirroring
        /// [`Self::on_subscribe_self`]. `None` (no local sender) replies
        /// `Err`. Idempotent on "not currently subscribed."
        ///
        /// # Agent
        /// `LifecycleUnsubscribeSelf { stage }`.
        #[handler]
        fn on_unsubscribe_self(
            &mut self,
            ctx: &mut NativeCtx<'_>,
            payload: LifecycleUnsubscribeSelf,
        ) {
            let stage_kind = KindId(payload.stage);
            let result = match ctx.source_mailbox() {
                None => LifecycleSubscribeResult::Err {
                    stage: payload.stage,
                    error: "aether.lifecycle.unsubscribe_self requires a local component sender; \
                            an external session or remote engine must use \
                            aether.lifecycle.unsubscribe with an explicit mailbox"
                        .to_string(),
                },
                Some(sender) => {
                    let known = self.graph.state(stage_kind).is_some()
                        || self.graph.is_terminal(stage_kind);
                    if known {
                        if let Some(set) = self.subscribers.get_mut(&stage_kind) {
                            set.remove(&DataMailboxId(sender.0));
                        }
                        LifecycleSubscribeResult::Ok
                    } else {
                        LifecycleSubscribeResult::Err {
                            stage: payload.stage,
                            error: format!(
                                "stage {stage_kind:?} is not declared by this chassis's \
                                 lifecycle graph"
                            ),
                        }
                    }
                }
            };
            ctx.reply(&result);
        }

        /// Remove `mailbox` from every lifecycle stage's subscriber set.
        /// Issued by `ComponentHostCapability` on `DropComponent` so a
        /// dropped trampoline doesn't keep receiving stage-broadcast mail
        /// — the lifecycle-family counterpart of
        /// [`InputCapability::on_unsubscribe_all`](crate::input::InputCapability),
        /// which the same drop path notifies for `aether.input`. No
        /// mailbox-validation: the trampoline's mailbox is already torn
        /// down by the time this fires; we accept any id and purge it from
        /// every stage. No reply.
        ///
        /// # Agent
        /// `LifecycleUnsubscribeAll { mailbox }`. Idempotent.
        #[handler]
        fn on_unsubscribe_all(
            &mut self,
            _ctx: &mut NativeCtx<'_>,
            payload: LifecycleUnsubscribeAll,
        ) {
            for set in self.subscribers.values_mut() {
                set.remove(&DataMailboxId(payload.mailbox));
            }
        }

        /// Lifecycle escape signal (ADR-0082 §3). Sets `quit_pending =
        /// true`; the next state in the graph that declares a `quit` edge
        /// consumes the flag.
        ///
        /// # Agent
        /// `Quit {}`. Sent by chassis bridges from ctrlc / winit
        /// `WindowEvent::CloseRequested` / future hub-shutdown mail.
        #[handler]
        fn on_quit(&mut self, _ctx: &mut NativeCtx<'_>, _payload: Quit) {
            self.quit_pending = true;
        }

        /// Drive the lifecycle one step (ADR-0082 §2). Broadcast the
        /// current state's signal to every subscriber registered for
        /// that stage, subscribe settlement on the broadcast root, and
        /// stash a [`PendingAdvance`] until [`Settled`] arrives. The
        /// state pointer mutates in [`Self::on_settled`], not here, so a
        /// chassis that overruns its cadence and sends two
        /// `LifecycleAdvance` mails in close succession sees the second
        /// warn-drop rather than skipping ahead through unsettled states.
        ///
        /// # Agent
        /// `LifecycleAdvance {}`. Sent by the chassis main loop each
        /// frame. Reply: [`LifecycleAdvanceComplete`] once the broadcast
        /// root settles.
        #[handler]
        fn on_advance(&mut self, ctx: &mut NativeCtx<'_>, _payload: LifecycleAdvance) {
            if self.terminal_reached {
                // Already done — reply immediately with zeros so the
                // chassis main loop unblocks and can break on `next == 0`.
                ctx.reply(&LifecycleAdvanceComplete {
                    completed: 0,
                    next: 0,
                });
                return;
            }

            if self.pending.is_some() {
                // Overlap: a prior advance hasn't settled yet. Normally
                // the chassis main loop wait-replies on every Advance, so
                // this is a duplicate-cadence-source bug — warn-and-drop
                // without state mutation. But if the pending advance has
                // blown past `advance_timeout`, its `Settled` is not
                // coming (a saturated settlement pipeline,
                // iamacoffeepot/aether#1048): force-complete it so the
                // lifecycle degrades to a stutter instead of wedging
                // forever, then fall through to process *this* advance.
                if !self.pending_timed_out() {
                    let pending = self
                        .pending
                        .as_ref()
                        .expect("pending.is_some() checked above");
                    tracing::warn!(
                        target: "aether_capabilities::lifecycle",
                        current = ?self.current_state,
                        pending_root = ?pending.root,
                        pending_for_millis = pending.started.elapsed().as_millis(),
                        stuck_stage = %pending.completed_kind,
                        fanout = ?self.subscribers.get(&pending.completed_kind),
                        "LifecycleAdvance received while a prior advance is still in flight; dropping"
                    );
                    return;
                }
                self.force_complete_pending(ctx);
                if self.terminal_reached {
                    ctx.reply(&LifecycleAdvanceComplete {
                        completed: 0,
                        next: 0,
                    });
                    return;
                }
            }

            // Decide what to broadcast and the post-settlement state.
            let step = if let Some(state) = self.graph.state(self.current_state) {
                let next = resolve_edge(state, &mut self.quit_pending);
                Step::StateAdvance {
                    broadcast: self.current_state,
                    next,
                }
            } else if self.graph.is_terminal(self.current_state) {
                Step::Terminal {
                    broadcast: self.current_state,
                }
            } else {
                // Defensive — builder finalize prevents this.
                Step::Unknown
            };

            let (broadcast, next_kind, is_terminal) = match step {
                Step::StateAdvance { broadcast, next } => (broadcast, next, false),
                Step::Terminal { broadcast } => (broadcast, KindId(0), true),
                Step::Unknown => {
                    ctx.reply(&LifecycleAdvanceComplete {
                        completed: 0,
                        next: 0,
                    });
                    return;
                }
            };

            // Broadcast first — children inherit the inbound's chain root
            // and parent edge. ADR-0080 settlement counts each child as
            // in-flight against the root. Stage payloads are empty
            // signals.
            broadcast_to_subscribers(ctx, &self.subscribers, broadcast);

            // Subscribe settlement on the inbound's chain root. The
            // broadcast subtree is part of that chain; settlement fires
            // once the inbound's `Finished` event drops the in-flight
            // count to zero (which includes every fan-out descendant).
            let root = ctx.in_flight_root();
            let reply_to = ctx.reply_target();
            if let Some(registry) = self.mailer.settlement_registry() {
                registry.subscribe_settlement_mail(
                    root,
                    // The cap's own mailbox id (Self::NAMESPACE) for its
                    // settlement subscription — a self-address compute with no
                    // sibling ctx, not a hardcoded peer namespace.
                    #[allow(clippy::disallowed_methods)]
                    mailbox_id_from_name(<Self as aether_actor::Actor>::NAMESPACE),
                    <Settled as Kind>::ID,
                    Arc::clone(&self.mailer),
                );
                self.pending = Some(PendingAdvance {
                    root,
                    completed_kind: broadcast,
                    next_kind,
                    is_terminal,
                    reply_to,
                    started: Instant::now(),
                });
            } else {
                // No settlement registry wired (test harness without
                // tracing). Fall back to fire-and-advance: reply
                // immediately and mutate state inline.
                if is_terminal {
                    self.terminal_reached = true;
                } else {
                    self.current_state = next_kind;
                }
                ctx.reply(&LifecycleAdvanceComplete {
                    completed: broadcast.0,
                    next: next_kind.0,
                });
            }
        }

        /// Settlement notice for the broadcast root pending in
        /// [`Self::pending`] (ADR-0082 §6). Advances the state pointer,
        /// flips `terminal_reached` if the settling broadcast was a
        /// terminal, and replies [`LifecycleAdvanceComplete`] to the
        /// chassis main loop that issued the [`LifecycleAdvance`].
        ///
        /// `Settled` notices for unrelated roots drop without state
        /// mutation.
        ///
        /// # Agent
        /// `Settled { root }`. Synthesised by the settlement registry
        /// when the in-flight count for `root` reaches zero; not a public
        /// API for user code.
        #[handler]
        fn on_settled(&mut self, ctx: &mut NativeCtx<'_>, payload: Settled) {
            let Some(pending) = self.pending.as_ref() else {
                return;
            };
            if payload.root != pending.root {
                return;
            }
            let latency = pending.started.elapsed();
            let root = pending.root;
            let completed = pending.completed_kind.0;
            let next = pending.next_kind.0;
            let reply_to = pending.reply_to;
            let is_terminal = pending.is_terminal;
            let next_kind = pending.next_kind;
            // Drop pending before reply so the reply-side mutation is
            // visible if a follow-on Advance lands inside the reply path.
            self.pending = None;
            if is_terminal {
                self.terminal_reached = true;
            } else {
                self.current_state = next_kind;
            }
            self.record_settlement_latency(latency, root);
            // Route the reply to whoever issued the LifecycleAdvance —
            // chassis main loops block on it to gate the next frame.
            ctx.reply_to(reply_to, &LifecycleAdvanceComplete { completed, next });
        }
    }

    impl LifecycleCapability {
        /// Fold one observed `Sent`→`Settled` latency into the rolling
        /// EWMA and emit a rate-limited warn when a settle blows past the
        /// slow threshold (`advance_timeout / SLOW_SETTLE_DIVISOR`). The
        /// early-warning for a degrading settlement pipeline
        /// (iamacoffeepot/aether#1052, the prevention follow-up to
        /// #1048): it fires with ~10× headroom before the force-complete
        /// deadline, naming the offending `root` so a
        /// `describe_tree <root>` surfaces the in-flight nodes. O(1) per
        /// settle.
        fn record_settlement_latency(&mut self, latency: Duration, root: MailId) {
            // EWMA in nanos, α = SETTLE_EWMA_ALPHA_PERMILLE/1000. Up and
            // down moves are handled separately so the whole thing stays
            // in u128 (no signed casts): next = prev ± α·|sample − prev|.
            let alpha = u128::from(SETTLE_EWMA_ALPHA_PERMILLE);
            let next_nanos = self
                .settlement_latency_ewma
                .map_or(latency.as_nanos(), |prev| {
                    let prev = prev.as_nanos();
                    let sample = latency.as_nanos();
                    if sample >= prev {
                        prev + (sample - prev) * alpha / 1000
                    } else {
                        prev - (prev - sample) * alpha / 1000
                    }
                });
            let ewma = Duration::from_nanos(u64::try_from(next_nanos).unwrap_or(u64::MAX));
            self.settlement_latency_ewma = Some(ewma);

            let threshold = self.advance_timeout / SLOW_SETTLE_DIVISOR;
            if latency < threshold {
                return;
            }
            if self
                .last_slow_warn
                .is_some_and(|t| t.elapsed() < SLOW_SETTLE_WARN_COOLDOWN)
            {
                return;
            }
            self.last_slow_warn = Some(Instant::now());
            tracing::warn!(
                target: "aether_capabilities::lifecycle",
                root = ?root,
                latency_millis = latency.as_millis(),
                ewma_millis = ewma.as_millis(),
                threshold_millis = threshold.as_millis(),
                "settlement latency exceeded the slow threshold; the trace/settlement \
                 pipeline is degrading — `describe_tree <root>` for the in-flight nodes; \
                 a sustained climb wedges the lifecycle (iamacoffeepot/aether#1048)"
            );
        }

        /// True when a pending advance has exceeded
        /// [`Self::advance_timeout`] without settling
        /// (iamacoffeepot/aether#1048). `false` when nothing is pending.
        fn pending_timed_out(&self) -> bool {
            self.pending
                .as_ref()
                .is_some_and(|p| p.started.elapsed() >= self.advance_timeout)
        }

        /// Force-complete a pending advance whose [`Settled`] never
        /// arrived (iamacoffeepot/aether#1048). Mirrors
        /// [`Self::on_settled`]'s state mutation + reply but logs at
        /// `error`: reaching here means the settlement pipeline stalled
        /// past `advance_timeout`. No-op when nothing is pending.
        fn force_complete_pending(&mut self, ctx: &mut NativeCtx<'_>) {
            let Some(pending) = self.pending.take() else {
                return;
            };
            tracing::error!(
                target: "aether_capabilities::lifecycle",
                root = ?pending.root,
                elapsed_millis = pending.started.elapsed().as_millis(),
                timeout_millis = self.advance_timeout.as_millis(),
                "LifecycleAdvance settlement timed out; force-advancing to avoid a permanent wedge \
                 (settlement pipeline may be saturated — see iamacoffeepot/aether#1048)"
            );
            if pending.is_terminal {
                self.terminal_reached = true;
            } else {
                self.current_state = pending.next_kind;
            }
            ctx.reply_to(
                pending.reply_to,
                &LifecycleAdvanceComplete {
                    completed: pending.completed_kind.0,
                    next: pending.next_kind.0,
                },
            );
        }

        /// Read-only access to the current state's kind id (test/inspect
        /// surface). Production callers observe lifecycle progress via
        /// subscribed stage broadcasts rather than peeking at this.
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

    /// Push the current stage's empty signal to each subscriber as an
    /// untyped envelope. Uses the runtime-id `send_envelope_traced` path
    /// because the broadcast kind is chosen at runtime (the current
    /// state's), not a compile-site `K`; the path preserves the inbound
    /// `(parent, root)` lineage so settlement counts each child against
    /// the root (ADR-0080 §6).
    fn broadcast_to_subscribers(
        ctx: &mut NativeCtx<'_>,
        subscribers: &BTreeMap<KindId, BTreeSet<DataMailboxId>>,
        stage: KindId,
    ) {
        let Some(set) = subscribers.get(&stage) else {
            return;
        };
        for mailbox in set {
            let _ = ctx.send_envelope_traced(MailboxId(mailbox.0), stage, &[]);
        }
    }

    /// Decide which edge to follow out of `state` given the current
    /// `quit_pending` flag (ADR-0082 §3). If `quit_pending` is set AND
    /// the state declares a `quit` edge, consume the flag and return the
    /// quit target; otherwise return the unconditional `next` target.
    fn resolve_edge(state: &LifecycleStateData, quit_pending: &mut bool) -> KindId {
        if *quit_pending && let Some(quit_target) = state.quit {
            *quit_pending = false;
            return quit_target;
        }
        state.next
    }

    #[cfg(test)]
    mod tests {
        //! Unit-level tests for the cap's decision logic. End-to-end
        //! broadcast / advance flow is covered by the `test_bench`
        //! frame-loop scenarios; the decision functions below carry the
        //! ADR-0082 §3 quit-flag semantics and the #1048/#1052
        //! settlement-latency gate, pinned at the unit layer.
        use super::*;
        use aether_kinds::{Present, Render, Shutdown, Tick};
        use aether_substrate::handle_store::HandleStore;
        use aether_substrate::mail::registry::Registry;

        fn state_with_quit(kind_id: u64, next: u64, quit: Option<u64>) -> LifecycleStateData {
            LifecycleStateData {
                kind: KindId(kind_id),
                next: KindId(next),
                quit: quit.map(KindId),
            }
        }

        /// Construction-level cap fixture: a Render→Present→Shutdown
        /// data graph + a fresh mailer, built directly (no chassis boot),
        /// with the supplied advance timeout.
        fn test_cap(advance_timeout: Duration) -> LifecycleCapability {
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
            let mailer = Arc::new(Mailer::new(
                Arc::new(Registry::default()),
                Arc::new(HandleStore::new(1024)),
            ));
            LifecycleCapability {
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

        #[test]
        fn resolve_edge_takes_next_when_no_quit_pending() {
            let state = state_with_quit(1, 2, Some(99));
            let mut quit = false;
            assert_eq!(resolve_edge(&state, &mut quit), KindId(2));
            assert!(!quit);
        }

        #[test]
        fn resolve_edge_takes_quit_when_pending_and_declared() {
            let state = state_with_quit(1, 2, Some(99));
            let mut quit = true;
            assert_eq!(resolve_edge(&state, &mut quit), KindId(99));
            assert!(!quit, "quit flag must be consumed");
        }

        #[test]
        fn resolve_edge_persists_quit_when_no_quit_edge_declared() {
            // ADR-0082 §3: the flag persists across states with no
            // declared quit edge; only states declaring `.quit::<K>()`
            // consume it.
            let state = state_with_quit(1, 2, None);
            let mut quit = true;
            assert_eq!(resolve_edge(&state, &mut quit), KindId(2));
            assert!(quit, "quit flag must persist when state has no quit edge");
        }

        #[test]
        fn cap_initial_state_is_graph_start() {
            let cap = test_cap(Duration::from_millis(ADVANCE_TIMEOUT_MS_DEFAULT));
            assert_eq!(cap.current_state(), <Render as Kind>::ID);
            assert!(!cap.is_terminal());
            assert!(!cap.quit_pending());
        }

        #[test]
        fn pending_timeout_predicate() {
            let mut cap = test_cap(Duration::ZERO);
            assert!(!cap.pending_timed_out());
            cap.pending = Some(PendingAdvance {
                root: MailId::NONE,
                completed_kind: <Render as Kind>::ID,
                next_kind: <Present as Kind>::ID,
                is_terminal: false,
                reply_to: Source::NONE,
                started: Instant::now(),
            });
            // Zero timeout: any elapsed >= 0 trips immediately.
            assert!(cap.pending_timed_out());
            // A long timeout never trips on a freshly-issued advance.
            cap.advance_timeout = Duration::from_hours(1);
            assert!(!cap.pending_timed_out());
        }

        #[test]
        fn settlement_latency_ewma_and_slow_warn_gate() {
            // advance_timeout 1s → slow threshold = 1s / 10 = 100ms.
            let mut cap = test_cap(Duration::from_secs(1));

            // First sample seeds the EWMA exactly.
            cap.record_settlement_latency(Duration::from_millis(10), MailId::NONE);
            assert_eq!(cap.settlement_latency_ewma, Some(Duration::from_millis(10)));
            assert!(cap.last_slow_warn.is_none());

            // Second sample moves the EWMA toward it by α=0.2:
            // 10ms + 0.2·(20ms − 10ms) = 12ms.
            cap.record_settlement_latency(Duration::from_millis(20), MailId::NONE);
            assert_eq!(cap.settlement_latency_ewma, Some(Duration::from_millis(12)));
            assert!(cap.last_slow_warn.is_none());

            // A settle past the 100ms threshold arms the warn + cooldown.
            cap.record_settlement_latency(Duration::from_millis(250), MailId::NONE);
            assert!(cap.last_slow_warn.is_some());
            let armed_at = cap.last_slow_warn.expect("warn armed");

            // A second slow settle inside the cooldown does not re-arm.
            cap.record_settlement_latency(Duration::from_millis(300), MailId::NONE);
            assert_eq!(
                cap.last_slow_warn.expect("still armed"),
                armed_at,
                "cooldown should suppress the second warn"
            );
        }

        #[test]
        fn on_unsubscribe_all_purges_mailbox_from_every_stage() {
            // A dropped trampoline's mailbox must leave every stage's
            // subscriber set in one shot (the drop-cleanup contract,
            // mirroring `InputCapability::on_unsubscribe_all`), while
            // co-subscribers on a shared stage survive.
            use aether_substrate::actor::native::binding::NativeBinding;

            let mut cap = test_cap(Duration::from_millis(ADVANCE_TIMEOUT_MS_DEFAULT));
            let dropped = DataMailboxId(0xDEAD);
            let survivor = DataMailboxId(0xBEEF);
            let render = <Render as Kind>::ID;
            let present = <Present as Kind>::ID;
            cap.subscribers.entry(render).or_default().insert(dropped);
            cap.subscribers.entry(render).or_default().insert(survivor);
            cap.subscribers.entry(present).or_default().insert(dropped);

            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&cap.mailer),
                MailboxId(0),
            ));
            let mut ctx = NativeCtx::new(&transport, Source::NONE, MailId::NONE, MailId::NONE);
            cap.on_unsubscribe_all(&mut ctx, LifecycleUnsubscribeAll { mailbox: dropped.0 });

            assert!(
                !cap.subscribers[&render].contains(&dropped),
                "dropped mailbox must leave the Render stage"
            );
            assert!(
                !cap.subscribers[&present].contains(&dropped),
                "dropped mailbox must leave the Present stage"
            );
            assert!(
                cap.subscribers[&render].contains(&survivor),
                "co-subscribers on a shared stage must survive the purge"
            );
        }

        /// A `Tick`→`Shutdown` graph fixture (the round-trip test wants
        /// `Tick` as a declared stage, which `test_cap`'s Render-rooted
        /// graph doesn't carry).
        fn tick_start_graph_cap() -> LifecycleCapability {
            let graph = LifecycleGraphData::builder()
                .state::<Tick>()
                .next::<Shutdown>()
                .terminal::<Shutdown>()
                .start::<Tick>()
                .build()
                .expect("test setup: tick graph builds");
            let mailer = Arc::new(Mailer::new(
                Arc::new(Registry::default()),
                Arc::new(HandleStore::new(1024)),
            ));
            LifecycleCapability {
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

        /// A `subscribe_self` carrying a `Component` source lands *that*
        /// mailbox in the stage set (ADR-0083: the cap reads the
        /// subscriber off the host-stamped envelope, not a payload field).
        #[test]
        fn subscribe_self_subscribes_the_component_source() {
            use aether_substrate::actor::native::binding::NativeBinding;
            use aether_substrate::mail::SourceAddr;

            let mut cap = test_cap(Duration::from_millis(ADVANCE_TIMEOUT_MS_DEFAULT));
            let render = <Render as Kind>::ID;
            let sender = DataMailboxId(0x00C0_FFEE);

            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&cap.mailer),
                MailboxId(0),
            ));
            let source = Source::to(SourceAddr::Component(MailboxId(sender.0)));
            let mut ctx = NativeCtx::new(&transport, source, MailId::NONE, MailId::NONE);
            cap.on_subscribe_self(&mut ctx, LifecycleSubscribeSelf { stage: render.0 });

            assert!(
                cap.subscribers
                    .get(&render)
                    .is_some_and(|s| s.contains(&sender)),
                "a Component-source subscribe_self lands that mailbox in the stage set"
            );
        }

        /// A `subscribe_self` from a non-`Component` source (an external
        /// session) replies `Err` and subscribes nothing — the reflexive
        /// form is gated to in-process actors by construction.
        #[test]
        fn subscribe_self_rejects_non_component_source() {
            use aether_data::{SessionToken, Uuid};
            use aether_substrate::actor::native::binding::NativeBinding;
            use aether_substrate::mail::SourceAddr;

            let mut cap = test_cap(Duration::from_millis(ADVANCE_TIMEOUT_MS_DEFAULT));
            let render = <Render as Kind>::ID;

            let transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&cap.mailer),
                MailboxId(0),
            ));
            let source = Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(0xFEED))));
            let mut ctx = NativeCtx::new(&transport, source, MailId::NONE, MailId::NONE);
            cap.on_subscribe_self(&mut ctx, LifecycleSubscribeSelf { stage: render.0 });

            assert!(
                cap.subscribers.get(&render).is_none_or(BTreeSet::is_empty),
                "a non-Component source subscribes nothing"
            );
        }

        /// Round trip through the host SDK path: calling
        /// `subscribe::<Tick>()` on a `NativeActorMailbox<LifecycleCapability>`
        /// emits `LifecycleSubscribeSelf { stage = Tick::ID }` whose
        /// `Source` the transport host-stamps to the calling actor, and
        /// delivering that mail to the cap lands the calling actor in the
        /// `Tick` stage set. The wasm FFI shims `export!` emits are
        /// wasm32-only, so the host test drives the cap through a
        /// `NativeBinding`.
        #[test]
        fn subscribe_via_native_mailbox_lands_calling_actor_in_stage_set() {
            use std::sync::mpsc;

            use aether_substrate::actor::native::NativeActorMailbox;
            use aether_substrate::actor::native::binding::NativeBinding;
            use aether_substrate::mail::SourceAddr;
            use aether_substrate::mail::registry::{InboxHandler, OwnedDispatch};

            use crate::lifecycle::LifecycleMailboxExt;
            use crate::test_chassis::fresh_substrate;

            let (registry, mailer) = fresh_substrate();

            // Capturing sink at the lifecycle mailbox: records the single
            // mail the SDK `subscribe::<Tick>()` emits so the test can read
            // back the kind, the host-stamped `Source`, and the payload.
            let (tx, rx) = mpsc::channel::<(KindId, Source, Vec<u8>)>();
            let handler: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
                let captured = (
                    dispatch.kind,
                    dispatch.sender,
                    dispatch.payload.bytes().to_vec(),
                );
                dispatch.discharge();
                let _ = tx.send(captured);
            });
            let lifecycle_id = registry.register_inbox(
                <LifecycleCapability as aether_actor::Actor>::NAMESPACE,
                handler,
            );

            // The calling actor: a transport stamped with SENDER as its
            // self-mailbox, so its sends carry `Source::Component(SENDER)`.
            let sender = DataMailboxId(0x00C0_FFEE);
            let tx_binding = NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(sender.0));
            let lifecycle =
                NativeActorMailbox::<LifecycleCapability>::__new(lifecycle_id.0, &tx_binding);
            lifecycle.subscribe::<Tick>();
            tx_binding.flush_outbound();

            let (kind, source, bytes) =
                rx.try_recv().expect("subscribe::<Tick>() emitted one mail");
            assert_eq!(
                kind,
                <LifecycleSubscribeSelf as Kind>::ID,
                "the SDK self-subscribe sends LifecycleSubscribeSelf"
            );
            assert_eq!(
                source.addr,
                SourceAddr::Component(MailboxId(sender.0)),
                "the host stamps the calling actor as the Source"
            );
            let decoded = LifecycleSubscribeSelf::decode_from_bytes(&bytes)
                .expect("payload decodes as LifecycleSubscribeSelf");
            assert_eq!(
                decoded.stage,
                <Tick as Kind>::ID.0,
                "the payload carries the Tick stage id"
            );

            // Deliver the captured mail to the cap exactly as the
            // dispatcher would, and confirm the calling actor is now in the
            // Tick stage set.
            let mut cap = tick_start_graph_cap();
            let cap_transport = Arc::new(NativeBinding::new_for_test(
                Arc::clone(&cap.mailer),
                MailboxId(0),
            ));
            let mut ctx = NativeCtx::new(&cap_transport, source, MailId::NONE, MailId::NONE);
            cap.on_subscribe_self(&mut ctx, decoded);

            assert!(
                cap.subscribers
                    .get(&<Tick as Kind>::ID)
                    .is_some_and(|s| s.contains(&sender)),
                "the calling actor lands in the Tick stage set"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_kinds::{InitCaps, InitComponents, Quit, Shutdown, Tick};

    fn init_to_shutdown_builder() -> LifecycleGraphBuilder<NoOpen> {
        LifecycleGraphData::builder()
            .state::<InitCaps>()
            .next::<Shutdown>()
            .terminal::<Shutdown>()
    }

    #[test]
    fn minimal_graph_init_to_terminal_builds() {
        let graph = init_to_shutdown_builder()
            .start::<InitCaps>()
            .build()
            .expect("test setup: minimal graph builds");
        assert_eq!(graph.start(), <InitCaps as Kind>::ID);
        assert!(graph.is_terminal(<Shutdown as Kind>::ID));
        assert!(!graph.is_terminal(<InitCaps as Kind>::ID));
        assert!(graph.state(<InitCaps as Kind>::ID).is_some());
    }

    #[test]
    fn build_rejects_missing_start() {
        let err = init_to_shutdown_builder()
            .build()
            .expect_err("missing start should fail");
        assert!(matches!(err, BuildError::MissingStart));
    }

    #[test]
    fn build_rejects_start_unregistered() {
        let err = init_to_shutdown_builder()
            .start::<Tick>()
            .build()
            .expect_err("start::<Tick> with no Tick state should fail");
        assert!(matches!(err, BuildError::StartNotRegistered { .. }));
    }

    #[test]
    fn build_rejects_next_unregistered() {
        let err = LifecycleGraphData::builder()
            .state::<InitCaps>()
            .next::<Tick>()
            .terminal::<Shutdown>()
            .start::<InitCaps>()
            .build()
            .expect_err("next::<Tick> with no Tick state should fail");
        assert!(matches!(err, BuildError::NextNotRegistered { .. }));
    }

    #[test]
    fn build_rejects_quit_unregistered() {
        let err = LifecycleGraphData::builder()
            .state::<InitCaps>()
            .quit::<Quit>()
            .next::<Shutdown>()
            .terminal::<Shutdown>()
            .start::<InitCaps>()
            .build()
            .expect_err("quit::<Quit> with no Quit state should fail");
        assert!(matches!(err, BuildError::QuitNotRegistered { .. }));
    }

    #[test]
    fn build_rejects_no_terminals() {
        let err = LifecycleGraphData::builder()
            .state::<InitCaps>()
            .next::<InitCaps>()
            .start::<InitCaps>()
            .build()
            .expect_err("graph with no terminals should fail");
        assert!(matches!(err, BuildError::NoTerminals));
    }

    #[test]
    fn build_rejects_duplicate_state_kind() {
        let err = LifecycleGraphData::builder()
            .state::<InitCaps>()
            .next::<Shutdown>()
            .state::<InitCaps>()
            .next::<Shutdown>()
            .terminal::<Shutdown>()
            .start::<InitCaps>()
            .build()
            .expect_err("duplicate state kind should fail");
        assert!(matches!(err, BuildError::DuplicateKind { .. }));
    }

    #[test]
    fn build_rejects_state_and_terminal_same_kind() {
        let err = LifecycleGraphData::builder()
            .state::<Shutdown>()
            .next::<InitCaps>()
            .terminal::<Shutdown>()
            .start::<Shutdown>()
            .build()
            .expect_err("kind registered as both state and terminal should fail");
        assert!(matches!(err, BuildError::DuplicateKind { .. }));
    }

    #[test]
    fn cycle_with_quit_edge_builds() {
        // InitCaps → InitComponents → InitCaps (back) — cyclic, with a
        // Quit escape edge to the Shutdown terminal. ADR-0082 §1 shape.
        let graph = LifecycleGraphData::builder()
            .state::<InitCaps>()
            .next::<InitComponents>()
            .state::<InitComponents>()
            .next::<InitCaps>()
            .quit::<Shutdown>()
            .terminal::<Shutdown>()
            .start::<InitCaps>()
            .build()
            .expect("test setup: cyclic graph with quit edge builds");
        assert!(
            graph
                .state(<InitComponents as Kind>::ID)
                .expect("test setup: InitComponents state registered")
                .quit
                .is_some()
        );
    }

    /// The typed-send gate the ext relies on: `ctx.actor::<LifecycleCapability>()`
    /// can only `.send()` the lifecycle subscribe kinds. A compile-time
    /// assertion — if a future edit drops a `HandlesKind` impl (e.g. the
    /// bridge stops emitting it), this stops building.
    #[test]
    fn cap_handles_the_subscribe_kinds() {
        use aether_actor::{HandlesKind, Singleton};
        fn assert_handles<R: HandlesKind<K>, K: Kind>() {}
        fn assert_singleton<R: Singleton>() {}
        assert_handles::<LifecycleCapability, LifecycleSubscribe>();
        assert_handles::<LifecycleCapability, LifecycleSubscribeSelf>();
        assert_handles::<LifecycleCapability, LifecycleUnsubscribe>();
        assert_handles::<LifecycleCapability, LifecycleUnsubscribeSelf>();
        assert_handles::<LifecycleCapability, LifecycleUnsubscribeAll>();
        assert_singleton::<LifecycleCapability>();
    }
}
