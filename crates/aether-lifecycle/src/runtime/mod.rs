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
pub use super::subscribers::broadcast_to_subscribers;

// Handler-argument and reply kinds named by the moved `#[runtime] impl`
// bodies. Private to this module — the identity in the parent resolves the
// lifted `HandlesKind<K>` markers through its own `aether_kinds` imports.
use aether_actor::AnyActorRef;
use aether_actor::runtime;
use aether_kinds::trace::Settled;
use aether_kinds::{
    LifecycleAdvance, LifecycleSubscribe, LifecycleSubscribeResult, LifecycleSubscribeSelf, LifecycleUnsubscribe,
    LifecycleUnsubscribeAll, LifecycleUnsubscribeSelf, MonitorNotice, Quit, Tick,
};
use aether_substrate::actor::monitor::MonitorHandle;

pub use aether_actor::Manual;
pub use aether_actor::OutboundReply;
pub use aether_actor::root_mailbox;
pub use aether_data::{Kind, KindId, MailboxId as DataMailboxId};
pub use aether_kinds::LifecycleAdvanceComplete;
use aether_substrate::Erased;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::mail::mailer::Mailer;
pub use std::collections::{BTreeMap, BTreeSet};
pub use std::sync::Arc;
pub use std::time::{Duration, Instant};

/// Resolve the typed payload for one lifecycle stage. Keeping this seam pure
/// makes the Tick wire contract deterministic without coupling its test to
/// the runtime's settlement machinery.
fn stage_payload(stage: KindId, delta_micros: u32) -> Vec<u8> {
    if stage == Tick::ID {
        Tick { delta_micros }.encode_into_bytes()
    } else {
        Vec::new()
    }
}

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
    /// Subscriber table keyed by stage kind id (ADR-0082 §7). The rows are
    /// proven references (ADR-0230): a subscription is accepted only once
    /// something has answered that an actor is live at the id, so the
    /// fan-out never handles a position a caller computed.
    pub subscribers: BTreeMap<KindId, BTreeSet<AnyActorRef>>,
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
    /// `Arc<Mailer>` cached at init for `subscribe_settlement_mail`
    /// calls inside handlers.
    pub mailer: Arc<Mailer>,
    /// One monitor per subscriber (ADR-0079 §8 amended), registered on its
    /// first stage subscription and released when its `MonitorNotice`
    /// purges it. The handle's `Drop` deregisters, so the map is both the
    /// dedup guard and the RAII anchor. Keyed by the same proven reference
    /// the stage sets hold (ADR-0230), never by a position.
    pub monitors: BTreeMap<AnyActorRef, MonitorHandle>,
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
    pub fn watch<M: aether_actor::ReplyMode>(&mut self, ctx: &mut NativeCtx<'_, Erased, M>, subscriber: AnyActorRef) {
        if !self.monitors.contains_key(&subscriber)
            && let Ok(handle) = ctx.monitor(subscriber)
        {
            self.monitors.insert(subscriber, handle);
        }
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
/// data graph + a fresh mailer, built directly (no chassis boot),
/// with the supplied advance timeout. Reachable from
/// `mod settlement`'s descendant tests via module privacy.
#[cfg(test)]
fn test_cap(advance_timeout: Duration) -> LifecycleCapabilityState {
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
        monitors: BTreeMap::new(),
    }
}

