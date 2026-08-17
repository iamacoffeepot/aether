//! aether-bloomery-git: the fleet-local git vocabulary (ADR-0199 slice 1).
//!
//! Neutral repository machinery extracted from `aether-bloomery-github` so a
//! first-party source authority can depend on git without depending on a crate
//! named github: the [`GitDataApi`] trait and its ref/commit operations,
//! [`GitSource`], [`GitObjectId`], [`MainlineRef`], the source error types, and
//! the source test suite.
//!
//! The GitHub REST client, projection mirror, and App authentication stay in
//! `aether-bloomery-github`, which depends *inward* on this crate. Neither
//! adapter is a dependency of [`aether_bloomery`] — the control core stays
//! adapter-neutral (ADR-0149).

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

mod client;
mod correspondence;
mod executor;
mod mainline;
mod marker;
mod source;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use client::{
    ActionsApi, Artifact, CheckConclusion, CheckRun, ChecksState, Comment, GitCommit, GitDataApi, GitRef, GithubApi,
    GithubError, IssueStateApi, MergeResult, NewCheckRun, NewComment, NewPullRequest, PullMergeResult, PullRequest,
    PullRequestApi, PullRequestState, RunConclusion, RunStatus, WorkflowRun, name_carries_nonce, strip_heads,
};
pub use correspondence::{GitObjectFormat, GitObjectId};
pub use executor::{INPUT_COMMAND, INPUT_DISPLAYED, INPUT_EFFORT, INPUT_MODEL, INPUT_NONCE, INPUT_SUBJECT};
pub use mainline::MainlineRef;
pub use marker::{Marker, check_run_external_id, parse_check_run_external_id, parse_marker, render_marker};
pub use source::{
    GitSource, LandAcceptance, LandingProposal, LandingRefusal, LandingSource, SourceError, candidate_ref_name,
    landing_branch, landing_floor_title, member_checkpoint_ref_name, to_hex,
};
