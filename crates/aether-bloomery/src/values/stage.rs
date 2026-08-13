//! The line: stage bindings, the catalog, attempts, and transformations
//! (ADR-0149 §The line).
//!
//! The pipeline is a closed stage vocabulary compiled into Rust, not a
//! workflow language. A [`StageBinding`] declares what one stage consumes
//! and produces, the profile that runs it, its process, its completion gate,
//! its retry budget, and the wall-clock limit its dispatched attempts run
//! within. A bloom can freeze an authored [`StageCatalog`] in its configuration
//! registry at seal.

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::{StageId, WorkpieceId};
use crate::values::{AgentProfile, ConfigScopes, Harness, ReasoningEffort, ResolvedConfigs, ResolvedModel, ToolPolicy};

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

/// The mechanical verify lane's typed command — the fan-out that runs fmt,
/// clippy and docs in CI-parity order. Named once because two stages dispatch
/// it: the member `Verify` over one candidate, and `AggregateVerify` over the
/// fold (ADR-0149 §The line).
pub const VERIFY_CHECK_COMMAND: &str = "verify.check";

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

/// Whether a binding's `process` names something an executor can actually route.
///
/// The dispatched lanes are the typed commands the host routes on; the rest are
/// the pre-seal and host-native positions whose `process` names the code that
/// runs them rather than a worker lane. A catalog naming anything else would seal
/// a stage nothing can execute, and the member would wedge with no attempt ever
/// made — a failure that belongs at the seal door, where the operator is still
/// holding the catalog they wrote.
fn is_known_process(process: &str) -> bool {
    is_model_lane(process)
        || matches!(
            process,
            "sketch"
                | "aether.bloomery.api"
                | "aether.bloomery.approve_gate"
                | "transform.verify"
                | "review"
                | "integrate"
                | "aggregate-verify"
                | "aggregate-review"
                | "source.cas_land"
                | "retrospect"
        )
}

/// Why a caller-authored [`StageCatalog`] cannot be sealed.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum CatalogError {
    /// A stage of the closed vocabulary has no binding.
    UnboundStage(StageId),
    /// A stage is bound more than once, so which binding runs is undetermined.
    DuplicateStage(StageId),
    /// A binding names a process no executor routes.
    UnknownProcess {
        /// The stage whose binding names it.
        stage: StageId,
        /// The unroutable process name.
        process: String,
    },
    /// A retry budget is zero (the stage could never run) or above
    /// [`StageCatalog::MAX_RETRY_BUDGET`].
    RetryBudgetOutOfRange {
        /// The stage whose binding carries it.
        stage: StageId,
        /// The out-of-range budget.
        budget: u32,
    },
    /// A wall-clock limit is zero (the stage's worker would have no time to run
    /// at all) or above [`ExecutionLimits::MAX_WALL_CLOCK_SECS`].
    WallClockOutOfRange {
        /// The stage whose binding carries it.
        stage: StageId,
        /// The out-of-range limit, in whole seconds.
        wall_clock_secs: u64,
    },
}

/// The resource limits one dispatched attempt runs under (ADR-0177).
///
/// Per dispatch, not per bloom. A whole-bloom ceiling can only ever refuse the
/// *next* dispatch once an overshoot is already spent, and it says nothing at
/// all about members executing concurrently; the bound a worker can actually be
/// held to is the one its own lane carries. The authority is the sealed
/// [`StageCatalog`]: every [`StageBinding`] states its stage's limit and each
/// [`Transformation`] constructor copies the resolved binding's value, so the
/// dispatched bound is a function of the catalog the bloom attested.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ExecutionLimits {
    /// How long one dispatched attempt may run, in whole seconds.
    ///
    /// Always a real bound: [`StageCatalog::validate`] refuses zero and anything
    /// above [`MAX_WALL_CLOCK_SECS`](Self::MAX_WALL_CLOCK_SECS), so there is no
    /// zero-means-unlimited mode for a reader to have to know about.
    pub wall_clock_secs: u64,
}

impl ExecutionLimits {
    /// The ceiling on an authored wall-clock limit: one day.
    ///
    /// Generous on purpose, like [`StageCatalog::MAX_RETRY_BUDGET`] — it exists
    /// to stop a mistyped unit (milliseconds authored into a seconds field)
    /// rather than to express a calibration opinion. A lane that legitimately
    /// wants longer than a day is a lane that should be split.
    pub const MAX_WALL_CLOCK_SECS: u64 = 86_400;
}

