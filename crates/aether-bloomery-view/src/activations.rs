//! Cursor-bearing fold: the latest activation per reactor head and any interval it owes.

use alloc::collections::BTreeMap;
use core::error::Error;
use core::fmt;

use crate::sequence::{SequenceError, check_next};
use crate::view::View;
use aether_bloomery_kinds::{Activated, ActivationRejected, DecodeError, Entry, Head, OpaqueBytes, Seq};
use aether_data::Kind;

/// Per-head activation state over a contiguous log prefix.
///
/// The cursor is the last applied [`Seq`]. It starts at `Seq(0)`, the empty
/// prefix. [`Self::apply`] requires the next contiguous sequence, including
/// unrelated entries, so the cursor is an exact statement about the observed
/// prefix rather than a best-effort watermark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activations {
    cursor: Seq,
    heads: BTreeMap<Head<OpaqueBytes>, HeadActivation>,
}

impl Activations {
    /// Empty fold: cursor `Seq(0)`, no heads seen.
    #[must_use]
    pub fn new() -> Self {
        Self { cursor: Seq(0), heads: BTreeMap::new() }
    }

    /// Last applied sequence, or `Seq(0)` when nothing has been applied.
    #[must_use]
    pub const fn cursor(&self) -> Seq {
        self.cursor
    }

    /// Apply `entry` as the next contiguous sequence.
    ///
    /// Unrelated kinds advance the cursor. An [`ActivationRejected`] owes its
    /// head the interval starting after its cause (decision 5). An
    /// [`Activated`] brings its head live, refusing to skip an owed interval.
    ///
    /// # Errors
    ///
    /// [`ActivationFoldError::Sequence`] when `entry.seq` is not the next
    /// contiguous sequence. [`ActivationFoldError::Decode`] when a recognized
    /// entry does not decode. The other variants report a history this
    /// fold's only writer, the driver, could not have produced. On error,
    /// this fold is unchanged.
    pub fn apply(&mut self, entry: &Entry) -> Result<(), ActivationFoldError> {
        check_next(self.cursor, entry.seq)?;

        if entry.kind == ActivationRejected::ID {
            self.apply_rejected(entry)?;
        } else if entry.kind == Activated::ID {
            self.apply_activated(entry)?;
        }

        self.cursor = entry.seq;
        Ok(())
    }

    /// The live activation or owed interval recorded for `head`, if any (decision 5).
    #[must_use]
    pub fn get(&self, head: &Head<OpaqueBytes>) -> Option<&HeadActivation> {
        self.heads.get(head)
    }

    fn apply_rejected(&mut self, entry: &Entry) -> Result<(), ActivationFoldError> {
        let rejected: ActivationRejected = entry.decode()?;
        let cause = entry.cause.ok_or(ActivationFoldError::UncausedRejection { seq: entry.seq })?;
        check_cause_in_range(entry.seq, cause)?;

        let already_owed = matches!(self.heads.get(&rejected.head), Some(HeadActivation::Owed { .. }));
        if !already_owed {
            self.heads.insert(rejected.head, HeadActivation::Owed { from: Seq(cause.0 + 1) });
        }
        Ok(())
    }

    fn apply_activated(&mut self, entry: &Entry) -> Result<(), ActivationFoldError> {
        let activated: Activated = entry.decode()?;

        if let Some(HeadActivation::Owed { from }) = self.heads.get(activated.head())
            && activated.live_from() != *from
        {
            return Err(ActivationFoldError::OwedMismatch {
                seq: entry.seq,
                head: activated.head().clone(),
                owed_from: *from,
                live_from: activated.live_from(),
            });
        }

        self.heads.insert(activated.head().clone(), HeadActivation::Live(activated));
        Ok(())
    }
}

impl Default for Activations {
    fn default() -> Self {
        Self::new()
    }
}

impl View for Activations {
    type Error = ActivationFoldError;

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

/// One reactor head's activation state: never both live and owed (decision 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadActivation {
    /// The reactor instance currently serving this head.
    Live(Activated),
    /// No instance serves this head; the interval from `from` onward is owed
    /// to the next successful activation.
    Owed {
        /// The first seq the next activation must evaluate live.
        from: Seq,
    },
}

/// Failure to fold one journal entry into [`Activations`].
#[derive(Debug)]
pub enum ActivationFoldError {
    /// `entry.seq` was not the next contiguous sequence.
    Sequence(SequenceError),
    /// A recognized entry's payload did not decode.
    Decode(DecodeError),
    /// A cause did not name an earlier entry in this prefix.
    CauseOutOfRange {
        /// The entry that named the cause.
        seq: Seq,
        /// The cause it named.
        cause: Seq,
    },
    /// An `ActivationRejected` carried no cause.
    UncausedRejection {
        /// The `ActivationRejected` entry.
        seq: Seq,
    },
    /// An `Activated` skipped or overlapped the interval its head owed.
    OwedMismatch {
        /// The `Activated` entry.
        seq: Seq,
        /// The head it activated.
        head: Head<OpaqueBytes>,
        /// The first seq that head owed.
        owed_from: Seq,
        /// The first seq the activation actually evaluates live.
        live_from: Seq,
    },
}

impl fmt::Display for ActivationFoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sequence(error) => write!(f, "{error}"),
            Self::Decode(error) => write!(f, "{error}"),
            Self::CauseOutOfRange { seq, cause } => {
                write!(f, "entry {seq} names cause {cause}, which is not an earlier entry in this prefix")
            }
            Self::UncausedRejection { seq } => write!(f, "entry {seq} is an activation rejection with no cause"),
            Self::OwedMismatch { seq, head, owed_from, live_from } => write!(
                f,
                "entry {seq} activates head {head:?} from {live_from}, but it owed the interval from {owed_from}"
            ),
        }
    }
}

impl Error for ActivationFoldError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sequence(error) => Some(error),
            Self::Decode(error) => Some(error),
            Self::CauseOutOfRange { .. } | Self::UncausedRejection { .. } | Self::OwedMismatch { .. } => None,
        }
    }
}

impl From<SequenceError> for ActivationFoldError {
    fn from(error: SequenceError) -> Self {
        Self::Sequence(error)
    }
}

impl From<DecodeError> for ActivationFoldError {
    fn from(error: DecodeError) -> Self {
        Self::Decode(error)
    }
}

fn check_cause_in_range(seq: Seq, cause: Seq) -> Result<(), ActivationFoldError> {
    if cause.0 == 0 || cause.0 >= seq.0 {
        Err(ActivationFoldError::CauseOutOfRange { seq, cause })
    } else {
        Ok(())
    }
}
