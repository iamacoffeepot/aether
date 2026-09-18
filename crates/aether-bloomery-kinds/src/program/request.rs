//! A program request, recorded before the attempt.

use crate::Digest;
use crate::program::name::{NativeOrigin, ReactorName, RuleName};
use crate::program::reference::ProgramRef;

/// Where a program request came from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, aether_data::Storage)]
pub enum RequestSource {
    /// A reactor rule's `CallProgram` intent, caused by the trigger seq.
    Reaction {
        bundle: Digest,
        reactor: ReactorName,
        rule: RuleName,
        /// The intent's position within that rule's output for the trigger seq.
        ordinal: u32,
    },
    /// A driver `Call` mail from outside the journal. Uncaused.
    Native {
        origin: NativeOrigin,
        /// The caller-chosen idempotency key within `origin`.
        key: u64,
    },
}

/// A program request, recorded before the attempt. Written only by the
/// driver. Caused by the trigger seq for a [`RequestSource::Reaction`]
/// source; uncaused for a [`RequestSource::Native`] source.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.requested")]
pub struct Requested {
    pub program: ProgramRef,
    pub input: Digest,
    pub source: RequestSource,
}
