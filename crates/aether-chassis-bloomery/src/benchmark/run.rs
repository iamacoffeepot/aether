//! Planning one benchmark run: the per-cell seal loop, as a value (ADR-0184).
//!
//! A run takes a [`GoldenTaskSet`] and a list of profile cells and produces one
//! [`BloomSpec`] per `(task, cell, sample)` — same order, same base, differing
//! only in the [`ModelOverride`] each member seals. That is the whole mechanism
//! ADR-0184 names, and it is pure: nothing here reads a store, admits a fact, or
//! touches a repository, so the shape of a run is testable without a coordinator
//! and the door next door is left with only the hops.

use std::error::Error;
use std::fmt;

use aether_bloomery::{
    BloomDraft, BloomId, BloomSpec, ConfigRegistry, Digest, Evidence, EvidenceKind, Membership, ModelOverride,
    Observation, Outcome, Provenance, Statement, StoreClass, WorkpieceId, digest_of,
};
use aether_bloomery_git::short_hex;
use aether_data::Kind;
use serde::{Deserialize, Serialize};

use super::golden::{GoldenTask, GoldenTaskSet};

/// Ceiling on how many blooms one run may seal.
///
/// A run's bloom count is the product of three operator-supplied numbers, so it
/// is the one input here that grows multiplicatively: four cells at sample size
/// four over eight tasks is a hundred and twenty-eight blooms, each of which
/// dispatches model lanes that cost money. The cap refuses the request outright
/// rather than sealing a prefix of it, because a partially-sealed benchmark is a
/// comparison with a hole in it — the cells that fit measured the task and the
/// ones that did not are silently absent from the table.
pub const MAX_BENCHMARK_BLOOMS: usize = 64;

/// The words the trial approval's supporting observation asserts — the sibling
/// of the approve gate's auto-tier record.
const TRIAL_APPROVAL_WORDS: &[u8] = b"aether.bloomery.benchmark: replay of landed history in trial mode";

/// The source label that record carries.
const TRIAL_APPROVAL_SOURCE: &str = "aether.bloomery.benchmark:trial-replay";

/// One profile cell a run measures: the address of the [`ModelOverride`] its
/// blooms seal.
///
/// The cell *is* the override address, and the address is the selector — which
/// is what bloom `73d025b42e0a` proved live. Nothing is copied out of the
/// override into the cell, because the ledger keys its rows on the resolved
/// agent it recomputes from the sealed registry (ADR-0184 §The agent is
/// recomputed): a second copy of the model id here could only agree or be a bug.
pub type CellAddress = Digest;

/// Why a benchmark run was refused before it sealed anything.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BenchmarkRefusal {
    /// The coordinator is not in trial mode. A benchmark against a live-classed
    /// store is a refusal and never a warning: its blooms would append rows to
    /// the estate's own history, where nothing downstream could separate them
    /// again (ADR-0184, issue #5794).
    NotTrialMode(StoreClass),
    /// The request named no landed pull requests, so there is nothing to
    /// replay.
    NoTasks,
    /// The request named no profile cells, so there is nothing to compare.
    NoCells,
    /// The request asked for no samples per cell.
    NoSamples,
    /// The run would seal more than [`MAX_BENCHMARK_BLOOMS`] blooms.
    TooManyBlooms(usize),
    /// A named cell address resolves to no stored configuration, so its blooms
    /// would fall back to the compiled line and every cell would measure the
    /// same agent under a different name.
    UnresolvableCell(CellAddress),
    /// A named cell address resolves to something that is not a
    /// [`ModelOverride`].
    CellIsNotAnOverride {
        /// The address named.
        cell: CellAddress,
        /// The kind it is actually filed under.
        kind: String,
    },
}

impl fmt::Display for BenchmarkRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotTrialMode(class) => write!(
                f,
                "this coordinator's journal is classed {}; a benchmark run seals only against a trial store on the fixture backend",
                class.as_str()
            ),
            Self::NoTasks => write!(f, "a benchmark run needs at least one landed pull request to replay"),
            Self::NoCells => write!(f, "a benchmark run needs at least one profile cell to compare"),
            Self::NoSamples => write!(f, "a benchmark run needs a sample size of at least one"),
            Self::TooManyBlooms(blooms) => {
                write!(f, "this run would seal {blooms} blooms; one run is capped at {MAX_BENCHMARK_BLOOMS}")
            }
            Self::UnresolvableCell(cell) => write!(f, "no stored configuration at cell address {}", cell.to_hex()),
            Self::CellIsNotAnOverride { cell, kind } => {
                write!(f, "cell address {} is filed as `{kind}`, not `{}`", cell.to_hex(), ModelOverride::NAME)
            }
        }
    }
}

impl Error for BenchmarkRefusal {}

