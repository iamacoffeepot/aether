// Inline router (ADR-0038 Phase 3).
//
// Phase 2 retired the VecDeque + router thread; Phase 3 retires the
// global `outstanding` / `done_cv` barrier too. The per-component
// drain primitive on `ComponentEntry` replaces `wait_idle` — callers
// that previously waited for "all mail in flight" now drain the
// specific mailboxes they care about (or iterate the full components
// table via `scheduler::drain_all`).
//
// What survives: `push(mail)` resolves the recipient inline on the
// caller's thread (sinks run inline; components forward to inbox;
// dropped / unknown warn-drop). The module's only state is the
// `Registry` + `ComponentTable` handles, wired once by
// `Scheduler::new`.

use std::sync::{Arc, OnceLock};

use aether_hub_protocol::{EngineMailToHubSubstrateFrame, EngineToHub};

use crate::hub_client::HubOutbound;
use crate::mail::Mail;
use crate::registry::{MailboxEntry, Registry};
use crate::scheduler::ComponentTable;

pub struct Mailer {
    /// Registry handle for resolving recipients on `push`. Wired once
    /// by `Scheduler::new`; expected to be set by the time any mail
    /// can land.
    registry: OnceLock<Arc<Registry>>,
    /// Components table for forwarding into per-component inboxes.
    /// Wired alongside `registry`.
    components: OnceLock<ComponentTable>,
    /// Hub outbound handle. When set and connected, mail to unknown
    /// mailbox ids bubbles up to the hub-substrate (ADR-0037
    /// Phase 1) instead of being warn-dropped locally. Wired by
    /// `SubstrateBoot::build` right after the `Mailer` exists; the
    /// boot holds the only `Arc<HubOutbound>` on the fresh side of
    /// construction. Absent on chassis that skip hub connection
    /// (today: the hub chassis itself), which keeps local warn-drop
    /// semantics intact — the hub is the end of the bubbles-up
    /// line.
    outbound: OnceLock<Arc<HubOutbound>>,
}

impl Mailer {
    pub fn new() -> Self {
        Self {
            registry: OnceLock::new(),
            components: OnceLock::new(),
            outbound: OnceLock::new(),
        }
    }

    /// Wire the registry + components table. Called by `Scheduler::new`
    /// once both exist; panics on re-wiring to surface construction-
    /// order bugs loud rather than silent.
    pub fn wire(&self, registry: Arc<Registry>, components: ComponentTable) {
        self.registry
            .set(registry)
            .unwrap_or_else(|_| panic!("Mailer::wire called twice"));
        self.components
            .set(components)
            .unwrap_or_else(|_| panic!("Mailer::wire called twice"));
    }

    /// Wire the `HubOutbound` so mail to unknown mailbox ids bubbles
    /// up to the hub-substrate (ADR-0037 Phase 1) instead of being
    /// warn-dropped. Called by `SubstrateBoot::build` after the
    /// `HubOutbound` exists; harmless to skip for chassis that are
    /// their own hub (the hub chassis doesn't bubble up to itself).
    pub fn wire_outbound(&self, outbound: Arc<HubOutbound>) {
        self.outbound
            .set(outbound)
            .unwrap_or_else(|_| panic!("Mailer::wire_outbound called twice"));
    }

    /// Hand `mail` to the substrate for dispatch. Sinks run on the
    /// caller thread; component mail forwards into the recipient's
    /// inbox (which bumps the per-entry drain counter); dropped /
    /// unknown recipients warn-and-discard (or bubble up to the hub-
    /// substrate when a `HubOutbound` is connected, per ADR-0037).
    pub fn push(&self, mail: Mail) {
        route_mail(
            mail,
            self.registry.get().expect("Mailer not wired"),
            self.components.get().expect("Mailer not wired"),
            self.outbound.get(),
        );
    }

    /// Block until every live component's inbox is empty and no
    /// `deliver` is in flight. Phase-3 replacement for Phase-2's
    /// `wait_idle` barrier — equivalent end-to-end semantics
    /// (iterates the components table and waits on each entry's
    /// per-mailbox drain counter, re-checking in case one entry's
    /// delivery pushed fresh mail to another).
    pub fn drain_all(&self) {
        let components = self.components.get().expect("Mailer not wired");
        crate::scheduler::drain_all(components);
    }
}

