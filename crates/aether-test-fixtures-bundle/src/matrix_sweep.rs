//! Issue 1977 (ADR-0114 amendment) cluster-addressing matrix fixture. A
//! multi-actor module — `export!(public = [MatrixParent, MatrixChild])` — whose entry
//! `MatrixParent` forms a small cluster: it spawns two co-located inline
//! children (`a` and `b`) in `wire`. On a `RunMatrix` command (sent over the
//! wire) the parent drives every in-cluster addressing direction in place,
//! plus one cross-cluster send made *during the in-place drain*; each
//! participant records the cell it observed — whether the mail arrived and
//! whether `ctx.sender()` was the proof of the actor that sent it. A
//! follow-up `CollectMatrix` query reads the cluster's shared observation log
//! and replies a `MatrixReport`.
//!
//! Matrix cells (each asserts delivery AND a sender verdict). The recipient
//! compares `ctx.sender()` with the proof it holds of the expected sender, so
//! the verdict is computed in the guest and no position leaves the cluster:
//!
//! - parent → child\[a\] (in place): child\[a\]'s sender is `ctx.parent()`'s
//!   proof.
//! - child\[a\] → parent (in place): the parent's sender is
//!   `ctx.child_as::<MatrixChild>("a")`'s proof.
//! - child\[a\] → sibling child\[b\] (in place): child\[b\]'s sender is
//!   `ctx.sibling_as::<MatrixChild>("a")`'s proof.
//! - child\[a\] → self (in place): child\[a\]'s sender is
//!   `ctx.sibling_as::<MatrixChild>("a")`'s proof, its own.
//! - cross-cluster (child\[a\] → a second loaded component, *during the
//!   drain*): the observer replies a `SourceReport` to the origin the host
//!   stamped, and child\[a\] counts the arrival. The member's ctx-mediated
//!   send threads its own id as the send's `from`, so the host stamps the
//!   member as origin (validated host-side to the cluster), not the
//!   cluster's inbound parent; a mis-stamp would land the reply on the
//!   parent instead.
//! - cross-cluster (the parent → the same observer, before the fan-out): the
//!   parent's own query brings exactly one reply back to the parent.
//!
//! A report counts only when its `had_sender` is set, so an observer whose
//! `ctx.sender()` read `None` for this component-origin mail shows as a
//! missing report.
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
    ActorInitError, ActorRef, Erased, ErasedActorRef, InlineChild, Manual, RelativeMailbox, Subname, WasmActor,
    WasmCtx, WasmInitCtx, actor,
};
use aether_test_fixtures_kinds::{
    CollectMatrix, MATRIX_CELL_CHILD_TO_PARENT, MATRIX_CELL_CHILD_TO_SELF, MATRIX_CELL_CHILD_TO_SIBLING,
    MATRIX_CELL_PARENT_TO_CHILD, MatrixPing, MatrixReport, RunMatrix, SourceQuery, SourceReport,
};

use super::source_observer::SourceObserver;

/// One cell's recorded observation: whether the mail arrived and whether the
/// recipient's `ctx.sender()` was the proof of the expected sender.
#[derive(Clone, Copy, Default)]
struct Cell {
    arrived: bool,
    sender_matched: bool,
}

/// The cluster-shared observation log. Indexed by the `MATRIX_CELL_*`
/// markers (1-based; index 0 is unused), plus the observer reports that
/// landed on the parent and on a child, plus the cross-cluster observer
/// reference the parent minted from its declared dependency. The reference is
/// shared through the log rather than threaded on `MatrixPing` because a
/// proven reference has no codec (ADR-0230); it never leaves this module
/// instance.
struct MatrixLog {
    cells: [Cell; 5],
    reports_to_parent: u32,
    reports_to_child: u32,
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
        cells: [Cell { arrived: false, sender_matched: false }; 5],
        reports_to_parent: 0,
        reports_to_child: 0,
        observer: None,
    }),
};

/// Record `cell` (a `MATRIX_CELL_*` marker) as arrived into the shared log,
/// with whether its sender matched the expected proof.
fn record_cell(cell: u32, sender_matched: bool) {
    // SAFETY: see `LogSlot`'s `Sync` impl — single-threaded guest, borrow
    // taken fresh and released before return.
    let log = unsafe { &mut *MATRIX_LOG.inner.get() };
    if let Some(slot) = log.cells.get_mut(cell as usize) {
        slot.arrived = true;
        slot.sender_matched = sender_matched;
    }
}

/// Count one observer report with a sender proof that landed on the parent.
fn record_report_to_parent() {
    // SAFETY: see `LogSlot`'s `Sync` impl.
    let log = unsafe { &mut *MATRIX_LOG.inner.get() };
    log.reports_to_parent += 1;
}

/// Count one observer report with a sender proof that landed on a child.
fn record_report_to_child() {
    // SAFETY: see `LogSlot`'s `Sync` impl.
    let log = unsafe { &mut *MATRIX_LOG.inner.get() };
    log.reports_to_child += 1;
}

