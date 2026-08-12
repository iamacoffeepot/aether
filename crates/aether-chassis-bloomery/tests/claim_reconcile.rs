#![cfg(feature = "github")]

//! The claim-ref interposition and boot-reconcile convergence suite (#3547,
//! ADR-0150 §The claim registry).
//!
//! Each test builds the exact `aether.source.*` mail the wasm control core
//! would send — through `aether-bloomery`'s pure `control::claim_plan`
//! functions, from snapshots evolved by the real reducer — and drives its wire
//! bytes through [`SourceCapabilityState`] over the adapter's in-process
//! `FakeGithub` (no token, no network). That covers three seams no unit test
//! reaches:
//!
//! - the enforcement decisions themselves: a seal refused on a foreign-held
//!   member or admission ref with the reducer's own refusal vocabulary, a
//!   supersession that transfers carried refs / frees dropped ones / refuses a
//!   foreign-held net-new member, and a land whose release frees the refs for
//!   the next seal;
//! - the restart tripwires: a `Landed` bloom whose fire-and-forget release a
//!   crash swallowed re-releases at boot (idempotently, and without ever
//!   stomping a successor's re-claimed ref), and a `Sealed` bloom's lost refs
//!   re-assert without tearing an intact holding;
//! - the wire seam: `aether-bloomery::control` and `aether-chassis-bloomery::source`
//!   declare the `aether.source.*` kinds independently (the package cycle bars a
//!   shared type), so building the mail with one side's type and decoding with
//!   the other holds the two byte-compatible.
//!
//! The deep heals the deep-heal slice added (#3555, ADR-0150 as amended PR #3556)
//! — the tombstone sweep and the own-bloom half-transfer completion — drive the
//! pure `plan_heals` fold over the capability's real enumeration the same way,
//! covering their restart convergence below. Own-orphan reclaim (case 1) stays
//! deferred to its own follow-on and is deliberately absent.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use aether_bloomery::control::{
    HealOp, ReconcileOp, held_to_seal_error, held_to_supersede_error, plan_heals, reconcile_op, release_reclaim_mail,
    release_seal_mail, seal_claim_mail, transfer_seal_mail,
};
use aether_bloomery::{
    BloomDraft, BloomId, BloomSpec, ClaimRefKind, ClaimRefState, ClaimSeal, ConfigRegistry, Decisions, Digest,
    EnumerateClaimsResult, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, Membership, ReleaseSeal,
    ResolutionClaim, ResolvedConfigs, SealConflict, SealError, Snapshot, SupersedeError, TransferSeal, WorkpieceId,
    reduce,
};
use aether_bloomery_github::GitSource;
use aether_bloomery_github::testing::FakeGithub;
use aether_chassis_bloomery::bloomery::SourceShell;
use aether_chassis_bloomery::source::SourceCapabilityState;
use aether_chassis_bloomery::source::kinds::{ClaimResult, CompleteReleaseResult};
use aether_data::wire::from_bytes;

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn workpiece(name: &str) -> WorkpieceId {
    WorkpieceId(name.into())
}

/// A membership whose approval evidence is bound to its subject — the workpiece,
/// scope revision, and sealed configuration together (ADR-0174) — so it seals
/// admissibly through `reduce`. Built in two steps because the subject covers
/// everything but the approval itself.
fn membership(name: &str, revision: u8) -> Membership {
    let mut member = Membership {
        workpiece: workpiece(name),
        scope_revision: digest(revision),
        configs: ConfigRegistry::default(),
        approval: Evidence { subject: digest(0), kind: EvidenceKind::Approval, detail: digest(200) },
    };
    member.approval.subject = member.subject();
    member
}

/// A draft sealing on `base` with the given memberships, stamped with the line
/// catalog digest the reducer admits.
fn spec(base: u8, members: Vec<Membership>) -> BloomSpec {
    BloomDraft { proposals: members, base: digest(base), ..Default::default() }.seal()
}

