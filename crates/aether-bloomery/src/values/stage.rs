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
use crate::values::{AgentProfile, Budget, ReasoningEffort, ToolPolicy};

/// The declared output name every dispatched attempt uploads its result record
/// under — the study/verdict envelope the intake broker binds to the displayed
/// digest (ADR-0149 §The line). The construct lane's original constant; hoisted
/// here so every per-member stage's [`Transformation`] declares the same output.
pub const RESULT_RECORD_OUTPUT: &str = "result_record";

/// One stage's declared contract (ADR-0149 §The line).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct StageBinding {
    /// The stage this binding declares.
    pub stage: StageId,
    /// The artifact-kind tags this stage consumes.
    pub consumes: Vec<String>,
    /// The artifact-kind tags this stage produces.
    pub produces: Vec<String>,
    /// The calibrated [`AgentProfile`] this stage runs under, referenced by
    /// digest — *how* it runs (model, reasoning effort, tool policy). *Who*
    /// runs is not stored: the `iama-{stage}` worker identity is derived from
    /// [`stage`](Self::stage) via [`StageId::worker_identity`].
    pub profile: Digest,
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
    /// invariant is one binding per stage, and it holds by construction: the
    /// catalog maps `binding_of` over the generated [`StageId::ALL`], which is
    /// complete and duplicate-free by construction, and `binding_of`'s
    /// exhaustive match forces a binding for every variant — so a thirteenth
    /// stage enters the catalog automatically and is a compile error until its
    /// binding is authored.
    #[must_use]
    pub fn line() -> Self {
        Self { bindings: StageId::ALL.iter().copied().map(Self::binding_of).collect() }
    }

    /// The [`line`](Self::line) catalog's digest — the only stage-catalog digest
    /// a v1 bloom may seal. Recomputes the twelve small bindings (cheap and
    /// `no_std`-clean, no lazy static).
    #[must_use]
    pub fn line_digest() -> Digest {
        Self::line().digest()
    }

    /// The per-member stage line a sealed bloom's members walk (ADR-0149 §The
    /// line): the dispatched sub-sequence `Construct → Verify → Refine → Review`.
    /// Its head [`entry_stage`](Self::entry_stage) is the stage a member enters at
    /// seal; passing its terminal `Review` produces the member's
    /// [`ResolutionClaim`](crate::values::ResolutionClaim), which folds into the
    /// existing integrate path rather than dispatching a further attempt. The
    /// bloom-level tail (`Integrate` / `AggregateVerify` / `AggregateReview` /
    /// `Land` / `Study`) is the coarse lifecycle the reducer already owns — never
    /// a dispatched per-member stage.
    pub const MEMBER_LINE: &'static [StageId] =
        &[StageId::Construct, StageId::Verify, StageId::Refine, StageId::Review];

    /// The stage a sealed bloom's members enter the line at — the head of
    /// [`MEMBER_LINE`](Self::MEMBER_LINE), `Construct`.
    #[must_use]
    pub const fn entry_stage() -> StageId {
        StageId::Construct
    }

    /// The stage a member advances to after `stage`'s completion gate passes, or
    /// `None` at the per-member terminus (`Review`) — a passing `Review` integrates
    /// the member instead of dispatching a successor. `None` for any stage outside
    /// [`MEMBER_LINE`](Self::MEMBER_LINE): the bloom-level tail is not a dispatched
    /// per-member progression.
    #[must_use]
    pub fn next_member_stage(stage: StageId) -> Option<StageId> {
        let index = Self::MEMBER_LINE.iter().position(|member_stage| *member_stage == stage)?;
        Self::MEMBER_LINE.get(index + 1).copied()
    }

    /// The retry budget of a stage's binding in the line catalog — the attempt
    /// allowance the reducer's completion gate caps re-dispatch at (ADR-0149 §The
    /// line). `None` for a stage the catalog does not bind (unreachable for the
    /// closed [`StageId`] set, but total by construction).
    #[must_use]
    pub fn retry_budget_of(stage: StageId) -> Option<u32> {
        Self::line().bindings.into_iter().find(|binding| binding.stage == stage).map(|binding| binding.retry_budget)
    }

    /// The authored binding for one stage. An exhaustive `match` over the closed
    /// [`StageId`] enum — the compile-time guard that every stage has exactly one
    /// binding (ADR-0149 §The line). The binding references the stage's
    /// calibrated [`AgentProfile`] by digest via [`profile_of`](Self::profile_of).
    fn binding_of(stage: StageId) -> StageBinding {
        let (consumes, produces, process, completion_gate, retry_budget): (&[&str], &[&str], &str, &str, u32) =
            match stage {
                StageId::Sketch => (&["bloom.intent"], &["bloom.sketch"], "sketch", "issue-well-formed", 1),
                // Scope is a pre-seal operator-harness process (ADR-0149 §The
                // line, ADR-0150): the operator's own developer-side Bloomery
                // session authors the scope revision and stages it through the
                // REST control API (`aether.bloomery.api`'s `POST /workpieces`,
                // `PATCH /drafts/{id}`, `POST /drafts/{id}/seal`) — never a
                // dispatched worker lane. `process` names that api cap's
                // `NAMESPACE`, not a skill slug.
                StageId::Scope => (&["bloom.sketch"], &["bloom.scope"], "aether.bloomery.api", "plan-present", 1),
                // Approve is a pre-seal host-side admission gate (ADR-0149 §The
                // line, ADR-0151): the coordinator's own host resolves the
                // workpiece's declared surface to an approval tier and forms the
                // membership `approval` before `Fact::Seal`, never a dispatched
                // worker lane (the member-line dispatch loop never reaches this
                // pre-seal stage). `process` names that host gate, not the retired
                // `.claude/skills/approve` skill slug.
                StageId::Approve => {
                    (&["bloom.scope"], &["bloom.ready"], "aether.bloomery.approve_gate", "phase-ready", 1)
                }
                StageId::Construct => (&["bloom.ready"], &["bloom.candidate"], "implement", "pr-open", 2),
                StageId::Verify => {
                    (&["bloom.candidate"], &["bloom.verify_evidence"], "transform.verify", "ci-green", 3)
                }
                StageId::Refine => (&["bloom.verify_evidence"], &["bloom.candidate"], "implement", "ci-green", 3),
                StageId::Review => (&["bloom.candidate"], &["bloom.review_rollup"], "review", "review-approved", 2),
                StageId::Integrate => {
                    (&["bloom.candidate"], &["bloom.integration"], "integrate", "integration-checkpoint", 2)
                }
                StageId::AggregateVerify => {
                    (&["bloom.integration"], &["bloom.aggregate_verify"], "aggregate-verify", "aggregate-ci-green", 2)
                }
                StageId::AggregateReview => (
                    &["bloom.integration"],
                    &["bloom.aggregate_review"],
                    "aggregate-review",
                    "aggregate-review-approved",
                    2,
                ),
                StageId::Land => {
                    (&["bloom.aggregate_verify", "bloom.aggregate_review"], &["bloom.receipt"], "land", "landed", 1)
                }
                StageId::Study => (&["bloom.receipt"], &["bloom.study"], "retrospect", "study-recorded", 1),
            };
        StageBinding {
            stage,
            consumes: consumes.iter().map(|tag| String::from(*tag)).collect(),
            produces: produces.iter().map(|tag| String::from(*tag)).collect(),
            profile: Self::profile_of(stage).digest(),
            process: String::from(process),
            completion_gate: String::from(completion_gate),
            retry_budget,
        }
    }

    /// The calibrated [`AgentProfile`] one stage runs under (ADR-0149 §The line):
    /// its fixed model + reasoning effort + tool policy, calibrated once — the
    /// `scope=opus`, `review`=`sonnet@high` precedent, most stages a pinned
    /// model. An exhaustive `match` over the closed [`StageId`] enum, so a new
    /// stage must be calibrated before it compiles; the catalog's `binding_of`
    /// references the returned profile by [`digest`](AgentProfile::digest), so a
    /// recalibration is a new digest and re-digests the catalog.
    ///
    /// The model/effort values are the initial calibration — refinable without
    /// an ADR (a change re-digests the catalog), like the per-binding tag/gate
    /// strings. `tools` is [`ToolPolicy::Full`] across the v1 line: every stage
    /// runs a real process over the full tool surface; the finer tiers exist so
    /// a later calibration can bound a stage without a vocabulary change.
    #[must_use]
    pub fn profile_of(stage: StageId) -> AgentProfile {
        // Calibrated once, grouped by tier: the design-adjacent stages run opus
        // (scope/construct/study at high effort, refine at medium), review's
        // finders run sonnet@high, and the mechanical remainder runs sonnet@medium.
        let (model, effort): (&str, ReasoningEffort) = match stage {
            StageId::Scope | StageId::Construct | StageId::Study => ("claude-opus-4-8", ReasoningEffort::High),
            StageId::Refine => ("claude-opus-4-8", ReasoningEffort::Medium),
            StageId::Review | StageId::AggregateReview => ("claude-sonnet-5", ReasoningEffort::High),
            StageId::Sketch
            | StageId::Approve
            | StageId::Verify
            | StageId::Integrate
            | StageId::AggregateVerify
            | StageId::Land => ("claude-sonnet-5", ReasoningEffort::Medium),
        };
        AgentProfile { model: String::from(model), effort, tools: ToolPolicy::Full }
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

impl Transformation {
    /// The portable transformation one per-member stage dispatches against
    /// `subject` — the frozen scope-revision digest the attempt pins as its input
    /// (ADR-0149 §The line). The reducer builds this from the sealed catalog when
    /// it decides to dispatch; the model-driven lanes re-read the pinned revision
    /// and re-resolve the effective model identically, so the dispatched model is a
    /// function of the frozen revision, not a dispatch-time choice.
    ///
    /// The per-stage lane details (typed command, execution image, network
    /// posture) are the initial calibration — refinable without an ADR, like the
    /// catalog's tag/gate strings. Security splits by lane (ADR-0149 §Execution on
    /// Actions): the mechanical `Verify` lane runs zero-egress
    /// ([`NetworkProfile::None`]); the model-driven `Construct` / `Refine` /
    /// `Review` lanes reach the model API under a restricted egress allowlist,
    /// never full network. A stage outside [`StageCatalog::MEMBER_LINE`] is not a
    /// dispatched per-member stage and has no lane here.
    #[must_use]
    pub fn for_member_stage(stage: StageId, subject: Digest) -> Self {
        let (command, image, network): (&str, &str, NetworkProfile) = match stage {
            // The mechanical Verify lane runs zero-egress; every model-driven lane
            // (Construct / Refine, and the non-member stages that fall through to
            // the construct lane here) reaches the model API under restricted
            // egress. Review is its own model lane.
            StageId::Verify => ("verify.check", "iama/verify:1", NetworkProfile::None),
            StageId::Review => ("review.critic", "iama/review-claude:1", NetworkProfile::Restricted),
            StageId::Scope => unreachable!(
                "Scope is a pre-seal operator-harness process staged via the REST control API, never a dispatched member transformation"
            ),
            StageId::Construct
            | StageId::Refine
            | StageId::Sketch
            | StageId::Approve
            | StageId::Integrate
            | StageId::AggregateVerify
            | StageId::AggregateReview
            | StageId::Land
            | StageId::Study => ("construct.implement", "iama/construct-claude:1", NetworkProfile::Restricted),
        };
        Self {
            command: String::from(command),
            inputs: alloc::vec![subject],
            outputs: alloc::vec![String::from(RESULT_RECORD_OUTPUT)],
            image: String::from(image),
            limits: Budget::default(),
            network,
        }
    }
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

    // The line has exactly one binding per stage, in `StageId::ALL` order. A
    // stage dropped or reordered in `line()` breaks this; `StageId::ALL` is
    // generated with the enum, so it cannot drop a variant, and a thirteenth
    // `StageId` is already a compile error in `binding_of`'s exhaustive match.
    #[test]
    fn line_binds_every_stage_exactly_once() {
        let catalog = StageCatalog::line();
        assert_eq!(catalog.bindings.len(), StageId::ALL.len());
        let bound: Vec<StageId> = catalog.bindings.iter().map(|binding| binding.stage).collect();
        assert_eq!(bound, StageId::ALL.to_vec());
    }

    // Every stage derives an attempt-scoped `iama-{stage}` worker identity
    // (ADR-0149 §The line) — never a resident actor. The identity is who runs,
    // derived from the stage; it is no longer stored on the binding, which now
    // references how it runs by AgentProfile digest.
    #[test]
    fn every_stage_derives_an_iama_worker_identity() {
        for stage in StageId::ALL {
            let identity = stage.worker_identity();
            assert!(identity.starts_with("iama-"), "worker identity {identity} is not iama-scoped");
        }
    }

    // Tripwire: every catalog binding references its own stage's calibrated
    // profile by digest. `profile_of` is exhaustive, so a new stage must be
    // calibrated to compile; this catches a binding wired to the wrong stage's
    // profile, or the digest reference drifting from the calibration.
    #[test]
    fn every_binding_references_its_calibrated_profile() {
        for binding in StageCatalog::line().bindings {
            assert_eq!(
                binding.profile,
                StageCatalog::profile_of(binding.stage).digest(),
                "binding for {:?} does not reference its calibrated profile digest",
                binding.stage
            );
        }
    }

    // Tripwire: the line catalog's digest. Computed over the authored bindings,
    // so it drifts the moment any consumes/produces/profile/process/gate/retry
    // value changes — catching an unintended catalog edit. Recompute-and-repin
    // only when a change *intends* to alter the authored line.
    const GOLDEN_LINE_DIGEST: [u8; 32] = [
        0x29, 0xd7, 0x27, 0xb4, 0xe3, 0x22, 0x6d, 0xb4, 0x93, 0xe4, 0x2d, 0x68, 0x0e, 0xf4, 0xcc, 0xb6, 0x68, 0x60,
        0xce, 0x61, 0x59, 0xa4, 0x97, 0x6a, 0xb6, 0x76, 0x3e, 0x29, 0x9d, 0x61, 0x38, 0x0e,
    ];

    #[test]
    fn line_digest_matches_pinned_golden() {
        assert_eq!(
            *StageCatalog::line_digest().as_bytes(),
            GOLDEN_LINE_DIGEST,
            "authored stage catalog drifted from the pinned golden digest"
        );
    }

    // ADR-0149 §The line — the per-member line is the linear sub-sequence
    // Construct → Verify → Refine → Review, entered at Construct and terminating
    // (no successor) at Review. Tripwire: a reordered or dropped member-line stage
    // breaks the dispatched progression the reducer walks.
    #[test]
    fn member_line_is_construct_verify_refine_review() {
        assert_eq!(StageCatalog::entry_stage(), StageId::Construct);
        assert_eq!(StageCatalog::next_member_stage(StageId::Construct), Some(StageId::Verify));
        assert_eq!(StageCatalog::next_member_stage(StageId::Verify), Some(StageId::Refine));
        assert_eq!(StageCatalog::next_member_stage(StageId::Refine), Some(StageId::Review));
        assert_eq!(StageCatalog::next_member_stage(StageId::Review), None, "Review is the per-member terminus");
        // A bloom-level tail stage is not part of the dispatched per-member line.
        assert_eq!(StageCatalog::next_member_stage(StageId::Integrate), None);
    }

    // Scope is a pre-seal operator-harness process, never a dispatched member
    // transformation — `for_member_stage` must not build a lane for it.
    #[test]
    #[should_panic(expected = "operator-harness process")]
    fn for_member_stage_panics_on_scope() {
        let subject = Digest::from_bytes([7; 32]);
        let _ = Transformation::for_member_stage(StageId::Scope, subject);
    }

    // The per-member dispatch transformation pins the given subject as its single
    // input and declares the shared result-record output; the mechanical Verify
    // lane runs zero-egress while the model lanes run under restricted egress
    // (ADR-0149 §Execution on Actions).
    #[test]
    fn member_stage_transformation_pins_subject_and_splits_egress_by_lane() {
        let subject = Digest::from_bytes([7; 32]);
        let construct = Transformation::for_member_stage(StageId::Construct, subject);
        assert_eq!(construct.inputs, alloc::vec![subject]);
        assert_eq!(construct.outputs, alloc::vec![String::from(RESULT_RECORD_OUTPUT)]);
        assert_eq!(construct.network, NetworkProfile::Restricted);
        assert_eq!(Transformation::for_member_stage(StageId::Verify, subject).network, NetworkProfile::None);
        assert_eq!(Transformation::for_member_stage(StageId::Review, subject).network, NetworkProfile::Restricted);
    }
}
