//! The filesystem side of the tier policy: read `approval-policy.yml` off the
//! host and hand back the canonical [`ApprovalPolicy`] value.
//!
//! The policy itself — the `{default, rules}` table, the strict text parser, and
//! the most-restrictive-wins resolver — lives in `aether-bloomery` as the sealed
//! `aether.bloomery.approval_policy` kind (#4616), so a bloom can attest the
//! policy its members were admitted under rather than inheriting whatever text
//! was on the coordinator's disk. What stays here is the one thing that value
//! type cannot do: reach a file.
//!
//! The file remains the **fallback** a bloom that seals no policy entry resolves
//! to, which is what keeps a coordinator that has authored none working
//! unchanged. Either failure below is a gate failure, never a silent tier.

use std::fs;
use std::io;
use std::path::Path;

pub use aether_bloomery::{ApprovalPolicy, Tier};

/// Why a policy artifact could not become a usable [`ApprovalPolicy`]. Either
/// case is a **gate failure**, never a silent tier.
#[derive(Debug)]
pub enum PolicyError {
    /// The policy file could not be read.
    Unreadable(io::Error),
    /// The file was read but is not a well-formed policy (fail-closed parse).
    Malformed,
}

/// Read and parse the fallback tier policy from a repository path.
///
/// # Errors
/// [`PolicyError::Unreadable`] if the file cannot be read, or
/// [`PolicyError::Malformed`] if its contents are not a well-formed policy.
pub fn load_policy(path: &Path) -> Result<ApprovalPolicy, PolicyError> {
    ApprovalPolicy::parse(&fs::read_to_string(path).map_err(PolicyError::Unreadable)?).ok_or(PolicyError::Malformed)
}