fn event(key: &str, fact: Fact) -> Event {
    Event { idempotency_key: IdempotencyKey(key.into()), fact }
}

/// Reduce and evolve in one step — the same fold boot journal replay runs, so a
/// snapshot built this way IS the replay-rebuilt snapshot a restart sees.
fn step(snapshot: &Snapshot, event: &Event) -> (Snapshot, Decisions) {
    let decisions = reduce(snapshot, event, &ResolvedConfigs::default());
    (snapshot.apply(event, &decisions, &ResolvedConfigs::default()), decisions)
}

/// Seal `spec` into a fresh snapshot on its base mainline.
fn seal(spec: &BloomSpec, base: u8) -> Snapshot {
    let (snapshot, decisions) = step(&Snapshot::new(digest(base)), &event("seal", Fact::Seal(spec.clone())));
    assert!(
        matches!(decisions.outcome, aether_bloomery::Outcome::Sealed(_)),
        "the fixture seal must be admissible, got {:?}",
        decisions.outcome
    );
    snapshot
}

/// Integrate every member, resolve, and land the bloom — returning the evolved
/// snapshot and the land's decisions (the release mail's source).
fn land(snapshot: Snapshot, spec: &BloomSpec, tree: u8, head: u8) -> (Snapshot, Decisions) {
    let bloom = spec.id();
    let mut snapshot = snapshot;
    let mut seed = 100u8;
    for member in spec.members() {
        let candidate = digest(seed);
        let claim = ResolutionClaim {
            workpiece: member.workpiece.clone(),
            scope_revision: member.scope_revision,
            candidate,
            evidence: Evidence { subject: candidate, kind: EvidenceKind::ResolutionClaim, detail: digest(201) },
        };
        (snapshot, _) = step(&snapshot, &event(&format!("integrate-{seed}"), Fact::Integrate { bloom, claim }));
        seed = seed.wrapping_add(1);
    }
    (snapshot, _) = step(
        &snapshot,
        &event("resolve", Fact::Resolve { bloom, tree: digest(tree), head: digest(head), lineage: vec![] }),
    );
    // The fold's aggregate review must pass before the bloom resolves (ADR-0153).
    (snapshot, _) = step(
        &snapshot,
        &event(
            "aggregate-review-pass",
            Fact::AggregateReviewCompleted {
                bloom,
                passed: true,
                evidence: Evidence { subject: digest(tree), kind: EvidenceKind::ReviewFinding, detail: digest(202) },
                implicated: vec![],
            },
        ),
    );
    let (snapshot, decisions) = step(&snapshot, &event("land", Fact::Land { bloom, new_head: digest(head) }));
    assert!(
        matches!(decisions.outcome, aether_bloomery::Outcome::Landed(_)),
        "the fixture land must be admissible, got {:?}",
        decisions.outcome
    );
    (snapshot, decisions)
}

/// The source capability over a fresh in-process fake, claims live.
fn claim_state() -> (SourceCapabilityState, FakeGithub) {
    let fake = FakeGithub::new();
    let backend = GitSource::new(fake.clone(), Arc::new(fake.clone()), false);
    (SourceCapabilityState::new(SourceShell::new(Arc::new(backend))), fake)
}

/// Point a workpiece's claim ref at a commit carrying `holder`'s id — the
/// holding another instance's seal leaves in the shared repository, staged
/// directly (ADR-0150: the ref namespace is the inter-instance truth).
fn stage_foreign_hold(fake: &FakeGithub, name: &str, holder: &BloomId) {
    fake.seed_claim_hold(&format!("bloomery/claims/{name}"), holder);
}

fn claim_ref(name: &str) -> String {
    format!("bloomery/claims/{name}")
}

const ADMISSION_REF: &str = "bloomery/admission/mainline";

/// Drive the plan mail's wire bytes through the capability — the exact fields
/// the wasm actor's `send_to_named` would carry.
fn drive_seal(state: &SourceCapabilityState, mail: &ClaimSeal) -> ClaimResult {
    state.claim_seal(&mail.bloom, &mail.workpieces)
}

