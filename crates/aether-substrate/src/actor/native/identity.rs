use std::sync::Arc;

use aether_data::MailboxId;

/// The canonical runtime identity carried by every typed native actor binding
/// (ADR-0165).
///
/// It carries no logical actor id: the actor's *type* is a compile-time fact
/// of the ctx a spawn is staged from (issue 4158), so the only consumer of a
/// runtime type tag — the parent-declaration check — is gone, and what remains
/// is the concrete instance the lineage fold and canonical name are built from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ActorRuntimeIdentity {
    mailbox: MailboxId,
    carry: u64,
    canonical_name: Arc<str>,
}

impl ActorRuntimeIdentity {
    pub(super) fn new(mailbox: MailboxId, carry: u64, canonical_name: Arc<str>) -> Self {
        Self { mailbox, carry, canonical_name }
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

    #[test]
    fn typed_identity_keeps_the_concrete_instance_facts_distinct() {
        let mailbox = MailboxId(0x4058);
        let identity = ActorRuntimeIdentity::new(mailbox, 0x165, Arc::from("test.parent/test.native.identity:child"));

        assert_eq!(identity.mailbox(), mailbox);
        assert_eq!(identity.carry(), 0x165);
        assert_eq!(&**identity.canonical_name(), "test.parent/test.native.identity:child");
    }
}
