use std::sync::Arc;

use aether_actor::Addressable;
use aether_data::{ActorId, MailboxId};

/// The canonical runtime identity carried by every typed native actor binding
/// (ADR-0165).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ActorRuntimeIdentity {
    logical: ActorId,
    mailbox: MailboxId,
    carry: u64,
    canonical_name: Arc<str>,
}

impl ActorRuntimeIdentity {
    pub(super) fn new<A: Addressable>(mailbox: MailboxId, carry: u64, canonical_name: Arc<str>) -> Self {
        Self { logical: ActorId::singleton(A::NAMESPACE), mailbox, carry, canonical_name }
    }

    pub(super) fn logical(&self) -> ActorId {
        self.logical
    }

    pub(super) fn mailbox(&self) -> MailboxId {
        self.mailbox
    }

    pub(super) fn carry(&self) -> u64 {
        self.carry
    }

    pub(super) fn canonical_name(&self) -> &Arc<str> {
        &self.canonical_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe;

    impl Addressable for Probe {
        const NAMESPACE: &'static str = "test.native.identity";
        type Resolver = aether_actor::One;
    }

    #[test]
    fn typed_identity_keeps_logical_and_concrete_facts_distinct() {
        let mailbox = MailboxId(0x4058);
        let identity =
            ActorRuntimeIdentity::new::<Probe>(mailbox, 0x165, Arc::from("test.parent/test.native.identity:child"));

        assert_eq!(identity.logical(), ActorId::singleton(Probe::NAMESPACE));
        assert_eq!(identity.mailbox(), mailbox);
        assert_eq!(identity.carry(), 0x165);
        assert_eq!(&**identity.canonical_name(), "test.parent/test.native.identity:child");
    }
}
