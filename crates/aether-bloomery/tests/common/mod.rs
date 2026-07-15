//! Shared fixtures for the `aether-bloomery` integration tests. Digests are
//! built from a single seed byte so a test can name distinct content cheaply
//! and deterministically.

#![allow(dead_code)]

use aether_bloomery::{
    BloomDraft, BloomSpec, Decisions, Digest, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, Membership,
    ResolutionClaim, Snapshot, WorkpieceId, reduce,
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

/// A draft sealing on `base` with the given memberships.
pub fn draft(base: u8, members: Vec<Membership>) -> BloomDraft {
    BloomDraft { proposals: members, base: digest(base), ..BloomDraft::default() }
}

/// A resolution claim whose evidence binds to its candidate.
pub fn claim(name: &str, candidate: u8) -> ResolutionClaim {
    let candidate = digest(candidate);
    ResolutionClaim {
        workpiece: workpiece(name),
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
            candidate,
            evidence: Evidence { subject: candidate, kind: EvidenceKind::ResolutionClaim, detail: digest(202) },
        };
        let ev = event(&format!("integrate-{seed}"), Fact::Integrate { bloom, claim: member_claim });
        snapshot = snapshot.apply(&ev, &reduce(&snapshot, &ev));
        seed = seed.wrapping_add(1);
    }
    let resolve = event("resolve", Fact::Resolve { bloom, tree: digest(tree), lineage: vec![] });
    snapshot = snapshot.apply(&resolve, &reduce(&snapshot, &resolve));
    (snapshot, spec)
}
