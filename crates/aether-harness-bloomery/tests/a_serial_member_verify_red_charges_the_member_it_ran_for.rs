//! A serial `verify.member` run holding exactly one member, red on a clippy
//! lint: the member is charged with the gate and leaves naming it.
//!
//! Bloom `0c5a157e` is the shape. `dispatch-8330-step-1` was a single-member
//! serial run for one workpiece; it failed `verify.clippy` alone, on a lint
//! that stated its file and line. The member was withdrawn saying its shared
//! verification "never resolved which member owed the failure" — of a run whose
//! composition held one candidate, verified standalone on its own tree.
//!
//! The contextual path had already been taught this (#6054): a composition of
//! one has nothing to bisect, so its sole member owns whatever the findings
//! leave open. The serial settlement never learned it and reported
//! `Unattributed` for every red, which the coordination reducer reads as "no
//! member is answerable individually".
//!
//! Two assertions carry it. The outcome's scope must name the member, and the
//! sentence the candidate is left with must name the gate that stopped it
//! rather than an attribution that was never in question.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::slice::from_ref;

use aether_bloomery::{
    BloomId, CONSTRUCT_IMPLEMENT_COMMAND, CoordinationPolicy, Digest, FailureScope, FakeKeyProvider, KeyId,
    MemberVerifyOutcome, SharedRunDispatch, SharedRunMode, VERIFY_MEMBER_COMMAND, VerificationMode, signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneMode, LaneScript};
use aether_chassis_bloomery::store::{CommissionBackend, OutstandingOrder, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

const MEMBER: &str = "wp-a";
const SURFACE: &str = "crates/example-a/**";
const OWNED: &str = "crates/example-a/src/contextual.rs";

/// The gate the run comes back red on, and the sentence the ejection must not
/// be composed from.
const GATE: &str = "verify.clippy";
const UNRESOLVED: &str = "never resolved which member owed the failure";

/// `dispatch-8330-step-1`'s findings in shape: a deny-level clippy lint, which
/// `-D warnings` reports at warning severity, locating itself inside the sole
/// member's surface.
const FINDINGS: &str = "\
The previous candidate failed verification. Fix these.

### verify.clippy

warning: these match arms have identical bodies
  --> crates/example-a/src/contextual.rs:1:22
   |
 1 | pub const MEMBER: &str = \"wp-a\";
   |                    ^^^^
   |
   = note: `-W clippy::match-same-arms` implied by `-W clippy::pedantic`

warning: `example-a` (lib) generated 1 warning
";

/// One member, verified on its own, with no probe budget to hide behind.
///
/// `WarmSerial` is the mode bloom 0c5a157e ran: member requests go through a
/// shared run that executes them serially over one warm slot — which is why
/// `dispatch-8330-step-1` carried a slot affinity and a step ordinal at all.
/// The reducer refuses a non-contextual plan that carries a probe budget, so
/// zero here is the mode's own constraint rather than a thumb on the scale.
fn serial_ejecting_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        red_verify: aether_bloomery::RedVerify::Eject,
        verification: VerificationMode::WarmSerial,
        eager_integration: false,
        max_run_members: 1,
        max_serial_requests: 1,
        max_attribution_probes: 0,
        movement_budget: 2,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
        coalesce_millis: Some(0),
    }
}

fn approve_scope(harness: &ScenarioHarness, scope_revision: Digest) {
    let mut store = harness.commission_store();
    let approval = signed_approval(KeyId(String::from("serial ejection harness")), &[0x0A; 32], scope_revision);
    store.insert_approval(&approval, &FakeKeyProvider).expect("the member scope retains its signed approval");
}

