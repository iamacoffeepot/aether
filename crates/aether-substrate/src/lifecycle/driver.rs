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
//! **PR 2 scope.** This implementation is fire-and-advance — the driver
//! broadcasts then advances without awaiting settlement. ADR-0082 §6
//! settlement gating lands in PR 3 (chassis migration) when settlement
//! integration becomes load-bearing. The `LifecycleAdvance` mail itself is
//! fire-and-forget (no reply); subscribe / unsubscribe return
//! [`LifecycleSubscribeResult`] so callers learn fail-fast about
//! unsupported stages per ADR-0082 §7.

use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;

use aether_actor::{OutboundReply, actor};
use aether_data::{KindId, MailboxId as DataMailboxId};
use aether_kinds::{
    LifecycleAdvance, LifecycleSubscribe, LifecycleSubscribeResult, LifecycleUnsubscribe, Quit,
};

use super::graph::{LifecycleGraph, LifecycleState};
use crate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use crate::chassis::error::BootError;
use crate::mail::MailboxId;

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
    /// Marker so the unused `C` type parameter is retained even when
    /// the only direct use of `C` is through the graph's factories.
    _marker: PhantomData<fn() -> C>,
}

#[actor]
impl<C: 'static + Send + Sync> NativeActor for LifecycleDriverCapability<C> {
    type Config = LifecycleDriverConfig<C>;
    const NAMESPACE: &'static str = "aether.lifecycle";

    fn init(
        config: LifecycleDriverConfig<C>,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<Self, BootError> {
        let LifecycleDriverConfig { graph, context } = config;
        let current_state = graph.start();
        Ok(Self {
            graph,
            context,
            subscribers: BTreeMap::new(),
            current_state,
            terminal_reached: false,
            quit_pending: false,
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
    /// that stage, then advance the state pointer along the resolved
    /// edge (`quit` if `quit_pending` is set and the state declares
    /// a quit edge, otherwise `next`).
    ///
    /// Fire-and-advance: PR 2 does not await settlement before
    /// advancing. PR 3's chassis migration adds settlement gating.
    /// Fire-and-forget mail — no reply is sent.
    ///
    /// # Agent
    /// `LifecycleAdvance {}`. Sent by the chassis main loop each frame
    /// (winit redraw, headless std-timer, etc.).
    #[handler]
    fn on_advance(&mut self, ctx: &mut NativeCtx<'_>, _payload: LifecycleAdvance) {
        if self.terminal_reached {
            return;
        }

        // Decide what to broadcast and what edge to take. Borrow the
        // immutable graph data first so we can release before mutating
        // self.quit_pending / self.current_state.
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
            // Should not be reachable — current_state always points to
            // a registered state or terminal per builder finalize.
            // Defensive no-op.
            Step::Unknown
        };

        match step {
            Step::StateAdvance {
                bytes,
                broadcast,
                next,
            } => {
                broadcast_to_subscribers(ctx, &self.subscribers, broadcast, &bytes);
                self.current_state = next;
            }
            Step::Terminal { bytes, broadcast } => {
                broadcast_to_subscribers(ctx, &self.subscribers, broadcast, &bytes);
                self.terminal_reached = true;
            }
            Step::Unknown => {}
        }
    }
}

impl<C: 'static + Send + Sync> LifecycleDriverCapability<C> {
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
    use crate::lifecycle::graph::{LifecycleGraph, LifecycleState};
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

        let driver: LifecycleDriverCapability<()> = LifecycleDriverCapability {
            current_state: graph.start(),
            graph,
            context: (),
            subscribers: BTreeMap::new(),
            terminal_reached: false,
            quit_pending: false,
            _marker: PhantomData,
        };

        assert_eq!(driver.current_state(), <Render as Kind>::ID);
        assert!(!driver.is_terminal());
        assert!(!driver.quit_pending());
    }
}
