//! Shared fixed clock and test event kinds.

use aether_bloomery_journal::{Clock, Digest, Draft, Journal, JournalError};

/// Wall-clock stamp every in-memory test uses.
pub const FIXED_MILLIS: u64 = 1_700_000_000_000;

/// Clock that always returns [`FIXED_MILLIS`].
pub struct FixedClock;

impl Clock for FixedClock {
    fn now_millis(&self) -> u64 {
        FIXED_MILLIS
    }
}

/// In-memory journal on the fixed clock.
pub fn journal() -> Result<Journal, JournalError> {
    Journal::open_in_memory_with_clock(Box::new(FixedClock))
}

/// Root note event.
#[derive(Debug, Clone, PartialEq, aether_data::Storage)]
#[kind(name = "test.journal.note")]
pub struct Note {
    pub text: String,
}

impl Note {
    pub fn new(text: &str) -> Self {
        Self { text: text.to_owned() }
    }

    pub fn draft(text: &str) -> Draft {
        Draft::of(&Self::new(text), None).expect("encode note")
    }
}

/// A second kind, used to prove decode refuses a name mismatch.
#[derive(Debug, Clone, PartialEq, aether_data::Storage)]
#[kind(name = "test.journal.other")]
pub struct Other {
    pub n: u64,
}

/// Event that references an artifact by digest.
#[derive(Debug, Clone, PartialEq, aether_data::Storage)]
#[kind(name = "test.journal.referenced")]
pub struct Referenced {
    pub digest: Digest,
}

/// Kind name of 257 bytes: the `entries.kind` CHECK is `length(kind) <= 256`.
#[derive(Debug, Clone, PartialEq, aether_data::Storage)]
#[kind(
    name = "test.journal.xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
)]
pub struct TooLong {
    pub n: u64,
}
