// The per-component context stored as wasmtime `Store` data. Holds the
// sender's own `MailboxId`, a handle to the shared mail queue, and a
// handle to the registry so the `send_mail` host function can route
// without consulting the scheduler's internals.
//
// Deliberately does NOT hold the scheduler's full shared state — doing
// so would create an Arc cycle through `Scheduler owns Actor, Actor
// owns Store<SubstrateCtx>, SubstrateCtx back to Scheduler`. By holding
// only `Arc<Registry>` and `Arc<Mailer>` the cycle is broken: neither
// of those owns any actor.

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use crate::hub_client::HubOutbound;
use crate::input::InputSubscribers;
use crate::mail::{Mail, MailKind, MailboxId, ReplyTarget, ReplyTo};
use crate::mailer::Mailer;
use crate::registry::{MailboxEntry, Registry};
use crate::reply_table::ReplyTable;

/// ADR-0016 §3: opt-in state migration payload. The substrate owns the
/// buffer from the moment `save_state` is called on the old instance
/// until the bundle is handed to the new instance via `on_rehydrate`
/// (or discarded if no successor consumes it). Both fields are opaque
/// to the substrate — the component owns versioning and the byte layout.
#[derive(Debug, Clone)]
pub struct StateBundle {
    pub version: u32,
    pub bytes: Vec<u8>,
}

pub struct SubstrateCtx {
    pub sender: MailboxId,
    pub registry: Arc<Registry>,
    pub queue: Arc<Mailer>,
    /// ADR-0013: direct outbound handle so the `reply_mail` host fn
    /// can address a specific Claude session without routing through
    /// a well-known sink. Broadcast still goes through
    /// `hub.claude.broadcast`; reply is the session-targeted twin.
    /// `HubOutbound::disconnected` when no hub is attached — sends
    /// silently drop, matching the broadcast semantics.
    pub outbound: Arc<HubOutbound>,
    /// ADR-0021 subscriber sets, shared with the platform-event
    /// publisher in `main.rs`. `#[handlers]`-decorated components
    /// auto-subscribe every `K::IS_INPUT` handler kind by mailing
    /// `aether.control.subscribe_input` from the init prologue the
    /// macro prepends (ADR-0033 phase 3), which the control plane
    /// processes and mutates this set.
    pub input_subscribers: InputSubscribers,
    /// ADR-0013 + ADR-0017: handle→entry map populated by
    /// `Component::deliver` whenever an inbound mail has a meaningful
    /// reply target — a Claude session (`ReplyEntry::Session`) or
    /// another component (`ReplyEntry::Component`). The guest
    /// receives an opaque `u32` handle as the 4th param on its
    /// `receive` shim and passes it back to `reply_mail`; the
    /// substrate routes either over `HubOutbound` or back through
    /// `Mailer` based on the variant.
    pub reply_table: ReplyTable,
    /// Set by the `save_state` host fn during `on_replace`. The
    /// substrate extracts it after hooks return via
    /// `Component::take_saved_state`. Never read by the guest —
    /// rehydration reads from a scratch offset written by the
    /// substrate, not from here.
    pub saved_state: Option<StateBundle>,
    /// Set by the `save_state` host fn when it rejects a call (1 MiB
    /// cap exceeded, OOB pointer). ADR-0016 §4: a failing save aborts
    /// the replace; the substrate checks this after `on_replace` and
    /// surfaces the message back up the control plane.
    pub save_state_error: Option<String>,
    /// ADR-0042 inbox machinery: the component's mpsc `Receiver`
    /// lives here (not on the dispatcher's stack) so the
    /// `wait_reply_p32` host fn can drain it directly, and a FIFO
    /// overflow holds non-matching mail pulled during a wait until
    /// the dispatcher drains it ahead of the mpsc on its next pass.
    /// Both slots are populated by `ComponentEntry::spawn` after the
    /// mpsc pair is built; `Component::instantiate` leaves the
    /// `Mutex`es empty / default because it has no scheduler.
    pub inbox_rx: Mutex<Option<Receiver<Mail>>>,
    pub inbox_overflow: Mutex<VecDeque<Mail>>,
    /// ADR-0042 correlation counter. Per-component (one
    /// `SubstrateCtx` per component instance). Holds the *next* id
    /// to mint; `prev_correlation()` reads `counter - 1` to return
    /// the last one minted. Starts at `1` so that `0` always means
    /// "no correlation" (backward-compat sentinel for waits that
    /// don't filter, and for `prev_correlation` before any send).
    ///
    /// `Cell` instead of `AtomicU64`: the component is single-
    /// threaded (ADR-0038 actor-per-component), so the counter is
    /// never touched from multiple threads.
    correlation_counter: Cell<u64>,
}

