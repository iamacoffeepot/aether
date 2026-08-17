//! The rebuildable projection the reducer reads, and the fold that evolves it.
//!
//! [`reduce`](super::reduce) *decides* against a [`Snapshot`]; [`Snapshot::apply`]
//! *evolves* one into the next. A live admission runs the two in sequence;
//! journal replay folds recorded decisions through [`Snapshot::apply`] alone
//! (ADR-0190) — the split that keeps the decision pure, the evolution
//! mechanical, and history immune to rule changes.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::{Decision, Decisions, Event, Fact, Outcome};
use crate::digest::Digest;
use crate::ids::{BloomId, IdempotencyKey, StageId, WorkpieceId};
use crate::values::{
    Adjudication, BloomSpec, CandidateRef, CompositionFinding, ConfigScopes, DispatchKey, Evidence, EvidenceKind,
    MemberDependency, OperatorHold, OperatorRepair, OrphanClaimReleaseRecord, ResolutionClaim, ResolvedConfigs,
    SpendQuiesce, StageCatalog, VerifiedTree, VerifyFailureSet, VerifyGateSet, VerifyProof, VerifyReuse, Wedge,
};

/// The rebuildable projection state the reducer reads (ADR-0149 §The control
/// core). Holds nothing that is not derivable from the journal.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// The current mainline head — landing is a compare-and-swap against it.
    pub mainline: Digest,
    /// The head the source last reported, which is not always one mainline was
    /// free to move onto (#4709).
    ///
    /// Mainline may only advance when nothing is in flight, so a repository that
    /// moves during a bloom leaves the two apart. This is the head a supersession
    /// may rebase onto — the only base other than current mainline a successor
    /// may take, which is what stops a caller from naming the compare-and-swap
    /// anchor whatever it likes.
    #[serde(default)]
    pub observed: Digest,
    /// The active-membership map: which bloom each workpiece is claimed by.
    /// The at-most-one-active-bloom-per-workpiece constraint is this map's
    /// key uniqueness.
    pub active: BTreeMap<WorkpieceId, BloomId>,
    /// Every sealed bloom the projection knows, by id.
    pub blooms: BTreeMap<BloomId, BloomRecord>,
    /// The idempotency keys already applied — a replayed key is a no-op.
    pub seen: BTreeSet<IdempotencyKey>,
    /// The authorized orphan-claim releases this instance has admitted, keyed by
    /// request digest (ADR-0179).
    ///
    /// Journal-derived like the rest of the projection, and the reason a repeated
    /// request is idempotent rather than a second release: the reducer reads this
    /// map before enqueuing, so a resubmitted digest returns the recorded state
    /// and emits no effect. The status route reads it too — pending until the
    /// reactor's completion folds a terminal result in.
    #[serde(default)]
    pub orphan_releases: BTreeMap<Digest, OrphanClaimReleaseRecord>,
    /// A dead construct lane's newest partial capture, keyed by bloom then
    /// workpiece. Folded from a failing [`Fact::AttemptCompleted`] that still
    /// carried a [`CandidateRef`] — the fact already has the field, and the
    /// cursor is not the home: putting a checkpoint there would adopt the
    /// partial tree as a finished candidate. #4994 reads this for checkout
    /// only — the retry checks out the capture commit, the cursor stays empty.
    ///
    /// Snapshot-level rather than a [`BloomRecord`] field so the fold can see
    /// the fact without a new [`Decision`] (the journal's decisions graph is
    /// wire-frozen) and so every existing `BloomRecord { … }` literal stays
    /// compiling. `#[serde(default)]` is the `orphan_releases` precedent.
    #[serde(default)]
    pub member_checkpoints: BTreeMap<BloomId, BTreeMap<WorkpieceId, CandidateRef>>,
    /// The spend-quiesce marker the last crossing recorded (ADR-0192).
    ///
    /// Snapshot-level rather than per-bloom: the door that closed is the
    /// fleet's, and `/view` renders one marker rather than one per bloom.
    /// `#[serde(default)]` is the `observed` / `orphan_releases` precedent —
    /// a JSON reader that predates the field still decodes. The positional
    /// wire never sees this field: a journaled
    /// [`Decision::RecordSpendQuiesce`] folds it back on replay.
    #[serde(default)]
    pub spend_quiesce: Option<SpendQuiesce>,
    /// Per-member machinery-fault series, keyed by bloom then workpiece
    /// (ADR-0195). Folded from [`Decision::RecordMemberMachinery`].
    ///
    /// Counted apart from [`StageProgress::attempts`] and
    /// [`StageProgress::repair_rolls`] because an executor that could not run
    /// judged no candidate. Snapshot-level rather than a [`BloomRecord`] field
    /// so every existing `BloomRecord { … }` literal stays compiling, the
    /// `member_checkpoints` precedent. `#[serde(default)]` is the same
    /// JSON-reader rescue.
    #[serde(default)]
    pub member_machinery: BTreeMap<BloomId, BTreeMap<WorkpieceId, MemberMachineryFault>>,
}

impl Snapshot {
    /// The genesis mainline base digest — the well-known head every fresh
    /// snapshot starts at (via [`Default`]) and every bloom seals against before
    /// any land. The boot genesis reconcile seeds this exact digest ↔ the
    /// repository's real head commit sha, so the first land's base
    /// reverse-resolves to it instead of faulting `UnresolvedCorrespondence`
    /// (issue #3615). Exposed as a named constant so the reconcile and the
    /// control core address the same genesis base.
    pub const GENESIS_MAINLINE: Digest = Digest::from_bytes([0; 32]);

    /// The newest construct checkpoint recorded for `workpiece` in `bloom`.
    #[must_use]
    pub fn member_checkpoint(&self, bloom: &BloomId, workpiece: &WorkpieceId) -> Option<CandidateRef> {
        self.member_checkpoints.get(bloom)?.get(workpiece).copied()
    }

    /// The machinery-fault series recorded for `workpiece` in `bloom`.
    #[must_use]
    pub fn member_machinery(&self, bloom: &BloomId, workpiece: &WorkpieceId) -> Option<MemberMachineryFault> {
        self.member_machinery.get(bloom)?.get(workpiece).copied()
    }
}

/// A member's run of executor faults against one stage (ADR-0195) — how many
/// times the dispatched gate could not judge that member, and what the latest
/// of them reported.
///
/// Keyed to the stage rather than to the bloom: leaving the stage begins a
/// fresh series, so a member that advanced after an outage is not carrying
/// the previous stage's spent retries.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MemberMachineryFault {
    /// The stage the faults are against.
    pub stage: StageId,
    /// How many faults this stage has taken, the latest one included.
    pub rolls: u32,
    /// The latest fault report's artifact digest.
    pub evidence: Digest,
}

