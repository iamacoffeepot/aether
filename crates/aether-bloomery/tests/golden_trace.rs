//! A scripted bloom driven draft → seal → integrate → resolve → land, replayed
//! from a wire-encoded journal for a byte-identical, pinned decision stream
//! (ADR-0149 migration step 1).
//!
//! The reducer is pure and the value vocabulary is content-addressed, so a
//! fixed journal produces a fixed decision stream. Two things are tested: that
//! the stream is *stable* against a pinned golden digest (a reducer change that
//! silently alters a decided transition trips it), and that replaying the same
//! journal — decoded from wire bytes, the way the host will replay it — yields
//! the identical stream, which is the property that makes the journal
//! replayable and the control plane auditable.

#![allow(clippy::unwrap_used)]

mod common;

use aether_bloomery::{
    BloomId, Decisions, Digest, Event, Evidence, EvidenceKind, Fact, OrphanClaimReleaseCompletion, Outcome,
    ResolvedConfigs, Snapshot, StageId, VerifyFailure, VerifyFailureSet, WorkpieceId, reduce,
};
use aether_data::wire::{from_bytes, to_vec};
use common::{claim, digest, draft, event, membership};

/// The canonical bloom, as the journal of admitted events: seal → integrate
/// each member → resolve (the fold, which dispatches the aggregate verify) →
/// the passing verify verdict (which dispatches the aggregate review) → the
/// passing review verdict (which resolves) → land (ADR-0153).
fn script() -> Vec<Event> {
    let members = vec![membership("alpha", 10), membership("beta", 11)];
    let spec = draft(1, members).seal();
    let bloom = spec.id();
    vec![
        event("seal", Fact::Seal(spec)),
        event(
            "construct-alpha",
            Fact::AttemptCompleted {
                bloom,
                workpiece: WorkpieceId("alpha".into()),
                stage: StageId::Construct,
                passed: true,
                evidence: Evidence { subject: digest(10), kind: EvidenceKind::VerificationResult, detail: digest(48) },
                candidate: None,
            },
        ),
        event(
            "verify-alpha-fmt",
            Fact::VerifyFailed {
                bloom,
                workpiece: WorkpieceId("alpha".into()),
                evidence: Evidence { subject: digest(10), kind: EvidenceKind::VerificationResult, detail: digest(49) },
                failed_verifiers: VerifyFailureSet::one(VerifyFailure::Fmt),
            },
        ),
        event(
            "refine-alpha",
            Fact::AttemptCompleted {
                bloom,
                workpiece: WorkpieceId("alpha".into()),
                stage: StageId::Refine,
                passed: true,
                evidence: Evidence { subject: digest(10), kind: EvidenceKind::VerificationResult, detail: digest(50) },
                candidate: None,
            },
        ),
        event("integrate-alpha", Fact::Integrate { bloom, claim: claim("alpha", 10, 20) }),
        event("integrate-beta", Fact::Integrate { bloom, claim: claim("beta", 11, 21) }),
        event(
            "resolve",
            Fact::Resolve { bloom, tree: digest(30), head: digest(40), lineage: vec![digest(20), digest(21)] },
        ),
        event(
            "aggregate-verify",
            Fact::AggregateVerifyCompleted {
                bloom,
                passed: true,
                evidence: Evidence { subject: digest(30), kind: EvidenceKind::VerificationResult, detail: digest(51) },
            },
        ),
        event(
            "aggregate-review",
            Fact::AggregateReviewCompleted {
                bloom,
                passed: true,
                evidence: Evidence { subject: digest(30), kind: EvidenceKind::ReviewFinding, detail: digest(50) },
                implicated: vec![],
            },
        ),
        event("land", Fact::Land { bloom, new_head: digest(40) }),
    ]
}

