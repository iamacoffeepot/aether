//! The capability ledger: what each calibrated agent actually did, measured
//! from the journal (ADR-0184 §The capability ledger).
//!
//! A pure read beside [`grade`](crate::study_report::grade), and the same kind
//! of read: the journal plus the study artifacts are the truth, and this folds
//! them into per-`(harness, model, effort) × stage` counts. It ranks nothing. A
//! cell reports what happened and how many observations back it; whether a count
//! is enough to act on is a presentation-side judgement, and the ledger's job is
//! honest counts rather than verdicts.
//!
//! # Folded, not queried
//!
//! [`CalibrationLedger::observe`] is the counterpart to
//! [`Snapshot::apply`](crate::reduce::Snapshot::apply): a caller folds each
//! admitted `(event, decisions)` pair through it in journal order and holds the
//! accumulator beside the snapshot, so boot replay rebuilds the ledger for free
//! and a live admission extends it. [`CalibrationLedger::report`] then renders
//! the measured [`CapabilityLedger`], resolving each attempt's study artifact
//! through the caller's `source` — the seam `grade` already uses, for the reason
//! it uses it: the fold holds digests, and only a caller with an artifact store
//! can turn one into cost columns.
//!
//! The snapshot deliberately is *not* the source. It keeps dispatch *counts* per
//! execution slot ([`BloomRecord::dispatches`](crate::BloomRecord::dispatches),
//! ADR-0180) and the *union* of a member's failed verifiers
//! ([`StageProgress::seen_verify_failures`](crate::StageProgress::seen_verify_failures)),
//! both of which have already lost what a calibration read is about: which stage
//! a cost belongs to, and how many verdicts named one verifier.
//!
//! # The agent is recomputed, never read off the dispatch
//!
//! Every journaled [`Decision::DispatchAttempt`] carries
//! [`Transformation::model`](crate::Transformation::model) as `None`. The
//! reducer authors it that way — it holds digests, not the catalog's resolution
//! — and the host fills it at dispatch, downstream of the journal. So the cell
//! key is recomputed here exactly as the host computes it: the sealed catalog's
//! [`AgentProfile`](crate::AgentProfile) for the stage, which the dispatch carries, with the member's
//! sealed [`ModelOverride`](crate::ModelOverride) resolved over it. Joining on the dispatch's own
//! `model` field would join on `None` for every row and yield an empty table
//! that still passes a naive test.
//!
//! # Only a lane that ran an agent is a cell
//!
//! A cell answers "how did this agent do here", so only the **model lanes**
//! ([`is_model_lane`](crate::is_model_lane)) enter one: Construct, its Refine repair re-entry, the
//! Reconcile lane, and the whole-bloom aggregate review. The mechanical verify
//! fan-out runs a compiler and the integrate / land positions are host-native,
//! so the profile their catalog binding names never ran anything and a row for
//! it would be a measurement of nothing.
//!
//! A failing member Verify still lands in this ledger, attributed to the model
//! lane that *wrote* the candidate the gates refused (ADR-0184: the failure mix
//! is the signature of the agent under measurement, not of the compiler that
//! caught it). That is what makes `verify.suppress` — the quiet-failure identity
//! ADR-0181 made countable — a column about a model rather than an anecdote.
//!
//! # What the ledger cannot see
//!
//! Gate-visible failure only, and it says so: [`LEDGER_CAVEAT`] rides on the
//! rendered [`CapabilityLedger`] rather than sitting in documentation around it.
//! Shared-artifact degradation, the second quiet-failure class, has no gate, so
//! no column here claims to measure it.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::ledger::{SeatDispatch, priced_micro_usd};
use crate::reduce::{Decision, Decisions, Event, Fact, Outcome};
use crate::study_report::StudyReport;
use crate::values::{
    DispatchKey, EvidenceKind, ReasoningEffort, ResolvedConfigs, ResolvedModel, StudyRecord, VerifyFailure,
    VerifyFailureSet,
};

/// The honesty boundary every rendered ledger carries (ADR-0184).
///
/// Part of the projection rather than prose around it: a cell read without it
/// invites "this model is clean" from a table that can only ever say "this
/// model's failures were the ones a gate can see".
pub const LEDGER_CAVEAT: &str = concat!(
    "Measured from the journal, and gate-visible only: these counts are what the verify gates could see. ",
    "Shared-artifact degradation has no gate and no column here. Cost and worker time come from study ",
    "artifacts, so a cell whose samples fall below its attempts is measuring only the attempts whose ",
    "artifact resolved.",
);

