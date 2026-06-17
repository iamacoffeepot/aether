//! Issue 1977 (ADR-0114 amendment) cluster-addressing matrix fixture. A
//! multi-actor module — `export!(MatrixParent, MatrixChild)` — whose entry
//! `MatrixParent` forms a small cluster: it spawns two co-located inline
//! children (`a` and `b`) in `wire`. On a `RunMatrix` command (sent over the
//! wire) the parent drives every in-cluster addressing direction in place,
//! plus one cross-cluster send made *during the in-place drain*; each
//! participant records the cell it observed — whether the mail arrived and
//! what `ctx.source_mailbox()` it read. A follow-up `CollectMatrix` query
//! reads the cluster's shared observation log and replies a `MatrixReport`.
//!
//! Matrix cells (each asserts delivery AND the source the recipient read):
//!
//! - parent → child[a] (in place): child[a]'s source is the parent's id.
//! - child[a] → parent (in place): the parent's source is child[a]'s id.
//! - child[a] → sibling child[b] (in place): child[b]'s source is child[a]'s id.
//! - child[a] → self (in place): child[a]'s source is its own id.
//! - cross-cluster (child[a] → a second loaded component, *during the drain*):
//!   observed out-of-band by the observer (read via `log_tail`). The observer
//!   reads child[a]'s id: the member's ctx-mediated `send_to` threads its own
//!   id as the send's `from`, so the host stamps the member as origin
//!   (validated host-side to the cluster), not the cluster's inbound parent.
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

extern crate alloc;

use core::cell::UnsafeCell;

use aether_actor::{
    BootError, FfiActor, FfiCtx, FfiInitCtx, Instanced, MailboxId, Manual, OutboundReply, Subname,
    actor,
};
use aether_test_fixtures::{
    CollectMatrix, MATRIX_CELL_CHILD_TO_PARENT, MATRIX_CELL_CHILD_TO_SELF,
    MATRIX_CELL_CHILD_TO_SIBLING, MATRIX_CELL_PARENT_TO_CHILD, MatrixPing, MatrixReport, RunMatrix,
    SourceQuery,
};

/// One cell's recorded observation: whether the mail arrived and the raw
/// `MailboxId` the recipient read from `ctx.source_mailbox()`.
#[derive(Clone, Copy, Default)]
struct Cell {
    arrived: bool,
    source: u64,
}

