// The per-component context stored as wasmtime `Store` data. Holds the
// sender's own `MailboxId`, a handle to the shared mail queue, and a
// handle to the registry so the `send_mail` host function can route
// without consulting the scheduler's internals.
//
// Deliberately does NOT hold the scheduler's full shared state — doing
// so would create an Arc cycle through `Scheduler owns Actor, Actor
// owns Store<SubstrateCtx>, SubstrateCtx back to Scheduler`. By holding
// only `Arc<Registry>` and `Arc<MailQueue>` the cycle is broken: neither
// of those owns any actor.

use std::sync::Arc;

use aether_hub_protocol::SessionToken;

use crate::hub_client::HubOutbound;
use crate::mail::{Mail, MailKind, MailboxId};
use crate::queue::MailQueue;
use crate::registry::{MailboxEntry, Registry};
use crate::sender_table::SenderTable;

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
    pub queue: Arc<MailQueue>,
    /// ADR-0013: direct outbound handle so the `reply_mail` host fn
    /// can address a specific Claude session without routing through
    /// a well-known sink. Broadcast still goes through
    /// `hub.claude.broadcast`; reply is the session-targeted twin.
    /// `HubOutbound::disconnected` when no hub is attached — sends
    /// silently drop, matching the broadcast semantics.
    pub outbound: Arc<HubOutbound>,
    /// ADR-0013 + ADR-0017: handle→entry map populated by
    /// `Component::deliver` whenever an inbound mail has a meaningful
    /// reply target — a Claude session (`SenderEntry::Session`) or
    /// another component (`SenderEntry::Component`). The guest
    /// receives an opaque `u32` handle as the 4th param on its
    /// `receive` shim and passes it back to `reply_mail`; the
    /// substrate routes either over `HubOutbound` or back through
    /// `MailQueue` based on the variant.
    pub sender_table: SenderTable,
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
}

impl SubstrateCtx {
    /// Build a fresh ctx with empty state-migration slots and an
    /// empty sender table. Using this over the struct literal keeps
    /// the private fields (sender_table, saved_state,
    /// save_state_error) internal to the wiring — callers should
    /// never set them directly.
    pub fn new(
        sender: MailboxId,
        registry: Arc<Registry>,
        queue: Arc<MailQueue>,
        outbound: Arc<HubOutbound>,
    ) -> Self {
        SubstrateCtx {
            sender,
            registry,
            queue,
            outbound,
            sender_table: SenderTable::new(),
            saved_state: None,
            save_state_error: None,
        }
    }

    /// Dispatch mail. If the recipient is a sink, the handler runs inline
    /// on the caller's thread. If it's a component, the mail is enqueued
    /// for a worker to deliver. Unknown recipients are dropped.
    pub fn send(&self, recipient: MailboxId, kind: MailKind, payload: Vec<u8>, count: u32) {
        match self.registry.entry(recipient) {
            Some(MailboxEntry::Sink(handler)) => {
                let kind_name = self.registry.kind_name(kind).unwrap_or_default();
                // Component-originated mail: the sender is this ctx's
                // mailbox, so its registry name is the `origin` any
                // sink cares about (ADR-0011). Claude-side session is
                // `NIL` — component sends never have a reply-to target.
                let origin = self.registry.mailbox_name(self.sender);
                handler(
                    &kind_name,
                    origin.as_deref(),
                    SessionToken::NIL,
                    &payload,
                    count,
                );
            }
            Some(MailboxEntry::Component) => {
                // ADR-0017: component-to-component mail carries the
                // sender's mailbox id so `Component::deliver` can
                // allocate a Component-variant `SenderEntry`. The
                // receiving guest gets a reply-capable handle that
                // routes back through the local queue.
                self.queue
                    .push(Mail::new(recipient, kind, payload, count).with_origin(self.sender));
            }
            Some(MailboxEntry::Dropped) => {
                tracing::warn!(
                    target: "aether_substrate::ctx",
                    sender = ?self.sender,
                    mailbox = ?recipient,
                    "component sent mail to dropped mailbox — discarded",
                );
            }
            None => {
                tracing::warn!(
                    target: "aether_substrate::ctx",
                    sender = ?self.sender,
                    mailbox = ?recipient,
                    "component sent mail to unknown mailbox — dropped",
                );
            }
        }
    }
}