impl SubstrateCtx {
    /// Build a fresh ctx with empty state-migration slots and an
    /// empty sender table. Using this over the struct literal keeps
    /// the private fields (reply_table, saved_state,
    /// save_state_error) internal to the wiring — callers should
    /// never set them directly.
    pub fn new(
        sender: MailboxId,
        registry: Arc<Registry>,
        queue: Arc<Mailer>,
        outbound: Arc<HubOutbound>,
        input_subscribers: InputSubscribers,
    ) -> Self {
        SubstrateCtx {
            sender,
            registry,
            queue,
            outbound,
            input_subscribers,
            reply_table: ReplyTable::new(),
            saved_state: None,
            save_state_error: None,
            inbox_rx: Mutex::new(None),
            inbox_overflow: Mutex::new(VecDeque::new()),
            correlation_counter: Cell::new(1),
        }
    }

    /// Mint the next correlation id and bump the counter. Private —
    /// callers that want a correlation use `SubstrateCtx::send`,
    /// which mints internally and tags the outgoing mail.
    fn mint_correlation(&self) -> u64 {
        let id = self.correlation_counter.get();
        self.correlation_counter.set(id + 1);
        id
    }

    /// Return the correlation id used by the most recent
    /// `SubstrateCtx::send` call. The `prev_correlation_p32` host fn
    /// surfaces this to the guest so sync wrappers know what to
    /// filter on in `wait_reply_p32`. Returns `0` (the "no
    /// correlation" sentinel) before any send has been made.
    pub fn prev_correlation(&self) -> u64 {
        // counter holds the *next* id to mint; subtract to get the
        // last one. `.saturating_sub(1)` covers the pre-send case
        // where counter is still `1` (initial) → returns `0`.
        self.correlation_counter.get().saturating_sub(1)
    }

    /// Install the mpsc `Receiver` the dispatcher will read from.
    /// Called once by `ComponentEntry::spawn` right after the mpsc
    /// pair is built; `wait_reply_p32` later drains the same
    /// receiver when a guest parks on a reply.
    pub fn install_inbox_rx(&self, rx: Receiver<Mail>) {
        *self.inbox_rx.lock().unwrap() = Some(rx);
    }

    /// Pop one mail for the dispatcher. Drains the overflow buffer
    /// first (FIFO-preserves mail that `wait_reply_p32` set aside
    /// while it was parked), then blocks on the mpsc. `None` when
    /// both are empty and the inbox has been disconnected —
    /// dispatcher_loop treats that as its exit signal.
    pub fn next_mail(&self) -> Option<Mail> {
        if let Some(mail) = self.inbox_overflow.lock().unwrap().pop_front() {
            return Some(mail);
        }
        let rx_guard = self.inbox_rx.lock().unwrap();
        let rx = rx_guard.as_ref()?;
        rx.recv().ok()
    }