fn drive_transfer(state: &SourceCapabilityState, mail: &TransferSeal) -> ClaimResult {
    state.transfer_seal(&mail.predecessor, &mail.successor, &mail.carried, &mail.net_new, &mail.dropped)
}

fn drive_release(state: &SourceCapabilityState, mail: &ReleaseSeal) -> ClaimResult {
    state.release_seal(&mail.bloom, &mail.workpieces)
}

/// Run the boot reconcile walk a restart runs: map every replayed bloom record
/// through `reconcile_op` and drive what it plans.
fn drive_reconcile(state: &SourceCapabilityState, snapshot: &Snapshot) -> Vec<ClaimResult> {
    snapshot
        .blooms
        .values()
        .filter_map(reconcile_op)
        .map(|op| match op.unwrap() {
            ReconcileOp::Assert(mail) => drive_seal(state, &mail),
            ReconcileOp::Release(mail) => drive_release(state, &mail),
        })
        .collect()
}

fn decode_held(reply: &ClaimResult) -> (ClaimRefKind, BloomId) {
    let ClaimResult::Held { ref_kind, held_by } = reply else {
        panic!("expected Held, got {reply:?}")
    };
    (from_bytes(ref_kind).unwrap(), from_bytes(held_by).unwrap())
}

#[test]
fn a_seal_refused_on_a_foreign_held_member_rolls_back_and_reads_as_a_membership_conflict() {
    let (state, fake) = claim_state();
    // Another instance's bloom holds w2 in the shared repository; the local
    // reducer cannot see it (its `active` map is instance-local), so the seal is
    // locally accepted and the ref gate is the only thing standing (ADR-0150).
    let foreign = BloomId(digest(70));
    stage_foreign_hold(&fake, "w2", &foreign);
    let sealing = spec(1, vec![membership("w1", 11), membership("w2", 12)]);

    let reply = drive_seal(&state, &seal_claim_mail(&sealing.id(), &sealing).unwrap());

    let (ref_kind, held_by) = decode_held(&reply);
    assert_eq!(held_by, foreign, "the refusal names the foreign holder");
    assert_eq!(
        held_to_seal_error(&ref_kind, held_by),
        SealError::MembershipConflict(SealConflict { workpiece: workpiece("w2"), held_by: foreign }),
        "the cross-instance refusal reads exactly as the reducer's own membership conflict",
    );
    // All-or-nothing: w1's ref was created before the w2 conflict and must be
    // rolled back — an aborted acquire leaks no partial claim.
    assert!(!fake.ref_exists(&claim_ref("w1")), "the aborted seal's earlier ref was rolled back");
    assert!(!fake.ref_exists(ADMISSION_REF), "the aborted seal took no admission ref");
}

#[test]
fn a_seal_refused_on_a_foreign_admission_ref_reads_as_active_bloom_exists() {
    let (state, fake) = claim_state();
    // Another instance's bloom holds the one mainline-admission ref — the
    // "one sealed, unlanded bloom per mainline" rule enforced across instances.
    let foreign = spec(1, vec![membership("w9", 19)]);
    assert_eq!(drive_seal(&state, &seal_claim_mail(&foreign.id(), &foreign).unwrap()), ClaimResult::Acquired);
    let sealing = spec(1, vec![membership("w1", 11)]);

    let reply = drive_seal(&state, &seal_claim_mail(&sealing.id(), &sealing).unwrap());

    let (ref_kind, held_by) = decode_held(&reply);
    assert_eq!(held_by, foreign.id());
    assert_eq!(
        held_to_seal_error(&ref_kind, held_by),
        SealError::ActiveBloomExists(foreign.id()),
        "an admission-ref hold reads as the one-active-bloom refusal, not a member conflict",
    );
    assert!(!fake.ref_exists(&claim_ref("w1")), "the refused seal's member ref was rolled back");
}

