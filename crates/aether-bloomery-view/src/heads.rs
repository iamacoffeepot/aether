//! Cursor-bearing fold: last move per `(target KindId, name)`.

use alloc::collections::BTreeMap;
use core::error::Error;
use core::fmt;

use crate::sequence::{SequenceError, check_next};
use crate::view::View;
use aether_bloomery_kinds::{
    DecodeError, Digest, Entry, Head, HeadNameError, Program, ProgramHeadMoved, RecordedHead, RecordedHeadMove, Ref,
    Seq,
};
use aether_data::Kind;

/// Last move per recorded `(target KindId, name)` over a contiguous log prefix.
///
/// The cursor is the last applied [`Seq`]. It starts at `Seq(0)`, the empty
/// prefix. [`Self::apply`] requires the next contiguous sequence, including
/// unrelated entries, so the cursor is an exact statement about the observed
/// prefix rather than a best-effort watermark.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Kind)]
#[kind(name = "bloomery.view.heads")]
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
    /// [`HeadFoldError::Sequence`] when `entry.seq` is not the next contiguous
    /// sequence. [`HeadFoldError::Decode`] when a recognized move does not
    /// decode. [`HeadFoldError::Name`] when a historical program name is not
    /// a valid [`Head`] name. On error, cursor and bindings are unchanged.
    pub fn apply(&mut self, entry: &Entry) -> Result<(), HeadFoldError> {
        check_next(self.cursor, entry.seq)?;

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

    /// Current binding of an untyped recorded head, if this prefix has seen a move for it.
    #[must_use]
    pub fn binding(&self, head: &RecordedHead) -> Option<Digest> {
        self.bindings.get(head).copied()
    }

    pub(crate) fn bindings(&self) -> &BTreeMap<RecordedHead, Digest> {
        &self.bindings
    }

    pub(crate) fn reconstruct(cursor: Seq, bindings: BTreeMap<RecordedHead, Digest>) -> Self {
        Self { cursor, bindings }
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
    /// `entry.seq` was not the next contiguous sequence.
    Sequence(SequenceError),
    /// A recognized move's payload did not decode.
    Decode(DecodeError),
    /// A historical [`ProgramHeadMoved`] name is not a valid [`Head`] name.
    Name(HeadNameError),
}

impl fmt::Display for HeadFoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sequence(error) => write!(f, "{error}"),
            Self::Decode(error) => write!(f, "{error}"),
            Self::Name(error) => write!(f, "historical program head name is not a valid head name: {error}"),
        }
    }
}

impl Error for HeadFoldError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sequence(error) => Some(error),
            Self::Decode(error) => Some(error),
            Self::Name(error) => Some(error),
        }
    }
}

impl From<SequenceError> for HeadFoldError {
    fn from(error: SequenceError) -> Self {
        Self::Sequence(error)
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

/// The binding a recognized move sets, shared by [`Heads`] and [`crate::HeadHistory`].
pub fn binding_from(entry: &Entry) -> Result<Option<(RecordedHead, Digest)>, HeadFoldError> {
    if entry.kind == RecordedHeadMove::ID {
        let event = entry.decode::<RecordedHeadMove>()?;
        Ok(Some((event.head().clone(), event.to())))
    } else if entry.kind == ProgramHeadMoved::ID {
        let event = entry.decode::<ProgramHeadMoved>()?;
        Ok(Some((RecordedHead::new(Program::ID, event.name.as_str())?, event.program.digest())))
    } else {
        Ok(None)
    }
}
