//! The line: stage bindings, the catalog, attempts, and transformations
//! (ADR-0149 §The line).
//!
//! The pipeline is a closed stage vocabulary compiled into Rust, not a
//! workflow language. A [`StageBinding`] declares what one stage consumes
//! and produces, the profile that runs it, its process, its completion gate,
//! and its retry budget. The full [`StageCatalog`] is itself a digest the
//! bloom freezes at seal.

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::StageId;
use crate::values::Budget;

/// One stage's declared contract (ADR-0149 §The line).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct StageBinding {
    /// The stage this binding declares.
    pub stage: StageId,
    /// The artifact-kind tags this stage consumes.
    pub consumes: Vec<String>,
    /// The artifact-kind tags this stage produces.
    pub produces: Vec<String>,
    /// The attempt-scoped worker identity that runs it (`iama-{stage}`) —
    /// never a resident actor or a delegable authority.
    pub profile: String,
    /// The skill or process the stage executes.
    pub process: String,
    /// The completion gate that decides the stage is done.
    pub completion_gate: String,
    /// The stage's retry budget.
    pub retry_budget: u32,
}

/// The closed set of stage bindings the line runs. Frozen as a digest the
/// bloom seals (ADR-0149 §The line) so an executed bloom is graded against
/// the exact catalog it promised.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct StageCatalog {
    /// The bindings, one per stage in the catalog.
    pub bindings: Vec<StageBinding>,
}

impl ContentAddressed for StageCatalog {
    const DOMAIN: &'static str = "aether.bloomery.stage_catalog";
}

impl StageCatalog {
    /// Every stage of the line, in execution order. The one place the closed
    /// vocabulary's order is written; [`line`](Self::line) maps it to bindings.
    const STAGES: [StageId; 12] = [
        StageId::Sketch,
        StageId::Scope,
        StageId::Approve,
        StageId::Construct,
        StageId::Verify,
        StageId::Refine,
        StageId::Review,
        StageId::Integrate,
        StageId::AggregateVerify,
        StageId::AggregateReview,
        StageId::Land,
        StageId::Study,
    ];

    /// The catalog's content-addressed digest — the value a bloom freezes at
    /// seal.
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }

    /// The one concrete catalog the line runs (ADR-0149 §The line): one
    /// [`StageBinding`] per [`StageId`], authored in Rust. A bloom freezes this
    /// catalog's [`digest`](Self::digest) at seal and is graded against the
    /// exact line it promised.
    ///
    /// The per-binding tag/gate strings are the initial vocabulary — refinable
    /// without an ADR (a change re-digests the catalog); the load-bearing
    /// invariants are one binding per stage and the exhaustive `binding_of`
    /// match, so a thirteenth stage is a compile error until its binding is
    /// authored.
    #[must_use]
    pub fn line() -> Self {
        Self { bindings: Self::STAGES.into_iter().map(Self::binding_of).collect() }
    }

    /// The [`line`](Self::line) catalog's digest — the only stage-catalog digest
    /// a v1 bloom may seal. Recomputes the twelve small bindings (cheap and
    /// `no_std`-clean, no lazy static).
    #[must_use]
    pub fn line_digest() -> Digest {
        Self::line().digest()
    }

    /// The authored binding for one stage. An exhaustive `match` over the closed
    /// [`StageId`] enum — the compile-time guard that every stage has exactly one
    /// binding (ADR-0149 §The line).
    fn binding_of(stage: StageId) -> StageBinding {
        let (consumes, produces, profile, process, completion_gate, retry_budget): (
            &[&str],
            &[&str],
            &str,
            &str,
            &str,
            u32,
        ) = match stage {
            StageId::Sketch => (&["bloom.intent"], &["bloom.sketch"], "iama-sketch", "sketch", "issue-well-formed", 1),
            StageId::Scope => (&["bloom.sketch"], &["bloom.scope"], "iama-scope", "scope", "plan-present", 1),
            StageId::Approve => (&["bloom.scope"], &["bloom.ready"], "iama-approve", "approve", "phase-ready", 1),
            StageId::Construct => (&["bloom.ready"], &["bloom.candidate"], "iama-construct", "implement", "pr-open", 2),
            StageId::Verify => {
                (&["bloom.candidate"], &["bloom.verify_evidence"], "iama-verify", "transform.verify", "ci-green", 3)
            }
            StageId::Refine => {
                (&["bloom.verify_evidence"], &["bloom.candidate"], "iama-refine", "implement", "ci-green", 3)
            }
            StageId::Review => {
                (&["bloom.candidate"], &["bloom.review_rollup"], "iama-review", "review", "review-approved", 2)
            }
            StageId::Integrate => (
                &["bloom.candidate"],
                &["bloom.integration"],
                "iama-integrate",
                "integrate",
                "integration-checkpoint",
                2,
            ),
            StageId::AggregateVerify => (
                &["bloom.integration"],
                &["bloom.aggregate_verify"],
                "iama-aggregate-verify",
                "aggregate-verify",
                "aggregate-ci-green",
                2,
            ),
            StageId::AggregateReview => (
                &["bloom.integration"],
                &["bloom.aggregate_review"],
                "iama-aggregate-review",
                "aggregate-review",
                "aggregate-review-approved",
                2,
            ),
            StageId::Land => (
                &["bloom.aggregate_verify", "bloom.aggregate_review"],
                &["bloom.receipt"],
                "iama-land",
                "land",
                "landed",
                1,
            ),
            StageId::Study => (&["bloom.receipt"], &["bloom.study"], "iama-study", "retrospect", "study-recorded", 1),
        };
        StageBinding {
            stage,
            consumes: consumes.iter().map(|tag| String::from(*tag)).collect(),
            produces: produces.iter().map(|tag| String::from(*tag)).collect(),
            profile: String::from(profile),
            process: String::from(process),
            completion_gate: String::from(completion_gate),
            retry_budget,
        }
    }
}