/// Whether `sender` is present and equals the `expected` proof. A missing
/// expectation never matches.
fn sender_matches(sender: Option<ErasedActorRef>, expected: Option<ErasedActorRef>) -> bool {
    sender.is_some() && sender == expected
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
        parent_to_child_sender_matched: u32::from(p2c.sender_matched),
        child_to_parent_arrived: u32::from(c2p.arrived),
        child_to_parent_sender_matched: u32::from(c2p.sender_matched),
        child_to_sibling_arrived: u32::from(c2s.arrived),
        child_to_sibling_sender_matched: u32::from(c2s.sender_matched),
        child_to_self_arrived: u32::from(c2self.arrived),
        child_to_self_sender_matched: u32::from(c2self.sender_matched),
        observer_reports_to_parent: log.reports_to_parent,
        observer_reports_to_child: log.reports_to_child,
    }
}

/// Entry export — the loaded component and cluster root. Spawns the two
/// inline children in `wire`, drives the sweep on `RunMatrix`, records the
/// child\[a\] → parent cell when it arrives, counts the observer reports
/// that land on it, and answers `CollectMatrix`.
pub struct MatrixParent;

// The cross-cluster recipient is a declared dependency, which is what turns
// it into a reference the parent can hold: a dependency is a root singleton
// (ADR-0241 §5), so the fold lands at its published name from any caller. The
// child reads the reference back rather than declaring the dependency itself.
#[actor(root, depends(SourceObserver), spawns(MatrixChild))]
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

    /// Drive the sweep: record the proven observer reference, query the
    /// observer once so its reply comes back to the parent (the witness that
    /// the host stamps the parent as the origin of its own send), then send
    /// the fan-out ping to child\[a\] in place. Child\[a\]'s handler drives the
    /// child-origin cells (child → parent / sibling / self) and the
    /// cross-cluster send. Everything settles in this one receive's drain. The
    /// handler spells its actor type because `actor_ref` and the flat `send`
    /// are bounded `A: DependsOn<R>`.
    #[handler::single]
    fn on_run_matrix(&mut self, ctx: &mut WasmCtx<'_, MatrixParent>, _msg: RunMatrix) {
        record_observer(ctx.actor_ref::<SourceObserver>());
        ctx.send::<SourceObserver>(&SourceQuery);

        let child_a = ctx.child("a").expect("inline child a is resident");
        child_a.send(&MatrixPing { cell: MATRIX_CELL_PARENT_TO_CHILD, fan_out: 1 });
    }

    /// child\[a\] → parent: a ping addressed to the parent's own id. Record the
    /// cell with whether the parent's sender is its proof of child\[a\] (the
    /// membrane's own-id path).
    #[handler::manual]
    fn on_matrix_ping(&mut self, ctx: &mut WasmCtx<'_, Erased, Manual>, ping: MatrixPing) {
        let expected = ctx.child_as::<MatrixChild>("a").map(InlineChild::erase);
        record_cell(ping.cell, sender_matches(ctx.sender(), expected));
    }

    /// The observer's reply to the parent's own query, routed to the origin
    /// the host stamped on it.
    #[handler::single]
    fn on_source_report(&mut self, _ctx: &mut WasmCtx<'_>, report: SourceReport) {
        if report.had_sender {
            record_report_to_parent();
        }
    }

    /// Read the cluster's shared observation log and reply the structured
    /// matrix report. Sent after `RunMatrix` has fully settled.
    #[handler::single]
    fn on_collect_matrix(&mut self, _ctx: &mut WasmCtx<'_>, _query: CollectMatrix) -> MatrixReport {
        snapshot_report()
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

    /// Record the ping's cell with whether the child's sender is the proof of
    /// the cell's expected sender — the parent for the fan-out ping, child\[a\]
    /// for the sibling and self pings — then, when the ping is the fan-out ping
    /// (parent → child\[a\]), drive the child-origin cells and the
    /// cross-cluster send, all in place.
    #[handler::manual]
    fn on_matrix_ping(&mut self, ctx: &mut WasmCtx<'_, Erased, Manual>, ping: MatrixPing) {
        let expected = match ping.cell {
            MATRIX_CELL_PARENT_TO_CHILD => ctx.parent().as_ref().map(RelativeMailbox::reference),
            MATRIX_CELL_CHILD_TO_SIBLING | MATRIX_CELL_CHILD_TO_SELF => {
                ctx.sibling_as::<MatrixChild>("a").map(InlineChild::erase)
            }
            _ => None,
        };
        record_cell(ping.cell, sender_matches(ctx.sender(), expected));

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
        // `sender()` proves child[a] and its reply lands back here — the
        // host stamps the guest-carried, in-cluster-validated origin (issue
        // 1987).
        if let Some(observer) = observer() {
            ctx.send_to(observer, &SourceQuery);
        }
    }

    /// The observer's reply to child\[a\]'s cross-cluster query, routed to
    /// the origin the host stamped on it.
    #[handler::single]
    fn on_source_report(&mut self, _ctx: &mut WasmCtx<'_>, report: SourceReport) {
        if report.had_sender {
            record_report_to_child();
        }
    }
}