/// The cluster-shared observation log. Indexed by the `MATRIX_CELL_*`
/// markers (1-based; index 0 is unused), plus the resolved parent / child[a]
/// ids the parent records so the test can assert the sources against the
/// actual folded addresses.
struct MatrixLog {
    cells: [Cell; 5],
    parent_id: u64,
    child_a_id: u64,
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
        cells: [Cell {
            arrived: false,
            source: 0,
        }; 5],
        parent_id: 0,
        child_a_id: 0,
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

/// Record the resolved parent / child[a] ids the parent learned at sweep
/// start, so the test can assert each source against the real folded address.
fn record_ids(parent_id: u64, child_a_id: u64) {
    // SAFETY: see `LogSlot`'s `Sync` impl.
    let log = unsafe { &mut *MATRIX_LOG.inner.get() };
    log.parent_id = parent_id;
    log.child_a_id = child_a_id;
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
/// child[a] → parent cell when it arrives, and answers `CollectMatrix`.
pub struct MatrixParent;

#[actor]
impl FfiActor for MatrixParent {
    const NAMESPACE: &'static str = "test.matrix.parent";

    fn init(_ctx: &mut FfiInitCtx<'_>) -> Result<Self, BootError> {
        Ok(MatrixParent)
    }

    /// Co-locate two inline children under the `Named` subnames `a` and `b`,
    /// the cluster's leaf nodes.
    fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
        let _ = ctx.spawn_inline_child::<MatrixChild>(Subname::Named("a"), &());
        let _ = ctx.spawn_inline_child::<MatrixChild>(Subname::Named("b"), &());
    }

    /// Drive the sweep: record the parent / child[a] ids, then send the
    /// fan-out ping to child[a] in place. Child[a]'s handler drives the
    /// child-origin cells (child → parent / sibling / self) and the
    /// cross-cluster send. Everything settles in this one receive's drain.
    #[handler]
    fn on_run_matrix(&mut self, ctx: &mut FfiCtx<'_>, msg: RunMatrix) {
        let parent_id = ctx.mailbox_id();
        let child_a = ctx.child("a").expect("inline child a is resident");
        record_ids(parent_id, child_a.mailbox_id().0);
        child_a.send(&MatrixPing {
            cell: MATRIX_CELL_PARENT_TO_CHILD,
            fan_out: 1,
            observer_mailbox: msg.observer_mailbox,
        });
    }

    /// child[a] → parent: a ping addressed to the parent's own id. Record the
    /// cell with the source the parent read (the membrane's own-id path).
    #[handler::manual]
    fn on_matrix_ping(&mut self, ctx: &mut FfiCtx<'_, Manual>, ping: MatrixPing) {
        record_cell(ping.cell, ctx.source_mailbox().map_or(0, |m| m.0));
    }

    /// Read the cluster's shared observation log and reply the structured
    /// matrix report. Sent after `RunMatrix` has fully settled.
    #[handler::manual]
    fn on_collect_matrix(&mut self, ctx: &mut FfiCtx<'_, Manual>, _query: CollectMatrix) {
        if ctx.reply_target().is_some() {
            ctx.reply(&snapshot_report());
        }
    }
}

/// Inline child — co-located in the parent's wasm instance. `Instanced`
/// satisfies the `spawn_inline_child` bound; it rides the `export!` list so
/// the multi-actor module's type set includes it.
pub struct MatrixChild;

impl Instanced for MatrixChild {}

#[actor]
impl FfiActor for MatrixChild {
    const NAMESPACE: &'static str = "test.matrix.child";

    fn init(_ctx: &mut FfiInitCtx<'_>) -> Result<Self, BootError> {
        Ok(MatrixChild)
    }

    /// Record the ping's cell with the source the child read, then — when the
    /// ping is the fan-out ping (parent → child[a]) — drive the child-origin
    /// cells and the cross-cluster send, all in place.
    #[handler::manual]
    fn on_matrix_ping(&mut self, ctx: &mut FfiCtx<'_, Manual>, ping: MatrixPing) {
        record_cell(ping.cell, ctx.source_mailbox().map_or(0, |m| m.0));

        if ping.fan_out == 0 {
            return;
        }

        // child[a] → parent (in place): the parent records its own cell.
        if let Some(parent) = ctx.parent() {
            parent.send(&MatrixPing {
                cell: MATRIX_CELL_CHILD_TO_PARENT,
                fan_out: 0,
                observer_mailbox: 0,
            });
        }

        // child[a] → sibling child[b] (in place): the sibling records its cell.
        if let Some(sibling) = ctx.sibling("b") {
            sibling.send(&MatrixPing {
                cell: MATRIX_CELL_CHILD_TO_SIBLING,
                fan_out: 0,
                observer_mailbox: 0,
            });
        }

        // child[a] → self (in place): a child resolves itself as the child
        // of its own parent named with its own subname (`a`), routed in place
        // back to its own alias.
        if let Some(self_handle) = ctx.sibling("a").or_else(|| ctx.child("a")) {
            self_handle.send(&MatrixPing {
                cell: MATRIX_CELL_CHILD_TO_SELF,
                fan_out: 0,
                observer_mailbox: 0,
            });
        }

        // Cross-cluster send *during the in-place drain*: addressed by the
        // observer's raw `MailboxId` via the ctx-mediated `send_to`, so it
        // takes the host send path. The send threads this child's own id
        // (`ctx.mailbox`, == child[a] during the drain) as the `from`, so the
        // observer's `source_mailbox()` reads child[a]'s id — the host stamps
        // the guest-carried, in-cluster-validated origin (issue 1987).
        if ping.observer_mailbox != 0 {
            ctx.send_to(MailboxId(ping.observer_mailbox), &SourceQuery);
        }
    }
}

aether_actor::export!(MatrixParent, MatrixChild);
