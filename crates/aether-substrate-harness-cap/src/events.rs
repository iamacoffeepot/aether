//! Cross-thread channel from the chassis-control handler to the in-process
//! harness's pump (ADR-0067, ADR-0161). The `aether.substrate_harness.advance`
//! handler runs on a scheduler worker; the pump runs on the caller's thread —
//! this channel carries the request and the wake.
//!
//! `Advance` carries the request's retained inbound guard so the pump can
//! reply through it once all ticks complete; the request's causal chain stays
//! open until then, because the guard records its `Finished` on drop, after
//! the reply's `Sent`.

use std::sync::mpsc;

use aether_substrate::InboundMail;
use aether_substrate::chassis::ctx::MailboxWakeFn;

/// Events the pump consumes. Single-consumer (the in-process harness's pump);
/// the one producer is the `aether.substrate_harness.advance` handler.
pub enum ChassisEvent {
    /// `aether.substrate_harness.advance { ticks, delta_micros }`. The pump
    /// runs `ticks` full cycles (advance → frame mail → drain), each
    /// representing `delta_micros` elapsed time, then replies with
    /// `AdvanceResult::Ok { ticks_completed }` through `reply`, the handler's
    /// retained inbound. The guard answers every sender kind (a wire `Call`
    /// names the rpc server's mailbox) and holds the request's chain open
    /// until it drops after the reply, so a caller awaiting settlement sees
    /// the reply first.
    Advance { reply: Box<InboundMail>, ticks: u32, delta_micros: u32 },
}

/// The producer half. `wake` fires after each send, so a consumer that blocks
/// on a wake channel shared with other sources learns the event is queued.
#[derive(Clone)]
pub struct EventSender {
    tx: mpsc::Sender<ChassisEvent>,
    wake: MailboxWakeFn,
}

impl EventSender {
    /// Push an event, then fire the wake: the event is queued before its
    /// wake is, so a consumer woken by it finds the event. Returns `Ok(())`
    /// on success, `Err` only if the receiver has been dropped — at that
    /// point the chassis is shutting down and the failure is informational.
    pub fn send(&self, event: ChassisEvent) -> Result<(), mpsc::SendError<ChassisEvent>> {
        self.tx.send(event)?;
        (self.wake)();
        Ok(())
    }
}

pub struct EventReceiver(mpsc::Receiver<ChassisEvent>);

impl EventReceiver {
    /// Non-blocking peek. Returns `Empty` immediately when no event
    /// is queued and `Disconnected` when every sender is gone. The
    /// in-process `SubstrateHarness` driver uses this to drain events
    /// inline between queue settles.
    pub fn try_recv(&self) -> Result<ChassisEvent, mpsc::TryRecvError> {
        self.0.try_recv()
    }
}

/// Build the sender/receiver pair the chassis wires once at boot. `wake`
/// fires after each send; the in-process harness passes the wake of the one
/// channel its pump loop blocks on.
#[must_use]
pub fn channel(wake: MailboxWakeFn) -> (EventSender, EventReceiver) {
    let (tx, rx) = mpsc::channel();
    (EventSender { tx, wake }, EventReceiver(rx))
}
