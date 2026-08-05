//! Where a binding's routing facts come from — one typed production identity
//! or one explicitly untyped test identity (ADR-0165).

use crate::actor::native::identity::ActorRuntimeIdentity;
use crate::mail::MailboxId;

/// Exactly one identity source for a native binding. Production construction
/// is typed; test-only construction retains only the concrete routing facts
/// needed by existing mailbox helpers and cannot spawn.
pub(super) enum BindingIdentity {
    Typed(ActorRuntimeIdentity),
    Untyped { mailbox: MailboxId, parent: MailboxId, carry: u64 },
}

impl BindingIdentity {
    pub(super) fn mailbox(&self) -> MailboxId {
        match self {
            Self::Typed(identity) => identity.mailbox(),
            Self::Untyped { mailbox, .. } => *mailbox,
        }
    }

    pub(super) fn carry(&self) -> u64 {
        match self {
            Self::Typed(identity) => identity.carry(),
            Self::Untyped { carry, .. } => *carry,
        }
    }

    pub(super) fn parent(&self) -> MailboxId {
        match self {
            Self::Typed(identity) => identity.parent(),
            Self::Untyped { parent, .. } => *parent,
        }
    }

    pub(super) fn runtime_identity(&self) -> Option<&ActorRuntimeIdentity> {
        match self {
            Self::Typed(identity) => Some(identity),
            Self::Untyped { .. } => None,
        }
    }
}
