//! The teardown gate: how a chassis closes the pooled slots that are still
//! open when its engine goes down (ADR-0247 rule 5).
//!
//! A pooled actor closes on a pool worker, never on the thread tearing the
//! chassis down (ADR-0165), so teardown cannot close a slot itself. It tells
//! the slot the engine is going, wakes it once so a worker runs its close
//! cycle, and waits for the cycle's close-done signal. [`TeardownGate::close`]
//! is that sequence, and both teardown walks call it: the one over the
//! instanced actors the spawner still holds, and the one over the composed
//! roots. An idle slot is woken like a busy one, so an actor that received
//! no mail still runs its `unwire`.

use std::time::Duration;

use crate::chassis::settlement::{TerminalDisposition, WaitOutcome, await_internal_signal};
use crate::runtime::lifecycle::{FatalAbortRecord, FatalAborter};
use crate::scheduler::{Drainable, WakeHandle};

/// The patience one chassis teardown waits under, and where a wedge goes.
pub struct TeardownGate<'a> {
    /// The per-round patience interval: the cadence of the slow-gate log.
    pub round_budget: Duration,
    /// The total patience per slot before its close is declared wedged.
    pub cumulative_cap: Duration,
    /// The chassis's first fatal abort, if any. A panicking handler unwinds
    /// its worker under [`PanicAborter`](crate::runtime::lifecycle::PanicAborter),
    /// so the slot it was mid-turn on never fires close-done; watching the
    /// record makes the gate report that abort instead of waiting the cap out
    /// and reporting a bare timeout (issue #4193).
    pub abort_record: &'a FatalAbortRecord,
    /// The chassis aborter a release build routes a wedge through.
    pub aborter: &'a dyn FatalAborter,
}

/// One pooled slot a teardown walk closes.
pub struct ClosingSlot<'a> {
    /// Names the wait in the slow-gate log and in the wedge, so a close that
    /// never ran points at its actor (issue #2509).
    pub gate: String,
    pub slot: &'a dyn Drainable,
    pub wake: &'a WakeHandle,
}

impl ClosingSlot<'_> {
    /// Tell the slot the engine is going and wake it, answering the receiver
    /// its close cycle fires.
    ///
    /// The close-done sender goes on the slot before the signal, so a close
    /// cycle that starts the moment the flag lands still finds it. A slot
    /// that already closed fires it at once.
    fn begin(&self) -> crossbeam_channel::Receiver<()> {
        let (close_done_tx, close_done) = crossbeam_channel::bounded::<()>(1);
        self.slot.set_close_done_tx(close_done_tx);
        self.slot.signal_engine_teardown();
        // Whether this wake won the scheduling race is irrelevant: a slot
        // already queued or running reads the flag on its own.
        let _ = self.wake.wake();
        close_done
    }
}

impl TeardownGate<'_> {
    /// Close every slot in `closing`: signal and wake them all, then wait
    /// for each one's close-done signal in turn. The pool must be up, since
    /// its workers run the close cycles.
    ///
    /// Each wait is [`await_internal_signal`] with escalating patience
    /// rather than a wall-clock deadline (issue #1305), which false-fired
    /// under load. A close cycle that never runs left `unwire` unrun, so a
    /// wedge is unrecoverable: it panics at the gate in a debug build, where
    /// the failure is then attributable, and aborts through the chassis
    /// aborter in a release build. This is also what bounds a guest whose
    /// `unwire` hangs at teardown.
    pub fn close(&self, closing: &[ClosingSlot<'_>]) {
        let waiters: Vec<crossbeam_channel::Receiver<()>> = closing.iter().map(ClosingSlot::begin).collect();
        let disposition = if cfg!(debug_assertions) {
            TerminalDisposition::Panic
        } else {
            TerminalDisposition::Abort
        };

        for (slot, close_done) in closing.iter().zip(&waiters) {
            let outcome = await_internal_signal(
                close_done,
                &slot.gate,
                self.round_budget,
                self.cumulative_cap,
                disposition,
                Some(self.abort_record),
            );
            if let WaitOutcome::Wedged(wedge) = outcome {
                self.aborter.abort(wedge.reason());
            }
        }
    }
}
