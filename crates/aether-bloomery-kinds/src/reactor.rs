//! Canonical membership of journal-selected reactor clusters.

use alloc::vec::Vec;
use core::error::Error;
use core::fmt;

use crate::{Head, OpaqueBytes};

/// Why a reactor set's stored member order was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactorSetError {
    /// One head appeared more than once.
    Duplicate,
    /// A later head sorted before its predecessor.
    Unsorted,
}

impl aether_data::Invariant for ReactorSetError {
    fn reason(&self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate",
            Self::Unsorted => "unsorted",
        }
    }
}

impl fmt::Display for ReactorSetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(aether_data::Invariant::reason(self))
    }
}

impl Error for ReactorSetError {}

/// Sorted, unique bundle-head names. Validation also runs when stored bytes decode.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
struct Clusters(Vec<Head<OpaqueBytes>>);

impl Clusters {
    fn check(heads: &[Head<OpaqueBytes>]) -> Result<(), ReactorSetError> {
        for pair in heads.windows(2) {
            if pair[0] == pair[1] {
                return Err(ReactorSetError::Duplicate);
            }
            if pair[0] > pair[1] {
                return Err(ReactorSetError::Unsorted);
            }
        }
        Ok(())
    }
}

/// Persisted reactor-set membership, ordered by the head's exact name.
///
/// Each member is a moving `Head<OpaqueBytes>`, not a pinned artifact digest.
/// Selection resolves every member at the event's historical boundary. An
/// empty set is encodable; the selector refuses one that omits its required
/// kernel head.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.reactor_set")]
pub struct ReactorSet {
    clusters: Clusters,
}

impl ReactorSet {
    /// Accept members already in strictly increasing head-name order.
    ///
    /// # Errors
    ///
    /// [`ReactorSetError::Duplicate`] or [`ReactorSetError::Unsorted`] when
    /// `clusters` is not canonical.
    pub fn new(clusters: Vec<Head<OpaqueBytes>>) -> Result<Self, ReactorSetError> {
        Clusters::check(&clusters)?;
        Ok(Self { clusters: Clusters(clusters) })
    }

    /// The canonical cluster-head names. Equal artifact digests do not merge
    /// entries because the heads remain separate identities.
    #[must_use]
    pub fn clusters(&self) -> &[Head<OpaqueBytes>] {
        &self.clusters.0
    }
}
