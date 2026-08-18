//! aether-bloomery-git: the adapter-neutral git vocabulary (ADR-0199 slice 1).
//!
//! Repository machinery that is not GitHub-specific — the git-data trait and
//! its ref/commit operations, [`GitSource`], [`GitObjectId`], [`MainlineRef`],
//! the source error types, and the in-process fake that exercises them — lives
//! here so a fleet-local source authority does not depend on a crate named
//! `github`. The GitHub REST/projection adapter (`aether-bloomery-github`)
//! depends *inward* on this crate and re-exports the shared vocabulary.
//!
//! The control core (`aether-bloomery`) stays adapter-neutral: it does not
//! depend on this crate (ADR-0149 §The boundary).

use aether_bloomery::Digest;

/// A digest's first six bytes as hex — the short form every human-facing
/// surface names a bloom by: projected comment bodies, the branch namespace,
/// and a landing proposal's subject.
///
/// Twelve hex characters is git's own short-sha convention and reads at a
/// glance where sixty-four does not. It is a *name*, never an identity: the
/// authoritative full digest rides the projection body and the sealed spec, so
/// a reader who needs to verify has it. Collision would need on the order of
/// 2^24 blooms against one mainline before it were likelier than not, and the
/// namespace is addressed by construction rather than parsed back.
#[must_use]
pub fn short_hex(digest: &Digest) -> String {
    let bytes = digest.as_bytes();
    let mut out = String::with_capacity(12);
    for byte in &bytes[..6] {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

// The workflow-dispatch input key the in-process fake reads. Kept as a module
// so the moved `testing.rs` can keep `use crate::executor::INPUT_NONCE` after
// the extraction; the GitHub executor still owns the public constant.
mod executor {
    pub const INPUT_NONCE: &str = "nonce";
}

pub mod client;
pub mod correspondence;
pub mod mainline;
pub mod marker;
pub mod source;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use client::{
    ActionsApi, Artifact, ChecksState, Comment, GitCommit, GitDataApi, GitDataError, GitRef, GithubApi, GithubError,
    IssueStateApi, MergeResult, NewComment, NewPullRequest, PullMergeResult, PullRequest, PullRequestApi,
    PullRequestState, RunConclusion, RunStatus, WorkflowRun, strip_heads,
};
pub use correspondence::{GitObjectFormat, GitObjectId};
pub use mainline::MainlineRef;
pub use marker::{Marker, check_run_external_id, parse_check_run_external_id, parse_marker, render_marker};
pub use source::{
    GitSource, HostSource, SourceError, candidate_ref_name, landing_branch, member_checkpoint_ref_name, to_hex,
};
