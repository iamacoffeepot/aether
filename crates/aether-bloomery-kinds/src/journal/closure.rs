//! Transitive-closure read over the journal's stored citation edges (ADR-0226 decision 10).

use alloc::string::String;
use alloc::vec::Vec;
use core::borrow::Borrow;
use core::error::Error as StdError;
use core::fmt;

use crate::{ClosureArtifact, Digest};

/// Byte budget for one closure read: at least one stored blob's kind prefix, at most 4 GiB.
///
/// A closure always contains its root, and every stored blob is at least
/// the eight-byte kind prefix, so a smaller limit could never be satisfied.
/// The ceiling bounds the resident bytes the journal checks into the engine
/// blob store for one closure, not a mail frame: members cross mail as
/// [`Blob`](aether_data::Blob)s (ADR-0238 decision 10). Construction and
/// every decode path re-run the same check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct ClosureLimit(u64);

impl ClosureLimit {
    /// Smallest accepted limit: one kind prefix.
    pub const MIN_BYTES: u64 = 8;

    /// Largest accepted limit: 4 GiB of resident closure bytes.
    pub const MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

    /// Accept a byte limit in `MIN_BYTES ..= MAX_BYTES`.
    ///
    /// # Errors
    ///
    /// [`ClosureLimitError::BelowPrefix`] below [`Self::MIN_BYTES`];
    /// [`ClosureLimitError::AboveCeiling`] above [`Self::MAX_BYTES`].
    pub fn new(bytes: u64) -> Result<Self, ClosureLimitError> {
        Self::check(bytes)?;
        Ok(Self(bytes))
    }

    /// The limit in bytes.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    // `#[storage(validate)]` calls `check(&inner)`; `Borrow` takes that
    // reference and `new`'s owned value alike, with no lint to suppress.
    fn check(bytes: impl Borrow<u64>) -> Result<(), ClosureLimitError> {
        let bytes = *bytes.borrow();
        if bytes < Self::MIN_BYTES {
            Err(ClosureLimitError::BelowPrefix)
        } else if bytes > Self::MAX_BYTES {
            Err(ClosureLimitError::AboveCeiling)
        } else {
            Ok(())
        }
    }
}

/// Why a closure byte limit was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosureLimitError {
    /// Smaller than one eight-byte kind prefix; no closure can fit.
    BelowPrefix,
    /// Larger than [`ClosureLimit::MAX_BYTES`].
    AboveCeiling,
}

impl ClosureLimitError {
    const fn reason(self) -> &'static str {
        match self {
            Self::BelowPrefix => "below-prefix",
            Self::AboveCeiling => "above-ceiling",
        }
    }
}

impl aether_data::Invariant for ClosureLimitError {
    fn reason(&self) -> &'static str {
        Self::reason(*self)
    }
}

impl fmt::Display for ClosureLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl StdError for ClosureLimitError {}

/// Read `root` and every artifact it transitively cites, under a byte budget.
///
/// The walk follows the citation edges the journal recorded when each
/// artifact was first stored, so it follows only `Ref<K>` citations. The
/// budget is the sum of each distinct member's stored blob length (the
/// eight-byte kind prefix plus the payload). The walk never truncates: a
/// closure over the limit is `TooLarge`, never a partial list. Artifacts
/// stored before the journal recorded citation edges have no edges, so their
/// closure is the artifact alone.
#[aether_data::kind(name = "aether.bloomery.journal.read_closure", copy, eq, no_serde)]
pub struct ReadClosure {
    /// Digest of the closure's root artifact.
    pub root: Digest,
    /// Largest total stored blob length the reply may carry.
    pub limit_bytes: ClosureLimit,
}

/// Exactly one outcome of a closure read.
#[aether_data::kind(name = "aether.bloomery.journal.read_closure_result", no_serde)]
pub enum ReadClosureResult {
    /// Every distinct reachable artifact, within the limit.
    Found {
        /// Requested root digest.
        root: Digest,
        /// The root first, then breadth-first levels; each member's cited
        /// children in ascending digest byte order. Each artifact appears once.
        artifacts: Vec<ClosureArtifact>,
    },
    /// A closure member is not stored. In a healthy journal only the root can be.
    Missing {
        /// Requested root digest.
        root: Digest,
        /// The digest with no stored artifact.
        digest: Digest,
    },
    /// The closure's total stored blob length exceeds the limit. Nothing is returned.
    TooLarge {
        /// Requested root digest.
        root: Digest,
        /// The limit the closure exceeded.
        limit_bytes: ClosureLimit,
    },
    /// Corrupt stored data or a journal backend failure.
    Err {
        /// Requested root digest.
        root: Digest,
        /// Human-readable failure.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use aether_data::Kind;
    use aether_data::wire::encode_to_vec;

    use super::{ClosureLimit, ClosureLimitError, ReadClosure};
    use crate::Digest;

    #[test]
    fn limit_bounds_are_inclusive() {
        // Catches an off-by-one in `check` at either bound.
        assert_eq!(ClosureLimit::new(ClosureLimit::MIN_BYTES - 1), Err(ClosureLimitError::BelowPrefix));
        assert_eq!(ClosureLimit::new(ClosureLimit::MAX_BYTES + 1), Err(ClosureLimitError::AboveCeiling));
        assert_eq!(ClosureLimit::new(ClosureLimit::MIN_BYTES).map(ClosureLimit::get), Ok(ClosureLimit::MIN_BYTES));
        assert_eq!(ClosureLimit::new(ClosureLimit::MAX_BYTES).map(ClosureLimit::get), Ok(ClosureLimit::MAX_BYTES));
    }

    #[test]
    fn read_closure_decode_refuses_a_zero_limit() {
        // Catches a dropped `#[storage(validate)]`, which would let an invalid limit in through mail.
        let root = Digest::from_bytes([3; 32]);
        let encode = |limit: u64| -> Vec<u8> {
            let mut bytes = encode_to_vec(&root).expect("encode root");
            bytes.extend(encode_to_vec(&limit).expect("encode limit"));
            bytes
        };
        let valid = ReadClosure { root, limit_bytes: ClosureLimit::new(ClosureLimit::MIN_BYTES).expect("minimum") };
        assert_eq!(valid.encode_into_bytes(), encode(ClosureLimit::MIN_BYTES), "hand-built layout matches the kind");
        assert_eq!(ReadClosure::decode_from_bytes(&encode(ClosureLimit::MIN_BYTES)), Some(valid));
        assert_eq!(ReadClosure::decode_from_bytes(&encode(0)), None);
    }
}
