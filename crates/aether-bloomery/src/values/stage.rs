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
use crate::values::{AgentProfile, Budget, Harness, ReasoningEffort, ResolvedModel, ToolPolicy};

/// The declared output name every dispatched attempt uploads its result record
/// under — the study/verdict envelope the intake broker binds to the displayed
/// digest (ADR-0149 §The line). The construct lane's original constant; hoisted
/// here so every per-member stage's [`Transformation`] declares the same output.
pub const RESULT_RECORD_OUTPUT: &str = "result_record";

/// The construct/refine lane's typed command — the one spelling shared by the
/// catalog binding's `process`, the dispatched [`Transformation::command`], and
/// the host executors that route on it (#3668). A drifted copy would dispatch
/// a lane no executor recognizes.
pub const CONSTRUCT_IMPLEMENT_COMMAND: &str = "construct.implement";

/// The review lane's typed command — shared with the host local executor that
/// routes it onto the xtask critic, like
/// [`CONSTRUCT_IMPLEMENT_COMMAND`] (#3668).
pub const REVIEW_CRITIC_COMMAND: &str = "review.critic";

/// Whether a typed command names a **model lane** — a lane whose worker runs a
/// model and therefore needs a credential, a resolved model, and a reasoning
/// effort, as opposed to the mechanical lanes that run a compiler and nothing
/// else.
///
/// The one spelling of that question, over the sealed
/// [`Transformation::command`] rather than any host-side overlay or routing
/// knob: the command id is content the bloom sealed, so a mechanical lane can
/// never acquire model-lane treatment through a mis-set config or an unfilled
/// dispatch field. Every executor backend asks here — the local one to decide
/// which argv the child gets, the Actions one to decide which wrapper workflow
/// the dispatch fires, and only the model wrapper carries a credential
/// (ADR-0149 §Execution on Actions).
#[must_use]
pub fn is_model_lane(command: &str) -> bool {
    command == CONSTRUCT_IMPLEMENT_COMMAND || command == REVIEW_CRITIC_COMMAND
}

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
    /// line, ADR-0153): the dispatched sub-sequence `Construct → Verify`. Its
    /// head [`entry_stage`](Self::entry_stage) is the stage a member enters at
    /// seal; passing its terminal `Verify` produces the member's
    /// [`ResolutionClaim`](crate::values::ResolutionClaim) — the verification
    /// evidence binds the exact candidate tree — which folds into the existing
    /// integrate path rather than dispatching a further attempt. `Refine` is off
    /// the standing line: the repair re-entry, dispatched only when a failing
    /// `Verify` routes into it, its pass returning to `Verify` for the
    /// delta-confirm. `Review` binds no dispatched member stage (the model
    /// review runs once per bloom at `AggregateReview`, ADR-0153); it stays in
    /// [`StageId`] for wire stability. The bloom-level tail (`Integrate` /
    /// `AggregateVerify` / `AggregateReview` / `Land` / `Study`) is the coarse
    /// lifecycle the reducer already owns — never a dispatched per-member stage.
    pub const MEMBER_LINE: &'static [StageId] = &[StageId::Construct, StageId::Verify];

    /// The stage a sealed bloom's members enter the line at — the head of
    /// [`MEMBER_LINE`](Self::MEMBER_LINE), `Construct`.
    #[must_use]
    pub const fn entry_stage() -> StageId {
        StageId::Construct
    }

    /// The stage a member advances to after `stage`'s completion gate passes, or
    /// `None` at the per-member terminus (`Verify`) — a passing `Verify` integrates
    /// the member instead of dispatching a successor (ADR-0153). `None` for any
    /// stage outside [`MEMBER_LINE`](Self::MEMBER_LINE): the repair-only `Refine`
    /// routes back to `Verify` in the reducer, not through the line walk, and the
    /// bloom-level tail is not a dispatched per-member progression.
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
                StageId::Construct => {
                    (&["bloom.ready"], &["bloom.candidate"], CONSTRUCT_IMPLEMENT_COMMAND, "pr-open", 2)
                }
                StageId::Verify => {
                    (&["bloom.candidate"], &["bloom.verify_evidence"], "transform.verify", "ci-green", 3)
                }
                StageId::Refine => {
                    (&["bloom.verify_evidence"], &["bloom.candidate"], CONSTRUCT_IMPLEMENT_COMMAND, "ci-green", 3)
                }
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
                StageId::Land => (
                    &["bloom.aggregate_verify", "bloom.aggregate_review"],
                    &["bloom.receipt"],
                    "source.cas_land",
                    "landed",
                    1,
                ),
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
    /// The harness/model/effort values are the initial calibration — refinable
    /// without an ADR (a change re-digests the catalog), like the per-binding
    /// tag/gate strings. `tools` is [`ToolPolicy::Full`] across the v1 line:
    /// every stage runs a real process over the full tool surface; the finer
    /// tiers exist so a later calibration can bound a stage without a vocabulary
    /// change.
    /// The model id every opus-tier stage below resolves to. Named once so a
    /// generation refresh is one edit rather than a sweep over the arms — the
    /// rows carry a tier, and only this line carries an id that can age.
    const OPUS_MODEL: &'static str = "claude-opus-5";
    /// The model id every sonnet-tier stage below resolves to, named for the
    /// same reason as [`Self::OPUS_MODEL`].
    const SONNET_MODEL: &'static str = "claude-sonnet-5";

    #[must_use]
    pub fn profile_of(stage: StageId) -> AgentProfile {
        // Calibrated once, grouped by tier: the design-adjacent stages run opus
        // (scope/construct/study at high effort, refine at medium), review's
        // finders run sonnet@high, and the mechanical remainder runs sonnet@medium.
        //
        // Every stage names [`Harness::Claude`]: the harness axis exists so a
        // stage *can* be calibrated onto another CLI, and the arms that run one
        // land with the lanes that implement them. A mechanical stage's harness
        // is inert — it runs a compiler, and `is_model_lane` keeps the resolved
        // value off its argv entirely.
        let (model, effort): (&str, ReasoningEffort) = match stage {
            StageId::Scope | StageId::Construct | StageId::Study => (Self::OPUS_MODEL, ReasoningEffort::High),
            StageId::Refine => (Self::OPUS_MODEL, ReasoningEffort::Medium),
            StageId::Review | StageId::AggregateReview => (Self::SONNET_MODEL, ReasoningEffort::High),
            StageId::Sketch
            | StageId::Approve
            | StageId::Verify
            | StageId::Integrate
            | StageId::AggregateVerify
            | StageId::Land => (Self::SONNET_MODEL, ReasoningEffort::Medium),
        };
        AgentProfile { harness: Harness::Claude, model: String::from(model), effort, tools: ToolPolicy::Full }
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
    /// The exact git commit this attempt's worker checks out — the source the
    /// attempt runs on (ADR-0149 §Execution: "the wrapper checks out the exact
    /// digest a resolved work order names"). Resolved per stage by the reducer:
    /// the bloom's sealed base until the member captures a candidate, then the
    /// candidate's capture commit (ADR-0152, [`CandidateRef::checkout`]).
    ///
    /// Distinct from [`inputs`](Self::inputs): `inputs[0]` is the digest that
    /// *binds the returned evidence* — the member's scope revision, or its
    /// candidate tree once one exists ([`CandidateRef::tree`]) — an aether
    /// content address orthogonal to git, never a checkoutable object. This is the git commit the wrapper feeds
    /// `actions/checkout`; the executor renders it as the dispatch's checkout
    /// input while the `workflow_dispatch` itself stays pinned at the protected
    /// ref (the checkout target moves, the workflow definition does not).
    pub checkout: Digest,
    /// The declared output names the broker accepts.
    pub outputs: Vec<String>,
    /// The execution image.
    pub image: String,
    /// The resource limits.
    pub limits: Budget,
    /// The network profile the lane permits.
    pub network: NetworkProfile,
    /// The advisory, human-readable work-order description the construct lane
    /// names in its assembled prompt's `## Task` section (#3595). It is model
    /// *context*, not signed instruction: it binds no evidence and never enters
    /// the content-addressed `BloomSpec`/`Membership` vocabulary, so the reducer
    /// authors it `None` ([`for_member_stage`](Self::for_member_stage)) and the
    /// host populates it at dispatch from durable state keyed by the member — a
    /// missing description leaves it `None`, a legible subject-only run rather
    /// than a blind dispatch.
    #[serde(default)]
    pub description: Option<String>,
    /// The effective model + reasoning effort the attempt runs under — the
    /// stage's calibrated [`AgentProfile`] resolved through the member's sealed
    /// [`ModelOverride`](crate::values::ModelOverride) (ADR-0149 §The line).
    /// Carried on the same host-overlay channel as
    /// [`description`](Self::description): the reducer holds digests, not the
    /// catalog's resolution, so it authors this `None`
    /// ([`for_member_stage`](Self::for_member_stage)) and the host resolves it
    /// at dispatch. `None` leaves the executor backend with no model to name, so
    /// the lane falls back to the runner's ambient default — legible, but
    /// unattested, so a model lane always carries one.
    #[serde(default)]
    pub model: Option<ResolvedModel>,
}