/// The verifier-identity vocabulary's width — the per-cell failure counters are
/// one slot per identity, so a ninth identity widens them with the vocabulary.
const IDENTITIES: usize = VerifyFailure::ALL.len();

/// How many failing terminal-Verify verdicts named one verifier identity, in
/// the cell of the model lane that wrote the refused candidate (ADR-0178 /
/// ADR-0181).
///
/// Counted per verdict rather than per member: a member that failed
/// `verify.clippy` on three consecutive laps is three observations of that agent
/// producing that failure, and the member cursor's union
/// ([`StageProgress::seen_verify_failures`](crate::StageProgress::seen_verify_failures))
/// would report it as one.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct VerifierFailures {
    /// The verifier identity that failed.
    pub verifier: VerifyFailure,
    /// How many failing verdicts named it.
    pub verdicts: u64,
}

/// One measured `(harness, model, effort) × stage` cell.
///
/// Every column is a raw count, and the ratios ADR-0184 names are left for the
/// reader to take — `rolls_to_green / resolved_members`,
/// [`cost_per_resolved_member`](Self::cost_per_resolved_member) — because a cell
/// that has already divided cannot say how many observations the quotient rests
/// on.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CapabilityCell {
    /// The agent this cell measures: the sealed catalog's profile for
    /// [`stage`](Self::stage) with the sealed [`ModelOverride`](crate::ModelOverride) resolved over it.
    pub agent: ResolvedModel,
    /// The stage it ran.
    pub stage: StageId,
    /// Dispatches into this cell — the sample count behind the failure and roll
    /// columns.
    pub attempts: u64,
    /// Distinct members this cell ran that hold a resolution claim. An
    /// aggregate-review cell counts its bloom's resolved members, since the fold
    /// it judged is exactly that set.
    pub resolved_members: u64,
    /// Dispatches this cell spent on those resolved members — the numerator of
    /// "rolls to green".
    pub rolls_to_green: u64,
    /// Failing terminal-Verify verdicts against candidates this cell wrote, per
    /// verifier identity, in [`VerifyFailure::ALL`] order. An identity that never
    /// failed is omitted rather than carried as a zero.
    pub failures: Vec<VerifierFailures>,
    /// What this cell's priced attempts cost, in micro-USD, summed off their
    /// study records — already priced against the bloom's sealed
    /// [`PriceTable`](crate::PriceTable), which is where a measured token count
    /// becomes a dollar figure. Unpriced records do not enter this sum.
    pub cost_micro_usd: u64,
    /// Worker time in whole seconds, summed over the same study records. Not
    /// elapsed wall-clock: concurrent members make that a different quantity.
    pub worker_secs: u64,
    /// How many of this cell's attempts a *priced* study record actually
    /// resolved for — the sample count behind
    /// [`cost_micro_usd`](Self::cost_micro_usd), which falls below
    /// [`attempts`](Self::attempts) whenever an artifact could not be read or
    /// was unpriced.
    pub samples: u64,
    /// Study records whose priced column is zero — counted, never averaged, and
    /// never treated as free.
    pub unpriced: u64,
}

impl CapabilityCell {
    /// What one resolved member cost under this agent, or `None` when the cell
    /// resolved none or any of its study records were unpriced.
    ///
    /// `None` is *unmeasured*, never zero — the same distinction
    /// [`PriceTable::price`](crate::PriceTable::price) draws between unpriced and
    /// free. Dividing a sum that includes a missing price row as zero would
    /// bias the per-member figure low exactly when the rates are unknown.
    #[must_use]
    pub fn cost_per_resolved_member(&self) -> Option<u64> {
        (self.resolved_members > 0 && self.unpriced == 0).then(|| self.cost_micro_usd / self.resolved_members)
    }
}

/// The rendered capability ledger: one cell per measured
/// `(harness, model, effort) × stage`, in canonical cell order, plus the honesty
/// boundary the counts are read under.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CapabilityLedger {
    /// The measured cells.
    pub cells: Vec<CapabilityCell>,
    /// [`LEDGER_CAVEAT`], carried on the document so a rendering cannot drop it.
    pub caveat: String,
}