/// The per-bloom projection record.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BloomRecord {
    /// The sealed, immutable spec.
    pub spec: BloomSpec,
    /// The stage catalog this bloom runs, resolved once at seal (ADR-0174).
    ///
    /// Journal-derived: a newly sealed bloom records it as
    /// [`Decision::RecordStageCatalog`] and the fold copies that value. A
    /// pre-existing row that never carried the effect keeps the compiled-line
    /// fallback at record construction (`StageCatalog::sealed_in`) — the
    /// incident class that fold closes, a no-catalog bloom whose recorded
    /// catalog otherwise moved with the binary.
    pub stage_catalog: StageCatalog,
    /// The bloom's lifecycle status.
    pub status: BloomStatus,
    /// The resolution claims accumulated by integration (and inherited from a
    /// predecessor on supersession), keyed by workpiece so a re-integration
    /// overwrites the stale candidate rather than accumulating beside it —
    /// resolve reads exactly one current claim per member (ADR-0149 §The bloom).
    pub claims: BTreeMap<WorkpieceId, ResolutionClaim>,
    /// The non-integrating evidence admitted against this bloom, in admission
    /// order (ADR-0151). Journal-derived, replay-rebuilt — a study record,
    /// verification result, or review finding is recorded here without advancing
    /// any member toward resolution. A resolution claim never lands here; it
    /// enters through `Fact::Integrate` and lives in [`claims`](Self::claims).
    pub evidence: Vec<Evidence>,
    /// The open pending-decision holds — the digests of the [`Question`](crate::Question)
    /// artifacts a parked attempt raised that no adopting answer has released
    /// yet (ADR-0151). Derived member state, folded from the evidence log: an
    /// admitted [`EvidenceKind::Question`] inserts its `detail` digest here, and
    /// an adopting answer removes it. A non-empty set blocks the bloom from
    /// resolving; the [`Question`](crate::Question)'s own `workpiece` (resolved from the digest)
    /// is what binds a hold to its member in the outward view. Journal-derived,
    /// replay-rebuilt like the rest of the record.
    pub holds: BTreeSet<Digest>,
    /// The per-member stage cursor (ADR-0149 §The line): for each member
    /// workpiece, the [`StageId`] it currently sits at plus its attempt count
    /// against that stage's `retry_budget`. Rebuilt from the journal like the rest
    /// of the record — a seal seeds each *ready* member at the entry stage
    /// ([`StageCatalog::entry_stage`](crate::StageCatalog::entry_stage)); dependents stay out until
    /// every incoming edge has a resolution claim (ADR-0196). A passing
    /// attempt advances the cursor, and a failing one bumps the attempt
    /// count in place — so replay reconstructs in-flight line position. A
    /// member drops out of the map only implicitly (it never does in V1; the
    /// record is discarded whole on supersession).
    pub progress: BTreeMap<WorkpieceId, StageProgress>,
    /// The members that have wedged, keyed by workpiece (ADR-0149 §The line).
    /// A member lands here when it exhausts a stage's `retry_budget` and stops
    /// dispatching; it leaves when its cursor moves again, since anything that
    /// re-dispatches the member — an adopting answer's redispatch, a stage
    /// advance — means it is no longer stuck. Journal-derived and
    /// replay-rebuilt like the rest of the record.
    ///
    /// Recorded rather than derived from [`progress`](Self::progress): a member
    /// sitting at `Verify` one roll below the ceiling has the same cursor
    /// whether its next verdict is still pending or has already come back
    /// failing, so the cursor cannot distinguish mid-flight from wedged.
    pub wedged: BTreeMap<WorkpieceId, Wedge>,
    /// How many times each execution slot has been dispatched — the bloom's
    /// dispatch ledger, and the source of the study grade's retry axis
    /// (ADR-0180). Journal-derived and replay-rebuilt like the rest of the
    /// record: every dispatch decision the reducer emits increments its own
    /// [`DispatchKey`], so replaying the same facts rebuilds the same counts and
    /// a bloom journaled before the ledger existed gets one for free.
    ///
    /// A record of what was *spent*, never headroom that may be handed back.
    /// [`StageProgress::attempts`] resets at each stage advance, a grant lowers
    /// `attempts` / `repair_rolls` to leave the granted allowance, and an
    /// adopted answer zeroes [`aggregate_rolls`](Self::aggregate_rolls) — all
    /// correct as budget cursors and all wrong as history, which is why the
    /// ledger sits beside them rather than being read out of one. Nothing inside
    /// a bloom's life clears it; only a successor, being a distinct id with its
    /// own record, starts a fresh one.
    pub dispatches: BTreeMap<DispatchKey, u32>,
    /// The integration fold's output held while the bloom's aggregate review
    /// runs (ADR-0153): set when [`Fact::Resolve`] verifies the claim set and
    /// dispatches the review, consumed by a passing
    /// [`Fact::AggregateReviewCompleted`] (which resolves from it), and
    /// cleared when a failing verdict re-opens members — the fold is stale the
    /// moment any member's claim is revoked. `None` outside that window.
    pub integration: Option<FoldedIntegration>,
    /// How many aggregate-review verdicts this bloom has consumed — the
    /// two-pass ceiling's cursor (ADR-0153). `0` before the first verdict;
    /// after a first failing verdict the re-fold dispatches the delta-confirm,
    /// and a second failing verdict parks the bloom to the owner: the machine
    /// never buys a third roll. An adopting answer resets it — the owner
    /// buying a whole fresh cycle.
    pub aggregate_rolls: u32,
    /// How many aggregate-*verify* verdicts this bloom has consumed, against
    /// `AggregateVerify`'s own catalog budget.
    ///
    /// Counted apart from [`aggregate_rolls`](Self::aggregate_rolls) because
    /// the two aggregate gates are separate stages holding separate budgets: a
    /// fold that burned the compiler's rolls has not touched the critic's, and
    /// one shared counter would let either gate spend the other's.
    pub aggregate_verify_rolls: u32,
    /// How many landing attempts this bloom has consumed (#4689), against the
    /// `Land` binding's catalog retry budget.
    ///
    /// `0` until a landing is refused. Each rejection spends one; below the
    /// budget the bloom un-resolves and its members repair, at the budget it
    /// parks to the owner. This is what keeps a persistently red landing branch
    /// from becoming an unbounded dispatch loop.
    pub landing_rolls: u32,
    /// The head the bloom is landing, held while it is `Resolved` (#4689).
    ///
    /// A landing rejection has to bind the head it judged, and the fold that
    /// produced it is cleared on resolve — so without this the reducer would
    /// have nothing to check a rejection's subject against, and a stale
    /// rejection from a superseded landing could re-open members under a newer
    /// one. Set by [`Decision::SetResolved`], cleared when the bloom leaves the
    /// resolved state.
    pub resolved_head: Option<Digest>,
    /// The bloom-scope park (ADR-0153): the pending-decision question raised
    /// when the delta-confirm still failed at the two-pass ceiling — the
    /// failing review's record artifact digest, held in
    /// [`holds`](Self::holds) like any ADR-0151 question while the owner
    /// decides. An adopting answer that names it releases the hold and re-arms
    /// the review cycle instead of re-dispatching a member stage. `None` when
    /// the bloom is not parked.
    pub review_park: Option<Digest>,
    /// The verify memo: every tree this bloom has a green verify verdict for,
    /// keyed by the tree and the gate set that proved it (#4891).
    ///
    /// A verify reads a checked-out tree and nothing else, so its verdict is a
    /// fact about content — which makes it answerable from a record instead of a
    /// second run whenever a later position targets the same
    /// [`VerifiedTree`]. The gate set is half the key, so a proof cannot answer
    /// for a vocabulary or a lane it was not collected under.
    ///
    /// Scoped to the bloom rather than the snapshot: the memo's whole value is
    /// intra-bloom (the fold of a single member, a repair lap that left the tree
    /// unchanged), and per-bloom scope means a superseded record's proofs retire
    /// with it and a bloom that sealed its own catalog can never read one proved
    /// under another's. Journal-derived and replay-rebuilt like the rest of the
    /// record.
    #[serde(default)]
    pub verify_proofs: BTreeMap<VerifiedTree, VerifyProof>,
    /// Every memo hit this bloom took, in the order they happened (#4891) — the
    /// receipt trail that keeps a pass-by-identity honest.
    ///
    /// A reused proof is still a claim about the bloom's work, so it is recorded
    /// rather than merely decided: without it the journal would show a stage
    /// that passed with no verdict of its own and no way to tell a reuse from a
    /// skipped gate. Each entry names the position that passed and the exact
    /// prior verdict it stood on, which is also what lets the calibration ledger
    /// count the worker-seconds the hit reclaimed.
    #[serde(default)]
    pub verify_reuses: Vec<VerifyReuse>,
    /// The aggregate-review executor faults this bloom has taken on the fold it
    /// currently holds (ADR-0176); `None` until one arrives.
    ///
    /// Counted apart from every other ledger on the record on purpose. An
    /// executor that could not run judged no candidate, so charging
    /// [`aggregate_rolls`](Self::aggregate_rolls) would spend the critic's
    /// budget on a verdict it never gave, and charging a member's
    /// [`repair_rolls`](StageProgress::repair_rolls) would spend a candidate's
    /// repair budget on a host outage. Journal-derived and replay-rebuilt like
    /// the rest of the record: folded from the evidence log, so it survives a
    /// restart and a redispatch of the same fold continues the series.
    pub aggregate_fault: Option<AggregateReviewFault>,
    /// The composition workpiece's findings channel (ADR-0191 §4 / §5): every
    /// verdict that refused the composed tree, in admission order.
    ///
    /// The composition is a subject like a member, so a defect discovered in it
    /// has an owner and a place to be recorded. Two readers: the re-weave, which
    /// is directed by the latest finding rather than re-rolling blind, and the
    /// operator (or the study that files the follow-up work), for the class of
    /// finding that is genuinely about a member's code — members are immutable
    /// after review, so that observation is filed forward instead of re-opening
    /// finished work. Journal-derived and replay-rebuilt like the rest of the
    /// record; defaulted so a journal written before the channel existed still
    /// decodes.
    #[serde(default)]
    pub composition_findings: Vec<CompositionFinding>,
    /// The operator adjudications recorded against this bloom (#4957), in
    /// admission order — which composition findings were closed, how, why, and
    /// by whom.
    ///
    /// The closure half of [`composition_findings`](Self::composition_findings):
    /// a finding is closed by being named here, never by being edited or
    /// dropped, so the record still carries the verdict beside the decision to
    /// waive it. [`open_composition_findings`](Self::open_composition_findings)
    /// is what reads the two together. Journal-derived and replay-rebuilt;
    /// defaulted so a journal written before the override existed still decodes.
    #[serde(default)]
    pub adjudications: Vec<Adjudication>,
    /// The operator-supplied repair candidates recorded against this bloom
    /// (#4957), in admission order.
    ///
    /// Recorded for the reason a [`Wedge`] is: the dispatch a repair emits is an
    /// ordinary `Verify` dispatch, indistinguishable from a lane's, so without
    /// this row nothing in the projection would say a person wrote the candidate
    /// the gates then judged. Journal-derived and replay-rebuilt; defaulted for
    /// the same reason as [`adjudications`](Self::adjudications).
    #[serde(default)]
    pub operator_repairs: Vec<OperatorRepair>,
    /// The operator hold currently on this bloom (#4976), or `None` while it
    /// dispatches normally.
    ///
    /// The brake for a bloom that looks wrong but has not stopped: while it is
    /// set the reducer emits no [`Decision::DispatchAttempt`],
    /// [`Decision::DispatchAggregateVerify`], or
    /// [`Decision::DispatchAggregateReview`] for this bloom, and every other
    /// fact — lane completions, verify results, fold outcomes — reduces exactly
    /// as it always did, so the work already running lands in the journal
    /// instead of being stranded by a killed coordinator.
    ///
    /// Bloom-level and flat: one hold, no per-member scope, no priority, no
    /// timed resume. It composes with the review park (`review_park`) rather than
    /// standing in for it — holding a parked bloom leaves the park where it was,
    /// and releasing one does not answer it. Journal-derived and replay-rebuilt;
    /// defaulted so a journal written before the brake existed still decodes.
    #[serde(default)]
    pub operator_hold: Option<OperatorHold>,
    /// The workpieces this bloom owes a dispatch, because the hold swallowed the
    /// one their cursor move earned (#4976).
    ///
    /// Recorded for the reason [`wedged`](Self::wedged) is, and the reason is the
    /// same sentence: a workpiece whose worker is still running and one whose
    /// dispatch was swallowed sit at the same cursor. A release that read only
    /// cursors would either strand the swallowed dispatch or put a second worker
    /// on a running one, so what is derived at release time is the dispatch (from
    /// the cursor, the catalog, and the configuration as they stand then) while
    /// the *set* is recorded as it happens.
    ///
    /// A workpiece leaves it implicitly, when the dispatch it names actually goes
    /// out — the same shape as leaving `wedged` on a cursor move, and the reason
    /// the release needs no clearing decision of its own. Journal-derived and
    /// replay-rebuilt; defaulted like its sibling.
    #[serde(default)]
    pub deferred_dispatches: BTreeSet<WorkpieceId>,
    /// The aggregate stages this bloom owes a dispatch, because the hold
    /// swallowed the verify or review their fold just earned (#5100).
    ///
    /// The same argument as [`deferred_dispatches`](Self::deferred_dispatches)
    /// at bloom scope: a fold whose aggregate is still in flight and one whose
    /// dispatch was withheld sit at the same integration. A release that read
    /// only the held fold would either strand the swallowed gate or put a
    /// second worker on a running one. The dispatch is re-derived from the
    /// fold, catalog, and roll as they stand then; the *set* is recorded as it
    /// happens. A stage leaves it when the dispatch it names actually goes
    /// out. Journal-derived and replay-rebuilt; defaulted like its sibling.
    #[serde(default)]
    pub deferred_aggregates: BTreeSet<StageId>,
    /// The door-resolved member-dependency graph (ADR-0196).
    ///
    /// Journal-derived: a seal records it as
    /// [`Decision::RecordMemberDependencies`] and the fold copies that value.
    /// Empty is the edgeless degenerate case — today's bloom. Defaulted so a
    /// journal written before the graph existed still decodes.
    #[serde(default)]
    pub dependencies: Vec<MemberDependency>,
    /// Members held at Verify because the host could not run the gates
    /// (#5020) — keyed by workpiece, carrying the preflight findings.
    ///
    /// A `verify.preflight`-only verdict writes one; the coordinator cadence
    /// clears it when it re-probes. Distinct from
    /// [`operator_hold`](Self::operator_hold): this is a per-member host
    /// condition, not a bloom-wide brake. Journal-derived and replay-rebuilt;
    /// defaulted so a journal written before the hold existed still decodes.
    #[serde(default)]
    pub host_faults: BTreeMap<WorkpieceId, HostFaultHold>,
    /// The capture-commit vehicle recorded for each resolved member (#5079).
    ///
    /// Tree identity stays on [`claims`](Self::claims); this map is the host
    /// checkout a dependent splice must name so construct is parented to the
    /// real capture rather than a parentless wrapper of the claimed tree.
    /// Journal-derived from [`Decision::RecordCandidateVehicle`] and
    /// replay-rebuilt. `#[serde(default)]` is the `host_faults` precedent
    /// for a JSON reader that predates the field.
    #[serde(default)]
    pub vehicles: BTreeMap<WorkpieceId, CandidateRef>,
    /// If superseded, the successor that replaced this bloom.
    pub superseded_by: Option<BloomId>,
}

