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

use crate::mail::{Mail, MailKind, MailboxId};
use crate::queue::MailQueue;
use crate::registry::{MailboxEntry, Registry};

pub struct SubstrateCtx {
    pub sender: MailboxId,
    pub registry: Arc<Registry>,
    pub queue: Arc<MailQueue>,
}

impl SubstrateCtx {
    /// Dispatch mail. If the recipient is a sink, the handler runs inline
    /// on the caller's thread. If it's a component, the mail is enqueued
    /// for a worker to deliver. Unknown recipients are dropped.
    pub fn send(&self, recipient: MailboxId, kind: MailKind, payload: Vec<u8>, count: u32) {
        match self.registry.entry(recipient) {
            Some(MailboxEntry::Sink(handler)) => {
                let kind_name = self.registry.kind_name(kind).unwrap_or("");
                // Component-originated mail: the sender is this ctx's
                // mailbox, so its registry name is the `origin` any
                // sink cares about (ADR-0011). Claude-side session is
                // `NIL` — component sends never have a reply-to target.
                let origin = self.registry.mailbox_name(self.sender);
                handler(kind_name, origin, SessionToken::NIL, &payload, count);
            }
            Some(MailboxEntry::Component) => {
                self.queue.push(Mail::new(recipient, kind, payload, count));
            }
            None => {
                eprintln!(
                    "substrate: dropped mail from {:?} to unknown mailbox {:?}",
                    self.sender, recipient
                );
            }
        }
    }
}