impl Transformation {
    /// The portable transformation one per-member stage dispatches against
    /// `subject` — the frozen scope-revision digest the attempt pins as its input
    /// (ADR-0149 §The line). The reducer builds this from the sealed catalog when
    /// it decides to dispatch; the model-driven lanes re-read the pinned revision
    /// and re-resolve the effective model identically, so the dispatched model is a
    /// function of the frozen revision, not a dispatch-time choice.
    ///
    /// `checkout` is the git commit the attempt's worker checks out, which the
    /// reducer resolves per stage — the bloom's sealed base until the member has
    /// a captured candidate, then that candidate's capture commit (ADR-0152). It
    /// is a separate axis from `subject`: `subject` binds the evidence, `checkout`
    /// names the tree the work runs on. See [`checkout`](Self::checkout).
    ///
    /// The per-stage lane details (typed command, execution image, network
    /// posture) are the initial calibration — refinable without an ADR, like the
    /// catalog's tag/gate strings. Security splits by lane (ADR-0149 §Execution on
    /// Actions): the mechanical `Verify` lane runs zero-egress
    /// ([`NetworkProfile::None`]); the model-driven `Construct` / `Refine` lanes
    /// reach the model API under a restricted egress allowlist, never full
    /// network. The `Review` lane keeps its `review.critic` command for the
    /// bloom-level `AggregateReview` position that dispatches it (ADR-0153) — it
    /// is no longer a standing member stage.
    #[must_use]
    pub fn for_member_stage(stage: StageId, subject: Digest, checkout: Digest) -> Self {
        let (command, image, network): (&str, &str, NetworkProfile) = match stage {
            // The mechanical Verify lane runs zero-egress; every model-driven lane
            // (Construct / Refine, and the non-member stages that fall through to
            // the construct lane here) reaches the model API under restricted
            // egress. Review is its own model lane.
            StageId::Verify => ("verify.check", "iama/verify:1", NetworkProfile::None),
            StageId::Review => (REVIEW_CRITIC_COMMAND, "iama/review-claude:1", NetworkProfile::Restricted),
            StageId::Scope => unreachable!(
                "Scope is a pre-seal operator-harness process staged via the REST control API, never a dispatched member transformation"
            ),
            StageId::Land => unreachable!(
                "Land is a host-native source-port CAS (LandReactorCapability, #3559), never a dispatched member transformation"
            ),
            StageId::Construct
            | StageId::Refine
            | StageId::Sketch
            | StageId::Approve
            | StageId::Integrate
            | StageId::AggregateVerify
            | StageId::AggregateReview
            | StageId::Study => (CONSTRUCT_IMPLEMENT_COMMAND, "iama/construct-claude:1", NetworkProfile::Restricted),
        };
        Self {
            command: String::from(command),
            inputs: alloc::vec![subject],
            checkout,
            outputs: alloc::vec![String::from(RESULT_RECORD_OUTPUT)],
            image: String::from(image),
            limits: Budget::default(),
            network,
            // The reducer holds only digests; the operator's work-order text is
            // advisory context the host threads on at dispatch, never here. The
            // effective model rides the same channel for the same reason — the
            // reducer names the catalog by digest, the host resolves it.
            description: None,
            model: None,
        }
    }

