//! Where a binding's routing facts come from — one typed production identity
//! or one explicitly untyped test identity (ADR-0165).

use crate::actor::native::identity::ActorRuntimeIdentity;
use crate::mail::MailboxId;

/// Exactly one identity source for a native binding. Production construction
/// is typed; test-only construction retains only the concrete routing facts
/// needed by existing mailbox helpers and cannot spawn.
pub(super) enum BindingIdentity {
    Typed(ActorRuntimeIdentity),
    #[cfg(any(test, feature = "test-support"))]
    Untyped {
        mailbox: MailboxId,
        parent: Option<MailboxId>,
        carry: u64,
    },
}

impl BindingIdentity {
    pub(super) fn mailbox(&self) -> MailboxId {
        match self {
            Self::Typed(identity) => identity.mailbox(),
            #[cfg(any(test, feature = "test-support"))]
            Self::Untyped { mailbox, .. } => *mailbox,
        }
    }

    pub(super) fn carry(&self) -> u64 {
        match self {
            Self::Typed(identity) => identity.carry(),
            #[cfg(any(test, feature = "test-support"))]
            Self::Untyped { carry, .. } => *carry,
        }
    }

    pub(super) fn parent(&self) -> Option<MailboxId> {
        match self {
            Self::Typed(identity) => identity.parent(),
            #[cfg(any(test, feature = "test-support"))]
            Self::Untyped { parent, .. } => *parent,
        }
    }

    #[cfg_attr(not(any(test, feature = "test-support")), expect(clippy::unnecessary_wraps))] // aether-suppression-request: without test-support the untyped test identity is compiled out, so every arm answers `Some`
    pub(super) fn runtime_identity(&self) -> Option<&ActorRuntimeIdentity> {
        match self {
            Self::Typed(identity) => Some(identity),
            #[cfg(any(test, feature = "test-support"))]
            Self::Untyped { .. } => None,
        }
    }
}
