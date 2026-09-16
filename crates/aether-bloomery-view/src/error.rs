//! Failures to synchronize or inject cached views.

use std::error::Error;
use std::fmt;
use std::ops::Range;

use aether_bloomery_journal::{JournalError, Seq};

/// Failure to bind, preflight, construct, or catch a view up to a target.
#[derive(Debug)]
pub enum ViewError {
    /// The owned journal was replaced with a different allocation.
    Binding,
    /// Reading the journal head failed. No cached view was mutated.
    Head(JournalError),
    /// `target` is past the current journal head.
    BeyondHead {
        /// Requested prefix.
        target: Seq,
        /// Journal head at preflight.
        head: Seq,
    },
    /// `target` is behind a requested view's last trusted cursor.
    Behind {
        /// `std::any::type_name` of the view.
        view: &'static str,
        /// Requested prefix.
        target: Seq,
        /// Last trusted cursor of that view.
        cursor: Seq,
    },
    /// The cached slot was marked unusable by a prior constructor or fold failure.
    Poisoned {
        /// `std::any::type_name` of the view.
        view: &'static str,
        /// Last cursor that passed contract verification.
        last_trusted_cursor: Seq,
    },
    /// [`View::empty`](crate::View::empty) returned a nonzero cursor.
    NonzeroEmpty {
        /// `std::any::type_name` of the view.
        view: &'static str,
        /// Cursor the constructor left.
        cursor: Seq,
    },
    /// A journal read failed while catching a view up. Successful prefixes are kept.
    Read {
        /// `std::any::type_name` of the view being caught up.
        view: &'static str,
        /// Last cursor that passed contract verification.
        last_trusted_cursor: Seq,
        /// Page the read attempted, end exclusive.
        attempted: Range<Seq>,
        /// Backend failure.
        source: JournalError,
    },
    /// A read ended before the target even though preflight saw a sufficient head.
    Exhausted {
        /// `std::any::type_name` of the view.
        view: &'static str,
        /// Last cursor that passed contract verification.
        last_trusted_cursor: Seq,
        /// Requested prefix.
        target: Seq,
    },
    /// A page was not a dense ordered continuation of the trusted cursor.
    InvalidRange {
        /// `std::any::type_name` of the view.
        view: &'static str,
        /// Last cursor that passed contract verification.
        last_trusted_cursor: Seq,
        /// Page that was read, end exclusive.
        attempted: Range<Seq>,
        /// Sequence the page should have continued from.
        expected: Seq,
        /// Sequence actually observed.
        actual: Seq,
    },
    /// [`View::advance`](crate::View::advance) returned an error. The slot is poisoned.
    Advance {
        /// `std::any::type_name` of the view.
        view: &'static str,
        /// Last cursor that passed contract verification.
        last_trusted_cursor: Seq,
        /// Batch fed to `advance`, end exclusive.
        attempted: Range<Seq>,
        /// The view's [`View::Error`](crate::View::Error).
        source: Box<dyn Error + 'static>,
    },
    /// `advance` succeeded but the cursor was not the last entry in the batch.
    CursorContract {
        /// `std::any::type_name` of the view.
        view: &'static str,
        /// Last cursor that passed contract verification.
        last_trusted_cursor: Seq,
        /// Batch fed to `advance`, end exclusive.
        attempted: Range<Seq>,
        /// Sequence the batch ended at.
        expected: Seq,
        /// Cursor the view reported.
        actual: Seq,
    },
}

impl fmt::Display for ViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Binding => write!(f, "view registry journal is not the bound journal"),
            Self::Head(error) => write!(f, "failed to read journal head: {error}"),
            Self::BeyondHead { target, head } => {
                write!(f, "view target {target} is beyond journal head {head}")
            }
            Self::Behind { view, target, cursor } => {
                write!(f, "view {view} target {target} is behind trusted cursor {cursor}")
            }
            Self::Poisoned { view, last_trusted_cursor } => {
                write!(f, "view {view} is poisoned at trusted cursor {last_trusted_cursor}")
            }
            Self::NonzeroEmpty { view, cursor } => {
                write!(f, "view {view} empty() started at {cursor}, not seq 0")
            }
            Self::Read { view, last_trusted_cursor, attempted, source } => {
                write!(
                    f,
                    "view {view} journal read failed at trusted cursor {last_trusted_cursor} for {attempted:?}: {source}"
                )
            }
            Self::Exhausted { view, last_trusted_cursor, target } => {
                write!(f, "view {view} read exhausted at trusted cursor {last_trusted_cursor} before target {target}")
            }
            Self::InvalidRange { view, last_trusted_cursor, attempted, expected, actual } => {
                write!(
                    f,
                    "view {view} read invalid range {attempted:?} at trusted cursor {last_trusted_cursor}: expected seq {expected}, got {actual}"
                )
            }
            Self::Advance { view, last_trusted_cursor, attempted, source } => {
                write!(
                    f,
                    "view {view} advance failed at trusted cursor {last_trusted_cursor} for {attempted:?}: {source}"
                )
            }
            Self::CursorContract { view, last_trusted_cursor, attempted, expected, actual } => {
                write!(
                    f,
                    "view {view} advance left cursor {actual} at trusted cursor {last_trusted_cursor} for {attempted:?}, expected {expected}"
                )
            }
        }
    }
}

impl Error for ViewError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Head(error) | Self::Read { source: error, .. } => Some(error),
            Self::Advance { source, .. } => Some(source.as_ref()),
            Self::Binding
            | Self::BeyondHead { .. }
            | Self::Behind { .. }
            | Self::Poisoned { .. }
            | Self::NonzeroEmpty { .. }
            | Self::Exhausted { .. }
            | Self::InvalidRange { .. }
            | Self::CursorContract { .. } => None,
        }
    }
}
