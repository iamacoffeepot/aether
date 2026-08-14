//! Shared fixtures for the `aether-bloomery` integration tests. Digests are
//! built from a single seed byte so a test can name distinct content cheaply
//! and deterministically.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use aether_data::Kind;
use aether_data::wire::to_vec;

use aether_bloomery::{
    BloomDraft, BloomRecord, BloomSpec, BloomStatus, ConfigKind, ConfigRegistry, Decisions, Digest, Event, Evidence,
    EvidenceKind, Fact, IdempotencyKey, Membership, ModelOverride, ResolutionClaim, ResolvedConfigs, Snapshot,
    StageCatalog, WorkpieceId, reduce,
};

/// A distinct digest named by one seed byte.
pub fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

/// A workpiece id from a name.
pub fn workpiece(name: &str) -> WorkpieceId {
    WorkpieceId(name.into())
}

/// A membership whose approval evidence is bound to its subject — the workpiece,
/// scope revision, and sealed configuration together (ADR-0174). Built in two
/// steps because the subject covers everything but the approval itself.
pub fn membership(name: &str, revision: u8) -> Membership {
    approved(Membership {
        workpiece: workpiece(name),
        scope_revision: digest(revision),
        configs: ConfigRegistry::default(),
        approval: Evidence { subject: digest(0), kind: EvidenceKind::Approval, detail: digest(200) },
    })
}

/// Re-bind a membership's approval to its own subject, for a fixture that
/// mutates the member after `membership` built it.
pub fn approved(mut member: Membership) -> Membership {
    member.approval = Evidence { subject: member.subject(), kind: EvidenceKind::Approval, detail: digest(200) };
    member
}

/// A draft sealing on `base` with the given memberships, configuring nothing —
/// so it runs the compiled line (ADR-0174).
pub fn draft(base: u8, members: Vec<Membership>) -> BloomDraft {
    BloomDraft { proposals: members, base: digest(base), ..BloomDraft::default() }
}

/// A draft sealing `catalog` bloom-wide, with the [`ResolvedConfigs`] that
/// produce it — the pair a reducer call needs to admit an authored line.
pub fn draft_with_catalog(base: u8, members: Vec<Membership>, catalog: &StageCatalog) -> (BloomDraft, ResolvedConfigs) {
    let mut configs = ConfigRegistry::default();
    configs.insert::<StageCatalog>(catalog.address());

    let mut resolved = ResolvedConfigs::default();
    resolved.insert(catalog.address(), StageCatalog::NAME, to_vec(catalog).expect("catalog encodes"));

    (BloomDraft { proposals: members, base: digest(base), configs, ..BloomDraft::default() }, resolved)
}

/// A draft whose sole member seals `override_` in its own registry, with the
/// [`ResolvedConfigs`] that produce it — the member-scoped counterpart of
/// [`draft_with_catalog`].
pub fn draft_with_member_override(
    base: u8,
    member: Membership,
    override_: &ModelOverride,
) -> (BloomDraft, ResolvedConfigs) {
    let mut member = member;
    member.configs.insert::<ModelOverride>(override_.address());
    let member = approved(member);

    let mut resolved = ResolvedConfigs::default();
    resolved.insert(override_.address(), ModelOverride::NAME, to_vec(override_).expect("override encodes"));

    (BloomDraft { proposals: vec![member], base: digest(base), ..BloomDraft::default() }, resolved)
}

/// A resolution claim integrated at `revision`, whose evidence binds to its
/// candidate. The revision is what the inherit filter matches a successor
/// membership against (M3).
pub fn claim(name: &str, revision: u8, candidate: u8) -> ResolutionClaim {
    let candidate = digest(candidate);
    ResolutionClaim {
        workpiece: workpiece(name),
        scope_revision: digest(revision),
        candidate,
        evidence: Evidence { subject: candidate, kind: EvidenceKind::ResolutionClaim, detail: digest(201) },
    }
}

/// A uniquely-keyed event.
pub fn event(key: &str, fact: Fact) -> Event {
    Event { idempotency_key: IdempotencyKey(key.into()), fact }
}