/// A member held at Verify because the host could not run the gates (#5020).
///
/// Projection state, not a journal row: the durable write is
/// [`Decision::RecordHostFault`]. The evidence digest keys the cadence
/// resume so two ticks against the same hold collapse, and a later miss
/// (a new evidence artifact) is a new key.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct HostFaultHold {
    /// The preflight findings — the missing tools, listed verbatim.
    pub findings: String,
    /// The preflight evidence digest the hold was recorded against.
    pub evidence: Digest,
}

/// A bloom's run of aggregate-review executor faults against one held fold
/// (ADR-0176) — how many times the dispatched review could not judge that exact
/// tree, and what the latest of them reported.
///
/// Keyed to the subject rather than to the bloom: a different fold is a
/// different subject and begins its own series, so a bloom that re-integrated
/// after an outage is not carrying the previous fold's spent retries.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AggregateReviewFault {
    /// The fold tree the faults are against — the held integration's tree.
    pub subject: Digest,
    /// How many faults this subject has taken, the latest one included.
    pub rolls: u32,
    /// The latest fault report's artifact digest.
    pub evidence: Digest,
}

impl AggregateReviewFault {
    /// The series a fault on `subject` reporting `evidence` produces from
    /// `previous`: one more roll when it names the same subject, a fresh series
    /// at one when it does not.
    ///
    /// The single place the rule lives, because the reducer reads it to decide
    /// redispatch-or-wedge *before* the fold writes it, and two copies of a
    /// counting rule drift into a ceiling that decides against a count the
    /// record never reaches.
    #[must_use]
    pub fn next(previous: Option<&Self>, subject: Digest, evidence: Digest) -> Self {
        let rolls = previous.filter(|fault| fault.subject == subject).map_or(0, |fault| fault.rolls);
        Self { subject, rolls: rolls.saturating_add(1), evidence }
    }
}

