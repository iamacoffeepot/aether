//! The name fold: last `ProgramNameMoved` per name in seq order.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use aether_bloomery_journal::{DecodeError, Journal, JournalError, Seq};
use aether_bloomery_kinds::{ProgramName, ProgramNameMoved, Ref};
use aether_data::Kind;

use crate::kinds;

const PAGE: usize = 256;

/// Failure to fold named programs.
#[derive(Debug)]
pub enum FoldError {
    /// Backend failure.
    Journal(JournalError),
    /// A `ProgramNameMoved` entry did not decode.
    Decode(DecodeError),
}

impl fmt::Display for FoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Journal(error) => write!(f, "{error}"),
            Self::Decode(error) => write!(f, "{error}"),
        }
    }
}

impl Error for FoldError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Journal(error) => Some(error),
            Self::Decode(error) => Some(error),
        }
    }
}

impl From<JournalError> for FoldError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

impl From<DecodeError> for FoldError {
    fn from(error: DecodeError) -> Self {
        Self::Decode(error)
    }
}

/// Current program for every name: the last [`ProgramNameMoved`] per name in seq order.
///
/// # Errors
///
/// [`FoldError`] on a backend or decode failure.
pub fn named(journal: &Journal) -> Result<BTreeMap<ProgramName, Ref<kinds::Program>>, FoldError> {
    let mut since = Seq(0);
    let mut names = BTreeMap::new();
    loop {
        let page = journal.read(since, PAGE)?;
        if page.is_empty() {
            return Ok(names);
        }
        for entry in page {
            if entry.kind == ProgramNameMoved::NAME {
                let event = Journal::decode::<ProgramNameMoved>(&entry)?;
                names.insert(event.name, event.program);
            }
            since = entry.seq;
        }
    }
}