impl Default for CapabilityLedger {
    fn default() -> Self {
        Self { cells: Vec::new(), caveat: String::from(LEDGER_CAVEAT) }
    }
}

/// The whole calibration read (ADR-0184): the measured capability ledger beside
/// the forecast grade of the blooms that produced it.
///
/// The two travel together because they answer one question from opposite ends.
/// The ledger says what an agent did across every bloom that ran it; the grade
/// says whether one bloom cost what it promised. A calibration edit argued from
/// either alone is arguing from half the evidence — and pairing them is what
/// finally gives [`grade`](crate::study_report::grade) a reader.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct CalibrationDocument {
    /// The measured cells.
    pub ledger: CapabilityLedger,
    /// Every bloom's actuals against its sealed forecast.
    pub study: StudyReport,
}

/// The journal-derived accumulator a caller folds admitted events through.
///
/// Holds nothing that is not derivable from the journal, so a replay rebuilds
/// exactly the ledger the live fold produced.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CalibrationLedger {
    /// Every model-lane execution slot that has dispatched, and what it did.
    slots: BTreeMap<SlotId, Slot>,
    /// Each member's latest model lane — the slot a failing Verify verdict is
    /// attributed to, because that lane wrote the candidate the gates refused.
    lanes: BTreeMap<(BloomId, WorkpieceId), SlotId>,
    /// The displayed digest each model-lane slot ran against, first dispatch
    /// wins. The join key from a study record's subject back to a slot: a
    /// mechanical Verify and the Refine that repairs its candidate display the
    /// same digest, and only the model lane spends tokens against it.
    displayed: BTreeMap<(BloomId, Digest), SlotId>,
    /// Admitted study evidence: the bloom, the attempt digest it grades, and the
    /// artifact digest [`CalibrationLedger::report`] resolves.
    studies: Vec<Study>,
    /// Members holding a resolution claim right now.
    claimed: BTreeSet<(BloomId, WorkpieceId)>,
}

/// One execution slot, keyed the way the dispatch ledger keys one (ADR-0180) —
/// which bloom, and whose slot within it.
type SlotId = (BloomId, DispatchKey);

/// What one model-lane slot did.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Slot {
    agent: ResolvedModel,
    stage: StageId,
    dispatches: u64,
    failures: [u64; IDENTITIES],
}

/// One admitted study evidence, unresolved: the fold holds digests, and
/// [`CalibrationLedger::report`] turns them into cost columns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Study {
    bloom: BloomId,
    subject: Digest,
    detail: Digest,
}

impl CalibrationLedger {
    /// Fold one admitted event and its recorded decisions.
    ///
    /// Reads the recorded [`Decision`]s for everything they carry — the
    /// dispatches, the admitted evidence, the claims — so the fold sees what was
    /// decided rather than what the current rules would decide (ADR-0190). Only
    /// the failing-verifier set has no decision to read it off, so that one axis
    /// comes off the fact, and only for a fact the outcome says was admitted.
    ///
    /// `configs` is the configuration content behind the addresses the sealed
    /// registries name, exactly as [`reduce`](crate::reduce::reduce) takes it. A
    /// [`ModelOverride`](crate::ModelOverride) a caller has not fetched leaves that dispatch on its
    /// catalog profile rather than dropping the row: the cell would otherwise
    /// vanish for the blooms most worth measuring, which are the ones that sealed
    /// an override.
    ///
    /// The fact is folded **before** the decisions, and the order is
    /// load-bearing: a verdict describes the candidate a lane already wrote,
    /// while the decisions beside it dispatch the lane that will repair it. Fold
    /// the effects first and every failing Verify is charged to the Refine
    /// re-entry it caused — the first verdict of a bloom lands on a lane that had
    /// not run when the candidate was written, and Construct's column reads
    /// empty however badly it did.
    pub fn observe(&mut self, event: &Event, decisions: &Decisions, configs: &ResolvedConfigs) {
        if let Fact::VerifyFailed { bloom, workpiece, failed_verifiers, .. } = &event.fact
            && !matches!(decisions.outcome, Outcome::VerifyFailedRejected(_))
        {
            self.observe_failure(*bloom, workpiece, *failed_verifiers);
        }
        for effect in &decisions.effects {
            self.observe_effect(effect, configs);
        }
    }

