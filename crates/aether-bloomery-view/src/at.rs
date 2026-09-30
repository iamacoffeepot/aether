//! One entry's own journal position, handed to a fold, guard, or rule beside
//! the entry.

use aether_bloomery_kinds::{Entry, Seq};

/// The journal position of the entry being folded or reacted to: its own
/// `seq` and the `seq` it reacts to, copied from the entry.
///
/// It fetches nothing and cannot fail. Following `cause` back to the entry
/// it names is the reader's work, through a view that folded that entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct At {
    /// The entry's own `seq`.
    pub seq: Seq,
    /// The `seq` the entry reacts to, if any.
    pub cause: Option<Seq>,
}

impl From<&Entry> for At {
    fn from(entry: &Entry) -> Self {
        Self { seq: entry.seq, cause: entry.cause }
    }
}