/// Replay a journal the way the host will: each event survives a wire
/// encode→decode round-trip before it is reduced, so what is exercised is
/// journal replay (decode the persisted bytes, then reduce), not a second
/// in-process call over the same in-memory values.
fn replay(journal: &[Event]) -> (Vec<Decisions>, Snapshot) {
    let mut snapshot = Snapshot::new(digest(1));
    let mut decisions = Vec::with_capacity(journal.len());
    for ev in journal {
        let decoded: Event = from_bytes(&to_vec(ev).unwrap()).unwrap();
        let outcome = reduce(&snapshot, &decoded, &ResolvedConfigs::default());
        snapshot = snapshot.apply(&decoded, &outcome, &ResolvedConfigs::default());
        decisions.push(outcome);
    }
    (decisions, snapshot)
}

#[test]
fn scripted_bloom_reaches_landed_and_advances_mainline() {
    let (decisions, snapshot) = replay(&script());

    assert!(matches!(decisions[0].outcome, Outcome::Sealed(_)));
    assert!(matches!(decisions[1].outcome, Outcome::AttemptAdvanced { to: StageId::Verify, .. }));
    assert!(matches!(decisions[2].outcome, Outcome::RefineReentered { rolls: 0, .. }));
    assert!(matches!(decisions[3].outcome, Outcome::AttemptAdvanced { to: StageId::Verify, .. }));
    assert!(matches!(decisions[4].outcome, Outcome::Integrated { .. }));
    assert!(matches!(decisions[5].outcome, Outcome::Integrated { .. }));
    // The fold dispatches the compiler, not the critic: a fold that does not
    // build must never reach a model lane, and the review is what a *passing*
    // verify hands off to.
    assert!(matches!(decisions[6].outcome, Outcome::AggregateVerifyDispatched { roll: 1, .. }));
    assert!(matches!(decisions[7].outcome, Outcome::AggregateVerifyPassed { rolls: 1, .. }));
    match &decisions[8].outcome {
        Outcome::Resolved(resolved) => assert_eq!(resolved.resolution_claims.len(), 2),
        other => panic!("expected Resolved, got {other:?}"),
    }
    assert!(matches!(decisions[9].outcome, Outcome::Landed(_)));
    assert_eq!(snapshot.mainline, digest(40));
}