/// One stage's declared contract (ADR-0149 §The line).
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct StageBinding {
    /// The stage this binding declares.
    pub stage: StageId,
    /// The artifact-kind tags this stage consumes.
    pub consumes: Vec<String>,
    /// The artifact-kind tags this stage produces.
    pub produces: Vec<String>,
    /// The calibrated [`AgentProfile`] this stage runs under — *how* it runs
    /// (harness, model, reasoning effort, tool policy). *Who* runs is not
    /// stored: the `iama-{stage}` worker identity is derived from
    /// [`stage`](Self::stage) via [`StageId::worker_identity`].
    ///
    /// Carried inline rather than by digest. A digest would name a second
    /// artifact the resolver would have to fetch separately, and there is
    /// nothing to fetch it from — a profile is authored as part of the catalog,
    /// never on its own. Inline, one resolution of the catalog yields everything
    /// a dispatch needs.
    pub profile: AgentProfile,
    /// The skill or process the stage executes.
    pub process: String,
    /// The completion gate that decides the stage is done.
    pub completion_gate: String,
    /// The stage's retry budget.
    pub retry_budget: u32,
    /// How long one of this stage's dispatched attempts may run, in whole
    /// seconds — the value each [`Transformation`] copies into its
    /// [`ExecutionLimits`]. Authored per stage for the same reason
    /// [`retry_budget`](Self::retry_budget) is: a model lane and a compiler lane
    /// do not converge on the same clock.
    pub wall_clock_secs: u64,
}

/// The set of stage bindings a bloom runs (ADR-0149 §The line).
///
/// Sealed as a configuration rather than a field (ADR-0174), so an operator can
/// author a catalog — choosing a cheap harness for construct and an expensive
/// one for review — and the bloom attests exactly the line it ran. A bloom that
/// seals none runs [`line`](Self::line), the calibration compiled into this
/// crate.
///
/// Read by the *reducer*, not only at dispatch: the retry budgets decide
/// re-dispatch versus wedge. That is why the catalog resolves through
/// [`ResolvedConfigs`] rather than host-side only.
#[derive(aether_data::Kind, aether_data::Schema, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[kind(name = "aether.bloomery.stage_catalog")]
pub struct StageCatalog {
    /// The bindings, one per stage in the catalog.
    pub bindings: Vec<StageBinding>,
}

impl ContentAddressed for StageCatalog {
    const DOMAIN: &'static str = "aether.bloomery.stage_catalog";
}

impl StageCatalog {
    /// The catalog's content-addressed digest — the address an operator records
    /// in a bloom's configuration registry at seal.
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

    /// The [`line`](Self::line) catalog's digest. Recomputes the twelve small
    /// bindings (cheap and `no_std`-clean, no lazy static).
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

    /// The catalog `scopes` seals, or [`line`](Self::line) when it seals none.
    ///
    /// The one place the "which catalog does this bloom run" question is answered,
    /// so the seal door, the snapshot fold, and anything later cannot give
    /// different answers for the same spec. A present unresolved entry is
    /// refused before this lookup; only absence selects the compiled line.
    #[must_use]
    pub fn sealed_in(scopes: ConfigScopes<'_>, configs: &ResolvedConfigs) -> Self {
        configs.resolve::<Self>(scopes).ok().flatten().unwrap_or_else(Self::line)
    }

    /// This catalog's binding for one stage, or `None` when it binds none.
    ///
    /// Total for a catalog that passed [`validate`](Self::validate), which is
    /// every catalog a sealed bloom runs — the seal door admits nothing that
    /// leaves a stage unbound.
    #[must_use]
    pub fn binding(&self, stage: StageId) -> Option<&StageBinding> {
        self.bindings.iter().find(|binding| binding.stage == stage)
    }

    /// The retry budget this catalog gives a stage — the attempt allowance the
    /// reducer's completion gate caps re-dispatch at (ADR-0149 §The line).
    #[must_use]
    pub fn retry_budget_of(&self, stage: StageId) -> Option<u32> {
        self.binding(stage).map(|binding| binding.retry_budget)
    }

    /// The [`AgentProfile`] this catalog runs a stage under.
    #[must_use]
    pub fn profile_for(&self, stage: StageId) -> Option<&AgentProfile> {
        self.binding(stage).map(|binding| &binding.profile)
    }

    /// The ceiling on a binding's retry budget.
    ///
    /// A budget is an attempt allowance the reducer counts up to before wedging a
    /// member, so an unbounded one is an unbounded spend on a lane that is not
    /// converging. The cap is deliberately generous — it exists to stop a
    /// typo'd `1000`, not to express a calibration opinion.
    pub const MAX_RETRY_BUDGET: u32 = 16;

    /// Check that a caller-authored catalog is one the line can actually run
    /// (ADR-0174).
    ///
    /// Structural, not equality against the compiled line: the point of sealing a
    /// catalog is that it may *differ*. What cannot differ is that every stage is
    /// bound exactly once, every binding names a process some executor routes,
    /// every retry budget is a number the reducer can count to, and every
    /// wall-clock limit is a duration a worker could actually run for.
    ///
    /// # Errors
    ///
    /// [`CatalogError`] naming the first violation found, in that order.
    pub fn validate(&self) -> Result<(), CatalogError> {
        for stage in StageId::ALL {
            let bound = self.bindings.iter().filter(|binding| binding.stage == *stage).count();
            if bound == 0 {
                return Err(CatalogError::UnboundStage(*stage));
            }
            if bound > 1 {
                return Err(CatalogError::DuplicateStage(*stage));
            }
        }
        for binding in &self.bindings {
            if !is_known_process(&binding.process) {
                return Err(CatalogError::UnknownProcess {
                    stage: binding.stage,
                    process: String::from(&binding.process),
                });
            }
            if binding.retry_budget == 0 || binding.retry_budget > Self::MAX_RETRY_BUDGET {
                return Err(CatalogError::RetryBudgetOutOfRange { stage: binding.stage, budget: binding.retry_budget });
            }
            if binding.wall_clock_secs == 0 || binding.wall_clock_secs > ExecutionLimits::MAX_WALL_CLOCK_SECS {
                return Err(CatalogError::WallClockOutOfRange {
                    stage: binding.stage,
                    wall_clock_secs: binding.wall_clock_secs,
                });
            }
        }
        Ok(())
    }

