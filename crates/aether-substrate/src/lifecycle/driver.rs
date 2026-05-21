//! [`LifecycleDriverCapability`] — chassis-driven lifecycle actor
//! (ADR-0082 §2).
//!
//! The driver is a `NativeActor` registered at `aether.lifecycle`. It
//! holds the compiled [`LifecycleGraph`] plus a shared chassis context
//! `C` and a subscriber table. On each [`LifecycleAdvance`] mail
//! received from the chassis, the driver:
//!
//! 1. Mints the current state's payload by calling the state's factory
//!    with `&C`.
//! 2. Broadcasts the encoded bytes to every subscriber registered for
//!    the current state's kind.
//! 3. Advances the internal state pointer along the resolved edge —
//!    `quit` if `quit_pending` is set and the state declares a quit
//!    edge (consuming the flag), otherwise `next`.
//!
//! `quit_pending` is set by inbound [`Quit`] mail and persists across
//! states with no declared quit edge — only states that declare
//! `.quit::<K>()` consume the flag.
//!
//! **Settlement gating (ADR-0082 §6).** `on_advance` broadcasts to
//! subscribers, captures the inbound chain root, and subscribes the
//! settlement registry's mail notice for that root. State pointer
//! mutation is deferred to `on_settled`, where the driver also
//! replies [`LifecycleAdvanceComplete`] to the chassis main loop
//! that issued the [`LifecycleAdvance`] — chassis loops wait-reply
//! on it, so cadence couples to actual subscriber drain time. When
//! no settlement registry is wired (test harness, future chassis
//! without trace pipeline) the driver falls back to fire-and-advance.
//! Subscribe / unsubscribe return [`LifecycleSubscribeResult`] so
//! callers learn fail-fast about unsupported stages per §7.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_actor::{OutboundReply, actor};
use aether_data::{Kind, KindId, MailboxId as DataMailboxId, mailbox_id_from_name};
use aether_kinds::trace::Settled;
use aether_kinds::{
    LifecycleAdvance, LifecycleAdvanceComplete, LifecycleSubscribe, LifecycleSubscribeResult,
    LifecycleUnsubscribe, Quit,
};

use super::graph::{LifecycleGraph, LifecycleState};
use crate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use crate::chassis::error::BootError;
use crate::mail::mailer::Mailer;
use crate::mail::{MailId, MailboxId, ReplyTo};

/// Internal state-advance decision produced by `on_advance` before the
/// driver mutates its own fields. Declared at module scope to keep the
/// handler body statement-only (`clippy::items_after_statements`).
enum Step {
    StateAdvance {
        bytes: Vec<u8>,
        broadcast: KindId,
        next: KindId,
    },
    Terminal {
        bytes: Vec<u8>,
        broadcast: KindId,
    },
    Unknown,
}

/// Default deadline for a pending advance's `Settled` to arrive
/// before [`LifecycleDriverCapability::on_advance`] force-completes it
/// (iamacoffeepot/aether#1048). Override via
/// `AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS`. Generous relative to the
/// ~16 ms frame tick: normal settlement is sub-tick, so this only
/// fires when the settlement pipeline has actually stalled — degrading
/// a permanent wedge into a visible stutter rather than tripping on
/// ordinary jitter.
const ADVANCE_TIMEOUT_MS_DEFAULT: u64 = 1_000;