// Tripwire: the sha256 of the canonical wire-encoded decision stream for the
// scripted bloom. It is a computed value — it drifts the moment any reducer
// transition changes the shape of what it decides — so recompute-and-repin only
// when a change *intends* to alter the decided output; an unexplained failure
// here is a silent behavioural regression, not a stale constant to bless.
// Repinned when `reduce_seal` began emitting per-member entry-stage dispatch
// (`AdvanceStage` + `DispatchAttempt`) effects on seal (ADR-0149 §The line,
// #3505) — an intended change to the decided output, so the golden is recomputed.
// Repinned again for #3572: `Transformation` gained a `checkout` field (the git
// commit the attempt's worker checks out, threaded onto every `DispatchAttempt`
// as the bloom's sealed base) and the Construct/Refine bindings re-pointed to
// `construct.implement` — intended changes to the decided output.
// Repinned again when the Scope binding's `process` re-pointed to
// `aether.bloomery.api` (#3570) — the sealed spec's stage catalog digest is
// part of the decided output, so an intended catalog edit re-digests here too.
// Repinned again for #3559: `reduce_resolve` now emits a `DispatchLand` effect
// alongside `SetResolved` (resolution is land-readiness, ADR-0149 migration
// step 3), so the resolve step's decided output gained the land decision — an
// intended change, recomputed.
// Repinned again for #3573: the Land binding's `process` re-pointed from the
// retired `land` skill to the native `source.cas_land` lane re-digests the
// sealed spec's stage catalog, so the merged decision stream carries this edit.
// Repinned again on the #3571→main merge when the Approve binding's `process`
// re-pointed to the host-side pre-seal admission gate `aether.bloomery.approve_gate`
// (#3571) — the merged decision stream carries the #3559, #3570, #3572, and #3573
// edits plus this Approve re-point, so the golden is recomputed once more.
// Repinned again for #3595: `Transformation` gained an advisory `description`
// field (the work-order text the construct lane names in its `## Task` prompt),
// which the reducer authors `None` on every `DispatchAttempt` — an additive shape
// change to the decided output, so the golden is recomputed.
// Repinned again for #3615: `reduce_resolve` now emits `DispatchLand.new_head`
// as the resolved integrated *head* commit digest (`Fact::Resolve.head`) rather
// than the artifact `tree`, splitting the two so a land CAS-es mainline onto a
// commit — an intended change to the resolve step's decided output, recomputed.
// Repinned again for ADR-0152 (#3648): `StageProgress` and `Decision::
// DispatchAttempt` gained candidate-propagation fields (`candidate`, plus the
// explicit `scope_revision` on the dispatch), so every seal/advance decision's
// wire shape changed — an intended, coordinated break, recomputed.
// Repinned again for ADR-0152 (#3650): the claim that completes a bloom's set
// now also emits `Decision::DispatchIntegration` (the git-side fold the
// integrate driver drains), so the final integrate step's decided output gained
// the integration dispatch — an intended change, recomputed.
// Repinned again for #3657: `StageProgress` gained the `review_rolls` ceiling
// cursor (a failing Review re-enters Refine instead of re-rolling the critic),
// so every cursor-carrying decision's wire shape changed — an intended,
// coordinated break, recomputed.
// Repinned again for ADR-0153: `Fact::Resolve` now records the fold and
// dispatches the whole-bloom aggregate review instead of resolving directly,
// and the appended `Fact::AggregateReviewCompleted`'s passing verdict is what
// emits `SetResolved` + `DispatchLand` — the script gained the aggregate hop
// and the resolve step's decided output changed shape, an intended,
// coordinated break, recomputed.
// Repinned again for #4314: the opus-tier stages re-point from `claude-opus-4-8`
// to `claude-opus-5`, so every dispatch decision carrying one of those profiles
// changed value. A recalibration is an intended catalog edit — see
// `StageCatalog::profile_of`, whose model and effort values are refinable
// without an ADR — and this golden moves with it.
// Repinned again for #4324: `Transformation` gained the `model` field carrying
// the stage's resolved agent profile, which the reducer authors `None` on every
// `DispatchAttempt` and the host overlays at dispatch — the same additive shape
// change to the decided output the `description` field made, recomputed.
// Repinned again for #4578: `AgentProfile` gains its `harness` field, so every
// binding's profile digest moves and the line catalog with them. The scripted
// draft seals `StageCatalog::line_digest()`, so the sealed spec — and the bloom
// id derived from it — move too, and every decision naming that bloom changes
// value. A vocabulary addition to the catalog is an intended edit, recomputed.
// Repinned again for #4579: the four dispatched model lanes recalibrate onto the
// muse harness, moving the line catalog and so the sealed spec and bloom id the
// scripted draft derives from it — the same mechanism as the #4578 repin.
// Repinned again for #4602: `BloomSpec` and `Membership` gain their `configs`
// registry (ADR-0174), so the sealed spec's bytes — and the bloom id derived
// from them — move even for the empty registry this script seals. The member
// sort key also re-keys onto the workpiece, which cannot reorder a single-member
// script but is part of the same shape change. An additive field on a sealed
// value type is an intended, coordinated break, recomputed.
// Repinned again for #4606: a member's approval binds its `MemberSubject` — the
// workpiece, scope revision, and sealed configuration together — instead of the
// bare scope revision, and `DispatchAttempt` carries the effective registry. The
// fixtures re-bind their approvals, so every member's evidence changes value and
// the dispatch decisions gain a field.
// Repinned again for #4607: `BloomSpec` drops the `toolchain` and `policy`
// digests, which nothing read (ADR-0174: no field stays sealed and inert), so the
// sealed spec loses two fields and the bloom id derived from it moves.
// Repinned again for #4587: `BloomSpec` drops its `stage_catalog` digest for a
// sealed `StageCatalog` config entry (ADR-0174), so the sealed spec loses a field
// and the bloom id derived from it moves; and every `DispatchAttempt` gains the
// `profile` the reducer resolves from that catalog, so the decision stream
// carries it too.
// Repinned again for #3653: `DispatchIntegration` carries each member's
// workpiece alongside its candidate tree, because a fold that must combine work
// merges the member's candidate ref and a tree carries no address. The decision
// stream gains that field.
// Repinned again for the candidate-ref adoption: `DispatchIntegration` carries
// the predecessor whose refs an inheriting fold adopts, since a successor is a
// different bloom and has no candidate refs of its own. `None` on the ordinary
// path, but the field is in the stream either way.
// Repinned again for #4696: the fold now dispatches `AggregateVerify` — the
// mechanical gate the catalog always specified and nothing produced — and it is
// a *passing* verify that dispatches the aggregate review. The script gained the
// verify hop and the resolve step's decided output names a different stage, an
// intended, coordinated break, recomputed.
// Repinned again for #4723: a `Transformation` carries the commit its candidate's
// diff is taken against, so a stage whose candidate is already committed — the
// aggregate review — can be shown one at all. Every dispatch decision gains the
// field, `None` on the working-tree lanes and the sealed base on the review, an
// intended, coordinated break, recomputed.
// Repinned for ADR-0178 (#4780): `StageProgress` gained per-member seen
// verifier history, and the canonical journal now exercises the appended
// `VerifyFailed` fact through a forgiven first failure and Refine return.
// Repinned for #4697: `BloomSpec` loses its `budget` field, so every sealed spec
// re-digests and the bloom ids derived from them move; and every dispatched
// `Transformation` carries `ExecutionLimits` copied from its stage's binding in
// place of a defaulted whole-bloom `Budget`. An intended, coordinated break,
// recomputed.
// Repinned for #4663: `Decision::EmitReceipt` carries a `ProjectedReceipt` — the
// unchanged `LandingReceipt` plus the landed bloom's members — because the
// receipt value names none and the outward projection has no other route to the
// objects a landing belongs on. The land decision gains that member list; every
// other decision in the stream is byte-identical. An intended, coordinated
// break, recomputed.
// Repinned for #4891: a passing verify files its verdict in the bloom's verify
// memo, so the aggregate verify's decided output gains a `RecordVerifyProof`
// beside it. The canonical journal's fold is a two-member one and misses the
// memo, so the stream still dispatches the full aggregate pass — only the new
// record joins it. An intended, coordinated break, recomputed.
// Repinned for #4890: the mechanical `Verify` lane's dispatched `Transformation`
// names the bloom's sealed base as its diff base, so the member verify can narrow
// its compiling gates to that candidate's reverse-dependency closure. Only the
// Verify dispatches move — every other decision in the stream is byte-identical.
// Recomputed once more where that change met #4891's on the merge, so the pin
// below is the merged tree's own stream rather than either branch's.
// Repinned so `DispatchAggregateReview` carries the bloom-wide config registry
// the critic resolves `ModelOverride` through (ADR-0174). The member
// `DispatchAttempt` already handed its layered registry on; without this the
// critic hardcoded the catalog default and the receipt attested a configuration
// that never ran. Only the aggregate-review decisions gain the field.
const GOLDEN_DECISION_DIGEST: [u8; 32] = [
    0xfa, 0x7e, 0xbe, 0xc0, 0x3b, 0x42, 0xcc, 0x11, 0xa9, 0x52, 0x5c, 0x1f, 0xb8, 0x23, 0x90, 0x8c, 0xf2, 0x29, 0x71,
    0xe1, 0xf9, 0x2a, 0xf9, 0x2e, 0x03, 0xd6, 0xa1, 0x64, 0x66, 0xb2, 0x22, 0x33,
];

