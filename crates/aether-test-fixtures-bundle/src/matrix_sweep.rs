//! Issue 1977 (ADR-0114 amendment) cluster-addressing matrix fixture. A
//! multi-actor module — `export!(MatrixParent, MatrixChild)` — whose entry
//! `MatrixParent` forms a small cluster: it spawns two co-located inline
//! children (`a` and `b`) in `wire`. On a `RunMatrix` command (sent over the
//! wire) the parent drives every in-cluster addressing direction in place,
//! plus one cross-cluster send made *during the in-place drain*; each
//! participant records the cell it observed — whether the mail arrived and
//! what `ctx.sender()` it read. A follow-up `CollectMatrix` query
//! reads the cluster's shared observation log and replies a `MatrixReport`.
//!
//! Matrix cells (each asserts delivery AND the source the recipient read):
//!
//! - parent → child\[a\] (in place): child\[a\]'s source is the parent's id.
//! - child\[a\] → parent (in place): the parent's source is child\[a\]'s id.
//! - child\[a\] → sibling child\[b\] (in place): child\[b\]'s source is child\[a\]'s id.
//! - child\[a\] → self (in place): child\[a\]'s source is its own id.
//! - cross-cluster (child\[a\] → a second loaded component, *during the drain*):
//!   observed out-of-band by the observer (read via `log_tail`). The observer
//!   reads child\[a\]'s id: the member's ctx-mediated send threads its own id
//!   as the send's `from`, so the host stamps the member as origin (validated
//!   host-side to the cluster), not the cluster's inbound parent.
//!
//! The observation log is a cluster-shared `static` with the same
//! single-run-token `UnsafeCell` + blanket `Sync` discipline the inline
//! registry and `Slot` use (ADR-0010 §5: the guest is single-threaded and
//! the substrate serializes delivery under the run token). All cluster
//! members write into it during the one drained cascade; the parent reads it
//! on the later `CollectMatrix` query. Using a shared log instead of a
//! child → parent reporting protocol keeps the fixture to the addressing
//! verbs under test.

// The handlers take `&mut self` to match the dispatch ABI even when an arm
// reads only the shared log, not the actor's own fields.
#![allow(clippy::unused_self)]

use core::cell::UnsafeCell;

use aether_actor::{
    ActorInitError, ActorRef, Erased, Manual, OutboundReply, Subname, WasmActor, WasmCtx, WasmInitCtx, actor,
};
use aether_test_fixtures_kinds::{
    CollectMatrix, MATRIX_CELL_CHILD_TO_PARENT, MATRIX_CELL_CHILD_TO_SELF, MATRIX_CELL_CHILD_TO_SIBLING,
    MATRIX_CELL_PARENT_TO_CHILD, MatrixPing, MatrixReport, RunMatrix, SourceQuery,
};

use super::source_observer::SourceObserver;

/// One cell's recorded observation: whether the mail arrived and the raw
/// `MailboxId` the recipient read from `ctx.sender()`.
#[derive(Clone, Copy, Default)]
struct Cell {
    arrived: bool,
    source: u64,
}

/// The cluster-shared observation log. Indexed by the `MATRIX_CELL_*`
/// markers (1-based; index 0 is unused), plus the resolved parent / child\[a\]
/// ids the parent records so the test can assert the sources against the
/// actual folded addresses, plus the cross-cluster observer reference the
/// parent minted from its declared dependency. The reference is shared through
/// the log rather than threaded on `MatrixPing` because a proven reference has
/// no codec (ADR-0230); it never leaves this module instance.
struct MatrixLog {
    cells: [Cell; 5],
    parent_id: u64,
    child_a_id: u64,
    observer: Option<ActorRef<SourceObserver>>,
}

/// Interior-mutable cluster-shared store for [`MatrixLog`].
struct LogSlot {
    inner: UnsafeCell<MatrixLog>,
}

// SAFETY: identical argument to `aether_actor::Slot` / the inline registry —
// the WASM guest is single-threaded (ADR-0010 §5) and the substrate
// serializes delivery under the run token, so this `static` is only ever
// touched from one thread at a time, and the whole drained matrix cascade
// runs inside one `receive_p32` under one run token. Each borrow below is
// taken fresh and released before its function returns, never spanning a
// nested dispatch.
unsafe impl Sync for LogSlot {}

