use std::{error, fmt};

use aether_data::SchemaType;

use crate::mail::MailboxId;

/// Rejected-load error returned when a runtime kind registration
/// names an existing kind but supplies a different descriptor than the
/// one first seen. Per ADR-0010, the load fails rather than silently
/// reinterpreting; agents rename, evolve the existing descriptor, or
/// restart the substrate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindConflict {
    pub name: String,
    pub existing: SchemaType,
    pub requested: SchemaType,
}

impl fmt::Display for KindConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "kind {:?} already registered with a different encoding (existing={:?}, requested={:?})",
            self.name, self.existing, self.requested
        )
    }
}

impl error::Error for KindConflict {}

/// A runtime mailbox registration lost to name collision. Returned
/// from `try_register_inbox` (ADR-0010) so a runtime caller can
/// reply with an error instead of panicking. The boot path that
/// registers hard-coded mailbox names still uses `register_inbox` /
/// `register_inline` and panics — collisions there are bugs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameConflict {
    pub name: String,
}

impl fmt::Display for NameConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mailbox name {:?} already registered", self.name)
    }
}

impl error::Error for NameConflict {}

/// Why a direct mailbox registration (`try_register_inbox`,
/// `try_register_inbox_with_id`) was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    /// The name is already registered, or is one no route may take.
    NameConflict(NameConflict),
    /// The name is nested beneath a parent the registry holds no record for
    /// (ADR-0248 §5). `name` is the refused registration's own.
    ParentUnknown { name: String },
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NameConflict(conflict) => conflict.fmt(f),
            Self::ParentUnknown { name } => {
                write!(f, "mailbox name {name:?} is nested beneath a parent the registry holds no record for")
            }
        }
    }
}

impl error::Error for RegisterError {}

/// Reasons `Registry::drop_mailbox` or `Registry::withdraw_claim` can
/// refuse. Distinct from the post-drop dispatch log, which the scheduler
/// handles independently. `AlreadyDropped` is also a withdrawal's refusal
/// of a retired route, whose name stays spent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropError {
    UnknownId(MailboxId),
    AlreadyDropped(MailboxId),
}

impl fmt::Display for DropError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownId(id) => write!(f, "unknown mailbox id {id:?}"),
            Self::AlreadyDropped(id) => write!(f, "mailbox {id:?} already dropped"),
        }
    }
}

impl error::Error for DropError {}
