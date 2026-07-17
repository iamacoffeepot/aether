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
//! - the wire seam: `aether-bloomery::control` and `aether-bloomery-host::source`
//!   declare the `aether.source.*` kinds independently (the package cycle bars a
//!   shared type), so building the mail with one side's type and decoding with
//!   the other holds the two byte-compatible.
//!
//! Deeper heals — own-orphan reclaim, tombstone sweep, half-transfer completion
//! — are the deep-heal slice's (#3555, ADR-0150 as amended 2026-07-17) and are
//! deliberately absent here.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use aether_bloomery::control::{
    ReconcileOp, held_to_seal_error, held_to_supersede_error, reconcile_op, release_reclaim_mail, release_seal_mail,
    seal_claim_mail, transfer_seal_mail,
};
use aether_bloomery::{
    BloomDraft, BloomId, BloomSpec, ClaimRefKind, ClaimSeal, Decisions, Digest, Event, Evidence, EvidenceKind, Fact,
    IdempotencyKey, Membership, ReleaseSeal, ResolutionClaim, SealConflict, SealError, Snapshot, StageCatalog,
    SupersedeError, TransferSeal, WorkpieceId, reduce,
};
use aether_bloomery_github::GitSource;
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_host::bloomery::SourceShell;
use aether_bloomery_host::source::SourceCapabilityState;
use aether_bloomery_host::source::kinds::ClaimResult;
use aether_data::wire::from_bytes;

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn workpiece(name: &str) -> WorkpieceId {
    WorkpieceId(name.into())
}

/// A membership whose approval evidence is bound to the scope revision, so it
/// seals admissibly through `reduce`.
fn membership(name: &str, revision: u8) -> Membership {
    let scope_revision = digest(revision);
    Membership {
        workpiece: workpiece(name),
        scope_revision,
        approval: Evidence { subject: scope_revision, kind: EvidenceKind::Approval, detail: digest(200) },
    }
}

/// A draft sealing on `base` with the given memberships, stamped with the line
/// catalog digest the reducer admits.
fn spec(base: u8, members: Vec<Membership>) -> BloomSpec {
    BloomDraft {
        proposals: members,
        base: digest(base),
        stage_catalog: StageCatalog::line_digest(),
        ..Default::default()
    }
    .seal()
}

fn event(key: &str, fact: Fact) -> Event {
    Event { idempotency_key: IdempotencyKey(key.into()), fact }
}

/// Reduce and evolve in one step — the same fold boot journal replay runs, so a
/// snapshot built this way IS the replay-rebuilt snapshot a restart sees.
fn step(snapshot: &Snapshot, event: &Event) -> (Snapshot, Decisions) {
    let decisions = reduce(snapshot, event);
    (snapshot.apply(event, &decisions), decisions)
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
    (snapshot, _) = step(&snapshot, &event("resolve", Fact::Resolve { bloom, tree: digest(tree), lineage: vec![] }));
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
    let backend = GitSource::new(fake.clone(), false);
    (SourceCapabilityState::new(SourceShell::new(Arc::new(backend))), fake)
}

/// Point a workpiece's claim ref at a commit whose tree is `holder`'s id —
/// the holding another instance's seal leaves in the shared repository, staged
/// directly (ADR-0150: the ref namespace is the inter-instance truth).
fn stage_foreign_hold(fake: &FakeGithub, name: &str, holder: &BloomId) {
    let commit = fake.seed_base_commit(&holder.0);
    fake.seed_ref_at(&format!("bloomery/claims/{name}"), &commit);
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