static MATRIX_LOG: LogSlot = LogSlot {
    inner: UnsafeCell::new(MatrixLog {
        cells: [Cell { arrived: false, source: 0 }; 5],
        parent_id: 0,
        child_a_id: 0,
        observer: None,
    }),
};

/// Record `(arrived, source)` for `cell` (a `MATRIX_CELL_*` marker) into the
/// shared log.
fn record_cell(cell: u32, source: u64) {
    // SAFETY: see `LogSlot`'s `Sync` impl — single-threaded guest, borrow
    // taken fresh and released before return.
    let log = unsafe { &mut *MATRIX_LOG.inner.get() };
    if let Some(slot) = log.cells.get_mut(cell as usize) {
        slot.arrived = true;
        slot.source = source;
    }
}

/// Record the resolved parent / child\[a\] ids the parent learned at sweep
/// start, so the test can assert each source against the real folded address.
fn record_ids(parent_id: u64, child_a_id: u64) {
    // SAFETY: see `LogSlot`'s `Sync` impl.
    let log = unsafe { &mut *MATRIX_LOG.inner.get() };
    log.parent_id = parent_id;
    log.child_a_id = child_a_id;
}

/// Record the cross-cluster observer reference the parent minted from its
/// declared dependency, so the fanning-out child can read it back during the
/// one drained cascade.
fn record_observer(observer: ActorRef<SourceObserver>) {
    // SAFETY: see `LogSlot`'s `Sync` impl.
    let log = unsafe { &mut *MATRIX_LOG.inner.get() };
    log.observer = Some(observer);
}

/// The observer reference the parent recorded, or `None` before it ran.
fn observer() -> Option<ActorRef<SourceObserver>> {
    // SAFETY: see `LogSlot`'s `Sync` impl.
    let log = unsafe { &*MATRIX_LOG.inner.get() };
    log.observer
}

/// Snapshot the shared log into a `MatrixReport` for the `CollectMatrix`
/// reply.
fn snapshot_report() -> MatrixReport {
    // SAFETY: see `LogSlot`'s `Sync` impl.
    let log = unsafe { &*MATRIX_LOG.inner.get() };
    let cell = |c: u32| log.cells[c as usize];
    let p2c = cell(MATRIX_CELL_PARENT_TO_CHILD);
    let c2p = cell(MATRIX_CELL_CHILD_TO_PARENT);
    let c2s = cell(MATRIX_CELL_CHILD_TO_SIBLING);
    let c2self = cell(MATRIX_CELL_CHILD_TO_SELF);
    MatrixReport {
        parent_to_child_arrived: u32::from(p2c.arrived),
        parent_to_child_source: p2c.source,
        child_to_parent_arrived: u32::from(c2p.arrived),
        child_to_parent_source: c2p.source,
        child_to_sibling_arrived: u32::from(c2s.arrived),
        child_to_sibling_source: c2s.source,
        child_to_self_arrived: u32::from(c2self.arrived),
        child_to_self_source: c2self.source,
        child_a_id: log.child_a_id,
        parent_id: log.parent_id,
    }
}

/// Entry export — the loaded component and cluster root. Spawns the two
/// inline children in `wire`, drives the sweep on `RunMatrix`, records the
/// child\[a\] → parent cell when it arrives, and answers `CollectMatrix`.
pub struct MatrixParent;

