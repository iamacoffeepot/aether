//! The reducer's effect vocabulary. A [`Decision`] is either snapshot-folding
//! (it evolves the projection) or snapshot-inert (it carries an outbox row the
//! host drains and turns into I/O) — the reducer never does I/O itself.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::gate::RecordedRefusal;
use super::{FoldedIntegration, StageProgress};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::port::ProjectedReceipt;
use crate::values::{
    Adjudication, AgentProfile, BaseReceipt, CandidateRef, CompositionFinding, ConfigRegistry, Evidence,
    MemberCandidate, MemberDependency, OperatorHold, OperatorProposal, OperatorRepair, OrphanClaimRelease,
    OrphanClaimReleaseCompletion, ResolutionClaim, ResolvedBloom, SpendQuiesce, StageCatalog, Transformation,
    VerifyProof, VerifyReuse, Wedge, Withdrawal,
};

/// The ordered effects a decision applies to the projection (and, in
/// production, the outbox/store).
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Decision {
    /// Claim a workpiece's active membership for a bloom.
    ClaimMembership {
        /// The claimed workpiece.
        workpiece: WorkpieceId,
        /// The claiming bloom.
        bloom: BloomId,
    },
    /// Release a workpiece's active membership from a bloom.
    ReleaseMembership {
        /// The released workpiece.
        workpiece: WorkpieceId,
        /// The bloom the claim is released from.
        bloom: BloomId,
    },
    /// Inherit a predecessor's resolution claim into a successor.
    InheritClaim {
        /// The successor inheriting the claim.
        bloom: BloomId,
        /// The inherited claim.
        claim: ResolutionClaim,
    },
    /// Record a resolution claim on a bloom (from integration).
    RecordResolution {
        /// The bloom the claim is recorded on.
        bloom: BloomId,
        /// The recorded claim.
        claim: ResolutionClaim,
    },
    /// Append non-integrating evidence to a bloom's evidence log (from
    /// admission). A [`EvidenceKind::Question`](crate::EvidenceKind::Question) entry additionally folds its
    /// `detail` digest into the record's open holds (see [`BloomRecord::holds`](crate::BloomRecord::holds)),
    /// and an [`EvidenceKind::ExecutorFault`](crate::EvidenceKind::ExecutorFault)
    /// entry folds the bloom's aggregate-review fault series keyed to the
    /// subject it names (see [`BloomRecord::aggregate_fault`](crate::BloomRecord::aggregate_fault),
    /// ADR-0176).
    RecordEvidence {
        /// The bloom the evidence is recorded on.
        bloom: BloomId,
        /// The admitted evidence.
        evidence: Evidence,
    },
    /// Mark a bloom superseded by a successor.
    MarkSuperseded {
        /// The superseded bloom.
        bloom: BloomId,
        /// The successor.
        by: BloomId,
    },
    /// Store a bloom's resolved artifact and mark it resolved.
    SetResolved {
        /// The resolved bloom's id.
        bloom: BloomId,
        /// The resolved artifact.
        resolved: ResolvedBloom,
    },
    /// Advance mainline — emitted by a land, by an observation that found the
    /// repository ahead with nothing in flight, and by a supersession that
    /// rebases onto the observed head (#4709).
    ///
    /// Also refreshes the observed head, because every one of those three moves
    /// mainline to a head at least as fresh as the last observation: a land
    /// authors the head itself, and the other two move onto the observed one.
    /// Without that, a land would leave `observed` pointing behind mainline and
    /// the next supersession could rebase *backwards* onto it.
    AdvanceMainline {
        /// The prior mainline head.
        from: Digest,
        /// The new mainline head.
        to: Digest,
    },
    /// Emit a landing receipt to the outbox, carrying the landed bloom's
    /// membership alongside it (ADR-0149 §The receipt carries its members): the
    /// receipt value names no members, and the outward projection has no other
    /// route to the objects the receipt belongs on.
    EmitReceipt(ProjectedReceipt),
    /// Release a member's pending-decision hold (from an adopted answer) —
    /// removes the named question digest from the bloom's open holds so the
    /// bloom can resolve once every member is integrated. Appended so the prior
    /// decisions' wire discriminants are unchanged.
    ReleaseHold {
        /// The bloom the hold is released on.
        bloom: BloomId,
        /// The released question's digest.
        question: Digest,
    },
    /// Re-dispatch a held stage with the adopted answer in its input closure
    /// (from an adopted answer). A snapshot-inert outbox effect — like
    /// [`Decision::EmitReceipt`], it carries no store-projection row and is
    /// republished to the dispatch reactor, which re-assembles the attempt's
    /// prompt manifest naming both the question and the answer digests
    /// (ADR-0151).
    RedispatchStage {
        /// The bloom whose held stage is re-dispatched.
        bloom: BloomId,
        /// The question whose hold was released.
        question: Digest,
        /// The adopting answer's digest — grounds the re-dispatched attempt's
        /// instruction slot.
        answer: Digest,
        /// The answer statement's exact asserted bytes, forwarded so the host
        /// can overlay the decision onto the re-dispatched lane's advisory
        /// channel (#3664). The reducer resolves nothing here — it holds the
        /// adopting [`Statement`](crate::Statement) already, and a lane
        /// re-dispatched without the decision that released it re-parks on the
        /// same question. Carrying content is the
        /// [`Decision::DispatchAttempt`] precedent, not an exception to it.
        words: Vec<u8>,
    },
    /// Dispatch an attempt of `stage` against `workpiece`'s subject in `bloom` —
    /// the transactional-outbox intent the host drains and submits through the
    /// executor port (ADR-0149 §The line / §The boundary). The reducer *decides*
    /// to dispatch; it never does I/O. A snapshot-inert outbox effect like
    /// [`Decision::EmitReceipt`] / [`Decision::RedispatchStage`] — it carries no
    /// in-snapshot state and is rebuilt on replay from the journaled fact.
    /// Appended so the prior decisions' wire discriminants are unchanged.
    DispatchAttempt {
        /// The bloom the dispatched member belongs to.
        bloom: BloomId,
        /// The member workpiece the attempt runs against.
        workpiece: WorkpieceId,
        /// The stage this attempt executes.
        stage: StageId,
        /// The fully-built portable transformation the host wraps in a work order
        /// (adding the idempotency nonce) and submits through the executor port.
        transformation: Transformation,
        /// The member's frozen scope-revision digest, carried explicitly so the
        /// host reactor records it without inferring it from the transformation's
        /// inputs (ADR-0152 — once a candidate exists, `inputs[0]` is the
        /// candidate tree, not the revision).
        scope_revision: Digest,
        /// The candidate tree this attempt runs against, when the member has one
        /// (ADR-0152). The host displays it as the digest returned evidence must
        /// bind to; `None` dispatches against the scope revision (Construct, or
        /// a member with no capture yet).
        candidate: Option<Digest>,
        /// The [`AgentProfile`] the bloom's sealed stage catalog calibrates this
        /// stage at (ADR-0174). Resolved by the reducer because the reducer is
        /// what holds the catalog; the host applies the member's
        /// [`ModelOverride`](crate::ModelOverride) over *this* rather than over
        /// the compiled line, so a bloom that sealed a catalog runs the agents
        /// that catalog names.
        profile: AgentProfile,
        /// The configuration this attempt runs under (ADR-0174): the member's
        /// registry layered over the bloom's, flattened here because a dispatch
        /// names one member. The host fetches what each remaining address names at
        /// the point of use.
        configs: ConfigRegistry,
    },
    /// Advance a member's stage cursor to `progress` — the snapshot-folding
    /// counterpart to a [`Decision::DispatchAttempt`]. Overwrites the member's
    /// entry in the record's progress map (see [`BloomRecord::progress`](crate::BloomRecord::progress)); a seal
    /// seeds each member here, a passing attempt moves the cursor forward, a
    /// failing one bumps the attempt count in place.
    AdvanceStage {
        /// The bloom the member belongs to.
        bloom: BloomId,
        /// The member whose cursor advances.
        workpiece: WorkpieceId,
        /// The member's new stage cursor.
        progress: StageProgress,
    },
    /// Drive the source-port compare-and-swap land of a just-resolved bloom —
    /// the transactional-outbox intent the host's land reactor drains and issues
    /// through the source port's `aether.source.land` op (ADR-0149 §The boundary,
    /// migration step 3). Emitted alongside [`Decision::SetResolved`] the moment a
    /// bloom resolves: resolution is land-readiness (a resolved bloom carries its
    /// one artifact and every member's claim), so the land decision rides the same
    /// resolve commit. A snapshot-inert outbox effect like [`Decision::EmitReceipt`]
    /// / [`Decision::DispatchAttempt`] — the actual mainline advance folds in later
    /// from the reactor's [`Fact::Land`](crate::Fact::Land) admit, never from this decision. Appended so
    /// the prior decisions' wire discriminants are unchanged.
    DispatchLand {
        /// The resolving bloom to land.
        bloom: BloomId,
        /// The sealed base the CAS lands on — a moved mainline forces
        /// supersession, never a land onto the new head (ADR-0149 §The bloom).
        expected_base: Digest,
        /// The head mainline advances to on a successful land — the bloom's one
        /// resolved artifact tree.
        new_head: Digest,
    },
    /// Drive the bloom's git-side integration (ADR-0152 §Resolution drives
    /// integration): emitted by the [`Fact::Integrate`](crate::Fact::Integrate) that completes the claim
    /// set — every member now carries a resolution — so the host integrate
    /// reactor folds each claim's candidate tree onto the bloom's integration
    /// branch in member order and admits the [`Fact::Resolve`](crate::Fact::Resolve) whose
    /// `DispatchLand` the land reactor then consumes. A snapshot-inert outbox
    /// effect like [`Decision::DispatchAttempt`], appended so the prior
    /// decisions' wire discriminants are unchanged.
    DispatchIntegration {
        /// The bloom whose members all carry claims.
        bloom: BloomId,
        /// The sealed base the integration branch bootstraps at.
        base: Digest,
        /// Every member's workpiece and claimed candidate tree, in member
        /// order — the fold sequence, and the resolve's integration lineage.
        /// The workpiece rides along because a fold that must *combine* work
        /// merges each member's candidate ref, which is addressed by workpiece;
        /// the tree alone cannot be merged.
        members: Vec<MemberCandidate>,
        /// The predecessor whose candidate refs this fold adopts before it
        /// runs, when any of the bloom's folded candidates was inherited from
        /// one.
        ///
        /// A candidate ref is addressed under the bloom that produced it, and a
        /// successor is a different bloom — its id is a content address over a
        /// spec that includes the base. So a successor folding inherited work
        /// has claims but no refs of its own to merge. Adopting copies them into
        /// its namespace first, leaving the successor self-contained rather than
        /// reaching back into a retired bloom's refs every time it folds.
        ///
        /// Named for a *mixed* set too, not only a wholly inherited one: a
        /// successor that re-ran some members folds their fresh captures beside
        /// the inherited claims, and adoption is adopt-if-absent, so the refs
        /// this bloom already carries stand while the missing ones are filled in
        /// (#4903).
        adopt_from: Option<BloomId>,
    },
    /// Record (or clear) the folded integration held on the bloom while its
    /// aggregate review runs (ADR-0153): a verified [`Fact::Resolve`](crate::Fact::Resolve) sets it,
    /// a failing review verdict clears it (the fold is stale once a member's
    /// claim is revoked). Appended so the prior decisions' wire discriminants
    /// are unchanged.
    RecordIntegration {
        /// The bloom the fold is held on.
        bloom: BloomId,
        /// The fold to hold, or `None` to clear a stale one.
        integration: Option<FoldedIntegration>,
    },
    /// Record the bloom's consumed aggregate-review verdict count — the
    /// two-pass ceiling's cursor (ADR-0153).
    RecordAggregateRoll {
        /// The reviewed bloom.
        bloom: BloomId,
        /// The verdicts consumed so far, this one included.
        rolls: u32,
    },
    /// Revoke a member's resolution claim (ADR-0153): a failing aggregate
    /// review re-opens every implicated member, and a bloom with a revoked
    /// claim cannot resolve until the member re-verifies and re-integrates.
    RevokeResolution {
        /// The bloom the claim is revoked on.
        bloom: BloomId,
        /// The re-opened member.
        workpiece: WorkpieceId,
    },
    /// Dispatch the whole-bloom aggregate review against the integrated head
    /// (ADR-0153) — the `review.critic` lane run once per bloom, judging the
    /// whole diff against the sealed intent. A snapshot-inert outbox effect
    /// like [`Decision::DispatchAttempt`]; the host wraps the transformation
    /// in a work order under a bloom-level order record.
    DispatchAggregateReview {
        /// The reviewed bloom.
        bloom: BloomId,
        /// The review lane transformation: `inputs[0]` is the integrated tree
        /// digest the returned evidence binds, `checkout` the landable head
        /// commit the critic checks out.
        transformation: Transformation,
        /// Which review pass this dispatches (`1` the full review, `2` the
        /// delta-confirm against the frozen finding set).
        roll: u32,
        /// The [`AgentProfile`] the bloom's sealed stage catalog calibrates
        /// `AggregateReview` at (ADR-0174). The critic is a model lane, so it
        /// takes its profile off the sealed catalog for the same reason a member
        /// lane does.
        profile: AgentProfile,
        /// The bloom-wide configuration this review runs under (ADR-0174). The
        /// host resolves the sealed [`ModelOverride`](crate::ModelOverride) from
        /// this registry at dispatch — the same overlay the member lane applies
        /// — so a bloom that sealed an override is not judged by the catalog
        /// default. An empty registry is the no-override case and resolves to
        /// the catalog default.
        configs: ConfigRegistry,
    },
    /// Record (or clear) the bloom-scope park (ADR-0153): raised when the
    /// delta-confirm still fails at the two-pass ceiling, holding the failing
    /// review's record artifact as a pending question (ADR-0151's hold
    /// vocabulary at bloom scope). Recording inserts the question into the
    /// bloom's open holds; clearing drops only the marker — the hold's release
    /// is [`Decision::ReleaseHold`]'s, emitted alongside by the adopting
    /// answer. Appended so the prior decisions' wire discriminants are
    /// unchanged.
    RecordReviewPark {
        /// The parked bloom.
        bloom: BloomId,
        /// The parked question digest, or `None` to clear on adoption.
        question: Option<Digest>,
    },
    /// Record that a member wedged — exhausted a stage's `retry_budget` and
    /// stopped dispatching (ADR-0149 §The line). Emitted alongside the
    /// [`Outcome::AttemptWedged`](crate::Outcome::AttemptWedged) the same
    /// reduction returns, because the outcome tells the caller of *this* fact
    /// and the record is what every later reader sees.
    ///
    /// No matching clear decision: a member leaves the wedged set through
    /// [`Decision::AdvanceStage`], since a cursor that moves is a member that is
    /// dispatching again. Appended so the prior decisions' wire discriminants
    /// are unchanged.
    RecordWedge {
        /// The bloom the wedged member belongs to.
        bloom: BloomId,
        /// The member that stopped.
        workpiece: WorkpieceId,
        /// Which stage exhausted, and the failure that spent the last of it.
        wedge: Wedge,
    },
    /// Dispatch the whole-bloom aggregate verify against the folded head — the
    /// mechanical `verify.check` fan-out run once per bloom, before the critic
    /// ever sees the fold.
    ///
    /// A snapshot-inert outbox effect like
    /// [`Decision::DispatchAggregateReview`]; the host wraps the
    /// transformation in a work order under a bloom-level order record.
    /// Appended so the prior decisions' wire discriminants are unchanged.
    DispatchAggregateVerify {
        /// The verified bloom.
        bloom: BloomId,
        /// The verify lane transformation: `inputs[0]` is the folded tree
        /// digest the returned evidence binds, `checkout` the folded head the
        /// compiler builds.
        transformation: Transformation,
        /// Which verify pass this dispatches, against the stage's own budget.
        roll: u32,
        /// The [`AgentProfile`] the bloom's sealed stage catalog calibrates
        /// `AggregateVerify` at (ADR-0174). A mechanical lane still takes its
        /// profile off the sealed catalog, so a receipt attests the exact
        /// configuration that ran.
        profile: AgentProfile,
    },
    /// Record the bloom's consumed aggregate-verify verdict count.
    ///
    /// Separate from [`Decision::RecordAggregateRoll`] because the two
    /// aggregate gates hold separate budgets in the catalog: a fold that
    /// exhausts the compiler's rolls must not also have spent the critic's.
    /// Appended so the prior decisions' wire discriminants are unchanged.
    RecordAggregateVerifyRoll {
        /// The verified bloom.
        bloom: BloomId,
        /// The verdicts consumed so far, this one included.
        rolls: u32,
    },
    /// Record the bloom's consumed landing-attempt count (#4689) — the cursor
    /// bounding how many times a red landing CI may re-open the line.
    RecordLandingRoll {
        /// The landing bloom.
        bloom: BloomId,
        /// The landing attempts consumed so far, this one included.
        rolls: u32,
    },
    /// Return a resolved bloom to `Sealed` after its landing was refused
    /// (#4689) — the one transition that walks the lifecycle backwards.
    ///
    /// A resolved bloom is land-ready by definition, and a rejected landing is
    /// exactly the statement that it is not. Leaving it `Resolved` while its
    /// members repair would let the land reactor re-propose the head that just
    /// failed. `Sealed` is still active-unlanded, so the one-bloom-per-mainline
    /// guard is unaffected.
    SetUnresolved {
        /// The bloom returning to the working state.
        bloom: BloomId,
    },
    /// Record the head the source last reported, whether or not mainline was
    /// free to move onto it (#4709).
    ///
    /// The live pointer half of the observation policy. Mainline is the base a
    /// land compare-and-swaps against and may only move when nothing is in
    /// flight; the observed head is just what the repository said, so it is
    /// recorded unconditionally. Keeping them apart is what lets a supersession
    /// rebase onto a head mainline was not allowed to follow — and recording it
    /// as a decision rather than host state is what keeps it derivable from the
    /// journal, which [`Snapshot`](crate::Snapshot) requires of everything it
    /// holds. Appended so the prior decisions' wire discriminants are unchanged.
    RecordObservation {
        /// The head the source reported.
        head: Digest,
    },
    /// Record an authorized orphan-claim release request as pending, or fold its
    /// terminal result onto the record already there (ADR-0179).
    ///
    /// One decision for both edges because they write the same map entry: an
    /// admitted request inserts it with `completion: None`, and the reactor's
    /// completion overwrites that field. Splitting them would mean two variants
    /// whose only difference is whether one `Option` is populated.
    RecordOrphanClaimRelease {
        /// The request digest keying the record.
        request: Digest,
        /// The signed target — carried so a replayed journal rebuilds the
        /// pending record without re-deriving it from the fact.
        target: OrphanClaimRelease,
        /// The terminal result, or `None` to open the record as pending.
        completion: Option<OrphanClaimReleaseCompletion>,
    },
    /// Drive the source-port expected-holder compare-and-swap that releases one
    /// orphaned claim ref (ADR-0179) — the transactional-outbox intent the
    /// claim-release reactor drains.
    ///
    /// A snapshot-inert outbox effect like [`Decision::DispatchLand`]: the
    /// terminal result folds in later from the reactor's
    /// [`Fact::CompleteOrphanClaimRelease`](crate::Fact::CompleteOrphanClaimRelease)
    /// admit, never from this decision. Emitted exactly once per request digest —
    /// a repeat request re-reads the record and enqueues nothing — so an
    /// authorized release is executed once however many times it is submitted.
    /// Appended so the prior decisions' wire discriminants are unchanged.
    DispatchOrphanClaimRelease {
        /// The request digest the completion admits back under.
        request: Digest,
        /// The typed ref and expected holder the compare-and-swap runs against.
        target: OrphanClaimRelease,
    },
    /// File a green verify verdict in the bloom's verify memo, keyed by the tree
    /// it judged and the gate set that judged it (#4891) — see
    /// [`BloomRecord::verify_proofs`](crate::BloomRecord::verify_proofs).
    ///
    /// Emitted wherever the reducer learns a tree passed its gates: a member's
    /// passing terminal `Verify` (which arrives as its
    /// [`Fact::Integrate`](crate::Fact::Integrate)) and a passing
    /// [`Fact::AggregateVerifyCompleted`](crate::Fact::AggregateVerifyCompleted).
    /// Snapshot-folding and journal-derived like every other `Record*`, so a
    /// replay rebuilds the memo rather than carrying it forward as host state.
    /// Appended so the prior decisions' wire discriminants are unchanged.
    RecordVerifyProof {
        /// The bloom whose memo the proof is filed in.
        bloom: BloomId,
        /// The proof, naming the gate set, the position that collected it, and
        /// the green verdict bound to its tree.
        proof: VerifyProof,
    },
    /// Record that a verify position passed by identity on an already-recorded
    /// proof instead of dispatching its gates (#4891) — the receipt half of a
    /// memo hit, see [`BloomRecord::verify_reuses`](crate::BloomRecord::verify_reuses).
    ///
    /// Emitted beside the effects the position's *pass* would have produced, so
    /// the journal never shows a verdict that nothing accounts for: a reader
    /// (and the calibration ledger counting reclaimed worker-seconds) can name
    /// the exact prior verdict the pass stood on. Appended so the prior
    /// decisions' wire discriminants are unchanged.
    RecordVerifyReuse {
        /// The bloom the reuse happened in.
        bloom: BloomId,
        /// Which position passed, and the proof it reused.
        reuse: VerifyReuse,
    },
    /// Record the stage catalog this bloom runs — resolved once at admission
    /// so the fold reads the record rather than re-deriving it from the
    /// compiled line (ADR-0174, #4944). Appended so the prior decisions' wire
    /// discriminants are unchanged. A journal written before this variant
    /// keeps the compiled-line fallback at `BloomRecord` construction.
    RecordStageCatalog {
        /// The bloom the catalog is recorded on.
        bloom: BloomId,
        /// The catalog admission resolved — the compiled line when the spec
        /// sealed none, otherwise the sealed value.
        catalog: StageCatalog,
    },
    /// File a composition-review observation on the bloom (ADR-0191 §4) — the
    /// findings channel the composition workpiece owns, see
    /// [`BloomRecord::composition_findings`](crate::BloomRecord::composition_findings).
    ///
    /// Every refusal of the composed tree records one, whether it goes on to
    /// re-weave, to wedge, or to park at a gate's ceiling (#4977), because the
    /// finding is what the repair reads, what an operator adjudicates, and what
    /// a member-scope observation becomes when there is no member to re-open:
    /// members are immutable after review, so an observation about one is filed
    /// as new work for a future bloom rather than routed back into finished,
    /// reviewed code. Appended so the prior decisions' wire discriminants are
    /// unchanged.
    RecordCompositionFinding {
        /// The bloom whose composition was judged.
        bloom: BloomId,
        /// The finding: the weave it was raised against, the verdict artifact,
        /// and the members it points at.
        finding: CompositionFinding,
    },
    /// File an operator adjudication on the bloom (#4957) — the closure record
    /// for the composition findings it names, see
    /// [`BloomRecord::adjudications`](crate::BloomRecord::adjudications).
    ///
    /// Closure is *derived* from this record rather than written into the
    /// finding: a [`CompositionFinding`] is what a verdict said, and editing it
    /// to say it was waived would lose the verdict. So the finding stands and
    /// [`BloomRecord::open_composition_findings`](crate::BloomRecord::open_composition_findings)
    /// filters it out, which is also what keeps replay honest — the journal
    /// still carries both halves in the order they happened. Appended so the
    /// prior decisions' wire discriminants are unchanged.
    RecordAdjudication {
        /// The bloom whose findings were adjudicated.
        bloom: BloomId,
        /// What was closed, how, why, and by whom.
        adjudication: Adjudication,
    },
    /// File an operator-supplied repair on the bloom (#4957) — the decider
    /// record beside the ordinary cursor move and `Verify` dispatch the same
    /// reduction emits, see
    /// [`BloomRecord::operator_repairs`](crate::BloomRecord::operator_repairs).
    ///
    /// Recorded rather than merely decided, for the reason a wedge is: the
    /// dispatch that follows is indistinguishable from a lane's, so without this
    /// row the journal would show a candidate nobody accounts for. Appended so
    /// the prior decisions' wire discriminants are unchanged.
    RecordOperatorRepair {
        /// The bloom the repaired workpiece belongs to.
        bloom: BloomId,
        /// The candidate, the workpiece it is for, and who supplied it.
        repair: OperatorRepair,
    },
    /// Put a bloom's dispatch on the operator brake (#4976) — see
    /// [`BloomRecord::operator_hold`](crate::BloomRecord::operator_hold).
    ///
    /// The flag the dispatch guard reads. While it is set the reducer emits
    /// [`Decision::DeferDispatch`] wherever it would have emitted a
    /// [`Decision::DispatchAttempt`], and [`Decision::DeferAggregate`] wherever
    /// it would have emitted a [`Decision::DispatchAggregateVerify`] or
    /// [`Decision::DispatchAggregateReview`]. Every other decision is unchanged
    /// — a hold is a brake on new work, not a pause on the projection.
    /// Appended so the prior decisions' wire discriminants are unchanged.
    RecordOperatorHold {
        /// The bloom whose dispatch is frozen.
        bloom: BloomId,
        /// Why, and by whom — journaled here rather than only on the fact,
        /// because replay folds decisions alone (ADR-0190).
        hold: OperatorHold,
    },
    /// Take the operator brake back off (#4976), clearing
    /// [`BloomRecord::operator_hold`](crate::BloomRecord::operator_hold).
    ///
    /// A separate variant rather than an `Option` on its sibling so the
    /// release's own reason and operator survive into the journal: a clear that
    /// carried `None` would record that a bloom was let go and nothing about who
    /// let it go. The dispatches the hold swallowed ride as ordinary
    /// [`Decision::DispatchAttempt`], [`Decision::DispatchAggregateVerify`], and
    /// [`Decision::DispatchAggregateReview`] effects emitted beside this one.
    /// Appended so the prior decisions' wire discriminants are unchanged.
    RecordOperatorRelease {
        /// The bloom being let go.
        bloom: BloomId,
        /// Why, and by whom.
        release: OperatorHold,
    },
    /// Record that a held bloom owes `workpiece` the dispatch its cursor move
    /// just earned (#4976) — see
    /// [`BloomRecord::deferred_dispatches`](crate::BloomRecord::deferred_dispatches).
    ///
    /// Emitted in the [`Decision::DispatchAttempt`] slot of every cursor move
    /// made while the bloom is held, so the pair a move emits stays a pair: the
    /// advance, and what the reducer decided to do about dispatching it.
    ///
    /// Recorded rather than derived for the reason
    /// [`Decision::RecordWedge`] is. A workpiece whose worker is still running
    /// and one whose dispatch the hold swallowed sit at the same cursor, so the
    /// cursor cannot tell them apart — and a release that could not tell them
    /// apart would either strand the swallowed dispatch or put a second worker on
    /// a running one. What is *not* recorded is the dispatch itself: the release
    /// re-derives targets, catalog, profile, and configuration from the record as
    /// it stands then, so a held bloom that moved on in some other way dispatches
    /// where it actually is. It leaves the set the way a wedge leaves `wedged` —
    /// implicitly, when the dispatch it names finally goes out. Appended so the
    /// prior decisions' wire discriminants are unchanged.
    DeferDispatch {
        /// The held bloom.
        bloom: BloomId,
        /// The workpiece owed a dispatch once the hold lifts.
        workpiece: WorkpieceId,
    },
    /// Record (or clear) the snapshot-level spend-quiesce marker (ADR-0192) —
    /// see [`crate::Snapshot::spend_quiesce`].
    ///
    /// `Some` records the crossing that closed the seal door; `None` clears it
    /// on the first seal that passes the governor again. One variant with an
    /// `Option` rather than a raised-and-cleared pair, because the clearing
    /// edge carries no operator, no reason, and nothing else worth journaling
    /// — the [`Decision::RecordReviewPark`] shape. Appended so the prior
    /// decisions' wire discriminants are unchanged.
    RecordSpendQuiesce {
        /// The crossing that closed the door, or `None` to clear.
        quiesce: Option<SpendQuiesce>,
    },
    /// Record the door-resolved member-dependency graph (ADR-0196) — see
    /// [`BloomRecord::dependencies`](crate::BloomRecord::dependencies).
    ///
    /// The edge set the seal door decided. Dispatch gates on declared edges
    /// only (ADR-0204); a surface-derived overlap is not in this list. Empty
    /// is the edgeless degenerate case — today's bloom, with this value
    /// appended. Snapshot-folding and journal-derived like every other
    /// `Record*`. Appended so the prior decisions' wire discriminants are
    /// unchanged.
    RecordMemberDependencies {
        /// The bloom the graph was sealed on.
        bloom: BloomId,
        /// The declared `(member, depends_on)` pairs, sorted and de-duplicated.
        edges: Vec<MemberDependency>,
    },
    /// Record that `workpiece` is held at Verify because the host could not
    /// run the gates (#5020) — see
    /// [`BloomRecord::host_faults`](crate::BloomRecord::host_faults).
    ///
    /// The findings are the preflight prose, journaled so the bloom view and
    /// the operator CLI can name the missing tools without looking the
    /// evidence artifact up. Appended so the prior decisions' wire
    /// discriminants are unchanged.
    RecordHostFault {
        /// The bloom the held member belongs to.
        bloom: BloomId,
        /// The member sitting at Verify.
        workpiece: WorkpieceId,
        /// The preflight findings — the missing tools, listed verbatim.
        findings: String,
        /// The preflight evidence digest, the idempotency half of the
        /// cadence resume so two ticks against the same hold collapse.
        evidence: Digest,
    },
    /// Clear a host-fault hold (#5020), the resume counterpart of
    /// [`Decision::RecordHostFault`].
    ///
    /// The dispatch the hold owed rides as an ordinary
    /// [`Decision::DispatchAttempt`] beside this one, re-derived from the
    /// cursor the way an operator release re-derives what a hold swallowed.
    /// Appended so the prior decisions' wire discriminants are unchanged.
    ClearHostFault {
        /// The bloom the member belongs to.
        bloom: BloomId,
        /// The member being let off the host-fault hold.
        workpiece: WorkpieceId,
    },
    /// Record the capture-commit vehicle known at integration (#5079) — see
    /// [`BloomRecord::vehicles`](crate::BloomRecord::vehicles).
    ///
    /// Tree identity stays on the claim; this row is the host checkout the
    /// dependent splice must name so a later construct is parented to the
    /// real capture rather than a parentless wrapper of the claimed tree.
    /// Appended so the prior decisions' wire discriminants are unchanged.
    RecordCandidateVehicle {
        /// The bloom the vehicle is recorded on.
        bloom: BloomId,
        /// The member whose capture this is.
        workpiece: WorkpieceId,
        /// The tree-plus-checkout pair known at integration.
        vehicle: CandidateRef,
    },
    /// Record that a held bloom owes its aggregate `stage` the dispatch a fold
    /// or verdict just earned (#5100) — see
    /// [`BloomRecord::deferred_aggregates`](crate::BloomRecord::deferred_aggregates).
    ///
    /// Emitted in the [`Decision::DispatchAggregateVerify`] /
    /// [`Decision::DispatchAggregateReview`] slot while the bloom is held, so
    /// the fact that produced the dispatch still journals and the work order
    /// is simply not written. The two aggregate gates ride their own decision
    /// paths, not [`Decision::DispatchAttempt`], which is why they need their
    /// own deferral rather than sharing [`Decision::DeferDispatch`].
    ///
    /// Recorded rather than derived for the reason [`Decision::DeferDispatch`]
    /// is: a fold whose aggregate is still in flight and one whose dispatch
    /// the hold swallowed sit at the same integration, so the fold cannot tell
    /// them apart. What is *not* recorded is the dispatch itself — the release
    /// re-derives it from the held fold as it stands then. It leaves the set
    /// when the dispatch it names actually goes out. Appended so the prior
    /// decisions' wire discriminants are unchanged.
    DeferAggregate {
        /// The held bloom.
        bloom: BloomId,
        /// The aggregate stage owed a dispatch once the hold lifts —
        /// [`StageId::AggregateVerify`] or [`StageId::AggregateReview`].
        stage: StageId,
    },
    /// Assemble a dependent's construct base from two or more independent
    /// ancestor tips (ADR-0196 G2).
    ///
    /// A unique-maximum ancestor is already one tree. A join is not: the
    /// host merges the named tips onto `base` the same way the weave merges
    /// candidates, then admits [`Fact::SpliceAssembled`](crate::Fact::SpliceAssembled)
    /// or a residual [`Fact::FoldConflict`](crate::Fact::FoldConflict). Snapshot-inert
    /// outbox like [`Decision::DispatchIntegration`]. Appended so the prior
    /// decisions' wire discriminants are unchanged.
    DispatchSplice {
        /// The bloom the dependent belongs to.
        bloom: BloomId,
        /// The dependent whose construct waits on the assembled tree.
        workpiece: WorkpieceId,
        /// The sealed bloom base the tips merge onto.
        base: Digest,
        /// The independent ancestor tips, in sealed member order.
        members: Vec<MemberCandidate>,
        /// The predecessor whose candidate refs this splice adopts first,
        /// when any tip was inherited.
        adopt_from: Option<BloomId>,
    },
    /// Record one member-stage machinery fault (ADR-0195) — see
    /// [`crate::Snapshot::member_machinery`].
    ///
    /// Counted apart from [`StageProgress::attempts`](crate::StageProgress::attempts)
    /// and [`StageProgress::repair_rolls`](crate::StageProgress::repair_rolls)
    /// because an executor that could not run judged no candidate. Snapshot-folding
    /// and journal-derived like every other `Record*`. Appended so the prior
    /// decisions' wire discriminants are unchanged.
    RecordMemberMachinery {
        /// The bloom the member belongs to.
        bloom: BloomId,
        /// The member whose gate could not run.
        workpiece: WorkpieceId,
        /// The stage the fault ran against.
        stage: StageId,
        /// Machinery faults this stage has taken, this one included.
        rolls: u32,
        /// The latest fault report's artifact digest.
        evidence: Digest,
    },
    /// Record one member's terminal withdrawal from a walking bloom (#5327) —
    /// see [`BloomRecord::withdrawn`](crate::BloomRecord::withdrawn).
    ///
    /// Recorded rather than derived for the reason [`Decision::RecordWedge`]
    /// is: a member with no cursor and no claim because it was withdrawn and
    /// one that simply never entered the line look identical from the record,
    /// and only this row says which. The fold also drops the member's cursor
    /// and its deferred dispatch, on the wedge's own argument — a workpiece
    /// that stops dispatching is owed nothing. Appended so the prior
    /// decisions' wire discriminants are unchanged.
    RecordWithdrawal {
        /// The bloom the member is leaving.
        bloom: BloomId,
        /// Who is leaving, why, and on whose word.
        withdrawal: Withdrawal,
    },
    /// Kill the withdrawn member's live lane, if it has one (#5327) — the
    /// transactional-outbox intent the executor reactor drains under
    /// [`Topic::CancelDispatch`](crate::Topic::CancelDispatch).
    ///
    /// Deliberately *not* the timeout path: that synthesises a `TimeoutRecord`
    /// and admits a failed attempt, which would spend the member's budget and
    /// route it into repair. A withdrawn member's lane is cancelled and its
    /// order consumed; no evidence is admitted, because there is no longer
    /// anything for evidence to be about. Snapshot-inert like
    /// [`Decision::DispatchLand`]. Appended so the prior decisions' wire
    /// discriminants are unchanged.
    CancelDispatch {
        /// The bloom the cancelled lane was dispatched under.
        bloom: BloomId,
        /// The withdrawn member whose orders are cancelled and consumed.
        workpiece: WorkpieceId,
    },
    /// Free `refs/bloomery/claims/<workpiece>` without touching the bloom's
    /// admission ref (#5327) — the intent the claim-release reactor drains
    /// under [`Topic::MemberClaimRelease`](crate::Topic::MemberClaimRelease).
    ///
    /// The single-ref door rather than the seal-wide one: the seal-wide
    /// release folds in the mainline admission ref, which a bloom that is
    /// still walking must keep. Snapshot-inert; the authority is the journaled
    /// withdrawal itself, so no completion fact is admitted back. Appended so
    /// the prior decisions' wire discriminants are unchanged.
    ReleaseMemberClaimRef {
        /// The bloom holding the ref.
        bloom: BloomId,
        /// The withdrawn member whose ref is freed.
        workpiece: WorkpieceId,
    },
    /// Move a bloom whose every member has been withdrawn to
    /// [`BloomStatus::Withdrawn`](crate::BloomStatus::Withdrawn) (#5327).
    ///
    /// Terminal: a bloom with no remaining member has no artifact to land, so
    /// it stops holding the one-active-bloom-per-mainline slot and a fresh
    /// bloom can seal. Appended so the prior decisions' wire discriminants are
    /// unchanged.
    MarkBloomWithdrawn {
        /// The bloom that has nothing left to run.
        bloom: BloomId,
    },
    /// Record that one composite gate has passed on the fold currently held.
    ///
    /// The two aggregate gates are dispatched together against one fold, so
    /// neither verdict on its own resolves the bloom: whichever verdict lands
    /// second reads this set to learn its sibling already passed, and only then
    /// is the landing dispatched. Bound to the *held fold* rather than to a
    /// tree because [`Decision::RecordIntegration`] clears the set — a new weave
    /// is a new subject, and a gate that passed on the tree before it has said
    /// nothing about this one.
    ///
    /// Snapshot-only: each gate's work order already went out with its own
    /// dispatch, and a pass opens no new work of its own. Appended so the prior
    /// decisions' wire discriminants are unchanged.
    RecordAggregateGatePass {
        /// The bloom whose fold was judged.
        bloom: BloomId,
        /// Which gate passed — [`StageId::AggregateVerify`] or
        /// [`StageId::AggregateReview`].
        stage: StageId,
    },
    /// Record why an operator-visible boundary refused (ADR-0206).
    ///
    /// Emitted *beside* the boundary's existing typed rejection rather than
    /// replacing it: the admitter keeps the `Outcome` variant it has always
    /// matched on, and this row is what the snapshot — and through it
    /// [`why_of`](crate::why_of) — reads back. A rejected event is journaled
    /// with its effects, so replay folds the refusal exactly as the live
    /// admission did.
    ///
    /// The refusal does not live on [`Decisions`](super::Decisions) itself,
    /// which is journaled under a frozen writing schema; a field there would
    /// need a fresh schema identity and an upcast for every prior row.
    ///
    /// Snapshot-only: a refusal is precisely the statement that no work went
    /// out, so a topic here would enqueue a row nothing drains. Appended so
    /// the prior decisions' wire discriminants are unchanged.
    RecordRefusal {
        /// The bloom the boundary refused for.
        bloom: BloomId,
        /// The member whose dispatch refused, or `None` for a bloom-scoped
        /// boundary.
        workpiece: Option<WorkpieceId>,
        /// The gate, the guard that failed, and the values it read.
        refusal: RecordedRefusal,
    },
    /// File a base-verify receipt in the snapshot ledger, keyed by the tree it
    /// judged and the whole-workspace gate set (ADR-0200).
    ///
    /// Snapshot-folding: writes both the tree index and the receipt map so a
    /// later seal of the same base (or another commit that peels to the same
    /// tree) reads one answer. Appended so the prior decisions' wire
    /// discriminants are unchanged.
    RecordBaseReceipt {
        /// The receipt, naming the commit, the peeled tree, the gate set, and
        /// the pending / green / red verdict.
        receipt: BaseReceipt,
    },
    /// Drive the whole-workspace `verify.base` fan-out against a sealed base
    /// (ADR-0200) — a snapshot-inert outbox intent on the
    /// [`Decision::DispatchOrphanClaimRelease`] model. The terminal result folds in later
    /// from [`crate::Fact::BaseVerifyCompleted`], never from this decision.
    /// Emitted exactly once per unproven base: a pending or terminal receipt
    /// already on record enqueues nothing.
    DispatchBaseVerify {
        /// The commit the lane checks out.
        base: Digest,
        /// The `verify.base` transformation, with no named diff base.
        transformation: Transformation,
        /// The catalog-calibrated profile the mechanical lane runs under.
        profile: AgentProfile,
    },
    /// Push an operator proposal onto the waiting queue (ADR-0205) — see
    /// [`crate::Snapshot::queued_proposals`].
    ///
    /// Appended so every stored row stays decodable under the current shape:
    /// new discriminants keep the v1 decisions wire frozen.
    QueueProposal {
        /// The signed candidate waiting for a clear board.
        proposal: OperatorProposal,
    },
    /// Remove a queued proposal by its digest (ADR-0205) — the fold of a
    /// successful memberless seal. Appended so every stored row stays
    /// decodable under the current shape.
    DequeueProposal {
        /// The proposal whose digest names the queue entry to drop.
        proposal: OperatorProposal,
    },
    /// Offer the queue head to the propose reactor (ADR-0205) — a
    /// snapshot-inert outbox intent on the
    /// [`Decision::DispatchOrphanClaimRelease`] model. Sealing needs content
    /// the reducer does not hold, so this names the proposal and the base it
    /// should seal against rather than sealing inline. Appended so every
    /// stored row stays decodable under the current shape.
    DispatchProposal {
        /// The queue head the reactor should seal.
        proposal: OperatorProposal,
        /// The mainline head the memberless spec bases on — current mainline
        /// at propose time, or the head a land just advanced to.
        base: Digest,
    },
}
