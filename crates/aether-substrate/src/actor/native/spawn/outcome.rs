//! What a birth answers with: the local receipt a staging site gets back,
//! and the authoritative fate that follows it through the ADR-0093 task
//! completion path once the registry owner has decided.

use std::sync::Arc;

use crate::actor::native::DispatchId;
use crate::mail::MailboxId;

/// Deterministic result returned when a handler has locally prepared and
/// staged a child birth. It names a reservation, not proof that the child is
/// live; the authoritative result arrives as a later `TaskDone`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnReceipt {
    pub mailbox_id: MailboxId,
    pub canonical_name: Arc<str>,
    pub completion: DispatchId,
}

/// The authoritative fate of one staged child birth, delivered through the
/// ADR-0093 task completion path once the registry owner has decided it.
///
/// Self-identifying on **both** arms: `mailbox_id` and `canonical_name` name
/// the child the handler staged whether or not it reached Live, so a completion
/// handler correlates the result without a hand-rolled context struct whose
/// only job was carrying an id back. `result` is `Ok(())` after the child is
/// published Live and catch-up is armed, and the precise [`SpawnError`](super::SpawnError)
/// otherwise.
#[derive(Debug)]
pub struct SpawnOutcome {
    pub mailbox_id: MailboxId,
    pub canonical_name: Arc<str>,
    pub result: Result<(), SpawnError>,
}