#[test]
fn decision_stream_matches_pinned_golden() {
    let (decisions, _) = replay(&script());
    let digest = Digest::of_wire_bytes(&to_vec(&decisions).unwrap());
    assert_eq!(*digest.as_bytes(), GOLDEN_DECISION_DIGEST, "decision stream drifted from the pinned golden");
}

/// The `u32` variant index `aether_data::wire` writes ahead of a sum type's
/// body, read back off a fact's own encoding.
fn fact_selector(fact: &Fact) -> u32 {
    u32::from_le_bytes(to_vec(fact).unwrap()[..4].try_into().unwrap())
}

// Tripwire: a `Fact` variant's *position* is its wire format, and a journal
// already on disk is decoded by these numbers. Inserting a variant rather than
// appending one silently re-points every later fact — a landed bloom's `Land`
// replays as somebody else's `GrantAttempts` — and nothing else in this suite
// would notice, because a freshly written journal round-trips against the
// shifted vocabulary perfectly well. Extend the table when a fact is appended;
// never renumber it.
#[test]
fn appended_facts_leave_every_prior_selector_where_the_journal_left_it() {
    let bloom = BloomId(digest(2));
    let evidence = |kind| Evidence { subject: digest(30), kind, detail: digest(60) };
    let pinned: [(u32, Fact); 8] = [
        (5, Fact::Land { bloom, new_head: digest(40) }),
        (
            8,
            Fact::AggregateReviewCompleted {
                bloom,
                passed: true,
                evidence: evidence(EvidenceKind::ReviewFinding),
                implicated: vec![],
            },
        ),
        (9, Fact::ObserveMainline { head: digest(40) }),
        (
            10,
            Fact::AggregateVerifyCompleted {
                bloom,
                passed: true,
                evidence: evidence(EvidenceKind::VerificationResult),
            },
        ),
        (11, Fact::LandingRejected { bloom, evidence: evidence(EvidenceKind::VerificationResult) }),
        (
            12,
            Fact::GrantAttempts { bloom, workpiece: WorkpieceId("alpha".into()), stage: StageId::Verify, attempts: 1 },
        ),
        (
            15,
            Fact::CompleteOrphanClaimRelease {
                request: digest(70),
                completion: OrphanClaimReleaseCompletion::Released,
            },
        ),
        (16, Fact::AggregateReviewExecutorFault { bloom, evidence: evidence(EvidenceKind::ExecutorFault) }),
    ];

    for (selector, fact) in pinned {
        assert_eq!(fact_selector(&fact), selector, "wire selector moved for {fact:?}");
    }
}

