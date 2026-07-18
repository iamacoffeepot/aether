//! Shared fixtures for the `aether-bloomery` integration tests. Digests are
//! built from a single seed byte so a test can name distinct content cheaply
//! and deterministically.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use aether_bloomery::{
    BloomDraft, BloomRecord, BloomSpec, BloomStatus, Decisions, Digest, Event, Evidence, EvidenceKind, Fact,
    IdempotencyKey, Membership, ResolutionClaim, Snapshot, StageCatalog, WorkpieceId, reduce,
};

/// A distinct digest named by one seed byte.
pub fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

/// A workpiece id from a name.
pub fn workpiece(name: &str) -> WorkpieceId {
    WorkpieceId(name.into())
}

/// A membership whose approval evidence is bound to the scope revision.
pub fn membership(name: &str, revision: u8) -> Membership {
    let scope_revision = digest(revision);
    Membership {
        workpiece: workpiece(name),
        scope_revision,
        approval: Evidence { subject: scope_revision, kind: EvidenceKind::Approval, detail: digest(200) },
    }
}

/// A draft sealing on `base` with the given memberships. Stamps the line
/// catalog digest so the draft seals admissibly through `reduce` (the reducer
/// rejects the zero-default catalog).
pub fn draft(base: u8, members: Vec<Membership>) -> BloomDraft {
    BloomDraft {
        proposals: members,
        base: digest(base),
        stage_catalog: StageCatalog::line_digest(),
        ..BloomDraft::default()
    }
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
    let decisions = reduce(snapshot, event);
    let next = snapshot.apply(event, &decisions);
    (next, decisions)
}

/// Seal, integrate every member, and resolve a bloom on `mainline`. Returns
/// the evolved snapshot and the bloom's spec — the common setup for the
/// land- and supersede-facing invariants.
pub fn sealed_and_resolved(mainline: u8, members: Vec<Membership>, tree: u8) -> (Snapshot, BloomSpec) {
    let spec = draft(mainline, members).seal();
    let bloom = spec.id();
    let mut snapshot = Snapshot::new(digest(mainline));
    let seal = event("seal", Fact::Seal(spec.clone()));
    snapshot = snapshot.apply(&seal, &reduce(&snapshot, &seal));
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
        snapshot = snapshot.apply(&ev, &reduce(&snapshot, &ev));
        seed = seed.wrapping_add(1);
    }
    // A distinct integrated head digest from the artifact tree (#3615) — this
    // setup does not land, so the exact value only needs to differ from `tree`.
    let resolve = event(
        "resolve",
        Fact::Resolve { bloom, tree: digest(tree), head: digest(tree.wrapping_add(1)), lineage: vec![] },
    );
    snapshot = snapshot.apply(&resolve, &reduce(&snapshot, &resolve));
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
            spec: spec.clone(),
            status,
            claims: BTreeMap::new(),
            evidence: Vec::new(),
            holds: BTreeSet::new(),
            progress: BTreeMap::new(),
            superseded_by: None,
        },
    );
}
