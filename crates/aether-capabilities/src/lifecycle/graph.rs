//! The lifecycle data graph (ADR-0082 §1): the freeze-at-construction
//! `{ stage_kind, next, optional quit }` edge set plus its type-state
//! builder. Always-compiled and substrate-free — depends only on
//! `aether-data` — so it lifts cleanly out of `mod native`.

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

use aether_data::{Kind, KindId};

/// A non-terminal state in the lifecycle graph (ADR-0082 §1). A stage
/// kind id, a required `next` edge, and an optional `quit` escape edge.
/// Stage payloads are empty signals, so a state carries no factory — the
/// `<C>` chassis-context closure the original generic driver threaded is
/// gone (the data graph is non-generic, which is what makes the cap
/// bridgeable).
#[derive(Clone)]
pub struct LifecycleStateData {
    pub(in crate::lifecycle) kind: KindId,
    pub(in crate::lifecycle) next: KindId,
    pub(in crate::lifecycle) quit: Option<KindId>,
}

/// A compiled lifecycle graph as plain data (ADR-0082 §1). Built via
/// [`LifecycleGraphData::builder`]; consumed by `LifecycleCapability`
/// at boot through `LifecycleConfig`. Freeze-at-construction — once
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
    pub(in crate::lifecycle) fn state(&self, kind: KindId) -> Option<&LifecycleStateData> {
        self.states.iter().find(|s| s.kind == kind)
    }

    /// True if `kind` is a registered terminal.
    pub(in crate::lifecycle) fn is_terminal(&self, kind: KindId) -> bool {
        self.terminals.contains(&kind)
    }

    /// The configured start state's kind id.
    pub(in crate::lifecycle) fn start(&self) -> KindId {
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
}
