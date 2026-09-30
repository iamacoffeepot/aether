//! Shared next-contiguous-sequence check for journal folds.

use core::error::Error;
use core::fmt;

use aether_bloomery_kinds::Seq;

/// Failure of the shared next-sequence check.
#[derive(Debug)]
pub enum SequenceError {
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
}

impl fmt::Display for SequenceError {
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
        }
    }
}

impl Error for SequenceError {}

/// Require `actual` to be the next contiguous sequence after `cursor`.
///
/// # Errors
///
/// [`SequenceError::Gap`], [`SequenceError::Duplicate`], or
/// [`SequenceError::Backwards`] when `actual` is not the next sequence.
/// [`SequenceError::Overflow`] when the next sequence does not fit in [`Seq`].
pub fn check_next(cursor: Seq, actual: Seq) -> Result<(), SequenceError> {
    let expected = cursor.0.checked_add(1).map(Seq).ok_or(SequenceError::Overflow)?;
    if actual == expected {
        return Ok(());
    }
    if actual.0 > expected.0 {
        Err(SequenceError::Gap { expected, actual })
    } else if actual == cursor && cursor.0 != 0 {
        Err(SequenceError::Duplicate { expected, actual })
    } else {
        Err(SequenceError::Backwards { expected, actual })
    }
}
