//! A member whose revision declares only `src/**` still owns its crate's
//! `tests/`: the candidate touches nothing outside the admitted crate atom, so
//! Verify containment holds and the bloom lands with no surface request.
//!
//! The measured fault (issue 6030): one crate's `src` and `tests` compile
//! together, but a surface naming only `src/**` refused the `tests/` edit the
//! change needed, parking a correct change for an amendment round trip. Before
//! the crate atom this scenario's Verify fails naming the tests path; after it
//! the bloom lands.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::slice::from_ref;

use aether_bloomery::{BloomStatus, CONSTRUCT_IMPLEMENT_COMMAND, Digest, FakeKeyProvider, KeyId, signed_approval};
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneMode, LaneScript};
use aether_chassis_bloomery::store::CommissionBackend;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

/// The declared surface: one half of the example crate.
const SURFACE: &str = "crates/example-a/src/**";

/// The `tests/` file the candidate changes: inside the admitted crate atom,
/// outside the declared subtree.
const TESTS_FILE: &str = "crates/example-a/tests/candidate.rs";

fn approve_scope(harness: &ScenarioHarness, scope_revision: Digest) {
    let approval = signed_approval(KeyId(String::from("crate-atom harness")), &[0x0A; 32], scope_revision);
    harness
        .commission_store()
        .insert_approval(&approval, &FakeKeyProvider)
        .expect("the member scope retains its signed approval");
}

#[test]
fn a_src_surface_admits_its_crates_tests() {
    let authority = Repo::with_formatted_example_project();
    // Park the construct so the tests file can be written into its real
    // worktree; releasing the park captures the worktree as the candidate.
    let script = LaneScript::all_passing().then(CONSTRUCT_IMPLEMENT_COMMAND, LaneMode::NeverExits);
    let mut harness =
        HarnessBuilder::local_authority(&authority).script(&script).start("src-surface-admits-crate-tests");

    let scope = harness.author_scope_revision("wp", from_ref(&SURFACE));
    approve_scope(&harness, scope);
    let bloom = harness.seal_members(&[("wp", scope)]);

    harness.pump_until("the parked Construct reaches its real worktree", |harness| {
        let orders = harness.orders();
        orders.len() == 1 && harness.ledger().iter().any(|run| run.nonce == orders[0].nonce)
    });
    let orders = harness.orders();
    let runs = harness.ledger();
    let worktree = runs
        .iter()
        .find(|run| run.nonce == orders[0].nonce)
        .and_then(|run| run.worktree.as_deref())
        .map(Path::new)
        .expect("the parked Construct recorded its real worktree");
    fs::remove_file(worktree.join(CANDIDATE_FILE)).expect("the mock's generic candidate is removed");
    fs::create_dir_all(worktree.join("crates/example-a/tests")).expect("the crate tests dir creates");
    fs::write(worktree.join(TESTS_FILE), "pub fn candidate() -> u8 {\n    1\n}\n")
        .expect("the parked Construct writes inside its crate's tests dir");
    harness.release_parked_lanes(&orders);

    harness.pump_until("the bloom lands", |harness| harness.bloom(bloom).status == BloomStatus::Landed);

    let member = &harness.bloom(bloom).members[0];
    assert!(member.awaiting_surface.is_none(), "landing needed no surface request: {member:?}");
}
