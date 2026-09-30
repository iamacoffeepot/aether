//! One entry's own journal position, handed to a fold, guard, or rule beside
//! the entry.

use aether_bloomery_kinds::{Entry, Seq};

/// The journal position of the entry being folded or reacted to: its own
/// `seq`, the `seq` it reacts to, and the journal time it was recorded at,
/// copied from the entry.
///
/// It fetches nothing and cannot fail. Following `cause` back to the entry
/// it names is the reader's work, through a view that folded that entry.
/// `recorded_at_millis` is the only time a fold, guard, or rule reads: a due
/// time is computed from it (ADR-0245), never from a live clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct At {
    /// The entry's own `seq`.
    pub seq: Seq,
    /// The `seq` the entry reacts to, if any.
    pub cause: Option<Seq>,
    /// The journal time the entry was recorded at, in unix milliseconds.
    pub recorded_at_millis: u64,
}

impl From<&Entry> for At {
    fn from(entry: &Entry) -> Self {
        Self { seq: entry.seq, cause: entry.cause, recorded_at_millis: entry.recorded_at_millis }
    }
}