#[test]
fn a_supersession_transfers_carried_refs_frees_dropped_ones_and_keeps_the_admission_continuous() {
    let (state, fake) = claim_state();
    let predecessor = spec(1, vec![membership("w1", 11), membership("w2", 12)]);
    let snapshot = seal(&predecessor, 1);
    assert_eq!(drive_seal(&state, &seal_claim_mail(&predecessor.id(), &predecessor).unwrap()), ClaimResult::Acquired);

    // Supersede onto {w2 carried, w3 net-new}, dropping w1. The mail is built
    // from the pre-supersede snapshot, exactly as `on_admit` builds it before
    // the commit applies.
    let successor = spec(1, vec![membership("w2", 12), membership("w3", 13)]);
    let (_, decisions) = step(
        &snapshot,
        &event("supersede", Fact::Supersede { predecessor: predecessor.id(), successor: successor.clone() }),
    );
    assert!(matches!(decisions.outcome, aether_bloomery::Outcome::Superseded { .. }));
    let reply = drive_transfer(&state, &transfer_seal_mail(&snapshot, &predecessor.id(), &successor).unwrap());

    assert_eq!(reply, ClaimResult::Acquired);
    assert!(!fake.ref_exists(&claim_ref("w1")), "the dropped member's ref was released");
    // The carried ref now names the successor: a contender sealing on w2 is
    // refused by the successor's hold, not the predecessor's.
    let contender = spec(1, vec![membership("w2", 12)]);
    let (ref_kind, held_by) = decode_held(&drive_seal(&state, &seal_claim_mail(&contender.id(), &contender).unwrap()));
    assert_eq!((ref_kind, held_by), (ClaimRefKind::Workpiece(workpiece("w2")), successor.id()));
    // The admission ref fast-forwarded predecessor→successor without a free
    // moment: a contender on a fresh workpiece is refused at admission.
    let fresh = spec(1, vec![membership("w9", 19)]);
    let (ref_kind, held_by) = decode_held(&drive_seal(&state, &seal_claim_mail(&fresh.id(), &fresh).unwrap()));
    assert_eq!((ref_kind, held_by), (ClaimRefKind::MainlineAdmission, successor.id()));
}

#[test]
fn a_supersession_refuses_on_a_foreign_held_net_new_member_as_a_membership_conflict() {
    let (state, fake) = claim_state();
    let predecessor = spec(1, vec![membership("w1", 11)]);
    let snapshot = seal(&predecessor, 1);
    assert_eq!(drive_seal(&state, &seal_claim_mail(&predecessor.id(), &predecessor).unwrap()), ClaimResult::Acquired);
    // Another instance holds the successor's net-new member — invisible to the
    // local reducer, caught only at the ref gate.
    let foreign = BloomId(digest(70));
    stage_foreign_hold(&fake, "w3", &foreign);

    let successor = spec(1, vec![membership("w1", 11), membership("w3", 13)]);
    let reply = drive_transfer(&state, &transfer_seal_mail(&snapshot, &predecessor.id(), &successor).unwrap());

    let (ref_kind, held_by) = decode_held(&reply);
    assert_eq!(
        held_to_supersede_error(&ref_kind, held_by),
        Some(SupersedeError::MembershipConflict(SealConflict { workpiece: workpiece("w3"), held_by: foreign })),
        "a foreign net-new hold reads as the reducer's own supersede membership conflict",
    );
}