    /// The whole-bloom aggregate-review transformation (ADR-0153): the
    /// `review.critic` lane dispatched once per bloom against the integrated
    /// head — `subject` is the integrated tree digest the returned evidence
    /// binds, `checkout` the landable head commit the critic checks out. The
    /// same lane shape as the member `Review` egress it replaces (restricted
    /// egress to the model API); the sealed intent the critic judges against
    /// rides the advisory description the host threads on at dispatch.
    #[must_use]
    pub fn for_aggregate_review(subject: Digest, checkout: Digest) -> Self {
        Self {
            command: String::from(REVIEW_CRITIC_COMMAND),
            inputs: alloc::vec![subject],
            checkout,
            outputs: alloc::vec![String::from(RESULT_RECORD_OUTPUT)],
            image: String::from("iama/review-claude:1"),
            limits: Budget::default(),
            network: NetworkProfile::Restricted,
            description: None,
            model: None,
        }
    }
}

/// A captured candidate — the source tree a model-lane attempt produced, as the
/// two correspondence-mapped digests ADR-0152 defines. `tree` is the identity of
/// the work: the digest evidence binds to, `ResolutionClaim.candidate` names, and
/// the source port integrates. `checkout` is the vehicle: the capture commit
/// wrapping that tree, which downstream stages check out exactly as they check
/// out the sealed base. The host captures both after a model-lane run and reports
/// them on the completion fact; the reducer stores the pair on the member's
/// cursor and re-targets later dispatches from it. Content-derived, so a Refine
/// that changes anything yields a new `tree` and prior evidence stops validating
/// (ADR-0149 §supersession).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CandidateRef {
    /// The produced git tree's digest — the candidate's identity.
    pub tree: Digest,
    /// The capture commit's digest — what a downstream stage checks out.
    pub checkout: Digest,
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
    // Repinned for #3572: the Construct and Refine bindings' `process` re-pointed
    // from the retired `implement` skill to the native `construct.implement`
    // transform lane — an intended catalog edit, so the golden is recomputed.
    // Repinned again when the Scope binding's `process` re-pointed to
    // `aether.bloomery.api` (#3570) — an intended catalog edit.
    // Repinned again for #3573: the Land binding's `process` re-pointed from the
    // retired `land` skill to the native `source.cas_land` lane — an intended
    // catalog edit.
    // Repinned again on the #3571→main merge: main already carries the #3570 Scope
    // (`aether.bloomery.api`), #3572 Construct/Refine (`construct.implement`), and
    // #3573 Land (`source.cas_land`) re-points; this branch adds the #3571 Approve
    // `process` re-point to the host-side pre-seal admission gate
    // `aether.bloomery.approve_gate`, so the merged catalog line carries all four
    // intended edits and the golden is recomputed once more.
    // Repinned again for #4314: the four opus-tier bindings re-point from
    // `claude-opus-4-8` to `claude-opus-5`, which changes their `AgentProfile`
    // digests and so the line. A recalibration is an intended catalog edit — see
    // `profile_of`, whose model and effort values are refinable without an ADR.
    // Repinned again for #4578: every profile gains a `harness` field, so every
    // binding's `AgentProfile` digest moves and the line with it. A vocabulary
    // addition is an intended catalog edit for the same reason a recalibration
    // is — and it is the point of the axis: which CLI ran a stage becomes
    // something the sealed catalog digest attests rather than a worker-local
    // accident.
    const GOLDEN_LINE_DIGEST: [u8; 32] = [
        0xfc, 0xdb, 0x2f, 0x0c, 0x83, 0x01, 0x50, 0xa2, 0xd9, 0x08, 0xf1, 0xe4, 0x39, 0xb0, 0x13, 0x95, 0xc1, 0x5d,
        0x71, 0x47, 0xab, 0x05, 0x98, 0x33, 0x83, 0x51, 0x8e, 0x03, 0xe2, 0x69, 0x1e, 0x53,
    ];

    #[test]
    fn line_digest_matches_pinned_golden() {
        assert_eq!(
            *StageCatalog::line_digest().as_bytes(),
            GOLDEN_LINE_DIGEST,
            "authored stage catalog drifted from the pinned golden digest"
        );
    }

    // ADR-0153 — the per-member line is the linear sub-sequence
    // Construct → Verify, entered at Construct and terminating (no successor) at
    // Verify; Refine and Review are off the standing line (repair re-entry and
    // the aggregate-position lane respectively). Tripwire: a reordered, dropped,
    // or re-grown member-line stage breaks the dispatched progression the
    // reducer walks.
    #[test]
    fn member_line_is_construct_verify() {
        assert_eq!(StageCatalog::entry_stage(), StageId::Construct);
        assert_eq!(StageCatalog::next_member_stage(StageId::Construct), Some(StageId::Verify));
        assert_eq!(StageCatalog::next_member_stage(StageId::Verify), None, "Verify is the per-member terminus");
        // The repair-only Refine and the aggregate-position Review are not on the
        // standing line; neither is a bloom-level tail stage.
        assert_eq!(StageCatalog::next_member_stage(StageId::Refine), None);
        assert_eq!(StageCatalog::next_member_stage(StageId::Review), None);
        assert_eq!(StageCatalog::next_member_stage(StageId::Integrate), None);
    }

    // Scope is a pre-seal operator-harness process, never a dispatched member
    // transformation — `for_member_stage` must not build a lane for it.
    #[test]
    #[should_panic(expected = "operator-harness process")]
    fn for_member_stage_panics_on_scope() {
        let subject = Digest::from_bytes([7; 32]);
        let checkout = Digest::from_bytes([9; 32]);
        let _ = Transformation::for_member_stage(StageId::Scope, subject, checkout);
    }

    // Land is a host-native source-port CAS (LandReactorCapability, #3559), never
    // a dispatched member transformation — a mis-dispatch must fail loudly rather
    // than silently running the Claude construct lane on a zero-secret worker.
    #[test]
    #[should_panic(expected = "host-native source-port CAS")]
    fn for_member_stage_panics_on_land() {
        let subject = Digest::from_bytes([7; 32]);
        let checkout = Digest::from_bytes([9; 32]);
        let _ = Transformation::for_member_stage(StageId::Land, subject, checkout);
    }

    // The per-member dispatch transformation pins the given subject as its single
    // input and declares the shared result-record output; the mechanical Verify
    // lane runs zero-egress while the model lanes run under restricted egress
    // (ADR-0149 §Execution on Actions).
    #[test]
    fn member_stage_transformation_pins_subject_and_splits_egress_by_lane() {
        let subject = Digest::from_bytes([7; 32]);
        let checkout = Digest::from_bytes([9; 32]);
        let construct = Transformation::for_member_stage(StageId::Construct, subject, checkout);
        assert_eq!(construct.inputs, alloc::vec![subject]);
        assert_eq!(construct.outputs, alloc::vec![String::from(RESULT_RECORD_OUTPUT)]);
        assert_eq!(construct.network, NetworkProfile::Restricted);
        assert_eq!(Transformation::for_member_stage(StageId::Verify, subject, checkout).network, NetworkProfile::None);
        assert_eq!(
            Transformation::for_member_stage(StageId::Review, subject, checkout).network,
            NetworkProfile::Restricted
        );
    }

    // The checkout target is a separate axis from the evidence-binding subject:
    // `checkout` is the git commit the worker checks out (the sealed source),
    // `inputs[0]` is the scope-revision digest the returned evidence binds to.
    // Tripwire: a construction that conflated the two — dropping `checkout` or
    // mirroring `subject` into it — would break the subject-threading contract
    // #3572 established for the model lanes.
    #[test]
    fn member_stage_transformation_carries_the_checkout_distinct_from_the_subject() {
        let subject = Digest::from_bytes([7; 32]);
        let checkout = Digest::from_bytes([9; 32]);
        let construct = Transformation::for_member_stage(StageId::Construct, subject, checkout);
        assert_eq!(construct.checkout, checkout, "the checkout target is threaded onto the transformation");
        assert_eq!(construct.inputs, alloc::vec![subject], "the subject stays the evidence-binding input, untouched");
        assert_ne!(construct.checkout, construct.inputs[0], "checkout and subject are independent axes");
    }

    // The reducer never authors the advisory work-order description (#3595): it
    // holds only digests, so `for_member_stage` builds every lane with
    // `description: None`. The host populates it at dispatch from durable state —
    // a construction that filled it in here would leak un-threaded text and mask
    // a missing-description warn.
    #[test]
    fn for_member_stage_authors_no_description() {
        let subject = Digest::from_bytes([7; 32]);
        let checkout = Digest::from_bytes([9; 32]);
        assert!(
            Transformation::for_member_stage(StageId::Construct, subject, checkout).description.is_none(),
            "the reducer holds only digests; the description is threaded on at dispatch",
        );
    }

    // Step-6 tripwire (#3572): the Construct and Refine bindings name the native
    // `construct.implement` transform lane, never the retired `implement` skill.
    // Deleting `.claude/skills/implement` (#3566) is gated on this — a binding that
    // still named `implement` would point the lane at a skill no longer in the tree.
    #[test]
    fn construct_and_refine_bindings_name_the_native_lane_not_the_retired_skill() {
        for stage in [StageId::Construct, StageId::Refine] {
            let binding = StageCatalog::binding_of(stage);
            assert_eq!(binding.process, "construct.implement", "{stage:?} must name the native construct lane");
            assert_ne!(binding.process, "implement", "{stage:?} must not name the retired `implement` skill");
        }
    }
}
