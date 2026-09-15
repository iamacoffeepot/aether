//! A contextual run holding exactly one member, red on a gate whose findings
//! name a file inside that member's surface: one run, zero probes, and the
//! member leaves carrying the diagnostic.
//!
//! Bloom 9680c483 (#6054) is the shape. Six single-member contextual runs
//! dispatched; two went red. For both, the ownership parser read rustc's
//! source-snippet gutter as a second finding that named no path, so the gate
//! "did not discriminate" and the walk bought a baseline and a singleton
//! `verify.check` — on a composition with nothing to bisect. The probing ran
//! past each request's deadline, and the members were recorded as *expired*
//! rather than red: known-red since step 0, sitting in `Verify` with no wedge,
//! no ejection and no diagnostic.
//!
//! The assertion that matters is the negative one: **zero probe steps**, on a
//! run whose probe budget is deliberately generous. A version that attributes
//! and still buys a confirming bisection has fixed nothing.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::slice::from_ref;

use aether_bloomery::{
    CONSTRUCT_IMPLEMENT_COMMAND, CoordinationPolicy, Digest, FailureScope, FakeKeyProvider, KeyId, MemberVerifyOutcome,
    SharedRunDispatch, SharedRunMode, VERIFY_CHECK_COMMAND, VerificationMode, signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneMode, LaneScript};
use aether_chassis_bloomery::store::{CommissionBackend, OutstandingOrder, SharedRunLifecycle, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

const MEMBER: &str = "wp-a";
const SURFACE: &str = "crates/example-a/**";
const OWNED: &str = "crates/example-a/src/contextual.rs";

/// The step-0 findings, in the shape bloom 9680c483's clippy run reported:
/// the diagnostic, its indented `-->` location, and the source snippet whose
/// line-number gutter starts at column zero — followed by the tally and the
/// compile notice cargo closes a failed build with. Every one of those trailing
/// lines used to read as a finding that named no path.
const FINDINGS: &str = "\
The previous candidate failed verification. Fix these.

### verify.clippy

warning: this expression creates a reference which is immediately dereferenced by the compiler
  --> crates/example-a/src/contextual.rs:1:22
   |
 1 | pub const MEMBER: &str = \"wp-a\";
   |                    ^^^^ help: change this to: `str`
   |
   = note: `-D clippy::needless-borrow` implied by `-D warnings`

warning: `example-a` (lib) generated 1 warning
error: could not compile `example-a` (lib) due to 1 previous error
";

fn ejecting_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        red_verify: aether_bloomery::RedVerify::Eject,
        verification: VerificationMode::Contextual,
        eager_integration: false,
        max_run_members: 1,
        max_serial_requests: 1,
        // Deliberately generous. A budget of zero would make "no probe ran" a
        // statement about the budget rather than about the attribution.
        max_attribution_probes: 8,
        movement_budget: 2,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
        coalesce_millis: Some(0),
    }
}

fn approve_scope(harness: &ScenarioHarness, scope_revision: Digest) {
    let mut store = harness.commission_store();
    let approval = signed_approval(KeyId(String::from("single-member ejection harness")), &[0x0A; 32], scope_revision);
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

fn contextual_runs(harness: &ScenarioHarness) -> Vec<(Vec<u8>, SharedRunLifecycle)> {
    harness
        .commission_store()
        .list_shared_runs()
        .expect("shared runs read")
        .into_iter()
        .filter(|run| {
            from_bytes::<SharedRunDispatch>(&run.dispatch)
                .is_ok_and(|dispatch| dispatch.plan.mode == SharedRunMode::Contextual)
        })
        .map(|run| (run.run, run.lifecycle))
        .collect()
}

fn settled_outcome(harness: &ScenarioHarness, run: &[u8]) -> Option<MemberVerifyOutcome> {
    harness
        .commission_store()
        .shared_run_members(run)
        .ok()?
        .iter()
        .filter_map(|member| member.outcome.as_deref())
        .find_map(|bytes| from_bytes::<MemberVerifyOutcome>(bytes).ok())
}

fn probe_steps(harness: &ScenarioHarness, run: &[u8]) -> usize {
    harness
        .commission_store()
        .shared_run_steps(run)
        .expect("the run's steps read")
        .iter()
        .filter_map(|step| serde_json::from_slice::<serde_json::Value>(&step.descriptor).ok())
        .filter(|descriptor| descriptor.get("Probe").is_some())
        .count()
}

#[test]
fn a_single_member_red_run_is_ejected_from_its_findings_without_probing() {
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing()
        .then(CONSTRUCT_IMPLEMENT_COMMAND, LaneMode::NeverExits)
        .then(VERIFY_CHECK_COMMAND, LaneMode::Fail)
        .reporting("verify.clippy", FINDINGS);
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(ejecting_policy())
        // Two prover slots, stated rather than measured: the base verify keeps
        // one warm, so a host whose measured ceiling is one would never free a
        // slot for the contextual run this scenario is about.
        .max_concurrent_provers(2)
        .script(&script)
        .start("single-member-red-run-ejects");

    let scope = harness.author_scope_revision(MEMBER, from_ref(&SURFACE));
    approve_scope(&harness, scope);
    let bloom = harness.seal_member(MEMBER, scope);

    let constructs = park_construct(&mut harness);
    harness.release_parked_lanes(&constructs);
    harness.pump_until("the single-member contextual run settles", |harness| {
        contextual_runs(harness).iter().any(|(run, lifecycle)| {
            *lifecycle == SharedRunLifecycle::Completed && settled_outcome(harness, run).is_some()
        })
    });

    let runs = contextual_runs(&harness);
    assert_eq!(runs.len(), 1, "one member is one run; the probe walk must not open a second: {runs:?}");
    let (run, _) = runs.into_iter().next().expect("the contextual run exists");
    assert_eq!(
        probe_steps(&harness, &run),
        0,
        "a composition of one has nothing to bisect, so the red is attributed on the step-0 findings alone",
    );

    let outcome = settled_outcome(&harness, &run).expect("the member settled");
    assert!(
        matches!(
            &outcome,
            MemberVerifyOutcome::Failed { scope: FailureScope::Attributed { members, .. }, .. }
                if members.len() == 1 && members[0].workpiece.0 == MEMBER
        ),
        "the sole member is charged with the gate rather than left pending or expired: {outcome:?}",
    );

    let projected = harness
        .commission_store()
        .lookup_review_findings(bloom.0.as_bytes(), MEMBER)
        .expect("the member findings row reads")
        .expect("an attributed member carries the run's findings");
    assert!(projected.contains(OWNED), "the ejection carries the finding that named the member: {projected}");

    let member = harness
        .bloom(bloom)
        .members
        .into_iter()
        .find(|member| member.workpiece.0 == MEMBER)
        .expect("the sealed member is in the view");
    assert!(
        member.withdrawn.is_some(),
        "under the Eject disposition a red member leaves rather than buying a repair lap: {member:?}",
    );
}
