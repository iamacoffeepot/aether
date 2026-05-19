//! [`LifecycleGraph`] + type-state builder (ADR-0082 §1).
//!
//! The graph is a directed sequence of states. Each state stores a
//! kind id (what to broadcast), a factory closure (how to mint the
//! payload from chassis context `&C`), a required `next` edge, and an
//! optional `quit` escape edge. Terminal states have no edges; they
//! signal lifecycle completion to the driver.
//!
//! The builder uses three type-state markers to enforce structural
//! invariants at compile time:
//!
//! - [`NoOpen`] — no pending state. Accepts `.state(...)`, `.terminal(...)`,
//!   `.start::<K>()`, `.build()`.
//! - [`OpenNoNext`] — a state was just registered via `.state(...)` and
//!   needs its `next` edge before the next state can be added. Accepts
//!   `.next::<K>()` (transitions to [`OpenWithNext`]) or
//!   `.quit::<K>()` (stays here).
//! - [`OpenWithNext`] — the current state has its `next` edge set;
//!   future `.state(...)` / `.terminal(...)` / `.start::<K>()` /
//!   `.build()` calls commit the pending state and transition back to
//!   [`NoOpen`]. `.quit::<K>()` is also still accepted on this state.
//!
//! Finalize-time checks (run at `.build()`) handle the rest: every
//! `next` / `quit` / `start` target must resolve to a registered state
//! or terminal; at least one terminal must be reachable from start;
//! exactly one `.start::<K>()` must have been called.

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

use aether_data::{Kind, KindId};

/// Type-erased per-state factory. The closure reads from chassis-owned
/// context `&C` and returns the encoded stage payload bytes. Held by
/// the [`LifecycleGraph`] and called by the
/// [`LifecycleDriverCapability`](super::LifecycleDriverCapability) on
/// each [`Advance`](aether_kinds::Advance) mail.
pub type StateFactory<C> = Box<dyn Fn(&C) -> Vec<u8> + Send + Sync + 'static>;

/// A non-terminal state in the lifecycle graph (ADR-0082 §1). Owns its
/// own broadcast kind, factory, required `next` edge, and optional
/// `quit` escape edge.
pub struct LifecycleState<C> {
    pub kind: KindId,
    pub factory: StateFactory<C>,
    pub next: KindId,
    pub quit: Option<KindId>,
}

/// A terminal state in the lifecycle graph. No outgoing edges; entry
/// here causes the driver to report `is_terminal() == true` and the
/// chassis main loop to break.
pub struct LifecycleTerminal<C> {
    pub kind: KindId,
    pub factory: StateFactory<C>,
}

/// A compiled lifecycle graph (ADR-0082 §1). Built via
/// [`LifecycleGraphBuilder`]; consumed by
/// [`LifecycleDriverCapability`](super::LifecycleDriverCapability) at
/// driver boot.
///
/// The graph is freeze-at-construction: once built it isn't mutated.
/// Runtime mutation of the lifecycle is out of scope for ADR-0082 §v1.
pub struct LifecycleGraph<C> {
    pub states: Vec<LifecycleState<C>>,
    pub terminals: Vec<LifecycleTerminal<C>>,
    pub start: KindId,
}

// Debug omits the opaque factory closures so `expect_err` / panic
// messages on `Result<LifecycleGraph<_>, _>` print useful structural
// information without violating closure-doesn't-impl-Debug.
impl<C> fmt::Debug for LifecycleGraph<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state_kinds: Vec<KindId> = self.states.iter().map(|s| s.kind).collect();
        let terminal_kinds: Vec<KindId> = self.terminals.iter().map(|t| t.kind).collect();
        f.debug_struct("LifecycleGraph")
            .field("start", &self.start)
            .field("states", &state_kinds)
            .field("terminals", &terminal_kinds)
            .finish()
    }
}

impl<C> LifecycleGraph<C> {
    /// Start building a new lifecycle graph. The returned builder is
    /// in the [`NoOpen`] state — no pending state — and accepts
    /// `.state(...)`, `.terminal(...)`, `.start::<K>()`, or
    /// `.build()`.
    #[must_use]
    pub fn builder() -> LifecycleGraphBuilder<C, NoOpen> {
        LifecycleGraphBuilder {
            inner: Inner {
                states: Vec::new(),
                terminals: Vec::new(),
                start: None,
                pending: None,
            },
            _state: PhantomData,
        }
    }