// ADR-0176 — the executor-fault fact goes through the same journal path every
// other fact does, so it has to survive the encode→decode round trip and decide
// the same thing on replay. Driven through `replay` rather than an in-memory
// reduce for exactly that reason: the in-memory value and the decoded one are
// only the same fact if the appended variant encodes.
#[test]
fn an_executor_fault_replays_from_wire_bytes_to_the_same_bounded_retry() {
    let spec = draft(2, vec![membership("alpha", 10)]).seal();
    let bloom = spec.id();
    let fault = |key: &str| {
        event(
            key,
            Fact::AggregateReviewExecutorFault {
                bloom,
                evidence: Evidence { subject: digest(30), kind: EvidenceKind::ExecutorFault, detail: digest(60) },
            },
        )
    };
    let journal = vec![
        event("seal", Fact::Seal(spec)),
        event("integrate", Fact::Integrate { bloom, claim: claim("alpha", 10, 20) }),
        event("resolve", Fact::Resolve { bloom, tree: digest(30), head: digest(40), lineage: vec![digest(20)] }),
        event(
            "aggregate-verify",
            Fact::AggregateVerifyCompleted {
                bloom,
                passed: true,
                evidence: Evidence { subject: digest(30), kind: EvidenceKind::VerificationResult, detail: digest(51) },
            },
        ),
        fault("fault-1"),
        fault("fault-2"),
    ];

    let (decisions, snapshot) = replay(&journal);

    assert!(matches!(decisions[4].outcome, Outcome::AggregateReviewExecutorFaulted { .. }));
    assert!(matches!(decisions[5].outcome, Outcome::AggregateReviewExecutorWedged { .. }));
    assert_eq!(snapshot.blooms.get(&bloom).unwrap().aggregate_fault.unwrap().rolls, 2);
    assert_eq!(replay(&journal).0, decisions, "the fault series replays identically");
}

#[test]
fn replay_is_byte_identical() {
    let (first, _) = replay(&script());
    let (second, _) = replay(&script());

    // Structural equality and a canonical-encoding byte match: the replayed
    // decision stream is byte-identical, which is what makes the journal
    // replayable (ADR-0149 §The control core).
    assert_eq!(first, second);
    assert_eq!(to_vec(&first).unwrap(), to_vec(&second).unwrap());
}
