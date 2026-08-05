//! Cross-thread channel from the chassis-control handler and the pumped
//! render slot's wake to the standalone binary's event loop (ADR-0067,
//! ADR-0161). The `aether.substrate_harness.advance` handler runs on a
//! scheduler worker; the loop runs on the main thread — this channel carries
//! the wake.
//!
//! `Advance` carries the reply target so the loop can reply once all ticks
//! complete. `RenderMail` is the pumped render slot's wake — "mail landed on
//! the render slot, drain it" — installed on the slot's `MailboxWakeSlot`
//! (mirroring desktop's `UserEvent::WindowMail`); the in-process harness never
//! installs that wake, so the variant is the binary's alone.

use std::sync::mpsc;
use std::time::Duration;

use aether_substrate::Source;

/// Events the event loop consumes. Single-consumer (the loop); the producers
/// are the `aether.substrate_harness.advance` handler (`Advance`) and the
/// pumped render slot's mailbox wake (`RenderMail`), so the underlying mpsc
/// channel tolerates the two.
pub enum ChassisEvent {
    /// `aether.substrate_harness.advance { ticks, delta_micros }`. The event
    /// loop runs `ticks` full cycles (advance → frame mail → drain), each
    /// representing `delta_micros` elapsed time, then replies with
    /// `AdvanceResult::Ok { ticks_completed }`.
    Advance { reply_to: Source, ticks: u32, delta_micros: u32 },
    /// The pumped `aether.render` slot took mail — drain it (ADR-0161). A
    /// wake-only signal, mirroring desktop's `UserEvent::WindowMail`: the
    /// slot's wake sends it so a render mail landing while the loop is parked
    /// (a `capture_frame` on an occluded chassis, a `pre_settled` notice) is
    /// serviced. Sent only by the standalone binary's render wake — the
    /// in-process harness drains the slot every pump iteration and installs
    /// no wake.
    RenderMail,
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

    /// Block until the next event arrives, the sender is dropped, or
    /// `timeout` elapses. The standalone binary parks on this while a
    /// capture deadline is pending (ADR-0161), so a wedged pre-mail chain on
    /// an otherwise-idle chassis still reaches the actor's deadline check.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<ChassisEvent, mpsc::RecvTimeoutError> {
        self.0.recv_timeout(timeout)
    }

    /// Non-blocking peek. Returns `Empty` immediately when no event
    /// is queued and `Disconnected` when every sender is gone. The
    /// in-process `SubstrateHarness` driver uses this to drain events
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
