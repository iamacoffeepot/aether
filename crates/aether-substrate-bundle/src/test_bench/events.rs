//! Cross-thread channel from the chassis-control handler to the
//! tick loop (ADR-0067). The handler runs on a scheduler worker;
//! the tick loop runs on the main thread. Both `aether.test_bench.advance`
//! and `aether.render.capture_frame` need to wake the tick loop —
//! this channel carries the wake.
//!
//! `Advance` carries the reply target so the loop can reply once
//! all ticks complete. `CaptureRequested` is a wake signal only;
//! the actual `PendingCapture` rides separately in `CaptureQueue`
//! (the queue stays the source of truth so multiple back-to-back
//! advances don't lose a stale wake event).

use std::sync::mpsc;

use aether_substrate::Source;

/// Events the tick loop consumes. Single-producer / single-consumer
/// in practice — the chassis-control handler is the only producer,
/// the tick loop is the only consumer — but the underlying channel
/// is mpsc which would tolerate multiple producers if a future
/// chassis variant grew them.
pub enum ChassisEvent {
    /// `aether.test_bench.advance { ticks }`. The tick loop runs
    /// `ticks` full cycles (Tick fanout → drain → render or capture)
    /// then replies with `AdvanceResult::Ok { ticks_completed }`.
    Advance { reply_to: Source, ticks: u32 },
    /// `aether.render.capture_frame`. The `PendingCapture` itself
    /// was pushed into `CaptureQueue` by the chassis-control
    /// handler; this event just wakes the loop so the next idle
    /// cycle picks it up. The loop runs one drain → render-with-
    /// capture cycle without dispatching `Tick` (capture observes,
    /// it doesn't advance the world). If the queue is empty when
    /// the loop wakes — possible if an in-flight `Advance` already
    /// drained the capture — the wake is silently absorbed.
    CaptureRequested,
}

#[derive(Clone)]
pub struct EventSender(mpsc::Sender<ChassisEvent>);

impl EventSender {
    /// Push an event. Returns `Ok(())` on success, `Err` only if
    /// the receiver has been dropped — at that point the chassis
    /// is shutting down and the failure is informational.
    pub fn send(&self, event: ChassisEvent) -> Result<(), mpsc::SendError<ChassisEvent>> {
        self.0.send(event)
    }
}

pub struct EventReceiver(mpsc::Receiver<ChassisEvent>);

impl EventReceiver {
    /// Block until the next event arrives or the sender is dropped.
    pub fn recv(&self) -> Result<ChassisEvent, mpsc::RecvError> {
        self.0.recv()
    }

    /// Non-blocking peek. Returns `Empty` immediately when no event
    /// is queued and `Disconnected` when every sender is gone. The
    /// in-process `TestBench` driver uses this to drain events
    /// inline between queue settles.
    ///
    /// The binary's events loop uses `recv` (blocking), not this —
    /// the dead-code lint sees this method as unused when compiling
    /// just the binary, hence the allow.
    #[allow(dead_code)]
    pub fn try_recv(&self) -> Result<ChassisEvent, mpsc::TryRecvError> {
        self.0.try_recv()
    }
}

/// Build the sender/receiver pair the chassis wires once at boot.
#[must_use]
pub fn channel() -> (EventSender, EventReceiver) {
    let (tx, rx) = mpsc::channel();
    (EventSender(tx), EventReceiver(rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recv_errors_after_all_senders_drop() {
        let (tx, rx) = channel();
        drop(tx);
        // No clones outstanding — the receiver returns Err once the
        // last sender goes away. The chassis loop interprets this
        // as shutdown.
        assert!(rx.recv().is_err());
    }
}