    /// The authored binding for one stage. An exhaustive `match` over the closed
    /// [`StageId`] enum — the compile-time guard that every stage has exactly one
    /// binding (ADR-0149 §The line). The binding references the stage's
    /// calibrated [`AgentProfile`] by digest via [`profile_of`](Self::profile_of).
    ///
    /// The in-crate consumer is `reduce::attempt::stage_binding`, which falls
    /// back to this compiled-line binding when a sealed catalog binds no such
    /// stage — the same totality [`profile_of`](Self::profile_of) gives the
    /// profile axis. It is `pub` rather than `pub(crate)` because
    /// `aether-chassis-bloomery` constructs compiled-line bindings directly in
    /// its executor, reactor-runtime, and study fixtures.
    ///
    /// Every arm calibrates `wall_clock_secs` at one hour. That is the initial
    /// calibration, refinable per stage without an ADR like the tag/gate strings
    /// and the retry budgets beside it; a custom catalog may author anything
    /// [`validate`](Self::validate) accepts.
    #[must_use]
    pub fn binding_of(stage: StageId) -> StageBinding {
        let (consumes, produces, process, completion_gate, retry_budget, wall_clock_secs): (
            &[&str],
            &[&str],
            &str,
            &str,
            u32,
            u64,
        ) = match stage {
            StageId::Sketch => (&["bloom.intent"], &["bloom.sketch"], "sketch", "issue-well-formed", 1, 3_600),
            // Scope is a pre-seal operator-harness process (ADR-0149 §The
            // line, ADR-0150): the operator's own developer-side Bloomery
            // session authors the scope revision and stages it through the
            // REST control API (`aether.bloomery.api`'s `POST /workpieces`,
            // `PATCH /drafts/{id}`, `POST /drafts/{id}/seal`) — never a
            // dispatched worker lane. `process` names that api cap's
            // `NAMESPACE`, not a skill slug.
            StageId::Scope => (&["bloom.sketch"], &["bloom.scope"], "aether.bloomery.api", "plan-present", 1, 3_600),
            // Approve is a pre-seal host-side admission gate (ADR-0149 §The
            // line, ADR-0151): the coordinator's own host resolves the
            // workpiece's declared surface to an approval tier and forms the
            // membership `approval` before `Fact::Seal`, never a dispatched
            // worker lane (the member-line dispatch loop never reaches this
            // pre-seal stage). `process` names that host gate, not the retired
            // `.claude/skills/approve` skill slug.
            StageId::Approve => {
                (&["bloom.scope"], &["bloom.ready"], "aether.bloomery.approve_gate", "phase-ready", 1, 3_600)
            }
            StageId::Construct => {
                (&["bloom.ready"], &["bloom.candidate"], CONSTRUCT_IMPLEMENT_COMMAND, "pr-open", 2, 3_600)
            }
            StageId::Verify => {
                (&["bloom.candidate"], &["bloom.verify_evidence"], "transform.verify", "ci-green", 3, 3_600)
            }
            StageId::Refine => {
                (&["bloom.verify_evidence"], &["bloom.candidate"], CONSTRUCT_IMPLEMENT_COMMAND, "ci-green", 3, 3_600)
            }
            StageId::Review => (&["bloom.candidate"], &["bloom.review_rollup"], "review", "review-approved", 2, 3_600),
            StageId::Integrate => {
                (&["bloom.candidate"], &["bloom.integration"], "integrate", "integration-checkpoint", 2, 3_600)
            }
            StageId::AggregateVerify => (
                &["bloom.integration"],
                &["bloom.aggregate_verify"],
                "aggregate-verify",
                "aggregate-ci-green",
                2,
                3_600,
            ),
            StageId::AggregateReview => (
                &["bloom.integration"],
                &["bloom.aggregate_review"],
                "aggregate-review",
                "aggregate-review-approved",
                2,
                3_600,
            ),
            // Two landing attempts, not one (#4689): the landing branch's
            // CI is the only gate that judges the fold against current
            // mainline, so its first red is often a conflict with work that
            // merged while this bloom ran — answerable by re-opening the
            // line once. A second red is not something re-proposing can
            // fix, so the budget stops there and the bloom parks.
            StageId::Land => (
                &["bloom.aggregate_verify", "bloom.aggregate_review"],
                &["bloom.receipt"],
                "source.cas_land",
                "landed",
                2,
                3_600,
            ),
            StageId::Study => (&["bloom.receipt"], &["bloom.study"], "retrospect", "study-recorded", 1, 3_600),
        };
        StageBinding {
            stage,
            consumes: consumes.iter().map(|tag| String::from(*tag)).collect(),
            produces: produces.iter().map(|tag| String::from(*tag)).collect(),
            profile: Self::profile_of(stage),
            process: String::from(process),
            completion_gate: String::from(completion_gate),
            retry_budget,
            wall_clock_secs,
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
    /// The model id the muse-harness stages resolve to.
    ///
    /// The **contributor** tier, deliberately: it is roughly an order of
    /// magnitude cheaper than standard `muse-spark-1.2`, which is what makes
    /// running every model lane on it affordable at all. Its terms state that
    /// content may be used for product improvement — sound here because this
    /// repository is public and a bloom's lanes see only its own source, and a
    /// decision to re-take before a Bloomery instance is ever pointed at a
    /// private repository. Naming it in the catalog is what makes that choice
    /// attestable rather than an operator's ambient default.
    const MUSE_MODEL: &'static str = "muse-spark-1.2-contributor";

    #[must_use]
    pub fn profile_of(stage: StageId) -> AgentProfile {
        // The four **dispatched model lanes** — the ones that actually fork an
        // agent CLI — run muse: Construct and its Refine repair re-entry, and
        // the two review positions. That is the whole set `is_model_lane`
        // recognizes, so this is the calibration that decides what writes the
        // code and what judges it.
        //
        // The remaining stages keep their Claude calibration and it is inert:
        // Scope and Approve are pre-seal operator/host processes, Study is not
        // dispatched as a worker lane, and the mechanical stages run a compiler.
        // `is_model_lane` keeps the resolved harness off every one of their
        // argvs, so their harness names a CLI none of them forks.
        //
        // Harness and model move together, and must: a model id belongs to the
        // provider its harness talks to, so a lane pointed at muse while still
        // naming an Anthropic id would dispatch an id its harness cannot resolve.
        //
        // Refine sits on Construct's tier rather than below it (#4685). It is
        // handed a candidate that failed plus the findings against it, and asked
        // to reconcile both — strictly more than Construct's clean start, so
        // calibrating it lower asks the harder half of the loop to run on less.
        // A repair that needs the approach reconsidered, rather than a line
        // patched, is exactly where that shortfall shows: the ceiling is Verify's
        // retry budget, so a member that cannot converge spends the whole budget
        // and wedges.
        let (harness, model, effort): (Harness, &str, ReasoningEffort) = match stage {
            StageId::Construct | StageId::Refine | StageId::Review | StageId::AggregateReview => {
                (Harness::Muse, Self::MUSE_MODEL, ReasoningEffort::High)
            }
            StageId::Scope | StageId::Study => (Harness::Claude, Self::OPUS_MODEL, ReasoningEffort::High),
            StageId::Sketch
            | StageId::Approve
            | StageId::Verify
            | StageId::Integrate
            | StageId::AggregateVerify
            | StageId::Land => (Harness::Claude, Self::SONNET_MODEL, ReasoningEffort::Medium),
        };
        AgentProfile { harness, model: String::from(model), effort, tools: ToolPolicy::Full }
    }
}

/// The execution slot one dispatch targets — the key of a bloom's dispatch
/// ledger (ADR-0180, [`BloomRecord::dispatches`](crate::BloomRecord::dispatches)).
///
/// A coordinate into the line rather than a stage of it: [`StageId`] names
/// *which* stage, and this names *whose* — the member a per-member stage
/// dispatches against, or the bloom itself for the positions that run once per
/// bloom. Counting dispatches per slot is what separates two members each
/// constructing once (two slots at one dispatch each, no retry) from one member
/// constructing twice (one slot at two dispatches, one retry).
///
/// Its [`Ord`] is the ledger map's key order, so `Member` entries sort by
/// workpiece then stage and precede every `Bloom` entry. Nothing reads that
/// order for meaning — the grade sums the whole map — but it makes a rendered
/// ledger stable across replays.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub enum DispatchKey {
    /// One member's slot at one stage: `Construct`, `Verify`, or the
    /// repair-only `Refine` re-entry (ADR-0153's dispatched member positions).
    Member {
        /// The member the attempt runs against.
        workpiece: WorkpieceId,
        /// The stage dispatched against it.
        stage: StageId,
    },
    /// One bloom-level position's slot: `Integrate`, `AggregateVerify`,
    /// `AggregateReview`, or `Land` — the stages that dispatch once per bloom
    /// rather than once per member.
    Bloom {
        /// The dispatched bloom-level stage.
        stage: StageId,
    },
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
    /// The commit the candidate's diff is taken **against** — the work order's
    /// diff source, distinct from the tree it is taken *in*
    /// ([`checkout`](Self::checkout)).
    ///
    /// `None` names the working tree: the candidate is the uncommitted change
    /// the checked-out tree carries, which is what every member lane produces.
    /// `Some(base)` names a commit range instead — the candidate is everything
    /// `base..checkout` contains. The aggregate review is the one stage whose
    /// candidate is already committed (the integration the fold built), so a
    /// working-tree diff there is always empty and a lane that assumed one
    /// judged nothing at all (#4723).
    ///
    /// A digest like [`checkout`](Self::checkout), resolved to a real git object
    /// by the executor backend through the same correspondence store, so the
    /// reducer keeps holding digests rather than shas.
    #[serde(default)]
    pub diff_base: Option<Digest>,
    /// The declared output names the broker accepts.
    pub outputs: Vec<String>,
    /// The execution image.
    pub image: String,
    /// The resource limits this one dispatch runs under, copied from the
    /// resolved [`StageBinding`] the sealed catalog gives its stage.
    pub limits: ExecutionLimits,
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
    ///
    /// `binding` is the sealed catalog's resolved binding for the stage being
    /// dispatched — it names the stage *and* carries its authored
    /// `wall_clock_secs`, so the dispatched limit cannot come from a stage other
    /// than the one being dispatched.
    #[must_use]
    pub fn for_member_stage(binding: &StageBinding, subject: Digest, checkout: Digest, base: Digest) -> Self {
        let (command, image, network): (&str, &str, NetworkProfile) = match binding.stage {
            // The mechanical Verify lane runs zero-egress; every model-driven lane
            // (Construct / Refine, and the non-member stages that fall through to
            // the construct lane here) reaches the model API under restricted
            // egress. Review is its own model lane.
            StageId::Verify => (VERIFY_CHECK_COMMAND, "iama/verify:1", NetworkProfile::None),
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
            // A model lane's candidate is the uncommitted change its worker
            // leaves in the checked-out tree, so it names no diff base. The
            // mechanical `Verify` lane is the exception: by the time it runs,
            // the candidate is already captured as `base..checkout`, and
            // naming that range is what lets it narrow its compiling gates to
            // the diff's reverse-dependency closure instead of recompiling the
            // workspace on every refine lap (#4890). The whole-bloom aggregate
            // verify deliberately names none, so it keeps the whole tree.
            diff_base: matches!(binding.stage, StageId::Verify).then_some(base),
            outputs: alloc::vec![String::from(RESULT_RECORD_OUTPUT)],
            image: String::from(image),
            limits: ExecutionLimits { wall_clock_secs: binding.wall_clock_secs },
            network,
            // The reducer holds only digests; the operator's work-order text is
            // advisory context the host threads on at dispatch, never here. The
            // effective model rides the same channel for the same reason — the
            // reducer names the catalog by digest, the host resolves it.
            description: None,
            model: None,
        }
    }

    /// The whole-bloom aggregate-verify transformation: the same mechanical
    /// `verify.check` fan-out the member `Verify` runs, dispatched once per
    /// bloom against the folded head.
    ///
    /// A member's verdict only ever judged its own candidate in isolation; the
    /// fold is the first tree that carries every member at once, so it is the
    /// first thing that can fail on their interaction. Running the compiler
    /// here is what stops that failure from being discovered by the landing
    /// CI, downstream of the point where the bloom can still route it back to
    /// an owner. Zero-egress like the member lane — it runs a compiler and
    /// nothing else.
    ///
    /// `binding` is the sealed catalog's `AggregateVerify` binding, carrying the
    /// authored wall-clock limit this fan-out runs under. That pairing is
    /// checked rather than assumed, but only in a debug build: the sibling
    /// `for_member_stage` derives its whole lane from an exhaustive
    /// `match binding.stage`, so cross-pairing is impossible there in every
    /// profile, while here a debug-only assertion is a test-and-CI tripwire that
    /// keeps release behavior total.
    ///
    /// # Panics
    ///
    /// In a debug build, when `binding` is not the `AggregateVerify` binding.
    #[must_use]
    pub fn for_aggregate_verify(binding: &StageBinding, subject: Digest, checkout: Digest) -> Self {
        debug_assert_eq!(
            binding.stage,
            StageId::AggregateVerify,
            "the dispatched limit must come from the stage being dispatched",
        );

        Self {
            command: String::from(VERIFY_CHECK_COMMAND),
            inputs: alloc::vec![subject],
            checkout,
            // The mechanical lane runs a compiler over the checked-out tree and
            // reads no diff at all, so there is no source to name.
            diff_base: None,
            outputs: alloc::vec![String::from(RESULT_RECORD_OUTPUT)],
            image: String::from("iama/verify:1"),
            limits: ExecutionLimits { wall_clock_secs: binding.wall_clock_secs },
            network: NetworkProfile::None,
            description: None,
            model: None,
        }
    }

    /// The whole-bloom aggregate-review transformation (ADR-0153): the
    /// `review.critic` lane dispatched once per bloom against the integrated
    /// head — `subject` is the integrated tree digest the returned evidence
    /// binds, `checkout` the landable head commit the critic checks out, and
    /// `base` the bloom's sealed base the fold was built onto. The same lane
    /// shape as the member `Review` egress it replaces (restricted egress to the
    /// model API); the sealed intent the critic judges against rides the
    /// advisory description the host threads on at dispatch.
    ///
    /// `base` is what makes the critic's candidate visible. The integration is
    /// committed, so the diff to judge is the range `base..checkout` — a lane
    /// left to read the working tree here sees a clean checkout and judges an
    /// empty candidate (#4723). It rides the work order as
    /// [`diff_base`](Self::diff_base) rather than a stage flag the lane branches
    /// on: the stage is what *knows* where its candidate lives, and the lane
    /// then reads one field instead of re-deriving that knowledge.
    ///
    /// Dispatched only once [`for_aggregate_verify`](Self::for_aggregate_verify)
    /// has passed over the same fold: the compiler is the cheaper and more
    /// decisive gate, and there is nothing to judge in a fold that does not
    /// build.
    ///
    /// `binding` is the sealed catalog's `AggregateReview` binding, carrying the
    /// authored wall-clock limit this critic runs under. That pairing is
    /// checked rather than assumed, but only in a debug build: the sibling
    /// `for_member_stage` derives its whole lane from an exhaustive
    /// `match binding.stage`, so cross-pairing is impossible there in every
    /// profile, while here a debug-only assertion is a test-and-CI tripwire that
    /// keeps release behavior total.
    ///
    /// # Panics
    ///
    /// In a debug build, when `binding` is not the `AggregateReview` binding.
    #[must_use]
    pub fn for_aggregate_review(binding: &StageBinding, subject: Digest, checkout: Digest, base: Digest) -> Self {
        debug_assert_eq!(
            binding.stage,
            StageId::AggregateReview,
            "the dispatched limit must come from the stage being dispatched",
        );

        Self {
            command: String::from(REVIEW_CRITIC_COMMAND),
            inputs: alloc::vec![subject],
            checkout,
            diff_base: Some(base),
            outputs: alloc::vec![String::from(RESULT_RECORD_OUTPUT)],
            image: String::from("iama/review-claude:1"),
            limits: ExecutionLimits { wall_clock_secs: binding.wall_clock_secs },
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

    // The compiled line's binding for one stage — what a reducer call site
    // resolves before it builds a dispatch.
    fn binding(stage: StageId) -> StageBinding {
        StageCatalog::binding_of(stage)
    }

    // The compiled line with one stage's wall-clock limit re-authored, which is
    // what an operator sealing a custom catalog produces.
    fn catalog_with_wall_clock(stage: StageId, wall_clock_secs: u64) -> StageCatalog {
        let mut catalog = StageCatalog::line();
        for authored in &mut catalog.bindings {
            if authored.stage == stage {
                authored.wall_clock_secs = wall_clock_secs;
            }
        }
        catalog
    }

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
                StageCatalog::profile_of(binding.stage),
                "binding for {:?} does not reference its calibrated profile digest",
                binding.stage
            );
        }
    }

