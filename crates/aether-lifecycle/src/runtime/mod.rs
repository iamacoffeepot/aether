//! The `aether.lifecycle` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;`
//! declaration in the parent carries the gate), so a transport-only build
//! of the `LifecycleCapability` identity never names these types nor pulls
//! `aether_substrate`. The substrate-typed imports are gated once by this
//! module rather than line-by-line; the `#[actor] impl` reaches the state,
//! ctx, settlement, and fan-out names through the single `use runtime::*`
//! glob in the parent.

// The settlement state machine and the boot-config seams, now nested under
// this `runtime` directory so the one `mod runtime;` gate in the parent covers
// them (no per-sibling `#[cfg]`).
mod config;
mod settlement;

// Lifecycle-level names the state and handlers reach. Explicit `use
// super::{…}` (never `use super::*` — clippy `wildcard_imports` is denied
// and exempts only `pub use`).
use super::{LifecycleCapability, LifecycleGraphData};

pub use self::config::{
    LifecycleConfig, LifecycleConfigLayer, LifecycleOverlay, LifecycleParams, frame_lifecycle_params,
};
#[cfg(test)]
pub use self::settlement::ADVANCE_TIMEOUT_MS_DEFAULT;
pub use self::settlement::{PendingAdvance, Step, resolve_edge};
pub use super::subscribers::{StageSubscribers, broadcast_to_subscribers};

// Handler-argument and reply kinds named by the moved `#[runtime] impl`
// bodies. Private to this module — the identity in the parent resolves the
// lifted `HandlesKind<K>` markers through its own `aether_kinds` imports.
use crate::kinds::{
    LifecycleSubscribe, LifecycleSubscribeResult, LifecycleSubscribeSelf, LifecycleUnsubscribe,
    LifecycleUnsubscribeSelf,
};
use aether_actor::ErasedActorRef;
use aether_actor::runtime;
use aether_kinds::trace::Settled;
use aether_kinds::{LifecycleAdvance, MonitorNotice, Quit};
use aether_substrate::actor::monitor::MonitorHandle;

pub use aether_actor::Manual;
pub use aether_actor::OutboundReply;
pub use aether_data::KindId;
pub use aether_kinds::LifecycleAdvanceComplete;
use aether_substrate::Erased;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use std::collections::BTreeMap;
pub use std::time::{Duration, Instant};

/// `aether.lifecycle` runtime state (ADR-0082). Owns the lifecycle data
/// graph, the subscriber table, the state pointer, and the settlement
/// gating; the chassis only feeds the cap [`LifecycleAdvance`]
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
/// Fields are `pub` so the settlement state machine
/// (`mod settlement`) can carry its inherent-impl cluster in a sibling
/// file and the parent's handlers can read them.
pub struct LifecycleCapabilityState {
    pub graph: LifecycleGraphData,
    /// Subscriber table, one typed set per stage (ADR-0082 §7). Each row is
    /// a `ProtocolRef<Subscriber<K>>` for its stage `K` (ADR-0231 §8): a
    /// subscription is accepted only once its subscriber is proven live and
    /// handling `K` silently, so the fan-out never handles a position a
    /// caller computed or an actor that cannot take the broadcast.
    pub subscribers: StageSubscribers,
    /// Kind id of the state the cap will broadcast on the next
    /// [`LifecycleAdvance`]. Starts at
    /// `graph.start()`; mutated after each settled advance to the resolved
    /// next/quit edge target.
    pub current_state: KindId,
    /// True once the lifecycle reached a terminal — further advances
    /// are no-ops.
    pub terminal_reached: bool,
    /// Quit flag (ADR-0082 §3). Set by inbound [`Quit`]
    /// mail; consumed at the next state whose graph declares a `quit` edge.
    pub quit_pending: bool,
    /// In-flight advance awaiting settlement (ADR-0082 §6).
    pub pending: Option<PendingAdvance>,
    /// Deadline for a pending advance's `Settled`
    /// (iamacoffeepot/aether#1048). Set from
    /// `AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS`.
    pub advance_timeout: Duration,
    /// EWMA of observed `Sent`→`Settled` latency (ADR-0082 §6),
    /// updated once per settle. `None` until the first settlement.
    pub settlement_latency_ewma: Option<Duration>,
    /// Last time a slow-settlement warn fired, for the
    /// `SLOW_SETTLE_WARN_COOLDOWN` rate limit.
    pub last_slow_warn: Option<Instant>,
    /// One monitor per subscriber (ADR-0079 §8 amended), registered on its
    /// first stage subscription and released when its `MonitorNotice`
    /// purges it. The handle's `Drop` deregisters, so the map is both the
    /// dedup guard and the RAII anchor. Keyed by the same proven reference
    /// the stage sets hold (ADR-0230), never by a position.
    pub monitors: BTreeMap<ErasedActorRef, MonitorHandle>,
}

