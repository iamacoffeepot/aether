//! Withdrawing a member something else depends on: refused without `cascade`,
//! and with it the dependent leaves too, naming the ancestor that took it.
//!
//! The pair is the point. A withdrawal that silently stranded a dependent would
//! leave a member whose edge can never close — the exact stall shape #5327
//! exists to remove — so the reducer refuses it and names who would be
//! stranded. The cascade set is *derived* there rather than closed over by the
//! caller, so the fact carries one named member and a flag, and this is what
//! proves the derivation happens.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{Outcome, WorkpieceId};
use aether_harness_bloomery::{FixtureHarness, OperatorMove, digest};

const ANCESTOR: &str = "wp-0";
const DEPENDENT: &str = "wp-1";

fn withdraw(cascade: bool) -> OperatorMove {
    OperatorMove::Withdraw {
        // Distinct ticks so the refused attempt and the granted one do not
        // share an idempotency key — a repeat under one key is a duplicate,
        // not a second decision.
        at_tick: u32::from(cascade),
        workpiece: WorkpieceId(ANCESTOR.into()),
        reason: "the whole line moved to a later bloom".into(),
        operator: "harness".into(),
        cascade,
    }
}

#[test]
fn a_withdrawn_dependency_cascades_to_its_dependent() {
    let mut harness = FixtureHarness::start("withdraw-cascade");
    // The edge is declared at the seal: without one the reducer has no
    // dependent to strand and nothing here would be refused.
    let bloom = harness.seal_graph(&[(ANCESTOR, digest(0x51)), (DEPENDENT, digest(0x52))], &[(DEPENDENT, ANCESTOR)]);

    let refused = harness.apply_operator(bloom, &withdraw(false));
    assert!(
        matches!(refused, Outcome::WithdrawRejected(_)),
        "stranding a dependent is refused, not silently applied: {refused:?}",
    );
    assert!(
        harness.bloom(bloom).members.iter().all(|member| member.withdrawn.is_none()),
        "a refused withdrawal moves nobody",
    );

    let outcome = harness.apply_operator(bloom, &withdraw(true));
    let Outcome::MembersWithdrawn { withdrawn, .. } = &outcome else {
        panic!("the cascading withdrawal applies: {outcome:?}");
    };
    assert_eq!(
        withdrawn.iter().map(|id| id.0.as_str()).collect::<Vec<_>>(),
        [ANCESTOR, DEPENDENT],
        "the operator named one member; the reducer derived the dependent",
    );

    let view = harness.bloom(bloom);
    let dependent =
        view.members.iter().find(|member| member.workpiece.0 == DEPENDENT).expect("the dependent is still listed");
    let record = dependent.withdrawn.as_ref().expect("the dependent left with the ancestor");
    assert_eq!(
        record.depends_on.as_ref().map(|id| id.0.as_str()),
        Some(ANCESTOR),
        "a cascaded withdrawal names the ancestor that took it: {record:?}",
    );
}