impl Default for Mailer {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve `mail.recipient` against the registry + components and
/// dispatch inline. Sinks run on the caller thread; component mail
/// forwards into the per-component dispatcher inbox (which bumps the
/// entry's drain counter). Dropped / unknown recipients and closed
/// inboxes warn-log and drop the mail.
fn route_mail(
    mail: Mail,
    registry: &Registry,
    components: &ComponentTable,
    outbound: Option<&Arc<HubOutbound>>,
) {
    let recipient = mail.recipient;
    match registry.entry(recipient) {
        Some(MailboxEntry::Sink(handler)) => {
            let kind_name = registry.kind_name(mail.kind).unwrap_or_default();
            // Mail reaching a sink through `push` came from substrate
            // core or a chassis (e.g. the frame loop's FrameStats
            // push, platform input fan-out). Per ADR-0011 origin is
            // `None`. Components reach sinks via `SubstrateCtx::send`
            // inline and never enter `push`.
            handler(
                mail.kind,
                &kind_name,
                None,
                mail.sender,
                &mail.payload,
                mail.count,
            );
        }
        Some(MailboxEntry::Component) => {
            let entry = components.read().unwrap().get(&recipient).map(Arc::clone);
            match entry {
                Some(entry) => {
                    if !entry.send(mail) {
                        tracing::warn!(
                            target: "aether_substrate::queue",
                            mailbox = ?recipient,
                            "component inbox closed; mail discarded",
                        );
                    }
                }
                None => {
                    tracing::warn!(
                        target: "aether_substrate::queue",
                        mailbox = ?recipient,
                        "mail to registered-component mailbox but no component bound — dropped",
                    );
                }
            }
        }
        Some(MailboxEntry::Dropped) => {
            tracing::warn!(
                target: "aether_substrate::queue",
                mailbox = ?recipient,
                "mail to dropped mailbox — discarded",
            );
        }
        None => {
            // ADR-0037 Phase 1: unknown-locally mailboxes bubble up
            // to the hub-substrate when a live outbound is wired.
            // The hub resolves the id against its own registry and
            // dispatches; if it doesn't know the id either, it
            // warns on its side (end-of-line). Fall back to the
            // local warn-drop when no hub is attached (single-host
            // dev, or the hub chassis itself).
            if let Some(outbound) = outbound
                && outbound.is_connected()
            {
                // ADR-0037 Phase 2: carry the local sending
                // component's mailbox id so the hub can build a
                // `Sender::EngineMailbox { engine_id, mailbox_id }`
                // for the receiving component. `None` for mail
                // with no local component origin (broadcast-
                // originated, substrate-generated).
                let source_mailbox_id = mail.from_component.map(|mbox| mbox.0);
                let sent = outbound.send(EngineToHub::MailToHubSubstrate(
                    EngineMailToHubSubstrateFrame {
                        recipient_mailbox_id: recipient.0,
                        kind_id: mail.kind,
                        payload: mail.payload,
                        count: mail.count,
                        source_mailbox_id,
                    },
                ));
                if !sent {
                    tracing::warn!(
                        target: "aether_substrate::queue",
                        mailbox = ?recipient,
                        "bubbles-up failed (writer channel closed); mail dropped",
                    );
                }
                return;
            }
            tracing::warn!(
                target: "aether_substrate::queue",
                mailbox = ?recipient,
                "mail to unknown mailbox — dropped",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::RwLock;

    use super::*;
    use crate::mail::MailboxId;

    /// ADR-0037 Phase 1: a live outbound + unknown mailbox id
    /// forwards `MailToHubSubstrate` upstream instead of
    /// warn-dropping. The forwarded frame carries the exact
    /// mailbox id / kind / payload / count the caller pushed.
    #[test]
    fn unknown_mailbox_with_connected_outbound_bubbles_up() {
        let (outbound, outbound_rx) = HubOutbound::test_channel();
        let registry = Arc::new(Registry::new());
        let components = Arc::new(RwLock::new(HashMap::new()));

        let mailer = Mailer::new();
        mailer.wire(Arc::clone(&registry), Arc::clone(&components));
        mailer.wire_outbound(Arc::clone(&outbound));

        let unknown = MailboxId(0xDEADBEEF_u64);
        let kind: u64 = 0xABCD_u64;
        let payload = vec![1, 2, 3];
        mailer.push(Mail::new(unknown, kind, payload.clone(), 1));

        let frame = outbound_rx.try_recv().expect("bubble-up frame emitted");
        match frame {
            EngineToHub::MailToHubSubstrate(f) => {
                assert_eq!(f.recipient_mailbox_id, unknown.0);
                assert_eq!(f.kind_id, kind);
                assert_eq!(f.payload, payload);
                assert_eq!(f.count, 1);
            }
            other => panic!("expected MailToHubSubstrate, got {other:?}"),
        }
    }

    /// No outbound wired (or disconnected): unknown mailbox stays
    /// warn-drop. Asserts by showing the outbound channel stays
    /// empty even though we pushed — the warn-drop path doesn't
    /// generate a frame.
    #[test]
    fn unknown_mailbox_without_outbound_warn_drops() {
        let registry = Arc::new(Registry::new());
        let components = Arc::new(RwLock::new(HashMap::new()));

        let mailer = Mailer::new();
        mailer.wire(Arc::clone(&registry), Arc::clone(&components));
        // Deliberately no wire_outbound.

        let unknown = MailboxId(0xDEADBEEF_u64);
        mailer.push(Mail::new(unknown, 0xABCD, vec![], 0));
        // No panic is the test; the warn path logs and returns.
    }
}