/// Read-only inspect surface on the runtime state (ADR-0122 split).
/// Production callers observe lifecycle progress via subscribed stage
/// broadcasts rather than peeking at these.
impl LifecycleCapabilityState {
    /// Monitor `subscriber` on its first stage subscription so the cap
    /// purges its rows itself when the occupant departs —
    /// vacate or close, whichever comes first (ADR-0079 §8 amended).
    /// An `Err` (an actor outside the registry, or a spawner-less test
    /// binding) means "not monitorable": the rows then live until
    /// substrate teardown, exactly as they would for a mailbox that
    /// never goes away.
    pub fn watch<A, M: aether_actor::ReplyMode>(&mut self, ctx: &mut NativeCtx<'_, A, M>, subscriber: ErasedActorRef) {
        if !self.monitors.contains_key(&subscriber)
            && let Ok(handle) = ctx.monitor(subscriber)
        {
            self.monitors.insert(subscriber, handle);
        }
    }

    /// Whether this chassis's lifecycle graph declares `stage` as a state or
    /// a terminal (ADR-0082 §7).
    fn declares(&self, stage: KindId) -> bool {
        self.graph.state(stage).is_some() || self.graph.is_terminal(stage)
    }

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
/// data graph, built directly (no chassis boot),
/// with the supplied advance timeout. Reachable from
/// `mod settlement`'s descendant tests via module privacy.
#[cfg(test)]
fn test_cap(advance_timeout: Duration) -> LifecycleCapabilityState {
    use aether_kinds::{Present, Render, Shutdown};

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
    LifecycleCapabilityState {
        current_state: graph.start(),
        graph,
        subscribers: StageSubscribers::default(),
        terminal_reached: false,
        quit_pending: false,
        pending: None,
        advance_timeout,
        settlement_latency_ewma: None,
        last_slow_warn: None,
        monitors: BTreeMap::new(),
    }
}

/// A `Tick`→`Shutdown` graph fixture (the round-trip test wants
/// `Tick` as a declared stage, which [`test_cap`]'s Render-rooted
/// graph doesn't carry).
#[cfg(test)]
fn tick_start_graph_cap() -> LifecycleCapabilityState {
    use aether_kinds::{Shutdown, Tick};

    let graph = LifecycleGraphData::builder()
        .state::<Tick>()
        .next::<Shutdown>()
        .terminal::<Shutdown>()
        .start::<Tick>()
        .build()
        .expect("test setup: tick graph builds");
    LifecycleCapabilityState {
        current_state: graph.start(),
        graph,
        subscribers: StageSubscribers::default(),
        terminal_reached: false,
        quit_pending: false,
        pending: None,
        advance_timeout: Duration::from_millis(ADVANCE_TIMEOUT_MS_DEFAULT),
        settlement_latency_ewma: None,
        last_slow_warn: None,
        monitors: BTreeMap::new(),
    }
}

/// The refusal of a subscription request naming a stage this chassis's
/// lifecycle graph does not declare (ADR-0082 §7).
fn undeclared(stage: KindId) -> LifecycleSubscribeResult {
    LifecycleSubscribeResult::Err {
        stage: stage.0,
        error: format!("stage {stage:?} is not declared by this chassis's lifecycle graph"),
    }
}

#[runtime]
impl NativeActor for LifecycleCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// data graph, subscriber table, state pointer, and settlement gating.
    type State = LifecycleCapabilityState;

    type Config = LifecycleConfig;
    type Params = LifecycleParams;
    const NAMESPACE: &'static str = "aether.lifecycle";