/// Construction-time configuration for [`LifecycleDriverCapability`].
/// Carries the compiled graph + initial chassis context. Constructed
/// at chassis-builder time and consumed by the actor's `init`.
pub struct LifecycleDriverConfig<C> {
    /// The compiled lifecycle graph. Built via
    /// [`LifecycleGraph::builder()`] on the chassis side.
    pub graph: LifecycleGraph<C>,
    /// The initial chassis context. Owned by the driver actor for the
    /// lifetime of the chassis; factories read it via `&C` each
    /// advance. Typically holds `Arc`-shared chassis state (frame
    /// timer, window handle, render queue) — see the `'static` bound
    /// requirement on `C`.
    pub context: C,
    /// Initial `(stage_kind, mailbox)` pairs to populate the
    /// subscriber table at boot. Chassis builders use this to wire
    /// the relay (e.g. `(Tick::ID, aether.input)`) without round-
    /// tripping a `LifecycleSubscribe` mail through the dispatcher.
    /// Each pair must reference a stage kind declared by `graph` —
    /// the boot path verifies this and returns `BootError` otherwise,
    /// so misconfiguration fails fast at chassis-build rather than
    /// silently dropping mail at runtime.
    pub initial_subscribers: Vec<(KindId, DataMailboxId)>,
}

/// The `aether.lifecycle` capability — ADR-0082's first-class actor
/// that drives the chassis lifecycle. Generic over chassis context
/// `C` so each chassis defines its own context shape; the driver is
/// concrete-per-chassis (`LifecycleDriverCapability<DesktopCtx>`,
/// `LifecycleDriverCapability<HeadlessCtx>`, etc.) once the chassis
/// migration in PR 3 lands.
///
/// Plain-field shape (ADR-0078): every handler runs on the cap's
/// single dispatcher thread, so no `Mutex`/`Arc<Atomic*>` is needed
/// for the subscriber table or state pointer.
pub struct LifecycleDriverCapability<C: 'static + Send + Sync> {
    graph: LifecycleGraph<C>,
    context: C,
    /// Subscriber table keyed by stage kind id (ADR-0082 §7).
    subscribers: BTreeMap<KindId, BTreeSet<DataMailboxId>>,
    /// The kind id of the state the driver will broadcast on the next
    /// [`LifecycleAdvance`]. Starts at `graph.start()`; mutated after each
    /// advance to the resolved next/quit edge target.
    current_state: KindId,
    /// True once the lifecycle reached a terminal — the next
    /// [`LifecycleAdvance`] broadcasts the terminal's payload and the driver
    /// then no-ops on every subsequent advance.
    terminal_reached: bool,
    /// Quit flag (ADR-0082 §3). Set by inbound [`Quit`] mail; consumed
    /// at the next state whose graph declares a `quit` edge.
    quit_pending: bool,
    /// In-flight advance awaiting settlement (ADR-0082 §6). Set in
    /// `on_advance` after the broadcast subscribes settlement on the
    /// chain root; consumed in `on_settled` when the matching
    /// `Settled` mail arrives. While `Some`, additional inbound
    /// `LifecycleAdvance` mails warn-and-drop — the chassis main loop
    /// `wait_reply`s on `LifecycleAdvanceComplete` so duplicates only
    /// happen on broken cadence sources.
    pending: Option<PendingAdvance>,
    /// Deadline for a pending advance's `Settled` (ADR-0082 §6 couples
    /// the lifecycle to the trace pipeline). When a pending advance
    /// exceeds this without settling, the next inbound advance
    /// force-completes it — defense-in-depth against a saturated
    /// settlement pipeline wedging the lifecycle permanently
    /// (iamacoffeepot/aether#1048). Set from `AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS`.
    advance_timeout: Duration,
    /// `Arc<Mailer>` cached at init for `subscribe_settlement_mail`
    /// calls inside handlers (which only have `&mut self` + `&mut
    /// NativeCtx`, not a way to clone the mailer cheaply otherwise).
    mailer: Arc<Mailer>,
    /// Marker so the unused `C` type parameter is retained even when
    /// the only direct use of `C` is through the graph's factories.
    _marker: PhantomData<fn() -> C>,
}