    /// Look up the state or terminal registered at `kind`. Returns
    /// `None` for an unknown kind (which the builder finalize-time
    /// check rejects, so production callers shouldn't see this).
    #[must_use]
    pub fn state(&self, kind: KindId) -> Option<&LifecycleState<C>> {
        self.states.iter().find(|s| s.kind == kind)
    }

    #[must_use]
    pub fn terminal(&self, kind: KindId) -> Option<&LifecycleTerminal<C>> {
        self.terminals.iter().find(|t| t.kind == kind)
    }

    /// True if the kind is a registered terminal.
    #[must_use]
    #[allow(dead_code)] // Surface kept for the chassis migration in PR 3.
    pub fn is_terminal(&self, kind: KindId) -> bool {
        self.terminal(kind).is_some()
    }

    /// The configured start state's kind id.
    #[must_use]
    pub fn start(&self) -> KindId {
        self.start
    }
}

/// Builder type-state marker: no pending state. Initial state of the
/// builder. Accepts `.state(...)`, `.terminal(...)`, `.start::<K>()`,
/// or `.build()`.
pub struct NoOpen;
/// Builder type-state marker: a state was just registered via
/// `.state(...)` and needs its `next` edge before another state can be
/// added. Accepts `.next::<K>()` (transitions to [`OpenWithNext`]) or
/// `.quit::<K>()` (stays here).
pub struct OpenNoNext;
/// Builder type-state marker: the current state has its `next` edge
/// set. Future `.state(...)` / `.terminal(...)` / `.start::<K>()` /
/// `.build()` calls commit the pending state and transition back to
/// [`NoOpen`]. `.quit::<K>()` is also still accepted to set the
/// optional escape edge.
pub struct OpenWithNext;

/// Builder for [`LifecycleGraph`]. Built via
/// [`LifecycleGraph::builder()`]; finalized by `.build()`.
pub struct LifecycleGraphBuilder<C, S> {
    inner: Inner<C>,
    _state: PhantomData<S>,
}

struct Inner<C> {
    states: Vec<LifecycleState<C>>,
    terminals: Vec<LifecycleTerminal<C>>,
    start: Option<KindId>,
    pending: Option<PendingState<C>>,
}

struct PendingState<C> {
    kind: KindId,
    factory: StateFactory<C>,
    next: Option<KindId>,
    quit: Option<KindId>,
}

impl<C> Inner<C> {
    /// Set the pending state's optional `quit` edge. Called from both
    /// `quit` methods (`OpenNoNext` and `OpenWithNext`) so the
    /// per-typestate handlers stay one-line wrappers.
    fn set_pending_quit(&mut self, quit: KindId) {
        if let Some(pending) = self.pending.as_mut() {
            pending.quit = Some(quit);
        }
    }

    /// Commit the pending state into `states`. Caller must have
    /// established `pending.is_some()` and `pending.next.is_some()`
    /// via the type-state machinery — this is the runtime
    /// implementation of that compile-time guarantee.
    fn commit_pending(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        // Safety: the only callers reach here from `Builder<_,
        // OpenWithNext>`, which guarantees `pending.next.is_some()`.
        // The unwrap is unreachable in well-typed code.
        let next = pending.next.expect(
            "lifecycle builder bug: commit_pending invoked without a next edge set; \
             type-state should prevent this",
        );
        self.states.push(LifecycleState {
            kind: pending.kind,
            factory: pending.factory,
            next,
            quit: pending.quit,
        });
    }
}