/// One bloom a run seals: which cell and sample of which task it is, and the
/// spec that pins them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PlannedBloom {
    /// The frozen spec — one member, sealing the cell's override on the set's
    /// base.
    pub spec: BloomSpec,
    /// The member's workpiece, which is also its claim and candidate-ref name.
    pub workpiece: WorkpieceId,
    /// The work order this bloom replays, recorded as the member's dispatch
    /// description so the lane reads the same text the landed member read.
    pub order: String,
    /// The landed pull request the order was drawn from.
    pub pull_request: u64,
    /// The cell selector this bloom seals.
    pub cell: CellAddress,
    /// Which repetition within the cell this is, from zero.
    pub sample: u32,
}

impl PlannedBloom {
    /// The bloom's identity.
    #[must_use]
    pub fn id(&self) -> BloomId {
        self.spec.id()
    }
}

/// A planned run: the set it replays and every bloom it will seal.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BenchmarkPlan {
    /// The set, and with it the base every bloom seals on.
    pub set: GoldenTaskSet,
    /// One entry per `(task, cell, sample)`, in that nesting order.
    pub blooms: Vec<PlannedBloom>,
}

/// What the control core answered for one benchmark bloom's seal.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum BenchmarkAdmission {
    /// The reducer's own answer. A run reports it verbatim rather than
    /// collapsing it to a boolean: a cell whose seal came back
    /// [`Duplicate`](aether_bloomery::Outcome::Duplicate) measured nothing, and
    /// that has to be visible in the run's output rather than inferred from a
    /// ledger that is short a sample.
    Admitted(Outcome),
    /// The admit itself faulted — the bytes did not decode, or the commit
    /// failed — so this cell has no bloom at all.
    Refused(String),
}

/// One bloom of a run, as an operator reads it back.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BenchmarkBloomView {
    /// The sealed bloom.
    pub bloom: BloomId,
    /// Its single member.
    pub workpiece: String,
    /// The landed pull request its order came from.
    pub pull_request: u64,
    /// The cell selector it seals — what the ledger's rows are attributable to.
    pub cell: Digest,
    /// Which repetition within the cell it is.
    pub sample: u32,
    /// What the control core answered.
    pub admission: BenchmarkAdmission,
}

/// A finished run, as the operator door renders it.
///
/// The caveats ride here for the reason they ride
/// [`CapabilityLedger`](aether_bloomery::CapabilityLedger): a run's own output is
/// where a reader first meets its cells, so the boundary those cells are read
/// under has to arrive with them rather than waiting for a later
/// `GET /calibration`. Rendered, never folded — nothing in the run adjusts a
/// count for either.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BenchmarkReport {
    /// The set that was replayed.
    pub set: String,
    /// Its version — the digest that pins exactly which comparison these blooms
    /// belong to (see [`GoldenTaskSet`]).
    pub set_version: Digest,
    /// The base every bloom sealed on.
    pub base: Digest,
    /// The tasks drawn, in the order they were named.
    pub tasks: Vec<GoldenTask>,
    /// One entry per sealed bloom.
    pub blooms: Vec<BenchmarkBloomView>,
    /// The ledger's own honesty boundary
    /// ([`LEDGER_CAVEAT`](aether_bloomery::LEDGER_CAVEAT)).
    pub caveat: String,
    /// The under-reporting boundary
    /// ([`COST_CAVEAT`](aether_bloomery::COST_CAVEAT)), beside the ledger's own
    /// and never inside a count.
    pub cost_caveat: String,
}

/// Plan a run over `set`, one bloom per `(task, cell, sample)`.
///
/// `resolve_cell` answers what kind a cell address is filed under, `None` when
/// the address resolves to nothing — the caller's window onto stored
/// configuration, taken as a closure so this stays a pure function of the values
/// it is handed.
///
/// # Errors
/// [`BenchmarkRefusal`] for an empty cell list, a zero sample size, a run over
/// the bloom cap, or a cell address that does not resolve to a
/// [`ModelOverride`].
pub fn plan(
    set: GoldenTaskSet,
    cells: &[CellAddress],
    samples: u32,
    resolve_cell: impl Fn(CellAddress) -> Option<String>,
) -> Result<BenchmarkPlan, BenchmarkRefusal> {
    if set.tasks.is_empty() {
        return Err(BenchmarkRefusal::NoTasks);
    }
    if cells.is_empty() {
        return Err(BenchmarkRefusal::NoCells);
    }
    if samples == 0 {
        return Err(BenchmarkRefusal::NoSamples);
    }
    for cell in cells {
        match resolve_cell(*cell) {
            None => return Err(BenchmarkRefusal::UnresolvableCell(*cell)),
            Some(kind) if kind != ModelOverride::NAME => {
                return Err(BenchmarkRefusal::CellIsNotAnOverride { cell: *cell, kind });
            }
            Some(_) => {}
        }
    }

    let per_cell = usize::try_from(samples).unwrap_or(usize::MAX);
    let count = set.tasks.len().saturating_mul(cells.len()).saturating_mul(per_cell);
    if count > MAX_BENCHMARK_BLOOMS {
        return Err(BenchmarkRefusal::TooManyBlooms(count));
    }

    let (base, version) = (set.base, set.version());
    let blooms = set
        .tasks
        .iter()
        .flat_map(|task| {
            cells
                .iter()
                .flat_map(move |cell| (0..samples).map(move |sample| planned_bloom(base, version, task, *cell, sample)))
        })
        .collect();

    Ok(BenchmarkPlan { set, blooms })
}

