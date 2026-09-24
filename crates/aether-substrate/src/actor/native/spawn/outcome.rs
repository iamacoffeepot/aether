//! What a birth answers with: the local receipt a staging site gets back,
//! and the authoritative fate that follows it through the ADR-0093 task
//! completion path once the registry owner has decided.

use std::fmt;
use std::sync::Arc;

use aether_actor::{ActorRef, Addressable};

use crate::actor::native::DispatchId;
use crate::mail::MailboxId;

use super::SpawnError;

/// Deterministic result returned when a handler has locally prepared and
/// staged a child birth. It names a reservation, not proof that the child is
/// live; the authoritative result arrives as a later `TaskDone`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnReceipt {
    pub mailbox_id: MailboxId,
    pub canonical_name: Arc<str>,
    pub completion: DispatchId,
}

/// The authoritative fate of one staged child birth of an `A`, delivered
/// through the ADR-0093 task completion path once the registry owner has
/// decided it.
///
/// Self-identifying on **both** arms: `mailbox_id` and `canonical_name` name
/// the child the handler staged whether or not it reached Live, so a completion
/// handler correlates the result without a hand-rolled context struct whose
/// only job was carrying an id back. `result` is the precise [`SpawnError`] on
/// a refused birth. Its `Ok` arm is the ADR-0230 proof: the child's
/// [`ActorRef<A>`], minted once by the registry after the child is published
/// Live and catch-up is armed. A parent that keeps or mails its child holds
/// that reference and sends through `ctx.send_to(&child, ..)`; it never
/// re-derives one from `mailbox_id`.
pub struct SpawnOutcome<A> {
    pub mailbox_id: MailboxId,
    pub canonical_name: Arc<str>,
    pub result: Result<ActorRef<A>, SpawnError>,
}

// By hand, because a derive would bound `A: Debug` and actor types do not
// implement it. The child is named by its canonical path and `result` prints
// through `ActorRef<A>`'s namespace form; the position is never printed.
impl<A: Addressable> fmt::Debug for SpawnOutcome<A> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SpawnOutcome")
            .field("canonical_name", &self.canonical_name)
            .field("result", &self.result)
            .finish_non_exhaustive()
    }
}