/// A `Tick`→`Shutdown` graph fixture (the round-trip test wants
/// `Tick` as a declared stage, which [`test_cap`]'s Render-rooted
/// graph doesn't carry).
#[cfg(test)]
fn tick_start_graph_cap() -> LifecycleCapabilityState {
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
        monitors: BTreeMap::new(),
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
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<LifecycleCapabilityState, BootError> {
        let LifecycleConfig { advance_timeout_millis } = config;
        let LifecycleParams { graph } = params;
        let current_state = graph.start();
        let mailer = ctx.mailer();
        Ok(LifecycleCapabilityState {
            graph,
            subscribers: BTreeMap::new(),
            current_state,
            terminal_reached: false,
            quit_pending: false,
            pending: None,
            advance_timeout: Duration::from_millis(advance_timeout_millis),
            settlement_latency_ewma: None,
            last_slow_warn: None,
            mailer,
            monitors: BTreeMap::new(),
        })
    }

    /// Subscribe a mailbox to a lifecycle stage broadcast (ADR-0082
    /// §7). Replies with [`LifecycleSubscribeResult`] —
    /// `Err { stage, error }` when the stage isn't declared in this
    /// chassis's graph (fail-fast at wire time).
    ///
    /// # Agent
    /// `LifecycleSubscribe { stage, mailbox }`. Stage must be a kind
    /// id registered as a state or terminal in the lifecycle graph, and
    /// `mailbox` must name an actor that is live right now: an unknown or
    /// already-dropped id replies `Err` rather than registering a
    /// subscription whose broadcasts could never land.
    #[handler::single]
    fn on_subscribe(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: LifecycleSubscribe,
    ) -> LifecycleSubscribeResult {
        let stage_kind = KindId(payload.stage);
        let known = state.graph.state(stage_kind).is_some() || state.graph.is_terminal(stage_kind);
        if !known {
            return LifecycleSubscribeResult::Err {
                stage: payload.stage,
                error: format!("stage {stage_kind:?} is not declared by this chassis's lifecycle graph"),
            };
        }

        // The payload's id is a position a caller computed (ADR-0230), so it
        // is proven once here, at receipt, and the table keeps the proof.
        let subscriber = match ctx.resolve_live(DataMailboxId(payload.mailbox)) {
            Ok(subscriber) => subscriber,
            Err(error) => {
                return LifecycleSubscribeResult::Err { stage: payload.stage, error: error.to_string() };
            }
        };

        state.subscribers.entry(stage_kind).or_default().insert(subscriber);
        state.watch(ctx, subscriber);
        LifecycleSubscribeResult::Ok
    }

    /// Subscribe the *sending* actor to a lifecycle stage broadcast
    /// (ADR-0082 §7, ADR-0083). Resolves the subscriber from the
    /// inbound envelope's host-stamped `Source` via
    /// [`sender`](NativeCtx::sender) rather than a
    /// caller-supplied mailbox, so the subscriber cannot be forged and
    /// needs no registry read — the host already answered who sent this.
    /// `None` means the sender has no local mailbox (an external
    /// session or another engine) — reply `Err` and subscribe
    /// nothing, which gates the reflexive form to in-process actors
    /// by construction. Reuses [`Self::on_subscribe`]'s insert path
    /// once the sender is in hand.
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
        match ctx.sender() {
            None => LifecycleSubscribeResult::Err {
                stage: payload.stage,
                error: "aether.lifecycle.subscribe_self requires a local component sender; \
                            an external session or remote engine must use \
                            aether.lifecycle.subscribe with an explicit mailbox"
                    .to_string(),
            },
            Some(subscriber) => {
                let known = state.graph.state(stage_kind).is_some() || state.graph.is_terminal(stage_kind);
                if known {
                    state.subscribers.entry(stage_kind).or_default().insert(subscriber);
                    state.watch(ctx, subscriber);
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
        }
    }

    /// Unsubscribe a mailbox from a lifecycle stage broadcast.
    /// Idempotent on "not currently subscribed."
    ///
    /// Takes a position, not a proof: a removal sends nothing, and a
    /// subscriber that has already departed must stay removable — demanding
    /// a proof here would refuse exactly the request this kind is for. The
    /// position is compared against the references the table holds.
    ///
    /// # Agent
    /// `LifecycleUnsubscribe { stage, mailbox }`.
    #[handler::single]
    fn on_unsubscribe(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        payload: LifecycleUnsubscribe,
    ) -> LifecycleSubscribeResult {
        let stage_kind = KindId(payload.stage);
        let mailbox = DataMailboxId(payload.mailbox);
        let known = state.graph.state(stage_kind).is_some() || state.graph.is_terminal(stage_kind);
        if known {
            if let Some(set) = state.subscribers.get_mut(&stage_kind) {
                set.retain(|reference| reference.id() != mailbox);
            }
            LifecycleSubscribeResult::Ok
        } else {
            LifecycleSubscribeResult::Err {
                stage: payload.stage,
                error: format!("stage {stage_kind:?} is not declared by this chassis's lifecycle graph"),
            }
        }
    }

    /// Unsubscribe the *sending* actor from a lifecycle stage
    /// broadcast (ADR-0082 §7, ADR-0083). Resolves the subscriber
    /// from the inbound envelope's host-stamped `Source` via
    /// [`sender`](NativeCtx::sender), mirroring
    /// [`Self::on_subscribe_self`]. `None` (no local sender) replies
    /// `Err`. Idempotent on "not currently subscribed."
    ///
    /// The exact removal of the four: it holds a reference rather than a
    /// position, and `AnyActorRef`'s `Eq` is id equality, so the set lookup
    /// is `O(log n)` instead of the scan the wire-borne forms pay.
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
        match ctx.sender() {
            None => LifecycleSubscribeResult::Err {
                stage: payload.stage,
                error: "aether.lifecycle.unsubscribe_self requires a local component sender; \
                            an external session or remote engine must use \
                            aether.lifecycle.unsubscribe with an explicit mailbox"
                    .to_string(),
            },
            Some(subscriber) => {
                let known = state.graph.state(stage_kind).is_some() || state.graph.is_terminal(stage_kind);
                if known {
                    if let Some(set) = state.subscribers.get_mut(&stage_kind) {
                        set.remove(&subscriber);
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
        }
    }

    /// Remove `mailbox` from every lifecycle stage's subscriber set in
    /// one shot — the lifecycle-family counterpart of the window
    /// family's `aether.window.unsubscribe_all`, the other half of the
    /// subscription surface now that no input capability exists.
    /// The externally sendable bulk form; drop-time cleanup happens
    /// through [`Self::on_monitor_notice`] instead, so nothing mails
    /// this on the component path anymore. No mailbox-validation: the
    /// target may already be torn down; we accept any id and purge it
    /// from every stage. No reply.
    ///
    /// # Agent
    /// `LifecycleUnsubscribeAll { mailbox }`. Idempotent.
    #[handler::single]
    fn on_unsubscribe_all(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, payload: LifecycleUnsubscribeAll) {
        let mailbox = DataMailboxId(payload.mailbox);
        for set in state.subscribers.values_mut() {
            set.retain(|reference| reference.id() != mailbox);
        }
    }

    /// Purge a departed mailbox (ADR-0079 §8 amended). The substrate
    /// fires one notice per [`LifecycleCapabilityState::watch`]ed
    /// mailbox when it vacates (the wasm trampoline on
    /// `DropComponent`) or closes, so a dropped component's stage
    /// broadcasts stop landing at its mailbox without any drop-time
    /// fan-out from the component host. Releasing the handle keeps the
    /// monitor map bounded by live subscribers; a later occupant of
    /// the same mailbox re-registers through its own subscribe.
    ///
    /// The notice names the departed *position* while both tables hold
    /// references, so this compares rather than looks up: the actor is gone,
    /// which is precisely the state no proof of it can describe.
    #[handler::single]
    fn on_monitor_notice(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, notice: MonitorNotice) {
        state.monitors.retain(|reference, _| reference.id() != notice.target);
        for set in state.subscribers.values_mut() {
            set.retain(|reference| reference.id() != notice.target);
        }
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
    fn on_advance(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Manual>, payload: LifecycleAdvance) {
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
                tracing::warn!(
                    target: "aether_lifecycle",
                    current = ?state.current_state,
                    pending_root = ?pending.root,
                    pending_for_millis = pending.started.elapsed().as_millis(),
                    stuck_stage = %pending.completed_kind,
                    fanout = ?state.subscribers.get(&pending.completed_kind),
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
        let stage_payload = stage_payload(broadcast, payload.delta_micros);
        broadcast_to_subscribers(ctx, &state.subscribers, broadcast, &stage_payload);

        // Subscribe settlement on the inbound's chain root. The
        // broadcast subtree is part of that chain; settlement fires
        // once the inbound's `Finished` event drops the in-flight
        // count to zero (which includes every fan-out descendant).
        let root = ctx.in_flight_root();
        let reply_to = ctx.reply_target();
        if let Some(registry) = state.mailer.settlement_registry() {
            registry.subscribe_settlement_mail(
                root,
                // The cap subscribes settlement against its own mailbox.
                root_mailbox::<Self>(),
                <Settled as Kind>::ID,
                Arc::clone(&state.mailer),
            );
            state.pending = Some(PendingAdvance {
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
    use super::*;
    use aether_kinds::{Present, Render, Tick};

    #[test]
    fn tick_payload_carries_elapsed_time_while_other_stages_stay_empty() {
        let bytes = stage_payload(Tick::ID, 83_335);
        assert_eq!(Tick::decode_from_bytes(&bytes), Some(Tick { delta_micros: 83_335 }));
        assert!(stage_payload(Render::ID, 83_335).is_empty());
    }

    /// Seed one stage set through the reflexive path — the only door a test
    /// has now that the table holds proofs and nothing outside
    /// `aether-substrate` can mint one.
    #[cfg(test)]
    fn subscribe_self_from(cap: &mut LifecycleCapabilityState, subscriber: DataMailboxId, stage: KindId) {
        use aether_substrate::actor::native::binding::NativeBinding;
        use aether_substrate::mail::{MailId, MailboxId, Source, SourceAddr};

        let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&cap.mailer), MailboxId(0)));
        let source = Source::to(SourceAddr::Component(MailboxId(subscriber.0)));
        let mut ctx = NativeCtx::new(&transport, source, MailId::NONE, MailId::NONE);
        LifecycleCapability::on_subscribe_self(cap, &mut ctx, LifecycleSubscribeSelf { stage: stage.0 });
    }

    #[cfg(test)]
    fn subscribed(cap: &LifecycleCapabilityState, stage: KindId, subscriber: DataMailboxId) -> bool {
        cap.subscribers.get(&stage).is_some_and(|set| set.iter().any(|r| r.id() == subscriber))
    }

    #[test]
    fn on_unsubscribe_all_purges_mailbox_from_every_stage() {
        // A dropped trampoline's mailbox must leave every stage's
        // subscriber set in one shot (the drop-cleanup contract,
        // mirroring the window family's `aether.window.unsubscribe_all`),
        // while co-subscribers on a shared stage survive. The bulk purge
        // now matches a wire position against the references the table
        // holds, which is the predicate this pins.
        use aether_substrate::actor::native::binding::NativeBinding;
        use aether_substrate::mail::{MailId, MailboxId, Source};

        let mut cap = test_cap(Duration::from_millis(ADVANCE_TIMEOUT_MS_DEFAULT));
        let dropped = DataMailboxId(0xDEAD);
        let survivor = DataMailboxId(0xBEEF);
        let render = <Render as Kind>::ID;
        let present = <Present as Kind>::ID;
        subscribe_self_from(&mut cap, dropped, render);
        subscribe_self_from(&mut cap, survivor, render);
        subscribe_self_from(&mut cap, dropped, present);

        let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&cap.mailer), MailboxId(0)));
        let mut ctx = NativeCtx::new(&transport, Source::NONE, MailId::NONE, MailId::NONE);
        LifecycleCapability::on_unsubscribe_all(&mut cap, &mut ctx, LifecycleUnsubscribeAll { mailbox: dropped.0 });

        assert!(!subscribed(&cap, render, dropped), "dropped mailbox must leave the Render stage");
        assert!(!subscribed(&cap, present, dropped), "dropped mailbox must leave the Present stage");
        assert!(subscribed(&cap, render, survivor), "co-subscribers on a shared stage must survive the purge");
    }

    /// An explicit `subscribe` proves its payload-borne mailbox once, at
    /// receipt (ADR-0230): a live id lands a reference in the stage set,
    /// while a dropped or never-registered one replies `Err` and leaves the
    /// table alone rather than registering a subscription whose broadcasts
    /// could never land. The two refusals stay the registry's own distinct
    /// renderings, so a caller can tell a component that unloaded from an id
    /// it guessed.
    #[test]
    fn explicit_subscribe_proves_the_mailbox_and_refuses_an_unproven_one() {
        use aether_substrate::actor::native::binding::NativeBinding;
        use aether_substrate::mail::registry::noop_handler;
        use aether_substrate::mail::{MailId, MailboxId, Source};
        use aether_substrate::testing::{boot_authority, fresh_substrate};

        let (registry, mailer) = fresh_substrate();
        let authority = boot_authority();
        let live = registry.register_inbox(&authority, "test.lifecycle.live", noop_handler());
        let gone = registry.register_inbox(&authority, "test.lifecycle.gone", noop_handler());
        registry.drop_mailbox(&authority, gone).expect("test setup: the second inbox drops");
        let never = MailboxId(0xDEAD_BEEF);

        let mut cap = test_cap(Duration::from_millis(ADVANCE_TIMEOUT_MS_DEFAULT));
        let render = <Render as Kind>::ID;
        let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
        let mut subscribe = |mailbox: MailboxId| {
            let mut ctx = NativeCtx::new(&transport, Source::NONE, MailId::NONE, MailId::NONE);
            LifecycleCapability::on_subscribe(
                &mut cap,
                &mut ctx,
                LifecycleSubscribe { stage: render.0, mailbox: mailbox.0 },
            )
        };

        assert!(matches!(subscribe(live), LifecycleSubscribeResult::Ok), "a live mailbox proves and subscribes");
        let dropped_reply = subscribe(gone);
        let unknown_reply = subscribe(never);

        assert!(subscribed(&cap, render, DataMailboxId(live.0)), "the proven subscriber holds the live id");
        assert!(!subscribed(&cap, render, DataMailboxId(gone.0)), "a dropped mailbox is not subscribed");
        assert!(!subscribed(&cap, render, DataMailboxId(never.0)), "an unregistered mailbox is not subscribed");
        assert_eq!(cap.subscribers[&render].len(), 1, "only the proven subscriber reached the stage set");

        let LifecycleSubscribeResult::Err { error: dropped_error, .. } = dropped_reply else {
            panic!("a dropped mailbox replies Err");
        };
        let LifecycleSubscribeResult::Err { error: unknown_error, .. } = unknown_reply else {
            panic!("an unregistered mailbox replies Err");
        };
        assert!(dropped_error.contains("already dropped"), "the dropped refusal reads as the registry writes it");
        assert!(
            unknown_error.contains("unknown mailbox id"),
            "the unknown refusal stays distinct from the dropped one"
        );
    }

    /// A `subscribe_self` carrying a `Component` source lands *that*
    /// mailbox in the stage set (ADR-0083: the cap reads the
    /// subscriber off the host-stamped envelope, not a payload field).
    #[test]
    fn subscribe_self_subscribes_the_component_source() {
        use aether_substrate::actor::native::binding::NativeBinding;
        use aether_substrate::mail::{MailId, MailboxId, Source, SourceAddr};

        let mut cap = test_cap(Duration::from_millis(ADVANCE_TIMEOUT_MS_DEFAULT));
        let render = <Render as Kind>::ID;
        let sender = DataMailboxId(0x00C0_FFEE);

        let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&cap.mailer), MailboxId(0)));
        let source = Source::to(SourceAddr::Component(MailboxId(sender.0)));
        let mut ctx = NativeCtx::new(&transport, source, MailId::NONE, MailId::NONE);
        LifecycleCapability::on_subscribe_self(&mut cap, &mut ctx, LifecycleSubscribeSelf { stage: render.0 });

        assert!(
            subscribed(&cap, render, sender),
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
        use aether_substrate::mail::{MailId, MailboxId, Source, SourceAddr};

        let mut cap = test_cap(Duration::from_millis(ADVANCE_TIMEOUT_MS_DEFAULT));
        let render = <Render as Kind>::ID;

        let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&cap.mailer), MailboxId(0)));
        let source = Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(0xFEED))));
        let mut ctx = NativeCtx::new(&transport, source, MailId::NONE, MailId::NONE);
        LifecycleCapability::on_subscribe_self(&mut cap, &mut ctx, LifecycleSubscribeSelf { stage: render.0 });

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
        use aether_substrate::mail::registry::{InboxHandler, OwnedDispatch};
        use aether_substrate::mail::{MailId, MailboxId, Source, SourceAddr};

        use crate::LifecycleMailboxExt;
        use aether_substrate::testing::{boot_authority, fresh_substrate};

        let (registry, mailer) = fresh_substrate();

        // Capturing sink at the lifecycle mailbox: records the single
        // mail the SDK `subscribe::<Tick>()` emits so the test can read
        // back the kind, the host-stamped `Source`, and the payload.
        let (tx, rx) = mpsc::channel::<(KindId, Source, Vec<u8>)>();
        let handler: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
            let captured = (dispatch.kind, dispatch.sender, dispatch.payload.bytes().to_vec());
            dispatch.discharge();
            let _ = tx.send(captured);
        });
        let lifecycle_id = registry.register_inbox(
            &boot_authority(),
            <LifecycleCapability as aether_actor::Addressable>::NAMESPACE,
            handler,
        );

        // The calling actor: a transport stamped with SENDER as its
        // self-mailbox, so its sends carry `Source::Component(SENDER)`.
        let sender = DataMailboxId(0x00C0_FFEE);
        let tx_binding = NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(sender.0));
        let lifecycle = NativeActorMailbox::<LifecycleCapability>::__new(lifecycle_id.0, &tx_binding);
        lifecycle.subscribe::<Tick>();
        tx_binding.flush_outbound();

        let (kind, source, bytes) = rx.try_recv().expect("subscribe::<Tick>() emitted one mail");
        assert_eq!(kind, <LifecycleSubscribeSelf as Kind>::ID, "the SDK self-subscribe sends LifecycleSubscribeSelf");
        assert_eq!(
            source.addr,
            SourceAddr::Component(MailboxId(sender.0)),
            "the host stamps the calling actor as the Source"
        );
        let decoded =
            LifecycleSubscribeSelf::decode_from_bytes(&bytes).expect("payload decodes as LifecycleSubscribeSelf");
        assert_eq!(decoded.stage, <Tick as Kind>::ID.0, "the payload carries the Tick stage id");

        // Deliver the captured mail to the cap exactly as the
        // dispatcher would, and confirm the calling actor is now in the
        // Tick stage set.
        let mut cap = tick_start_graph_cap();
        let cap_transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&cap.mailer), MailboxId(0)));
        let mut ctx = NativeCtx::new(&cap_transport, source, MailId::NONE, MailId::NONE);
        LifecycleCapability::on_subscribe_self(&mut cap, &mut ctx, decoded);

        assert!(subscribed(&cap, <Tick as Kind>::ID, sender), "the calling actor lands in the Tick stage set");
    }
}
