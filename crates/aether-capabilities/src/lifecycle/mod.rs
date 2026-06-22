//! `aether.lifecycle` cap (ADR-0082). The bridged, non-generic
//! capability the chassis drives one frame at a time.
//!
//! The chassis owns cadence: it sends [`LifecycleAdvance`] once per
//! frame. The cap owns everything else — the lifecycle graph (a data
//! graph of `{ stage_kind, next, optional quit }` edges, in
//! `mod graph`), the subscriber table keyed by stage kind and
//! the fan-out (the sender side + `broadcast_to_subscribers` in
//! `mod subscribers`), and the settlement gating (the
//! advance state machine in `mod settlement`). Because it
//! is `#[bridge(singleton)]`d like
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
// Reply types ride only the native handler bodies (via `super::`), so they
// elide on wasm where `mod native` is compiled out.
#[cfg(not(target_arch = "wasm32"))]
use aether_kinds::{LifecycleAdvanceComplete, LifecycleSubscribeResult};

mod graph;
pub(crate) use graph::LifecycleStateData;
pub use graph::{
    BuildError, LifecycleGraphBuilder, LifecycleGraphData, NoOpen, OpenNoNext, OpenWithNext,
};

mod subscribers;
pub use subscribers::LifecycleMailboxExt;

#[cfg(not(target_arch = "wasm32"))]
mod settlement;

#[cfg(not(target_arch = "wasm32"))]
pub use native::LifecycleConfig;

#[aether_actor::bridge(singleton)]
mod native {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use aether_actor::actor::ctx::OutboundReply;
    use aether_actor::{Manual, actor};
    use aether_data::{Kind, KindId, MailboxId as DataMailboxId, mailbox_id_from_name};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::mailer::Mailer;

