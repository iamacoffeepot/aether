//! One entry's direct citations, handed to a fold or rule beside the entry.

use alloc::vec::Vec;
use core::error::Error;
use core::fmt;

use aether_bloomery_kinds::{ClosureArtifact, Digest, DigestMismatch, Ref};
use aether_data::{KindId, Storage, StorageError};

/// The artifacts one journal entry cites directly, read by the driver before
/// the entry was delivered.
///
/// [`Self::get`] answers only a digest the entry itself cites, so a ref found
/// inside a cited artifact is not here: a fold or rule sees one level of
/// citations. Every read verifies the artifact's bytes against the digest,
/// so an artifact is never trusted on its sender's claim. An entry appended
/// before the journal recorded entry citations cites nothing.
#[derive(Clone, Debug, Default)]
pub struct Cited {
    digests: Vec<Digest>,
    artifacts: Vec<ClosureArtifact>,
}

impl Cited {
    /// The citations of one entry that cites `digests`, holding the
    /// artifacts among `pool` whose claimed digest it cites. An artifact in
    /// `pool` the entry does not cite is left out.
    #[must_use]
    pub fn new(digests: Vec<Digest>, pool: &[ClosureArtifact]) -> Self {
        let artifacts =
            pool.iter().filter(|artifact| digests.contains(&artifact.claimed().unverified())).cloned().collect();
        Self { digests, artifacts }
    }

    /// The digests the entry cites, in citation order.
    #[must_use]
    pub fn digests(&self) -> &[Digest] {
        &self.digests
    }

    /// Read the artifact `cited` names and decode it as `K`.
    ///
    /// # Errors
    ///
    /// [`CitedError::NotCited`] when the entry does not cite `cited`'s
    /// digest, [`CitedError::Missing`] when it does but its artifact was not
    /// delivered, [`CitedError::Mismatch`] when the delivered bytes do not
    /// hash to the digest, [`CitedError::KindMismatch`] when the artifact is
    /// not a `K`, and [`CitedError::Decode`] when its payload does not decode
    /// as `K`.
    pub fn get<K: Storage>(&self, cited: Ref<K>) -> Result<K, CitedError> {
        let digest = cited.digest();
        let artifact = self.delivered(digest)?;
        let payload = artifact.load(digest).map_err(CitedError::Mismatch)?;
        if artifact.kind() != K::ID {
            return Err(CitedError::KindMismatch { digest, expected: K::ID, actual: artifact.kind() });
        }
        K::decode_storage(&payload).map(|data| data.value).map_err(|source| CitedError::Decode { digest, source })
    }
}

impl Cited {
    /// The kind the artifact at `digest` is stored under, read from the
    /// artifact delivered with the entry. Its bytes are verified against the
    /// digest, which covers the kind.
    ///
    /// # Errors
    ///
    /// [`CitedError::NotCited`] when the entry does not cite `digest`,
    /// [`CitedError::Missing`] when it does but its artifact was not
    /// delivered, and [`CitedError::Mismatch`] when the delivered bytes do not
    /// hash to the digest.
    pub fn kind(&self, digest: Digest) -> Result<KindId, CitedError> {
        let artifact = self.delivered(digest)?;
        artifact.load(digest).map_err(CitedError::Mismatch)?;
        Ok(artifact.kind())
    }

    /// The artifact delivered for `digest`, which the entry cites.
    fn delivered(&self, digest: Digest) -> Result<&ClosureArtifact, CitedError> {
        if !self.digests.contains(&digest) {
            return Err(CitedError::NotCited { digest });
        }
        self.artifacts
            .iter()
            .find(|artifact| artifact.claimed().unverified() == digest)
            .ok_or(CitedError::Missing { digest })
    }
}

/// Why [`Cited::get`] or [`Cited::kind`] refused a read.
#[derive(Debug)]
pub enum CitedError {
    /// The entry does not cite this digest, such as a ref found inside a
    /// cited artifact or one a neighbouring entry cites.
    NotCited {
        /// The digest asked for.
        digest: Digest,
    },
    /// The entry cites this digest, but its artifact was not delivered.
    Missing {
        /// The digest asked for.
        digest: Digest,
    },
    /// The delivered bytes do not hash to the digest they were read under.
    Mismatch(DigestMismatch),
    /// The artifact is stored under another kind than the one asked for.
    KindMismatch {
        /// The digest asked for.
        digest: Digest,
        /// The kind the reader asked to decode.
        expected: KindId,
        /// The kind the artifact is stored under.
        actual: KindId,
    },
    /// The payload did not decode as the requested kind.
    Decode {
        /// The digest asked for.
        digest: Digest,
        /// The storage decode failure.
        source: StorageError,
    },
}

impl fmt::Display for CitedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotCited { digest } => write!(f, "the entry does not cite {digest}"),
            Self::Missing { digest } => write!(f, "cited artifact {digest} was not delivered"),
            Self::Mismatch(error) => error.fmt(f),
            Self::KindMismatch { digest, expected, actual } => {
                write!(f, "cited artifact {digest} is kind {actual}, not {expected}")
            }
            Self::Decode { digest, source } => write!(f, "cited artifact {digest} did not decode: {source}"),
        }
    }
}

impl Error for CitedError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotCited { .. } | Self::Missing { .. } | Self::KindMismatch { .. } => None,
            Self::Mismatch(error) => Some(error),
            Self::Decode { source, .. } => Some(source),
        }
    }
}
