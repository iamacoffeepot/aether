//! aether-bloomery-git: the adapter-neutral git vocabulary (ADR-0199 slice 1).
//!
//! Repository machinery that is not GitHub-specific — the git-data trait and
//! its ref/commit operations, [`GitSource`], [`GitObjectId`], [`MainlineRef`],
//! the source error types, and the in-process [`fixture`] repository they run
//! against — lives here so a fleet-local source authority does not depend on a
//! crate named `github`. The GitHub REST/projection adapter
//! (`aether-bloomery-github`) depends *inward* on this crate and re-exports the
//! shared vocabulary.
//!
//! The control core (`aether-bloomery`) stays adapter-neutral: it does not
//! depend on this crate (ADR-0149 §The boundary).

#![forbid(unsafe_code)]

use aether_bloomery::{Digest, encode_hex};

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
    encode_hex(&digest.as_bytes()[..6])
}

// The workflow-dispatch input key the in-process fixture reads. Kept as a
// module so `fixture.rs` can keep `use crate::executor::INPUT_NONCE` after the
// extraction; the GitHub executor still owns the public constant.
mod executor {
    pub const INPUT_NONCE: &str = "nonce";
}

pub mod client;
pub mod command;
pub mod correspondence;
pub mod fixture;
pub mod local;
pub mod mainline;
pub mod marker;
pub mod replica;
pub mod roll;
pub mod source;

pub use client::{
    ActionsApi, Artifact, ChecksState, Comment, CommissionProjectionApi, GitCommit, GitDataApi, GitDataError, GitRef,
    GithubApi, GithubError, IssueStateApi, MergeResult, NewComment, NewIssue, NewPullRequest, ProjectedIssue,
    PullMergeResult, PullRequest, PullRequestApi, PullRequestState, RefTxnOp, RunConclusion, RunStatus, WorkflowRun,
    strip_heads,
};
pub use correspondence::{GitObjectFormat, GitObjectId};
pub use local::LocalGitData;
pub use mainline::MainlineRef;
pub use marker::{Marker, check_run_external_id, parse_check_run_external_id, parse_marker, render_marker};
pub use replica::{GitSourceReplica, PublishedRefspec, ReplicaError, SourceReplica, published_refspecs};
pub use roll::{DayCoverage, RollError, advance_main, cut_daily};
pub use source::{
    GitSource, HostSource, SourceError, candidate_ref_name, construction_checkpoint_namespace,
    construction_checkpoint_promotion, construction_checkpoint_ref_name, construction_checkpoint_scope_namespace,
    landing_branch, member_checkpoint_ref_name, partial_head_repair_namespace, partial_head_repair_ref_name, to_hex,
    transient_construction_checkpoint_ref_name, transient_construction_checkpoint_ref_prefix,
};