    /// Render the measured ledger, resolving each attempt's study artifact
    /// through `source`.
    ///
    /// `source` returns `None` when the artifact is unavailable, which costs the
    /// cell that attempt's cost and time columns and its sample — and nothing
    /// else, the same posture [`grade`](crate::study_report::grade) takes. A
    /// record that does not grade the attempt it was admitted against, or that
    /// names a different bloom, is skipped the same way: an unbound record is no
    /// more attributable than an unreadable one.
    #[must_use]
    pub fn report(&self, source: impl Fn(&Digest) -> Option<StudyRecord>) -> CapabilityLedger {
        let mut cells: BTreeMap<CellKey, Accumulator> = BTreeMap::new();
        for ((bloom, key), slot) in &self.slots {
            let resolved = self.resolved_members(*bloom, key);
            let cell = cells.entry(CellKey::of(slot)).or_insert_with(|| Accumulator::of(slot));
            cell.attempts = cell.attempts.saturating_add(slot.dispatches);
            cell.resolved_members = cell.resolved_members.saturating_add(resolved);
            if resolved > 0 {
                cell.rolls_to_green = cell.rolls_to_green.saturating_add(slot.dispatches);
            }
            for (identity, verdicts) in VerifyFailure::ALL.into_iter().zip(slot.failures) {
                cell.failures[identity as usize] = cell.failures[identity as usize].saturating_add(verdicts);
            }
        }

        for study in &self.studies {
            let Some(slot) = self.displayed.get(&(study.bloom, study.subject)).and_then(|id| self.slots.get(id)) else {
                continue;
            };
            let bound = |record: &StudyRecord| record.grades(&study.subject) && record.bloom == study.bloom;
            let Some(record) = source(&study.detail).filter(bound) else {
                continue;
            };
            // The loop above gave every slot a cell, so this only ever finds one.
            let cell = cells.entry(CellKey::of(slot)).or_insert_with(|| Accumulator::of(slot));
            cell.worker_millis = cell.worker_millis.saturating_add(record.cost.duration_millis);
            if let Some(cost) = priced_micro_usd(record.cost.cost_micro_usd) {
                cell.cost_micro_usd = cell.cost_micro_usd.saturating_add(cost);
                cell.samples = cell.samples.saturating_add(1);
            } else {
                cell.unpriced = cell.unpriced.saturating_add(1);
            }
        }

        CapabilityLedger {
            cells: cells.into_values().map(Accumulator::into_cell).collect(),
            caveat: String::from(LEDGER_CAVEAT),
        }
    }

    /// How many resolved members one slot's dispatches are answerable for: the
    /// member itself for a per-member slot, and the whole bloom's resolved
    /// membership for a bloom-level one — the aggregate review judged exactly
    /// that fold, so its cost amortizes over exactly those members.
    fn resolved_members(&self, bloom: BloomId, key: &DispatchKey) -> u64 {
        match key {
            DispatchKey::Member { workpiece, .. } => u64::from(self.claimed.contains(&(bloom, workpiece.clone()))),
            DispatchKey::Bloom { .. } => {
                u64::try_from(self.claimed.iter().filter(|(claimed, _)| *claimed == bloom).count()).unwrap_or(u64::MAX)
            }
        }
    }

    /// Fold one recorded decision.
    fn observe_effect(&mut self, effect: &Decision, configs: &ResolvedConfigs) {
        if let Some(dispatched) = SeatDispatch::from_effect(effect) {
            let lane = match &dispatched.key {
                DispatchKey::Member { workpiece, .. } => Some((dispatched.bloom, workpiece.clone())),
                DispatchKey::Bloom { .. } => None,
            };
            if let Some(slot) = self.dispatch(dispatched, configs)
                && let Some(lane) = lane
            {
                self.lanes.insert(lane, slot);
            }
            return;
        }
        match effect {
            Decision::RecordEvidence { bloom, evidence } if evidence.kind == EvidenceKind::StudyRecord => {
                self.studies.push(Study { bloom: *bloom, subject: evidence.subject, detail: evidence.detail });
            }
            Decision::RecordResolution { bloom, claim } | Decision::InheritClaim { bloom, claim } => {
                self.claimed.insert((*bloom, claim.workpiece.clone()));
            }
            Decision::RevokeResolution { bloom, workpiece } => {
                self.claimed.remove(&(*bloom, workpiece.clone()));
            }
            _ => {}
        }
    }