/// Reduce and evolve in one step — the journal-replay unit.
pub fn step(snapshot: &Snapshot, event: &Event) -> (Snapshot, Decisions) {
    let decisions = reduce(snapshot, event, &ResolvedConfigs::default());
    let next = snapshot.apply(event, &decisions, &ResolvedConfigs::default());
    (next, decisions)
}

/// Record `head` as the source's live mainline — the step a rebasing
/// supersession must be preceded by, since a successor may take only the base
/// mainline is already at or the one the source last reported (#4709).
pub fn observing(snapshot: &Snapshot, head: u8) -> Snapshot {
    step(snapshot, &event(&format!("observe-{head}"), Fact::ObserveMainline { head: digest(head) })).0
}

/// Seal, integrate every member, and resolve a bloom on `mainline`. Returns
/// the evolved snapshot and the bloom's spec — the common setup for the
/// land- and supersede-facing invariants.
pub fn sealed_and_resolved(mainline: u8, members: Vec<Membership>, tree: u8) -> (Snapshot, BloomSpec) {
    let spec = draft(mainline, members).seal();
    let bloom = spec.id();
    let mut snapshot = Snapshot::new(digest(mainline));
    let seal = event("seal", Fact::Seal(spec.clone()));
    snapshot =
        snapshot.apply(&seal, &reduce(&snapshot, &seal, &ResolvedConfigs::default()), &ResolvedConfigs::default());
    let mut seed = 100u8;
    for member in spec.members() {
        let candidate = digest(seed);
        let member_claim = ResolutionClaim {
            workpiece: member.workpiece.clone(),
            scope_revision: member.scope_revision,
            candidate,
            evidence: Evidence { subject: candidate, kind: EvidenceKind::ResolutionClaim, detail: digest(202) },
        };
        let ev = event(&format!("integrate-{seed}"), Fact::Integrate { bloom, claim: member_claim });
        snapshot =
            snapshot.apply(&ev, &reduce(&snapshot, &ev, &ResolvedConfigs::default()), &ResolvedConfigs::default());
        seed = seed.wrapping_add(1);
    }
    // A distinct integrated head digest from the artifact tree (#3615) — this
    // setup does not land, so the exact value only needs to differ from `tree`.
    let resolve = event(
        "resolve",
        Fact::Resolve { bloom, tree: digest(tree), head: digest(tree.wrapping_add(1)), lineage: vec![] },
    );
    snapshot = snapshot.apply(
        &resolve,
        &reduce(&snapshot, &resolve, &ResolvedConfigs::default()),
        &ResolvedConfigs::default(),
    );
    // The fold dispatches the whole-bloom aggregate review (ADR-0153); a
    // passing verdict bound to the integrated tree is what resolves the bloom.
    let verdict = event(
        "aggregate-review-pass",
        Fact::AggregateReviewCompleted {
            bloom,
            passed: true,
            evidence: Evidence { subject: digest(tree), kind: EvidenceKind::ReviewFinding, detail: digest(203) },
            implicated: vec![],
        },
    );
    snapshot = snapshot.apply(
        &verdict,
        &reduce(&snapshot, &verdict, &ResolvedConfigs::default()),
        &ResolvedConfigs::default(),
    );
    (snapshot, spec)
}

/// Splice a bloom into a snapshot at `status`, claiming its memberships in
/// `active` directly. The reducer's own transitions never place two blooms in
/// `active` at once (the V1 one-active-bloom rule), so the supersede
/// double-claim and the V1 seal guard are exercised from a hand-built snapshot
/// — the state a store bug or a future multi-mainline could present the pure
/// guard, which is exactly what those checks defend against.
pub fn splice_bloom(snapshot: &mut Snapshot, spec: &BloomSpec, status: BloomStatus) {
    let bloom = spec.id();
    for member in spec.members() {
        snapshot.active.insert(member.workpiece.clone(), bloom);
    }
    snapshot.blooms.insert(
        bloom,
        BloomRecord {
            stage_catalog: StageCatalog::line(),
            spec: spec.clone(),
            status,
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
            superseded_by: None,
        },
    );
}