    // Tripwire: a model lane's harness and model id agree. A model id belongs to
    // the provider its harness talks to, so a stage moved onto a model lane — or
    // recalibrated onto a different harness — while keeping the other half of
    // the pair would dispatch an id its harness cannot resolve. The failure is
    // remote and late (the child CLI rejects the model mid-run), so the pairing
    // is pinned here where it is authored.
    #[test]
    fn every_dispatched_model_lane_pairs_its_harness_with_that_harnesss_model() {
        for binding in StageCatalog::line().bindings {
            if !is_model_lane(&binding.process) {
                continue;
            }
            let profile = StageCatalog::profile_of(binding.stage);
            assert_eq!(
                profile.harness,
                Harness::Muse,
                "{:?} is a dispatched model lane, so it must name the calibrated model harness",
                binding.stage,
            );
            assert_eq!(
                profile.model,
                StageCatalog::MUSE_MODEL,
                "{:?} runs under muse, so its model id must be a muse id",
                binding.stage,
            );
        }
    }

    // Tripwire: the compiled line catalog's digest. Computed over its authored bindings,
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
    // Repinned again for #4579: the four dispatched model lanes recalibrate onto
    // the muse harness and its model id, moving their profile digests and the
    // line with them. A recalibration is an intended catalog edit — see
    // `profile_of`, whose harness/model/effort values are refinable without an
    // ADR.
    // Repinned again for #4587: `StageBinding` carries its `AgentProfile` inline
    // rather than by digest, so the catalog's own bytes now contain each stage's
    // calibration instead of a reference to it. One resolution of a sealed catalog
    // yields everything a dispatch needs — see `StageBinding::profile`.
    // Repinned again for #4685: Refine recalibrates from medium to high effort,
    // joining Construct on the tier it repairs against. A recalibration is an
    // intended catalog edit — see `profile_of`.
    // Repinned again for #4689: `Land`'s retry budget goes 1 → 2, buying a bloom
    // one repair cycle when the landing branch's CI refuses it. That gate is the
    // only one judging the fold against current mainline, so its first red is
    // often a conflict with work that merged mid-bloom — answerable by
    // re-opening the line once, and terminal on the second.
    // Repinned again for #4697: every binding gains a `wall_clock_secs`, so the
    // catalog's bytes carry a per-stage dispatch limit the line never stated. A
    // vocabulary addition is an intended catalog edit — the limit that bounds a
    // stage's worker becomes something the sealed catalog attests rather than a
    // whole-bloom number nothing read.
    const GOLDEN_LINE_DIGEST: [u8; 32] = [
        0xb3, 0x02, 0x2f, 0xfa, 0x87, 0x49, 0x0d, 0x2d, 0x8e, 0x60, 0xbb, 0x14, 0x47, 0xfa, 0x89, 0xc7, 0xdc, 0x66,
        0xc7, 0x2f, 0xae, 0xa5, 0x83, 0xbb, 0x19, 0x7a, 0xbf, 0x08, 0xd6, 0x73, 0xf4, 0xb8,
    ];