    fn init(
        config: LifecycleConfig,
        params: LifecycleParams,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<LifecycleCapabilityState, BootError> {
        let LifecycleConfig { advance_timeout_millis } = config;
        let LifecycleParams { graph } = params;
        let current_state = graph.start();
        Ok(LifecycleCapabilityState {
            graph,
            subscribers: StageSubscribers::default(),
            current_state,
            terminal_reached: false,
            quit_pending: false,
            pending: None,
            advance_timeout: Duration::from_millis(advance_timeout_millis),
            settlement_latency_ewma: None,
            last_slow_warn: None,
            monitors: BTreeMap::new(),
        })
    }

    /// Subscribe an explicitly named actor to a lifecycle stage broadcast
    /// (ADR-0082 §7). Replies with [`LifecycleSubscribeResult`] —
    /// `Err { stage, error }` when the stage isn't declared in this
    /// chassis's graph (fail-fast at wire time), or when the subscriber is
    /// no longer live.
    ///
    /// The subscriber's path reached this handler only because its decode
    /// proved the live route there handles the stage silently (ADR-0231 §3);
    /// it is proven live once more here, at receipt, and the table keeps the
    /// `ProtocolRef<Subscriber<K>>` that proof returns.
    ///
    /// # Agent
    /// `LifecycleSubscribe { subscription }`, where `subscription` is
    /// `{"<Stage>": "<canonical subscriber path>"}`. The stage must be
    /// declared as a state or terminal in the lifecycle graph, and the actor
    /// at the path must be live and handle the stage with no reply.
    #[handler::single]
    fn on_subscribe(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: LifecycleSubscribe,
    ) -> LifecycleSubscribeResult {
        let stage_kind = StageSubscribers::stage_of(&payload.subscription);
        if !state.declares(stage_kind) {
            return undeclared(stage_kind);
        }

        match state.subscribers.subscribe(ctx, &payload.subscription) {
            Ok(subscriber) => {
                state.watch(ctx, subscriber);
                LifecycleSubscribeResult::Ok
            }
            Err(error) => LifecycleSubscribeResult::Err { stage: stage_kind.0, error: error.to_string() },
        }
    }

    /// Subscribe the *sending* actor to a lifecycle stage broadcast
    /// (ADR-0082 §7, ADR-0083). Resolves the subscriber from the
    /// inbound envelope's host-stamped `Source` via
    /// [`sender`](NativeCtx::sender) rather than a caller-supplied path, so
    /// the subscriber cannot be forged: the host already answered who sent
    /// this, and `sender` mints it only for a position that holds a route.
    /// `None` means the sender has no local mailbox (an external
    /// session or another engine) — reply `Err` and subscribe
    /// nothing, which gates the reflexive form to in-process actors
    /// by construction.
    ///
    /// The sender is then typed as a `Subscriber<K>` for the stage by the
    /// guard cast (ADR-0231 §4), which admits a route whose published rows
    /// handle the stage silently or manually. A sender with no such row is
    /// refused and subscribes nothing: its broadcasts could never be handled.
    ///
    /// # Agent
    /// `LifecycleSubscribeSelf { stage }`. Stage must be a kind id
    /// registered as a state or terminal in the lifecycle graph.
    #[handler::single]
    fn on_subscribe_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: LifecycleSubscribeSelf,
    ) -> LifecycleSubscribeResult {
        let stage_kind = KindId(payload.stage);
        let Some(sender) = ctx.sender() else {
            return LifecycleSubscribeResult::Err {
                stage: payload.stage,
                error: "aether.lifecycle.subscribe_self requires a local component sender; an external session or \
                        remote engine must use aether.lifecycle.subscribe with an explicit subscriber path"
                    .to_string(),
            };
        };
        if !state.declares(stage_kind) {
            return undeclared(stage_kind);
        }

        if state.subscribers.subscribe_sender(ctx, stage_kind, sender) {
            state.watch(ctx, sender);
            LifecycleSubscribeResult::Ok
        } else {
            LifecycleSubscribeResult::Err {
                stage: payload.stage,
                error: format!(
                    "{} has no silent or manual handler for stage {stage_kind:?}, so it cannot subscribe to it",
                    ctx.actor_path(sender)
                ),
            }
        }
    }