/// Park the construct and give it the file its own surface covers, so the
/// candidate's delta is exactly the path the scripted findings name.
fn park_construct(harness: &mut ScenarioHarness) -> Vec<OutstandingOrder> {
    harness.pump_until("the member receives its parked Construct order", |harness| {
        let orders = harness.orders();
        assert!(orders.len() <= 1, "only the sealed member can be dispatched: {orders:?}");
        orders.len() == 1
    });
    let constructs = harness.orders();
    harness.pump_until("the parked Construct child reached its real worktree", |harness| {
        let runs = harness.ledger();
        constructs.iter().all(|order| runs.iter().any(|run| run.nonce == order.nonce))
    });

    let worktree = harness
        .ledger()
        .iter()
        .find(|run| run.nonce == constructs[0].nonce)
        .and_then(|run| run.worktree.as_deref())
        .map(Path::new)
        .map(Path::to_path_buf)
        .expect("the parked local Construct recorded its real worktree");
    fs::remove_file(worktree.join(CANDIDATE_FILE)).expect("the mock's generic candidate is removed");
    fs::write(worktree.join(OWNED), format!("pub const MEMBER: &str = \"{MEMBER}\";\n"))
        .expect("the parked Construct writes inside its own approved surface");
    constructs
}

/// The outcome the warm-serial run settled its one member with.
///
/// Read after the withdrawal rather than caught mid-flight: a run row is never
/// deleted, so the settlement the reducer acted on is still there to be read
/// against the sentence it produced.
fn settled_outcome(harness: &ScenarioHarness) -> Option<MemberVerifyOutcome> {
    let mut store = harness.commission_store();
    let serial: Vec<Vec<u8>> = store
        .list_shared_runs()
        .expect("shared runs read")
        .into_iter()
        .filter(|run| {
            from_bytes::<SharedRunDispatch>(&run.dispatch)
                .is_ok_and(|dispatch| dispatch.plan.mode == SharedRunMode::WarmSerial)
        })
        .map(|run| run.run)
        .collect();

    serial
        .iter()
        .filter_map(|run| store.shared_run_members(run).ok())
        .flatten()
        .filter_map(|member| member.outcome)
        .find_map(|bytes| from_bytes::<MemberVerifyOutcome>(&bytes).ok())
}

fn withdrawal(harness: &mut ScenarioHarness, bloom: BloomId) -> Option<String> {
    harness
        .bloom(bloom)
        .members
        .into_iter()
        .find(|member| member.workpiece.0 == MEMBER)
        .and_then(|member| member.withdrawn)
        .map(|withdrawn| withdrawn.reason)
}

#[test]
fn a_serial_member_verify_red_charges_the_member_it_ran_for() {
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing()
        .then(CONSTRUCT_IMPLEMENT_COMMAND, LaneMode::NeverExits)
        .then(VERIFY_MEMBER_COMMAND, LaneMode::Fail)
        .reporting(GATE, FINDINGS);
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(serial_ejecting_policy())
        // Two prover slots, stated rather than measured: the base verify keeps
        // one warm, so a host whose measured ceiling is one would never free a
        // slot for the member run this scenario is about.
        .max_concurrent_provers(2)
        .script(&script)
        .start("serial-member-red-charges-its-member");

    let scope = harness.author_scope_revision(MEMBER, from_ref(&SURFACE));
    approve_scope(&harness, scope);
    let bloom = harness.seal_member(MEMBER, scope);

    let constructs = park_construct(&mut harness);
    harness.release_parked_lanes(&constructs);
    harness.pump_until("the red serial member leaves the bloom", |harness| withdrawal(harness, bloom).is_some());

    let outcome = settled_outcome(&harness).expect("the serial run settled its member");
    assert!(
        matches!(
            &outcome,
            MemberVerifyOutcome::Failed { scope: FailureScope::Attributed { members, .. }, .. }
                if members.len() == 1 && members[0].workpiece.0 == MEMBER
        ),
        "a serial step verifies one member's own tree, so its red is that member's: {outcome:?}",
    );

    let reason = withdrawal(&mut harness, bloom).expect("under the Eject disposition a red member leaves");
    assert!(reason.contains(GATE), "the sentence the candidate is left with names the gate that stopped it: {reason}");
    assert!(
        !reason.contains(UNRESOLVED),
        "there was one member and one tree, so nothing about ownership was unresolved: {reason}",
    );

    let projected = harness
        .commission_store()
        .lookup_review_findings(bloom.0.as_bytes(), MEMBER)
        .expect("the member findings row reads")
        .expect("an attributed member carries the run's findings");
    assert!(projected.contains(OWNED), "the ejection's evidence carries the diagnostic that stopped it: {projected}");
}
