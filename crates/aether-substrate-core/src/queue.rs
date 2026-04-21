// Dispatch barrier + inline router (ADR-0038 Phase 2).
//
// With per-component dispatcher threads from Phase 1, the shared mail
// queue no longer needs a VecDeque or a router thread. `push`
// resolves the recipient against the registry on the caller's thread
// and either runs the sink handler inline, forwards into the
// recipient's dispatcher inbox, or warn-drops (dropped / unknown /
// closed-inbox).
//
// The `outstanding` counter + `done_cv` barrier survives unchanged:
// `push` increments before routing; the per-component dispatcher
// decrements after each `deliver`, and the inline sink / warn-drop
// arms decrement directly. `wait_idle` callers see end-to-end
// completion semantics identical to Phase 1.
//
// Invariants:
//   - `push` increments `outstanding` BEFORE any routing work. A
//     producer cannot hand the mail off to a consumer and race its
//     decrement past a `wait_idle` observing zero.
//   - Consumers decrement after processing via `mark_completed`.
//   - `wait_idle` blocks until the counter reaches zero. Safe to call
//     multiple times in sequence (the next frame's pushes re-raise
//     the counter).
//
// Phase 3 will retire the global barrier altogether: `wait_idle`
// callers (desktop frame barrier, capture_frame pre-bundle, headless
// tick cadence) migrate to per-mailbox drain primitives.

use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::mail::Mail;
use crate::registry::{MailboxEntry, Registry};
use crate::scheduler::ComponentTable;

pub struct MailQueue {
    /// Registry handle for resolving recipients on `push`. Wired once
    /// by `Scheduler::new`; expected to be set by the time any mail
    /// can land.
    registry: OnceLock<Arc<Registry>>,
    /// Components table for forwarding into per-component inboxes.
    /// Wired alongside `registry`.
    components: OnceLock<ComponentTable>,
    outstanding: Mutex<usize>,
    done_cv: Condvar,
}

impl MailQueue {
    pub fn new() -> Self {
        Self {
            registry: OnceLock::new(),
            components: OnceLock::new(),
            outstanding: Mutex::new(0),
            done_cv: Condvar::new(),
        }
    }

    /// Wire the registry + components table. Called by `Scheduler::new`
    /// once both exist; panics on re-wiring to surface construction-
    /// order bugs loud rather than silent.
    pub fn wire(&self, registry: Arc<Registry>, components: ComponentTable) {
        self.registry
            .set(registry)
            .unwrap_or_else(|_| panic!("MailQueue::wire called twice"));
        self.components
            .set(components)
            .unwrap_or_else(|_| panic!("MailQueue::wire called twice"));
    }

    /// Hand `mail` to the substrate for dispatch. Increments
    /// `outstanding`, then routes inline: sinks run on the caller
    /// thread, component mail forwards into the recipient's inbox,
    /// dropped / unknown recipients warn-and-discard.
    pub fn push(&self, mail: Mail) {
        {
            let mut n = self.outstanding.lock().unwrap();
            *n += 1;
        }
        route_mail(
            mail,
            self.registry.get().expect("MailQueue not wired"),
            self.components.get().expect("MailQueue not wired"),
            self,
        );
    }

    /// Block until every mail enqueued so far has been processed.
    /// Used by the frame loop to wait for a frame to drain.
    pub fn wait_idle(&self) {
        let mut n = self.outstanding.lock().unwrap();
        while *n > 0 {
            n = self.done_cv.wait(n).unwrap();
        }
    }

    pub(crate) fn mark_completed(&self) {
        let mut n = self.outstanding.lock().unwrap();
        *n -= 1;
        if *n == 0 {
            self.done_cv.notify_all();
        }
    }
}

impl Default for MailQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve `mail.recipient` against the registry + components and
/// dispatch inline. Sinks run on the caller thread; component mail
/// forwards into the per-component dispatcher inbox (which decrements
/// `outstanding` after `deliver` returns). Dropped / unknown
/// recipients and closed inboxes warn-log and decrement immediately.
fn route_mail(mail: Mail, registry: &Registry, components: &ComponentTable, queue: &MailQueue) {
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
            queue.mark_completed();
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
                        queue.mark_completed();
                    }
                    // Happy path: dispatcher owns mark_completed.
                }
                None => {
                    tracing::warn!(
                        target: "aether_substrate::queue",
                        mailbox = ?recipient,
                        "mail to registered-component mailbox but no component bound — dropped",
                    );
                    queue.mark_completed();
                }
            }
        }
        Some(MailboxEntry::Dropped) => {
            tracing::warn!(
                target: "aether_substrate::queue",
                mailbox = ?recipient,
                "mail to dropped mailbox — discarded",
            );
            queue.mark_completed();
        }
        None => {
            tracing::warn!(
                target: "aether_substrate::queue",
                mailbox = ?recipient,
                "mail to unknown mailbox — dropped",
            );
            queue.mark_completed();
        }
    }
}