impl<C: 'static> LifecycleGraphBuilder<C, NoOpen> {
    /// Register a new state with the given factory. The factory's
    /// return type infers the state's broadcast kind id via `K::ID`
    /// — authors don't write `::ID` anywhere.
    pub fn state<K, F>(mut self, factory: F) -> LifecycleGraphBuilder<C, OpenNoNext>
    where
        K: Kind,
        F: Fn(&C) -> K + Send + Sync + 'static,
    {
        let boxed: StateFactory<C> = Box::new(move |c| factory(c).encode_into_bytes());
        self.inner.pending = Some(PendingState {
            kind: <K as Kind>::ID,
            factory: boxed,
            next: None,
            quit: None,
        });
        LifecycleGraphBuilder {
            inner: self.inner,
            _state: PhantomData,
        }
    }

    /// Register a terminal state with the given factory. Terminals
    /// have no outgoing edges; reaching a terminal causes the driver
    /// to report `is_terminal() == true`.
    #[must_use]
    pub fn terminal<K, F>(mut self, factory: F) -> Self
    where
        K: Kind,
        F: Fn(&C) -> K + Send + Sync + 'static,
    {
        let boxed: StateFactory<C> = Box::new(move |c| factory(c).encode_into_bytes());
        self.inner.terminals.push(LifecycleTerminal {
            kind: <K as Kind>::ID,
            factory: boxed,
        });
        self
    }

    /// Set the start state. Exactly one `.start::<K>()` call is
    /// required before `.build()`; multiple calls overwrite (the
    /// `build` check enforces "non-None," not "exactly one
    /// non-overwriting"). Builder finalize-time check rejects if the
    /// target wasn't registered as a state or terminal.
    #[must_use]
    pub fn start<K: Kind>(mut self) -> Self {
        self.inner.start = Some(<K as Kind>::ID);
        self
    }

    /// Finalize the graph. Validates that every `next` / `quit` /
    /// `start` target resolves to a registered state or terminal,
    /// that a start was set, and that at least one terminal is
    /// reachable from start.
    pub fn build(self) -> Result<LifecycleGraph<C>, BuildError> {
        finalize(self.inner)
    }
}

impl<C: 'static> LifecycleGraphBuilder<C, OpenNoNext> {
    /// Set the pending state's `next` edge. Transitions the builder
    /// to [`OpenWithNext`]; the pending state's commit is deferred to
    /// the next `.state(...)` / `.terminal(...)` / `.start::<K>()` /
    /// `.build()` call.
    #[must_use]
    pub fn next<K: Kind>(mut self) -> LifecycleGraphBuilder<C, OpenWithNext> {
        if let Some(pending) = self.inner.pending.as_mut() {
            pending.next = Some(<K as Kind>::ID);
        }
        LifecycleGraphBuilder {
            inner: self.inner,
            _state: PhantomData,
        }
    }

    /// Set the pending state's optional `quit` escape edge. Stays in
    /// [`OpenNoNext`] — `next` is still required before another state
    /// can be added.
    #[must_use]
    pub fn quit<K: Kind>(mut self) -> Self {
        self.inner.set_pending_quit(<K as Kind>::ID);
        self
    }
}

impl<C: 'static> LifecycleGraphBuilder<C, OpenWithNext> {
    /// Set or override the pending state's optional `quit` escape
    /// edge. Stays in [`OpenWithNext`].
    #[must_use]
    pub fn quit<K: Kind>(mut self) -> Self {
        self.inner.set_pending_quit(<K as Kind>::ID);
        self
    }

    /// Commit the pending state and start a new one (ADR-0082 §1).
    /// The type-state guarantees this call is only reachable after
    /// `.next::<K>()` set the prior state's required edge.
    pub fn state<K, F>(mut self, factory: F) -> LifecycleGraphBuilder<C, OpenNoNext>
    where
        K: Kind,
        F: Fn(&C) -> K + Send + Sync + 'static,
    {
        self.inner.commit_pending();
        let boxed: StateFactory<C> = Box::new(move |c| factory(c).encode_into_bytes());
        self.inner.pending = Some(PendingState {
            kind: <K as Kind>::ID,
            factory: boxed,
            next: None,
            quit: None,
        });
        LifecycleGraphBuilder {
            inner: self.inner,
            _state: PhantomData,
        }
    }

    /// Commit the pending state and add a terminal.
    pub fn terminal<K, F>(mut self, factory: F) -> LifecycleGraphBuilder<C, NoOpen>
    where
        K: Kind,
        F: Fn(&C) -> K + Send + Sync + 'static,
    {
        self.inner.commit_pending();
        let boxed: StateFactory<C> = Box::new(move |c| factory(c).encode_into_bytes());
        self.inner.terminals.push(LifecycleTerminal {
            kind: <K as Kind>::ID,
            factory: boxed,
        });
        LifecycleGraphBuilder {
            inner: self.inner,
            _state: PhantomData,
        }
    }

    /// Commit the pending state and set the start.
    #[must_use]
    pub fn start<K: Kind>(mut self) -> LifecycleGraphBuilder<C, NoOpen> {
        self.inner.commit_pending();
        self.inner.start = Some(<K as Kind>::ID);
        LifecycleGraphBuilder {
            inner: self.inner,
            _state: PhantomData,
        }
    }

    /// Commit the pending state and finalize the graph.
    pub fn build(mut self) -> Result<LifecycleGraph<C>, BuildError> {
        self.inner.commit_pending();
        finalize(self.inner)
    }
}

