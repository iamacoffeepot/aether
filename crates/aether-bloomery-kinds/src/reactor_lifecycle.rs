//! Conventional kernel configuration heads and recorded lifecycle observations.

use crate::{Detail, Head, OpaqueBytes, ReactorSet, Ref};

/// Required kernel bundle head. The selected set must include this head.
pub const KERNEL_HEAD: Head<OpaqueBytes> = Head::new("core.kernel");

/// Root head whose artifact declares the selected reactor cluster heads.
pub const REACTORS_HEAD: Head<ReactorSet> = Head::new("core.reactors");

/// Native lifecycle observation written as journal data.
///
/// Historical head and set bindings alone select event recipients. In
/// particular, `Rejected` describes an attempted artifact, not a selected or
/// active one. The containing journal entry supplies sequence and cause.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.reactor_lifecycle_outcome")]
pub enum ReactorLifecycleOutcome {
    /// The selected artifact became resident for this cluster head.
    Activated { cluster: Head<OpaqueBytes>, artifact: Ref<OpaqueBytes> },
    /// Preparation refused an attempted artifact before publication.
    Rejected { cluster: Head<OpaqueBytes>, attempted: Ref<OpaqueBytes>, reason: Detail },
    /// The cluster head left the selected set and its slot was retired.
    Retired { cluster: Head<OpaqueBytes> },
}