    /// Count one dispatch into its slot, returning that slot when the lane ran
    /// an agent.
    ///
    /// A mechanical lane returns `None` and leaves no slot behind: its catalog
    /// binding still names a profile, and that profile never ran anything.
    fn dispatch(&mut self, dispatched: SeatDispatch<'_>, configs: &ResolvedConfigs) -> Option<SlotId> {
        if !dispatched.is_model_lane() {
            return None;
        }
        // The dispatch's registry is already the member's layered over the
        // bloom's (`ConfigRegistry::layered_over`), so a bloom-wide lookup over it
        // is the member-scoped answer — the same resolution the executor reactor
        // runs at dispatch time.
        let agent = dispatched.agent(configs);
        let SeatDispatch { bloom, key, stage, displayed, .. } = dispatched;

        let id = (bloom, key);
        let slot = self.slots.entry(id.clone()).or_insert_with(|| Slot {
            agent,
            stage,
            dispatches: 0,
            failures: [0; IDENTITIES],
        });
        slot.dispatches = slot.dispatches.saturating_add(1);
        self.displayed.entry((bloom, displayed)).or_insert_with(|| id.clone());
        Some(id)
    }

    /// Attribute one failing terminal-Verify verdict to the model lane that
    /// wrote the candidate it refused.
    ///
    /// A member with no model lane on record is one whose candidate this
    /// coordinator never dispatched — an operator-supplied repair (#4957), whose
    /// failures belong to no agent — so its verdict is counted nowhere rather
    /// than charged to whichever lane happened to run last.
    fn observe_failure(&mut self, bloom: BloomId, workpiece: &WorkpieceId, failed: VerifyFailureSet) {
        let Some(id) = self.lanes.get(&(bloom, workpiece.clone())).cloned() else {
            return;
        };
        let Some(slot) = self.slots.get_mut(&id) else {
            return;
        };
        for identity in failed.iter() {
            slot.failures[identity as usize] = slot.failures[identity as usize].saturating_add(1);
        }
    }
}

/// The cell one slot aggregates into. The harness rides as its runner-facing
/// name so the key is orderable without giving [`Harness`](crate::Harness) an
/// ordering it has no meaning for.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct CellKey {
    harness: &'static str,
    model: String,
    effort: ReasoningEffort,
    stage: StageId,
}

impl CellKey {
    fn of(slot: &Slot) -> Self {
        Self {
            harness: slot.agent.harness.as_str(),
            model: slot.agent.model.clone(),
            effort: slot.agent.effort,
            stage: slot.stage,
        }
    }
}

/// One cell mid-fold: the emitted columns, plus worker time still in millis so
/// the seconds conversion happens once over the whole sum rather than per record,
/// and the failure counters still one slot per identity so the emitted vector can
/// drop the identities that never failed.
struct Accumulator {
    agent: ResolvedModel,
    stage: StageId,
    attempts: u64,
    resolved_members: u64,
    rolls_to_green: u64,
    failures: [u64; IDENTITIES],
    cost_micro_usd: u64,
    worker_millis: u64,
    samples: u64,
    unpriced: u64,
}

impl Accumulator {
    fn of(slot: &Slot) -> Self {
        Self {
            agent: slot.agent.clone(),
            stage: slot.stage,
            attempts: 0,
            resolved_members: 0,
            rolls_to_green: 0,
            failures: [0; IDENTITIES],
            cost_micro_usd: 0,
            worker_millis: 0,
            samples: 0,
            unpriced: 0,
        }
    }

    fn into_cell(self) -> CapabilityCell {
        CapabilityCell {
            agent: self.agent,
            stage: self.stage,
            attempts: self.attempts,
            resolved_members: self.resolved_members,
            rolls_to_green: self.rolls_to_green,
            failures: VerifyFailure::ALL
                .into_iter()
                .zip(self.failures)
                .filter(|(_, verdicts)| *verdicts > 0)
                .map(|(verifier, verdicts)| VerifierFailures { verifier, verdicts })
                .collect(),
            cost_micro_usd: self.cost_micro_usd,
            worker_secs: self.worker_millis / 1000,
            samples: self.samples,
            unpriced: self.unpriced,
        }
    }
}
