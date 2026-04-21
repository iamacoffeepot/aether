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
}

impl Mailer {
    pub fn new() -> Self {
        Self {
            registry: OnceLock::new(),
            components: OnceLock::new(),
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

    /// Hand `mail` to the substrate for dispatch. Sinks run on the
    /// caller thread; component mail forwards into the recipient's
    /// inbox (which bumps the per-entry drain counter); dropped /
    /// unknown recipients warn-and-discard.
    pub fn push(&self, mail: Mail) {
        route_mail(
            mail,
            self.registry.get().expect("Mailer not wired"),
            self.components.get().expect("Mailer not wired"),
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
fn route_mail(mail: Mail, registry: &Registry, components: &ComponentTable) {
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
            tracing::warn!(
                target: "aether_substrate::queue",
                mailbox = ?recipient,
                "mail to unknown mailbox — dropped",
            );
        }
    }
}