#[test]
fn a_land_release_frees_the_refs_for_the_next_seal() {
    let (state, fake) = claim_state();
    let landing = spec(1, vec![membership("w1", 11)]);
    let snapshot = seal(&landing, 1);
    assert_eq!(drive_seal(&state, &seal_claim_mail(&landing.id(), &landing).unwrap()), ClaimResult::Acquired);

    let (_, decisions) = land(snapshot, &landing, 40, 41);
    let reply = drive_release(&state, &release_seal_mail(&decisions).unwrap().unwrap());

    assert_eq!(reply, ClaimResult::Acquired);
    assert!(!fake.ref_exists(&claim_ref("w1")) && !fake.ref_exists(ADMISSION_REF), "the land freed every ref");
    // The freed workpiece is claimable by the next bloom (ADR-0149 m5).
    let next = spec(41, vec![membership("w1", 21)]);
    assert_eq!(drive_seal(&state, &seal_claim_mail(&next.id(), &next).unwrap()), ClaimResult::Acquired);
}

#[test]
fn boot_reconcile_re_releases_a_landed_blooms_refs_lost_to_a_crash_and_converges_idempotently() {
    let (state, fake) = claim_state();
    let landing = spec(1, vec![membership("w1", 11), membership("w2", 12)]);
    let snapshot = seal(&landing, 1);
    assert_eq!(drive_seal(&state, &seal_claim_mail(&landing.id(), &landing).unwrap()), ClaimResult::Acquired);
    // The land commits durably, then the process dies before its
    // fire-and-forget release reaches the source: the journal says Landed, the
    // repository still holds every ref — the canonical V1 crash state.
    let (snapshot, _) = land(snapshot, &landing, 40, 41);
    assert!(fake.ref_exists(&claim_ref("w1")) && fake.ref_exists(ADMISSION_REF), "the crash stranded the refs");

    // Restart: replay rebuilt `snapshot`; the reconcile walk re-releases.
    assert_eq!(drive_reconcile(&state, &snapshot), vec![ClaimResult::Acquired]);

    for name in [claim_ref("w1"), claim_ref("w2"), ADMISSION_REF.to_owned()] {
        assert!(!fake.ref_exists(&name), "{name} was re-released at boot");
    }
    // A second restart re-runs the same walk over now-absent refs: skipped,
    // acquired, nothing re-created — the heal is idempotent.
    assert_eq!(drive_reconcile(&state, &snapshot), vec![ClaimResult::Acquired]);
    assert!(!fake.ref_exists(&claim_ref("w1")), "a repeated reconcile re-creates nothing");
}

#[test]
fn boot_reconcile_release_spares_a_ref_the_next_bloom_re_claimed() {
    let (state, fake) = claim_state();
    let landed = spec(1, vec![membership("w1", 11)]);
    let snapshot = seal(&landed, 1);
    assert_eq!(drive_seal(&state, &seal_claim_mail(&landed.id(), &landed).unwrap()), ClaimResult::Acquired);
    let (_, decisions) = land(snapshot, &landed, 40, 41);
    assert_eq!(drive_release(&state, &release_seal_mail(&decisions).unwrap().unwrap()), ClaimResult::Acquired);
    // The next bloom re-claims the freed workpiece before this instance's next
    // boot replays the Landed record and re-issues its release.
    let successor = spec(41, vec![membership("w1", 21)]);
    assert_eq!(drive_seal(&state, &seal_claim_mail(&successor.id(), &successor).unwrap()), ClaimResult::Acquired);

    let reply = drive_release(&state, &release_reclaim_mail(&landed.id(), &landed).unwrap());

    // The CAS read-guard spares the successor's holding: the stale release is
    // refused, never a deletion of a ref this bloom no longer owns.
    let (ref_kind, held_by) = decode_held(&reply);
    assert_eq!((ref_kind, held_by), (ClaimRefKind::Workpiece(workpiece("w1")), successor.id()));
    assert!(fake.ref_exists(&claim_ref("w1")), "the successor's ref survived the stale release");
}