/// The integration fold's output — the axes [`Fact::Resolve`] carries, held on
/// the bloom record while the whole-bloom aggregate review judges the
/// integrated head (ADR-0153).
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct FoldedIntegration {
    /// The final integrated tree digest — the subject the review evidence binds.
    pub tree: Digest,
    /// The landable head commit's digest — the checkout the critic reviews and
    /// the head a subsequent land swaps mainline onto.
    pub head: Digest,
    /// The integration lineage.
    pub lineage: Vec<Digest>,
}

/// One member's position in the per-member line: the stage it currently sits at
/// and how many attempts it has made against that stage (ADR-0149 §The line). The
/// attempt count is capped at the stage's `retry_budget`; a member at
/// `attempts == retry_budget` whose latest attempt failed is wedged — it stops
/// dispatching rather than looping.
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct StageProgress {
    /// The stage the member currently sits at.
    pub stage: StageId,
    /// The number of attempts dispatched against this stage so far (`1` on the
    /// first dispatch). Never exceeds the stage's `retry_budget`.
    pub attempts: u32,
    /// The candidate the member is currently at (ADR-0152) — written when a
    /// passing completion carries one, carried forward otherwise, `None` until
    /// the first capture (a member still constructing against the bare sealed
    /// base). Later dispatches re-target from it: evidence binds `tree`, the
    /// worker checks out `checkout`.
    pub candidate: Option<CandidateRef>,
    /// How many repeated terminal-Verify verdicts this member has consumed
    /// (ADR-0178) — a wholly novel failure set costs no roll, while any
    /// intersection with `seen_verify_failures` costs exactly one. The counter
    /// is carried across Refine re-entry because `attempts` resets on each stage
    /// advance. A repeat at the Verify retry budget wedges instead of re-entering.
    pub repair_rolls: u32,
    /// Every verifier identity this member has failed in an admitted terminal
    /// Verify verdict (ADR-0178). Carried across repair transitions and grants;
    /// a successor gets a fresh cursor and therefore an empty set.
    pub seen_verify_failures: VerifyFailureSet,
    /// The landable head of the folded tree this member is reconciling onto
    /// (ADR-0189) — the *fold round* it is in. Set by [`Fact::FoldConflict`]
    /// and consumed as the Reconcile lane's checkout.
    ///
    /// It outlives the Reconcile stage on purpose (#4952). A member is in the
    /// round until its candidate actually folds or the fold moves under it, and
    /// a reconciled candidate has done neither at the moment the lane passes —
    /// it still has to verify and re-integrate before the fold sees it. Wiping
    /// the round at the stage boundary left the next collision unable to say
    /// whether the member was colliding with the tree it had already reconciled
    /// onto (its own inability, which the Reconcile budget guards) or with a
    /// tree a sibling's reconcile had moved underneath it (which costs the
    /// member nothing). Carried across advances, retries, repair re-entry, and
    /// grants for that reason; `None` until the member's first collision.
    /// Defaulted so a journal written before the field existed still decodes.
    #[serde(default)]
    pub fold_checkpoint: Option<Digest>,
    /// The `FoldConflict` evidence detail to attach if Reconcile exhausts.
    /// Defaulted for the same reason as [`fold_checkpoint`](Self::fold_checkpoint).
    #[serde(default)]
    pub fold_conflict_evidence: Option<Digest>,
}

