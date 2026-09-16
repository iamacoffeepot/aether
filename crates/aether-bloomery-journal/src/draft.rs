//! The only append path: a typed [`aether_data::Storage`] value encoded before it reaches SQLite.

use std::error::Error;
use std::fmt;

use aether_data::{Kind, Storage, StorageData, StorageError};

use crate::Seq;

/// An encoded event ready to append. There is no public path from raw bytes into the log.
pub struct Draft {
    pub(crate) kind: String,
    pub(crate) cause: Option<Seq>,
    pub(crate) bytes: Vec<u8>,
}

impl Draft {
    /// Encode `event` as a draft.
    ///
    /// `K` is `Clone` because [`Storage::encode_storage`] takes an owned
    /// [`StorageData`]. The public `&K` argument is the issue's binding shape.
    ///
    /// # Errors
    ///
    /// Returns [`DraftError::Storage`] when encoding fails.
    pub fn of<K: Storage + Clone>(event: &K, cause: Option<Seq>) -> Result<Self, DraftError> {
        let bytes = K::encode_storage(&StorageData::from_value(event.clone())).map_err(DraftError::Storage)?;
        Ok(Self { kind: K::NAME.to_owned(), cause, bytes })
    }
}

/// Failure to encode a draft.
#[derive(Debug)]
pub enum DraftError {
    /// [`Storage::encode_storage`] refused the value.
    Storage(StorageError),
}

impl fmt::Display for DraftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(f, "failed to encode draft: {error}"),
        }
    }
}

impl Error for DraftError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
        }
    }
}