#[test]
fn boot_reconcile_re_asserts_a_sealed_blooms_lost_refs_without_tearing_an_intact_holding() {
    let (state, fake) = claim_state();
    // The journal proves a Sealed bloom, but the repository has no refs for it
    // (the lost-ref-set case the re-assert exists for).
    let sealed = spec(1, vec![membership("w1", 11)]);
    let snapshot = seal(&sealed, 1);

    assert_eq!(drive_reconcile(&state, &snapshot), vec![ClaimResult::Acquired], "lost refs are re-acquired at boot");
    assert!(fake.ref_exists(&claim_ref("w1")) && fake.ref_exists(ADMISSION_REF));

    // A later restart re-asserts over the now-intact holding: the acquire
    // refuses on the bloom's own ref and rolls back, leaving the holding
    // exactly as it was (the actor ignores the reply — asserted here so the
    // no-op contract is pinned, not assumed).
    let replies = drive_reconcile(&state, &snapshot);
    let (_, held_by) = decode_held(&replies[0]);
    assert_eq!(held_by, sealed.id(), "an intact holding refuses on the bloom's own ref");
    // The holding is untouched: a contender is still refused by this bloom.
    let contender = spec(1, vec![membership("w1", 31)]);
    let (ref_kind, held_by) = decode_held(&drive_seal(&state, &seal_claim_mail(&contender.id(), &contender).unwrap()));
    assert_eq!((ref_kind, held_by), (ClaimRefKind::Workpiece(workpiece("w1")), sealed.id()));
}

/// Point `name` at a commit carrying `holder`'s id — a live claim ref staged
/// directly, for a `name` outside the `bloomery/claims/<wp>` shape
/// [`stage_foreign_hold`] covers (e.g. the admission ref).
fn stage_hold_at(fake: &FakeGithub, name: &str, holder: &BloomId) {
    fake.seed_claim_hold(name, holder);
}

/// Point `name` at a tombstone commit (empty tree + `Bloom-Id: tombstone`) —
/// the ref state an interrupted `release_seal` leaves after its
/// CAS-to-tombstone linearized but its name-only cleanup delete never ran.
fn stage_tombstone(fake: &FakeGithub, name: &str) {
    fake.seed_claim_tombstone(name);
}

/// Enumerate the live claim refs through the capability, decoding each state —
/// the surface the boot reconcile folds through [`plan_heals`].
fn drive_enumerate(state: &SourceCapabilityState) -> Vec<ClaimRefState> {
    let EnumerateClaimsResult::Ok { states } = state.enumerate_claims() else {
        panic!("expected an Ok enumeration")
    };
    states.iter().map(|bytes| from_bytes(bytes).unwrap()).collect()
}

/// Run the boot-reconcile deep-heal walk: enumerate, fold through `plan_heals`
/// against the replay-rebuilt `snapshot`, and drive each planned heal through the
/// capability — exactly what [`ControlCore`]'s `on_enumerate_claims_result` does.
fn drive_heals(state: &SourceCapabilityState, snapshot: &Snapshot) {
    for op in plan_heals(snapshot, &drive_enumerate(state)) {
        match op.unwrap() {
            HealOp::Transfer(mail) => {
                assert_eq!(
                    state.complete_transfer(&mail.predecessor, &mail.successor, &mail.ref_kind),
                    ClaimResult::Acquired,
                );
            }
            HealOp::Release(mail) => {
                // Every clean release terminal is convergence for a boot heal:
                // the ref is not held by the stranded predecessor any more,
                // whether this call deleted it or a prior one already had.
                assert!(
                    matches!(
                        state.complete_release(&mail.bloom, &mail.ref_kind),
                        CompleteReleaseResult::Released | CompleteReleaseResult::AlreadyAbsent
                    ),
                    "a planned heal release converges",
                );
            }
        }
    }
}