/// A bloom's position in the one-way lifecycle.
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum BloomStatus {
    /// Sealed and active — the single unlanded bloom (V1 permits one per
    /// mainline).
    Sealed,
    /// Resolved: its one artifact exists, awaiting land.
    Resolved,
    /// Landed: mainline moved onto it.
    Landed,
    /// Superseded by a successor that inherited its claims.
    Superseded,
}

/// The spec a successful seal or supersede just admitted, so the fold can
/// open the bloom record before membership claims land on it.
fn admitted_spec<'a>(fact: &'a Fact, outcome: &Outcome) -> Option<(&'a BloomSpec, BloomId)> {
    match (fact, outcome) {
        (Fact::Seal(spec), Outcome::Sealed(id)) => Some((spec, *id)),
        (Fact::Supersede { successor, .. }, Outcome::Superseded { successor: id, .. }) => Some((successor, *id)),
        (Fact::GraphSeal { spec, .. }, Outcome::Sealed(id) | Outcome::Superseded { successor: id, .. }) => {
            Some((spec, *id))
        }
        _ => None,
    }
}

impl Snapshot {
    /// A fresh snapshot at a mainline base, with no blooms.
    #[must_use]
    pub fn new(mainline: Digest) -> Self {
        Self { mainline, ..Self::default() }
    }

    /// Evolve the snapshot by applying a decided event's effects — the
    /// mechanical counterpart to [`reduce`](crate::reduce::reduce)'s decision. Registers a newly
    /// sealed or superseding bloom from the fact, folds every [`Decision`],
    /// and records the idempotency key. A duplicate or rejected outcome
    /// still records the key (so a replay stays a no-op) but changes nothing
    /// else.
    #[must_use]
    pub fn apply(&self, event: &Event, decisions: &Decisions, configs: &ResolvedConfigs) -> Self {
        let mut next = self.clone();
        next.seen.insert(event.idempotency_key.clone());
        // Register the bloom the fact seals, before its membership claims
        // land, so the claim/inherit effects have a record to attach to.
        if let Some((spec, id)) = admitted_spec(&event.fact, &decisions.outcome) {
            next.blooms.insert(id, BloomRecord::sealed(spec.clone(), configs));
        }
        for effect in &decisions.effects {
            next.apply_effect(effect);
        }
        next.record_construct_checkpoint(event, decisions);
        next
    }

    /// Record a failing construct's capture as the member's newest checkpoint.
    ///
    /// Keyed on `passed: false` and `StageId::Construct`: a passing capture is
    /// a candidate (the cursor adopts it), and a failing Refine still discards
    /// its capture so a tree that failed its own gate is not a resume seed.
    /// Gated on a retried or wedged outcome so a refused completion — unknown
    /// bloom, stage mismatch — cannot plant a checkpoint the reducer rejected.
    /// Raises no hold and does not write the stage cursor.
    fn record_construct_checkpoint(&mut self, event: &Event, decisions: &Decisions) {
        if !matches!(decisions.outcome, Outcome::AttemptRetried { .. } | Outcome::AttemptWedged { .. }) {
            return;
        }
        let Fact::AttemptCompleted {
            bloom,
            workpiece,
            stage: StageId::Construct,
            passed: false,
            candidate: Some(checkpoint),
            ..
        } = &event.fact
        else {
            return;
        };
        self.member_checkpoints.entry(*bloom).or_default().insert(workpiece.clone(), *checkpoint);
    }

    fn apply_graph_effect(&mut self, effect: &Decision) {
        if let Decision::RecordMemberDependencies { bloom, edges } = effect
            && let Some(record) = self.blooms.get_mut(bloom)
        {
            record.dependencies.clone_from(edges);
        }
    }