/// Errors returned by [`LifecycleGraphBuilder::build()`]. Each variant
/// names the structural invariant that was violated and the kind id
/// involved (where applicable).
#[derive(Debug)]
pub enum BuildError {
    /// No `.start::<K>()` was called before `.build()`.
    MissingStart,
    /// `.start::<K>()` targeted a kind id that wasn't registered as
    /// a state or terminal.
    StartNotRegistered { start: KindId },
    /// A state's `next` edge targets a kind id that isn't registered
    /// as a state or terminal.
    NextNotRegistered { state: KindId, next: KindId },
    /// A state's `quit` edge targets a kind id that isn't registered
    /// as a state or terminal.
    QuitNotRegistered { state: KindId, quit: KindId },
    /// The graph contains no terminals — there's no way for the
    /// lifecycle to complete cleanly.
    NoTerminals,
    /// A kind id is registered more than once (either as a state, a
    /// terminal, or both). Each state and terminal must have a unique
    /// broadcast kind.
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

fn finalize<C>(inner: Inner<C>) -> Result<LifecycleGraph<C>, BuildError> {
    let Inner {
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
        .chain(terminals.iter().map(|t| t.kind));
    for kind in all_kinds {
        if seen.contains(&kind) {
            return Err(BuildError::DuplicateKind { kind });
        }
        seen.push(kind);
    }

    // Resolution check for start.
    let known =
        |k: KindId| states.iter().any(|s| s.kind == k) || terminals.iter().any(|t| t.kind == k);
    if !known(start) {
        return Err(BuildError::StartNotRegistered { start });
    }

    // Resolution check for next + quit on every state.
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

    Ok(LifecycleGraph {
        states,
        terminals,
        start,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_kinds::{InitCaps, InitComponents, Quit, Shutdown, Tick};

    /// Shared fixture: a single-state graph (`InitCaps → Shutdown`)
    /// that several build-error tests start from. Returns the builder
    /// post-state-and-terminal but pre-start so tests can either commit
    /// or substitute the start before `.build()`.
    fn init_to_shutdown_builder() -> LifecycleGraphBuilder<(), NoOpen> {
        LifecycleGraph::<()>::builder()
            .state::<InitCaps, _>(|()| InitCaps {})
            .next::<Shutdown>()
            .terminal::<Shutdown, _>(|()| Shutdown {})
    }

    #[test]
    fn minimal_graph_init_to_terminal_builds() {
        let graph = init_to_shutdown_builder()
            .start::<InitCaps>()
            .build()
            .expect("test setup: minimal graph builds");

        assert_eq!(graph.start(), <InitCaps as Kind>::ID);
        assert_eq!(graph.states.len(), 1);
        assert_eq!(graph.terminals.len(), 1);
        assert!(graph.is_terminal(<Shutdown as Kind>::ID));
        assert!(!graph.is_terminal(<InitCaps as Kind>::ID));
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
            .start::<Tick>() // Tick is not registered in the graph
            .build()
            .expect_err("start::<Tick> with no Tick state should fail");
        assert!(matches!(err, BuildError::StartNotRegistered { .. }));
    }

    #[test]
    fn build_rejects_next_unregistered() {
        let err = LifecycleGraph::<()>::builder()
            .state::<InitCaps, _>(|()| InitCaps {})
            .next::<Tick>() // Tick is not registered as a state or terminal
            .terminal::<Shutdown, _>(|()| Shutdown {})
            .start::<InitCaps>()
            .build()
            .expect_err("next::<Tick> with no Tick state should fail");
        assert!(matches!(err, BuildError::NextNotRegistered { .. }));
    }

    #[test]
    fn build_rejects_quit_unregistered() {
        let err = LifecycleGraph::<()>::builder()
            .state::<InitCaps, _>(|()| InitCaps {})
            .quit::<Quit>() // Quit is not registered as a state or terminal
            .next::<Shutdown>()
            .terminal::<Shutdown, _>(|()| Shutdown {})
            .start::<InitCaps>()
            .build()
            .expect_err("quit::<Quit> with no Quit state should fail");
        assert!(matches!(err, BuildError::QuitNotRegistered { .. }));
    }

    #[test]
    fn build_rejects_no_terminals() {
        let err = LifecycleGraph::<()>::builder()
            .state::<InitCaps, _>(|()| InitCaps {})
            .next::<InitComponents>()
            .state::<InitComponents, _>(|()| InitComponents {})
            .next::<InitCaps>()
            .start::<InitCaps>()
            .build()
            .expect_err("graph with no terminals should fail");
        assert!(matches!(err, BuildError::NoTerminals));
    }

    #[test]
    fn build_rejects_duplicate_state_kind() {
        let err = LifecycleGraph::<()>::builder()
            .state::<InitCaps, _>(|()| InitCaps {})
            .next::<Shutdown>()
            .state::<InitCaps, _>(|()| InitCaps {})
            .next::<Shutdown>()
            .terminal::<Shutdown, _>(|()| Shutdown {})
            .start::<InitCaps>()
            .build()
            .expect_err("duplicate state kind should fail");
        assert!(matches!(err, BuildError::DuplicateKind { .. }));
    }

    #[test]
    fn build_rejects_state_and_terminal_same_kind() {
        let err = LifecycleGraph::<()>::builder()
            .state::<Shutdown, _>(|()| Shutdown {})
            .next::<InitCaps>()
            .terminal::<Shutdown, _>(|()| Shutdown {})
            .start::<Shutdown>()
            .build()
            .expect_err("kind registered as both state and terminal should fail");
        assert!(matches!(err, BuildError::DuplicateKind { .. }));
    }

    #[test]
    fn cycle_with_quit_edge_builds() {
        // Init → InitComponents → InitCaps (back) — cyclic, with Quit
        // escape edge to Shutdown terminal. ADR-0082 §1 example shape.
        let graph = LifecycleGraph::<()>::builder()
            .state::<InitCaps, _>(|()| InitCaps {})
            .next::<InitComponents>()
            .state::<InitComponents, _>(|()| InitComponents {})
            .next::<InitCaps>()
            .quit::<Shutdown>()
            .terminal::<Shutdown, _>(|()| Shutdown {})
            .start::<InitCaps>()
            .build()
            .expect("test setup: cyclic graph with quit edge builds");
        assert_eq!(graph.states.len(), 2);
        assert_eq!(graph.terminals.len(), 1);
        assert!(
            graph
                .state(<InitComponents as Kind>::ID)
                .expect("test setup: InitComponents state registered")
                .quit
                .is_some()
        );
    }

    #[test]
    fn factory_receives_chassis_ctx() {
        use std::cell::Cell;
        #[derive(Default)]
        struct TestCtx {
            tick_count: Cell<u32>,
        }
        let graph = LifecycleGraph::<TestCtx>::builder()
            .state::<InitCaps, _>(|ctx: &TestCtx| {
                ctx.tick_count.set(ctx.tick_count.get() + 1);
                InitCaps {}
            })
            .next::<Shutdown>()
            .terminal::<Shutdown, _>(|_ctx: &TestCtx| Shutdown {})
            .start::<InitCaps>()
            .build()
            .expect("test setup: ctx-aware graph builds");
        let ctx = TestCtx::default();
        let state = graph
            .state(<InitCaps as Kind>::ID)
            .expect("test setup: InitCaps state registered");
        let _bytes = (state.factory)(&ctx);
        assert_eq!(ctx.tick_count.get(), 1);
        let _bytes = (state.factory)(&ctx);
        assert_eq!(ctx.tick_count.get(), 2);
    }
}
