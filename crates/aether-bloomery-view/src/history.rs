//! Cursor-bearing fold: every head move, so [`Heads`] can be answered at any folded seq.

use alloc::collections::BTreeMap;

use crate::heads::{HeadFoldError, Heads, binding_from};
use crate::sequence::check_next;
use aether_bloomery_kinds::{Digest, Entry, RecordedHead, Seq};

/// Every move per recorded `(target KindId, name)` over a contiguous log prefix.
///
/// It folds the same entries as [`Heads`] and recognizes the same moves, but
/// keeps each head's moves by seq instead of only the last one, so
/// [`Self::heads_at`] can rebuild the [`Heads`] of any prefix up to the
/// cursor without reading the journal again. The cursor follows the same
/// contiguity rule as [`Heads::apply`].
#[derive(Debug)]
pub struct HeadHistory {
    cursor: Seq,
    moves: BTreeMap<RecordedHead, BTreeMap<Seq, Digest>>,
}

impl HeadHistory {
    /// Empty fold: cursor `Seq(0)`, no moves.
    #[must_use]
    pub const fn new() -> Self {
        Self { cursor: Seq(0), moves: BTreeMap::new() }
    }

    /// Last applied sequence, or `Seq(0)` when nothing has been applied.
    #[must_use]
    pub const fn cursor(&self) -> Seq {
        self.cursor
    }

    /// Apply `entry` as the next contiguous sequence.
    ///
    /// Unrelated kinds advance the cursor. A move [`Heads::apply`] would bind
    /// is recorded under its head at `entry.seq`.
    ///
    /// # Errors
    ///
    /// The same as [`Heads::apply`]. On error, cursor and moves are unchanged.
    pub fn apply(&mut self, entry: &Entry) -> Result<(), HeadFoldError> {
        check_next(self.cursor, entry.seq)?;

        if let Some((head, digest)) = binding_from(entry)? {
            self.moves.entry(head).or_default().insert(entry.seq, digest);
        }
        self.cursor = entry.seq;
        Ok(())
    }

    /// The [`Heads`] of the prefix `1..=seq`, or `None` when `seq` is past the cursor.
    ///
    /// Each head is bound to its last move at or before `seq`; a head with no
    /// move by then is absent.
    #[must_use]
    pub fn heads_at(&self, seq: Seq) -> Option<Heads> {
        if seq > self.cursor {
            return None;
        }
        let bindings = self
            .moves
            .iter()
            .filter_map(|(head, moves)| moves.range(..=seq).next_back().map(|(_, digest)| (head.clone(), *digest)))
            .collect();
        Some(Heads::reconstruct(seq, bindings))
    }
}

impl Default for HeadHistory {
    fn default() -> Self {
        Self::new()
    }
}