// The cross-cluster recipient is a declared dependency, which is what turns
// it into a reference the parent can hold: `MatrixParent` is the entry actor,
// so its `Embedded` seed is the trampoline and the fold lands beside it under
// the shared component host. An inline child's seed is its slot parent, which
// is why the child reads the reference back instead of minting its own.
#[actor(depends(SourceObserver))]
impl WasmActor for MatrixParent {
    const NAMESPACE: &'static str = "test.matrix.parent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(MatrixParent)
    }

    /// Co-locate two inline children under the `Named` subnames `a` and `b`,
    /// the cluster's leaf nodes.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        let _ = ctx.spawn_inline_child::<MatrixParent, MatrixChild>(Subname::Named("a"), &());
        let _ = ctx.spawn_inline_child::<MatrixParent, MatrixChild>(Subname::Named("b"), &());
    }

    /// Drive the sweep: record the parent / child\[a\] ids and the proven
    /// observer reference, then send the fan-out ping to child\[a\] in place.
    /// Child\[a\]'s handler drives the child-origin cells (child → parent /
    /// sibling / self) and the cross-cluster send. Everything settles in this
    /// one receive's drain. The handler spells its actor type because
    /// `actor_ref` is bounded `A: DependsOn<R>`.
    #[handler::single]
    fn on_run_matrix(&mut self, ctx: &mut WasmCtx<'_, MatrixParent>, _msg: RunMatrix) {
        record_observer(ctx.actor_ref::<SourceObserver>());

        let parent_id = ctx.mailbox_id();
        let child_a = ctx.child("a").expect("inline child a is resident");
        record_ids(parent_id.0, child_a.mailbox_id().0);
        child_a.send(&MatrixPing { cell: MATRIX_CELL_PARENT_TO_CHILD, fan_out: 1 });
    }

    /// child\[a\] → parent: a ping addressed to the parent's own id. Record the
    /// cell with the source the parent read (the membrane's own-id path).
    #[handler::manual]
    fn on_matrix_ping(&mut self, ctx: &mut WasmCtx<'_, Erased, Manual>, ping: MatrixPing) {
        record_cell(ping.cell, ctx.sender().map_or(0, |sender| sender.id().0));
    }

    /// Read the cluster's shared observation log and reply the structured
    /// matrix report. Sent after `RunMatrix` has fully settled.
    #[handler::manual]
    fn on_collect_matrix(&mut self, ctx: &mut WasmCtx<'_, Erased, Manual>, _query: CollectMatrix) {
        if ctx.reply_target().is_some() {
            ctx.reply(&snapshot_report());
        }
    }
}

/// Inline child co-located beneath `MatrixParent`, the only actor that builds
/// this fixed matrix topology. It declares that exact parent rather than
/// general module-child composability; `Instanced` satisfies the spawn bound.
pub struct MatrixChild;

#[actor(instanced, child_of(MatrixParent))]
impl WasmActor for MatrixChild {
    const NAMESPACE: &'static str = "test.matrix.child";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(MatrixChild)
    }

    /// Record the ping's cell with the source the child read, then — when the
    /// ping is the fan-out ping (parent → child\[a\]) — drive the child-origin
    /// cells and the cross-cluster send, all in place.
    #[handler::manual]
    fn on_matrix_ping(&mut self, ctx: &mut WasmCtx<'_, Erased, Manual>, ping: MatrixPing) {
        record_cell(ping.cell, ctx.sender().map_or(0, |sender| sender.id().0));

        if ping.fan_out == 0 {
            return;
        }

        // child[a] → parent (in place): the parent records its own cell.
        if let Some(parent) = ctx.parent() {
            parent.send(&MatrixPing { cell: MATRIX_CELL_CHILD_TO_PARENT, fan_out: 0 });
        }

        // child[a] → sibling child[b] (in place): the sibling records its cell.
        if let Some(sibling) = ctx.sibling("b") {
            sibling.send(&MatrixPing { cell: MATRIX_CELL_CHILD_TO_SIBLING, fan_out: 0 });
        }

        // child[a] → self (in place): a child resolves itself as the child
        // of its own parent named with its own subname (`a`), routed in place
        // back to its own alias.
        if let Some(self_handle) = ctx.sibling("a").or_else(|| ctx.child("a")) {
            self_handle.send(&MatrixPing { cell: MATRIX_CELL_CHILD_TO_SELF, fan_out: 0 });
        }

        // Cross-cluster send *during the in-place drain*: addressed through
        // the reference the parent minted from its declared dependency and
        // left in the cluster-shared log, so it still takes the host send
        // path. The send threads this child's own id (`ctx.mailbox`, ==
        // child[a] during the drain) as the `from`, so the observer's
        // `sender()` reads child[a]'s id — the host stamps the
        // guest-carried, in-cluster-validated origin (issue 1987).
        if let Some(observer) = observer() {
            ctx.send_to(observer, &SourceQuery);
        }
    }
}
