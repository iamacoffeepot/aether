use aether_data::{ActorPath, MailboxId};

/// The canonical runtime identity carried by every typed native actor binding
/// (ADR-0165).
///
/// It carries no logical actor id: the actor's *type* is a compile-time fact
/// of the ctx a spawn is staged from (issue 4158), so the only consumer of a
/// runtime type tag — the parent-declaration check — is gone, and what remains
/// is the concrete instance the lineage fold and canonical name are built from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActorRuntimeIdentity {
    mailbox: MailboxId,
    parent: Option<MailboxId>,
    carry: u64,
    canonical_name: ActorPath,
}

impl ActorRuntimeIdentity {
    pub fn new(mailbox: MailboxId, parent: Option<MailboxId>, carry: u64, canonical_name: ActorPath) -> Self {
        Self { mailbox, parent, carry, canonical_name }
    }

    pub fn mailbox(&self) -> MailboxId {
        self.mailbox
    }

    pub fn parent(&self) -> Option<MailboxId> {
        self.parent
    }

    pub fn carry(&self) -> u64 {
        self.carry
    }

    pub fn canonical_name(&self) -> &ActorPath {
        &self.canonical_name
    }
}