/// Per-advance state tracked across `on_advance` → `on_settled`. Holds
/// the chain root for matching the inbound `Settled` mail and the
/// reply target / payload fields needed to emit
/// [`LifecycleAdvanceComplete`] once settlement fires.
struct PendingAdvance {
    /// Causal-chain root of the in-flight broadcast (ADR-0080 §6).
    /// Compared against the [`Settled`] payload's `root` to confirm
    /// the inbound notice corresponds to *this* advance rather than a
    /// stale subscription from a prior root that hasn't been GC'd.
    root: MailId,
    /// Kind id of the state the driver just broadcast — echoed in the
    /// `completed` field of [`LifecycleAdvanceComplete`].
    completed_kind: KindId,
    /// Kind id of the state the driver will broadcast on the *next*
    /// advance — echoed in the `next` field. `KindId(0)` when the
    /// settling broadcast was a terminal (`terminal_reached` flips
    /// alongside).
    next_kind: KindId,
    /// True if the settling broadcast is a terminal state.
    is_terminal: bool,
    /// Original chassis sender of the [`LifecycleAdvance`] mail.
    /// `LifecycleAdvanceComplete` reply target.
    reply_to: ReplyTo,
    /// When this advance was issued. Drives the `advance_timeout`
    /// force-complete fallback (iamacoffeepot/aether#1048).
    started: Instant,
}

