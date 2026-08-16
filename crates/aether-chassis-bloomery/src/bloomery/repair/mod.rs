//! Operator repair of a wedged member from a commit the coordinator can see.
//!
//! The existing repair door admits a [`aether_bloomery::CandidateRef`] the operator already
//! pushed and recorded. This module is the missing first half (#5032): given a
//! reachable commit it derives the tree and checkout digests through the same
//! domain tags the executor uses, records both correspondence rows, and pushes
//! the workpiece's candidate ref. The REST repair route then admits as before.
//!
//! Digest helpers live here so the capture path and the repair surface cannot
//! drift onto different tags.

mod digest;

pub use digest::{candidate_tree_digest, capture_commit_digest};

#[cfg(feature = "github")]
mod prepare;
#[cfg(feature = "github")]
pub use prepare::{CandidateSource, PrepareError, prepare_candidate};

#[cfg(test)]
mod tests;