    use super::settlement::{ADVANCE_TIMEOUT_MS_DEFAULT, PendingAdvance, Step, resolve_edge};
    use super::subscribers::broadcast_to_subscribers;
    use super::{
        LifecycleAdvance, LifecycleAdvanceComplete, LifecycleGraphData, LifecycleSubscribe,
        LifecycleSubscribeResult, LifecycleSubscribeSelf, LifecycleUnsubscribe,
        LifecycleUnsubscribeAll, LifecycleUnsubscribeSelf, Quit, Settled,
    };

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
        /// Force-complete deadline for a pending advance's `Settled`
        /// (iamacoffeepot/aether#1048), in milliseconds. Resolved
        /// chassis-side (env override over [`Self::ADVANCE_TIMEOUT_MS_DEFAULT`])
        /// rather than read from the environment in `init`, so the cap
        /// configures through this struct rather than a naked env read.
        pub advance_timeout_millis: u64,
    }

    impl LifecycleConfig {
        /// Default force-complete deadline (ms) for a pending advance.
        /// Chassis builders that don't override use this.
        pub const ADVANCE_TIMEOUT_MS_DEFAULT: u64 = ADVANCE_TIMEOUT_MS_DEFAULT;
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
    ///
    /// Fields are `pub(crate)` so the settlement state machine
    /// (`mod settlement`) can carry its inherent-impl
    /// cluster in a sibling file without nesting under `native`.
    pub struct LifecycleCapability {
        pub(crate) graph: LifecycleGraphData,
        /// Subscriber table keyed by stage kind id (ADR-0082 §7).
        pub(crate) subscribers: BTreeMap<KindId, BTreeSet<DataMailboxId>>,
        /// Kind id of the state the cap will broadcast on the next
        /// [`LifecycleAdvance`]. Starts at `graph.start()`; mutated after
        /// each settled advance to the resolved next/quit edge target.
        pub(crate) current_state: KindId,
        /// True once the lifecycle reached a terminal — further advances
        /// are no-ops.
        pub(crate) terminal_reached: bool,
        /// Quit flag (ADR-0082 §3). Set by inbound [`Quit`] mail;
        /// consumed at the next state whose graph declares a `quit` edge.
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

    #[actor]
    impl NativeActor for LifecycleCapability {
        type Config = LifecycleConfig;
        const NAMESPACE: &'static str = "aether.lifecycle";

        fn init(config: LifecycleConfig, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let LifecycleConfig {
                graph,
                initial_subscribers,
                advance_timeout_millis,
            } = config;
            let current_state = graph.start();
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
        fn on_subscribe(
            &mut self,
            _ctx: &mut NativeCtx<'_>,
            payload: LifecycleSubscribe,
        ) -> LifecycleSubscribeResult {
            let stage_kind = KindId(payload.stage);
            let mailbox = DataMailboxId(payload.mailbox);
            let known =
                self.graph.state(stage_kind).is_some() || self.graph.is_terminal(stage_kind);
            if known {
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
            }
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
        fn on_subscribe_self(
            &mut self,
            ctx: &mut NativeCtx<'_>,
            payload: LifecycleSubscribeSelf,
        ) -> LifecycleSubscribeResult {
            let stage_kind = KindId(payload.stage);
            match ctx.source_mailbox() {
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
            }
        }

        /// Unsubscribe a mailbox from a lifecycle stage broadcast.
        /// Idempotent on "not currently subscribed."
        ///
        /// # Agent
        /// `LifecycleUnsubscribe { stage, mailbox }`.
        #[handler]
        fn on_unsubscribe(
            &mut self,
            _ctx: &mut NativeCtx<'_>,
            payload: LifecycleUnsubscribe,
        ) -> LifecycleSubscribeResult {
            let stage_kind = KindId(payload.stage);
            let mailbox = DataMailboxId(payload.mailbox);
            let known =
                self.graph.state(stage_kind).is_some() || self.graph.is_terminal(stage_kind);
            if known {
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
            }
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
        ) -> LifecycleSubscribeResult {
            let stage_kind = KindId(payload.stage);
            match ctx.source_mailbox() {
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
            }
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
        #[handler::manual]
        fn on_advance(&mut self, ctx: &mut NativeCtx<'_, Manual>, _payload: LifecycleAdvance) {
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
                    mailbox_id_from_name(<Self as aether_actor::Addressable>::NAMESPACE),
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
        #[handler::manual]
        fn on_settled(&mut self, ctx: &mut NativeCtx<'_, Manual>, payload: Settled) {
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

    /// Construction-level cap fixture: a Render→Present→Shutdown
    /// data graph + a fresh mailer, built directly (no chassis boot),
    /// with the supplied advance timeout. Shared with
    /// `mod settlement`'s tests via `pub(crate)`.
    #[cfg(test)]
    pub fn test_cap(advance_timeout: Duration) -> LifecycleCapability {
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

    /// A `Tick`→`Shutdown` graph fixture (the round-trip test wants
    /// `Tick` as a declared stage, which [`test_cap`]'s Render-rooted
    /// graph doesn't carry).
    #[cfg(test)]
    pub fn tick_start_graph_cap() -> LifecycleCapability {
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

    #[cfg(test)]
    mod tests {
        use super::*;
        use aether_kinds::{Present, Render, Tick};

        #[test]
        fn cap_initial_state_is_graph_start() {
            let cap = test_cap(Duration::from_millis(ADVANCE_TIMEOUT_MS_DEFAULT));
            assert_eq!(cap.current_state(), <Render as Kind>::ID);
            assert!(!cap.is_terminal());
            assert!(!cap.quit_pending());
        }

        #[test]
        fn on_unsubscribe_all_purges_mailbox_from_every_stage() {
            // A dropped trampoline's mailbox must leave every stage's
            // subscriber set in one shot (the drop-cleanup contract,
            // mirroring `InputCapability::on_unsubscribe_all`), while
            // co-subscribers on a shared stage survive.
            use aether_substrate::actor::native::binding::NativeBinding;
            use aether_substrate::mail::{MailId, MailboxId, Source};

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
            use aether_substrate::mail::{MailId, MailboxId, Source, SourceAddr};

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
            use aether_substrate::mail::registry::{InboxHandler, OwnedDispatch};
            use aether_substrate::mail::{MailId, MailboxId, Source, SourceAddr};

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
                <LifecycleCapability as aether_actor::Addressable>::NAMESPACE,
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
}