    fn apply_effect(&mut self, effect: &Decision) {
        match effect {
            Decision::ClaimMembership { workpiece, bloom } => {
                self.active.insert(workpiece.clone(), *bloom);
            }
            Decision::ReleaseMembership { workpiece, bloom } => {
                if self.active.get(workpiece) == Some(bloom) {
                    self.active.remove(workpiece);
                }
            }
            Decision::InheritClaim { .. } | Decision::RecordResolution { .. } => self.apply_claim_effect(effect),
            Decision::RecordEvidence { bloom, evidence } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.record_evidence(evidence);
                }
            }
            Decision::ReleaseHold { .. } | Decision::RecordReviewPark { .. } => self.apply_hold_effect(effect),
            Decision::AdvanceStage { .. } => self.apply_advance_stage(effect),
            Decision::RecordWedge { bloom, workpiece, wedge } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.wedged.insert(workpiece.clone(), *wedge);
                    // A wedged workpiece is owed nothing (#4976). It spent its
                    // budget, and a wedge is the reducer's statement that it
                    // stops dispatching — so a release must not hand it the lap
                    // the hold happened to be sitting on, which would make the
                    // brake a retry grant wearing a different name. The doors
                    // that do hand a wedged workpiece attempts (a grant, an
                    // operator repair) move its cursor, and that is what puts it
                    // back in the line.
                    record.deferred_dispatches.remove(workpiece);
                }
            }
            Decision::DispatchAttempt { .. }
            | Decision::DispatchIntegration { .. }
            | Decision::DispatchAggregateVerify { .. }
            | Decision::DispatchAggregateReview { .. }
            | Decision::DispatchLand { .. } => self.apply_dispatch_effect(effect),
            // Wholly snapshot-inert, like EmitReceipt's outbox row: a re-dispatch
            // replays a held work order host-side under a fresh nonce rather than
            // deciding a dispatch, so ADR-0151's "parking consumes no retry" holds
            // structurally — the ledger never sees it. Its hold release rides
            // ReleaseHold. An orphan-claim release is inert for a different
            // reason: it dispatches no member work at all, so it counts against
            // no bloom's retry ledger — its whole state is the record below.
            Decision::RedispatchStage { .. } | Decision::DispatchOrphanClaimRelease { .. } => {}
            Decision::RecordOrphanClaimRelease { request, target, completion } => {
                // Opening the record and completing it write the same entry, so
                // the completion overwrites rather than inserting beside — a
                // request digest names exactly one release for the life of the
                // journal.
                self.orphan_releases
                    .insert(*request, OrphanClaimReleaseRecord { target: target.clone(), completion: *completion });
            }
            Decision::RecordIntegration { bloom, integration } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.integration.clone_from(integration);
                }
            }
            Decision::RecordAggregateRoll { bloom, rolls } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.aggregate_rolls = *rolls;
                }
            }
            Decision::RecordAggregateVerifyRoll { bloom, rolls } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.aggregate_verify_rolls = *rolls;
                }
            }
            Decision::RecordVerifyProof { .. } | Decision::RecordVerifyReuse { .. } => {
                self.apply_verify_memo_effect(effect);
            }
            Decision::RevokeResolution { bloom, workpiece } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.claims.remove(workpiece);
                }
            }
            Decision::MarkSuperseded { bloom, by } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.status = BloomStatus::Superseded;
                    record.superseded_by = Some(*by);
                }
            }
            Decision::SetResolved { .. } | Decision::SetUnresolved { .. } | Decision::RecordLandingRoll { .. } => {
                self.apply_landing_effect(effect);
            }
            Decision::AdvanceMainline { to, .. } => {
                self.mainline = *to;
                self.observed = *to;
            }
            Decision::RecordObservation { head } => {
                self.observed = *head;
            }
            Decision::RecordStageCatalog { .. } => self.apply_catalog_effect(effect),
            Decision::RecordCompositionFinding { .. }
            | Decision::RecordAdjudication { .. }
            | Decision::RecordOperatorRepair { .. } => self.apply_composition_effect(effect),
            Decision::RecordOperatorHold { .. }
            | Decision::RecordOperatorRelease { .. }
            | Decision::DeferDispatch { .. }
            | Decision::DeferAggregate { .. } => self.apply_operator_hold_effect(effect),
            Decision::RecordSpendQuiesce { quiesce } => {
                self.spend_quiesce.clone_from(quiesce);
            }
            Decision::RecordMemberDependencies { .. } => self.apply_graph_effect(effect),
            Decision::RecordHostFault { .. } | Decision::ClearHostFault { .. } => self.apply_host_fault_effect(effect),
            Decision::RecordCandidateVehicle { .. } => self.apply_vehicle_effect(effect),
            Decision::RecordMemberMachinery { .. } => self.apply_machinery_effect(effect),
            Decision::EmitReceipt(projected) => {
                if let Some(record) = self.blooms.get_mut(&projected.receipt.bloom) {
                    record.status = BloomStatus::Landed;
                }
            }
        }
    }

    /// Fold a moving cursor and the machinery series it may retire (ADR-0195).
    ///
    /// Split out of [`apply_effect`](Self::apply_effect) so the parent match
    /// stays inside its line budget: writing the cursor, leaving the wedged
    /// set, and retiring a machinery series that no longer applies are one
    /// fold, and that is only visible when they sit together.
    fn apply_advance_stage(&mut self, effect: &Decision) {
        let Decision::AdvanceStage { bloom, workpiece, progress } = effect else {
            return;
        };
        let was_wedged = if let Some(record) = self.blooms.get_mut(bloom) {
            record.progress.insert(workpiece.clone(), *progress);
            // A moving cursor is a member that is dispatching again, so it
            // is by definition no longer wedged. This is the only way out
            // of the wedged set — there is no clearing decision, because
            // every route back into the line already writes a cursor.
            record.wedged.remove(workpiece).is_some()
        } else {
            false
        };
        // A grant or stage change starts a fresh machinery series
        // (ADR-0195): the operator repaired the host, or the member
        // left the stage the faults were against. A same-stage work
        // retry keeps the series — the two axes are independent.
        let stage_changed = self
            .member_machinery
            .get(bloom)
            .and_then(|by_member| by_member.get(workpiece))
            .is_some_and(|fault| fault.stage != progress.stage);
        if (was_wedged || stage_changed)
            && let Some(by_member) = self.member_machinery.get_mut(bloom)
        {
            by_member.remove(workpiece);
        }
    }

    /// Fold a member machinery-fault series (ADR-0195).
    fn apply_machinery_effect(&mut self, effect: &Decision) {
        if let Decision::RecordMemberMachinery { bloom, workpiece, stage, rolls, evidence } = effect {
            self.member_machinery
                .entry(*bloom)
                .or_default()
                .insert(workpiece.clone(), MemberMachineryFault { stage: *stage, rolls: *rolls, evidence: *evidence });
        }
    }

    /// Fold the two decisions that write a bloom's pending-decision holds: the
    /// bloom-scope park that raises one, and the release that drops it.
    ///
    /// Split out of [`apply_effect`](Self::apply_effect) for the reason its
    /// siblings are: they are a raise/release pair over one set, and that is
    /// only visible when they sit together — a park that recorded its marker
    /// without raising the hold, or a release that dropped the hold and left the
    /// marker, would each read as correct in isolation.
    fn apply_hold_effect(&mut self, effect: &Decision) {
        match effect {
            Decision::ReleaseHold { bloom, question } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.holds.remove(question);
                }
            }
            Decision::RecordReviewPark { bloom, question } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.review_park = *question;
                    // Recording raises the hold in the same fold (idempotent
                    // when an admitted Question already inserted it through
                    // RecordEvidence — the marker then only classifies it);
                    // clearing leaves the release to whichever door answers the
                    // park: an adopting answer's `ReleaseHold`, or an operator
                    // adjudication's (#4957).
                    if let Some(question) = question {
                        record.holds.insert(*question);
                    }
                }
            }
            _ => {}
        }
    }

    /// Append to one of the composition workpiece's three append-only channels:
    /// the findings its gates raised (ADR-0191 §4), the operator adjudications
    /// that closed some of them, and the operator-supplied repairs (#4957).
    ///
    /// Split out of [`apply_effect`](Self::apply_effect) for the reason its
    /// siblings are: the parent match stays inside its line budget, and the
    /// three arms read as one mechanism — a defect discovered in the
    /// composition, the person who answered for it, and the candidate they
    /// supplied — which is only visible when they sit together. Each is
    /// append-only on purpose: closure is derived by
    /// [`BloomRecord::open_composition_findings`], never written back over the
    /// verdict that raised the finding.
    fn apply_composition_effect(&mut self, effect: &Decision) {
        match effect {
            Decision::RecordCompositionFinding { bloom, finding } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.composition_findings.push(finding.clone());
                }
            }
            Decision::RecordAdjudication { bloom, adjudication } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.adjudications.push(adjudication.clone());
                }
            }
            Decision::RecordOperatorRepair { bloom, repair } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.operator_repairs.push(repair.clone());
                }
            }
            _ => {}
        }
    }

    /// Fold the decisions that write the operator brake (#4976 / #5100): raising
    /// it, dropping it, and the deferrals a raised hold records each time it
    /// swallows a member or aggregate dispatch.
    ///
    /// Split out of [`apply_effect`](Self::apply_effect) for the reason its
    /// siblings are: they are one mechanism — a flag, its clear, and the ledger
    /// of what the flag cost — and that only reads as one when they sit together.
    /// A raise that forgot the flag, or a deferral recorded against no hold,
    /// would each look correct alone.
    fn apply_operator_hold_effect(&mut self, effect: &Decision) {
        match effect {
            Decision::RecordOperatorHold { bloom, hold } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.operator_hold = Some(hold.clone());
                }
            }
            // The deferrals are left standing rather than cleared here: the
            // release emits the dispatches beside this effect, and each of those
            // is what removes its own entry (see `apply_dispatch_effect`). A
            // release that cleared the set itself would erase a workpiece whose
            // dispatch the same reduction could not rebuild — an inherited claim
            // holding no cursor — and lose it silently.
            Decision::RecordOperatorRelease { bloom, .. } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.operator_hold = None;
                }
            }
            Decision::DeferDispatch { bloom, workpiece } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.deferred_dispatches.insert(workpiece.clone());
                }
            }
            Decision::DeferAggregate { bloom, stage } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.deferred_aggregates.insert(*stage);
                }
            }
            _ => {}
        }
    }

    /// Fold a resolution claim and drop a vehicle whose tree is no longer
    /// the claim's identity (#5079).
    fn apply_claim_effect(&mut self, effect: &Decision) {
        let (Decision::InheritClaim { bloom, claim } | Decision::RecordResolution { bloom, claim }) = effect else {
            return;
        };
        let Some(record) = self.blooms.get_mut(bloom) else {
            return;
        };
        // Keyed by workpiece: a re-integration overwrites the stale
        // candidate rather than accumulating a second claim.
        record.claims.insert(claim.workpiece.clone(), claim.clone());
        // A claim-only re-integrate must not leave a previous capture
        // sitting over a new tree. The matching vehicle, when there is
        // one, is written by `RecordCandidateVehicle` later in the same
        // effect list.
        if record.vehicles.get(&claim.workpiece).is_some_and(|vehicle| vehicle.tree != claim.candidate) {
            record.vehicles.remove(&claim.workpiece);
        }
    }

    /// Fold the capture-commit vehicle recorded at integration or inherited
    /// with a valid claim (#5079).
    fn apply_vehicle_effect(&mut self, effect: &Decision) {
        if let Decision::RecordCandidateVehicle { bloom, workpiece, vehicle } = effect
            && let Some(record) = self.blooms.get_mut(bloom)
        {
            record.vehicles.insert(workpiece.clone(), *vehicle);
        }
    }

    /// Fold the two decisions that write a member's host-fault hold (#5020):
    /// recording the preflight findings, and clearing them on a cadence resume.
    fn apply_host_fault_effect(&mut self, effect: &Decision) {
        match effect {
            Decision::RecordHostFault { bloom, workpiece, findings, evidence } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record
                        .host_faults
                        .insert(workpiece.clone(), HostFaultHold { findings: findings.clone(), evidence: *evidence });
                }
            }
            Decision::ClearHostFault { bloom, workpiece } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.host_faults.remove(workpiece);
                }
            }
            _ => {}
        }
    }

    /// Fold the catalog a seal recorded (#4944). Split out of
    /// [`apply_effect`](Self::apply_effect) so adding the arm does not blow
    /// the parent match's line budget.
    fn apply_catalog_effect(&mut self, effect: &Decision) {
        if let Decision::RecordStageCatalog { bloom, catalog } = effect
            && let Some(record) = self.blooms.get_mut(bloom)
        {
            record.stage_catalog.clone_from(catalog);
        }
    }

    /// Count one dispatch in the bloom's ledger (ADR-0180) — the fold the five
    /// dispatch decisions share. Their outbox rows are the store's and are
    /// untouched here; only the slot count is this fold's.
    ///
    /// Split out of [`apply_effect`](Self::apply_effect) for the same reason
    /// [`apply_landing_effect`](Self::apply_landing_effect) is: the five arms
    /// read as one group, differing only in which [`DispatchKey`] they resolve
    /// to, and that shape is only visible when they sit together. Deriving the
    /// key from the dispatch decision itself, rather than from a second effect
    /// emitted beside it, is what keeps the ledger from desynchronizing — a
    /// dispatch site added later is counted the moment its decision joins the
    /// match. Any other decision is a no-op here, and an unknown bloom is
    /// ignored exactly as every other record-scoped arm ignores one.
    fn apply_dispatch_effect(&mut self, effect: &Decision) {
        // A work order that actually goes out settles whatever the hold owed
        // this workpiece, so the deferral leaves the set here rather than through
        // a clearing decision — the same shape as `AdvanceStage` clearing a
        // wedge. That also makes it self-correcting: a member released and
        // dispatched, held again, and deferred again re-enters the set on its own
        // terms rather than carrying a stale entry.
        if let Decision::DispatchAttempt { bloom, workpiece, .. } = effect
            && let Some(record) = self.blooms.get_mut(bloom)
        {
            record.deferred_dispatches.remove(workpiece);
        }
        // An aggregate work order that actually goes out settles whatever the
        // hold owed that gate, the same implicit clear a member dispatch uses.
        if let Decision::DispatchAggregateVerify { bloom, .. } = effect
            && let Some(record) = self.blooms.get_mut(bloom)
        {
            record.deferred_aggregates.remove(&StageId::AggregateVerify);
        }
        if let Decision::DispatchAggregateReview { bloom, .. } = effect
            && let Some(record) = self.blooms.get_mut(bloom)
        {
            record.deferred_aggregates.remove(&StageId::AggregateReview);
        }
        let (bloom, key) = match effect {
            Decision::DispatchAttempt { bloom, workpiece, stage, .. } => {
                (bloom, DispatchKey::Member { workpiece: workpiece.clone(), stage: *stage })
            }
            Decision::DispatchIntegration { bloom, .. } => (bloom, DispatchKey::Bloom { stage: StageId::Integrate }),
            Decision::DispatchAggregateVerify { bloom, .. } => {
                (bloom, DispatchKey::Bloom { stage: StageId::AggregateVerify })
            }
            Decision::DispatchAggregateReview { bloom, .. } => {
                (bloom, DispatchKey::Bloom { stage: StageId::AggregateReview })
            }
            Decision::DispatchLand { bloom, .. } => (bloom, DispatchKey::Bloom { stage: StageId::Land }),
            _ => return,
        };

        if let Some(record) = self.blooms.get_mut(bloom) {
            let count = record.dispatches.entry(key).or_insert(0);
            *count = count.saturating_add(1);
        }
    }

    /// Fold the two halves of the verify memo (#4891): the proofs a green
    /// verdict files, and the receipts a pass-by-identity leaves.
    ///
    /// Split out of [`apply_effect`](Self::apply_effect) for the reason its
    /// siblings are: the pair reads as one mechanism — a record written so a
    /// later verify can read it, and the record of that read — and only shows
    /// that when they sit together. Any other decision is a no-op here, and an
    /// unknown bloom is ignored exactly as every other record-scoped arm ignores
    /// one.
    fn apply_verify_memo_effect(&mut self, effect: &Decision) {
        match effect {
            Decision::RecordVerifyProof { bloom, proof } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    // Keyed by tree *and* gate set, so re-proving the same tree
                    // under the same gates overwrites one entry rather than
                    // accumulating beside it: two green verdicts over one key are
                    // interchangeable by construction.
                    record.verify_proofs.insert(proof.verified(), proof.clone());
                }
            }
            Decision::RecordVerifyReuse { bloom, reuse } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.verify_reuses.push(reuse.clone());
                }
            }
            _ => {}
        }
    }

    /// Fold the three decisions that move a bloom across the land boundary
    /// (#4689) — resolving onto a head, returning off one when its landing was
    /// refused, and the attempt counter that bounds the round trip.
    ///
    /// Split out of [`apply_effect`](Self::apply_effect) because they are the
    /// only arms that read as a group: the same field pair moves in all three,
    /// and the resolve/un-resolve symmetry is only visible when they sit
    /// together. Any other decision is a no-op here.
    fn apply_landing_effect(&mut self, effect: &Decision) {
        match effect {
            Decision::SetResolved { bloom, resolved } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.status = BloomStatus::Resolved;
                    record.resolved_head = Some(resolved.head);
                }
            }
            Decision::SetUnresolved { bloom } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.status = BloomStatus::Sealed;
                    record.resolved_head = None;
                }
            }
            Decision::RecordLandingRoll { bloom, rolls } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.landing_rolls = *rolls;
                }
            }
            _ => {}
        }
    }
}

