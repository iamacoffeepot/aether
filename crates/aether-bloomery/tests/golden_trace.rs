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

use aether_bloomery::{Decisions, Digest, Event, Fact, Outcome, Snapshot, reduce};
use aether_data::wire::{from_bytes, to_vec};
use common::{claim, digest, draft, event, membership};

/// The canonical five-stage bloom, as the journal of admitted events.
fn script() -> Vec<Event> {
    let members = vec![membership("alpha", 10), membership("beta", 11)];
    let spec = draft(1, members).seal();
    let bloom = spec.id();
    vec![
        event("seal", Fact::Seal(spec)),
        event("integrate-alpha", Fact::Integrate { bloom, claim: claim("alpha", 10, 20) }),
        event("integrate-beta", Fact::Integrate { bloom, claim: claim("beta", 11, 21) }),
        event("resolve", Fact::Resolve { bloom, tree: digest(30), lineage: vec![digest(20), digest(21)] }),
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
        let outcome = reduce(&snapshot, &decoded);
        snapshot = snapshot.apply(&decoded, &outcome);
        decisions.push(outcome);
    }
    (decisions, snapshot)
}

#[test]
fn scripted_bloom_reaches_landed_and_advances_mainline() {
    let (decisions, snapshot) = replay(&script());

    assert!(matches!(decisions[0].outcome, Outcome::Sealed(_)));
    assert!(matches!(decisions[1].outcome, Outcome::Integrated { .. }));
    assert!(matches!(decisions[2].outcome, Outcome::Integrated { .. }));
    match &decisions[3].outcome {
        Outcome::Resolved(resolved) => assert_eq!(resolved.resolution_claims.len(), 2),
        other => panic!("expected Resolved, got {other:?}"),
    }
    assert!(matches!(decisions[4].outcome, Outcome::Landed(_)));
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
const GOLDEN_DECISION_DIGEST: [u8; 32] = [
    0x63, 0x1a, 0x52, 0x47, 0xb8, 0xbe, 0x71, 0xdb, 0xf4, 0x09, 0x74, 0x3a, 0x14, 0x93, 0x63, 0x7d, 0xf4, 0xb9, 0x79,
    0xb6, 0xfd, 0xde, 0x1d, 0x55, 0x8a, 0x60, 0xa2, 0x8c, 0x9c, 0xd4, 0x4c, 0x6b,
];

#[test]
fn decision_stream_matches_pinned_golden() {
    let (decisions, _) = replay(&script());
    let digest = Digest::of_wire_bytes(&to_vec(&decisions).unwrap());
    assert_eq!(*digest.as_bytes(), GOLDEN_DECISION_DIGEST, "decision stream drifted from the pinned golden");
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
