//! The read port an executor gets.

use std::error::Error;
use std::fmt;

use aether_bloomery_journal::{DecodeError, GetError, Journal, JournalError};
use aether_bloomery_kinds::Digest;
use aether_data::{KindId, Storage};

/// Read artifacts from a store. The journal is the only implementation here.
pub trait ReadArtifacts {
    /// Load one artifact as `(kind, payload)`. `Ok(None)` when absent.
    ///
    /// # Errors
    ///
    /// [`ReadError`] on a backend or corrupt-blob failure.
    fn get_bytes(&self, digest: &Digest) -> Result<Option<(KindId, Vec<u8>)>, ReadError>;
}

impl ReadArtifacts for Journal {
    #[allow(clippy::use_self)]
    fn get_bytes(&self, digest: &Digest) -> Result<Option<(KindId, Vec<u8>)>, ReadError> {
        Journal::get_bytes(self, digest).map_err(ReadError::Journal)
    }
}

impl dyn ReadArtifacts + '_ {
    /// Load and decode a stored encoded artifact as `K`.
    ///
    /// # Errors
    ///
    /// [`ReadError`] wrapping a get or decode failure.
    pub fn get<K: Storage>(&self, digest: &Digest) -> Result<Option<K>, ReadError> {
        decode_loaded(self.get_bytes(digest)?)
    }
}

fn decode_loaded<K: Storage>(loaded: Option<(KindId, Vec<u8>)>) -> Result<Option<K>, ReadError> {
    match loaded {
        None => Ok(None),
        Some((kind, payload)) if kind == K::ID => K::decode_storage(&payload)
            .map(|data| Some(data.value))
            .map_err(|error| ReadError::Get(GetError::Decode(DecodeError::Storage(error)))),
        Some((actual, _)) => Err(ReadError::Get(GetError::PrefixMismatch { expected: K::ID, actual })),
    }
}

/// Failure to read an artifact for an executor.
#[derive(Debug)]
pub enum ReadError {
    /// Typed get failed.
    Get(GetError),
    /// Backend or corrupt-blob failure.
    Journal(JournalError),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Get(error) => write!(f, "{error}"),
            Self::Journal(error) => write!(f, "{error}"),
        }
    }
}

impl Error for ReadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Get(error) => Some(error),
            Self::Journal(error) => Some(error),
        }
    }
}

impl From<GetError> for ReadError {
    fn from(error: GetError) -> Self {
        Self::Get(error)
    }
}

impl From<JournalError> for ReadError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}
