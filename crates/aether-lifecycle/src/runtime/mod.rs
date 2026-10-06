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
use aether_actor::PathRefused;
use aether_actor::runtime;
use aether_kinds::trace::Settled;
use aether_kinds::{LifecycleAdvance, MonitorNotice, Quit};
use aether_substrate::actor::monitor::MonitorHandle;

pub use aether_actor::OutboundReply;
pub use aether_actor::Unchecked;
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
    /// purges its rows itself when the subscriber closes (ADR-0079 §8
    /// amended).
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

/// The Render→Present→Shutdown data graph the construction-level fixture
/// and the booted-cap tests share.
#[cfg(test)]
fn render_present_graph() -> LifecycleGraphData {
    use aether_kinds::{Present, Render, Shutdown};

    LifecycleGraphData::builder()
        .state::<Render>()
        .next::<Present>()
        .state::<Present>()
        .next::<Shutdown>()
        .quit::<Shutdown>()
        .terminal::<Shutdown>()
        .start::<Render>()
        .build()
        .expect("test setup: graph builds")
}

/// Construction-level state fixture: the [`render_present_graph`], built
/// directly (no chassis boot), with the supplied advance timeout. Reachable
/// from `mod settlement`'s descendant tests via module privacy.
#[cfg(test)]
fn test_cap(advance_timeout: Duration) -> LifecycleCapabilityState {
    let graph = render_present_graph();
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

/// The refusal of a subscription request naming a stage this chassis's
/// lifecycle graph does not declare (ADR-0082 §7).
fn undeclared(stage: KindId) -> LifecycleSubscribeResult {
    LifecycleSubscribeResult::stage_error(
        stage.0,
        format!("stage {stage:?} is not declared by this chassis's lifecycle graph"),
    )
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
    /// `Err(Stage { stage, error })` when the stage isn't declared in this
    /// chassis's graph (fail-fast at wire time), or `Err(Subscriber(..))`
    /// when the subscriber is no longer live.
    ///
    /// The subscriber's path reached this handler only because its decode
    /// proved the route there, live or closed, handles the stage silently
    /// (ADR-0231 §3); it is proven live here, at receipt, and the table keeps
    /// the `ProtocolRef<Subscriber<K>>` that proof returns. A closed
    /// subscriber is answered `Err(Subscriber(..))` naming its path. A path
    /// no route has stood at, or whose route does not handle the stage
    /// silently, is refused at decode, and the dispatch answers it the same
    /// way.
    ///
    /// # Agent
    /// `LifecycleSubscribe { subscription }`, where `subscription` is
    /// `{"<Stage>": "<canonical subscriber path>"}`. The stage must be
    /// declared as a state or terminal in the lifecycle graph, and the actor
    /// at the path must be live and handle the stage with no reply.
    #[handler::request]
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
            Err(error) => PathRefused::from(error).into(),
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
    /// handle the stage with a silent or unchecked handler. A sender with no such row is
    /// refused and subscribes nothing: its broadcasts could never be handled.
    ///
    /// # Agent
    /// `LifecycleSubscribeSelf { stage }`. Stage must be a kind id
    /// registered as a state or terminal in the lifecycle graph.
    #[handler::request]
    fn on_subscribe_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: LifecycleSubscribeSelf,
    ) -> LifecycleSubscribeResult {
        let stage_kind = KindId(payload.stage);
        let Some(sender) = ctx.sender() else {
            return LifecycleSubscribeResult::stage_error(
                payload.stage,
                "aether.lifecycle.subscribe_self requires a local component sender; an external session or \
                 remote engine must use aether.lifecycle.subscribe with an explicit subscriber path",
            );
        };
        if !state.declares(stage_kind) {
            return undeclared(stage_kind);
        }

        if state.subscribers.subscribe_sender(ctx, stage_kind, sender) {
            state.watch(ctx, sender);
            LifecycleSubscribeResult::Ok
        } else {
            LifecycleSubscribeResult::stage_error(
                payload.stage,
                format!(
                    "{} has no silent or unchecked handler for stage {stage_kind:?}, so it cannot subscribe to it",
                    ctx.actor_path(sender)
                ),
            )
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
    #[handler::request]
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
    #[handler::request]
    fn on_unsubscribe_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: LifecycleUnsubscribeSelf,
    ) -> LifecycleSubscribeResult {
        let stage_kind = KindId(payload.stage);
        let Some(sender) = ctx.sender() else {
            return LifecycleSubscribeResult::stage_error(
                payload.stage,
                "aether.lifecycle.unsubscribe_self requires a local component sender; an external session \
                 or remote engine must use aether.lifecycle.unsubscribe with an explicit subscriber path",
            );
        };
        if !state.declares(stage_kind) {
            return undeclared(stage_kind);
        }

        state.subscribers.unsubscribe_sender(stage_kind, sender);
        LifecycleSubscribeResult::Ok
    }

    /// Purge a departed subscriber (ADR-0079 §8 amended). The substrate
    /// fires one notice per [`LifecycleCapabilityState::watch`]ed
    /// subscriber when it closes (the wasm trampoline on `DropComponent`),
    /// so a dropped component's
    /// stage broadcasts stop landing at its mailbox without any drop-time
    /// fan-out from the component host. Releasing the handle keeps the
    /// monitor map bounded by live subscribers.
    ///
    /// The host stamps the departed actor as the notice's sender, so
    /// `ctx.sender()` is the same erased key both tables are keyed by
    /// (ADR-0230) and each removal is a keyed lookup. A notice with no
    /// sender names nothing and changes nothing.
    #[handler::event]
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
    #[handler::tell]
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
    #[handler::unchecked(reason = "a held reply would pin the root the advance waits to settle (#6967)")]
    fn on_advance(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, Unchecked>, payload: LifecycleAdvance) {
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
    #[handler::unchecked(reason = "a held reply would pin the root the advance waits to settle (#6967)")]
    fn on_settled(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Unchecked>, payload: Settled) {
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

    use aether_actor::{ActorPath, ActorRef, HandlesKind, PathRefusal, Publisher};
    use aether_data::{Kind, LoadName, MailId, SessionToken, Uuid};
    use aether_kinds::{Present, Render, Shutdown, Tick};
    use aether_substrate::mail::outbound::EgressEvent;
    use aether_substrate::mail::registry::{InboxHandler, OwnedDispatch};
    use aether_substrate::testing::{PumpedDriver, boot_bare_test_chassis, fresh_substrate_and_rx, registered_ref};
    use aether_substrate::{BootError, Registry, ReplyTarget, Subname};

    use super::*;
    use crate::kinds::{LifecycleSubscribeError, LifecycleSubscription};

    /// A stage a [`Listener`] heard, forwarded to the test.
    #[derive(Debug, PartialEq)]
    enum Heard {
        Tick(Tick),
        Render,
        Present,
        Shutdown,
    }

    /// A keyed stage subscriber: silent `Tick`, `Render`, `Present`, and
    /// `Shutdown` handlers, so its path narrows to a subscriber of each, each
    /// reporting what it heard over its config's channel. A send whose
    /// receiver the test dropped is discarded. `Quit` closes it, so the
    /// runtime posts a `MonitorNotice` to every actor watching it.
    struct Listener {
        heard: mpsc::Sender<Heard>,
    }

    #[aether_actor::actor(instanced, root, depends(LifecycleCapability))]
    impl NativeActor for Listener {
        const NAMESPACE: &'static str = "test.lifecycle.listener";
        type Config = mpsc::Sender<Heard>;

        fn init(heard: mpsc::Sender<Heard>, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { heard })
        }

        #[handler::event]
        fn on_tick(&mut self, _ctx: &mut NativeCtx<'_>, tick: Tick) {
            let _ = self.heard.send(Heard::Tick(tick));
        }

        #[handler::event]
        fn on_render(&mut self, _ctx: &mut NativeCtx<'_>, _render: Render) {
            let _ = self.heard.send(Heard::Render);
        }

        #[handler::event]
        fn on_present(&mut self, _ctx: &mut NativeCtx<'_>, _present: Present) {
            let _ = self.heard.send(Heard::Present);
        }

        #[handler::event]
        fn on_shutdown(&mut self, _ctx: &mut NativeCtx<'_>, _shutdown: Shutdown) {
            let _ = self.heard.send(Heard::Shutdown);
        }

        #[handler::tell]
        fn on_quit(&mut self, ctx: &mut NativeCtx<'_>, _quit: Quit) {
            let _ = self;
            ctx.shutdown();
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

        #[handler::event]
        fn on_tick(&mut self, _ctx: &mut NativeCtx<'_>, _tick: Tick) {
            let _ = self;
        }
    }

    fn listener(key: &str) -> ActorPath<Listener> {
        ActorPath::instance(&LoadName::new(key).expect("a valid key"))
    }

    /// The Tick→Shutdown graph the broadcast and flat-send tests advance
    /// over: `Tick` is a declared stage there, which [`render_present_graph`]
    /// does not carry.
    fn tick_graph() -> LifecycleGraphData {
        LifecycleGraphData::builder()
            .state::<Tick>()
            .next::<Shutdown>()
            .terminal::<Shutdown>()
            .start::<Tick>()
            .build()
            .expect("test setup: tick graph builds")
    }

    /// The hub session every externally sent request replies to.
    fn session(correlation: u64) -> ReplyTarget {
        ReplyTarget::Session { session: SessionToken(Uuid::from_u128(0xFEED)), correlation }
    }

    /// A `LifecycleCapability` booted pumped on a bare `TestChassis` and
    /// driven the way a pumped chassis driver drives it, beside the registry
    /// it routes through, the egress its session replies leave through, and
    /// the replies read off it but not yet asked for, as
    /// `(correlation, kind name, payload)`.
    struct Booted {
        registry: Arc<Registry>,
        driver: PumpedDriver<LifecycleCapability>,
        egress: mpsc::Receiver<EgressEvent>,
        replies: Vec<(u64, String, Vec<u8>)>,
    }

    fn boot_lifecycle(graph: LifecycleGraphData) -> Booted {
        let (registry, mailer, egress) = fresh_substrate_and_rx();
        let driver = PumpedDriver::boot(
            boot_bare_test_chassis(&registry, &mailer),
            LifecycleConfig::default(),
            LifecycleParams { graph },
        );
        Booted { registry, driver, egress, replies: Vec::new() }
    }

    impl Booted {
        /// Mail `request` to the cap as an external session correlated by
        /// `correlation` — a sender with no local mailbox — and return its
        /// tracked root. Nothing runs until the root is settled.
        fn request<K: Kind>(&self, request: &K, correlation: u64) -> MailId
        where
            LifecycleCapability: HandlesKind<K>,
        {
            let lifecycle = self.driver.chassis().actor_ref::<LifecycleCapability>();
            self.driver.send_tracked(lifecycle, request, Some(session(correlation)))
        }

        /// The session reply correlated by `correlation`, decoded as `K`. The
        /// wait that covers a request returns only after its reply is sent,
        /// so the reply is read here, never waited on.
        fn reply<K: Kind>(&mut self, correlation: u64) -> K {
            for event in self.egress.try_iter() {
                if let EgressEvent::ToSession { kind_name, payload, correlation_id, .. } = event {
                    self.replies.push((correlation_id, kind_name, payload));
                }
            }
            let at = self
                .replies
                .iter()
                .position(|(id, ..)| *id == correlation)
                .unwrap_or_else(|| panic!("reply {correlation} was not sent before its root settled"));
            let (_, kind_name, payload) = self.replies.remove(at);
            assert_eq!(kind_name, K::NAME, "reply {correlation} is a {}", K::NAME);
            K::decode_from_bytes(&payload).expect("the reply decodes")
        }

        /// Subscribe `subscription` from an external session and answer the
        /// cap's reply.
        fn subscribe(&mut self, subscription: LifecycleSubscription, correlation: u64) -> LifecycleSubscribeResult {
            let root = self.request(&LifecycleSubscribe { subscription }, correlation);
            self.driver.settle(&[root]);
            self.reply(correlation)
        }

        fn subscribers_of(&self, stage: KindId) -> Vec<ErasedActorRef> {
            self.driver.read_state(|state| state.subscribers.subscribers_of(stage)).expect("the lifecycle cap is live")
        }

        /// Spawn a [`Listener`] at `key` reporting what it hears on `heard`.
        fn spawn_listener(&self, key: &str, heard: mpsc::Sender<Heard>) -> ActorRef<Listener> {
            self.driver
                .chassis()
                .spawn_actor_for_test::<Listener>(Subname::Named(key), heard, ())
                .finish()
                .expect("the listener spawns")
        }

        /// Close `listener` through its own `Quit` handler.
        fn quit(&self, listener: ActorRef<Listener>) {
            self.driver.chassis().send_for_reply(listener, &Quit, session(0));
        }

        /// [`Self::quit`], then wait until the listener's route stops
        /// answering live.
        fn close(&self, listener: ActorRef<Listener>) {
            self.quit(listener);
            self.driver.chassis().await_closed(listener.erase());
        }
    }

    /// An explicit `subscribe` proves its subscriber path live (ADR-0231 §3):
    /// a live path lands its reference in the stage set, and a path whose
    /// actor has closed still decodes, so the handler answers
    /// `Err(Subscriber(..))` naming the path as not live and leaves the set
    /// alone rather than registering a subscription whose broadcasts could
    /// never land.
    #[test]
    fn explicit_subscribe_holds_a_live_path_and_refuses_one_that_is_gone() {
        let mut booted = boot_lifecycle(render_present_graph());
        let (heard, _) = mpsc::channel();
        let live = booted.spawn_listener("live", heard.clone());
        booted.close(booted.spawn_listener("gone", heard));

        let gone = booted
            .request(&LifecycleSubscribe { subscription: LifecycleSubscription::Render(listener("gone").narrow()) }, 1);
        let held = booted
            .request(&LifecycleSubscribe { subscription: LifecycleSubscription::Render(listener("live").narrow()) }, 2);
        booted.driver.settle(&[gone, held]);

        assert!(matches!(booted.reply(2), LifecycleSubscribeResult::Ok), "a live path subscribes");
        let LifecycleSubscribeResult::Err(LifecycleSubscribeError::Subscriber(refused)) = booted.reply(1) else {
            panic!("a closed path is refused");
        };
        assert_eq!(
            refused,
            PathRefused { path: listener("gone").as_erased().clone(), reason: PathRefusal::NotLive },
            "the refusal names the path and why",
        );
        assert_eq!(booted.subscribers_of(Render::ID), [live.erase()], "only the live subscriber is held");
    }

    /// A departed subscriber's `MonitorNotice` removes it from every stage it
    /// held, keyed by the notice's host-stamped sender, while a co-subscriber
    /// on a shared stage stays. A purge that missed a stage would keep
    /// broadcasting to a closed actor; one that matched loosely would drop a
    /// live subscriber.
    #[test]
    fn monitor_notice_purges_the_departed_subscriber_from_every_stage() {
        let mut booted = boot_lifecycle(render_present_graph());
        let (heard, _) = mpsc::channel();
        let departed = booted.spawn_listener("departed", heard.clone());
        let survivor = booted.spawn_listener("survivor", heard);
        for (correlation, subscription) in [
            (1, LifecycleSubscription::Render(listener("departed").narrow())),
            (2, LifecycleSubscription::Present(listener("departed").narrow())),
            (3, LifecycleSubscription::Render(listener("survivor").narrow())),
        ] {
            assert!(matches!(booted.subscribe(subscription, correlation), LifecycleSubscribeResult::Ok));
        }

        booted.quit(departed);
        booted.driver.pump_until("the departed subscriber's purge", |state| {
            state.subscribers.subscribers_of(Present::ID).is_empty()
        });

        assert_eq!(booted.subscribers_of(Render::ID), [survivor.erase()], "the co-subscriber survives");
    }

    /// The broadcast is a typed send of each stage: `Tick` carries the
    /// advance's elapsed time and every other stage its empty signal
    /// (issue 4470). A broadcast that dropped the elapsed time would leave
    /// every motion subscriber still.
    #[test]
    fn broadcast_sends_tick_with_its_elapsed_time_and_other_stages_empty() {
        let mut booted = boot_lifecycle(tick_graph());
        let (heard, motion) = mpsc::channel();
        booted.spawn_listener("motion", heard);
        for (correlation, subscription) in [
            (1, LifecycleSubscription::Tick(listener("motion").narrow())),
            (2, LifecycleSubscription::Shutdown(listener("motion").narrow())),
        ] {
            assert!(matches!(booted.subscribe(subscription, correlation), LifecycleSubscribeResult::Ok));
        }

        // Each advance's broadcast rides its root, so the listener has heard
        // it once the root settles; the cap replies on its own `Settled`
        // notice for that root, which the pump then drains.
        for correlation in [3, 4] {
            let root = booted.request(&LifecycleAdvance { delta_micros: 83_335 }, correlation);
            booted.driver.settle(&[root]);
            booted.driver.pump_until("the advance's settlement notice", |state| state.pending.is_none());
            booted.reply::<LifecycleAdvanceComplete>(correlation);
        }

        assert_eq!(
            motion.try_iter().collect::<Vec<_>>(),
            [Heard::Tick(Tick { delta_micros: 83_335 }), Heard::Shutdown],
            "Tick carries its time and the terminal stage broadcasts its signal"
        );
    }

    /// A `subscribe_self` from a non-`Component` source (an external
    /// session) replies `Err` and subscribes nothing — the reflexive
    /// form is gated to in-process actors by construction. A gate that
    /// admitted a sender with no local mailbox would hold a subscriber no
    /// broadcast can reach.
    #[test]
    fn subscribe_self_rejects_non_component_source() {
        let mut booted = boot_lifecycle(render_present_graph());

        let root = booted.request(&LifecycleSubscribeSelf { stage: Render::ID.0 }, 1);
        booted.driver.settle(&[root]);

        assert!(matches!(booted.reply(1), LifecycleSubscribeResult::Err(_)), "an external session is refused");
        assert!(booted.subscribers_of(Render::ID).is_empty(), "a non-Component source subscribes nothing");
    }

    /// Round trip through the host SDK path: the request the cap's
    /// `Publisher` impl builds for `Tick`, whose sender the host stamps. A
    /// caller whose `wire` sends it through the flat
    /// `ctx.send::<LifecycleCapability>` and whose published rows handle
    /// `Tick` silently lands in the `Tick` set; the same request stamped by a
    /// closure route, whose empty contract handles nothing, is refused with a
    /// reply naming it (ADR-0231 §4's guard cast). A cast that admitted any
    /// sender would fan `Tick` out to an actor with no handler for it.
    #[test]
    fn subscribe_request_via_flat_send_lands_a_handling_caller_and_refuses_a_closure_route() {
        let mut booted = boot_lifecycle(tick_graph());
        let (tx, replies) = mpsc::channel();
        let handler: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
            let captured = (dispatch.kind, dispatch.payload.bytes().to_vec());
            dispatch.discharge();
            let _ = tx.send(captured);
        });
        let closure = registered_ref(&booted.registry, "test.lifecycle.closure_caller", handler);

        // The closure route discharges its reply without settling it, so the
        // request's root never settles: the reply routes inline while the
        // driver drains the cap, and only that drain can deliver it.
        booted.driver.send_tracked(
            booted.driver.chassis().actor_ref::<LifecycleCapability>(),
            &<LifecycleCapability as Publisher>::subscribe_request::<Tick>(),
            Some(ReplyTarget::Actor { to: closure, correlation: 1 }),
        );
        let mut received = None;
        booted.driver.pump_until("the closure route's reply", |_| {
            received = received.take().or_else(|| replies.try_recv().ok());
            received.is_some()
        });
        let (kind, reply) = received.expect("the pump returned on the reply");

        assert_eq!(kind, <LifecycleSubscribeResult as Kind>::ID, "the closure route is answered");
        let Some(LifecycleSubscribeResult::Err(LifecycleSubscribeError::Stage { error, .. })) =
            LifecycleSubscribeResult::decode_from_bytes(&reply)
        else {
            panic!("a closure route has no Tick handler, so it cannot subscribe");
        };
        assert!(error.contains("test.lifecycle.closure_caller"), "the refusal names the sender: {error}");
        assert!(booted.subscribers_of(Tick::ID).is_empty(), "the refused sender subscribes nothing");

        let (caller_slot, _wake) =
            booted.driver.chassis().boot_pumped_actor::<Caller>((), ()).expect("the caller boots");
        let caller = booted.driver.chassis().actor_ref::<Caller>().erase();
        booted
            .driver
            .pump_until("the caller's wire subscribe", |state| !state.subscribers.subscribers_of(Tick::ID).is_empty());
        drop(caller_slot);

        assert_eq!(booted.subscribers_of(Tick::ID), [caller], "the caller lands in the Tick set");
    }
}
