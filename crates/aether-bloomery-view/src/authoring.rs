//! Runtime support for authored aggregate views.

use alloc::boxed::Box;
use core::error::Error;
use core::fmt;

use aether_bloomery_kinds::{DecodeError, Seq};

use crate::sequence::{SequenceError, check_next as check_sequence};

/// Cursor stored explicitly by an authored aggregate view.
///
/// The default is the empty journal prefix at sequence zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ViewCursor(Seq);

impl ViewCursor {
    /// Last sequence consumed by the aggregate.
    #[must_use]
    pub const fn get(self) -> Seq {
        self.0
    }

    /// Commit a successfully consumed sequence.
    #[doc(hidden)]
    pub fn set(&mut self, seq: Seq) {
        self.0 = seq;
    }
}

impl Default for ViewCursor {
    fn default() -> Self {
        Self(Seq(0))
    }
}

/// Failure generated while advancing an authored aggregate view.
#[derive(Debug)]
pub enum ViewFoldError {
    /// The entry was not the next contiguous sequence.
    Sequence(SequenceError),
    /// A matching entry had malformed stored bytes.
    Decode {
        /// Fold method that attempted the decode.
        handler: &'static str,
        /// Stored-value decoding failure.
        source: DecodeError,
    },
    /// A fallible fold method refused the decoded event.
    Handler {
        /// Fold method that returned the error.
        handler: &'static str,
        /// Original handler error.
        source: Box<dyn Error + 'static>,
    },
}

impl ViewFoldError {
    /// Wrap a decode failure with the fold method that matched the entry.
    #[doc(hidden)]
    #[must_use]
    pub fn decode(handler: &'static str, source: DecodeError) -> Self {
        Self::Decode { handler, source }
    }

    /// Wrap a fallible handler's original error.
    #[doc(hidden)]
    #[must_use]
    pub fn handler(handler: &'static str, source: impl Error + 'static) -> Self {
        Self::Handler { handler, source: Box::new(source) }
    }

    /// Fold method associated with a decode or handler failure.
    #[must_use]
    pub const fn handler_name(&self) -> Option<&'static str> {
        match self {
            Self::Sequence(_) => None,
            Self::Decode { handler, .. } | Self::Handler { handler, .. } => Some(handler),
        }
    }
}

impl fmt::Display for ViewFoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sequence(error) => error.fmt(f),
            Self::Decode { handler, source } => write!(f, "view fold `{handler}` could not decode its event: {source}"),
            Self::Handler { handler, source } => write!(f, "view fold `{handler}` failed: {source}"),
        }
    }
}

impl Error for ViewFoldError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sequence(error) => Some(error),
            Self::Decode { source, .. } => Some(source),
            Self::Handler { source, .. } => Some(source.as_ref()),
        }
    }
}

impl From<SequenceError> for ViewFoldError {
    fn from(error: SequenceError) -> Self {
        Self::Sequence(error)
    }
}

/// Require an entry to follow an authored view cursor.
#[doc(hidden)]
pub fn check_next(cursor: ViewCursor, actual: Seq) -> Result<(), ViewFoldError> {
    check_sequence(cursor.get(), actual).map_err(ViewFoldError::from)
}