impl BloomRecord {
    /// The record a sealed spec opens with.
    ///
    /// A newly sealed bloom overwrites [`stage_catalog`](Self::stage_catalog)
    /// from [`Decision::RecordStageCatalog`]. Pre-existing rows without that
    /// effect keep this compiled-line fallback: [`StageCatalog::sealed_in`],
    /// which is [`StageCatalog::line`] when the spec sealed no catalog. Stated
    /// here because a later binary's edited line must not rewrite a no-catalog
    /// bloom that never recorded one.
    fn sealed(spec: BloomSpec, configs: &ResolvedConfigs) -> Self {
        let stage_catalog = StageCatalog::sealed_in(ConfigScopes::bloom_wide(spec.configs()), configs);
        Self {
            spec,
            stage_catalog,
            status: BloomStatus::Sealed,
            claims: BTreeMap::new(),
            evidence: Vec::new(),
            holds: BTreeSet::new(),
            progress: BTreeMap::new(),
            wedged: BTreeMap::new(),
            dispatches: BTreeMap::new(),
            integration: None,
            aggregate_rolls: 0,
            aggregate_verify_rolls: 0,
            landing_rolls: 0,
            resolved_head: None,
            review_park: None,
            verify_proofs: BTreeMap::new(),
            verify_reuses: Vec::new(),
            aggregate_fault: None,
            composition_findings: Vec::new(),
            adjudications: Vec::new(),
            operator_repairs: Vec::new(),
            operator_hold: None,
            deferred_dispatches: BTreeSet::new(),
            deferred_aggregates: BTreeSet::new(),
            dependencies: Vec::new(),
            host_faults: BTreeMap::new(),
            vehicles: BTreeMap::new(),
            superseded_by: None,
        }
    }

