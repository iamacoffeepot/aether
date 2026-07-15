//! A scripted bloom driven draft → seal → integrate → resolve → land in
//! memory, replayed for byte-identical output (ADR-0149 migration step 1).
//!
//! The reducer is pure and the value vocabulary is content-addressed, so a
//! fixed script produces a fixed decision stream. Replaying it must yield the
//! identical stream — the property that makes the journal replayable and the
//! control plane auditable.

#![allow(clippy::unwrap_used)]

mod common;

use aether_bloomery::{Decisions, Fact, Outcome, Snapshot, reduce};
use aether_data::wire::to_vec;
use common::{claim, digest, draft, event, membership};

/// Run the canonical five-stage bloom and return every decision plus the
/// final snapshot.
fn run_trace() -> (Vec<Decisions>, Snapshot) {
    let members = vec![membership("alpha", 10), membership("beta", 11)];
    let spec = draft(1, members).seal();
    let bloom = spec.id();

    let script = vec![
        event("seal", Fact::Seal(spec)),
        event("integrate-alpha", Fact::Integrate { bloom, claim: claim("alpha", 20) }),
        event("integrate-beta", Fact::Integrate { bloom, claim: claim("beta", 21) }),
        event("resolve", Fact::Resolve { bloom, tree: digest(30), lineage: vec![digest(20), digest(21)] }),
        event("land", Fact::Land { bloom, expected_base: digest(1), new_head: digest(40) }),
    ];

    let mut snapshot = Snapshot::new(digest(1));
    let mut decisions = Vec::with_capacity(script.len());
    for ev in &script {
        let decided = reduce(&snapshot, ev);
        snapshot = snapshot.apply(ev, &decided);
        decisions.push(decided);
    }
    (decisions, snapshot)
}

#[test]
fn scripted_bloom_reaches_landed_and_advances_mainline() {
    let (decisions, snapshot) = run_trace();

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

#[test]
fn replay_is_byte_identical() {
    let (first, _) = run_trace();
    let (second, _) = run_trace();

    // Structural equality and a canonical-encoding byte match: the replayed
    // decision stream is byte-identical, which is what makes the journal
    // replayable (ADR-0149 §The control core). Compared over the raw
    // `aether_data::wire` bytes directly — a `Vec<Decisions>` is not a
    // content-addressed vocabulary type, so it carries no domain tag; this
    // replays byte-identity more directly than a digest would.
    assert_eq!(first, second);
    assert_eq!(to_vec(&first).unwrap(), to_vec(&second).unwrap());
}