    /// Unsubscribe an explicitly named actor from a lifecycle stage
    /// broadcast. Idempotent on "not currently subscribed."
    ///
    /// The stage is checked first; the subscriber path is then proven at
    /// receipt and its key removed from the stage's set. A path that does not
    /// resolve to a live actor is answered `Ok`: a subscriber that has closed
    /// already lost its rows through the `MonitorNotice` that
    /// [`LifecycleCapabilityState::watch`] registered at subscribe, so it
    /// holds nothing to remove.
    ///
    /// # Agent
    /// `LifecycleUnsubscribe { subscription }`, the same shape as
    /// `LifecycleSubscribe`.
    #[handler::single]
    fn on_unsubscribe(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: LifecycleUnsubscribe,
    ) -> LifecycleSubscribeResult {
        let stage_kind = StageSubscribers::stage_of(&payload.subscription);
        if !state.declares(stage_kind) {
            return undeclared(stage_kind);
        }

        state.subscribers.unsubscribe(ctx, &payload.subscription);
        LifecycleSubscribeResult::Ok
    }

    /// Unsubscribe the *sending* actor from a lifecycle stage
    /// broadcast (ADR-0082 §7, ADR-0083). Resolves the subscriber
    /// from the inbound envelope's host-stamped `Source` via
    /// [`sender`](NativeCtx::sender), mirroring
    /// [`Self::on_subscribe_self`]. `None` (no local sender) replies
    /// `Err`. Idempotent on "not currently subscribed."
    ///
    /// # Agent
    /// `LifecycleUnsubscribeSelf { stage }`.
    #[handler::single]
    fn on_unsubscribe_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: LifecycleUnsubscribeSelf,
    ) -> LifecycleSubscribeResult {
        let stage_kind = KindId(payload.stage);
        let Some(sender) = ctx.sender() else {
            return LifecycleSubscribeResult::Err {
                stage: payload.stage,
                error: "aether.lifecycle.unsubscribe_self requires a local component sender; an external session \
                        or remote engine must use aether.lifecycle.unsubscribe with an explicit subscriber path"
                    .to_string(),
            };
        };
        if !state.declares(stage_kind) {
            return undeclared(stage_kind);
        }

        state.subscribers.unsubscribe_sender(stage_kind, sender);
        LifecycleSubscribeResult::Ok
    }

    /// Purge a departed subscriber (ADR-0079 §8 amended). The substrate
    /// fires one notice per [`LifecycleCapabilityState::watch`]ed
    /// subscriber when it vacates (the wasm trampoline on
    /// `DropComponent`) or closes, so a dropped component's stage
    /// broadcasts stop landing at its mailbox without any drop-time
    /// fan-out from the component host. Releasing the handle keeps the
    /// monitor map bounded by live subscribers; a later occupant of
    /// the same mailbox re-registers through its own subscribe.
    ///
    /// The host stamps the departed actor as the notice's sender, so
    /// `ctx.sender()` is the same erased key both tables are keyed by
    /// (ADR-0230) and each removal is a keyed lookup. A notice with no
    /// sender names nothing and changes nothing.
    #[handler::single]
    fn on_monitor_notice(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _notice: MonitorNotice) {
        let Some(departed) = ctx.sender() else {
            return;
        };
        state.monitors.remove(&departed);
        state.subscribers.purge(departed);
    }

    /// Lifecycle escape signal (ADR-0082 §3). Sets `quit_pending =
    /// true`; the next state in the graph that declares a `quit` edge
    /// consumes the flag.
    ///
    /// # Agent
    /// `Quit {}`. Sent by chassis bridges from ctrlc / winit
    /// `WindowEvent::CloseRequested` / future hub-shutdown mail.
    #[handler::single]
    fn on_quit(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _payload: Quit) {
        state.quit_pending = true;
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
    /// `LifecycleAdvance { delta_micros }`. Sent by the chassis main loop each
    /// frame. Reply: [`LifecycleAdvanceComplete`] once the broadcast
    /// root settles.
    #[handler::manual]
    fn on_advance(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, Manual>, payload: LifecycleAdvance) {
        if state.terminal_reached {
            // Already done — reply immediately with zeros so the
            // chassis main loop unblocks and can break on `next == 0`.
            ctx.reply(&LifecycleAdvanceComplete { completed: 0, next: 0 });
            return;
        }

        if state.pending.is_some() {
            // Overlap: a prior advance hasn't settled yet. Normally
            // the chassis main loop wait-replies on every Advance, so
            // this is a duplicate-cadence-source bug — warn-and-drop
            // without state mutation. But if the pending advance has
            // blown past `advance_timeout`, its `Settled` is not
            // coming (a saturated settlement pipeline,
            // iamacoffeepot/aether#1048): force-complete it so the
            // lifecycle degrades to a stutter instead of wedging
            // forever, then fall through to process *this* advance.
            if !state.pending_timed_out() {
                let pending = state.pending.as_ref().expect("pending.is_some() checked above");
                let fanout: Vec<String> = state
                    .subscribers
                    .subscribers_of(pending.completed_kind)
                    .into_iter()
                    .map(|subscriber| ctx.actor_path(subscriber).to_string())
                    .collect();
                tracing::warn!(
                    target: "aether_lifecycle",
                    current = ?state.current_state,
                    pending_root = ?pending.root,
                    pending_for_millis = pending.started.elapsed().as_millis(),
                    stuck_stage = %pending.completed_kind,
                    ?fanout,
                    "LifecycleAdvance received while a prior advance is still in flight; dropping"
                );
                return;
            }
            state.force_complete_pending(ctx);
            if state.terminal_reached {
                ctx.reply(&LifecycleAdvanceComplete { completed: 0, next: 0 });
                return;
            }
        }

        // Decide what to broadcast and the post-settlement state.
        let step = if let Some(state_data) = state.graph.state(state.current_state) {
            let next = resolve_edge(state_data, &mut state.quit_pending);
            Step::StateAdvance { broadcast: state.current_state, next }
        } else if state.graph.is_terminal(state.current_state) {
            Step::Terminal { broadcast: state.current_state }
        } else {
            // Defensive — builder finalize prevents this.
            Step::Unknown
        };

        let (broadcast, next_kind, is_terminal) = match step {
            Step::StateAdvance { broadcast, next } => (broadcast, next, false),
            Step::Terminal { broadcast } => (broadcast, KindId(0), true),
            Step::Unknown => {
                ctx.reply(&LifecycleAdvanceComplete { completed: 0, next: 0 });
                return;
            }
        };

        // Broadcast first — children inherit the inbound's chain root and
        // parent edge. ADR-0080 settlement counts each child as in-flight
        // against the root. Tick carries the chassis cadence's elapsed time;
        // every other stage remains an empty signal (issue 4470).
        broadcast_to_subscribers(ctx, &state.subscribers, broadcast, payload.delta_micros);

        // Subscribe settlement on the inbound's chain root. The
        // broadcast subtree is part of that chain; settlement fires
        // once the inbound's `Finished` event drops the in-flight
        // count to zero (which includes every fan-out descendant). An
        // advance that carries no chain has nothing to wait on and takes
        // the fire-and-advance branch.
        let reply_to = ctx.reply_target();
        if let Some(root) = ctx.in_flight_root()
            && ctx.subscribe_settlement::<Settled>(root)
        {
            state.pending = Some(PendingAdvance {
                root,
                completed_kind: broadcast,
                next_kind,
                is_terminal,
                reply_to,
                started: Instant::now(),
            });
        } else {
            // No chain to wait on, or no settlement registry wired (test
            // harness without tracing). Fall back to fire-and-advance:
            // reply immediately and mutate state inline.
            if is_terminal {
                state.terminal_reached = true;
            } else {
                state.current_state = next_kind;
            }
            ctx.reply(&LifecycleAdvanceComplete { completed: broadcast.0, next: next_kind.0 });
        }
    }

    /// Settlement notice for the broadcast root pending in
    /// [`LifecycleCapabilityState::pending`] (ADR-0082 §6). Advances the state pointer,
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
    #[handler::manual]
    fn on_settled(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Manual>, payload: Settled) {
        let Some(pending) = state.pending.as_ref() else {
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
        state.pending = None;
        if is_terminal {
            state.terminal_reached = true;
        } else {
            state.current_state = next_kind;
        }
        state.record_settlement_latency(latency, root);
        // Route the reply to whoever issued the LifecycleAdvance —
        // chassis main loops block on it to gate the next frame.
        ctx.reply_to(reply_to, &LifecycleAdvanceComplete { completed, next });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, mpsc};

    use aether_actor::{ActorPath, Addressable, Publisher};
    use aether_data::{Kind, LoadName};
    use aether_kinds::{Present, Render, Shutdown, Tick};
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::Source;
    use aether_substrate::mail::registry::{InboxHandler, OwnedDispatch, noop_handler};
    use aether_substrate::testing::{
        boot_test_chassis_with, drop_ref, fresh_substrate, registered_binding, registered_ref, unrouted_binding,
    };
    use aether_substrate::{BootError, Registry};

    use super::*;
    use crate::kinds::LifecycleSubscription;

    /// A keyed stage subscriber: silent `Tick`, `Render`, `Present`, and
    /// `Shutdown` handlers, so its path narrows to a subscriber of each. Each test
    /// stands a route at one of its keyed paths.
    struct Listener;

    #[aether_actor::actor(instanced, root, depends(LifecycleCapability))]
    impl NativeActor for Listener {
        const NAMESPACE: &'static str = "test.lifecycle.listener";
        type Config = ();

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }

        #[handler::single]
        fn on_tick(&mut self, _ctx: &mut NativeCtx<'_>, _tick: Tick) {
            let _ = self;
        }

        #[handler::single]
        fn on_render(&mut self, _ctx: &mut NativeCtx<'_>, _render: Render) {
            let _ = self;
        }

        #[handler::single]
        fn on_present(&mut self, _ctx: &mut NativeCtx<'_>, _present: Present) {
            let _ = self;
        }

        #[handler::single]
        fn on_shutdown(&mut self, _ctx: &mut NativeCtx<'_>, _shutdown: Shutdown) {
            let _ = self;
        }
    }

    /// A booted caller whose `wire` subscribes it to `Tick` through the flat
    /// send, the way a component's `ctx.subscribe` does.
    struct Caller;

    #[aether_actor::actor(singleton, root, depends(LifecycleCapability))]
    impl NativeActor for Caller {
        const NAMESPACE: &'static str = "test.lifecycle.caller";
        type Config = ();

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }

        fn wire(_state: &mut Self, ctx: &mut NativeCtx<'_>) {
            ctx.send::<LifecycleCapability>(&<LifecycleCapability as Publisher>::subscribe_request::<Tick>());
        }

        #[handler::single]
        fn on_tick(&mut self, _ctx: &mut NativeCtx<'_>, _tick: Tick) {
            let _ = self;
        }
    }

    fn listener(key: &str) -> ActorPath<Listener> {
        ActorPath::instance(&LoadName::new(key).expect("a valid key"))
    }

    /// Stand a closure route at `key`'s listener path, answering its proof.
    fn stand(registry: &Registry, key: &str) -> ErasedActorRef {
        registered_ref(registry, listener(key).as_erased().as_str(), noop_handler())
    }

    /// Deliver an explicit `subscribe` to `cap` as a handler receives it.
    fn subscribe(
        cap: &mut LifecycleCapabilityState,
        transport: &Arc<NativeBinding>,
        subscription: LifecycleSubscription,
    ) -> LifecycleSubscribeResult {
        let mut ctx = NativeCtx::new_for_actor(transport, Source::NONE, None, None);
        LifecycleCapability::on_subscribe(cap, &mut ctx, LifecycleSubscribe { subscription })
    }

    /// Stand a capturing sink at the lifecycle mailbox: it records each mail
    /// sent there with its host-stamped `Source`, which is what a handler's
    /// `ctx.sender()` reads back.
    fn lifecycle_sink(registry: &Registry) -> mpsc::Receiver<(KindId, Source, Vec<u8>)> {
        let (tx, rx) = mpsc::channel();
        let handler: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
            let captured = (dispatch.kind, dispatch.sender, dispatch.payload.bytes().to_vec());
            dispatch.discharge();
            let _ = tx.send(captured);
        });
        registered_ref(registry, <LifecycleCapability as Addressable>::NAMESPACE, handler);
        rx
    }

    /// Send `request` to the lifecycle mailbox from `binding` as a `Listener`
    /// through the flat typed send, answering the `Source` the host stamped.
    fn stamped_by(
        binding: &Arc<NativeBinding>,
        sink: &mpsc::Receiver<(KindId, Source, Vec<u8>)>,
        request: LifecycleSubscribeSelf,
    ) -> Source {
        NativeCtx::<'_, Listener>::new_for_actor(binding, Source::NONE, None, None)
            .send::<LifecycleCapability>(&request);
        binding.flush_outbound();

        sink.try_recv().expect("the request reached the lifecycle mailbox").1
    }

    /// An explicit `subscribe` proves its subscriber path live once, at
    /// receipt (ADR-0231 §3): a live path lands its reference in the stage
    /// set, and a path whose actor has gone replies `Err` naming the path and
    /// leaves the set alone rather than registering a subscription whose
    /// broadcasts could never land.
    #[test]
    fn explicit_subscribe_holds_a_live_path_and_refuses_one_that_is_gone() {
        let (registry, mailer) = fresh_substrate();
        let live = stand(&registry, "live");
        drop_ref(&registry, stand(&registry, "gone"));
        let mut cap = test_cap(Duration::from_millis(ADVANCE_TIMEOUT_MS_DEFAULT));
        let transport = unrouted_binding(&mailer);

        let held = subscribe(&mut cap, &transport, LifecycleSubscription::Render(listener("live").narrow()));
        let refused = subscribe(&mut cap, &transport, LifecycleSubscription::Render(listener("gone").narrow()));

        assert!(matches!(held, LifecycleSubscribeResult::Ok), "a live path subscribes");
        let LifecycleSubscribeResult::Err { error, .. } = refused else {
            panic!("a path whose actor has gone replies Err");
        };
        assert!(error.contains(listener("gone").as_erased().as_str()), "the refusal names the path: {error}");
        assert_eq!(cap.subscribers.subscribers_of(Render::ID), [live], "only the live subscriber is held");
    }

    /// A departed subscriber's `MonitorNotice` removes it from every stage it
    /// held, keyed by the notice's host-stamped sender, while a co-subscriber
    /// on a shared stage stays. A purge that missed a stage would keep
    /// broadcasting to a closed actor; one that matched loosely would drop a
    /// live subscriber.
    #[test]
    fn monitor_notice_purges_the_departed_subscriber_from_every_stage() {
        let (registry, mailer) = fresh_substrate();
        let sink = lifecycle_sink(&registry);
        let (departed_binding, _departed) =
            registered_binding(&registry, &mailer, listener("departed").as_erased().as_str(), noop_handler());
        let survivor = stand(&registry, "survivor");
        let mut cap = test_cap(Duration::from_millis(ADVANCE_TIMEOUT_MS_DEFAULT));
        let transport = unrouted_binding(&mailer);
        for subscription in [
            LifecycleSubscription::Render(listener("departed").narrow()),
            LifecycleSubscription::Present(listener("departed").narrow()),
            LifecycleSubscription::Render(listener("survivor").narrow()),
        ] {
            assert!(matches!(subscribe(&mut cap, &transport, subscription), LifecycleSubscribeResult::Ok));
        }

        let departed = stamped_by(&departed_binding, &sink, LifecycleSubscribeSelf { stage: Render::ID.0 });
        let mut ctx = NativeCtx::new_for_actor(&transport, departed, None, None);
        LifecycleCapability::on_monitor_notice(&mut cap, &mut ctx, MonitorNotice);

        assert_eq!(cap.subscribers.subscribers_of(Render::ID), [survivor], "the co-subscriber survives");
        assert!(cap.subscribers.subscribers_of(Present::ID).is_empty(), "the departed leaves every stage");
    }

    /// The broadcast is a typed send of each stage: `Tick` carries the
    /// advance's elapsed time and every other stage its empty signal
    /// (issue 4470). A broadcast that dropped the elapsed time would leave
    /// every motion subscriber still.
    #[test]
    fn broadcast_sends_tick_with_its_elapsed_time_and_other_stages_empty() {
        let (registry, mailer) = fresh_substrate();
        let (tx, rx) = mpsc::channel();
        let handler: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
            let captured = (dispatch.kind, dispatch.payload.bytes().to_vec());
            dispatch.discharge();
            let _ = tx.send(captured);
        });
        registered_ref(&registry, listener("motion").as_erased().as_str(), handler);
        let mut cap = tick_start_graph_cap();
        let transport = unrouted_binding(&mailer);
        for subscription in [
            LifecycleSubscription::Tick(listener("motion").narrow()),
            LifecycleSubscription::Shutdown(listener("motion").narrow()),
        ] {
            assert!(matches!(subscribe(&mut cap, &transport, subscription), LifecycleSubscribeResult::Ok));
        }

        let mut ctx: NativeCtx<'_> = NativeCtx::new_for_actor(&transport, Source::NONE, None, None);
        broadcast_to_subscribers(&mut ctx, &cap.subscribers, Tick::ID, 83_335);
        broadcast_to_subscribers(&mut ctx, &cap.subscribers, Shutdown::ID, 83_335);
        drop(ctx);
        transport.flush_outbound();

        let (tick_kind, tick) = rx.try_recv().expect("the Tick broadcast arrives");
        let (shutdown_kind, shutdown) = rx.try_recv().expect("the Shutdown broadcast arrives");
        assert_eq!((tick_kind, Tick::decode_from_bytes(&tick)), (Tick::ID, Some(Tick { delta_micros: 83_335 })));
        assert_eq!((shutdown_kind, Shutdown::decode_from_bytes(&shutdown)), (Shutdown::ID, Some(Shutdown)));
    }

    /// A `subscribe_self` from a non-`Component` source (an external
    /// session) replies `Err` and subscribes nothing — the reflexive
    /// form is gated to in-process actors by construction.
    #[test]
    fn subscribe_self_rejects_non_component_source() {
        use aether_data::{SessionToken, Uuid};
        use aether_substrate::mail::SourceAddr;
        use aether_substrate::testing::bare_substrate;

        let mut cap = test_cap(Duration::from_millis(ADVANCE_TIMEOUT_MS_DEFAULT));

        let (_registry, mailer) = bare_substrate();
        let transport = unrouted_binding(&mailer);
        let source = Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(0xFEED))));
        let mut ctx = NativeCtx::new_for_actor(&transport, source, None, None);
        LifecycleCapability::on_subscribe_self(&mut cap, &mut ctx, LifecycleSubscribeSelf { stage: Render::ID.0 });

        assert!(cap.subscribers.subscribers_of(Render::ID).is_empty(), "a non-Component source subscribes nothing");
    }

    /// Round trip through the host SDK path: the request the cap's
    /// `Publisher` impl builds for `Tick`, sent through the flat
    /// `ctx.send::<LifecycleCapability>`, is a `LifecycleSubscribeSelf` whose
    /// `Source` the transport stamps to the sender. Delivered to the cap, it
    /// lands a caller whose published rows handle `Tick` silently in the
    /// `Tick` set, and refuses a closure route, whose empty contract handles
    /// nothing (ADR-0231 §4's guard cast). A cast that admitted any sender
    /// would fan `Tick` out to an actor with no handler for it.
    #[test]
    fn subscribe_request_via_flat_send_lands_a_handling_caller_and_refuses_a_closure_route() {
        let (registry, mailer) = fresh_substrate();
        let sink = lifecycle_sink(&registry);
        let mut cap = tick_start_graph_cap();
        let cap_transport = unrouted_binding(&mailer);
        let deliver = |cap: &mut LifecycleCapabilityState, (kind, source, bytes): (KindId, Source, Vec<u8>)| {
            assert_eq!(kind, <LifecycleSubscribeSelf as Kind>::ID, "the flat subscribe sends LifecycleSubscribeSelf");
            let request = LifecycleSubscribeSelf::decode_from_bytes(&bytes).expect("the request decodes");
            assert_eq!(request.stage, Tick::ID.0, "the request carries the Tick stage id");
            let mut ctx = NativeCtx::new_for_actor(&cap_transport, source, None, None);
            LifecycleCapability::on_subscribe_self(cap, &mut ctx, request)
        };

        let (closure_binding, _closure) =
            registered_binding(&registry, &mailer, "test.lifecycle.closure_caller", noop_handler());
        NativeCtx::<'_, Caller>::new_for_actor(&closure_binding, Source::NONE, None, None)
            .send::<LifecycleCapability>(&<LifecycleCapability as Publisher>::subscribe_request::<Tick>());
        closure_binding.flush_outbound();
        let refused = deliver(&mut cap, sink.try_recv().expect("the closure route's request arrives"));

        let LifecycleSubscribeResult::Err { error, .. } = refused else {
            panic!("a closure route has no Tick handler, so it cannot subscribe");
        };
        assert!(error.contains("test.lifecycle.closure_caller"), "the refusal names the sender: {error}");
        assert!(cap.subscribers.subscribers_of(Tick::ID).is_empty(), "the refused sender subscribes nothing");

        let chassis = boot_test_chassis_with::<Caller>(&registry, &mailer, (), ());
        let caller = chassis.actor_ref::<Caller>().erase();
        let held =
            deliver(&mut cap, sink.recv_timeout(Duration::from_secs(5)).expect("the caller's wire sent its request"));

        assert!(matches!(held, LifecycleSubscribeResult::Ok), "a caller handling Tick silently subscribes");
        assert_eq!(cap.subscribers.subscribers_of(Tick::ID), [caller], "the caller lands in the Tick set");
    }
}