    // Tripwire: the compiled line passes the same validation an authored catalog
    // must. It is the fallback every unconfigured bloom runs, so a line that fails
    // its own rule would refuse every seal — and the rule is authored by hand
    // against binding values authored by hand, which is exactly where the two
    // drift. Caught for real when `is_known_process` first omitted `Review`'s
    // `review` process.
    #[test]
    fn the_compiled_line_satisfies_the_rule_authored_catalogs_are_held_to() {
        assert_eq!(StageCatalog::line().validate(), Ok(()));
    }

    // Tripwire: the seal door refuses a wall-clock limit no worker could run
    // under — zero (no time at all) and anything past the one-day ceiling. The
    // limit is authored by hand, which is where a mistyped unit lands, and an
    // unvalidated one would ride every dispatch of that stage with nothing
    // downstream able to tell it from a deliberate value.
    #[test]
    fn validate_refuses_a_wall_clock_limit_outside_the_authored_range() {
        assert_eq!(
            catalog_with_wall_clock(StageId::Verify, 0).validate(),
            Err(CatalogError::WallClockOutOfRange { stage: StageId::Verify, wall_clock_secs: 0 })
        );
        assert_eq!(
            catalog_with_wall_clock(StageId::Verify, ExecutionLimits::MAX_WALL_CLOCK_SECS + 1).validate(),
            Err(CatalogError::WallClockOutOfRange { stage: StageId::Verify, wall_clock_secs: 86_401 })
        );
        assert_eq!(catalog_with_wall_clock(StageId::Verify, 900).validate(), Ok(()));
    }

