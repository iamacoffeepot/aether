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
    BloomSpec, CandidateRef, ConfigScopes, DispatchKey, Evidence, EvidenceKind, OrphanClaimReleaseRecord,
    ResolutionClaim, ResolvedConfigs, StageCatalog, VerifyFailureSet, Wedge,
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
                    // A moving cursor is a member that is dispatching again, so it
                    // is by definition no longer wedged. This is the only way out
                    // of the wedged set — there is no clearing decision, because
                    // every route back into the line already writes a cursor.
                    record.wedged.remove(workpiece);
                }
            }
            Decision::RecordWedge { bloom, workpiece, wedge } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.wedged.insert(workpiece.clone(), *wedge);
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
            Decision::EmitReceipt(receipt) => {
                if let Some(record) = self.blooms.get_mut(&receipt.bloom) {
                    record.status = BloomStatus::Landed;
                }
            }
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
            wedged: BTreeMap::new(),
            dispatches: BTreeMap::new(),
            integration: None,
            aggregate_rolls: 0,
            aggregate_verify_rolls: 0,
            landing_rolls: 0,
            resolved_head: None,
            review_park: None,
            superseded_by: None,
        }
    }
}
