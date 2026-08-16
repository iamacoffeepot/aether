//! Content-derived addresses for a captured candidate's tree and checkout.
//!
//! The executor mints these after a construct lap; the operator repair surface
//! mints the same pair from a commit the coordinator can already see. Both go
//! through [`digest_of`] under the same domain tags, so a digest produced on
//! either path resolves the same backend object.

use aether_bloomery::digest::ContentAddressed;
use aether_bloomery::{BackendObjectId, Digest, digest_of};
use serde::Serialize;

/// The content-derived digest of a captured candidate tree: a domain-tagged
/// address over the backend tree object's raw bytes, so the digest changes
/// exactly when the captured content does — ADR-0152's supersession property
/// falls out of the identity choice.
#[derive(Serialize)]
struct CandidateTreeAddress<'a> {
    object: &'a [u8],
}

impl ContentAddressed for CandidateTreeAddress<'_> {
    const DOMAIN: &'static str = "aether.bloomery.candidate.tree";
}

/// Digest of a captured candidate tree. Shared by the executor capture path and
/// the operator repair surface so the two cannot drift onto different tags.
#[must_use]
pub fn candidate_tree_digest(tree: &BackendObjectId) -> Digest {
    digest_of(&CandidateTreeAddress { object: tree.as_bytes() })
}

/// The content-derived digest of a capture commit — the
/// [`aether_bloomery::CandidateRef::checkout`] axis, distinct from the tree's
/// by domain tag so the two never collide even over equal object bytes.
#[derive(Serialize)]
struct CaptureCommitAddress<'a> {
    object: &'a [u8],
}

impl ContentAddressed for CaptureCommitAddress<'_> {
    const DOMAIN: &'static str = "aether.bloomery.candidate.checkout";
}

/// Digest of a capture commit. Paired with [`candidate_tree_digest`].
#[must_use]
pub fn capture_commit_digest(commit: &BackendObjectId) -> Digest {
    digest_of(&CaptureCommitAddress { object: commit.as_bytes() })
}