#[actor]
impl<C: 'static + Send + Sync> NativeActor for LifecycleDriverCapability<C> {
    type Config = LifecycleDriverConfig<C>;
    const NAMESPACE: &'static str = "aether.lifecycle";

    fn init(
        config: LifecycleDriverConfig<C>,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<Self, BootError> {
        let LifecycleDriverConfig {
            graph,
            context,
            initial_subscribers,
        } = config;
        let current_state = graph.start();
        let advance_timeout_ms = env::var("AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(ADVANCE_TIMEOUT_MS_DEFAULT);
        let mailer = ctx.mailer();
        let mut subscribers: BTreeMap<KindId, BTreeSet<DataMailboxId>> = BTreeMap::new();
        for (stage, mailbox) in initial_subscribers {
            // Reject unknown-stage subscriptions at boot rather than
            // silently dropping mail at runtime — ADR-0082 §7's
            // fail-fast contract applies to compile-site config too,
            // not just LifecycleSubscribe mail.
            if graph.state(stage).is_none() && graph.terminal(stage).is_none() {
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
            context,
            subscribers,
            current_state,
            terminal_reached: false,
            quit_pending: false,
            pending: None,
            advance_timeout: Duration::from_millis(advance_timeout_ms),
            mailer,
            _marker: PhantomData,
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
            self.graph.state(stage_kind).is_some() || self.graph.terminal(stage_kind).is_some();
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
            self.graph.state(stage_kind).is_some() || self.graph.terminal(stage_kind).is_some();
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

    /// Lifecycle escape signal (ADR-0082 §3). Sets `quit_pending = true`;
    /// the next state in the graph that declares a `quit` edge will
    /// consume the flag.
    ///
    /// # Agent
    /// `Quit {}`. Sent by chassis bridges from ctrlc / winit
    /// `WindowEvent::CloseRequested` / future hub-shutdown mail.
    #[handler]
    fn on_quit(&mut self, _ctx: &mut NativeCtx<'_>, _payload: Quit) {
        self.quit_pending = true;
    }

    /// Drive the lifecycle one step (ADR-0082 §2). Broadcast the
    /// current state's payload to every subscriber registered for
    /// that stage, subscribe settlement on the broadcast root, and
    /// stash a [`PendingAdvance`] until [`Settled`] arrives. The
    /// state pointer mutates *in* [`Self::on_settled`], not here,
    /// so a chassis that overruns its cadence and sends two
    /// `LifecycleAdvance` mails in close succession sees the second
    /// warn-drop rather than skipping ahead through unsettled
    /// states.
    ///
    /// # Agent
    /// `LifecycleAdvance {}`. Sent by the chassis main loop each
    /// frame (winit redraw, headless std-timer, etc.). Reply:
    /// [`LifecycleAdvanceComplete`] once the broadcast root settles.
    #[handler]
    fn on_advance(&mut self, ctx: &mut NativeCtx<'_>, _payload: LifecycleAdvance) {
        if self.terminal_reached {
            // Already done — reply immediately with zeros so the
            // chassis main loop unblocks and can break its loop on
            // `next == 0`.
            ctx.reply(&LifecycleAdvanceComplete {
                completed: 0,
                next: 0,
            });
            return;
        }

        if self.pending.is_some() {
            // Overlap: a prior advance hasn't settled yet. Normally the
            // chassis main loop wait-replies on every Advance, so this is
            // a duplicate-cadence-source bug — warn-and-drop without
            // state mutation. But if the pending advance has blown past
            // `advance_timeout`, its `Settled` is not coming (a saturated
            // settlement pipeline, iamacoffeepot/aether#1048): force-
            // complete it so the lifecycle degrades to a stutter instead
            // of wedging forever, then fall through to process *this*
            // advance against the now-advanced state.
            if !self.pending_timed_out() {
                tracing::warn!(
                    target: "aether_substrate::lifecycle",
                    current = ?self.current_state,
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
            let bytes = (state.factory)(&self.context);
            let next = resolve_edge(state, &mut self.quit_pending);
            Step::StateAdvance {
                bytes,
                broadcast: self.current_state,
                next,
            }
        } else if let Some(term) = self.graph.terminal(self.current_state) {
            let bytes = (term.factory)(&self.context);
            Step::Terminal {
                bytes,
                broadcast: self.current_state,
            }
        } else {
            // Defensive — builder finalize prevents this.
            Step::Unknown
        };

        let (bytes, broadcast, next_kind, is_terminal) = match step {
            Step::StateAdvance {
                bytes,
                broadcast,
                next,
            } => (bytes, broadcast, next, false),
            Step::Terminal { bytes, broadcast } => (bytes, broadcast, KindId(0), true),
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
        // in-flight against the root.
        broadcast_to_subscribers(ctx, &self.subscribers, broadcast, &bytes);

        // Subscribe settlement on the inbound's chain root. The
        // broadcast subtree is part of that chain; settlement fires
        // once the inbound's `Finished` event drops the in-flight
        // count to zero (which includes every fan-out descendant).
        let root = ctx.in_flight_root();
        let reply_to = ctx.reply_target();
        if let Some(registry) = self.mailer.settlement_registry() {
            registry.subscribe_settlement_mail(
                root,
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
    /// `Settled` notices for unrelated roots (stale subscriptions
    /// from a torn-down session, post-terminal cleanup events) drop
    /// without state mutation.
    ///
    /// # Agent
    /// `Settled { root }`. Synthesised by the settlement registry
    /// when the in-flight count for `root` reaches zero; not a
    /// public API for user code.
    #[handler]
    fn on_settled(&mut self, ctx: &mut NativeCtx<'_>, payload: Settled) {
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        if payload.root != pending.root {
            return;
        }
        let LifecycleAdvanceComplete { completed, next } = LifecycleAdvanceComplete {
            completed: pending.completed_kind.0,
            next: pending.next_kind.0,
        };
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
        // Route the reply to whoever issued the LifecycleAdvance —
        // chassis main loops `wait_reply` on it to gate the next frame.
        ctx.reply_to(reply_to, &LifecycleAdvanceComplete { completed, next });
    }
}

impl<C: 'static + Send + Sync> LifecycleDriverCapability<C> {
    /// True when a pending advance has exceeded [`Self::advance_timeout`]
    /// without settling (iamacoffeepot/aether#1048). `false` when nothing
    /// is pending.
    fn pending_timed_out(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|p| p.started.elapsed() >= self.advance_timeout)
    }

    /// Force-complete a pending advance whose [`Settled`] never arrived
    /// (iamacoffeepot/aether#1048). Mirrors [`Self::on_settled`]'s state
    /// mutation + reply but logs at `error`: reaching here means the
    /// settlement pipeline stalled past `advance_timeout`, so the
    /// lifecycle is degrading to a stutter rather than wedging. No-op
    /// when nothing is pending.
    fn force_complete_pending(&mut self, ctx: &mut NativeCtx<'_>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        tracing::error!(
            target: "aether_substrate::lifecycle",
            root = ?pending.root,
            elapsed_ms = pending.started.elapsed().as_millis(),
            timeout_ms = self.advance_timeout.as_millis(),
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
    /// surface). Production callers should observe lifecycle progress
    /// via subscribed stage broadcasts rather than peeking at this.
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
    /// Cleared automatically at the next state whose graph declares a
    /// `quit` edge.
    #[must_use]
    pub fn quit_pending(&self) -> bool {
        self.quit_pending
    }
}

/// Push the encoded `payload` to each subscriber of `stage` as an
/// untyped envelope. Uses the runtime-id `send_envelope_traced` path
/// because the graph's factories produce bytes for a kind chosen at
/// runtime (the current state's), not a compile-site `K`.
fn broadcast_to_subscribers(
    ctx: &mut NativeCtx<'_>,
    subscribers: &BTreeMap<KindId, BTreeSet<DataMailboxId>>,
    stage: KindId,
    payload: &[u8],
) {
    let Some(set) = subscribers.get(&stage) else {
        return;
    };
    for mailbox in set {
        let _ = ctx.send_envelope_traced(MailboxId(mailbox.0), stage, payload);
    }
}

/// Decide which edge to follow out of `state` given the current
/// `quit_pending` flag (ADR-0082 §3). If `quit_pending` is set AND
/// the state declares a `quit` edge, consume the flag and return the
/// quit target; otherwise return the unconditional `next` target.
fn resolve_edge<C>(state: &LifecycleState<C>, quit_pending: &mut bool) -> KindId {
    if *quit_pending && let Some(quit_target) = state.quit {
        *quit_pending = false;
        return quit_target;
    }
    state.next
}

#[cfg(test)]
mod tests {
    //! Unit-level tests for the lifecycle decision logic. End-to-end
    //! broadcast / `LifecycleAdvance` flow waits for PR 3 chassis integration —
    //! the substrate-wide mailer / settlement plumbing required to
    //! exercise `on_advance` on a live dispatcher isn't reachable from
    //! a `cargo test -p aether-substrate` invocation. The
    //! `resolve_edge` function below carries the ADR-0082 §3 quit-flag
    //! semantics that subsequent integration work depends on; covering
    //! it at the unit layer keeps the property pinned even if the
    //! advance integration is delayed.

    use super::*;
    use crate::handle_store::HandleStore;
    use crate::lifecycle::graph::{LifecycleGraph, LifecycleState};
    use crate::mail::registry::Registry;
    use aether_data::Kind;
    use aether_kinds::{Present, Render, Shutdown};

    fn dummy_factory<C>() -> super::super::graph::StateFactory<C> {
        Box::new(|_| Vec::new())
    }

    fn state_with_quit<C>(kind_id: u64, next: u64, quit: Option<u64>) -> LifecycleState<C> {
        LifecycleState {
            kind: KindId(kind_id),
            factory: dummy_factory(),
            next: KindId(next),
            quit: quit.map(KindId),
        }
    }

    #[test]
    fn resolve_edge_takes_next_when_no_quit_pending() {
        let state = state_with_quit::<()>(1, 2, Some(99));
        let mut quit = false;
        let next = resolve_edge(&state, &mut quit);
        assert_eq!(next, KindId(2));
        assert!(!quit);
    }

    #[test]
    fn resolve_edge_takes_quit_when_pending_and_declared() {
        let state = state_with_quit::<()>(1, 2, Some(99));
        let mut quit = true;
        let next = resolve_edge(&state, &mut quit);
        assert_eq!(next, KindId(99));
        assert!(!quit, "quit flag must be consumed");
    }

    #[test]
    fn resolve_edge_persists_quit_when_no_quit_edge_declared() {
        // ADR-0082 §3: the flag persists across states with no
        // declared quit edge; only states declaring `.quit::<K>()`
        // consume it.
        let state = state_with_quit::<()>(1, 2, None);
        let mut quit = true;
        let next = resolve_edge(&state, &mut quit);
        assert_eq!(next, KindId(2));
        assert!(quit, "quit flag must persist when state has no quit edge");
    }

    #[test]
    fn driver_initial_state_is_graph_start() {
        // Smoke that the driver's init derives `current_state` from
        // `graph.start()`. We construct the driver fields directly
        // rather than booting a chassis — PR 2 scope ships the
        // primitive, PR 3's chassis integration exercises the boot
        // path end-to-end.
        //
        // Uses Render/Present states to dodge textual overlap with the
        // graph-side test fixtures (Qodana's `DuplicatedCode` keys on
        // the chained-builder shape, not the underlying logic).
        let graph = LifecycleGraph::<()>::builder()
            .state::<Render, _>(|()| Render {})
            .next::<Present>()
            .state::<Present, _>(|()| Present {})
            .next::<Shutdown>()
            .quit::<Shutdown>()
            .terminal::<Shutdown, _>(|()| Shutdown {})
            .start::<Render>()
            .build()
            .expect("test setup: graph builds");

        let mailer = Arc::new(Mailer::new(
            Arc::new(Registry::default()),
            Arc::new(HandleStore::new(1024)),
        ));
        let driver: LifecycleDriverCapability<()> = LifecycleDriverCapability {
            current_state: graph.start(),
            graph,
            context: (),
            subscribers: BTreeMap::new(),
            terminal_reached: false,
            quit_pending: false,
            pending: None,
            advance_timeout: Duration::from_millis(ADVANCE_TIMEOUT_MS_DEFAULT),
            mailer,
            _marker: PhantomData,
        };

        assert_eq!(driver.current_state(), <Render as Kind>::ID);
        assert!(!driver.is_terminal());
        assert!(!driver.quit_pending());
    }

    /// iamacoffeepot/aether#1048: `pending_timed_out` is the gate that
    /// turns a permanent settlement wedge into a stutter. A zero timeout
    /// trips immediately on any pending advance; an hour-long one never
    /// trips on a freshly-issued one. Force-completion's state mutation
    /// and reply need a live `NativeCtx` (exercised by the chassis
    /// integration tests); the decision itself is unit-checkable here.
    #[test]
    fn pending_timeout_predicate() {
        let graph = LifecycleGraph::<()>::builder()
            .state::<Render, _>(|()| Render {})
            .next::<Present>()
            .state::<Present, _>(|()| Present {})
            .next::<Shutdown>()
            .terminal::<Shutdown, _>(|()| Shutdown {})
            .start::<Render>()
            .build()
            .expect("test setup: graph builds");
        let mailer = Arc::new(Mailer::new(
            Arc::new(Registry::default()),
            Arc::new(HandleStore::new(1024)),
        ));
        let mut driver: LifecycleDriverCapability<()> = LifecycleDriverCapability {
            current_state: graph.start(),
            graph,
            context: (),
            subscribers: BTreeMap::new(),
            terminal_reached: false,
            quit_pending: false,
            pending: None,
            advance_timeout: Duration::ZERO,
            mailer,
            _marker: PhantomData,
        };

        // Nothing pending → never timed out.
        assert!(!driver.pending_timed_out());

        let pending = PendingAdvance {
            root: MailId::NONE,
            completed_kind: <Render as Kind>::ID,
            next_kind: <Present as Kind>::ID,
            is_terminal: false,
            reply_to: ReplyTo::NONE,
            started: Instant::now(),
        };
        driver.pending = Some(pending);
        // Zero timeout: any elapsed >= 0 trips immediately.
        assert!(driver.pending_timed_out());

        // A long timeout never trips on a freshly-issued advance.
        driver.advance_timeout = Duration::from_hours(1);
        assert!(!driver.pending_timed_out());
    }
}
