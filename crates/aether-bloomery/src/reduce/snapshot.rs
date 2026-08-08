//! The rebuildable projection the reducer reads, and the fold that evolves it.
//!
//! [`reduce`](super::reduce) *decides* against a [`Snapshot`]; [`Snapshot::apply`]
//! *evolves* one into the next. Journal replay is the two in sequence, event by
//! event — the split that keeps the decision pure and the evolution mechanical.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::{Decision, Decisions, Event, Fact, Outcome};
use crate::digest::Digest;
use crate::ids::{BloomId, IdempotencyKey, StageId, WorkpieceId};
use crate::values::{
    BloomSpec, CandidateRef, ConfigScopes, Evidence, EvidenceKind, ResolutionClaim, ResolvedConfigs, StageCatalog,
};

/// The rebuildable projection state the reducer reads (ADR-0149 §The control
/// core). Holds nothing that is not derivable from the journal.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// The current mainline head — landing is a compare-and-swap against it.
    pub mainline: Digest,
    /// The active-membership map: which bloom each workpiece is claimed by.
    /// The at-most-one-active-bloom-per-workpiece constraint is this map's
    /// key uniqueness.
    pub active: BTreeMap<WorkpieceId, BloomId>,
    /// Every sealed bloom the projection knows, by id.
    pub blooms: BTreeMap<BloomId, BloomRecord>,
    /// The idempotency keys already applied — a replayed key is a no-op.
    pub seen: BTreeSet<IdempotencyKey>,
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
}

/// The per-bloom projection record.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BloomRecord {
    /// The sealed, immutable spec.
    pub spec: BloomSpec,
    /// The stage catalog this bloom runs, resolved once at seal (ADR-0174).
    ///
    /// Derived state, not sealed content: the spec names the catalog by address
    /// in its [`configs`](BloomSpec::configs) registry, and this is what that
    /// address resolved to. Held rather than re-resolved per fact because the
    /// reducer reads it to decide re-dispatch versus wedge, and that decision has
    /// to be total — the seal door already refused a bloom whose catalog could not
    /// be produced, so by the time any later fact arrives the answer exists.
    ///
    /// A bloom sealing no catalog holds [`StageCatalog::line`], the compiled
    /// calibration, which is what makes an unconfigured bloom behave exactly as
    /// it did before catalogs were sealable.
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
    /// of the record — a seal seeds every member at the entry stage
    /// ([`StageCatalog::entry_stage`](crate::StageCatalog::entry_stage)), a passing attempt advances the cursor, and
    /// a failing one bumps the attempt count in place — so replay reconstructs
    /// in-flight line position. A member drops out of the map only implicitly (it
    /// never does in V1; the record is discarded whole on supersession).
    pub progress: BTreeMap<WorkpieceId, StageProgress>,
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
    /// The bloom-scope park (ADR-0153): the pending-decision question raised
    /// when the delta-confirm still failed at the two-pass ceiling — the
    /// failing review's record artifact digest, held in
    /// [`holds`](Self::holds) like any ADR-0151 question while the owner
    /// decides. An adopting answer that names it releases the hold and re-arms
    /// the review cycle instead of re-dispatching a member stage. `None` when
    /// the bloom is not parked.
    pub review_park: Option<Digest>,
    /// If superseded, the successor that replaced this bloom.
    pub superseded_by: Option<BloomId>,
}

/// The integration fold's output — the axes [`Fact::Resolve`] carries, held on
/// the bloom record while the whole-bloom aggregate review judges the
/// integrated head (ADR-0153).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
    /// How many failing terminal-Verify verdicts this member has consumed
    /// (ADR-0153) — the repair ceiling's counter, carried across the Refine
    /// re-entry a failing Verify routes into (the `attempts` reset on every
    /// stage advance, so the ceiling needs its own cursor field). A failing
    /// Verify at `repair_rolls >= retry_budget_of(Verify)` wedges instead of
    /// re-entering.
    pub repair_rolls: u32,
}

/// A bloom's position in the one-way lifecycle.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
        match (&event.fact, &decisions.outcome) {
            (Fact::Seal(spec), Outcome::Sealed(id)) => {
                next.blooms.insert(*id, BloomRecord::sealed(spec.clone(), configs));
            }
            (Fact::Supersede { successor, .. }, Outcome::Superseded { successor: id, .. }) => {
                next.blooms.insert(*id, BloomRecord::sealed(successor.clone(), configs));
            }
            _ => {}
        }
        for effect in &decisions.effects {
            next.apply_effect(effect);
        }
        next
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
            Decision::InheritClaim { bloom, claim } | Decision::RecordResolution { bloom, claim } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    // Keyed by workpiece: a re-integration overwrites the stale
                    // candidate rather than accumulating a second claim.
                    record.claims.insert(claim.workpiece.clone(), claim.clone());
                }
            }
            Decision::RecordEvidence { bloom, evidence } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    // Append in admission order — the evidence log is a growing
                    // journal-derived history, not a keyed latest-wins map.
                    record.evidence.push(evidence.clone());
                    // A question admission derives a pending-decision hold as part
                    // of this same fold (ADR-0151, no new fact): the question digest
                    // (the evidence detail) blocks the bloom until an answer releases
                    // it.
                    if evidence.kind == EvidenceKind::Question {
                        record.holds.insert(evidence.detail);
                    }
                }
            }
            Decision::ReleaseHold { bloom, question } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.holds.remove(question);
                }
            }
            Decision::AdvanceStage { bloom, workpiece, progress } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.progress.insert(workpiece.clone(), *progress);
                }
            }
            // Snapshot-inert outbox effects the store projects and republishes,
            // rebuilt on replay from the journaled fact — they carry no in-snapshot
            // state, like EmitReceipt's outbox row. A dispatch's paired cursor rides
            // its sibling AdvanceStage; a re-dispatch's hold release rides ReleaseHold.
            Decision::RedispatchStage { .. }
            | Decision::DispatchAttempt { .. }
            | Decision::DispatchLand { .. }
            | Decision::DispatchIntegration { .. }
            | Decision::DispatchAggregateReview { .. } => {}
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
            Decision::RecordReviewPark { bloom, question } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.review_park = *question;
                    // Recording raises the hold in the same fold (idempotent
                    // when an admitted Question already inserted it through
                    // RecordEvidence — the marker then only classifies it);
                    // clearing leaves the release to the adopting answer's
                    // ReleaseHold.
                    if let Some(question) = question {
                        record.holds.insert(*question);
                    }
                }
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
            Decision::SetResolved { bloom, .. } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.status = BloomStatus::Resolved;
                }
            }
            Decision::AdvanceMainline { to, .. } => {
                self.mainline = *to;
            }
            Decision::EmitReceipt(receipt) => {
                if let Some(record) = self.blooms.get_mut(&receipt.bloom) {
                    record.status = BloomStatus::Landed;
                }
            }
        }
    }
}

impl BloomRecord {
    /// The record a sealed spec opens with, its stage catalog resolved.
    ///
    /// The resolution cannot fail here: `reduce` refused any spec whose registry
    /// named content it was not given, and a resolved set only grows, so an
    /// address producible at the seal door is producible at the fold. An
    /// unresolvable catalog therefore means the two ran against different sets,
    /// which is a broken caller rather than a state to represent — it falls back
    /// to the compiled line and the bloom runs the calibration it would have run
    /// with no catalog sealed at all.
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
            integration: None,
            aggregate_rolls: 0,
            review_park: None,
            superseded_by: None,
        }
    }
}