/// One execution of one binding against one subject (ADR-0149 §The line).
/// Agents return proposed artifacts and evidence only — the reducer alone
/// advances state.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Attempt {
    /// The binding this attempt executed.
    pub binding: StageId,
    /// The subject digest the attempt ran against.
    pub subject: Digest,
    /// The digests of the artifacts and evidence the attempt proposed.
    pub produced: Vec<Digest>,
}

/// The portable unit of execution: a typed command with declared inputs,
/// outputs, image, limits, and network profile — invoked identically on a
/// laptop, on Actions, or in an isolated worker (ADR-0149 §The line). There
/// is no arbitrary-command shape.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Transformation {
    /// The typed command name (e.g. `verify.clippy`, `construct.implement`).
    pub command: String,
    /// The digest-pinned inputs.
    pub inputs: Vec<Digest>,
    /// The declared output names the broker accepts.
    pub outputs: Vec<String>,
    /// The execution image.
    pub image: String,
    /// The resource limits.
    pub limits: Budget,
    /// The network profile the lane permits.
    pub network: NetworkProfile,
}

/// The network posture a transformation runs under. Untrusted lanes run with
/// no egress (ADR-0149 §Execution on Actions).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum NetworkProfile {
    /// No network at all.
    None,
    /// A restricted egress allowlist.
    Restricted,
    /// Full network — trusted lanes only.
    Full,
}

#[cfg(test)]
mod tests {
    use super::*;

    // The line has exactly one binding per stage, in `STAGES` order. A stage
    // dropped or duplicated in `line()` breaks this; a thirteenth `StageId` is
    // already a compile error in `binding_of`'s exhaustive match.
    #[test]
    fn line_binds_every_stage_exactly_once() {
        let catalog = StageCatalog::line();
        assert_eq!(catalog.bindings.len(), StageCatalog::STAGES.len());
        let bound: Vec<StageId> = catalog.bindings.iter().map(|binding| binding.stage).collect();
        assert_eq!(bound, StageCatalog::STAGES.to_vec());
    }

    // Every binding runs under the attempt-scoped `iama-{stage}` profile
    // (ADR-0149 §The line) — never a resident actor.
    #[test]
    fn every_binding_runs_an_iama_profile() {
        for binding in StageCatalog::line().bindings {
            assert!(binding.profile.starts_with("iama-"), "profile {} is not iama-scoped", binding.profile);
        }
    }

    // Tripwire: the line catalog's digest. Computed over the authored bindings,
    // so it drifts the moment any consumes/produces/profile/process/gate/retry
    // value changes — catching an unintended catalog edit. Recompute-and-repin
    // only when a change *intends* to alter the authored line.
    const GOLDEN_LINE_DIGEST: [u8; 32] = [
        0xad, 0xdd, 0x32, 0xc7, 0x73, 0x66, 0x54, 0x03, 0x3e, 0xcd, 0x8d, 0x0e, 0x2e, 0x3f, 0x35, 0x9a, 0x07, 0xe0,
        0x28, 0x95, 0xef, 0x4c, 0x71, 0xe6, 0x9c, 0x10, 0x59, 0xdd, 0x17, 0x30, 0x1a, 0x32,
    ];

    #[test]
    fn line_digest_matches_pinned_golden() {
        assert_eq!(
            *StageCatalog::line_digest().as_bytes(),
            GOLDEN_LINE_DIGEST,
            "authored stage catalog drifted from the pinned golden digest"
        );
    }
}