    /// Dispatch mail. If the recipient is a sink, the handler runs inline
    /// on the caller's thread. Otherwise defer to the mailer, which
    /// routes to the component's inbox, warn-drops dropped/unknown
    /// mailboxes, or bubbles unknown ids up to the hub-substrate when
    /// a `HubOutbound` is wired (ADR-0037).
    pub fn send(&self, recipient: MailboxId, kind: MailKind, payload: Vec<u8>, count: u32) {
        // ADR-0042: mint a fresh correlation_id for this send and
        // stash it on `last_correlation` so `prev_correlation_p32`
        // can return it to the guest. The minted id rides on the
        // outgoing `ReplyTo.correlation_id`; the reply's echo
        // (auto-routed by `Mailer::send_reply`) carries it back, and
        // `wait_reply_p32` filters on it.
        let correlation = self.mint_correlation();
        let reply_to = ReplyTo::with_correlation(ReplyTarget::Component(self.sender), correlation);

        if let Some(MailboxEntry::Sink(handler)) = self.registry.entry(recipient) {
            let kind_name = self.registry.kind_name(kind).unwrap_or_default();
            // Component-originated mail: the sender is this ctx's
            // mailbox, so its registry name is the `origin` any
            // sink cares about (ADR-0011), and the same mailbox id
            // rides on `reply_to.target` so sink handlers that want
            // to reply (ADR-0041's io sink is the motivating case)
            // can route `*Result` back to this component via
            // `Mailer::send_reply`.
            let origin = self.registry.mailbox_name(self.sender);
            handler(
                kind,
                &kind_name,
                origin.as_deref(),
                reply_to,
                &payload,
                count,
            );
            return;
        }

        // Component / dropped / unknown all funnel through `Mailer::push`:
        // - Component (ADR-0017): mail enters the recipient's inbox with
        //   `from_component = self.sender` so `Component::deliver` can
        //   allocate a Component-variant `ReplyEntry`.
        // - Dropped: warn-drops in `route_mail`.
        // - Unknown (ADR-0037): bubbles up to the hub-substrate via
        //   `MailToHubSubstrate` with `source_mailbox_id = self.sender`
        //   when a `HubOutbound` is connected; warn-drops otherwise.
        self.queue.push(
            Mail::new(recipient, kind, payload, count)
                .with_reply_to(reply_to)
                .with_origin(self.sender),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::RwLock;

    use aether_hub_protocol::EngineToHub;

    use super::*;

    /// ADR-0037 Phase 1 + Phase 2: when a component sends to a mailbox
    /// id the local registry doesn't know, `ctx.send` defers to the
    /// mailer, which emits an upstream `MailToHubSubstrate` frame
    /// carrying the sender's mailbox id so the hub can build a
    /// `ReplyTo::EngineMailbox` for the receiving component.
    #[test]
    fn unknown_recipient_bubbles_up_with_sender_mailbox() {
        let (outbound, outbound_rx) = HubOutbound::attached_loopback();
        let registry = Arc::new(Registry::new());
        let sender = registry.register_component("client");

        let mailer = Arc::new(Mailer::new());
        mailer.wire(Arc::clone(&registry), Arc::new(RwLock::new(HashMap::new())));
        mailer.wire_outbound(Arc::clone(&outbound));

        let ctx = SubstrateCtx::new(
            sender,
            Arc::clone(&registry),
            Arc::clone(&mailer),
            outbound,
            crate::input::new_subscribers(),
        );

        let unknown = MailboxId(0xDEADBEEF_u64);
        let kind: u64 = 0xABCD_u64;
        ctx.send(unknown, kind, vec![1, 2, 3], 1);

        let frame = outbound_rx.try_recv().expect("bubble-up frame emitted");
        match frame {
            EngineToHub::MailToHubSubstrate(f) => {
                assert_eq!(f.recipient_mailbox_id, unknown.0);
                assert_eq!(f.kind_id, kind);
                assert_eq!(f.payload, vec![1, 2, 3]);
                assert_eq!(f.count, 1);
                assert_eq!(f.source_mailbox_id, Some(sender.0));
            }
            other => panic!("expected MailToHubSubstrate, got {other:?}"),
        }
    }

    /// No hub wired (disconnected substrate, or the hub chassis
    /// itself): unknown recipients still warn-drop — no crash, no
    /// upstream frame.
    #[test]
    fn unknown_recipient_without_outbound_warn_drops() {
        let (outbound, outbound_rx) = HubOutbound::attached_loopback();
        let registry = Arc::new(Registry::new());
        let sender = registry.register_component("client");

        let mailer = Arc::new(Mailer::new());
        mailer.wire(Arc::clone(&registry), Arc::new(RwLock::new(HashMap::new())));
        // Deliberately no `wire_outbound`.

        let ctx = SubstrateCtx::new(
            sender,
            Arc::clone(&registry),
            Arc::clone(&mailer),
            outbound,
            crate::input::new_subscribers(),
        );

        ctx.send(MailboxId(0xDEADBEEF_u64), 0xABCD, vec![], 0);
        assert!(
            outbound_rx.try_recv().is_err(),
            "no bubble-up without a wired outbound"
        );
    }
}