    /// The composition findings no operator adjudication has closed (#4957).
    ///
    /// The one place closure is decided, so the adjudication door and every
    /// later reader agree on which findings are still open. Derived rather than
    /// stored because a finding is a verdict: a closed one still happened, and
    /// what changed is that somebody answered for it. A finding named by any
    /// adjudication is closed — an override is not undone by a later one, and a
    /// finding raised twice under the same verdict artifact closes once.
    pub fn open_composition_findings(&self) -> impl Iterator<Item = &CompositionFinding> {
        self.composition_findings.iter().filter(|finding| {
            !self.adjudications.iter().any(|adjudication| adjudication.findings.contains(&finding.detail))
        })
    }

    /// The recorded green verdict for `tree` under the gate set the compiled
    /// verify lane runs, or `None` when this bloom holds none (#4891).
    ///
    /// The one place a memo hit is decided, so every verify position asks the
    /// question the same way and none of them can reach past the gate-set half
    /// of the key. The current gate set is recomputed rather than remembered:
    /// that is what makes a proof journaled under a different verify vocabulary
    /// or lane miss instead of answering for gates that no longer exist.
    #[must_use]
    pub fn verify_proof_for(&self, tree: Digest) -> Option<&VerifyProof> {
        self.verify_proofs.get(&VerifiedTree { tree, gate_set: VerifyGateSet::lane().digest() })
    }

    /// Fold one admitted evidence artifact into the record: the log entry, plus
    /// whatever derived state its kind carries.
    ///
    /// Two kinds carry derived state, both by the same rule — a hold or a fault
    /// series is *read out of* the evidence log rather than written by a fact of
    /// its own, so replay rebuilds it for free and no second decision can
    /// desynchronize from it. They sit together here for the same reason
    /// [`Snapshot::apply_dispatch_effect`] groups the dispatch arms: the shape is
    /// only visible when the derivations are in one place.
    fn record_evidence(&mut self, evidence: &Evidence) {
        // Append in admission order — the evidence log is a growing
        // journal-derived history, not a keyed latest-wins map.
        self.evidence.push(evidence.clone());
        match evidence.kind {
            // A question admission derives a pending-decision hold (ADR-0151):
            // the question digest (the evidence detail) blocks the bloom until an
            // answer releases it.
            EvidenceKind::Question => {
                self.holds.insert(evidence.detail);
            }
            // An executor-fault admission derives the aggregate-review fault
            // series (ADR-0176), keyed to the subject it names — so a fault on a
            // fresh fold starts over rather than inheriting the previous fold's
            // spent retries.
            EvidenceKind::ExecutorFault => {
                // Only the bloom-scoped aggregate-review series folds here
                // (ADR-0176). A member-stage fault (ADR-0195) binds a member
                // subject, not the held fold, and records its roll via
                // `RecordMemberMachinery`. Folding every ExecutorFault here
                // would charge the critic's series for a member-stage outage.
                if self.integration.as_ref().is_some_and(|fold| fold.tree == evidence.subject) {
                    self.aggregate_fault = Some(AggregateReviewFault::next(
                        self.aggregate_fault.as_ref(),
                        evidence.subject,
                        evidence.detail,
                    ));
                }
            }
            EvidenceKind::Approval
            | EvidenceKind::VerificationResult
            | EvidenceKind::ReviewFinding
            | EvidenceKind::ResolutionClaim
            | EvidenceKind::StudyRecord
            // An advisory-carrying pass derives nothing here: the findings it
            // records are filed by `RecordCompositionFinding` beside this row,
            // the same decision a refusal files, so the record carries them
            // whether the bloom went on to re-weave or to resolve.
            | EvidenceKind::FoldConflict
            | EvidenceKind::RepairTriage
            | EvidenceKind::ReviewAdvisory => {}
        }
    }
}
