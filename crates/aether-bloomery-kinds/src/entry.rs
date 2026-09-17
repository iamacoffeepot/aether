//! Entry envelope: identity, kind name, optional cause, wall clock, payload bytes.

use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error;
use core::fmt;

use aether_data::{KindId, Storage, StorageError};

/// Dense sequence number assigned by the store. Starts at 1; `Seq(0)` is the empty-journal head.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(pub u64);

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One recorded journal entry. `bytes` are the verbatim [`aether_data::Storage::encode_storage`] output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Store-assigned identity and fence.
    pub seq: Seq,
    /// `K::NAME` of the appended kind.
    pub kind: String,
    /// The `seq` this entry reacts to, if any.
    pub cause: Option<Seq>,
    /// Wall clock at insert, for people and consoles. A fold never reads it.
    pub recorded_at_millis: u64,
    /// Encoded payload, stored verbatim.
    pub bytes: Vec<u8>,
}

impl Entry {
    /// Decode this entry as `K`. Refuses when `kind` is not `K::NAME`.
    ///
    /// A well-formed payload of a different typed specialization of a shared
    /// stored kind is [`DecodeError::SpecializationMismatch`], not a broken
    /// payload. Malformed bytes stay [`DecodeError::Storage`].
    ///
    /// # Errors
    ///
    /// [`DecodeError::KindMismatch`] when the stored name is not `K::NAME`.
    /// [`DecodeError::SpecializationMismatch`] when the payload discriminator
    /// is not `K`. [`DecodeError::Storage`] when TLV decode fails.
    pub fn decode<K: Storage>(&self) -> Result<K, DecodeError> {
        if self.kind != K::NAME {
            return Err(DecodeError::KindMismatch { expected: K::NAME, actual: self.kind.clone() });
        }
        K::decode_storage(&self.bytes).map(|data| data.value).map_err(DecodeError::from_storage)
    }
}

/// Failure to decode an entry as a requested kind.
#[derive(Debug)]
pub enum DecodeError {
    /// `entry.kind` was not `K::NAME`.
    KindMismatch {
        /// `K::NAME` the caller asked for.
        expected: &'static str,
        /// Name stored on the entry.
        actual: String,
    },
    /// The envelope kind matched, but the payload is a different typed
    /// specialization of that shared stored kind.
    SpecializationMismatch {
        /// Kind id the caller asked to specialize as.
        expected: KindId,
        /// Kind id stored on the payload discriminator.
        actual: KindId,
    },
    /// TLV decode failed.
    Storage(StorageError),
}

impl DecodeError {
    /// Envelope or specialization mismatch. Matching layers decline; they do
    /// not treat this as a broken payload.
    #[must_use]
    pub const fn is_unmatched(&self) -> bool {
        matches!(self, Self::KindMismatch { .. } | Self::SpecializationMismatch { .. })
    }

    fn from_storage(error: StorageError) -> Self {
        match error {
            StorageError::TypeMismatch { expected, actual } => Self::SpecializationMismatch { expected, actual },
            other => Self::Storage(other),
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KindMismatch { expected, actual } => {
                write!(f, "entry kind {actual:?} is not {expected:?}")
            }
            Self::SpecializationMismatch { expected, actual } => {
                write!(f, "entry specialization {actual} is not {expected}")
            }
            Self::Storage(error) => write!(f, "failed to decode entry: {error}"),
        }
    }
}

impl Error for DecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::KindMismatch { .. } | Self::SpecializationMismatch { .. } => None,
            Self::Storage(error) => Some(error),
        }
    }
}
