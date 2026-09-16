//! Cursor-bearing fold: last move per `(target KindId, name)`.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use aether_bloomery_journal::{DecodeError, Entry, Journal, Seq};

use crate::view::View;
use aether_bloomery_kinds::{
    Digest, Head, HeadNameError, Program, ProgramHeadMoved, RecordedHead, RecordedHeadMove, Ref,
};
use aether_data::Kind;

/// Last move per recorded `(target KindId, name)` over a contiguous log prefix.
///
/// The cursor is the last applied [`Seq`]. It starts at `Seq(0)`, the empty
/// prefix. [`Self::apply`] requires the next contiguous sequence, including
/// unrelated entries, so the cursor is an exact statement about the observed
/// prefix rather than a best-effort watermark.
#[derive(Debug, Clone)]
pub struct Heads {
    cursor: Seq,
    bindings: BTreeMap<RecordedHead, Digest>,
}

impl Heads {
    /// Empty fold: cursor `Seq(0)`, no bindings.
    #[must_use]
    pub const fn new() -> Self {
        Self { cursor: Seq(0), bindings: BTreeMap::new() }
    }

    /// Last applied sequence, or `Seq(0)` when nothing has been applied.
    #[must_use]
    pub const fn cursor(&self) -> Seq {
        self.cursor
    }

    /// Apply `entry` as the next contiguous sequence.
    ///
    /// Unrelated kinds advance the cursor. A recorded [`RecordedHeadMove`] or
    /// historical [`ProgramHeadMoved`] updates that head's binding and the
    /// cursor together.
    ///
    /// # Errors
    ///
    /// [`HeadFoldError::Gap`], [`HeadFoldError::Duplicate`], or
    /// [`HeadFoldError::Backwards`] when `entry.seq` is not the next sequence.
    /// [`HeadFoldError::Decode`] when a recognized move does not decode.
    /// [`HeadFoldError::Name`] when a historical program name is not a valid
    /// [`Head`] name. [`HeadFoldError::Overflow`] when the next sequence does not
    /// fit in [`Seq`]. On error, cursor and bindings are unchanged.
    pub fn apply(&mut self, entry: &Entry) -> Result<(), HeadFoldError> {
        let expected = next_seq(self.cursor)?;
        if entry.seq != expected {
            return Err(seq_error(self.cursor, expected, entry.seq));
        }

        let binding = binding_from(entry)?;
        if let Some((head, digest)) = binding {
            self.bindings.insert(head, digest);
        }
        self.cursor = entry.seq;
        Ok(())
    }

    /// Current binding of `head`, if this prefix has seen a move for it.
    #[must_use]
    pub fn get<K: Kind>(&self, head: &Head<K>) -> Option<Ref<K>> {
        self.bindings.get(&RecordedHead::from(head)).copied().map(Ref::from_digest)
    }
}

impl Default for Heads {
    fn default() -> Self {
        Self::new()
    }
}

impl View for Heads {
    type Error = HeadFoldError;

    fn empty() -> Self {
        Self::new()
    }

    fn cursor(&self) -> Seq {
        self.cursor
    }

    fn advance(&mut self, entries: &[Entry]) -> Result<(), Self::Error> {
        for entry in entries {
            self.apply(entry)?;
        }
        Ok(())
    }
}

/// Failure to fold one journal entry into [`Heads`].
#[derive(Debug)]
pub enum HeadFoldError {
    /// `entry.seq` is past the next contiguous sequence.
    Gap {
        /// Sequence the fold required.
        expected: Seq,
        /// Sequence on the refused entry.
        actual: Seq,
    },
    /// `entry.seq` repeats the last applied sequence.
    Duplicate {
        /// Sequence the fold required.
        expected: Seq,
        /// Sequence on the refused entry.
        actual: Seq,
    },
    /// `entry.seq` is before the next contiguous sequence and is not the last applied sequence.
    Backwards {
        /// Sequence the fold required.
        expected: Seq,
        /// Sequence on the refused entry.
        actual: Seq,
    },
    /// The next sequence does not fit in [`Seq`].
    Overflow,
    /// A recognized move's payload did not decode.
    Decode(DecodeError),
    /// A historical [`ProgramHeadMoved`] name is not a valid [`Head`] name.
    Name(HeadNameError),
}

impl fmt::Display for HeadFoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gap { expected, actual } => {
                write!(f, "journal fold gap: expected seq {expected}, got {actual}")
            }
            Self::Duplicate { expected, actual } => {
                write!(f, "journal fold duplicate: expected seq {expected}, got {actual}")
            }
            Self::Backwards { expected, actual } => {
                write!(f, "journal fold backwards: expected seq {expected}, got {actual}")
            }
            Self::Overflow => write!(f, "journal fold sequence overflow"),
            Self::Decode(error) => write!(f, "{error}"),
            Self::Name(error) => write!(f, "historical program head name is not a valid head name: {error}"),
        }
    }
}

impl Error for HeadFoldError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Decode(error) => Some(error),
            Self::Name(error) => Some(error),
            Self::Gap { .. } | Self::Duplicate { .. } | Self::Backwards { .. } | Self::Overflow => None,
        }
    }
}

impl From<DecodeError> for HeadFoldError {
    fn from(error: DecodeError) -> Self {
        Self::Decode(error)
    }
}

impl From<HeadNameError> for HeadFoldError {
    fn from(error: HeadNameError) -> Self {
        Self::Name(error)
    }
}

fn next_seq(cursor: Seq) -> Result<Seq, HeadFoldError> {
    cursor.0.checked_add(1).map(Seq).ok_or(HeadFoldError::Overflow)
}

fn seq_error(cursor: Seq, expected: Seq, actual: Seq) -> HeadFoldError {
    if actual.0 > expected.0 {
        HeadFoldError::Gap { expected, actual }
    } else if actual == cursor && cursor.0 != 0 {
        HeadFoldError::Duplicate { expected, actual }
    } else {
        HeadFoldError::Backwards { expected, actual }
    }
}

fn binding_from(entry: &Entry) -> Result<Option<(RecordedHead, Digest)>, HeadFoldError> {
    if entry.kind == RecordedHeadMove::NAME {
        let event = Journal::decode::<RecordedHeadMove>(entry)?;
        Ok(Some((event.head().clone(), event.to())))
    } else if entry.kind == ProgramHeadMoved::NAME {
        let event = Journal::decode::<ProgramHeadMoved>(entry)?;
        Ok(Some((RecordedHead::new(Program::ID, event.name.as_str())?, event.program.digest())))
    } else {
        Ok(None)
    }
}