/// Freeze one `(task, cell, sample)` into its spec.
fn planned_bloom(base: Digest, version: Digest, task: &GoldenTask, cell: CellAddress, sample: u32) -> PlannedBloom {
    let workpiece = WorkpieceId(benchmark_workpiece(task.pull_request, cell, sample));

    let mut configs = ConfigRegistry::default();
    configs.insert::<ModelOverride>(cell);

    // The scope revision is the task's own content digest, and nothing writes a
    // `ScopeRevision` row at it. A benchmark member's work order is the landed
    // issue text, carried to the lane as its dispatch description the way every
    // sealed member's is; inventing a scope revision would put a second,
    // divergent copy of that text in the commission store. What the pin has to
    // do is name the order the approval binds, and the task's digest does that.
    let mut member = Membership {
        workpiece: workpiece.clone(),
        scope_revision: digest_of(task),
        configs,
        approval: Evidence { subject: Digest::default(), kind: EvidenceKind::Approval, detail: Digest::default() },
    };
    member.approval = trial_approval(member.subject(), version);

    PlannedBloom {
        spec: BloomDraft { proposals: vec![member], base, ..BloomDraft::default() }.seal(),
        workpiece,
        order: task.order.clone(),
        pull_request: task.pull_request,
        cell,
        sample,
    }
}

/// The workpiece one benchmark bloom's member covers.
///
/// Every cell and every sample gets its own workpiece rather than replaying one
/// name, and that is forced twice over. A sealed spec is addressed by its own
/// content, so two samples of one cell sharing a workpiece would seal to the
/// same bloom id and the second would be admitted as a duplicate of the first —
/// a run of four that measured two. And a workpiece is what a resolution claim
/// and a candidate ref are named by, so concurrent members sharing one would
/// contend for the same ref.
///
/// The cell's short hex is in the name so a bloom is attributable to its
/// selector by inspection, which is what an operator reading a claim ref or a
/// worktree directory has in front of them.
#[must_use]
pub fn benchmark_workpiece(pull_request: u64, cell: CellAddress, sample: u32) -> String {
    format!("bench-{pull_request}-{}-{sample}", short_hex(&cell))
}

/// The `approval` evidence a benchmark member carries.
///
/// A benchmark member is not work anyone approved — it is a replay of work that
/// already landed, in a world that reaches nothing — so its approval is an
/// observation attestation and never an author signature, the same shape the
/// approve gate's auto-tier grant takes and for the same reason: the record is
/// *context* about how the membership came to be admitted, not an instruction
/// anyone gave.
///
/// The tier ladder is deliberately not walked. A golden task's declared surface
/// is the landed diff's, which for real history routinely resolves `human`, and
/// asking an owner to sign a replay of their own merged pull request would make
/// the ladder a formality on the one path where it protects nothing: a trial
/// coordinator's blooms cannot reach the estate's repository, its refs, or its
/// journal. That containment is what earns the exemption, which is why
/// [`BenchmarkRefusal::NotTrialMode`] is checked before anything here runs.
///
/// The supporting record's `parents` pin the member subject and the golden-task
/// set version, so the approval attests exactly which comparison admitted it.
fn trial_approval(subject: Digest, set_version: Digest) -> Evidence {
    let record = Statement {
        words: TRIAL_APPROVAL_WORDS.to_vec(),
        provenance: Provenance::ObservationAttestation(Observation { source: TRIAL_APPROVAL_SOURCE.to_owned() }),
        parents: vec![subject, set_version],
    };
    Evidence { subject, kind: EvidenceKind::Approval, detail: digest_of(&record) }
}

/// The trial-mode gate: a run may plan only against a trial-classed journal.
///
/// One term, not two. Issue #5794 made trial mode a single mode — the class
/// follows the fixture backend, and a stated class that disagrees is a boot
/// fault — so by the time a coordinator is serving requests its class already
/// carries the backend's answer, and a second independent check here could only
/// ever restate it or contradict a fact boot proved.
///
/// # Errors
/// [`BenchmarkRefusal::NotTrialMode`] when the journal is live-classed.
pub fn require_trial_mode(store: StoreClass) -> Result<(), BenchmarkRefusal> {
    match store {
        StoreClass::Trial => Ok(()),
        StoreClass::Live => Err(BenchmarkRefusal::NotTrialMode(StoreClass::Live)),
    }
}
