//! Reading several named artifacts in one request, under a byte limit (ADR-0240 D7).

use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;

use aether_actor::HeldReply;

use aether_data::Digest;

use crate::{ClosureArtifact, ClosureLimit};

/// The digests one [`ReadArtifacts`] names, in request order: at least one,
/// at most [`ReadArtifacts::MAX_ARTIFACTS`]. Construction and every decode
/// path run the same check.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct ArtifactDigests(Vec<Digest>);

impl ArtifactDigests {
    /// Accept a non-empty list of at most [`ReadArtifacts::MAX_ARTIFACTS`] digests.
    ///
    /// # Errors
    ///
    /// [`ArtifactDigestsError::Empty`] for no digests;
    /// [`ArtifactDigestsError::TooMany`] above [`ReadArtifacts::MAX_ARTIFACTS`].
    pub fn new(digests: Vec<Digest>) -> Result<Self, ArtifactDigestsError> {
        Self::check(&digests)?;
        Ok(Self(digests))
    }

    /// The digests, in request order.
    #[must_use]
    pub fn as_slice(&self) -> &[Digest] {
        &self.0
    }

    fn check(digests: &[Digest]) -> Result<(), ArtifactDigestsError> {
        if digests.is_empty() {
            Err(ArtifactDigestsError::Empty)
        } else if digests.len() > ReadArtifacts::MAX_ARTIFACTS {
            Err(ArtifactDigestsError::TooMany)
        } else {
            Ok(())
        }
    }
}

/// Why a digest list was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactDigestsError {
    /// No digests; a read must name at least one.
    Empty,
    /// More than [`ReadArtifacts::MAX_ARTIFACTS`] digests.
    TooMany,
}

impl ArtifactDigestsError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooMany => "too-many",
        }
    }
}

impl aether_data::Invariant for ArtifactDigestsError {
    fn reason(&self) -> &'static str {
        Self::reason(*self)
    }
}

impl fmt::Display for ArtifactDigestsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl StdError for ArtifactDigestsError {}

/// Read the artifacts `digests` names, in order, up to a byte limit.
///
/// The limit counts each answered artifact's stored blob length (the
/// eight-byte kind prefix plus the payload), as [`ReadClosure`](crate::ReadClosure)
/// counts its members. Unlike a closure read, the answer is a prefix by
/// contract: the caller knows what it asked for and asks again for the rest.
#[aether_data::kind(name = "aether.bloomery.journal.read_artifacts", eq, no_serde)]
pub struct ReadArtifacts {
    /// The artifacts to read, in the order the answer lists them.
    pub digests: ArtifactDigests,
    /// Largest total stored blob length the answer may carry past its first artifact.
    pub limit_bytes: ClosureLimit,
}

impl ReadArtifacts {
    /// Most digests one read may name: the stage batch's artifact bound.
    pub const MAX_ARTIFACTS: usize = 4_096;
}

/// Exactly one outcome of a [`ReadArtifacts`].
#[aether_data::kind(name = "aether.bloomery.journal.read_artifacts_result", no_serde)]
pub enum ReadArtifactsResult {
    /// A non-empty prefix of the requested artifacts, in request order: every
    /// artifact while the running stored length stays within the limit, and
    /// always the first, so an artifact larger than the limit is answered
    /// alone. Each member's claimed digest is the requested one, and a reader
    /// verifies it through [`ClosureArtifact::load`].
    Found {
        /// The answered prefix.
        artifacts: Vec<ClosureArtifact>,
    },
    /// An artifact before the cut is not stored.
    Missing {
        /// The first requested digest with no stored artifact.
        digest: Digest,
    },
    /// Corrupt stored data or a journal backend failure.
    Err {
        /// Human-readable failure.
        message: String,
    },
}

impl HeldReply for ReadArtifactsResult {
    fn unanswered() -> Self {
        Self::Err { message: String::from("bloomery journal closed before answering") }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use aether_data::wire::encode_to_vec;
    use aether_data::{Digest, Kind};

    use super::{ArtifactDigests, ArtifactDigestsError, ReadArtifacts};
    use crate::ClosureLimit;

    fn digests(count: usize) -> Vec<Digest> {
        (0..count).map(|index| Digest::from_bytes([u8::try_from(index % 256).expect("byte"); 32])).collect()
    }

    #[test]
    fn digest_count_bounds_are_inclusive() {
        // Catches an off-by-one in `check` at either bound.
        assert_eq!(ArtifactDigests::new(Vec::new()), Err(ArtifactDigestsError::Empty));
        assert_eq!(ArtifactDigests::new(digests(ReadArtifacts::MAX_ARTIFACTS + 1)), Err(ArtifactDigestsError::TooMany));
        assert!(ArtifactDigests::new(digests(1)).is_ok());
        assert!(ArtifactDigests::new(digests(ReadArtifacts::MAX_ARTIFACTS)).is_ok());
    }

    #[test]
    fn read_artifacts_decode_refuses_an_empty_or_oversized_list() {
        // Catches a dropped `#[storage(validate)]`, which would let an invalid list in through mail.
        let limit = ClosureLimit::new(ClosureLimit::MIN_BYTES).expect("minimum");
        let encode = |list: &Vec<Digest>| -> Vec<u8> {
            let mut bytes = encode_to_vec(list).expect("encode digests");
            bytes.extend(encode_to_vec(&limit.get()).expect("encode limit"));
            bytes
        };

        let one = digests(1);
        let valid = ReadArtifacts { digests: ArtifactDigests::new(one.clone()).expect("one"), limit_bytes: limit };
        assert_eq!(valid.encode_into_bytes(), encode(&one), "hand-built layout matches the kind");
        assert_eq!(ReadArtifacts::decode_from_bytes(&encode(&one)), Some(valid));
        assert_eq!(ReadArtifacts::decode_from_bytes(&encode(&vec![])), None);
        assert_eq!(ReadArtifacts::decode_from_bytes(&encode(&digests(ReadArtifacts::MAX_ARTIFACTS + 1))), None);
    }
}