    #[test]
    fn line_digest_matches_pinned_golden() {
        assert_eq!(
            *StageCatalog::line_digest().as_bytes(),
            GOLDEN_LINE_DIGEST,
            "compiled stage catalog drifted from the pinned golden digest"
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
        let base = Digest::from_bytes([5; 32]);
        let _ = Transformation::for_member_stage(&binding(StageId::Scope), subject, checkout, base);
    }

    // Land is a host-native source-port CAS (LandReactorCapability, #3559), never
    // a dispatched member transformation — a mis-dispatch must fail loudly rather
    // than silently running the Claude construct lane on a zero-secret worker.
    #[test]
    #[should_panic(expected = "host-native source-port CAS")]
    fn for_member_stage_panics_on_land() {
        let subject = Digest::from_bytes([7; 32]);
        let checkout = Digest::from_bytes([9; 32]);
        let base = Digest::from_bytes([5; 32]);
        let _ = Transformation::for_member_stage(&binding(StageId::Land), subject, checkout, base);
    }

    // The per-member dispatch transformation pins the given subject as its single
    // input and declares the shared result-record output; the mechanical Verify
    // lane runs zero-egress while the model lanes run under restricted egress
    // (ADR-0149 §Execution on Actions).
    #[test]
    fn member_stage_transformation_pins_subject_and_splits_egress_by_lane() {
        let subject = Digest::from_bytes([7; 32]);
        let checkout = Digest::from_bytes([9; 32]);
        let base = Digest::from_bytes([5; 32]);
        let construct = Transformation::for_member_stage(&binding(StageId::Construct), subject, checkout, base);
        assert_eq!(construct.inputs, alloc::vec![subject]);
        assert_eq!(construct.outputs, alloc::vec![String::from(RESULT_RECORD_OUTPUT)]);
        assert_eq!(construct.network, NetworkProfile::Restricted);
        assert_eq!(
            Transformation::for_member_stage(&binding(StageId::Verify), subject, checkout, base).network,
            NetworkProfile::None
        );
        assert_eq!(
            Transformation::for_member_stage(&binding(StageId::Review), subject, checkout, base).network,
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
        let base = Digest::from_bytes([5; 32]);
        let construct = Transformation::for_member_stage(&binding(StageId::Construct), subject, checkout, base);
        assert_eq!(construct.checkout, checkout, "the checkout target is threaded onto the transformation");
        assert_eq!(construct.inputs, alloc::vec![subject], "the subject stays the evidence-binding input, untouched");
        assert_ne!(construct.checkout, construct.inputs[0], "checkout and subject are independent axes");
    }

    // Only the mechanical Verify lane names a diff base (#4890). It is the one
    // member stage whose candidate is already captured by the time it runs, so
    // `base..checkout` is a real range there, and naming it is what lets the
    // lane narrow its compiling gates to that range's reverse-dependency
    // closure instead of recompiling the workspace on every refine lap.
    //
    // Tripwire in both directions. A model lane that gained one would judge the
    // sealed base's own history instead of the uncommitted candidate in front
    // of it. And the whole-bloom aggregate verify — built by
    // `for_aggregate_verify`, asserted here beside its member sibling — must
    // keep naming none, because the stage that proves what lands is the one
    // stage that must go on compiling the whole tree.
    #[test]
    fn only_the_member_verify_lane_names_the_range_its_candidate_was_captured_over() {
        let subject = Digest::from_bytes([7; 32]);
        let checkout = Digest::from_bytes([9; 32]);
        let base = Digest::from_bytes([5; 32]);

        assert_eq!(
            Transformation::for_member_stage(&binding(StageId::Verify), subject, checkout, base).diff_base,
            Some(base),
            "the member verify's candidate is the committed range its narrowing reads",
        );
        for stage in [StageId::Construct, StageId::Refine, StageId::Review] {
            assert_eq!(
                Transformation::for_member_stage(&binding(stage), subject, checkout, base).diff_base,
                None,
                "{stage:?}'s candidate is the working tree, which names no range",
            );
        }
        assert_eq!(
            Transformation::for_aggregate_verify(&binding(StageId::AggregateVerify), subject, checkout).diff_base,
            None,
            "the aggregate verify keeps the whole workspace, so it names nothing to narrow by",
        );
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
        let base = Digest::from_bytes([5; 32]);
        assert!(
            Transformation::for_member_stage(&binding(StageId::Construct), subject, checkout, base)
                .description
                .is_none(),
            "the reducer holds only digests; the description is threaded on at dispatch",
        );
    }

    // Every dispatch constructor copies the limit off the binding it was handed
    // — never a default, never a hard-coded one. An operator who shortens one
    // lane in an authored catalog must see that lane's dispatch shorten while
    // every other stage keeps its compiled calibration; a constructor that
    // reached for a whole-bloom or hard-coded value would pass every other
    // assertion in this module.
    #[test]
    fn every_dispatch_constructor_copies_the_limit_off_the_binding_it_was_handed() {
        let mut shortened = binding(StageId::Verify);
        shortened.wall_clock_secs = 900;
        // Each aggregate binding keeps its own `stage` and re-authors only the
        // limit, to a value the compiled line never produces and distinct from
        // the member half's — so a constructor reaching for a hard-coded 3_600
        // fails here, and the two stages cannot pass by reading each other.
        let mut shortened_aggregate_verify = binding(StageId::AggregateVerify);
        shortened_aggregate_verify.wall_clock_secs = 1_200;
        let mut shortened_aggregate_review = binding(StageId::AggregateReview);
        shortened_aggregate_review.wall_clock_secs = 1_500;

        let subject = Digest::from_bytes([7; 32]);
        let checkout = Digest::from_bytes([9; 32]);
        let base = Digest::from_bytes([3; 32]);

        assert_eq!(
            Transformation::for_member_stage(&shortened, subject, checkout, base).limits.wall_clock_secs,
            900,
            "the re-authored binding's limit reaches the dispatch"
        );
        assert_eq!(
            Transformation::for_member_stage(&binding(StageId::Construct), subject, checkout, base)
                .limits
                .wall_clock_secs,
            3_600,
            "an untouched sibling keeps the compiled calibration"
        );
        assert_eq!(
            Transformation::for_aggregate_verify(&shortened_aggregate_verify, subject, checkout).limits.wall_clock_secs,
            1_200,
            "the aggregate-verify fan-out copies the binding it was handed, not the compiled calibration"
        );
        assert_eq!(
            Transformation::for_aggregate_review(&shortened_aggregate_review, subject, checkout, base)
                .limits
                .wall_clock_secs,
            1_500,
            "the aggregate-review critic copies the binding it was handed, not the compiled calibration"
        );
    }

    // Tripwire: each aggregate constructor rejects a binding for a stage other
    // than its own. Its limit is copied straight off the binding it was handed,
    // so a member `Verify` binding reaching the whole-bloom fan-out would
    // dispatch the member lane's wall-clock as the aggregate's — a wrong number
    // that no other assertion here can see, because the copy itself is correct.
    // Nothing in the tree hands either constructor a mismatched binding, so
    // without these two the guards could be deleted and every suite stay green.
    //
    // The guards are `debug_assert_eq!`, so they only fire when
    // `debug_assertions` is on; gate the panic tests on it so a
    // `cargo test --release` run (assertions compiled out) stays green.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "the dispatched limit must come from the stage being dispatched")]
    fn for_aggregate_verify_panics_on_a_binding_for_another_stage() {
        let subject = Digest::from_bytes([7; 32]);
        let checkout = Digest::from_bytes([9; 32]);
        let _ = Transformation::for_aggregate_verify(&binding(StageId::Verify), subject, checkout);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "the dispatched limit must come from the stage being dispatched")]
    fn for_aggregate_review_panics_on_a_binding_for_another_stage() {
        let subject = Digest::from_bytes([7; 32]);
        let checkout = Digest::from_bytes([9; 32]);
        let base = Digest::from_bytes([3; 32]);
        let _ = Transformation::for_aggregate_review(&binding(StageId::Review), subject, checkout, base);
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
