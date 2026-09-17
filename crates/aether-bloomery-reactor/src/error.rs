//! Failures while pushing a prefix or folding retained views.

use alloc::boxed::Box;
use alloc::string::String;
use core::error::Error;
use core::fmt;

use aether_bloomery_kinds::{DecodeError, Seq};

/// Failure to push entries or prepare a trigger against a retained prefix.
#[derive(Debug)]
pub enum PrepareError {
    /// No entries were retained, so there is no trigger.
    Empty,
    /// The last retained entry did not decode as the requested trigger.
    Trigger(DecodeError),
    /// An entry is past the next contiguous sequence.
    Gap {
        /// Sequence the prefix required.
        expected: Seq,
        /// Sequence on the refused entry.
        actual: Seq,
    },
    /// An entry repeats the previous sequence.
    Duplicate {
        /// Sequence the prefix required.
        expected: Seq,
        /// Sequence on the refused entry.
        actual: Seq,
    },
    /// An entry is before the next contiguous sequence and is not a duplicate.
    Backwards {
        /// Sequence the prefix required.
        expected: Seq,
        /// Sequence on the refused entry.
        actual: Seq,
    },
    /// The next sequence does not fit in [`Seq`].
    Overflow,
    /// [`aether_bloomery_view::View::empty`] did not start at [`Seq`] `(0)`.
    NonzeroEmpty {
        /// View type that failed construction.
        view: &'static str,
        /// Cursor returned by `empty`.
        cursor: Seq,
    },
    /// A successful fold did not land on the last supplied sequence.
    CursorContract {
        /// View type that broke the contract.
        view: &'static str,
        /// Cursor trusted before this batch.
        last_trusted_cursor: Seq,
        /// Sequence the fold was required to reach.
        expected: Seq,
        /// Cursor after [`aether_bloomery_view::View::advance`].
        actual: Seq,
    },
    /// The view refused the batch. The instance is unusable.
    Advance {
        /// View type that failed.
        view: &'static str,
        /// Cursor trusted before this batch.
        last_trusted_cursor: Seq,
        /// Fold error.
        source: Box<dyn Error + 'static>,
    },
    /// A required view was missing or left poisoned after a failed fold.
    Poisoned {
        /// View type that could not be injected.
        view: &'static str,
        /// Last cursor trusted for that view, if any.
        last_trusted_cursor: Seq,
    },
    /// A required published snapshot was absent from the prepared prefix.
    MissingSnapshot {
        /// Stable published name of the view.
        view: &'static str,
    },
    /// The prepared prefix listed the same published view twice.
    DuplicateSnapshot {
        /// Published name that appeared more than once.
        view: String,
    },
    /// Publish-codec encode or decode failed for a snapshot.
    Snapshot {
        /// Stable published name of the view.
        view: &'static str,
        /// Codec error.
        source: Box<dyn Error + 'static>,
    },
}

impl fmt::Display for PrepareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "reactor prepare needs a trigger entry"),
            Self::Trigger(error) => write!(f, "{error}"),
            Self::Gap { expected, actual } => {
                write!(f, "reactor prepare gap: expected seq {expected}, got {actual}")
            }
            Self::Duplicate { expected, actual } => {
                write!(f, "reactor prepare duplicate: expected seq {expected}, got {actual}")
            }
            Self::Backwards { expected, actual } => {
                write!(f, "reactor prepare backwards: expected seq {expected}, got {actual}")
            }
            Self::Overflow => write!(f, "reactor prepare sequence overflow"),
            Self::NonzeroEmpty { view, cursor } => {
                write!(f, "view {view} empty() started at seq {cursor}")
            }
            Self::CursorContract { view, last_trusted_cursor, expected, actual } => {
                write!(f, "view {view} cursor {actual} after seq {last_trusted_cursor} did not reach {expected}")
            }
            Self::Advance { view, last_trusted_cursor, source } => {
                write!(f, "view {view} failed to fold after seq {last_trusted_cursor}: {source}")
            }
            Self::Poisoned { view, last_trusted_cursor } => {
                write!(f, "view {view} is unusable after seq {last_trusted_cursor}")
            }
            Self::MissingSnapshot { view } => write!(f, "prepared prefix is missing snapshot {view}"),
            Self::DuplicateSnapshot { view } => write!(f, "prepared prefix repeats snapshot {view}"),
            Self::Snapshot { view, source } => write!(f, "prepared snapshot {view} failed: {source}"),
        }
    }
}

impl Error for PrepareError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Trigger(error) => Some(error),
            Self::Advance { source, .. } | Self::Snapshot { source, .. } => Some(source.as_ref()),
            Self::Empty
            | Self::Gap { .. }
            | Self::Duplicate { .. }
            | Self::Backwards { .. }
            | Self::Overflow
            | Self::NonzeroEmpty { .. }
            | Self::CursorContract { .. }
            | Self::Poisoned { .. }
            | Self::MissingSnapshot { .. }
            | Self::DuplicateSnapshot { .. } => None,
        }
    }
}

impl PrepareError {
    /// The last retained entry is a different stored kind than this arm's trigger.
    ///
    /// Generated evaluation treats this as a decline of that arm, not a
    /// poisoned owner. A storage-decode failure stays an error.
    #[must_use]
    pub fn is_unknown_trigger(&self) -> bool {
        matches!(self, Self::Trigger(DecodeError::KindMismatch { .. }))
    }
}

pub fn seq_mismatch(expected: Seq, actual: Seq) -> PrepareError {
    let cursor = Seq(expected.0.saturating_sub(1));
    if actual.0 > expected.0 {
        PrepareError::Gap { expected, actual }
    } else if actual == cursor && cursor.0 != 0 {
        PrepareError::Duplicate { expected, actual }
    } else {
        PrepareError::Backwards { expected, actual }
    }
}