#[test]
fn boot_reconcile_sweeps_a_tombstoned_ref_an_interrupted_release_stranded() {
    // A release CAS-to-tombstoned a member ref but the process died before its
    // name-only cleanup delete ran: the repository holds a tombstone marker no
    // bloom owns. The V1 walk (per-bloom, by status) never sees it — only the
    // enumeration-driven sweep reclaims the name.
    let (state, fake) = claim_state();
    let landed = spec(1, vec![membership("w1", 11)]);
    let snapshot = seal(&landed, 1);
    let (snapshot, _) = land(snapshot, &landed, 40, 41);
    stage_tombstone(&fake, &claim_ref("w1"));

    drive_heals(&state, &snapshot);

    assert!(!fake.ref_exists(&claim_ref("w1")), "the tombstoned ref name was swept");
    // Idempotent: a second boot over the now-absent ref sweeps nothing new.
    drive_heals(&state, &snapshot);
    assert!(!fake.ref_exists(&claim_ref("w1")));
    // The freed workpiece is claimable — no legitimate seal is blocked by the
    // stranded tombstone.
    let next = spec(41, vec![membership("w1", 21)]);
    assert_eq!(drive_seal(&state, &seal_claim_mail(&next.id(), &next).unwrap()), ClaimResult::Acquired);
}

#[test]
fn boot_reconcile_completes_a_half_transferred_supersede() {
    // A supersession P→S crashed mid-`transfer_seal`: the admission ref already
    // fast-forwarded to S, but the carried member w2 is still at P and the dropped
    // member w1 was never released. The journal records P superseded by S (members
    // {w2, w3}); the deep heal completes the carried ref to S and releases the
    // stranded drop.
    let (state, fake) = claim_state();
    let predecessor = spec(1, vec![membership("w1", 11), membership("w2", 12)]);
    let snapshot = seal(&predecessor, 1);
    let successor = spec(1, vec![membership("w2", 12), membership("w3", 13)]);
    let (snapshot, decisions) = step(
        &snapshot,
        &event("supersede", Fact::Supersede { predecessor: predecessor.id(), successor: successor.clone() }),
    );
    assert!(matches!(decisions.outcome, aether_bloomery::Outcome::Superseded { .. }));

    // Stage the half-transfer repository state: admission at S (moved), w2 and w1
    // still at P.
    stage_hold_at(&fake, ADMISSION_REF, &successor.id());
    stage_hold_at(&fake, &claim_ref("w2"), &predecessor.id());
    stage_hold_at(&fake, &claim_ref("w1"), &predecessor.id());

    drive_heals(&state, &snapshot);

    // The carried ref completed to the successor: a contender on w2 is refused by
    // S's hold, not P's.
    let contender = spec(1, vec![membership("w2", 12)]);
    let (ref_kind, held_by) = decode_held(&drive_seal(&state, &seal_claim_mail(&contender.id(), &contender).unwrap()));
    assert_eq!((ref_kind, held_by), (ClaimRefKind::Workpiece(workpiece("w2")), successor.id()));
    // The dropped member was released — its workpiece is claimable again.
    assert!(!fake.ref_exists(&claim_ref("w1")), "the stranded dropped ref was released");
    // Idempotent: a second boot over the converged state re-drives to no effect.
    drive_heals(&state, &snapshot);
    assert!(!fake.ref_exists(&claim_ref("w1")));
}

#[test]
fn boot_reconcile_deep_heal_leaves_a_foreign_held_ref_untouched() {
    // A ref held by a bloom this journal does not know as a superseded predecessor
    // is report-only (ADR-0150's foreign-staleness boundary): the deep heal plans
    // nothing for it, so an active peer's live claim survives the reconcile.
    let (state, fake) = claim_state();
    // The local journal is empty of this bloom; the repository holds its ref.
    let foreign = BloomId(digest(70));
    stage_foreign_hold(&fake, "w1", &foreign);
    let snapshot = Snapshot::new(digest(1));

    drive_heals(&state, &snapshot);

    assert!(fake.ref_exists(&claim_ref("w1")), "a foreign live claim is never healed away");
    let local = spec(1, vec![membership("w1", 11)]);
    let (_, held_by) = decode_held(&drive_seal(&state, &seal_claim_mail(&local.id(), &local).unwrap()));
    assert_eq!(held_by, foreign, "the foreign hold still blocks a local seal");
}
